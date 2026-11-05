//! A work item's note when its session runs on Claude Code: the head asks
//! the `claude` on its own machine for the note (one call, locked down as
//! Ask is), and with none there it passes over that session for the
//! newest one whose model it can ask.

use std::path::{Path, PathBuf};

use c6_api::work::WorkItem;
use c6_api::{Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use serde_json::{Value, json};

const NOTE: &str = "Keep the user's note on a piece of work";

struct Env {
    head: c6_head::Head,
    api: String,
    http: reqwest::Client,
    mock: MockLlm,
    _dir: tempfile::TempDir,
}

async fn head(dir: &Path, claude_bin: PathBuf) -> c6_head::Head {
    c6_head::start(HeadConfig {
        claude_bin: Some(claude_bin),
        data_dir: dir.to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap()
}

impl Env {
    async fn new(claude_bin: PathBuf) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let head = head(dir.path(), claude_bin).await;
        let env = Self {
            api: format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port()),
            head,
            http: reqwest::Client::new(),
            mock: MockLlm::start().await,
            _dir: dir,
        };
        // Only the note asks a model: no guesses, names or suggestions.
        env.put("/settings/work", json!({"guess": false, "notes": true, "quiet_days": 3, "nudge": false, "needs_hours": 4})).await;
        env.put("/settings/suggestions", json!({"enabled": false, "catchup": false, "fold": false, "names": false})).await;
        env
    }

    /// The same head started again, with another `claude` (or none).
    async fn restart(self, claude_bin: PathBuf) -> Self {
        let Self { head: old, http, mock, _dir, .. } = self;
        old.shutdown().await.unwrap();
        let head = head(_dir.path(), claude_bin).await;
        Self { api: format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port()), head, http, mock, _dir }
    }

    async fn put(&self, path: &str, body: Value) {
        let r = self.http.put(format!("{}{path}", self.api)).json(&body).send().await.unwrap();
        assert!(r.status().is_success(), "{path}: {}", r.text().await.unwrap());
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let r = self.http.post(format!("{}{path}", self.api)).json(&body).send().await.unwrap();
        let ok = r.status().is_success();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(ok, "{path}: {v}");
        v
    }

    /// An Ask conversation on `connection`, once its first turn has ended.
    async fn conversation(&self, connection: &Value, model: &str, prompt: &str) -> Session {
        let s: Session = serde_json::from_value(
            self.post("/assistant", json!({"prompt": prompt, "connection": connection["id"], "model": model, "realm": "home"})).await,
        )
        .unwrap();
        eventually("the conversation's turn to end", async || {
            let events: Vec<Event> =
                self.http.get(format!("{}/sessions/{}/events", self.api, s.id)).send().await.ok()?.json().await.ok()?;
            events.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { .. })).then_some(())
        })
        .await;
        s
    }

    /// A work item made with these sessions (all at once, so its first note
    /// sees them all), once its note has been written.
    async fn noted(&self, sessions: &[&Session]) -> WorkItem {
        let attach: Vec<Value> = sessions.iter().map(|s| json!({"ref": format!("session:{}", s.id)})).collect();
        let w: WorkItem = serde_json::from_value(self.post("/work", json!({"title": "Harden retries", "attach": attach})).await).unwrap();
        eventually("the work item's note", async || {
            let w: WorkItem = self.http.get(format!("{}/work/{}", self.api, w.id)).send().await.ok()?.json().await.ok()?;
            (w.fields.note.is_some() && !c6_head::work::writing(&self.head.state).await).then_some(w)
        })
        .await
    }
}

/// A `claude` that answers one-off calls (`-p --output-format json`) with a
/// note, keeping what it was given, and fails a conversation's turn.
async fn fake_claude(dir: &Path) -> PathBuf {
    let bin = dir.join("claude");
    let note = json!({"done": "Read the logs", "left": "the fix", "next": "Patch the retry", "finished": false}).to_string();
    let result = json!({"type": "result", "subtype": "success", "is_error": false, "result": note,
                        "usage": {"input_tokens": 120, "output_tokens": 30}})
    .to_string();
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nd=$(dirname \"$0\")\ncase \" $* \" in *\" --input-format \"*) exit 1;; esac\n\
             printf '%s\\n' \"$@\" > \"$d/args.txt\"\npwd > \"$d/cwd.txt\"\ncat > \"$d/prompt.txt\"\nprintf '%s\\n' '{result}'\n"
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    c6_testkit::runnable(&bin, &["--version"]).await;
    bin
}

#[tokio::test]
async fn a_claude_code_session_s_work_gets_its_note_from_claude_on_the_head() {
    let bin_dir = tempfile::tempdir().unwrap();
    let env = Env::new(fake_claude(bin_dir.path()).await).await;
    let cc = env.post("/connections", json!({"name": "cc", "kind": "claude_code"})).await;
    let s = env.conversation(&cc, "haiku", "look into the retries").await;

    let w = env.noted(&[&s]).await;
    let note = w.fields.note.unwrap();
    assert_eq!((note.done.as_str(), note.next.as_str()), ("Read the logs", "Patch the retry"));
    let args = std::fs::read_to_string(bin_dir.path().join("args.txt")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    let at = |flag: &str| args.iter().position(|a| *a == flag);
    assert!(at("-p").is_some() && at("--output-format").map(|i| args[i + 1]) == Some("json"), "one call: {args:?}");
    assert_eq!(at("--model").map(|i| args[i + 1]), Some("haiku"), "the session's model: {args:?}");
    assert_eq!(at("--tools").map(|i| args[i + 1]), Some(""), "none of Claude Code's tools: {args:?}");
    assert!(at("--strict-mcp-config").is_some() && at("--disable-slash-commands").is_some(), "{args:?}");
    assert!(std::fs::read_to_string(bin_dir.path().join("prompt.txt")).unwrap().contains(NOTE), "the note's prompt on stdin");
    let cwd = std::fs::read_to_string(bin_dir.path().join("cwd.txt")).unwrap();
    assert!(!Path::new(cwd.trim()).exists(), "its own folder, removed after: {cwd}");
    assert!(env.mock.requests().is_empty(), "no API model was asked");
}

#[tokio::test]
async fn without_claude_on_the_head_the_note_uses_the_newest_session_it_can_ask() {
    let bin_dir = tempfile::tempdir().unwrap();
    let env = Env::new(fake_claude(bin_dir.path()).await).await;
    let m = env.post("/connections", json!({"name": "m", "base_url": env.mock.base_url()})).await;
    let cc = env.post("/connections", json!({"name": "cc", "kind": "claude_code"})).await;
    env.mock.push_about("check the retry logs", Reply::Text("The retries give up after one try.".into()));
    let older = env.conversation(&m, "small", "check the retry logs").await;
    let newer = env.conversation(&cc, "haiku", "look into the retries").await;
    // The head now runs where there's no `claude`: the newer session's
    // model is one it can't ask.
    let env = env.restart(bin_dir.path().join("no-claude-here")).await;
    let at = async |s: &Session| -> u64 {
        let v: Value = env.http.get(format!("{}/sessions/{}", env.api, s.id)).send().await.unwrap().json().await.unwrap();
        v["session"]["updated_at"].as_u64().unwrap()
    };
    assert!(at(&newer).await > at(&older).await, "the Claude Code session is the one that moved last");

    env.mock.push_about(
        NOTE,
        Reply::Text(json!({"done": "Found the cause", "left": "the fix", "next": "Raise the retry limit", "finished": false}).to_string()),
    );
    let w = env.noted(&[&older, &newer]).await;
    assert_eq!(w.fields.note.unwrap().next, "Raise the retry limit");
    let asked = env.mock.requests().into_iter().find(|r| r["messages"].to_string().contains(NOTE)).unwrap();
    assert_eq!(asked["model"], "small", "the older session's model, passing over Claude Code's");
}
