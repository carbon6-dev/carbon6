//! The assistant's operations (`c6_call`) read the memex as its
//! conversation's scope allows, as its memex tools do (docs/assistant.md
//! §6): mail and chat stay out of its model's sight, and out of what the
//! memex's model summarizes for it, until a scope rule for `assistant` lets
//! them in. The model can't widen its scope by asking for one.

use c6_api::{Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_memex::mail::{Parsed, Stored};
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use serde_json::{Value, json};

const HOUR: i64 = 3_600_000;

#[tokio::test]
async fn the_assistant_reads_within_its_scope() {
    let (mock, reader) = (MockLlm::start().await, MockLlm::start().await);
    let head_dir = tempfile::tempdir().unwrap();
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
    let put = async |path: &str, body: Value| {
        let r = http.put(format!("{api}/{path}")).json(&body).send().await.unwrap();
        assert!(r.status().is_success(), "{path}: {}", r.text().await.unwrap());
    };
    for (name, llm) in [("m", &mock), ("reader", &reader)] {
        let r = http.post(format!("{api}/connections")).json(&json!({"name": name, "base_url": llm.base_url()})).send().await.unwrap();
        assert!(r.status().is_success());
    }
    put("settings/assistant", json!({"model": {"connection": "m", "model": "mock-1"}})).await;
    // The memex's model sums things up when asked (what it reads itself
    // meanwhile fails for now: its script has only the summaries).
    put("settings/memex", json!({"understanding": {"connection": "reader", "model": "reader-1"}, "calls_per_day": 1})).await;

    // About Ana: her mail, her Slack DM, and your note; a commitment read
    // from the mail, and one from the note.
    let m = c6_head::knowledge::get(&head.state).expect("the head's knowledge store");
    let ana = ("ana@acme.com", "Ana");
    let parsed = Parsed {
        message_id: "<a1@test>".into(),
        from: (ana.0.into(), ana.1.into()),
        to: vec![("me@mine.io".into(), String::new())],
        subject: "Pilot".into(),
        text: "The pilot code is 4417.".into(),
        ..Default::default()
    };
    let at = Stored {
        account: "import:mbox",
        external_id: "a1",
        thread_id: "t1",
        source: "test-mail",
        rfc_id: &parsed.message_id,
        origin: "imported",
        owner: "me@mine.io",
        message: "a1",
    };
    c6_memex::mail::store(&m, &parsed, "", &at, &["INBOX".into()], c6_core::now_ms() - HOUR).await.unwrap();
    let dm = c6_api::MemexItemIn {
        kind: "chat".into(),
        source: "test-slack".into(),
        external_id: "k1".into(),
        title: "D9 · Ana".into(),
        text: "Ana: Can we move our 1:1 to Wednesday?".into(),
        created_at: Some(c6_core::now_ms() - HOUR),
        meta: json!({"service": "slack", "workspace": "T1", "channel": "D9", "channel_name": "D9", "channel_kind": "dm",
                     "ts": "1000.0001", "user": "U2", "user_name": "Ana", "from_me": false, "mentions_me": false}),
        ..Default::default()
    };
    m.store.put(&dm, "test-slack").await.unwrap();
    let note = c6_api::MemexItemIn {
        kind: "note".into(),
        source: "you".into(),
        title: "Ana runs the pilot".into(),
        text: "Ana runs the pilot at Acme.".into(),
        ..Default::default()
    };
    let note = m.store.put(&note, "you").await.unwrap();
    let person = c6_memex::people::person(&m.store, ana.0, ana.1).await.unwrap();
    m.store.link(&note, "about", &person, "observed", "you", None).await.unwrap();
    let email: String = sqlx::query_scalar("SELECT id FROM items WHERE kind = 'email'").fetch_one(&m.store.pool).await.unwrap();
    for (item, what) in [(&email, "share the pilot code"), (&note, "send Ana the deck")] {
        sqlx::query("INSERT INTO commitments (item_id, who, what, status, made_by, created_at) VALUES (?, 'me', ?, 'open', 'test', 0)")
            .bind(item)
            .bind(what)
            .execute(&m.store.pool)
            .await
            .unwrap();
    }

    // A conversation making `calls`: what each returned, and whether it failed.
    let converse = async |calls: &[(&str, Value)]| -> Vec<(String, bool)> {
        reader.push_about("Sum up what these records say", Reply::Text(json!({"summary": "Ana runs the pilot."}).to_string()));
        for (op, input) in calls {
            mock.push(Reply::ToolCalls(vec![("c6_call".into(), json!({"op": op, "input": input}).to_string())]));
        }
        mock.push(Reply::Text("Done.".into()));
        let s: Session = http
            .post(format!("{api}/assistant"))
            .json(&json!({"prompt": "what's up with Ana?"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        eventually("the conversation's calls", async || {
            let es: Vec<Event> = http.get(format!("{api}/sessions/{}/events?after=0", s.id)).send().await.ok()?.json().await.ok()?;
            let outs: Vec<(String, bool)> = es
                .into_iter()
                .filter_map(|e| match e.kind {
                    EventKind::ToolResult { output, is_error, .. } => Some((output, is_error)),
                    _ => None,
                })
                .collect();
            (outs.len() == calls.len()).then_some(outs)
        })
        .await
    };
    let summarized = || reader.requests().iter().map(|r| r["messages"].to_string()).rfind(|t| t.contains("Sum up")).unwrap();
    let calls = [
        ("mail.thread", json!({"account": "import:mbox", "thread_id": "t1"})),
        ("chat.messages", json!({"workspace": "T1", "channel": "D9"})),
        ("memex.search", json!({"q": "pilot", "kinds": "email,chat,note"})),
        ("memex.commitments", json!({})),
        ("memex.summarize", json!({"id": "person:ana@acme.com"})),
    ];

    // The default scope: no mail, no chat.
    let mut widened = calls.to_vec();
    widened.push(("mail.thread", json!({"account": "import:mbox", "thread_id": "t1", "scope": ["email"]})));
    let r = converse(&widened).await;
    assert!(r[0].1 && r[0].0.contains("email is outside"), "mail.thread: {:?}", r[0]);
    assert!(r[1].1 && r[1].0.contains("chat is outside"), "chat.messages: {:?}", r[1]);
    assert!(!r[2].1 && r[2].0.contains("runs the pilot") && !r[2].0.contains("4417"), "memex.search: {:?}", r[2]);
    assert!(!r[3].1 && r[3].0.contains("the deck") && !r[3].0.contains("pilot code"), "memex.commitments: {:?}", r[3]);
    assert!(!r[4].1, "memex.summarize: {:?}", r[4]);
    assert!(summarized().contains("runs the pilot") && !summarized().contains("4417"), "{}", summarized());
    assert!(r[5].1 && r[5].0.contains("email is outside"), "a scope it asks for: {:?}", r[5]);

    // A scope rule lets mail and chat in.
    put("settings/memex_scopes", json!([{"name": "assistant reads mail", "applies_to": ["assistant"], "kinds": ["email", "chat"]}])).await;
    let r = converse(&calls).await;
    assert!(!r[0].1 && r[0].0.contains("4417"), "mail.thread: {:?}", r[0]);
    assert!(!r[1].1 && r[1].0.contains("Wednesday"), "chat.messages: {:?}", r[1]);
    assert!(!r[2].1 && r[2].0.contains("4417"), "memex.search: {:?}", r[2]);
    assert!(!r[3].1 && r[3].0.contains("pilot code"), "memex.commitments: {:?}", r[3]);
    assert!(summarized().contains("4417"), "{}", summarized());
    head.shutdown().await.unwrap();
}
