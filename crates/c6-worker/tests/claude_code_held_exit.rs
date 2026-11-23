//! Held wrappers must be observed and reaped even when no exit-code file is written.
//! Own binary: C6_CLAUDE_BIN is process-global; no real CLI or live services.
use c6_api::{Event, EventKind, TurnEndReason};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually, eventually_or, within};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use nix::{
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

struct Cleanup(std::path::PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for entry in std::fs::read_dir(&self.0).into_iter().flatten().flatten() {
            if let Ok(bytes) = std::fs::read(entry.path().join("meta.json"))
                && let Ok(meta) = serde_json::from_slice::<Value>(&bytes)
                && let Some(pg) = meta["pgid"].as_i64()
            {
                let _ = killpg(Pid::from_raw(pg as i32), Signal::SIGKILL);
            }
        }
    }
}

#[tokio::test]
async fn held_exit_fails_the_running_turn_and_reaps_the_wrapper() {
    let bins = tempfile::tempdir().unwrap();
    let claude = bins.path().join("claude");
    std::fs::write(&claude, r#"#!/bin/sh
[ "$1" = --version ] && exit 0
while read -r line; do
  case "$line" in *'"control_request"'*) continue;; esac
  printf '%s\n' '{"type":"system","subtype":"init"}'
  uuid=$(printf '%s' "$line" | sed -n 's/.*"uuid":"\([^"]*\)".*/\1/p')
  printf '{"type":"user","isReplay":true,"uuid":"%s"}\n' "$uuid"
  printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Working."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}'
  case "$line" in *IDLE*) printf '%s\n' '{"type":"result","subtype":"success","usage":{}}';; esac
  read -r gate < gate
  printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Final output before exit."}],"stop_reason":"end_turn"},"parent_tool_use_id":null}'
  printf '%s\n' 'Held CLI stderr detail.' >&2
  exit 17
done
"#).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    // SAFETY: this binary has one test and owns its stand-in CLI.
    unsafe { std::env::set_var("C6_CLAUDE_BIN", &claude) };
    c6_testkit::runnable(&claude, &["--version"]).await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let head = c6_head::start(HeadConfig {
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
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("worker attached", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let cc: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"kind":"claude_code","name":"cc","base_url":""}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let held = state.path().join("claude/held");
    let _cleanup = Cleanup(held.clone());
    for mode in ["cli_exit", "running_signal", "idle_signal"] {
        let idle = mode == "idle_signal";
        let wrapper_signal = mode != "cli_exit";
        let project = tempfile::tempdir().unwrap();
        nix::unistd::mkfifo(&project.path().join("gate"), nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR).unwrap();
        let s: Value = http
            .post(format!("{api}/sessions"))
            .json(&json!({"worker_id":worker.identity.meta.worker_id.0,
                "cwd":project.path().display().to_string(), "connection_id":cc["id"], "model":"default", "prompt":if idle { "IDLE" } else { "Work" }}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = s["id"].as_str().unwrap();
        let events = async || -> Vec<Event> { http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json().await.unwrap() };
        eventually("a visible running turn", async || {
            events()
                .await
                .iter()
                .any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. } if format!("{blocks:?}").contains("Working.")))
                .then_some(())
        })
        .await;
        let (dir, pid) = eventually("this session's held wrapper", async || {
            for entry in std::fs::read_dir(&held).ok()?.flatten() {
                let meta: Value = serde_json::from_slice(&std::fs::read(entry.path().join("meta.json")).ok()?).ok()?;
                if meta["session"] == id {
                    return Some((entry.path(), Pid::from_raw(meta["pgid"].as_i64()? as i32)));
                }
            }
            None
        })
        .await;
        if idle {
            eventually("the first turn to complete before killing its idle held wrapper", async || {
                (events().await.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { reason: TurnEndReason::Completed, .. }))
                    && worker.sessions.holding().iter().all(|(s, what)| s != id || what != "a turn"))
                .then_some(())
            })
            .await;
        }
        if wrapper_signal {
            // Kill only the wrapper: it cannot run its `echo $? > code`.
            kill(pid, Signal::SIGTERM).unwrap();
        } else {
            let gate = project.path().join("gate");
            c6_testkit::within("release the CLI's exit gate", tokio::task::spawn_blocking(move || std::fs::write(gate, "exit\n").unwrap()))
                .await
                .unwrap();
        }
        if idle {
            eventually("the idle wrapper exit to be observed and reaped", async || {
                matches!(kill(pid, None), Err(nix::errno::Errno::ESRCH)).then_some(())
            })
            .await;
            // Reaping only ends wait_exit(), not the actor's cleanup/adoption
            // work. Wait for the whole runtime's cleanup before inspecting
            // the journal, including any ownership error that work appended.
            eventually("the idle runtime's whole group and held record to be released", async || {
                (matches!(killpg(pid, None), Err(nix::errno::Errno::ESRCH)) && !dir.join("meta.json").exists()).then_some(())
            })
            .await;
            let ev = events().await;
            assert_eq!(ev.iter().filter(|e| matches!(e.kind, EventKind::TurnStarted { .. })).count(), 1);
            assert_eq!(ev.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count(), 1);
            assert!(!ev.iter().any(|e| matches!(e.kind, EventKind::Error { .. })), "idle death must not invent a failed turn: {ev:?}");
            assert!(!dir.join("code").exists(), "idle SIGTERM must also bypass the code file");
            let response = http.post(format!("{api}/sessions/{id}/messages")).json(&json!({"text":"IDLE again"})).send().await.unwrap();
            assert!(response.status().is_success(), "{}", response.status());
            let ev = eventually("a new input to complete after idle runtime cleanup", async || {
                let ev = events().await;
                (ev.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { turn: 2, reason: TurnEndReason::Completed }))
                    && worker.sessions.holding().iter().all(|(s, what)| s != id || what != "a turn"))
                .then_some(ev)
            })
            .await;
            assert_eq!(ev.iter().filter(|e| matches!(e.kind, EventKind::User { .. })).count(), 2);
            assert_eq!(ev.iter().filter(|e| matches!(e.kind, EventKind::TurnStarted { .. })).count(), 2);
            assert_eq!(ev.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count(), 2);
            assert!(!ev.iter().any(|e| matches!(e.kind, EventKind::Error { .. })), "idle recovery must not invent a failed turn: {ev:?}");
            continue;
        }
        let ev = eventually("held exit visibly fails the running turn", async || {
            let ev = events().await;
            ev.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { reason: TurnEndReason::Error, .. })).then_some(ev)
        })
        .await;
        assert_eq!(ev.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count(), 1);
        assert!(ev.iter().any(|e| matches!(&e.kind, EventKind::Error { message } if message.contains("Claude Code exited") && message.contains("without a result"))));
        if !wrapper_signal {
            assert!(ev.iter().any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. } if format!("{blocks:?}").contains("Final output before exit."))), "drain final output before reporting exit");
            assert!(
                ev.iter().any(|e| matches!(&e.kind, EventKind::Error { message } if message.contains("17") && message.contains("Held CLI stderr detail."))),
                "retain the actual exit status and stderr detail"
            );
        }
        eventually("the wrapper is reaped, not left a zombie", async || {
            matches!(kill(pid, None), Err(nix::errno::Errno::ESRCH)).then_some(())
        })
        .await;
        if wrapper_signal {
            assert!(!dir.join("code").exists(), "SIGTERM must exercise exit without the wrapper's code file");
        }
        eventually("failed turn releases its running hold", async || {
            worker.sessions.holding().iter().all(|(s, what)| s != id || what != "a turn").then_some(())
        })
        .await;
    }
    held_subagent_exit_wakes_parent(&head, &http, &api, &worker, &cc, &held).await;
    stop.cancel();
    head.shutdown().await.unwrap();
}

// Keep this scenario in the same isolated test: the fixture's Claude path is
// process-global. The real worker observes the killed wrapper and authors the
// terminal event; the parent must hear it over the normal notice path.
async fn held_subagent_exit_wakes_parent(
    head: &c6_head::Head,
    http: &reqwest::Client,
    api: &str,
    child_worker: &Worker,
    cc: &Value,
    held: &std::path::Path,
) {
    let mock = MockLlm::start().await;
    let model: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"name":"mock","base_url":mock.base_url()}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let parent_state = tempfile::tempdir().unwrap();
    let e = c6_head::api::new_enrollment(&head.state, Some("held-exit-parent")).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(parent_state.path()).unwrap();
    let parent_worker = Worker::open_with_resource_sampler(
        parent_state.path(),
        Identity::load(parent_state.path()).unwrap(),
        c6_worker::ResourceMeasurement::default,
    )
    .await
    .unwrap();
    let parent_worker_id = parent_worker.identity.meta.worker_id.0;
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    let _stop_on_drop = stop.clone().drop_guard();
    tokio::spawn(conn::run(parent_worker, tx, stop));
    within("the parent worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();

    let child_dir = tempfile::tempdir().unwrap();
    nix::unistd::mkfifo(&child_dir.path().join("gate"), nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR).unwrap();
    let response = http
        .put(format!("{api}/settings/templates"))
        .json(&json!([{
            "name":"held-child", "workers":[child_worker.identity.meta.worker_name], "cwd":child_dir.path().canonicalize().unwrap(),
            "connection":cc["id"], "model":"default", "agents":true, "prompt":"{{input}}"
        }]))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.text().await.unwrap());
    let prompt = "Delegate the held exit test in the background.";
    mock.push_about(
        prompt,
        Reply::ToolCalls(vec![("task".into(), json!({
        "description":"Run the held child", "template":"held-child", "prompt":"Work until the fixture exits", "run_in_background":true
    }).to_string())]),
    );
    mock.push_about(prompt, Reply::Text("I will wait for its report.".into()));
    let project = tempfile::tempdir().unwrap();
    let parent: c6_api::Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({
            "worker_id":parent_worker_id,"cwd":project.path(),"connection_id":model["id"],"model":"m","prompt":prompt
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let events =
        async |id: &str| -> Vec<Event> { http.get(format!("{api}/sessions/{id}/events")).send().await.unwrap().json().await.unwrap() };
    let child = eventually_or(
        "the held child link and completed parent turn",
        async || {
            let ev = events(&parent.id).await;
            if !ev.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { reason: TurnEndReason::Completed, .. })) {
                return None;
            }
            ev.into_iter().find_map(|e| match e.kind {
                EventKind::SubagentSpawned { child_session_id, remote: Some(_), .. } => Some(child_session_id),
                _ => None,
            })
        },
        || mock.log(),
    )
    .await;
    eventually("the held child to record progress while still running", async || {
        events(&child)
            .await
            .iter()
            .any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. } if format!("{blocks:?}").contains("Working.")))
            .then_some(())
    })
    .await;
    let wrapper = eventually("the live wrapper of this particular child", async || {
        for entry in std::fs::read_dir(held).ok()?.flatten() {
            let Some(meta) =
                std::fs::read(entry.path().join("meta.json")).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            else {
                continue;
            };
            if meta["session"] == child {
                let pg = i32::try_from(meta["pgid"].as_i64()?).ok()?;
                assert!(pg > 0);
                let pid = Pid::from_raw(pg);
                assert!(kill(pid, None).is_ok());
                return Some(pid);
            }
        }
        None
    })
    .await;
    eventually("the parent opening's naming attempt to finish", async || {
        head.state.store.setting(&format!("session_naming:{}", parent.id)).await.unwrap().is_none().then_some(())
    })
    .await;
    mock.push_about_all(&[&child, "Working."], Reply::Text("I received the failed child's report.".into()));
    kill(wrapper, Signal::SIGTERM).unwrap();
    let (notice_seq, text, notice) = eventually_or(
        "the failure notice in the parent's journal",
        async || {
            events(&parent.id).await.into_iter().find_map(|e| match e.kind {
                EventKind::User { command, text, task_notice, .. } if command == "task-notification" => Some((e.seq, text, task_notice)),
                _ => None,
            })
        },
        || mock.log(),
    )
    .await;
    assert!(text.contains(&child) && text.contains("Working."), "{text}");
    let notice = notice.expect("the failed child has a structured report for the parent");
    // The normal native failure path may use a generic background-task card.
    // The notice text above must still identify the child and its progress.
    assert_eq!(notice.status, "failed");
    let reason = eventually_or(
        "the parent's acknowledgement turn to end",
        async || {
            events(&parent.id).await.into_iter().filter(|e| e.seq > notice_seq).find_map(|e| match e.kind {
                EventKind::TurnEnded { reason, .. } => Some(reason),
                _ => None,
            })
        },
        || mock.log(),
    )
    .await;
    assert_eq!(reason, TurnEndReason::Completed, "parent acknowledged the failed child");
    let ev = events(&child).await;
    assert!(ev.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { reason: TurnEndReason::Error, .. })), "{ev:?}");
    assert!(head.state.store.waiting_delegations(&child).await.unwrap().is_empty());
}
