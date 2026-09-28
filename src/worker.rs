use crate::model::{Assignment, Claim, Report, safe_component};
use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use reqwest::Client;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    process::Command,
};

#[derive(Args)]
pub struct Options {
    #[arg(long)]
    server: String,
    #[arg(long, env = "CONAN_SERVER_WORKER_TOKEN", hide_env_values = true)]
    token: String,
    #[arg(long, default_value = "worker-1")]
    id: String,
    /// IDs from the server target matrix that this machine can build.
    #[arg(long = "target", required = true, value_delimiter = ',')]
    targets: Vec<String>,
    #[arg(long, default_value = "work")]
    work: PathBuf,
    /// Conan profile file for the target compiler. Otherwise detect this machine's compiler.
    #[arg(long)]
    host_profile: Option<PathBuf>,
    #[arg(long, default_value = "conan")]
    conan: String,
    /// Process one queued job and exit; a failed build returns a nonzero exit code.
    #[arg(long)]
    once: bool,
    #[arg(long, default_value_t = 5)]
    poll_seconds: u64,
    #[arg(long, default_value_t = 3600)]
    timeout_seconds: u64,
    /// Disable ConanCenter; all dependencies must be available from this registry.
    #[arg(long)]
    no_conancenter: bool,
}

fn runner_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "Macos",
        "windows" => "Windows",
        "linux" => "Linux",
        _ => "Unsupported",
    }
}

/// Rust uses extended-length paths on Windows; cmd.exe and some build tools
/// reject them as working directories. Keep the resolved local drive path.
fn compiler_path(path: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut components = canonical.components();
        match components.next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::VerbatimDisk(drive) => {
                    let mut result = PathBuf::from(format!("{}:\\", drive as char));
                    for component in components {
                        if component != Component::RootDir {
                            result.push(component.as_os_str());
                        }
                    }
                    Ok(result)
                }
                Prefix::Disk(_) => Ok(canonical),
                _ => bail!("worker paths must be on a local Windows drive, not a UNC share"),
            },
            _ => bail!("expected an absolute Windows drive path"),
        }
    }
    #[cfg(not(windows))]
    Ok(canonical)
}

pub async fn run(mut options: Options) -> Result<()> {
    ensure!(safe_component(&options.id), "invalid worker id");
    ensure!(
        options.poll_seconds > 0 && options.timeout_seconds > 0,
        "timeouts must be positive"
    );
    ensure!(
        options.token.len() >= 32,
        "worker token must contain at least 32 characters"
    );
    options.server = options.server.trim_end_matches('/').into();
    let url = reqwest::Url::parse(&options.server)?;
    ensure!(
        ["http", "https"].contains(&url.scheme())
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "use an HTTP(S) server URL without credentials, query, or fragment"
    );
    std::fs::create_dir_all(&options.work)?;
    options.work = compiler_path(&options.work)?;
    if let Some(profile) = &options.host_profile {
        options.host_profile = Some(compiler_path(profile)?);
    }
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let targets: Vec<crate::model::Target> = client
        .get(format!("{}/api/targets", options.server))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    for id in &options.targets {
        let target = targets
            .iter()
            .find(|t| &t.id == id)
            .with_context(|| format!("unknown target: {id}"))?;
        ensure!(
            target.runner_os == runner_os(),
            "target {id} requires a {} worker, this is {}",
            target.runner_os,
            runner_os()
        );
    }
    tracing::info!(worker=%options.id, os=runner_os(), targets=?options.targets, "worker started; recipes run with this account's permissions");
    loop {
        let assignment: Option<Assignment> = client
            .post(format!("{}/api/jobs/claim", options.server))
            .bearer_auth(&options.token)
            .json(&Claim {
                worker_id: options.id.clone(),
                runner_os: runner_os().into(),
                target_ids: options.targets.clone(),
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let Some(assignment) = assignment else {
            if options.once {
                return Ok(());
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(options.poll_seconds)) => {},
                _ = tokio::signal::ctrl_c() => return Ok(()),
            }
            continue;
        };
        let folder = options.work.join(format!(
            "job-{}-{}",
            assignment.job.id,
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir(&folder)?;
        tracing::info!(job=assignment.job.id, reference=%assignment.job.reference, target=%assignment.job.target.id, "build started");
        let mut stopping = false;
        let result = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(options.timeout_seconds), build(&options, &assignment, &folder)) => {
                match result { Ok(result) => result, Err(_) => Err(anyhow::anyhow!("job exceeded {} seconds", options.timeout_seconds)) }
            },
            result = heartbeat_loop(&client, &options, &assignment) => result,
            _ = tokio::signal::ctrl_c() => { stopping = true; Err(anyhow::anyhow!("worker interrupted")) },
        };
        let success = result.is_ok();
        let message = match &result {
            Ok(()) => "built and uploaded".into(),
            Err(error) => format!("{error:#}"),
        };
        let message: String = message
            .replace(&options.token, "[redacted]")
            .chars()
            .take(8000)
            .collect();
        let report_result = client
            .post(format!(
                "{}/api/jobs/{}/complete",
                options.server, assignment.job.id
            ))
            .bearer_auth(&options.token)
            .json(&Report {
                lease: assignment.lease,
                success,
                message: message.clone(),
            })
            .send()
            .await;
        // Discard cache credentials and build products; retain logs and graph JSON for diagnosis.
        if let Err(error) = tokio::fs::remove_dir_all(folder.join("conan-home")).await {
            tracing::warn!(%error, "could not remove job cache; remove it manually before reusing the worker");
        }
        report_result?.error_for_status()?;
        tracing::info!(job = assignment.job.id, success, message, "build finished");
        if options.once || stopping {
            return result;
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn resolved_working_directory_is_accepted_by_cmd() {
        let root = std::env::temp_dir().join(format!("conan path {}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = compiler_path(&root).unwrap();
        assert!(!path.to_string_lossy().starts_with(r"\\?\"));
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "cd"])
            .current_dir(&path)
            .output()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(output.status.success());
        assert!(
            output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("conan path"));
    }
}

async fn heartbeat_loop(client: &Client, options: &Options, assignment: &Assignment) -> Result<()> {
    loop {
        tokio::time::sleep(Duration::from_secs(20)).await;
        client
            .post(format!(
                "{}/api/jobs/{}/heartbeat",
                options.server, assignment.job.id
            ))
            .bearer_auth(&options.token)
            .json(&Report {
                lease: assignment.lease.clone(),
                success: false,
                message: String::new(),
            })
            .send()
            .await
            .context("worker heartbeat failed")?
            .error_for_status()
            .context("worker lease lost")?;
    }
}

async fn build(options: &Options, assignment: &Assignment, folder: &Path) -> Result<()> {
    command(
        options,
        folder,
        "detect",
        &["profile".into(), "detect".into(), "--force".into()],
        false,
    )
    .await?;
    if options.no_conancenter {
        command(
            options,
            folder,
            "disable-center",
            &["remote".into(), "disable".into(), "conancenter".into()],
            false,
        )
        .await?;
    }
    command(
        options,
        folder,
        "remote",
        &[
            "remote".into(),
            "add".into(),
            "forge".into(),
            options.server.clone(),
            "--index=0".into(),
        ],
        false,
    )
    .await?;
    let target = &assignment.job.target;
    let mut args = vec![
        "install".into(),
        format!("--requires={}", assignment.job.reference),
        "--build=missing".into(),
        "-pr:b=default".into(),
        format!(
            "-pr:h={}",
            options
                .host_profile
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "default".into())
        ),
        format!("-s:h=os={}", target.os),
        format!("-s:h=arch={}", target.arch),
        format!("-s:h=build_type={}", target.build_type),
        "--format=json".into(),
        "--out-file=graph.json".into(),
        "--output-folder=output".into(),
    ];
    if let Some(sdk) = &target.sdk {
        args.push(format!("-s:h=os.sdk={sdk}"));
    }
    if let Some(version) = &target.os_version {
        args.push(format!("-s:h=os.version={version}"));
    }
    command(options, folder, "build", &args, false).await?;
    // Authentication is only supplied to the upload phase, after executing the build.
    command(
        options,
        folder,
        "upload",
        &[
            "upload".into(),
            assignment.job.reference.clone(),
            "-r=forge".into(),
            "--confirm".into(),
            "--check".into(),
        ],
        true,
    )
    .await?;
    Ok(())
}

async fn command(
    options: &Options,
    folder: &Path,
    label: &str,
    args: &[String],
    authenticate: bool,
) -> Result<()> {
    let logfile = folder.join(format!("{label}.log"));
    let file = std::fs::File::create(&logfile)?;
    let mut cmd = Command::new(&options.conan);
    cmd.args(args)
        .current_dir(folder)
        .env("CONAN_HOME", folder.join("conan-home"))
        .env("CONAN_NON_INTERACTIVE", "1")
        .env_remove("CONAN_SERVER_WORKER_TOKEN")
        .env_remove("CONAN_SERVER_PUBLISH_TOKEN")
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .kill_on_drop(true);
    if authenticate {
        cmd.env("CONAN_LOGIN_USERNAME_FORGE", "worker")
            .env("CONAN_PASSWORD_FORGE", &options.token);
    }
    let status = cmd
        .spawn()
        .with_context(|| format!("start Conan ({label}); is Conan 2 installed?"))?
        .wait()
        .await?;
    if !status.success() {
        let mut file = tokio::fs::File::open(&logfile).await?;
        let length = file.metadata().await?.len();
        file.seek(std::io::SeekFrom::Start(length.saturating_sub(6000)))
            .await?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).await?;
        bail!(
            "Conan {label} failed ({status}):\n{}",
            String::from_utf8_lossy(&bytes)
        );
    }
    Ok(())
}
