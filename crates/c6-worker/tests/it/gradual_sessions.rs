//! Starting with only a worker and model, then changing where the session works.

use std::path::Path;

use c6_api::{ContentBlock, Event, EventKind, Session, SessionDetail, SessionStatus, TurnEndReason};
use c6_testkit::{Reply, eventually};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

use crate::batch_capture::Env;

#[tokio::test]
async fn omitting_cwd_runs_in_isolated_worker_scratch() {
    let env = Env::new().await;
    let mut notes = Vec::new();
    for label in ["first", "second"] {
        // Leave cwd out of the JSON entirely, rather than passing an empty string.
        let session: Session = serde_json::from_value(env.post("sessions", json!({"worker_id": env.wid, "model": "m"})).await).unwrap();
        assert_eq!(session.connection_id, env.conn, "the existing default connection is used");

        let prompt = format!("Write the {label} scratch note");
        let reply = format!("Wrote the {label} scratch note.");
        // Both sessions write the same filename, but neither should see it already there.
        let command = format!("test ! -e notes.txt && printf '%s' {label} > notes.txt");
        env.mock.push_about(&prompt, Reply::ToolCalls(vec![("bash".into(), json!({"command": command}).to_string())]));
        env.mock.push_about(&prompt, Reply::Text(reply.clone()));
        env.post(&format!("sessions/{}/messages", session.id), json!({"text": prompt})).await;

        let events = env.ended(&session.id, 1).await;
        assert!(events.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { reason: TurnEndReason::Completed, .. })), "{events:?}");
        assert!(
            events.iter().any(|e| matches!(&e.kind, EventKind::Assistant { blocks, .. }
                if blocks.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == &reply)))),
            "{events:?}"
        );

        let scratch = env.worker_state().join("scratch").join(&session.id).canonicalize().unwrap();
        let detail: SessionDetail = serde_json::from_value(env.get(&format!("sessions/{}", session.id)).await).unwrap();
        assert_eq!(std::path::Path::new(&detail.session.cwd), scratch.as_path());
        notes.push(scratch.join("notes.txt"));
    }

    assert_ne!(notes[0], notes[1]);
    assert_eq!(std::fs::read_to_string(&notes[0]).unwrap(), "first");
    assert_eq!(std::fs::read_to_string(&notes[1]).unwrap(), "second");
    env.stop.cancel();
}

async fn action(env: &Env, method: Method, path: &str, body: Value, expected: StatusCode) -> String {
    let response = env.http.request(method, format!("{}/{path}", env.api)).json(&body).send().await.unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    assert_eq!(status, expected, "{path}: {text}");
    text
}

fn script_bash(env: &Env, prompt: &str, args: Value) {
    env.mock.push_about(prompt, Reply::ToolCalls(vec![("bash".into(), args.to_string())]));
    env.mock.push_about(prompt, Reply::Text("Done with the command.".into()));
}

async fn placement_event(env: &Env, id: &str, command: &Value) -> Event {
    let command = command["command_id"].as_str().expect("placement command id");
    eventually("the placement event and updated session", async || {
        let detail: SessionDetail = serde_json::from_value(env.get(&format!("sessions/{id}")).await).unwrap();
        if let Some(reason) = detail.queued.iter().find(|q| q.command_id == command).and_then(|q| q.rejected.as_ref()) {
            panic!("placement was refused: {reason}");
        }
        env.events(id).await.into_iter().find(|e| {
            matches!(&e.kind, EventKind::PlaceChanged { command_id, to, .. }
                if command_id == command && detail.session.cwd == to.cwd && detail.session.last_seq >= e.seq)
        })
    })
    .await
}

#[tokio::test]
async fn scratch_to_folder_copies_files_and_uses_the_edited_notice() {
    let env = Env::new().await;
    let destination = tempfile::tempdir().unwrap();
    let folder = destination.path().canonicalize().unwrap();
    std::fs::create_dir(folder.join("drafts")).unwrap();
    std::fs::write(folder.join("drafts/keep.txt"), "already here").unwrap();
    action(
        &env,
        Method::PUT,
        "settings/prompts/session.place_changed",
        json!({"body": "Custom workspace notice: {{from}} => {{to}}"}),
        StatusCode::NO_CONTENT,
    )
    .await;

    let prompt = "Prepare two scratch drafts";
    script_bash(&env, prompt, json!({"command": "mkdir drafts && printf draft > drafts/note.txt && printf index > index.txt"}));
    let session: Session =
        serde_json::from_value(env.post("sessions", json!({"worker_id": env.wid, "model": "m", "prompt": prompt})).await).unwrap();
    env.ended(&session.id, 1).await;
    let id = &session.id;
    let scratch = env.worker_state().join("scratch").join(id).canonicalize().unwrap();
    let before = place_seq(&env, id).await;
    let command = env
        .post(
            &format!("sessions/{id}/placement"),
            json!({
                "kind": "folder", "expected_place_seq": before, "cwd": folder, "copy_scratch": true,
            }),
        )
        .await;
    let event = placement_event(&env, id, &command).await;
    let EventKind::PlaceChanged { from, to, copied_paths, uncommitted_files, .. } = event.kind else { unreachable!() };
    assert_eq!(Path::new(&from.cwd), scratch.as_path());
    assert_eq!(Path::new(&to.cwd), folder.as_path());
    assert_eq!(copied_paths, ["drafts/note.txt", "index.txt"]);
    assert_eq!(uncommitted_files, 0);
    let places = env.get(&format!("sessions/{id}/placement")).await;
    assert_eq!(places[0]["seq"], event.seq);
    assert_eq!(places[0]["copied_paths"], json!(copied_paths));
    assert_eq!(places[1]["seq"], before);
    for dir in [&scratch, &folder] {
        assert_eq!(std::fs::read_to_string(dir.join("drafts/note.txt")).unwrap(), "draft");
        assert_eq!(std::fs::read_to_string(dir.join("index.txt")).unwrap(), "index");
    }
    assert_eq!(std::fs::read_to_string(folder.join("drafts/keep.txt")).unwrap(), "already here");

    let next = "Revise the copied draft";
    script_bash(&env, next, json!({"command": "printf revised > drafts/note.txt"}));
    env.post(&format!("sessions/{id}/messages"), json!({"text": next})).await;
    env.ended(id, 2).await;
    assert_eq!(std::fs::read_to_string(folder.join("drafts/note.txt")).unwrap(), "revised");
    assert_eq!(std::fs::read_to_string(scratch.join("drafts/note.txt")).unwrap(), "draft");
    let notice = format!("Custom workspace notice: {} => {}", scratch.display(), folder.display());
    let requests = env.mock.requests();
    let history = requests.last().unwrap()["messages"].to_string();
    let (old, notice, next) = (history.find(prompt), history.find(&notice), history.find(next));
    assert!(matches!((old, notice, next), (Some(a), Some(b), Some(c)) if a < b && b < c), "{history}");
    env.stop.cancel();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} in {}: {}", dir.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn init_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("tracked.txt"), "base\n").unwrap();
    git(root, &["add", "tracked.txt"]);
    git(root, &["-c", "user.email=test@example.invalid", "-c", "user.name=Test", "-c", "commit.gpgsign=false", "commit", "-qm", "Base"]);
}

#[tokio::test]
async fn worktree_carries_staged_and_untracked_files_and_is_kept_for_move_back() {
    let env = Env::new().await;
    let repo = tempfile::tempdir().unwrap();
    let root = repo.path().canonicalize().unwrap();
    init_repo(&root);
    let prompt = "Stage a change and leave another file untracked";
    script_bash(&env, prompt, json!({"command": "echo staged > tracked.txt && git add tracked.txt && echo draft > untracked.txt"}));
    let session = env.start(&root, prompt).await;
    let id = &session.id;
    env.ended(id, 1).await;
    let initial_seq = place_seq(&env, id).await;
    let command = env
        .post(
            &format!("sessions/{id}/placement"),
            json!({
                "kind": "worktree", "expected_place_seq": initial_seq, "options": {"branch": "c6/gradual-test"},
            }),
        )
        .await;
    let entered = placement_event(&env, id, &command).await;
    let EventKind::PlaceChanged { to, copied_paths, uncommitted_files, .. } = entered.kind else { unreachable!() };
    assert_eq!(uncommitted_files, 2);
    assert!(copied_paths.is_empty());
    assert_eq!(Path::new(&to.repo_root), root.as_path());
    assert_eq!(to.worktree_branch, "c6/gradual-test");
    let linked = Path::new(&to.cwd);
    assert_ne!(linked, root.as_path());
    for dir in [root.as_path(), linked] {
        assert_eq!(git(dir, &["status", "--porcelain=v1", "--untracked-files=all"]), "M  tracked.txt\n?? untracked.txt\n");
        assert_eq!(git(dir, &["show", ":tracked.txt"]), "staged\n");
        assert_eq!(std::fs::read_to_string(dir.join("untracked.txt")).unwrap(), "draft\n");
    }
    let places = env.get(&format!("sessions/{id}/placement")).await;
    assert_eq!(places[0]["uncommitted_files"], 2);
    assert_eq!(places[0]["repo_root"], json!(root));

    let command = env
        .post(
            &format!("sessions/{id}/placement"),
            json!({
                "kind": "leave_worktree", "expected_place_seq": entered.seq, "merge": false,
            }),
        )
        .await;
    let left = placement_event(&env, id, &command).await;
    assert!(matches!(&left.kind, EventKind::PlaceChanged { to, .. }
        if Path::new(&to.cwd) == root.as_path() && to.worktree_branch.is_empty()));
    assert!(linked.is_dir(), "leaving does not remove a worktree");
    std::fs::write(linked.join("untracked.txt"), "only in the linked checkout\n").unwrap();
    let command = env
        .post(
            &format!("sessions/{id}/placement"),
            json!({
                "kind": "back", "expected_place_seq": left.seq, "place_seq": entered.seq,
            }),
        )
        .await;
    let back = placement_event(&env, id, &command).await;
    let places = env.get(&format!("sessions/{id}/placement")).await;
    assert_eq!(
        places.as_array().unwrap().iter().map(|p| p["seq"].as_u64().unwrap()).collect::<Vec<_>>(),
        [back.seq, left.seq, entered.seq, initial_seq]
    );
    assert_eq!(places[0]["cwd"], json!(linked));
    assert_eq!(places[0]["worktree_branch"], "c6/gradual-test");
    assert_eq!(std::fs::read_to_string(root.join("untracked.txt")).unwrap(), "draft\n");
    assert_eq!(git(linked, &["show", ":tracked.txt"]), "staged\n");
    assert_eq!(std::fs::read_to_string(linked.join("untracked.txt")).unwrap(), "only in the linked checkout\n");
    env.stop.cancel();
}

#[tokio::test]
async fn slow_worktree_setup_orders_its_session_without_blocking_another() {
    let env = Env::new().await;
    let (repo, other_dir, gate) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let root = repo.path().canonicalize().unwrap();
    init_repo(&root);
    env.mock.push_about("Prepare to enter a worktree", Reply::Text("Ready to move.".into()));
    env.mock.push_about("Prepare the independent session", Reply::Text("Ready here too.".into()));
    let moving = env.start(&root, "Prepare to enter a worktree").await;
    let other = env.start(other_dir.path(), "Prepare the independent session").await;
    env.ended(&moving.id, 1).await;
    env.ended(&other.id, 1).await;

    let id = &moving.id;
    let before = place_seq(&env, id).await;
    // Setup signals from inside the checkout, then waits for an explicit release
    // outside both checkouts. No elapsed time determines when commands are sent.
    let gate_path = gate.path().to_str().unwrap().replace('\'', "'\\''");
    let setup = format!(
        "gate='{gate_path}'; pwd -P > \"$gate/starting\" && mv \"$gate/starting\" \"$gate/started\" \
         && while [ -d \"$gate\" ] && [ ! -f \"$gate/release\" ]; do sleep 0.05; done"
    );
    let command = env
        .post(
            &format!("sessions/{id}/placement"),
            json!({
                "kind": "worktree", "expected_place_seq": before,
                "options": {"branch": "c6/slow-placement", "setup": [setup]},
            }),
        )
        .await;
    let started = gate.path().join("started");
    let linked = eventually("worktree setup to signal that it is waiting", async || {
        let path = std::fs::read_to_string(&started).ok()?;
        (!path.trim().is_empty()).then(|| std::path::PathBuf::from(path.trim()))
    })
    .await;
    assert_ne!(linked, root);

    let after_move = "Write in the destination after setup";
    script_bash(&env, after_move, json!({"command": "pwd -P > after-move.txt"}));
    env.post(&format!("sessions/{id}/messages"), json!({"text": after_move})).await;
    let independent = "Write without waiting for setup";
    script_bash(&env, independent, json!({"command": "printf independent > independent.txt"}));
    env.post(&format!("sessions/{}/messages", other.id), json!({"text": independent})).await;
    env.ended(&other.id, 2).await;
    assert_eq!(std::fs::read_to_string(other_dir.path().join("independent.txt")).unwrap(), "independent");
    assert_eq!(place_seq(&env, id).await, before, "the independent turn finished before the move committed");
    assert!(!root.join("after-move.txt").exists() && !linked.join("after-move.txt").exists());

    std::fs::write(gate.path().join("release"), "").unwrap();
    let placed = placement_event(&env, id, &command).await;
    assert!(matches!(&placed.kind, EventKind::PlaceChanged { to, .. } if Path::new(&to.cwd) == linked.as_path()));
    let events = env.ended(id, 2).await;
    let message = events.iter().find(|e| matches!(&e.kind, EventKind::User { text, .. } if text == after_move)).unwrap();
    assert!(placed.seq < message.seq, "the queued message is recorded after the placement");
    assert_eq!(std::fs::read_to_string(linked.join("after-move.txt")).unwrap().trim(), linked.to_str().unwrap());
    assert!(!root.join("after-move.txt").exists(), "the queued turn must never write to the old checkout");
    env.stop.cancel();
}

#[tokio::test]
async fn a_running_background_task_prevents_moving_an_idle_session() {
    let env = Env::new().await;
    let (source, destination) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let source_dir = source.path().canonicalize().unwrap();
    let destination_dir = destination.path().canonicalize().unwrap();
    let prompt = "Start a quiet background task";
    script_bash(&env, prompt, json!({"command": "exec sleep 60", "run_in_background": true, "notify": false}));
    let session = env.start(&source_dir, prompt).await;
    let id = &session.id;
    env.ended(id, 1).await;
    let task = format!("sessions/{id}/tasks/bg1");
    assert_eq!(env.get(&task).await["running"], true);
    let path = format!("sessions/{id}/placement");
    let before = env.get(&path).await;
    let request = json!({"kind": "folder", "cwd": destination_dir, "expected_place_seq": before[0]["seq"]});
    let command = env.post(&path, request.clone()).await;
    let command_id = command["command_id"].as_str().expect("placement command id");
    let rejected = eventually("the worker to refuse the placement", async || {
        let detail: SessionDetail = serde_json::from_value(env.get(&format!("sessions/{id}")).await).unwrap();
        detail.queued.into_iter().find(|q| q.command_id == command_id).and_then(|q| q.rejected).map(|reason| (detail.session, reason))
    })
    .await;
    action(&env, Method::POST, &format!("{task}/stop"), Value::Null, StatusCode::NO_CONTENT).await;
    assert!(rejected.1.contains("running tasks"), "{}", rejected.1);
    assert_eq!(rejected.0.status, SessionStatus::Idle);
    assert_eq!(Path::new(&rejected.0.cwd), source_dir.as_path());
    assert_eq!(env.get(&path).await, before);

    eventually("the task to stop", async || (env.get(&task).await["running"] == false).then_some(())).await;
    let retry = env.post(&path, request).await;
    let event = placement_event(&env, id, &retry).await;
    assert!(matches!(&event.kind, EventKind::PlaceChanged { to, .. } if Path::new(&to.cwd) == destination_dir.as_path()));
    env.stop.cancel();
}

#[tokio::test]
async fn rewind_across_a_move_keeps_both_workspaces_and_the_current_place() {
    let env = Env::signed_in().await;
    let (source, destination) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let source_dir = source.path().canonicalize().unwrap();
    let destination_dir = destination.path().canonicalize().unwrap();
    let first = "Write the original version";
    script_bash(&env, first, json!({"command": "printf original > note.txt"}));
    let session = env.start(&source_dir, first).await;
    let id = &session.id;
    env.ended(id, 1).await;
    let second = "Write the revised version";
    script_bash(&env, second, json!({"command": "printf revised > note.txt"}));
    env.post(&format!("sessions/{id}/messages"), json!({"text": second})).await;
    let events = env.ended(id, 2).await;
    let to_seq = events.iter().find(|e| matches!(&e.kind, EventKind::User { text, .. } if text == second)).unwrap().seq;
    assert!(events.iter().any(|e| e.seq < to_seq && matches!(&e.kind, EventKind::Checkpoint { error } if error.is_empty())));
    std::fs::write(destination_dir.join("note.txt"), "destination").unwrap();
    let before = place_seq(&env, id).await;
    let path = format!("sessions/{id}/placement");
    let command = env.post(&path, json!({"kind": "folder", "expected_place_seq": before, "cwd": destination_dir})).await;
    placement_event(&env, id, &command).await;
    let places = env.get(&path).await;

    action(&env, Method::POST, &format!("sessions/{id}/rewind"), json!({"to_seq": to_seq, "restore_files": true}), StatusCode::ACCEPTED)
        .await;
    let rewound = eventually("the conversation-only rewind", async || {
        env.events(id).await.into_iter().find_map(|e| match e.kind {
            EventKind::Rewound { to_seq: target, restored_files, restore_error } if target == to_seq => {
                Some((restored_files, restore_error))
            }
            _ => None,
        })
    })
    .await;
    assert!(!rewound.0 && rewound.1.contains("changed place"), "{rewound:?}");
    assert_eq!(std::fs::read_to_string(source_dir.join("note.txt")).unwrap(), "revised");
    assert_eq!(std::fs::read_to_string(destination_dir.join("note.txt")).unwrap(), "destination");
    assert_eq!(env.get(&path).await, places);

    let next = "Read the current version";
    script_bash(&env, next, json!({"command": "cat note.txt"}));
    env.post(&format!("sessions/{id}/messages"), json!({"text": next})).await;
    let events = env.ended(id, 3).await;
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.trim() == "destination")));
    let history = env.mock.requests().last().unwrap()["messages"].to_string();
    assert!(history.contains(first) && !history.contains(second) && history.contains(next), "{history}");
    assert!(history.contains("working directory moved from") && history.contains(destination_dir.to_str().unwrap()), "{history}");
    env.stop.cancel();
}

#[tokio::test]
async fn placement_is_refused_when_the_workers_realms_no_longer_include_the_session() {
    let env = Env::new().await;
    let destination = tempfile::tempdir().unwrap();
    env.post("realms", json!({"id": "other"})).await;
    let prompt = "Settle in the home realm";
    env.mock.push_about(prompt, Reply::Text("Ready.".into()));
    let session: Session = serde_json::from_value(
        env.post(
            "sessions",
            json!({
                "realm": "home", "worker_id": env.wid, "model": "m", "prompt": prompt,
            }),
        )
        .await,
    )
    .unwrap();
    let id = &session.id;
    env.ended(id, 1).await;
    let path = format!("sessions/{id}/placement");
    let before = env.get(&path).await;
    action(&env, Method::PUT, &format!("workers/{}/realms", env.wid), json!({"realms": ["other"]}), StatusCode::NO_CONTENT).await;
    let refused = action(
        &env,
        Method::POST,
        &path,
        json!({
            "kind": "folder", "expected_place_seq": before[0]["seq"], "cwd": destination.path(),
        }),
        StatusCode::FORBIDDEN,
    )
    .await;
    assert!(refused.contains("realm may not use this worker"), "{refused}");
    assert_eq!(env.get(&path).await, before);
    env.stop.cancel();
}

async fn place_seq(env: &Env, id: &str) -> u64 {
    let places = env.get(&format!("sessions/{id}/placement")).await;
    places[0]["seq"].as_u64().unwrap_or_else(|| panic!("missing placement: {places}"))
}

async fn moved(env: &Env, id: &str, cwd: &std::path::Path, command: &Value) -> Vec<c6_api::Event> {
    eventually("the placement command to commit", async || {
        let detail = env.get(&format!("sessions/{id}")).await;
        let events = env.events(id).await;
        let places = env.get(&format!("sessions/{id}/placement")).await;
        (detail["session"]["cwd"].as_str()? == cwd.to_str()?
            && places[0]["cwd"].as_str()? == cwd.to_str()?
            && detail["queued"].as_array()?.iter().all(|q| q["command_id"] != command["command_id"]))
        .then_some(events)
    })
    .await
}

#[tokio::test]
async fn scratch_to_folder_without_copy_keeps_conversation_and_reports_the_change() {
    let env = Env::new().await;
    let folder = tempfile::tempdir().unwrap();
    let folder = folder.path().canonicalize().unwrap();
    let prompt = "Prepare scratch";
    env.mock.push_about(prompt, Reply::ToolCalls(vec![("write".into(), json!({"path":"note.txt", "content":"scratch"}).to_string())]));
    env.mock.push_about(prompt, Reply::Text("Scratch prepared".into()));
    let session: Session =
        serde_json::from_value(env.post("sessions", json!({"worker_id":env.wid,"model":"m","prompt":prompt})).await).unwrap();
    env.ended(&session.id, 1).await;
    let scratch = env.worker_state().join("scratch").join(&session.id);
    assert_eq!(std::fs::read_to_string(scratch.join("note.txt")).unwrap(), "scratch");
    let before = place_seq(&env, &session.id).await;
    let command = env
        .post(
            &format!("sessions/{}/placement", session.id),
            json!({
                "kind":"folder", "cwd":folder, "copy_scratch":false, "expected_place_seq":before
            }),
        )
        .await;
    let events = moved(&env, &session.id, &folder, &command).await;
    assert_eq!(events.iter().filter(|e| matches!(&e.kind, EventKind::PlaceChanged { .. })).count(), 1);
    assert!(!folder.join("note.txt").exists());
    assert!(scratch.join("note.txt").exists(), "the old scratch is kept for Move back");
    let places = env.get(&format!("sessions/{}/placement", session.id)).await;
    assert_eq!(places.as_array().unwrap().len(), 2);
    assert_eq!(places[1]["seq"], before);

    env.mock.push_about("Continue here", Reply::ToolCalls(vec![("bash".into(), json!({"command":"pwd > location.txt"}).to_string())]));
    env.mock.push_about("Continue here", Reply::Text("Continued".into()));
    env.post(&format!("sessions/{}/messages", session.id), json!({"text":"Continue here"})).await;
    env.ended(&session.id, 2).await;
    assert_eq!(std::fs::read_to_string(folder.join("location.txt")).unwrap().trim(), folder.to_str().unwrap());
    let request = env.mock.requests().last().unwrap().clone();
    let history = request["messages"].to_string();
    assert!(history.contains("Prepare scratch") && history.contains("Scratch prepared"), "{history}");
    assert!(history.contains("working directory moved") && history.contains(folder.to_str().unwrap()), "{history}");
    env.stop.cancel();
}
