//! This binary owns C6_CLAUDE_BIN. Recovery must retire a held CLI after a
//! journal failure, but distinguish a turn already sent from an unsent turn.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use c6_api::{EventKind, TurnEndReason};
use c6_proto::{ModelSpec, SessionCommand, UserInput, session_command, session_event};
use c6_testkit::eventually;
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const WARMUP: &str = "Hold this warmup turn at the reply gate.";
const ORIGINAL: &str = "Keep this exact request after the TurnStarted failure.";
const SECOND: &str = "Keep this second request in the same original batch.";
const UNOWNED: &str = "unowned-recovery-fixture";

struct StopFakeCli(PathBuf);

impl Drop for StopFakeCli {
    fn drop(&mut self) {
        for entry in std::fs::read_dir(&self.0).into_iter().flatten().flatten() {
            if entry.file_name() == UNOWNED {
                continue; // The stand-in is owned only by its child handle.
            }
            if let Ok(data) = std::fs::read(entry.path().join("meta.json"))
                && let Ok(meta) = serde_json::from_slice::<Value>(&data)
                && let Some(pgid) = meta["pgid"].as_i64().filter(|p| *p > 1)
            {
                let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pgid as i32), nix::sys::signal::Signal::SIGKILL);
            }
        }
    }
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

fn cli_inputs(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

async fn send_once(worker: &Worker, sid: &str, command_id: &str, text: &str, model: Option<ModelSpec>) {
    worker
        .sessions
        .handle(SessionCommand {
            session_id: sid.into(),
            command_id: command_id.into(),
            ownership_epoch: 0,
            kind: Some(session_command::Kind::Input(UserInput { text: text.into(), human_origin: true, model, ..Default::default() })),
        })
        .await
        .unwrap();
}

fn completed(event: &c6_proto::SessionEvent) -> bool {
    matches!(&event.kind, Some(session_event::Kind::TurnEnded(end)) if end.reason == c6_proto::TurnEndReason::Completed as i32)
}

async fn release_reply(gate: &Path) {
    let mut release = tokio::process::Command::new("sh")
        .args(["-c", "printf 'reply\\n' > \"$1\"", "release-gate"])
        .arg(gate)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    assert!(c6_testkit::within("the fake CLI to read its reply gate", release.wait()).await.unwrap().success());
}

#[tokio::test]
async fn journal_recovery_retires_a_held_cli_and_distinguishes_sent_and_unsent_turns() {
    let temp = tempfile::tempdir().unwrap();
    let gate = temp.path().join("reply");
    assert!(tokio::process::Command::new("mkfifo").arg(&gate).status().await.unwrap().success());
    let starts = temp.path().join("starts");
    let stdin = temp.path().join("stdin.jsonl");
    let batch_model = temp.path().join("batch-model");
    let cli = temp.path().join("claude");
    std::fs::write(
        &cli,
        format!(
            r#"#!/bin/sh
[ "$1" = --version ] && exit 0
model=default
while [ "$#" -gt 0 ]; do
  if [ "$1" = --model ]; then shift; model="$1"; fi
  shift
done
first=0
[ -f {starts} ] || first=1
echo start >> {starts}
while read -r line; do
  case "$line" in
    *'"control_request"'*)
      case "$line" in
        *'"subtype":"set_model"'*) model=$(printf '%s' "$line" | sed -n 's/.*"model":"\([^"]*\)".*/\1/p') ;;
      esac
      continue ;;
  esac
  printf '%s\n' "$line" >> {stdin}
  uuid=$(printf '%s' "$line" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p')
  printf '{{"type":"user","isReplay":true,"uuid":"%s","message":{{"role":"user","content":"echo"}}}}\n' "$uuid"
  case "$line" in
    *'{WARMUP}'*) read -r ignored < {gate} ;;
    *'{ORIGINAL}'*)
      printf '%s\n' "$model" > {batch_model}
      continue ;; # The next input completes this two-input turn.
  esac
  if [ "$first" = 1 ]; then
    read -r ignored < {gate}
    printf '%s\n' '{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"native-call","name":"Task","input":{{"description":"Native child","prompt":"Look around"}}}}],"stop_reason":"tool_use"}}}}'
    printf '%s\n' '{{"type":"assistant","parent_tool_use_id":"native-call","message":{{"content":[{{"type":"text","text":"Native child was still running"}}],"stop_reason":""}}}}'
    text='fail-this-held-journal-append'
  else
    text='Reply from explicit recovery'
  fi
  printf '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"%s"}}],"stop_reason":"end_turn"}}}}\n' "$text"
  printf '%s\n' '{{"type":"result","subtype":"success","is_error":false,"usage":{{"input_tokens":1,"output_tokens":1}}}}'
done
"#,
            starts = shell_path(&starts),
            stdin = shell_path(&stdin),
            batch_model = shell_path(&batch_model),
            gate = shell_path(&gate),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: this binary has one test and sets the override before starting the worker.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", &cli) };
    c6_testkit::runnable(&cli, &["--version"]).await;
    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let held = state.path().join("claude").join("held");
    let _cleanup = StopFakeCli(held.clone());
    let head = c6_head::start(c6_head::HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    let enrollment = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&enrollment.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let connection: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"kind": "claude_code", "name": "cc", "base_url": ""}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let session: Value = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path(),
                      "connection_id": connection["id"], "model": "default", "prompt": "Finish the work"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let sid = session["id"].as_str().unwrap();
    let db = sqlx::SqlitePool::connect(&format!("sqlite:{}", state.path().join("journal.db").display())).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_held_reply BEFORE INSERT ON events
         WHEN instr(NEW.data, CAST('fail-this-held-journal-append' AS BLOB)) > 0
         BEGIN SELECT RAISE(FAIL, 'injected held reply failure'); END",
    )
    .execute(&db)
    .await
    .unwrap();
    eventually("the held Claude Code CLI to start", async || {
        (starts.exists() && std::fs::read_dir(&held).into_iter().flatten().flatten().any(|entry| entry.path().join("meta.json").exists()))
            .then_some(())
    })
    .await;

    // Add an unowned group to the first cleanup. It stays live across one
    // full attempt, then exits on its own during the next attempt. The actor
    // must neither signal it nor launch a replacement before it has stopped.
    let mut unowned = tokio::process::Command::new("/bin/sh")
        .args(["-c", r#"printf 'ready\n'; read -r release; [ "$release" = quit ]"#])
        .process_group(0)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut readiness = BufReader::new(unowned.stdout.take().unwrap()).lines();
    assert_eq!(c6_testkit::within("unowned group readiness", readiness.next_line()).await.unwrap().as_deref(), Some("ready"));
    let unowned_pgid = unowned.id().unwrap() as i32;
    let unowned_record = held.join(UNOWNED);
    std::fs::create_dir(&unowned_record).unwrap();
    std::fs::write(
        unowned_record.join("meta.json"),
        serde_json::to_vec(&json!({"session": sid, "conversation": UNOWNED, "pgid": unowned_pgid, "keeper": unowned_pgid})).unwrap(),
    )
    .unwrap();
    release_reply(&gate).await;
    let retiring = unowned_record.join("retiring");
    eventually("the first attempt to encounter the unowned group", async || retiring.exists().then_some(())).await;
    // This deliberately unadoptable, differently named record is test-only.
    // Recreating the marker causes an observable edge on the next attempt.
    std::fs::remove_file(&retiring).unwrap();
    eventually("the supervisor to retry cleanup with the group still live", async || retiring.exists().then_some(())).await;
    assert!(unowned.try_wait().unwrap().is_none(), "recovery must not signal a group without ownership");
    assert!(worker.journal.claude_processes(sid).await.unwrap().contains(&unowned_pgid));
    assert_eq!(std::fs::read_to_string(&starts).unwrap().lines().count(), 1, "the unowned group kept the replacement fenced");
    unowned.stdin.take().unwrap().write_all(b"quit\n").await.unwrap();
    assert!(c6_testkit::within("the unowned group to exit without signals", unowned.wait()).await.unwrap().success());

    let events = eventually("explicit recovery without another message", async || {
        let events: Vec<c6_api::Event> = http.get(format!("{api}/sessions/{sid}/events")).send().await.unwrap().json().await.unwrap();
        events.iter().any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. } if format!("{blocks:?}").contains("Reply from explicit recovery"))).then_some(events)
    }).await;
    assert!(!unowned_record.exists(), "fresh absence clears the old held record");
    assert!(!worker.journal.claude_processes(sid).await.unwrap().contains(&unowned_pgid));
    assert!(events.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { reason: TurnEndReason::Error, .. })));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(&e.kind, EventKind::User { command_id, .. } if command_id.starts_with("resume-after-failure-")))
            .count(),
        1
    );
    assert_eq!(std::fs::read_to_string(&starts).unwrap().lines().count(), 2);
    let child = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::SubagentSpawned { child_session_id, .. } => Some(child_session_id),
            _ => None,
        })
        .expect("the fake CLI's native child was mirrored");
    eventually("the retired CLI's native child to end with the journal failure cause", async || {
        let child_events: Vec<c6_api::Event> =
            http.get(format!("{api}/sessions/{child}/events")).send().await.unwrap().json().await.unwrap();
        (child_events.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { reason: TurnEndReason::Error, .. }))
            && child_events.iter().any(|e| matches!(&e.kind, EventKind::Error { message } if message.contains("journal write failed"))))
        .then_some(())
    })
    .await;
    let recovery_end = eventually("the replacement's successful turn to commit", async || {
        worker.journal.events_after(sid, 0, u32::MAX).await.unwrap().into_iter().rev().find(completed).map(|event| event.seq)
    })
    .await;
    let (held_record, held_pgid) = std::fs::read_dir(&held)
        .unwrap()
        .find_map(|entry| {
            let record = entry.unwrap().path().join("meta.json");
            let meta: Value = serde_json::from_slice(&std::fs::read(&record).ok()?).ok()?;
            (meta["session"] == sid).then(|| (record, meta["pgid"].as_i64().unwrap()))
        })
        .expect("the replacement has a held record");
    // Hold that replacement in a warmup turn. The first queued input changes
    // the model without changing the CLI launch settings, so neither input can
    // join the warmup; both must be drained in order into the following turn.
    let post_send_inputs = cli_inputs(&stdin);
    send_once(&worker, sid, "warmup-for-ordered-batch", WARMUP, None).await;
    eventually("the warmup TurnStarted to commit", async || {
        worker
            .journal
            .events_after(sid, recovery_end, u32::MAX)
            .await
            .unwrap()
            .iter()
            .any(|event| matches!(event.kind, Some(session_event::Kind::TurnStarted(_))))
            .then_some(())
    })
    .await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER fail_turn_start BEFORE UPDATE OF open_turn ON sessions
         WHEN NEW.id = '{}' AND NEW.open_turn IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'injected TurnStarted failure'); END",
        sid.replace('\'', "''")
    )))
    .execute(&db)
    .await
    .unwrap();
    let first_id = "input-that-never-reached-stdin";
    let second_id = "second-input-that-never-reached-stdin";
    let batch_user = |event: &c6_proto::SessionEvent| match &event.kind {
        Some(session_event::Kind::User(user)) if [first_id, second_id].contains(&user.command_id.as_str()) => Some(user.clone()),
        _ => None,
    };
    let (spec, _) = worker.journal.load_session(sid).await.unwrap().unwrap();
    let mut model = spec.model.unwrap();
    assert_ne!(model.model, "sonnet");
    model.model = "sonnet".into();
    // Deliver once at the worker command boundary. The head has no queued
    // command that could redeliver this input and mask a broken local retry.
    send_once(&worker, sid, first_id, ORIGINAL, Some(model)).await;
    send_once(&worker, sid, second_id, SECOND, None).await;
    eventually("both inputs to queue behind the model switch", async || {
        (worker.sessions.inbox_holds(sid, first_id) && worker.sessions.inbox_holds(sid, second_id)).then_some(())
    })
    .await;
    release_reply(&gate).await;
    let last_end = eventually("the warmup turn to commit", async || {
        worker.journal.events_after(sid, recovery_end, u32::MAX).await.unwrap().into_iter().find(completed).map(|event| event.seq)
    })
    .await;
    eventually("the faulted held CLI to retire", async || {
        // Recovery can publish another record at this same path while the
        // trigger is still installed. Wait for the original owner to go away.
        match std::fs::read(&held_record) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(()),
            Ok(bytes) => {
                let meta = serde_json::from_slice::<Value>(&bytes).ok()?;
                (meta["pgid"].as_i64()? != held_pgid).then_some(())
            }
            Err(e) => panic!("reading the held record: {e}"),
        }
    })
    .await;
    let pending = worker.journal.events_after(sid, last_end, u32::MAX).await.unwrap();
    assert!(!pending.iter().any(|event| matches!(event.kind, Some(session_event::Kind::TurnStarted(_)))));
    let before = cli_inputs(&stdin);
    assert_eq!(&before[..post_send_inputs.len()], post_send_inputs.as_slice());
    assert!(
        matches!(&before[post_send_inputs.len()..], [line] if line["message"]["content"].as_str() == Some(WARMUP)),
        "the retired CLI only got the warmup on stdin"
    );
    let receipts_before_retry = pending.iter().filter_map(&batch_user).count();
    sqlx::query("DROP TRIGGER fail_turn_start").execute(&db).await.unwrap();

    let replay_end = eventually("the replayed turn to complete without a new command", async || {
        worker.journal.events_after(sid, last_end, u32::MAX).await.unwrap().into_iter().find(completed).map(|event| event.seq)
    })
    .await;
    let inputs = cli_inputs(&stdin);
    let replacement_inputs: Vec<_> = inputs[before.len()..].iter().map(|line| line["message"]["content"].as_str().unwrap()).collect();
    assert!(
        matches!(replacement_inputs.as_slice(), [first, second] if first.ends_with(ORIGINAL) && *second == SECOND),
        "replacement stdin must contain the original inputs in order exactly once, not a generic continuation; got {replacement_inputs:?} \
         ({receipts_before_retry} original User receipts committed before retry)"
    );
    assert_eq!(receipts_before_retry, 0, "a failed turn boundary must roll back the entire unsent batch's receipts");
    assert_eq!(std::fs::read_to_string(&batch_model).unwrap().trim(), "sonnet", "retry must keep the requested model");

    // Only after proving autonomous recovery, use a later turn to check that
    // neither the journal nor the CLI receives another copy of the input.
    let later = "This is the next independent request.";
    let later_id = "input-after-recovery";
    send_once(&worker, sid, later_id, later, None).await;
    let recovered = eventually("the independent turn to complete", async || {
        let events = worker.journal.events_after(sid, last_end, u32::MAX).await.unwrap();
        events
            .iter()
            .skip_while(|event| !matches!(&event.kind, Some(session_event::Kind::User(user)) if user.command_id == later_id))
            .any(|event| event.seq > replay_end && completed(event))
            .then_some(events)
    })
    .await;
    assert_eq!(
        recovered.iter().filter_map(batch_user).map(|user| (user.command_id, user.text)).collect::<Vec<_>>(),
        [(first_id.into(), ORIGINAL.into()), (second_id.into(), SECOND.into())],
        "one journal User receipt per input in the original batch, in order"
    );
    assert!(
        !recovered.iter().any(
            |event| matches!(&event.kind, Some(session_event::Kind::User(user)) if user.command_id.starts_with("resume-after-failure-"))
        ),
        "an unsent turn needs replay, not a generic continuation"
    );
    let inputs = cli_inputs(&stdin);
    let new_inputs: Vec<_> = inputs[before.len()..].iter().map(|line| line["message"]["content"].as_str().unwrap()).collect();
    assert!(
        matches!(new_inputs.as_slice(), [first, second, third] if first.ends_with(ORIGINAL) && *second == SECOND && *third == later),
        "{new_inputs:?}"
    );

    // A report arriving while the next CLI turn is held has no head command
    // to redeliver in this test. Fail its queue write, then verify recovery
    // delivers that same notice once without another user input.
    let notice_report = "notice-arrived-during-held-turn";
    let notice_id = "subagent:held-turn-report";
    let before_notice = recovered.last().unwrap().seq;
    let stdin_before_notice = inputs.len();
    send_once(&worker, sid, "warmup-for-held-notice", WARMUP, None).await;
    eventually("the running CLI to take the held-notice warmup", async || {
        let captured = std::fs::read_to_string(&stdin).ok()?;
        let line: Value = serde_json::from_str(captured.lines().nth(stdin_before_notice)?).ok()?;
        (line["message"]["content"].as_str() == Some(WARMUP)).then_some(())
    })
    .await;
    sqlx::query(
        "CREATE TRIGGER fail_held_notice BEFORE INSERT ON events
         WHEN instr(NEW.data, CAST('notice-arrived-during-held-turn' AS BLOB)) > 0
         BEGIN SELECT RAISE(ABORT, 'injected held notice failure'); END",
    )
    .execute(&db)
    .await
    .unwrap();
    let failed = worker
        .sessions
        .handle(SessionCommand {
            session_id: sid.into(),
            command_id: "held-turn-report".into(),
            ownership_epoch: 0,
            kind: Some(session_command::Kind::DelegationEnded(c6_proto::DelegationEnded {
                child_session_id: "test-remote-child".into(),
                reason: c6_proto::TurnEndReason::Completed as i32,
                report: notice_report.into(),
                wake: true,
                ..Default::default()
            })),
        })
        .await
        .unwrap_err();
    assert!(failed.contains("injected held notice failure"), "{failed}");
    sqlx::query("DROP TRIGGER fail_held_notice").execute(&db).await.unwrap();

    let notice_events = eventually("the retained notice to be sent and its turn to complete", async || {
        let events = worker.journal.events_after(sid, before_notice, u32::MAX).await.unwrap();
        let receipt = events.iter().position(|e| {
            matches!(&e.kind, Some(session_event::Kind::User(user)) if user.command_id == notice_id && user.text.contains(notice_report))
        })?;
        events[receipt + 1..].iter().any(completed).then_some(events)
    })
    .await;
    assert_eq!(
        notice_events.iter().filter(|e| matches!(&e.kind, Some(session_event::Kind::User(user)) if user.command_id == notice_id)).count(),
        1
    );
    assert_eq!(
        cli_inputs(&stdin)
            .iter()
            .filter(|line| line["message"]["content"].as_str().is_some_and(|text| text.contains(notice_report)))
            .count(),
        1
    );
    stop.cancel();
    head.shutdown().await.unwrap();
}
