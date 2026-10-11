use std::path::PathBuf;

use anyhow::bail;
use c6_worker::{ConnState, Identity, conn, enroll, identity};
use clap::{Parser, Subcommand};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

/// carbon6 memex: your knowledge service, on the c6 fabric.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Where the memex keeps its identity and its store.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Enroll with a head using a token from Settings → Memex.
    Enroll {
        token: String,
        /// Replace an existing identity.
        #[arg(long)]
        force: bool,
    },
    /// Attach to the head and serve until interrupted.
    Run,
    /// Show this memex's identity.
    Status,
}

fn default_state_dir() -> PathBuf {
    let worker = identity::default_state_dir();
    worker.parent().map(|p| p.join("c6m")).unwrap_or(worker)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let cli = Cli::parse();
    let state_dir = cli.state_dir.unwrap_or_else(default_state_dir);

    match cli.command {
        Command::Enroll { token, force } => {
            if Identity::exists(&state_dir) && !force {
                bail!("already enrolled (identity in {}); pass --force to replace it", identity::identity_dir(&state_dir).display());
            }
            let id = enroll::enroll(&token).await?;
            id.save(&state_dir)?;
            println!("enrolled as {} ({}) with head {}", id.meta.worker_name, id.meta.worker_id, id.meta.head_url);
            println!("start the memex with: c6m run");
            Ok(())
        }
        Command::Run => {
            // The memex is a fabric node: the worker's connection, with the
            // memex's methods on top (it runs no sessions).
            let node = c6_worker::Worker::open(&state_dir, Identity::load(&state_dir)?).await?;
            let memex = c6_memex::Memex::open(&state_dir.join("memex"), node.link.clone()).await?;
            memex.spawn_jobs();
            node.set_extension(memex);
            let (state_tx, _state_rx) = watch::channel(ConnState::Connecting);
            let stop = CancellationToken::new();
            let token = stop.clone();
            tokio::spawn(async move {
                tokio::signal::ctrl_c().await.ok();
                token.cancel();
            });
            conn::run(node, state_tx, stop).await
        }
        Command::Status => {
            let id = Identity::load(&state_dir)?;
            println!("memex:  {} ({})", id.meta.worker_name, id.meta.worker_id);
            println!("head:   {}", id.meta.head_url);
            println!("state:  {}", state_dir.display());
            Ok(())
        }
    }
}
