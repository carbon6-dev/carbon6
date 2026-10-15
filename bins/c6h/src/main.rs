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
    /// Upgrade the head's database to this version without starting the
    /// head (starting it upgrades too). A backup is made first, and a failed
    /// upgrade restores it.
    Upgrade {
        /// Only try it: upgrade a copy and print what would change, leaving
        /// the database untouched.
        #[arg(long)]
        check: bool,
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
            let auth = cfg.auth;
            let mut head = c6_head::start(cfg).await?;
            for url in &head.ui_urls {
                eprintln!("web UI: {url}");
            }
            if auth {
                eprintln!("sign in with a passkey, or run `c6h login-link` for a one-time link");
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
            head.wait().await
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
        Command::EnrollToken { name } => {
            let enrollment = c6_head::offline_enrollment(cfg, name.as_deref()).await.context("creating enrollment")?;
            println!("c6w enroll {}", enrollment.token);
            Ok(())
        }
    }
}
