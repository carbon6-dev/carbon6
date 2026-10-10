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
    let mut queued = vec![PendingInput { command_id: "c2".into(), text: "and say done".into() }];
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
    assert_eq!(reqs[0]["tools"].as_array().unwrap().len(), 11);

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
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        c2.cancel();
    });
    let reason = run_turn(1, &cfg(), &client, &default_tools(), &session, &sink, &mut conv, &mut Vec::new, &cancel).await.unwrap();
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
    }
}
