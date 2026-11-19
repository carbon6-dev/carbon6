use std::sync::Arc;

use c6_proto::{
    BackgroundTask, CreateSession, Delegation, ErrorEvent, SessionCommand, SessionEvent, SessionStarted, SubagentReported, TurnEndReason,
    TurnEnded, TurnStarted, UserInput, UserMessage, session_command, session_event,
};
use c6_testkit::eventually;
use c6_worker::{journal::Journal, link::HeadLink, session::SessionManager};

const SID: &str = "parent";

async fn fixture(prompts_json: &str) -> (tempfile::TempDir, Arc<Journal>, SessionManager) {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::open(&dir.path().join("journal.db")).await.unwrap());
    let manager = SessionManager::new(journal.clone(), Arc::new(HeadLink::default()), dir.path().join("worktrees"));
    let started = SessionStarted { cwd: dir.path().display().to_string(), ..Default::default() };
    journal
        .import(
            SID,
            &CreateSession { prompts_json: prompts_json.into(), ..Default::default() },
            &[SessionEvent { seq: 1, kind: Some(session_event::Kind::Started(started)), ..Default::default() }],
        )
        .await
        .unwrap();
    (dir, journal, manager)
}

async fn start_record(journal: &Journal, seq: u64) {
    let task = BackgroundTask {
        task_id: "bg1".into(),
        kind: "agent".into(),
        description: "Check the tree".into(),
        state: "started".into(),
        notify: true,
        ..Default::default()
    };
    journal.append(SID, &SessionEvent { seq, kind: Some(session_event::Kind::BackgroundTask(task)), ..Default::default() }).await.unwrap();
}

async fn message(manager: &SessionManager, id: &str) {
    manager
        .handle(SessionCommand {
            session_id: SID.into(),
            command_id: id.into(),
            kind: Some(session_command::Kind::Input(UserInput { from_session_id: "sender".into(), ..Default::default() })),
        })
        .await
        .unwrap();
}

async fn lost_notice(journal: &Journal) -> Option<String> {
    journal.events_after(SID, 0, u32::MAX).await.unwrap().into_iter().find_map(|event| match event.kind {
        Some(session_event::Kind::NoticeQueued(n)) if n.command_id == "task:bg1:lost" => Some(n.text),
        _ => None,
    })
}

#[tokio::test]
async fn lazy_load_of_a_restored_session_does_not_invent_a_restart() {
    let (_dir, journal, manager) = fixture("").await;
    // Import with an open task while this worker is running, as when it
    // restores a session from the head. This is not startup recovery.
    start_record(&journal, 2).await;
    message(&manager, "load-restored-session").await;
    let notice = eventually("agent-visible task loss notice", || lost_notice(&journal)).await;
    assert!(notice.contains("outcome could be recorded"), "{notice}");
    assert!(!notice.contains("worker restarted"), "{notice}");
}

#[tokio::test]
async fn lazy_load_uses_the_editable_loss_prompt() {
    let head_dir = tempfile::tempdir().unwrap();
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
    c6_head::prompts::set(&head.state, "session.task_lost_unknown", "Edited task-loss cause.").await.unwrap();
    let prompts = c6_head::prompts::for_worker(&head.state, "root").await;
    let (_dir, journal, manager) = fixture(&prompts).await;
    start_record(&journal, 2).await;
    message(&manager, "load-restored-session").await;
    let notice = eventually("agent-visible task loss notice", || lost_notice(&journal)).await;
    assert!(notice.contains("Edited task-loss cause."), "{notice}");
    assert!(!notice.contains("worker restarted"), "{notice}");
    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn worker_startup_reports_the_previous_worker_restarted() {
    let (dir, journal, manager) = fixture("").await;
    start_record(&journal, 2).await;
    drop(manager);
    let manager = SessionManager::new(journal.clone(), Arc::new(HeadLink::default()), dir.path().join("worktrees"));
    manager.recover().await.unwrap();
    let notice = eventually("agent-visible task loss notice", || lost_notice(&journal)).await;
    assert!(notice.contains("worker restarted"), "{notice}");
    assert!(!notice.contains("journal write failed"), "{notice}");
}

#[tokio::test]
async fn replayed_child_failure_uses_the_editable_journal_failure_cause() {
    use session_event::Kind;

    let (dir, journal, manager) = fixture("").await;
    let spec = CreateSession {
        delegation: Some(Delegation { parent_session_id: SID.into(), root_session_id: SID.into(), ..Default::default() }),
        prompts_json: r#"{"session.task_lost_after_failure":"Edited child failure cause."}"#.into(),
        ..Default::default()
    };
    let kinds = [
        Kind::Started(SessionStarted { cwd: dir.path().display().to_string(), ..Default::default() }),
        Kind::SubagentReported(SubagentReported { command_id: "subagent:local:child:tracking".into(), ..Default::default() }),
        Kind::User(UserMessage { text: "Check the tree".into(), delegated_by: "call1".into(), ..Default::default() }),
        Kind::TurnStarted(TurnStarted { turn: 1, ..Default::default() }),
        Kind::Error(ErrorEvent { message: "Session stopped after a journal write failed".into() }),
        Kind::TurnEnded(TurnEnded { turn: 1, reason: TurnEndReason::Error as i32 }),
    ];
    let events = kinds
        .into_iter()
        .enumerate()
        .map(|(i, kind)| SessionEvent { seq: i as u64 + 1, kind: Some(kind), ..Default::default() })
        .collect::<Vec<_>>();
    journal.import("child", &spec, &events).await.unwrap();
    manager.recover().await.unwrap();
    let notice = eventually("journal-failure report delivered to the parent", async || {
        journal.events_after(SID, 0, u32::MAX).await.unwrap().into_iter().find_map(|event| match event.kind {
            Some(Kind::NoticeQueued(n)) if n.command_id == "subagent:local:child:6" => Some(n.text),
            _ => None,
        })
    })
    .await;
    assert!(notice.contains("Edited child failure cause."), "{notice}");
    assert!(notice.contains(r#"<subagent id="child" status="failed""#), "{notice}");
    assert!(!notice.contains("Session stopped after a journal write failed"), "{notice}");
    assert!(!notice.contains("worker restarted"), "{notice}");
}

struct MockHead(String);

#[async_trait::async_trait]
impl c6_worker::link::LocalHead for MockHead {
    async fn call(&self, _: &str, _: serde_json::Value) -> Result<serde_json::Value, String> {
        Err("not needed by this test".into())
    }

    async fn credential(&self, connection_id: &str) -> c6_proto::CredentialGrant {
        c6_proto::CredentialGrant {
            connection_id: connection_id.into(),
            kind: "openai_compat".into(),
            base_url: self.0.clone(),
            ..Default::default()
        }
    }
}

#[tokio::test]
async fn handed_child_report_falls_back_when_the_parent_cannot_record_its_result() {
    use c6_testkit::{MockLlm, Reply};
    use session_event::Kind;

    c6_llm::install_crypto_provider();
    let mock = MockLlm::start().await;
    let (dir, journal, _) = fixture("").await;
    let manager =
        SessionManager::new(journal.clone(), Arc::new(HeadLink::local(Arc::new(MockHead(mock.base_url())))), dir.path().join("worktrees"));
    let sid = "live-parent";
    let model = c6_proto::ModelSpec { connection_id: "mock".into(), model: "m".into(), ..Default::default() };
    let spec = CreateSession { model: Some(model.clone()), ..Default::default() };
    let started =
        SessionStarted { cwd: dir.path().display().to_string(), tool_profile: "generic".into(), model: Some(model), ..Default::default() };
    journal.import(sid, &spec, &[SessionEvent { seq: 1, kind: Some(Kind::Started(started)), ..Default::default() }]).await.unwrap();
    let db = sqlx::SqlitePool::connect(&format!("sqlite:{}", dir.path().join("journal.db").display())).await.unwrap();
    // Reject only the inline result. The parent's fallback NoticeQueued has a
    // different shape and remains writable, as does the child's final text.
    sqlx::query(
        "CREATE TRIGGER reject_inline_child_result BEFORE INSERT ON events \
         WHEN NEW.session_id = 'live-parent' AND instr(NEW.data, CAST('[subagent session ' AS BLOB)) > 0 \
         BEGIN SELECT RAISE(FAIL, 'child result write failed'); END",
    )
    .execute(&db)
    .await
    .unwrap();

    mock.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"Check the tree","prompt":"Check the tree"}"#.into())]));
    mock.push(Reply::Text("The child finished its checks.".into()));
    for _ in 0..3 {
        mock.push(Reply::Text("The parent continued.".into()));
    }
    manager
        .handle(SessionCommand {
            session_id: sid.into(),
            command_id: "run-task".into(),
            kind: Some(session_command::Kind::Input(UserInput { text: "Run one task".into(), ..Default::default() })),
        })
        .await
        .unwrap();

    let child = eventually("the parent to start its child", async || {
        journal.events_after(sid, 0, u32::MAX).await.unwrap().into_iter().find_map(|e| match e.kind {
            Some(Kind::SubagentSpawned(s)) => Some(s.child_session_id),
            _ => None,
        })
    })
    .await;
    eventually("the parent's failed inline result to close its turn", async || {
        journal
            .events_after(sid, 0, u32::MAX)
            .await
            .unwrap()
            .iter()
            .any(|e| matches!(&e.kind, Some(Kind::TurnEnded(t)) if t.reason == TurnEndReason::Error as i32))
            .then_some(())
    })
    .await;
    eventually("the live child to retry its report and observe a durable receipt", async || {
        journal.events_after(&child, 0, u32::MAX).await.unwrap().into_iter().find_map(|e| match e.kind {
            Some(Kind::SubagentReported(r)) if r.status == "completed" => Some(r.command_id),
            _ => None,
        })
    })
    .await;
    let parent = journal.events_after(sid, 0, u32::MAX).await.unwrap();
    let fallback = parent
        .iter()
        .filter(|e| {
            matches!(&e.kind, Some(Kind::NoticeQueued(n)) if n.command_id.starts_with(&format!("subagent:local:{child}:"))
            && n.text.contains("The child finished its checks."))
        })
        .count();
    assert_eq!(fallback, 1, "{parent:?}");
    db.close().await;
}
