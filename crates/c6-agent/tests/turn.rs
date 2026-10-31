use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use c6_agent::{Conversation, EventSink, PendingInput, TurnConfig, history_from_events, run_turn};
use c6_llm::{Block, Message, ModelClient, ModelEvent, ModelRequest, Role, openai_chat::OpenAiChat, stop};
use c6_proto::{SessionEvent, TurnEndReason, ephemeral_event, session_event};
use c6_testkit::{MockLlm, Reply};
use c6_tools::{SessionTools, Tool, default_tools};
use futures::StreamExt;
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
        compact_at: 0,
    }
}

/// A server report independent of the prompt's estimated size.
struct ReportedUsage {
    inner: OpenAiChat,
    tokens: u64,
}

#[async_trait]
impl ModelClient for ReportedUsage {
    async fn stream(&self, req: &c6_llm::ModelRequest) -> Result<c6_llm::EventStream, c6_llm::LlmError> {
        let tokens = self.tokens;
        Ok(self
            .inner
            .stream(req)
            .await?
            .map(move |ev| {
                ev.map(|ev| match ev {
                    ModelEvent::Usage(_) => ModelEvent::Usage(c6_llm::Usage { input_tokens: tokens, ..Default::default() }),
                    ev => ev,
                })
            })
            .boxed())
    }
}

/// A thinking model that fills the summary allowance and can pause once.
#[derive(Default)]
struct SummaryModel {
    requests: Mutex<Vec<ModelRequest>>,
    pause: bool,
}

#[async_trait]
impl ModelClient for SummaryModel {
    async fn stream(&self, req: &ModelRequest) -> Result<c6_llm::EventStream, c6_llm::LlmError> {
        let mut requests = self.requests.lock().unwrap();
        assert!(requests.len() < 5, "compaction must not keep asking for summaries");
        let summarizing = |r: &ModelRequest| {
            r.messages.iter().flat_map(|m| &m.blocks).any(|b| matches!(b, Block::Text(t) if t.contains(c6_agent::COMPACT_PROMPT)))
        };
        let events = if summarizing(req) {
            vec![
                ModelEvent::ThinkingDelta("Let me gather the important details.".into()),
                ModelEvent::TextDelta(format!("SUMMARY: {}", "s".repeat(req.max_output_tokens.unwrap() as usize * 7 / 2 - 32))),
                ModelEvent::Done { stop_reason: "stop".into() },
            ]
        } else if self.pause && requests.iter().all(summarizing) {
            vec![ModelEvent::Done { stop_reason: stop::PAUSE_TURN.into() }]
        } else {
            vec![ModelEvent::TextDelta("done".into()), ModelEvent::Done { stop_reason: "stop".into() }]
        };
        requests.push(req.clone());
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

fn large_tool_history(chars: usize) -> Vec<Message> {
    vec![
        Message::user_text("Read large.txt"),
        Message {
            role: Role::Assistant,
            blocks: vec![Block::ToolUse { id: "read-1".into(), name: "read".into(), input_json: r#"{"path":"large.txt"}"#.into() }],
        },
        Message { role: Role::User, blocks: vec![Block::ToolResult { id: "read-1".into(), content: "x".repeat(chars), is_error: false }] },
        Message { role: Role::Assistant, blocks: vec![Block::Text("Read it.".into())] },
    ]
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
    let client = ReportedUsage { inner: OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap(), tokens: 14_000 };
    let vec_sink = Arc::new(VecSink::default());
    let sink: Arc<dyn EventSink> = vec_sink.clone();
    let session = Arc::new(SessionTools::new(dir.path().to_owned()));
    // The report is above 85% of the 16k window after the first call.
    let cfg = TurnConfig { context_window: 16_384, ..cfg() };
    let mut conv = Conversation { history: vec![Message::user_text("say hi")], context_tokens: 0 };
    let reason =
        run_turn(1, &cfg, &client, &default_tools(), &session, &sink, &mut conv, &mut Vec::new, &CancellationToken::new()).await.unwrap();
    assert_eq!(reason, TurnEndReason::Completed);

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 3);
    let compact_req = reqs[1]["messages"].as_array().unwrap();
    assert!(compact_req.last().unwrap()["content"].as_str().unwrap().contains("summary"));
    assert_eq!(reqs[1]["tools"], reqs[0]["tools"], "compaction reuses the cached prefix");
    let limit = reqs[1]["max_tokens"].as_u64().unwrap();
    assert!(limit > 0 && limit < cfg.context_window - 14_000, "the report and summary instructions leave room for output");
    let after = reqs[2]["messages"].as_array().unwrap();
    assert_eq!(after.len(), 2, "system + summary only");
    assert!(after[1]["content"].as_str().unwrap().contains("SUMMARY: ran echo"));

    let events = vec_sink.events.lock().unwrap().clone();
    assert!(events.iter().any(|e| matches!(e.kind, Some(session_event::Kind::Compaction(_)))));
    // Rebuilding from the log yields the same post-compaction conversation.
    assert_eq!(history_from_events(&events), conv.history);
}

/// A turn reporting 14k tokens in a 16k window, so its second step compacts.
async fn compacting_turn(replies: Vec<Reply>) -> (TurnEndReason, Vec<serde_json::Value>, Vec<SessionEvent>, Conversation) {
    let mock = MockLlm::start().await;
    for r in replies {
        mock.push(r);
    }
    let dir = tempfile::tempdir().unwrap();
    let client = ReportedUsage { inner: OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap(), tokens: 14_000 };
    let vec_sink = Arc::new(VecSink::default());
    let sink: Arc<dyn EventSink> = vec_sink.clone();
    let session = Arc::new(SessionTools::new(dir.path().to_owned()));
    let cfg = TurnConfig { context_window: 16_384, ..cfg() };
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
async fn truncated_summaries_fall_back_without_replacing_history_with_a_fragment() {
    for truncated in [1, 2] {
        let mock = MockLlm::start().await;
        for _ in 0..truncated {
            mock.push(Reply::Truncated {
                reasoning: "Let me gather the important details.".into(),
                text: "FRAGMENT: unfinished summary".into(),
            });
        }
        if truncated == 1 {
            mock.push(Reply::Text("SUMMARY: read large.txt".into()));
        }
        mock.push(Reply::Text("done".into()));
        let client = OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let session = Arc::new(SessionTools::new(dir.path().to_owned()));
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let cfg = TurnConfig { context_window: 16_384, ..cfg() };
        let mut conv = Conversation { history: large_tool_history(100), context_tokens: 14_000 };
        let reason = run_turn(1, &cfg, &client, &default_tools(), &session, &sink, &mut conv, &mut Vec::new, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(reason, TurnEndReason::Completed);
        let requests = mock.requests();
        assert_eq!(requests.len(), 3, "full summary, digest summary, then the normal request");
        assert!(requests[0].get("tools").is_some() && requests[1].get("tools").is_none());
        let events = vec_sink.events.lock().unwrap();
        let summary = compaction(&events);
        assert!(!summary.contains("FRAGMENT"), "{summary}");
        if truncated == 1 {
            assert!(summary.starts_with("SUMMARY:"));
        } else {
            assert!(summary.contains("mechanical digest") && summary.contains("output token limit"), "{summary}");
        }
        assert_eq!(history_from_events(&events), conv.history);
    }
}

#[tokio::test]
async fn summaries_fit_beside_the_prefix_and_do_not_compact_again_without_growth() {
    for (window, bounded) in [(16_384, false), (16_384, true), (200_000, true), (272_000, true)] {
        let client = SummaryModel { pause: true, ..Default::default() };
        let dir = tempfile::tempdir().unwrap();
        let session = Arc::new(SessionTools::new(dir.path().to_owned()));
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let cfg = TurnConfig { context_window: window, system_prompt: "s".repeat(9_000 * 7 / 2), ..cfg() };
        let mut history = large_tool_history(100);
        if bounded {
            history.extend((0..8).map(|_| Message { role: Role::Assistant, blocks: vec![Block::Text("previous step ".repeat(500))] }));
        }
        let mut conv = Conversation { history, context_tokens: if bounded { window + 1 } else { 12_000 } };
        // Trigger the full-summary path manually: its reported size leaves
        // more output room than the prefix the summary returns to does.
        let reason =
            c6_agent::compact_turn(1, &cfg, &client, &default_tools(), &sink, &mut conv, "", &CancellationToken::new()).await.unwrap();
        assert_eq!(reason, TurnEndReason::Completed);
        // A pause adds neither history nor usage. It must not compact again.
        let reason = run_turn(2, &cfg, &client, &default_tools(), &session, &sink, &mut conv, &mut Vec::new, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(reason, TurnEndReason::Completed);
        let requests = client.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].tools.is_empty(), bounded);
        if bounded {
            let input = cfg.system_prompt.len() + serde_json::to_string(&requests[0].messages).unwrap().len();
            assert!(
                (input * 2).div_ceil(7) + requests[0].max_output_tokens.unwrap() as usize <= cfg.context_window as usize,
                "the digest input and output fit beside the 9k system prompt"
            );
        }
        let prefix =
            cfg.system_prompt.len() + serde_json::to_string(&requests[1].tools).unwrap().len() + c6_agent::compacted_context("").len();
        assert!(prefix * 2 / 7 > 9_000);
        let output = requests[0].max_output_tokens.unwrap() as usize;
        if window > 16_384 {
            assert_eq!(output, 16_000, "large windows retain the full digest summary allowance");
        }
        assert!((prefix * 2).div_ceil(7) + output < cfg.context_window as usize * 85 / 100);
        let next = cfg.system_prompt.len()
            + serde_json::to_string(&requests[1].tools).unwrap().len()
            + serde_json::to_string(&requests[1].messages).unwrap().len();
        assert!((next * 2).div_ceil(7) < cfg.context_window as usize * 85 / 100);
        assert_eq!(
            vec_sink.events.lock().unwrap().iter().filter(|e| matches!(e.kind, Some(session_event::Kind::Compaction(_)))).count(),
            1
        );
    }
}

fn large_prefix(spare: i32) -> (TurnConfig, Vec<Arc<dyn Tool>>) {
    let tools = default_tools();
    let specs: Vec<_> = tools.iter().map(|t| t.spec()).collect();
    let fixed = serde_json::to_string(&specs).unwrap().len() + c6_agent::compacted_context("").len() + 96;
    let system_chars = ((16_384 * 85 / 100 - spare) * 7 / 2) as usize - fixed;
    (TurnConfig { context_window: 16_384, model: "qwen3.8:27b".into(), system_prompt: "s".repeat(system_chars), ..cfg() }, tools)
}

#[tokio::test]
async fn a_prefix_with_no_summary_room_keeps_history_that_fits() {
    for spare in [128, -128] {
        let client = SummaryModel { pause: true, ..Default::default() };
        let dir = tempfile::tempdir().unwrap();
        let session = Arc::new(SessionTools::new(dir.path().to_owned()));
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let (cfg, tools) = large_prefix(spare);
        let mut conv = Conversation { history: vec![Message::user_text("What does this function do?")], context_tokens: 0 };
        let before = conv.history.clone();
        let reason =
            run_turn(1, &cfg, &client, &tools, &session, &sink, &mut conv, &mut Vec::new, &CancellationToken::new()).await.unwrap();
        assert_eq!(reason, TurnEndReason::Completed);
        let requests = client.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "only the normal request and its continuation after a pause");
        assert!(requests.iter().all(|r| r.messages == before), "the question reaches the model unchanged");
        assert_eq!(conv.history[..before.len()], before);
        let events = vec_sink.events.lock().unwrap();
        assert!(!events.iter().any(|e| matches!(e.kind, Some(session_event::Kind::Compaction(_)))));
    }
}

#[tokio::test]
async fn a_prefix_with_no_summary_room_fails_without_dropping_history() {
    for (manual, large_history) in [(false, true), (true, true), (true, false)] {
        let client = SummaryModel::default();
        let dir = tempfile::tempdir().unwrap();
        let session = Arc::new(SessionTools::new(dir.path().to_owned()));
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let (cfg, tools) = large_prefix(-128);
        let history = if large_history { large_tool_history(30_000) } else { vec![Message::user_text("What does this function do?")] };
        let mut conv = Conversation { history, context_tokens: 0 };
        let before = conv.history.clone();
        let cancel = CancellationToken::new();
        let reason = if manual {
            c6_agent::compact_turn(1, &cfg, &client, &tools, &sink, &mut conv, "", &cancel).await.unwrap()
        } else {
            run_turn(1, &cfg, &client, &tools, &session, &sink, &mut conv, &mut Vec::new, &cancel).await.unwrap()
        };
        assert_eq!(reason, TurnEndReason::Error);
        assert_eq!(conv.history, before);
        assert_eq!(conv.context_tokens, 0);
        assert!(client.requests.lock().unwrap().is_empty(), "neither a summary nor an oversized request is sent");
        let events = vec_sink.events.lock().unwrap();
        assert!(!events.iter().any(|e| matches!(e.kind, Some(session_event::Kind::Compaction(_)))));
        let error = events
            .iter()
            .find_map(|e| match &e.kind {
                Some(session_event::Kind::Error(e)) => Some(e.message.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(error.contains("the system prompt and tools take about") && error.contains("of qwen3.8:27b's 16384 tokens"), "{error}");
        assert!(error.contains("no room to compact this conversation"), "{error}");
        assert!(error.contains("give the session fewer tools (a routing rule's tool list) or a larger context window"), "{error}");
    }
}

#[tokio::test]
async fn manual_compaction_counts_tool_results_since_the_report() {
    let mock = MockLlm::start().await;
    mock.push(Reply::Text("SUMMARY: read large.txt".into()));
    let client = OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap();
    let vec_sink = Arc::new(VecSink::default());
    let sink: Arc<dyn EventSink> = vec_sink.clone();
    let cfg = TurnConfig { context_window: 16_384, ..cfg() };
    let mut history = large_tool_history(30_000);
    history.pop(); // No report yet for the result after the assistant's tool call.
    let mut conv = Conversation { history, context_tokens: 10_000 };
    let reason = c6_agent::compact_turn(1, &cfg, &client, &default_tools(), &sink, &mut conv, "", &CancellationToken::new()).await.unwrap();
    assert_eq!(reason, TurnEndReason::Completed);
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].get("tools").is_none(), "the report plus the new result requires a bounded digest");
    assert_eq!(compaction(&vec_sink.events.lock().unwrap()), "SUMMARY: read large.txt");
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

#[tokio::test]
async fn carried_history_compacts_before_the_first_request() {
    let mock = MockLlm::start().await;
    mock.push(Reply::Text("SUMMARY: carried conversation".into()));
    mock.push(Reply::Text("continued".into()));
    let dir = tempfile::tempdir().unwrap();
    let client = OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap();
    let vec_sink = Arc::new(VecSink::default());
    let sink: Arc<dyn EventSink> = vec_sink.clone();
    let session = Arc::new(SessionTools::new(dir.path().to_owned()));
    let cfg = TurnConfig { context_window: 16_384, ..cfg() };
    // A side chat's seed or a handover transcript, before any usage report.
    let mut conv = Conversation { history: vec![Message::user_text("carried context ".repeat(8_000))], context_tokens: 0 };
    let reason =
        run_turn(1, &cfg, &client, &default_tools(), &session, &sink, &mut conv, &mut Vec::new, &CancellationToken::new()).await.unwrap();
    assert_eq!(reason, TurnEndReason::Completed);
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let first = requests[0]["messages"].to_string();
    assert!(first.contains("Write a summary") && first.contains("Below is a conversation"), "{first}");
    assert!(first.len() < 16_384 * 7 / 2, "the digest fits the window");
    assert!(requests[0].get("tools").is_none());
    let chars: usize = requests[0]["messages"].as_array().unwrap().iter().map(|m| m["content"].as_str().unwrap().chars().count()).sum();
    assert!(
        (chars * 2).div_ceil(7) as u64 + requests[0]["max_tokens"].as_u64().unwrap() <= cfg.context_window,
        "the summary's input and output fit together"
    );
    assert!(requests[1]["messages"].to_string().contains("SUMMARY: carried conversation"));
    let events = vec_sink.events.lock().unwrap().clone();
    assert!(events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::Compaction(c)) if c.tokens_before == 0)));
}

#[tokio::test]
async fn a_large_tool_result_compacts_before_the_next_request() {
    let mock = MockLlm::start().await;
    mock.push(Reply::ToolCalls(vec![("read".into(), r#"{"path":"large.txt"}"#.into())]));
    mock.push(Reply::Text("SUMMARY: read large.txt".into()));
    mock.push(Reply::Text("done".into()));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("large.txt"), "tool output\n".repeat(2_000)).unwrap();
    let client = ReportedUsage { inner: OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap(), tokens: 10_000 };
    let vec_sink = Arc::new(VecSink::default());
    let sink: Arc<dyn EventSink> = vec_sink.clone();
    let session = Arc::new(SessionTools::new(dir.path().to_owned()));
    let cfg = TurnConfig { context_window: 16_384, ..cfg() };
    let mut conv = Conversation { history: vec![Message::user_text("read large.txt")], context_tokens: 0 };
    let reason =
        run_turn(1, &cfg, &client, &default_tools(), &session, &sink, &mut conv, &mut Vec::new, &CancellationToken::new()).await.unwrap();
    assert_eq!(reason, TurnEndReason::Completed);
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[1]["messages"].to_string().contains("Write a summary"));
    assert!(requests[2]["messages"].to_string().contains("SUMMARY: read large.txt"));
    let events = vec_sink.events.lock().unwrap().clone();
    assert!(events.iter().any(|e| matches!(&e.kind, Some(session_event::Kind::Compaction(c)) if c.tokens_before == 10_000)));
    assert_eq!(conv.context_tokens, 10_000, "usage remains the reported size");
}

#[tokio::test]
async fn a_small_fresh_request_does_not_compact() {
    let mock = MockLlm::start().await;
    mock.push(Reply::Text("hi".into()));
    let dir = tempfile::tempdir().unwrap();
    let client = OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap();
    let vec_sink = Arc::new(VecSink::default());
    let sink: Arc<dyn EventSink> = vec_sink.clone();
    let session = Arc::new(SessionTools::new(dir.path().to_owned()));
    let cfg = TurnConfig { context_window: 16_384, ..cfg() };
    let mut conv = Conversation { history: vec![Message::user_text("hi")], context_tokens: 0 };
    let reason =
        run_turn(1, &cfg, &client, &default_tools(), &session, &sink, &mut conv, &mut Vec::new, &CancellationToken::new()).await.unwrap();
    assert_eq!(reason, TurnEndReason::Completed);
    assert_eq!(mock.requests().len(), 1);
    assert!(!vec_sink.events.lock().unwrap().iter().any(|e| matches!(e.kind, Some(session_event::Kind::Compaction(_)))));
}

#[tokio::test]
async fn reported_usage_below_the_threshold_does_not_compact_large_raw_history() {
    for window in [200_000, 272_000] {
        let mock = MockLlm::start().await;
        mock.push(Reply::Text("done".into()));
        mock.push(Reply::Text("unexpected compaction".into()));
        let dir = tempfile::tempdir().unwrap();
        let client = ReportedUsage { inner: OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap(), tokens: 80_000 };
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let session = Arc::new(SessionTools::new(dir.path().to_owned()));
        let cfg = TurnConfig { context_window: window, ..cfg() };
        // The report covers this history, whose raw estimate exceeds the window.
        let mut conv = Conversation { history: large_tool_history(window as usize * 4), context_tokens: 80_000 };
        let reason = run_turn(1, &cfg, &client, &default_tools(), &session, &sink, &mut conv, &mut Vec::new, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(reason, TurnEndReason::Completed);
        assert_eq!(mock.requests().len(), 1);
        assert!(!vec_sink.events.lock().unwrap().iter().any(|e| matches!(e.kind, Some(session_event::Kind::Compaction(_)))));
    }
}

#[tokio::test]
async fn reported_usage_keeps_the_full_summary_and_output_limit() {
    for (window, manual) in [(200_000, false), (272_000, false), (200_000, true), (272_000, true)] {
        let mock = MockLlm::start().await;
        mock.push(Reply::Text("SUMMARY: read large.txt".into()));
        if !manual {
            mock.push(Reply::Text("done".into()));
        }
        let dir = tempfile::tempdir().unwrap();
        let reported = window * 86 / 100;
        let client = ReportedUsage { inner: OpenAiChat::new(&mock.base_url(), "", HashMap::new()).unwrap(), tokens: reported };
        let vec_sink = Arc::new(VecSink::default());
        let sink: Arc<dyn EventSink> = vec_sink.clone();
        let session = Arc::new(SessionTools::new(dir.path().to_owned()));
        let cfg = TurnConfig { context_window: window, ..cfg() };
        let mut conv = Conversation { history: large_tool_history(window as usize * 4), context_tokens: reported };
        let tools = default_tools();
        let cancel = CancellationToken::new();
        let reason = if manual {
            c6_agent::compact_turn(1, &cfg, &client, &tools, &sink, &mut conv, "", &cancel).await.unwrap()
        } else {
            run_turn(1, &cfg, &client, &tools, &session, &sink, &mut conv, &mut Vec::new, &cancel).await.unwrap()
        };
        assert_eq!(reason, TurnEndReason::Completed);
        let requests = mock.requests();
        assert_eq!(requests.len(), if manual { 1 } else { 2 });
        let tool = requests[0]["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool["content"].as_str().unwrap().len(), window as usize * 4, "the full history reaches the summary call");
        assert_eq!(requests[0]["tools"].as_array().unwrap().len(), tools.len());
        assert_eq!(requests[0]["max_tokens"], 16_000, "reported usage leaves the full summary output budget");
        let events = vec_sink.events.lock().unwrap().clone();
        assert_eq!(compaction(&events), "SUMMARY: read large.txt");
    }
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
        session_event::Kind::SubagentReported(_) => "subagent_reported",
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
