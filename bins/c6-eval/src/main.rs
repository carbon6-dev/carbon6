//! c6-eval: runs a suite of small coding tasks against a real model with the
//! carbon6 agent loop and tools, and reports pass rate, steps, time and
//! tokens. Use it to compare models, tool profiles, efforts and prompts.
//!
//! A task is a directory under `evals/tasks/<name>/` with `task.toml`
//! (`prompt`, optional `setup`, `check`, `timeout_secs`) and optional
//! `files/` copied into a fresh git repository the agent works in. The task
//! passes when `check` (a shell script run in that directory) exits 0.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use async_trait::async_trait;
use c6_agent::{BASE_PROMPT, Conversation, EventSink, TurnConfig, build_system_prompt, run_turn};
use c6_llm::{Message, Provider, ToolProfile, model_info};
use c6_proto::{SessionEvent, TurnEndReason, ephemeral_event, session_event};
use c6_tools::SessionTools;
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(version, about = "Run carbon6's eval suite against a model")]
struct Cli {
    /// Connection kind: openai_compat, openai, anthropic.
    #[arg(long, default_value = "openai_compat")]
    kind: String,
    #[arg(long)]
    base_url: String,
    /// Environment variable holding the API key (keys never go on the command line).
    #[arg(long)]
    api_key_env: Option<String>,
    #[arg(long)]
    model: String,
    /// Tool profile: auto (by model), generic, claude, codex.
    #[arg(long, default_value = "auto")]
    profile: String,
    #[arg(long)]
    effort: Option<String>,
    /// Task directory.
    #[arg(long, default_value = "evals/tasks")]
    tasks: PathBuf,
    /// Only these tasks (comma-separated).
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// Write results as JSON here.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Keep each task's working directory for inspection.
    #[arg(long)]
    keep: bool,
}

#[derive(Deserialize)]
struct TaskSpec {
    prompt: String,
    #[serde(default)]
    setup: Option<String>,
    check: String,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}

fn default_timeout() -> u64 {
    600
}

#[derive(Serialize)]
struct Outcome {
    task: String,
    passed: bool,
    turn_end: String,
    model_calls: usize,
    tool_calls: usize,
    tool_errors: usize,
    input_tokens: u64,
    cache_read_tokens: u64,
    output_tokens: u64,
    seconds: f64,
    check_output: String,
    workdir: Option<String>,
}

#[derive(Serialize)]
struct Report {
    model: String,
    kind: String,
    profile: String,
    effort: Option<String>,
    date: String,
    passed: usize,
    total: usize,
    outcomes: Vec<Outcome>,
}

#[derive(Default)]
struct Recorder(Mutex<Vec<SessionEvent>>);

#[async_trait]
impl EventSink for Recorder {
    async fn append(&self, kind: session_event::Kind) -> anyhow::Result<u64> {
        let mut v = self.0.lock().unwrap();
        let seq = v.len() as u64 + 1;
        v.push(SessionEvent { seq, ts_ms: 0, kind: Some(kind) });
        Ok(seq)
    }

    fn ephemeral(&self, _: ephemeral_event::Kind) {}
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    c6_llm::install_crypto_provider();
    let key = match &cli.api_key_env {
        Some(var) => std::env::var(var).with_context(|| format!("{var} is not set"))?,
        None => String::new(),
    };
    let client: Arc<dyn c6_llm::ModelClient> = Arc::from(c6_llm::client_for(&cli.kind, &cli.base_url, &key, HashMap::new())?);
    let provider = Provider::from_kind(&cli.kind).context("unknown kind")?;
    let info = model_info(provider, &cli.model);
    let profile = match cli.profile.as_str() {
        "auto" => info.tool_profile,
        p => ToolProfile::parse(p),
    };

    let mut names: Vec<String> = std::fs::read_dir(&cli.tasks)
        .with_context(|| format!("reading {}", cli.tasks.display()))?
        .flatten()
        .filter(|e| e.path().join("task.toml").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| cli.only.is_empty() || cli.only.contains(n))
        .collect();
    names.sort();
    if names.is_empty() {
        bail!("no tasks found");
    }

    eprintln!(
        "{} task(s) · {} · profile {} · effort {}",
        names.len(),
        cli.model,
        profile.as_str(),
        cli.effort.as_deref().unwrap_or("default")
    );
    let mut outcomes = Vec::new();
    for name in &names {
        let endpoint =
            c6_llm::pool::Endpoint { kind: cli.kind.clone(), base_url: cli.base_url.clone(), bearer: key.clone(), ..Default::default() };
        let window = c6_llm::window::known(&cli.base_url, &cli.model, &endpoint).await.unwrap_or(info.context_window);
        let o = run_task(&cli, &client, profile, window, &cli.tasks.join(name), name).await?;
        eprintln!(
            "  {} {:<24} {:>3} calls {:>3} tools {:>7.1}s  {}",
            if o.passed { "PASS" } else { "FAIL" },
            name,
            o.model_calls,
            o.tool_calls,
            o.seconds,
            if o.passed { String::new() } else { o.check_output.lines().last().unwrap_or("").to_owned() }
        );
        outcomes.push(o);
    }
    let passed = outcomes.iter().filter(|o| o.passed).count();
    let report = Report {
        model: cli.model.clone(),
        kind: cli.kind.clone(),
        profile: profile.as_str().into(),
        effort: cli.effort.clone(),
        date: time::OffsetDateTime::now_utc().date().to_string(),
        passed,
        total: outcomes.len(),
        outcomes,
    };
    print_summary(&report);
    if let Some(out) = &cli.out {
        std::fs::write(out, serde_json::to_string_pretty(&report)?)?;
        eprintln!("results written to {}", out.display());
    }
    Ok(())
}

async fn run_task(
    cli: &Cli,
    client: &Arc<dyn c6_llm::ModelClient>,
    profile: ToolProfile,
    context_window: u64,
    dir: &Path,
    name: &str,
) -> anyhow::Result<Outcome> {
    let spec: TaskSpec = toml::from_str(&std::fs::read_to_string(dir.join("task.toml"))?).with_context(|| format!("{name}/task.toml"))?;
    let work = tempfile::Builder::new().prefix(&format!("c6-eval-{name}-")).tempdir()?;
    let wd = work.path().canonicalize()?;
    if dir.join("files").is_dir() {
        copy_dir(&dir.join("files"), &wd)?;
    }
    sh(&wd, "git init -q -b main && git add -A && git -c user.email=eval@c6 -c user.name=eval commit -qm fixture --allow-empty", 60)
        .await?;
    if let Some(setup) = &spec.setup {
        let (ok, out) = sh(&wd, setup, 300).await?;
        if !ok {
            bail!("{name}: setup failed: {out}");
        }
    }

    let date = time::OffsetDateTime::now_utc().date().to_string();
    let cfg = TurnConfig {
        model: cli.model.clone(),
        system_prompt: build_system_prompt(BASE_PROMPT, &wd, &date),
        max_output_tokens: None,
        cache_key: format!("eval-{name}"),
        effort: cli.effort.clone(),
        context_window,
        web_search: false,
        compact: Default::default(),
        compact_at: 0,
    };
    let tools: Vec<_> = c6_tools::tools_for(profile).into_iter().filter(|t| !c6_tools::Task::is_task(&t.spec().name)).collect();
    let recorder = Arc::new(Recorder::default());
    let sink: Arc<dyn EventSink> = recorder.clone();
    let session_tools = Arc::new(SessionTools::new(wd.clone()));
    let mut conv = Conversation { history: vec![Message::user_text(spec.prompt.clone())], context_tokens: 0 };
    let cancel = CancellationToken::new();
    let started = Instant::now();
    let turn = tokio::time::timeout(
        Duration::from_secs(spec.timeout_secs),
        run_turn(1, &cfg, client.as_ref(), &tools, &session_tools, &sink, &mut conv, &mut Vec::new, &cancel),
    )
    .await;
    let turn_end = match turn {
        Ok(Ok(r)) => format!("{r:?}"),
        Ok(Err(e)) => format!("error: {e:#}"),
        Err(_) => {
            cancel.cancel();
            "timeout".into()
        }
    };
    let seconds = started.elapsed().as_secs_f64();
    let (passed, check_output) = sh(&wd, &spec.check, 120).await?;

    let events = recorder.0.lock().unwrap().clone();
    let mut o = Outcome {
        task: name.into(),
        passed,
        turn_end,
        model_calls: 0,
        tool_calls: 0,
        tool_errors: 0,
        input_tokens: 0,
        cache_read_tokens: 0,
        output_tokens: 0,
        seconds,
        check_output: check_output.chars().rev().take(2000).collect::<String>().chars().rev().collect(),
        workdir: None,
    };
    for e in &events {
        match &e.kind {
            Some(session_event::Kind::Assistant(_)) => o.model_calls += 1,
            Some(session_event::Kind::ToolResult(r)) => {
                o.tool_calls += 1;
                o.tool_errors += r.is_error as usize;
            }
            Some(session_event::Kind::Usage(u)) => {
                o.input_tokens += u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
                o.cache_read_tokens += u.cache_read_tokens;
                o.output_tokens += u.output_tokens;
            }
            Some(session_event::Kind::TurnEnded(t)) if t.reason == TurnEndReason::Error as i32 => {}
            _ => {}
        }
    }
    if cli.keep {
        o.workdir = Some(work.keep().display().to_string());
    }
    Ok(o)
}

async fn sh(dir: &Path, script: &str, timeout_secs: u64) -> anyhow::Result<(bool, String)> {
    let child = tokio::process::Command::new("bash").arg("-c").arg(script).current_dir(dir).kill_on_drop(true).output();
    match tokio::time::timeout(Duration::from_secs(timeout_secs), child).await {
        Ok(out) => {
            let out = out?;
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            Ok((out.status.success(), text))
        }
        Err(_) => Ok((false, "check timed out".into())),
    }
}

fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    for e in std::fs::read_dir(from)?.flatten() {
        let dest = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            std::fs::create_dir_all(&dest)?;
            copy_dir(&e.path(), &dest)?;
        } else {
            std::fs::copy(e.path(), dest)?;
        }
    }
    Ok(())
}

fn print_summary(r: &Report) {
    let total_in: u64 = r.outcomes.iter().map(|o| o.input_tokens).sum();
    let cached: u64 = r.outcomes.iter().map(|o| o.cache_read_tokens).sum();
    let out: u64 = r.outcomes.iter().map(|o| o.output_tokens).sum();
    let secs: f64 = r.outcomes.iter().map(|o| o.seconds).sum();
    println!("\n| task | result | model calls | tool calls | tool errors | seconds |");
    println!("|---|---|---:|---:|---:|---:|");
    for o in &r.outcomes {
        println!(
            "| {} | {} | {} | {} | {} | {:.1} |",
            o.task,
            if o.passed { "pass" } else { "FAIL" },
            o.model_calls,
            o.tool_calls,
            o.tool_errors,
            o.seconds
        );
    }
    println!(
        "\n**{}/{} passed** · {} · profile {} · {:.0}s · {} input tokens ({}% cached) · {} output tokens",
        r.passed,
        r.total,
        r.model,
        r.profile,
        secs,
        total_in,
        (cached * 100).checked_div(total_in).unwrap_or(0),
        out
    );
}
