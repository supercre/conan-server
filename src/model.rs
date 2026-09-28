use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub id: String,
    /// Operating system on which the worker runs: Linux, Macos, Windows.
    pub runner_os: String,
    /// Conan host OS; iOS targets run on Macos workers.
    pub os: String,
    pub arch: String,
    #[serde(default = "release")]
    pub build_type: String,
    #[serde(default)]
    pub sdk: Option<String>,
    #[serde(default)]
    pub os_version: Option<String>,
}
fn release() -> String {
    "Release".into()
}

pub fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.+".contains(&b))
}

pub fn validate_targets(targets: &[Target]) -> Result<()> {
    let mut seen = HashSet::new();
    for target in targets {
        ensure!(
            safe_component(&target.id) && seen.insert(&target.id),
            "invalid or duplicate target id"
        );
        ensure!(
            ["Linux", "Macos", "Windows"].contains(&target.runner_os.as_str()),
            "invalid runner_os"
        );
        ensure!(
            ["Linux", "Macos", "Windows", "iOS"].contains(&target.os.as_str()),
            "invalid target os"
        );
        ensure!(
            target.runner_os == target.os || (target.runner_os == "Macos" && target.os == "iOS"),
            "native workers are required (Macos also supports iOS)"
        );
        ensure!(
            ["armv8", "x86_64"].contains(&target.arch.as_str()),
            "unsupported architecture"
        );
        ensure!(
            ["Debug", "Release", "RelWithDebInfo", "MinSizeRel"]
                .contains(&target.build_type.as_str()),
            "invalid build_type"
        );
        if target.os == "iOS" {
            ensure!(
                matches!(target.sdk.as_deref(), Some("iphoneos" | "iphonesimulator")),
                "iOS requires an SDK"
            );
            ensure!(
                target.os_version.as_deref().is_some_and(
                    |v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                ),
                "iOS requires os_version"
            );
            ensure!(
                target.sdk.as_deref() != Some("iphoneos") || target.arch == "armv8",
                "iOS devices require armv8"
            );
        } else {
            ensure!(
                target.sdk.is_none() && target.os_version.is_none(),
                "SDK and os_version are currently iOS-only"
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Job {
    pub id: i64,
    pub reference: String,
    pub target: Target,
    pub status: String,
    pub attempts: i64,
    pub worker_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub message: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Claim {
    pub worker_id: String,
    pub runner_os: String,
    pub target_ids: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Assignment {
    pub job: Job,
    pub lease: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Report {
    pub lease: String,
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_and_ios_matrix() {
        for bad in ["", "..", ".", "a/b", "a\\b", "C:", "x%2fy", "hello\0"] {
            assert!(!safe_component(bad), "{bad:?}");
        }
        let targets: Vec<Target> =
            serde_json::from_str(include_str!("../examples/targets.json")).unwrap();
        validate_targets(&targets).unwrap();
        let mut ios = targets.into_iter().find(|t| t.os == "iOS").unwrap();
        ios.runner_os = "Linux".into();
        assert!(validate_targets(&[ios]).is_err());
    }
}
