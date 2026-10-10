use std::path::PathBuf;

use anyhow::bail;
use c6_worker::{ConnState, Identity, conn, enroll, identity};
use clap::{Parser, Subcommand};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

/// carbon6 worker: runs agent sessions on this machine for the head.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Where the worker keeps its identity and caches.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Enroll with a head using a token from the web UI or `c6h enroll-token`.
    Enroll {
        token: String,
        /// Replace an existing identity.
        #[arg(long)]
        force: bool,
    },
    /// Attach to the head and serve until interrupted.
    Run,
    /// Show this worker's identity.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let cli = Cli::parse();
    let state_dir = cli.state_dir.unwrap_or_else(identity::default_state_dir);

    match cli.command {
        Command::Enroll { token, force } => {
            if Identity::exists(&state_dir) && !force {
                bail!("already enrolled (identity in {}); pass --force to replace it", identity::identity_dir(&state_dir).display());
            }
            let id = enroll::enroll(&token).await?;
            id.save(&state_dir)?;
            println!("enrolled as {} ({}) with head {}", id.meta.worker_name, id.meta.worker_id, id.meta.head_url);
            println!("start the worker with: c6w run");
            Ok(())
        }
        Command::Run => {
            let id = Identity::load(&state_dir)?;
            let (state_tx, _state_rx) = watch::channel(ConnState::Connecting);
            let stop = CancellationToken::new();
            let token = stop.clone();
            tokio::spawn(async move {
                tokio::signal::ctrl_c().await.ok();
                token.cancel();
            });
            conn::run(id, state_tx, stop).await
        }
        Command::Status => {
            let id = Identity::load(&state_dir)?;
            println!("worker:  {} ({})", id.meta.worker_name, id.meta.worker_id);
            println!("head:    {}", id.meta.head_url);
            println!("state:   {}", state_dir.display());
            Ok(())
        }
    }
}
