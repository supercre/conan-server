mod model;
mod registry;
mod store;
mod worker;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::{net::SocketAddr, path::PathBuf};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the Conan registry and build queue. Does not execute recipes.
    Serve {
        #[arg(long, default_value = "127.0.0.1:9300")]
        listen: SocketAddr,
        #[arg(long, default_value = "data")]
        data: PathBuf,
        /// JSON file containing the build target matrix. Omit for registry-only mode.
        #[arg(long)]
        targets: Option<PathBuf>,
        #[arg(long, env = "CONAN_SERVER_PUBLISH_TOKEN", hide_env_values = true)]
        publish_token: String,
        #[arg(long, env = "CONAN_SERVER_WORKER_TOKEN", hide_env_values = true)]
        worker_token: String,
        #[arg(long, default_value_t = 2048)]
        max_upload_mib: u64,
    },
    /// Run trusted recipes on this machine. Use disposable, dedicated build machines.
    Worker(worker::Options),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    match Cli::parse().command {
        Command::Serve {
            listen,
            data,
            targets,
            publish_token,
            worker_token,
            max_upload_mib,
        } => {
            anyhow::ensure!(
                publish_token.len() >= 32 && worker_token.len() >= 32,
                "publish and worker tokens must each contain at least 32 characters"
            );
            anyhow::ensure!(
                publish_token != worker_token,
                "use different publish and worker tokens"
            );
            anyhow::ensure!(
                (1..=65536).contains(&max_upload_mib),
                "max-upload-mib must be between 1 and 65536"
            );
            let targets: Vec<model::Target> = match targets {
                Some(path) => serde_json::from_slice(&std::fs::read(path)?)?,
                None => Vec::new(),
            };
            model::validate_targets(&targets)?;
            let state = registry::State::new(
                data,
                targets,
                publish_token,
                worker_token,
                max_upload_mib * 1024 * 1024,
            )?;
            let listener = tokio::net::TcpListener::bind(listen)
                .await
                .context("bind registry")?;
            tracing::info!(address = %listener.local_addr()?, "Conan registry listening");
            axum::serve(listener, registry::router(state))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
        Command::Worker(options) => worker::run(options).await?,
    }
    Ok(())
}
