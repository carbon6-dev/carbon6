use std::path::PathBuf;

use anyhow::Context;
use c6_head::HeadConfig;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// carbon6 head: manages workers and serves the web UI.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Path to c6h.toml (defaults apply if it does not exist).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the head (the default).
    Serve,
    /// Print a one-time worker enrollment token (valid for one hour).
    EnrollToken {
        /// Display name for the worker; defaults to its hostname.
        #[arg(long)]
        name: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(c6_head::config::default_config_path);
    let cfg = HeadConfig::load(&path)?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            let mut head = c6_head::start(cfg).await?;
            for addr in &head.ui_addrs {
                eprintln!("web UI: http://{addr}");
            }
            let token = head.shutdown_token();
            tokio::spawn(async move {
                tokio::signal::ctrl_c().await.ok();
                token.cancel();
            });
            head.wait().await
        }
        Command::EnrollToken { name } => {
            let enrollment = c6_head::offline_enrollment(cfg, name.as_deref()).await.context("creating enrollment")?;
            println!("c6w enroll {}", enrollment.token);
            Ok(())
        }
    }
}
