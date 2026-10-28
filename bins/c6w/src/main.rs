use std::path::PathBuf;

use anyhow::bail;
use c6_worker::{ConnState, Identity, conn, enroll, identity};
use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};
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
        /// Reach the head at this `host` or `host:port` instead of the
        /// address in the token (the head's certificate is still checked
        /// against the token's name).
        #[arg(long, value_name = "HOST[:PORT]")]
        head: Option<String>,
    },
    /// Attach to the head and serve until interrupted.
    Run,
    /// Show this worker's identity.
    Status,
    /// Install an update this daemon downloaded (run as root by
    /// `c6w-update.service`; docs/updates.md §6).
    #[command(hide = true)]
    UpdateInstall { dir: PathBuf },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The exact version this build is (docs/updates.md).
    c6_core::set_build(option_env!("C6_BUILD_VERSION"));
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let cli = Cli::parse();
    let state_dir = cli.state_dir.unwrap_or_else(identity::default_state_dir);

    match cli.command {
        Command::Enroll { token, force, head } => {
            if Identity::exists(&state_dir) && !force {
                bail!("already enrolled (identity in {}); pass --force to replace it", identity::identity_dir(&state_dir).display());
            }
            let id = enroll::enroll_via(&token, head.as_deref()).await?;
            id.save(&state_dir)?;
            println!("enrolled as {} ({}) with head {}", id.meta.worker_name, id.meta.worker_id, id.meta.head_url);
            println!("start the worker with: c6w run");
            Ok(())
        }
        Command::Run => {
            let worker = c6_worker::Worker::open(&state_dir, Identity::load(&state_dir)?).await?;
            let (state_tx, _state_rx) = watch::channel(ConnState::Connecting);
            let stop = CancellationToken::new();
            let token = stop.clone();
            // Ctrl-C, or SIGTERM from systemd, launchd or `docker stop`.
            let mut term = signal(SignalKind::terminate())?;
            tokio::spawn(async move {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                token.cancel();
            });
            match conn::run(worker.clone(), state_tx, stop).await {
                // This version was rolled back: the previous one runs after a restart.
                Err(e) if e.is::<c6_worker::update::RolledBack>() => {
                    eprintln!("{e}");
                    std::process::exit(c6_core::update::RESTART_CODE);
                }
                r => r?,
            }
            // Updated (docs/updates.md): the service manager starts the new version.
            if worker.update.restart_requested() {
                restart();
            }
            Ok(())
        }
        Command::UpdateInstall { dir } => {
            let r = c6_core::update::install_requested(&dir, c6_core::build(), c6_core::update::RELEASE_KEYS);
            match r {
                Ok(m) => {
                    println!("{m}");
                    Ok(())
                }
                Err(e) => anyhow::bail!(e),
            }
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

/// Exits so the new version runs: with the restart code under a service
/// manager (systemd, launchd), else by running it in this process's place.
fn restart() -> ! {
    let supervised = ["INVOCATION_ID", "XPC_SERVICE_NAME", "LAUNCHD_SOCKET"].iter().any(|v| std::env::var_os(v).is_some_and(|x| x != "0"));
    // As it was started (a brew symlink now points at the new version).
    if !supervised && let Some(exe) = std::env::args_os().next() {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(exe).args(std::env::args_os().skip(1)).exec();
        eprintln!("restarting: {err}");
    }
    std::process::exit(c6_core::update::RESTART_CODE)
}
