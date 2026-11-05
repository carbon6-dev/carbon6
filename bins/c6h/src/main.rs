use std::path::PathBuf;

use anyhow::Context;
use c6_head::HeadConfig;
use clap::{Parser, Subcommand};
use tokio::signal::unix::{SignalKind, signal};
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
    /// Install an update this daemon downloaded (run as root by
    /// `c6h-update.service`; docs/updates.md §6).
    #[command(hide = true)]
    UpdateInstall { dir: PathBuf },
    /// Print a one-time worker enrollment token (valid for one hour).
    EnrollToken {
        /// Display name for the worker; defaults to its hostname.
        #[arg(long)]
        name: Option<String>,
    },
    /// Print a one-time link that signs a browser in to the web UI (valid
    /// for 15 minutes). Use it to sign in the first time, then add a passkey
    /// under Settings, or to recover after losing your passkeys.
    LoginLink,
    /// Print a new API token for scripts and the `c6` CLI. It is shown once;
    /// revoke it under Settings → Sign-in.
    ApiToken {
        /// What it is for.
        #[arg(default_value = "c6 CLI")]
        name: String,
        /// Limit it to one realm's data (e.g. home); without this it reaches
        /// every realm and carbon6's administration.
        #[arg(long)]
        realm: Option<String>,
    },
    /// Rebuild the head's data folder from a backup (docs/backups.md §6),
    /// with the head stopped: its databases as of a time, its blobs, CA,
    /// master key and config file.
    Restore {
        /// Where the backups are: a folder, or s3://bucket/prefix (with
        /// AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY set).
        #[arg(long)]
        from: String,
        /// As of when (2026-11-04T09:30:00Z, or 2026-11-04 09:30 in UTC);
        /// the newest without it.
        #[arg(long)]
        at: Option<String>,
        /// The folder to rebuild (empty or new); the config's data folder
        /// without it.
        #[arg(long)]
        to: Option<PathBuf>,
        /// A file holding your backup key (c6key1…); else C6_BACKUP_KEY, else
        /// it asks.
        #[arg(long)]
        key_file: Option<PathBuf>,
        /// s3: the endpoint (R2, MinIO…); AWS's for the region without it.
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long)]
        region: Option<String>,
    },
    /// Upgrade the head's database to this version without starting the
    /// head (starting it upgrades too). A backup is made first, and a failed
    /// upgrade restores it.
    Upgrade {
        /// Only try it: upgrade a copy and print what would change, leaving
        /// the database untouched.
        #[arg(long)]
        check: bool,
    },
    /// Take in a memex's knowledge (the `memex` folder in c6m's state
    /// folder): the memex is part of the head now. Stop the head first. The
    /// head does this by itself when c6m ran on this machine.
    ImportKnowledge {
        /// The memex folder, e.g. ~/Library/Application Support/c6m/memex.
        from: PathBuf,
        /// Copy it rather than move it (another disk).
        #[arg(long)]
        copy: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // The exact version this build is (docs/updates.md).
    c6_core::set_build(option_env!("C6_BUILD_VERSION"));
    {
        use tracing_subscriber::prelude::*;
        // The head's warnings and errors also go to the improvement
        // watcher (docs/watchers.md §6).
        tracing_subscriber::registry()
            .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
            .with(tracing_subscriber::fmt::layer())
            .with(c6_head::watchers::SignalLayer)
            .init();
    }
    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(c6_head::config::default_config_path);
    let cfg = HeadConfig::load(&path)?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            let auth = cfg.auth;
            let mut head = match c6_head::start(cfg).await {
                Ok(h) => h,
                // This version was rolled back: the previous one runs after a restart.
                Err(e) if e.is::<c6_head::updates::RolledBack>() => {
                    eprintln!("{e}");
                    std::process::exit(c6_core::update::RESTART_CODE);
                }
                Err(e) => return Err(e),
            };
            for url in &head.ui_urls {
                eprintln!("web UI: {url}");
            }
            if auth {
                eprintln!("sign in with a passkey, or run `{}` for a one-time link", head.state.auth.login_link_command);
            }
            let token = head.shutdown_token();
            // Ctrl-C, or SIGTERM from systemd, launchd or `docker stop`.
            let mut term = signal(SignalKind::terminate())?;
            tokio::spawn(async move {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                token.cancel();
            });
            let state = head.state.clone();
            head.wait().await?;
            // Updated (docs/updates.md): the service manager starts the new version.
            if state.updates.restart_requested() {
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
        Command::LoginLink => {
            for url in c6_head::offline_login_link(cfg).await.context("creating login link")? {
                println!("{url}");
            }
            eprintln!("Open one of these within 15 minutes; it works once. Then add a passkey under Settings → Sign-in.");
            Ok(())
        }
        Command::ApiToken { name, realm } => {
            println!("{}", c6_head::offline_api_token(cfg, &name, realm.as_deref()).await.context("creating API token")?);
            Ok(())
        }
        Command::Restore { from, at, to, key_file, endpoint, region } => {
            use c6_head::backups::restore;
            let source = restore::source(&from, endpoint.as_deref(), region.as_deref())?;
            let at = at.as_deref().map(restore::parse_time).transpose()?;
            let key = match (key_file, std::env::var("C6_BACKUP_KEY").ok().filter(|k| !k.trim().is_empty())) {
                (Some(f), _) => std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?,
                (None, Some(k)) => k,
                (None, None) => {
                    eprint!("Your backup key (c6key1…): ");
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line
                }
            };
            let to = to.unwrap_or_else(|| cfg.data_dir.clone());
            let r = restore::restore(source.as_ref(), key.trim(), at, &to).await?;
            print!("{}", r.text());
            anyhow::ensure!(r.ok(), "a database didn't check out; see above");
            eprintln!("Restored into {}. Start the head on it; workers trust it as before (point them at its address).", to.display());
            Ok(())
        }
        Command::Upgrade { check } => {
            let path = cfg.data_dir.join("c6h.db");
            if check {
                let report = c6_head::store::check_upgrade(&path).await?;
                print!("{}", report.text());
                if !report.applied.is_empty() {
                    eprintln!("(a check: {} is unchanged)", path.display());
                }
                return Ok(());
            }
            let (_, report) = c6_head::store::Store::open_reporting(&path).await?;
            match report {
                Some(r) => print!("{}", r.text()),
                None => println!("{}: up to date", path.display()),
            }
            Ok(())
        }
        Command::ImportKnowledge { from, copy } => {
            println!("{}", c6_head::knowledge::import(&cfg, &from, copy).await?);
            Ok(())
        }
        Command::EnrollToken { name } => {
            let enrollment = c6_head::offline_enrollment(cfg, name.as_deref()).await.context("creating enrollment")?;
            println!("c6w enroll {}", enrollment.token);
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
