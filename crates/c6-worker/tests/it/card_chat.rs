//! A card's discussion explains and revises it without answering it. Its
//! thread and before/after history survive the ordinary answer buttons.

use c6_api::{Decision, Session};
use c6_testkit::{Reply, eventually};
use serde_json::{Value, json};

use crate::batch_capture::{Env, user_texts};

const INSTRUCTIONS: &str = "Discuss this decision card, using only its discussion tools. CARD_CHAT_TEST instructions.";
const TITLE: &str = "Remove the obsolete retry adapter";
const WHY: &str = "The client already retries; the adapter doubles retries.";
const PROMPT: &str = "Delete the obsolete retry adapter and its tests.";

async fn proposed(env: &Env, cwd: &std::path::Path) -> (Session, Decision) {
    let r =
        env.http.put(format!("{}/settings/prompts/decision.discuss", env.api)).json(&json!({"body": INSTRUCTIONS})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    env.mock.push_about(
        "inspect the retry adapter",
        Reply::ToolCalls(vec![("task_propose".into(), json!({"title": TITLE, "why": WHY, "prompt": PROMPT}).to_string())]),
    );
    env.mock.push_about("inspect the retry adapter", Reply::Text("I proposed the adapter cleanup. SOURCE_DIGEST_MARKER".into()));
    let source = env.start(cwd, "inspect the retry adapter").await;
    env.ended(&source.id, 1).await;
    let cards: Vec<Decision> = serde_json::from_value(env.get(&format!("sessions/{}/decisions", source.id)).await).unwrap();
    assert_eq!(cards.len(), 1, "{cards:?}");
    (source, cards.into_iter().next().unwrap())
}

fn tool(name: &str, args: Value) -> Reply {
    Reply::ToolCalls(vec![(name.into(), args.to_string())])
}

fn messages(thread: &Value) -> Vec<(&str, &str)> {
    thread["messages"].as_array().unwrap().iter().map(|m| (m["role"].as_str().unwrap(), m["text"].as_str().unwrap())).collect()
}

fn discussion_requests(env: &Env) -> Vec<Value> {
    env.mock.requests().into_iter().filter(|r| r["messages"].to_string().contains("CARD_CHAT_TEST instructions")).collect()
}

#[tokio::test]
async fn discussion_sees_the_card_revises_it_and_start_uses_the_revision() {
    let env = Env::new().await;
    let cwd = tempfile::tempdir().unwrap();
    let (source, card) = proposed(&env, cwd.path()).await;
    let path = format!("decisions/{}/discussion", card.id);
    let empty = env.get(&path).await;
    assert_eq!(messages(&empty), []);
    assert_eq!(empty["revisions"], json!([]));
    assert_eq!(empty["closed"], false);
    assert_eq!(empty["can_ask_session"], true);

    // Native provider tool calls, not tool-shaped assistant text: the
    // source read and revision must both execute in the head's loop.
    let question = "Keep the compatibility shim and revise the cleanup accordingly.";
    env.mock.push_about(question, tool("read_source_events", json!({"after": 0, "limit": 4})));
    env.mock.push_about(
        question,
        tool(
            "revise_card",
            json!({"title": "Retire only the retry adapter", "why": "Keep the public compatibility shim.",
                   "prompt": "Remove the retry adapter, keep the compatibility shim, and run its tests.",
                   "worktree": false, "labels": ["compatibility"]}),
        ),
    );
    env.mock.push_about(question, Reply::Text("Revised; the compatibility shim stays.".into()));
    let thread = env.post(&path, json!({"text": question})).await;
    assert_eq!(messages(&thread), [("user", question), ("assistant", "Revised; the compatibility shim stays.")]);
    assert!(thread["messages"].as_array().unwrap().iter().all(|m| m["created_at"].is_i64()));
    assert_eq!(thread["closed"], false);
    let revisions = thread["revisions"].as_array().unwrap();
    assert_eq!(revisions.len(), 1, "{thread}");
    assert_eq!(revisions[0]["before"], json!({"title": card.title, "document": card.document, "context": card.context}));
    assert!(revisions[0]["created_at"].is_i64());
    let revised = env.decision(card.id).await;
    assert_eq!(revised.title, "Retire only the retry adapter");
    assert_eq!(revised.document, "Keep the public compatibility shim.");
    assert_eq!(revised.context["prompt"], "Remove the retry adapter, keep the compatibility shim, and run its tests.");
    assert_eq!(revised.context["labels"], json!(["compatibility"]));
    assert_eq!(revised.context["worktree"], false);
    assert_eq!(revised.context["session"], source.id);
    assert_eq!(revised.options, card.options, "a discussion cannot change the buttons");
    assert_eq!(revised.state, "open");
    assert_eq!(revisions[0]["after"], json!({"title": revised.title, "document": revised.document, "context": revised.context}));

    let requests = discussion_requests(&env);
    assert_eq!(requests.len(), 3, "{}", env.mock.log());
    let first = &requests[0];
    assert!(
        first["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "system" && m["content"].as_str().is_some_and(|s| s.contains(INSTRUCTIONS)))
    );
    let seed = first["messages"].as_array().unwrap().iter().find(|m| m["role"] == "user").unwrap();
    let seed: Value = serde_json::from_str(seed["content"].as_str().unwrap()).expect("the card context is JSON");
    assert_eq!(seed["card"], serde_json::to_value(&card).unwrap(), "the full card, including context and answer options");
    assert_eq!(seed["source"]["session"], source.id);
    assert_eq!(seed["source"]["worker"], env.wid.to_string());
    let source_cwd = env
        .events(&source.id)
        .await
        .into_iter()
        .find_map(|e| match e.kind {
            c6_api::EventKind::Started { cwd, .. } => Some(cwd),
            _ => None,
        })
        .unwrap();
    assert_eq!(seed["source"]["cwd"], source_cwd);
    assert_eq!(seed["source"]["worktree"], false);
    assert!(seed["source"]["labels"].is_array());
    assert!(seed.to_string().contains("digest") && seed.to_string().contains("inspect the retry adapter"), "{seed}");
    let names: Vec<&str> = first["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(names.contains(&"read_source_events") && names.contains(&"revise_card"), "{names:?}");
    let loop_messages = requests[2]["messages"].as_array().unwrap();
    assert_eq!(loop_messages.iter().filter(|m| m["role"] == "tool").count(), 2, "{loop_messages:?}");
    assert!(requests[1]["messages"].to_string().contains("inspect the retry adapter"));

    // A later turn gets the latest card AND the earlier conversation.
    env.mock.push_about("What will the revised task preserve?", Reply::Text("The public compatibility shim.".into()));
    let continued = env.post(&path, json!({"text": "What will the revised task preserve?"})).await;
    assert_eq!(messages(&continued).len(), 4);
    let last = discussion_requests(&env).pop().unwrap()["messages"].to_string();
    assert!(last.contains("Retire only the retry adapter") && last.contains("Revised; the compatibility shim stays."), "{last}");
    assert_eq!(continued["revisions"], thread["revisions"]);

    env.mock.push_about("Remove the retry adapter, keep the compatibility shim", Reply::Text("Removed only the adapter.".into()));
    env.mock.push_about("Start session:", Reply::Text("Noted the revised task starting.".into()));
    env.post(&format!("decisions/{}/answer", card.id), json!({"option": "Start it"})).await;
    let done = env.decision(card.id).await;
    assert_eq!(done.state, "answered");
    assert_eq!(done.sessions.len(), 1);
    let task = &done.sessions[0];
    let task_texts = user_texts(&env.ended(task, 1).await);
    assert!(task_texts.iter().any(|t| t.contains(revised.context["prompt"].as_str().unwrap())), "{task_texts:?}");
    assert!(!task_texts.iter().any(|t| t.contains(PROMPT)), "the old prompt must not run: {task_texts:?}");
    assert_eq!(env.get(&format!("sessions/{task}")).await["session"]["title"], revised.title);
    env.ended(&source.id, 2).await;

    let closed = env.get(&path).await;
    assert_eq!(closed["closed"], true);
    assert_eq!(closed["messages"], continued["messages"]);
    assert_eq!(closed["revisions"], thread["revisions"], "the old title, why and prompt remain in history");
    let count = discussion_requests(&env).len();
    env.refused(&path, json!({"text": "Change it after answering"})).await;
    assert_eq!(discussion_requests(&env).len(), count, "closed threads do not call the model");
    assert_eq!(env.get(&path).await, closed, "even the rejected user message is not appended");
    env.stop.cancel();
}

#[tokio::test]
async fn non_task_discussion_merges_context_but_cannot_change_task_fields_or_options() {
    let env = Env::new().await;
    let cwd = tempfile::tempdir().unwrap();
    let (source, _) = proposed(&env, cwd.path()).await;
    let card: Decision = serde_json::from_value(
        env.post(
            "decisions",
            json!({"kind": "choose", "title": "Which retry policy?", "document": "Choose a policy for clients.",
                   "source": format!("session:{}", source.id),
                   "context": {"session": source.id, "facts": [17], "existing": "retain this"},
                   "options": [{"label": "Once", "does": "Retry once."}, {"label": "Never", "does": "Do not retry."}]}),
        )
        .await,
    )
    .unwrap();
    let question = "Clarify the retry policy wording without changing the choices.";
    env.mock.push_about(
        question,
        tool(
            "revise_card",
            json!({"title": "How many client retries?", "document": "Only the client retries, not the adapter.",
                                  "context": {"discussion": "client only"}}),
        ),
    );
    env.mock.push_about(question, tool("revise_card", json!({"prompt": "A choose card is not a task."})));
    env.mock.push_about(question, tool("revise_card", json!({"context": {"session": "another-session", "worker": "another-worker"}})));
    env.mock.push_about(question, Reply::Text("Clarified the client policy; the choices stay the same.".into()));
    let thread = env.post(&format!("decisions/{}/discussion", card.id), json!({"text": question})).await;
    assert_eq!(thread["revisions"].as_array().unwrap().len(), 1, "{thread}");
    let revised = env.decision(card.id).await;
    assert_eq!(revised.title, "How many client retries?");
    assert_eq!(revised.document, "Only the client retries, not the adapter.");
    assert_eq!(revised.context, json!({"session": source.id, "facts": [17], "existing": "retain this", "discussion": "client only"}));
    assert_eq!(revised.options, card.options);
    assert_eq!(revised.source, card.source);
    assert_eq!(revised.state, "open");
    assert!(revised.answer.is_none());
    let requests = discussion_requests(&env);
    assert_eq!(requests.len(), 4, "{}", env.mock.log());
    let messages = requests[2]["messages"].as_array().unwrap();
    let error = messages.iter().rev().find(|m| m["role"] == "tool").unwrap();
    assert!(error["content"].to_string().contains("only"), "the tool must reject task-only fields: {error}");
    let messages = requests[3]["messages"].as_array().unwrap();
    assert_eq!(messages.iter().filter(|m| m["role"] == "tool").count(), 3);
    env.stop.cancel();
}

#[tokio::test]
async fn ask_source_opens_an_ordinary_side_chat_without_answering_the_card() {
    let env = Env::new().await;
    let cwd = tempfile::tempdir().unwrap();
    let (source, card) = proposed(&env, cwd.path()).await;
    let path = format!("decisions/{}/discussion", card.id);
    let before = env.get(&path).await;
    let source_events = env.events(&source.id).await.len();
    let question = "Which callers still need the compatibility shim?";
    env.mock.push_about(question, Reply::Text("Only the legacy client needs the shim.".into()));
    let side: Session = serde_json::from_value(env.post(&format!("{path}/session"), json!({"text": question})).await).unwrap();
    assert_eq!(side.parent_id.as_deref(), Some(source.id.as_str()));
    assert!(side.fork_seq.is_some(), "an ordinary side chat: {side:?}");
    assert_eq!(side.realm, card.realm);
    assert_eq!(side.connection_id, source.connection_id);
    let events = env.ended(&side.id, 1).await;
    let texts = user_texts(&events);
    for text in [question, TITLE, PROMPT] {
        assert!(texts.iter().any(|t| t.contains(text)), "missing {text}: {texts:?}");
    }
    assert_eq!(env.events(&source.id).await.len(), source_events, "the proposer was not sent a message");
    assert_eq!(env.decision(card.id).await.state, "open");
    assert_eq!(env.get(&path).await, before, "the source chat is not the card's head-run thread");

    let r = env.http.patch(format!("{}/sessions/{}", env.api, source.id)).json(&json!({"archived": true})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    assert_eq!(env.get(&path).await["can_ask_session"], false);
    env.refused(&format!("{path}/session"), json!({"text": "Ask the archived proposer"})).await;
    env.stop.cancel();
}

#[tokio::test]
async fn discussion_cannot_answer_or_retarget_a_card_and_is_realm_scoped() {
    let env = Env::new().await;
    let cwd = tempfile::tempdir().unwrap();
    let (source, card) = proposed(&env, cwd.path()).await;
    let path = format!("decisions/{}/discussion", card.id);
    let question = "Try answering this card and changing its source.";
    env.mock.push_about(question, tool("decision_answer", json!({"id": card.id, "option": "Start it"})));
    env.mock.push_about(question, tool("revise_card", json!({"context": {"session": "another-session", "worker": "another-worker"}})));
    env.mock.push_about(question, Reply::Text("I cannot answer it or change its source.".into()));
    let thread = env.post(&path, json!({"text": question})).await;
    assert_eq!(thread["revisions"], json!([]));
    assert_eq!(messages(&thread), [("user", question), ("assistant", "I cannot answer it or change its source.")]);
    let unchanged = env.decision(card.id).await;
    assert_eq!(unchanged.state, "open");
    assert!(unchanged.answer.is_none() && unchanged.sessions.is_empty());
    assert_eq!(unchanged.context, card.context);
    assert_eq!(env.events(&source.id).await.len(), env.ended(&source.id, 1).await.len());
    let requests = discussion_requests(&env);
    assert_eq!(requests.len(), 3, "{}", env.mock.log());
    let tools: Vec<&Value> = requests[2]["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(tools.len(), 2, "{tools:?}");
    assert!(
        tools.iter().all(|m| m["content"].to_string().contains("only")),
        "both unavailable tools and protected fields are rejected: {tools:?}"
    );

    // Auth is off for the fixture, but a supplied valid token still scopes
    // the caller. This card belongs to home, not the separate realm.
    env.post("realms", json!({"id": "lab", "name": "Lab"})).await;
    let token = env.post("auth/tokens", json!({"name": "lab-card-reader", "realm": "lab"})).await;
    let token = token["token"].as_str().unwrap();
    for (method, suffix, body) in [
        (reqwest::Method::GET, "discussion", json!({})),
        (reqwest::Method::POST, "discussion", json!({"text": "Read another realm's proposal"})),
        (reqwest::Method::POST, "discussion/session", json!({})),
    ] {
        let r = env
            .http
            .request(method.clone(), format!("{}/decisions/{}/{suffix}", env.api, card.id))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert!(matches!(r.status(), reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::NOT_FOUND), "{method} {suffix}: {}", r.status());
    }
    assert_eq!(env.get(&path).await, thread);
    assert_eq!(discussion_requests(&env).len(), requests.len(), "denied requests never reach the model");
    env.stop.cancel();
}

#[tokio::test]
async fn answering_while_the_model_is_running_discards_its_late_revision() {
    let env = Env::new().await;
    let cwd = tempfile::tempdir().unwrap();
    let (source, card) = proposed(&env, cwd.path()).await;
    let path = format!("decisions/{}/discussion", card.id);
    let question = "Revise this card after considering its source.";
    let gate = env.mock.push_gated_about(question, tool("revise_card", json!({"title": "Too late", "prompt": "This must never run."})));
    let (http, url) = (env.http.clone(), format!("{}/{path}", env.api));
    let pending = tokio::spawn(async move { http.post(url).json(&json!({"text": question})).send().await.unwrap() });
    eventually("the discussion model request to start", async || (!discussion_requests(&env).is_empty()).then_some(())).await;
    env.mock.push_about("Not now", Reply::Text("Noted, leaving the adapter alone.".into()));
    env.post(&format!("decisions/{}/answer", card.id), json!({"option": "Not now"})).await;
    gate.notify_one();
    let response = c6_testkit::within("the answered discussion to finish", pending).await.unwrap();
    assert!(!response.status().is_server_error(), "{}", response.text().await.unwrap());
    let closed = env.get(&path).await;
    assert_eq!(closed["closed"], true);
    assert_eq!(closed["revisions"], json!([]));
    assert!(!messages(&closed).iter().any(|(role, _)| *role == "assistant"), "a late assistant response must not be saved: {closed}");
    let done = env.decision(card.id).await;
    assert_eq!(done.state, "answered");
    assert_eq!(done.title, card.title);
    assert_eq!(done.context, card.context);
    assert!(done.sessions.is_empty());
    env.ended(&source.id, 2).await;
    env.stop.cancel();
}
