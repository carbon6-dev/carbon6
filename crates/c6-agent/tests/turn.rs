use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use c6_agent::{Conversation, EventSink, PendingInput, TurnConfig, history_from_events, run_turn};
use c6_llm::{Message, openai_chat::OpenAiChat};
use c6_proto::{SessionEvent, TurnEndReason, ephemeral_event, session_event};
use c6_testkit::{MockLlm, Reply};
use c6_tools::{SessionTools, default_tools};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct VecSink {
    events: Mutex<Vec<SessionEvent>>,
    ephemeral: Mutex<Vec<ephemeral_event::Kind>>,
}

#[async_trait]
impl EventSink for VecSink {
    async fn append(&self, kind: session_event::Kind) -> anyhow::Result<u64> {
        let mut ev = self.events.lock().unwrap();
        let seq = ev.len() as u64 + 1;
        ev.push(SessionEvent { seq, ts_ms: 0, kind: Some(kind) });
        Ok(seq)
    }
    fn ephemeral(&self, kind: ephemeral_event::Kind) {
        self.ephemeral.lock().unwrap().push(kind);
    }
}

fn cfg() -> TurnConfig {
    TurnConfig {
        model: "mock-1".into(),
        system_prompt: "sys".into(),
        max_output_tokens: None,
        cache_key: "s1".into(),
        effort: None,
        context_window: 128_000,
        web_search: false,
        compact: Default::default(),
    }
}

#[tokio::test]
async fn tool_call_then_answer_with_mid_turn_input() {
    let mock = MockLlm::start().await;
    mock.push(Reply::ToolCalls(vec![("write".into(), r#"{"path":"hello.txt","content":"hi\n"}"#.into())]));
    mock.push(Reply::Text("Created hello.txt.".into()));
    let dir = tempfile::tempdir().unwrap();

    let client = OpenAiChat::new(&mock.base_url(), "test-key", HashMap::new()).unwrap();
    let vec_sink = Arc::new(VecSink::default());
    let sink: Arc<dyn EventSink> = vec_sink.clone();
    let session = Arc::new(SessionTools::new(dir.path().to_owned()));
    let mut conv = Conversation { history: vec![Message::user_text("make hello.txt")], context_tokens: 0 };
    let mut queued = vec![PendingInput { command_id: "c2".into(), text: "and say done".into(), ..Default::default() }];
    let mut pending = move || std::mem::take(&mut queued);

    let reason =
        run_turn(1, &cfg(), &client, &default_tools(), &session, &sink, &mut conv, &mut pending, &CancellationToken::new()).await.unwrap();
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(), "hi\n");

    // The second request carries the tool result and the queued input, in that order.
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    let msgs = reqs[1]["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["system", "user", "assistant", "tool", "user"]);
    assert_eq!(msgs[4]["content"], "and say done");
    assert_eq!(reqs[0]["tools"].as_array().unwrap().len(), 12);

    // The recorded log rebuilds the same conversation.
    let events = vec_sink.events.lock().unwrap().clone();
    let kinds: Vec<&str> = events.iter().map(|e| kind_name(e)).collect();
    assert_eq!(kinds, vec!["turn_started", "assistant", "usage", "tool_result", "user", "assistant", "usage", "turn_ended"]);
    let mut rebuilt = vec![Message::user_text("make hello.txt")];
    rebuilt.extend(history_from_events(&events));
    assert_eq!(rebuilt, conv.history);
}

#[tokio::test]
async fn interrupt_mid_stream_keeps_partial_text() {
    let mock = MockLlm::start().await;
    let _gate = mock.push_gated(Reply::Text("partial answer that never finishes".into()));
    let dir = tempfile::tempdir().unwrap();
    let client = OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap();
    let vec_sink = Arc::new(VecSink::default());
    let sink: Arc<dyn EventSink> = vec_sink.clone();
    let session = Arc::new(SessionTools::new(dir.path().to_owned()));
    let mut conv = Conversation { history: vec![Message::user_text("hi")], context_tokens: 0 };
    let cancel = CancellationToken::new();
    let (c2, seen) = (cancel.clone(), vec_sink.clone());
    // Interrupted once the partial text has streamed (not after some time).
    tokio::spawn(async move {
        while !seen.ephemeral.lock().unwrap().iter().any(|k| matches!(k, ephemeral_event::Kind::Text(_))) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        c2.cancel();
    });
    let (cfg, tools) = (cfg(), default_tools());
    let mut nothing = Vec::new;
    let turn = run_turn(1, &cfg, &client, &tools, &session, &sink, &mut conv, &mut nothing, &cancel);
    let reason = c6_testkit::within("the interrupted turn", turn).await.unwrap();
    assert_eq!(reason, TurnEndReason::Interrupted);
    let events = vec_sink.events.lock().unwrap().clone();
    let kinds: Vec<&str> = events.iter().map(|e| kind_name(e)).collect();
    assert_eq!(kinds, vec!["turn_started", "assistant", "turn_ended"]);
}

#[tokio::test]
async fn compacts_when_the_context_fills_up() {
    let mock = MockLlm::start().await;
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo hi"}"#.into())]));
    mock.push(Reply::Text("SUMMARY: ran echo".into())); // the compaction call
    mock.push(Reply::Text("All done.".into()));
    let dir = tempfile::tempdir().unwrap();
    let client = OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap();
    let vec_sink = Arc::new(VecSink::default());
    let sink: Arc<dyn EventSink> = vec_sink.clone();
    let session = Arc::new(SessionTools::new(dir.path().to_owned()));
    // The mock reports 110 tokens per call; a 100-token window forces compaction.
    let cfg = TurnConfig { context_window: 100, ..cfg() };
    let mut conv = Conversation { history: vec![Message::user_text("say hi")], context_tokens: 0 };
    let reason =
        run_turn(1, &cfg, &client, &default_tools(), &session, &sink, &mut conv, &mut Vec::new, &CancellationToken::new()).await.unwrap();
    assert_eq!(reason, TurnEndReason::Completed);

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 3);
    let compact_req = reqs[1]["messages"].as_array().unwrap();
    assert!(compact_req.last().unwrap()["content"].as_str().unwrap().contains("summary"));
    assert_eq!(reqs[1]["tools"], reqs[0]["tools"], "compaction reuses the cached prefix");
    let after = reqs[2]["messages"].as_array().unwrap();
    assert_eq!(after.len(), 2, "system + summary only");
    assert!(after[1]["content"].as_str().unwrap().contains("SUMMARY: ran echo"));

    let events = vec_sink.events.lock().unwrap().clone();
    assert!(events.iter().any(|e| matches!(e.kind, Some(session_event::Kind::Compaction(_)))));
    // Rebuilding from the log yields the same post-compaction conversation.
    assert_eq!(history_from_events(&events), conv.history);
}

/// A turn on a 100-token window (the mock reports 110 per call, so the
/// second step compacts), with the given replies.
async fn compacting_turn(replies: Vec<Reply>) -> (TurnEndReason, Vec<serde_json::Value>, Vec<SessionEvent>, Conversation) {
    let mock = MockLlm::start().await;
    for r in replies {
        mock.push(r);
    }
    let dir = tempfile::tempdir().unwrap();
    let client = OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap();
    let vec_sink = Arc::new(VecSink::default());
    let sink: Arc<dyn EventSink> = vec_sink.clone();
    let session = Arc::new(SessionTools::new(dir.path().to_owned()));
    let cfg = TurnConfig { context_window: 100, ..cfg() };
    let mut conv = Conversation { history: vec![Message::user_text("say hi")], context_tokens: 0 };
    let reason =
        run_turn(1, &cfg, &client, &default_tools(), &session, &sink, &mut conv, &mut Vec::new, &CancellationToken::new()).await.unwrap();
    let events = vec_sink.events.lock().unwrap().clone();
    (reason, mock.requests(), events, conv)
}

fn bash_call() -> Reply {
    Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo hi"}"#.into())])
}

fn compaction(events: &[SessionEvent]) -> String {
    events.iter().find_map(|e| if let Some(session_event::Kind::Compaction(c)) = &e.kind { Some(c.summary.clone()) } else { None }).unwrap()
}

#[tokio::test]
async fn compaction_asks_again_without_tools_when_the_model_calls_one() {
    let (reason, reqs, events, conv) =
        compacting_turn(vec![bash_call(), bash_call(), Reply::Text("SUMMARY: ran echo".into()), Reply::Text("All done.".into())]).await;
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(reqs.len(), 4);
    // The second try: the conversation as text, no tools.
    assert!(reqs[2].get("tools").is_none(), "{}", reqs[2]);
    let retry = reqs[2]["messages"].as_array().unwrap();
    assert_eq!(retry.len(), 2, "system + one user message");
    let text = retry[1]["content"].as_str().unwrap();
    assert!(text.contains("say hi") && text.contains("[tool call bash:") && text.contains("summary"), "{text}");
    assert_eq!(compaction(&events), "SUMMARY: ran echo");
    assert_eq!(history_from_events(&events), conv.history);
}

#[tokio::test]
async fn automatic_compaction_falls_back_to_a_digest() {
    let (reason, reqs, events, conv) = compacting_turn(vec![bash_call(), bash_call(), bash_call(), Reply::Text("All done.".into())]).await;
    assert_eq!(reason, TurnEndReason::Completed, "the session is not stuck");
    assert_eq!(reqs.len(), 4);
    let digest = compaction(&events);
    assert!(digest.contains("the model called `bash` instead of writing a summary"), "{digest}");
    assert!(digest.contains("mechanical digest") && digest.contains("say hi") && digest.contains("echo hi"), "{digest}");
    assert!(reqs[3]["messages"][1]["content"].as_str().unwrap().contains("mechanical digest"));
    assert_eq!(history_from_events(&events), conv.history);
}

#[tokio::test]
async fn asked_compaction_reports_what_the_model_did() {
    let mock = MockLlm::start().await;
    mock.push(bash_call());
    mock.push(bash_call());
    let client = OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap();
    let vec_sink = Arc::new(VecSink::default());
    let sink: Arc<dyn EventSink> = vec_sink.clone();
    let mut conv = Conversation { history: vec![Message::user_text("say hi")], context_tokens: 0 };
    let before = conv.history.clone();
    let reason =
        c6_agent::compact_turn(1, &cfg(), &client, &default_tools(), &sink, &mut conv, "", &CancellationToken::new()).await.unwrap();
    assert_eq!(reason, TurnEndReason::Error);
    assert_eq!(conv.history, before, "nothing dropped");
    let events = vec_sink.events.lock().unwrap().clone();
    let err =
        events.iter().find_map(|e| if let Some(session_event::Kind::Error(x)) = &e.kind { Some(x.message.clone()) } else { None }).unwrap();
    assert!(err.contains("compaction failed: the model called `bash` instead of writing a summary"), "{err}");
}

fn kind_name(e: &SessionEvent) -> &'static str {
    match e.kind.as_ref().unwrap() {
        session_event::Kind::Started(_) => "started",
        session_event::Kind::User(_) => "user",
        session_event::Kind::TurnStarted(_) => "turn_started",
        session_event::Kind::Assistant(_) => "assistant",
        session_event::Kind::ToolResult(_) => "tool_result",
        session_event::Kind::Usage(_) => "usage",
        session_event::Kind::TurnEnded(_) => "turn_ended",
        session_event::Kind::Error(_) => "error",
        session_event::Kind::Compaction(_) => "compaction",
        session_event::Kind::ModelSwitched(_) => "model_switched",
        session_event::Kind::Checkpoint(_) => "checkpoint",
        session_event::Kind::Rewound(_) => "rewound",
        session_event::Kind::BackgroundTask(_) => "background_task",
        session_event::Kind::Output(_) => "output",
        session_event::Kind::Memory(_) => "memory",
        session_event::Kind::Approval(_) => "approval",
        session_event::Kind::Notice(_) => "notice",
        session_event::Kind::Question(_) => "question",
        session_event::Kind::SubagentModelSet(_) => "subagent_model_set",
        session_event::Kind::SubagentSpawned(_) => "subagent_spawned",
    }
}

/// A response that breaks off is tried again (what it streamed dropped),
/// and the turn goes on; a request the provider rejects is not.
#[tokio::test]
async fn failed_model_calls_are_tried_again() {
    let mock = MockLlm::start().await;
    mock.push(Reply::Broken("half an answer that breaks".into()));
    mock.push(Reply::Text("The whole answer.".into()));
    let dir = tempfile::tempdir().unwrap();
    let client = OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap();
    let vec_sink = Arc::new(VecSink::default());
    let sink: Arc<dyn EventSink> = vec_sink.clone();
    let session = Arc::new(SessionTools::new(dir.path().to_owned()));
    let mut conv = Conversation { history: vec![Message::user_text("hi")], context_tokens: 0 };
    let reason =
        run_turn(1, &cfg(), &client, &default_tools(), &session, &sink, &mut conv, &mut Vec::new, &CancellationToken::new()).await.unwrap();
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(mock.requests().len(), 2);
    let events = vec_sink.events.lock().unwrap().clone();
    let kinds: Vec<&str> = events.iter().map(|e| kind_name(e)).collect();
    assert_eq!(kinds, vec!["turn_started", "assistant", "usage", "turn_ended"], "{events:?}");
    assert!(
        format!("{:?}", conv.history.last().unwrap()).contains("The whole answer.") && !format!("{:?}", conv.history).contains("half an")
    );
    // The session said it was waiting, then that it isn't any more.
    let statuses: Vec<String> = vec_sink
        .ephemeral
        .lock()
        .unwrap()
        .iter()
        .filter_map(|k| match k {
            ephemeral_event::Kind::Activity(a) => Some(a.status.clone()),
            _ => None,
        })
        .collect();
    assert!(statuses.first().is_some_and(|s| s.contains("trying again in 2s (1 of 6)")), "{statuses:?}");
    assert_eq!(statuses.last().map(String::as_str), Some(""));

    // Rejected (400): no retry, the turn stops with the error.
    let mock = MockLlm::start().await;
    mock.push(Reply::Status(400));
    let client = OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap();
    let mut conv = Conversation { history: vec![Message::user_text("hi")], context_tokens: 0 };
    let reason =
        run_turn(2, &cfg(), &client, &default_tools(), &session, &sink, &mut conv, &mut Vec::new, &CancellationToken::new()).await.unwrap();
    assert_eq!(reason, TurnEndReason::Error);
    assert_eq!(mock.requests().len(), 1);
}
