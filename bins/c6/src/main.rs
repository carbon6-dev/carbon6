//! `c6`: carbon6 from the command line. Runs agent sessions headless
//! (stream the reply, exit with its status), sends follow-ups, lists and
//! searches sessions. Talks to the head's API with an API token.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use c6_api::{
    Connection, ContentBlock, Event, EventKind, QueuedInput, SearchResponse, Session, SessionDetail, SessionStatus, TurnEndReason, Worker,
    WorkerStatus,
};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// carbon6 from the command line.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// The head's web address (default: from `c6 login`, or C6_URL).
    #[arg(long, global = true)]
    url: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Save the head's address and an API token (create one under Settings
    /// → Sign-in, or with `c6h api-token` on the head).
    Login {
        url: String,
        #[arg(long)]
        token: String,
        /// The head's CA certificate (from /ca.pem), if it signs its own.
        #[arg(long)]
        ca: Option<PathBuf>,
    },
    /// Start a session and print the agent's reply as it works. Exits 1 if
    /// the turn fails.
    Run {
        /// The prompt; `-` (or nothing, with input piped) reads stdin. With
        /// --template, the template's {{input}}.
        prompt: Vec<String>,
        /// Run this template (one session, or one per target: a fleet run).
        #[arg(short, long)]
        template: Option<String>,
        /// Worker name or id (default: the only one online).
        #[arg(short, long)]
        worker: Option<String>,
        /// Folder on the worker (default: this folder).
        #[arg(short = 'C', long)]
        cwd: Option<String>,
        /// No folder: a scratch workspace of its own on the worker.
        #[arg(long, conflicts_with = "cwd")]
        scratch: bool,
        /// Work on this machine over SSH, through the worker (user@host or an
        /// ssh_config alias); -C is then a folder there (default: home).
        #[arg(long, conflicts_with = "scratch")]
        ssh: Option<String>,
        /// Connection name or id (default: from your routing rules, else
        /// that of your latest session).
        #[arg(short, long)]
        connection: Option<String>,
        /// Model (default: from your routing rules, else the connection's
        /// default, else that of your latest session on the connection).
        #[arg(short, long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Run in a new git worktree.
        #[arg(long)]
        worktree: bool,
        /// A label for the session (repeatable, or comma-separated).
        #[arg(short, long = "label")]
        label: Vec<String>,
        /// Print events as JSON lines instead of text.
        #[arg(long)]
        json: bool,
        /// Start it and print its id without waiting.
        #[arg(long)]
        detach: bool,
    },
    /// Send a message to a session and print the reply.
    Send {
        /// Session id (or a unique prefix).
        session: String,
        prompt: Vec<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        detach: bool,
    },
    /// Follow a session until it is idle.
    Wait {
        session: String,
        #[arg(long)]
        json: bool,
    },
    /// Print a session as Markdown.
    Show { session: String },
    /// Interrupt a session's turn.
    Stop { session: String },
    /// List sessions, newest first.
    Sessions {
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        /// Only sessions with this label (repeatable; e.g. repo:github.com/acme).
        #[arg(short, long = "label")]
        label: Vec<String>,
    },
    /// Search all sessions.
    Search {
        query: Vec<String>,
        /// Only sessions with this label (repeatable).
        #[arg(short, long = "label")]
        label: Vec<String>,
    },
    /// List workers.
    Workers,
    /// Find operations: everything you can do in carbon6 (with no words:
    /// the areas; with an operation's exact name: its input).
    Ops {
        words: Vec<String>,
        /// Only this area.
        #[arg(short, long)]
        area: Option<String>,
    },
    /// Run an operation (see `c6 ops`). Inputs are key=value (JSON values,
    /// or text), or one JSON object; a body is body=<json> or body=@file.
    /// Prints the result as JSON.
    Op {
        name: String,
        input: Vec<String>,
        /// Run a destructive operation without asking.
        #[arg(short, long)]
        yes: bool,
    },
}

#[derive(Default, Serialize, Deserialize)]
struct Config {
    url: String,
    token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ca: Option<PathBuf>,
}

fn config_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("c6").join("config.toml")
}

fn load_config() -> Config {
    let mut c: Config = std::fs::read_to_string(config_path()).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default();
    if let Ok(u) = std::env::var("C6_URL") {
        c.url = u;
    }
    if let Ok(t) = std::env::var("C6_TOKEN") {
        c.token = t;
    }
    if let Ok(ca) = std::env::var("C6_CA") {
        c.ca = Some(ca.into());
    }
    c
}

struct Client {
    http: reqwest::Client,
    api: String,
    token: String,
}

impl Client {
    fn new(url: &str, token: &str, ca: Option<&PathBuf>) -> anyhow::Result<Self> {
        if url.is_empty() || token.is_empty() {
            bail!("not signed in: run `c6 login <url> --token <token>` (or set C6_URL and C6_TOKEN)");
        }
        let mut b = reqwest::Client::builder().timeout(Duration::from_secs(60));
        if let Some(ca) = ca {
            let pem = std::fs::read(ca).with_context(|| format!("reading {}", ca.display()))?;
            b = b.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
        }
        Ok(Self { http: b.build()?, api: format!("{}/api/v1", url.trim_end_matches('/')), token: token.to_owned() })
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
        let r = req.bearer_auth(&self.token).send().await.context("cannot reach the head")?;
        if r.status().is_success() {
            return Ok(r);
        }
        let status = r.status();
        let body = r.text().await.unwrap_or_default();
        let msg =
            serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| v["error"].as_str().map(str::to_owned)).unwrap_or(body);
        bail!("{status}: {msg}")
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        Ok(self.send(self.http.get(format!("{}{path}", self.api))).await?.json().await?)
    }

    async fn post<T: serde::de::DeserializeOwned>(&self, path: &str, body: &serde_json::Value) -> anyhow::Result<T> {
        Ok(self.send(self.http.post(format!("{}{path}", self.api)).json(body)).await?.json().await?)
    }

    /// A session id from an id or a unique prefix of one.
    async fn session_id(&self, s: &str) -> anyhow::Result<String> {
        if s.len() == 36 {
            return Ok(s.to_owned());
        }
        let all: Vec<Session> = self.get("/sessions").await?;
        let found: Vec<&Session> = all.iter().filter(|x| x.id.starts_with(s)).collect();
        match found.as_slice() {
            [one] => Ok(one.id.clone()),
            [] => bail!("no session starts with {s}"),
            _ => bail!("{s} matches {} sessions; give more of the id", found.len()),
        }
    }
}

/// The prompt from arguments, or stdin.
fn prompt_text(args: &[String]) -> anyhow::Result<String> {
    let joined = args.join(" ");
    if joined == "-" || (joined.is_empty() && !std::io::stdin().is_terminal()) {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        return Ok(s);
    }
    if joined.trim().is_empty() {
        bail!("give a prompt (or pipe one in)");
    }
    Ok(joined)
}

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("c6: {e:#}");
            std::process::exit(2);
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<i32> {
    if let Command::Login { url, token, ca } = &cli.command {
        let ca = ca.as_ref().map(|p| p.canonicalize()).transpose()?;
        let client = Client::new(url, token, ca.as_ref())?;
        let _: Vec<Worker> = client.get("/workers").await.context("checking the token")?;
        let path = config_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let text = toml::to_string(&Config { url: url.trim_end_matches('/').to_owned(), token: token.clone(), ca })?;
        write_private(&path, &text)?;
        eprintln!("Signed in to {url}; saved to {}", path.display());
        return Ok(0);
    }
    let cfg = load_config();
    let c = Client::new(cli.url.as_deref().unwrap_or(&cfg.url), &cfg.token, cfg.ca.as_ref())?;
    match cli.command {
        Command::Login { .. } => unreachable!(),
        Command::Run { prompt, template: Some(name), json, detach, .. } => {
            let input = if prompt.is_empty() && std::io::stdin().is_terminal() { String::new() } else { prompt_text(&prompt)? };
            let r: c6_api::TemplateRun = c.post(&format!("/templates/{name}/run"), &json!({"input": input})).await?;
            for e in &r.errors {
                eprintln!("could not start: {e}");
            }
            match r.sessions.as_slice() {
                [] => Ok(1),
                [one] if !detach => {
                    eprintln!("session {} · {}", one.id, one.title);
                    follow(&c, &one.id, 0, json).await
                }
                many if detach => {
                    for s in many {
                        println!("{}", s.id);
                    }
                    Ok(0)
                }
                many => fleet(&c, &r.run_id, many).await,
            }
        }
        Command::Run { prompt, worker, cwd, scratch, ssh, connection, model, effort, title, worktree, label, json, detach, .. } => {
            let labels = c6_api::labels::parse_labels(&label.join(","));
            let text = prompt_text(&prompt)?;
            let workers: Vec<Worker> = c.get("/workers").await?;
            let worker = pick_worker(&workers, worker.as_deref())?;
            let cwd = match cwd {
                Some(d) => d,
                None if scratch || ssh.is_some() => String::new(),
                None => std::env::current_dir()?.display().to_string(),
            };
            let connection = match connection {
                Some(w) => {
                    let connections: Vec<Connection> = c.get("/connections").await?;
                    connections.iter().find(|x| x.name == w || x.id == w).with_context(|| format!("no connection named {w}"))?.id.clone()
                }
                None => String::new(),
            };
            // Empty: the head routes it.
            let model = model.unwrap_or_default();
            let s: Session = c
                .post(
                    "/sessions",
                    &json!({"worker_id": worker.id, "cwd": cwd, "connection_id": connection, "model": model,
                            "title": title, "prompt": text, "effort": effort, "worktree": worktree, "labels": labels, "ssh": ssh}),
                )
                .await?;
            if detach {
                println!("{}", s.id);
                return Ok(0);
            }
            eprintln!("session {} · {} · {}", s.id, worker.name, s.model);
            follow(&c, &s.id, 0, json).await
        }
        Command::Send { session, prompt, json, detach } => {
            let id = c.session_id(&session).await?;
            let text = prompt_text(&prompt)?;
            let before: Vec<Event> = c.get(&format!("/sessions/{id}/events")).await?;
            let after = before.last().map_or(0, |e| e.seq);
            let q: QueuedInput = c.post(&format!("/sessions/{id}/messages"), &json!({"text": text})).await?;
            if let Some(r) = q.rejected {
                bail!("the worker refused it: {r}");
            }
            if detach {
                return Ok(0);
            }
            follow(&c, &id, after, json).await
        }
        Command::Wait { session, json } => {
            let id = c.session_id(&session).await?;
            let evs: Vec<Event> = c.get(&format!("/sessions/{id}/events")).await?;
            follow(&c, &id, evs.last().map_or(0, |e| e.seq), json).await
        }
        Command::Show { session } => {
            let id = c.session_id(&session).await?;
            let md = c.send(c.http.get(format!("{}/sessions/{id}/export", c.api))).await?.text().await?;
            print!("{md}");
            Ok(0)
        }
        Command::Stop { session } => {
            let id = c.session_id(&session).await?;
            c.send(c.http.post(format!("{}/sessions/{id}/interrupt", c.api))).await?;
            Ok(0)
        }
        Command::Sessions { limit, label } => {
            let wanted = c6_api::labels::parse_labels(&label.join(","));
            let mut all: Vec<Session> = c.get("/sessions").await?;
            all.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
            let matching = all.iter().filter(|s| {
                let labels: Vec<String> = s.labels.iter().chain(s.auto_labels.iter()).cloned().collect();
                s.parent_id.is_none() && c6_api::labels::applies(&wanted, &labels)
            });
            for s in matching.take(limit) {
                let tags = if s.labels.is_empty() { String::new() } else { format!("  [{}]", s.labels.join(", ")) };
                println!("{}  {:<8} {:<22} {}{tags}", &s.id[..13], status_word(s.status), trim(&s.model, 22), s.title);
            }
            Ok(0)
        }
        Command::Search { query, label } => {
            let labels = c6_api::labels::parse_labels(&label.join(",")).join(",");
            let r: SearchResponse =
                c.send(c.http.get(format!("{}/search", c.api)).query(&[("q", query.join(" ")), ("labels", labels)])).await?.json().await?;
            if !r.ignored.is_empty() {
                eprintln!("(ignored short words: {})", r.ignored.join(", "));
            }
            for res in &r.results {
                println!("{}  {}  ({} matches)", &res.session.id[..13], res.session.title, res.total_hits);
                for h in &res.hits {
                    let snippet: String =
                        h.snippet.iter().map(|p| if p.hit { format!("\x1b[1m{}\x1b[0m", p.text) } else { p.text.clone() }).collect();
                    println!("    {:<11} {}", h.kind, snippet.replace('\n', " "));
                }
            }
            Ok(0)
        }
        Command::Ops { words, area } => {
            let catalog: serde_json::Value = c.get("/catalog").await?;
            let ops = catalog["operations"].as_array().cloned().unwrap_or_default();
            if let [name] = words.as_slice()
                && let Some(op) = ops.iter().find(|o| o["op"] == name.as_str())
            {
                println!("{}", serde_json::to_string_pretty(op)?);
                return Ok(0);
            }
            let wanted: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
            if wanted.is_empty() && area.is_none() {
                for a in catalog["areas"].as_array().into_iter().flatten() {
                    let n = ops.iter().filter(|o| o["area"] == a["area"]).count();
                    println!("{:<14} {:>3}  {}", a["area"].as_str().unwrap_or(""), n, a["what"].as_str().unwrap_or(""));
                }
                println!("\n`c6 ops <words>` or `c6 ops --area <area>` to see operations; `c6 ops <name>` for one's input.");
                return Ok(0);
            }
            for o in &ops {
                let text = format!("{} {}", o["op"].as_str().unwrap_or(""), o["summary"].as_str().unwrap_or("")).to_lowercase();
                if area.as_deref().is_some_and(|a| o["area"] != a) || !wanted.iter().all(|w| text.contains(w.as_str())) {
                    continue;
                }
                let effect = match o["effect"].as_str().unwrap_or("") {
                    "read" => "",
                    "write" => " ✎",
                    "destructive" => " ⚠",
                    _ => " 🔑",
                };
                println!("{:<32} {}{effect}", o["op"].as_str().unwrap_or(""), o["summary"].as_str().unwrap_or(""));
            }
            Ok(0)
        }
        Command::Op { name, input, yes } => {
            let input = op_input(&input)?;
            let call = |confirmed: bool| json!({"op": name, "input": input, "confirmed": confirmed});
            let r = c
                .http
                .post(format!("{}/catalog/call", c.api))
                .bearer_auth(&c.token)
                .json(&call(yes))
                .send()
                .await
                .context("cannot reach the head")?;
            let r = if r.status() == reqwest::StatusCode::CONFLICT {
                let why: serde_json::Value = r.json().await.unwrap_or_default();
                eprintln!("{}", why["error"].as_str().unwrap_or("this operation is destructive"));
                if !std::io::stdin().is_terminal() {
                    bail!("run it with --yes to confirm");
                }
                eprint!("Run it? [y/N] ");
                std::io::stderr().flush()?;
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim(), "y" | "Y" | "yes") {
                    eprintln!("Not run.");
                    return Ok(1);
                }
                c.send(c.http.post(format!("{}/catalog/call", c.api)).json(&call(true))).await?
            } else if r.status().is_success() {
                r
            } else {
                let status = r.status();
                let body: serde_json::Value = r.json().await.unwrap_or_default();
                bail!("{status}: {}", body["error"].as_str().unwrap_or(""));
            };
            let v: serde_json::Value = r.json().await.unwrap_or(serde_json::Value::Null);
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(0)
        }
        Command::Workers => {
            let workers: Vec<Worker> = c.get("/workers").await?;
            for w in workers {
                println!(
                    "{}  {:<8} {} ({}/{})",
                    w.id,
                    if w.status == WorkerStatus::Online { "online" } else { "offline" },
                    w.name,
                    w.os,
                    w.arch
                );
            }
            Ok(0)
        }
    }
}

fn write_private(path: &std::path::Path, text: &str) -> anyhow::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(path)?.write_all(text.as_bytes())?;
    Ok(())
}

fn pick_worker<'a>(workers: &'a [Worker], wanted: Option<&str>) -> anyhow::Result<&'a Worker> {
    if let Some(w) = wanted {
        return workers
            .iter()
            .find(|x| x.name == w || x.id.to_string() == w || x.id.to_string().starts_with(w))
            .with_context(|| format!("no worker named {w}"));
    }
    let online: Vec<&Worker> = workers.iter().filter(|w| w.status == WorkerStatus::Online).collect();
    match online.as_slice() {
        [one] => Ok(one),
        [] => bail!("no worker is online"),
        many => bail!(
            "several workers are online ({}); pick one with --worker",
            many.iter().map(|w| w.name.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

fn status_word(s: SessionStatus) -> &'static str {
    match s {
        SessionStatus::Starting => "starting",
        SessionStatus::Idle => "idle",
        SessionStatus::Running => "running",
        SessionStatus::Failed => "failed",
    }
}

fn trim(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_owned() } else { format!("{}…", s.chars().take(n - 1).collect::<String>()) }
}

/// Prints the session's events after `after` until it is idle; the exit
/// code reflects how its last turn ended. Ctrl-C interrupts the turn.
async fn follow(c: &Client, id: &str, mut after: u64, json: bool) -> anyhow::Result<i32> {
    let mut ended: Option<TurnEndReason> = None;
    let mut tools = std::collections::HashMap::new();
    let mut out = std::io::stdout();
    // Ctrl-C interrupts the turn rather than leaving it running.
    let (http, url, token) = (c.http.clone(), format!("{}/sessions/{id}/interrupt", c.api), c.token.clone());
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = http.post(url).bearer_auth(token).send().await;
            eprintln!("\ninterrupted");
            std::process::exit(130);
        }
    });
    loop {
        let evs: Vec<Event> = c.get(&format!("/sessions/{id}/events?after={after}")).await?;
        for e in &evs {
            after = e.seq;
            if json {
                writeln!(out, "{}", serde_json::to_string(e)?)?;
                if let EventKind::TurnEnded { reason, .. } = &e.kind {
                    ended = Some(*reason);
                }
                continue;
            }
            match &e.kind {
                EventKind::Assistant { blocks, .. } => {
                    for b in blocks {
                        match b {
                            ContentBlock::Text { text } if !text.trim().is_empty() => writeln!(out, "{}", text.trim_end())?,
                            ContentBlock::ToolUse { id, name, input_json } => {
                                tools.insert(id.clone(), name.clone());
                                eprintln!("\x1b[2m→ {name} {}\x1b[0m", trim(&input_json.replace('\n', " "), 120));
                            }
                            ContentBlock::ServerTool { label } => eprintln!("\x1b[2m→ {label}\x1b[0m"),
                            _ => {}
                        }
                    }
                }
                EventKind::ToolResult { is_error: true, output, .. } => {
                    eprintln!("\x1b[2m  ✗ {}\x1b[0m", trim(output.lines().next().unwrap_or(""), 120))
                }
                EventKind::User { blocked, .. } if !blocked.is_empty() => eprintln!("blocked by a hook: {blocked}"),
                EventKind::Error { message } => eprintln!("error: {message}"),
                EventKind::Compaction { .. } => eprintln!("\x1b[2m(conversation compacted)\x1b[0m"),
                EventKind::TurnEnded { reason, .. } => ended = Some(*reason),
                _ => {}
            }
        }
        out.flush()?;
        let d: SessionDetail = c.get(&format!("/sessions/{id}")).await?;
        if d.session.status == SessionStatus::Failed {
            eprintln!("error: {}", d.session.error.unwrap_or_else(|| "the session failed to start".into()));
            return Ok(1);
        }
        let pending = d.queued.iter().any(|q| q.rejected.is_none());
        if d.session.status == SessionStatus::Idle && !pending && ended.is_some() && evs.is_empty() {
            return Ok(match ended {
                Some(TurnEndReason::Completed) => 0,
                Some(TurnEndReason::Interrupted) => 130,
                _ => 1,
            });
        }
        if evs.is_empty() {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

/// Waits for a fleet run's sessions and prints each one's last reply.
async fn fleet(c: &Client, run_id: &str, sessions: &[Session]) -> anyhow::Result<i32> {
    eprintln!("fleet run {run_id}: {} sessions", sessions.len());
    let mut code = 0;
    for s in sessions {
        // Wait until it has run and is idle (or failed).
        let reply = loop {
            let d: SessionDetail = c.get(&format!("/sessions/{}", s.id)).await?;
            let evs: Vec<Event> = c.get(&format!("/sessions/{}/events", s.id)).await?;
            let ended = evs.iter().rev().find_map(|e| match &e.kind {
                EventKind::TurnEnded { reason, .. } => Some(*reason),
                _ => None,
            });
            if d.session.status == SessionStatus::Failed {
                break (Some(TurnEndReason::Error), d.session.error.unwrap_or_default());
            }
            if d.session.status == SessionStatus::Idle && ended.is_some() && d.queued.is_empty() {
                let text = evs
                    .iter()
                    .rev()
                    .find_map(|e| match &e.kind {
                        EventKind::Assistant { blocks, .. } => blocks.iter().rev().find_map(|b| match b {
                            ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.clone()),
                            _ => None,
                        }),
                        _ => None,
                    })
                    .unwrap_or_default();
                break (ended, text);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        if reply.0 != Some(TurnEndReason::Completed) {
            code = 1;
        }
        println!("== {} ({})\n{}\n", s.title, &s.id[..13], reply.1.trim());
    }
    Ok(code)
}

/// An operation's input from `key=value` arguments (values as JSON when
/// they parse, else text; `body=@file` reads a file), or one JSON object.
fn op_input(args: &[String]) -> anyhow::Result<serde_json::Value> {
    if let [one] = args
        && one.trim_start().starts_with('{')
    {
        return serde_json::from_str(one).context("the input is not a JSON object");
    }
    let mut m = serde_json::Map::new();
    for a in args {
        let (k, v) = a.split_once('=').with_context(|| format!("{a}: inputs are key=value"))?;
        let text = match v.strip_prefix('@') {
            Some(path) => std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?,
            None => v.to_owned(),
        };
        let value = serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text));
        m.insert(k.to_owned(), value);
    }
    Ok(serde_json::Value::Object(m))
}
