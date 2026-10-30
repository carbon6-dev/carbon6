//! Work and the memex (docs/work.md §3): a request attached to work makes
//! it need you while you owe the reply, and moves it when it changes.

use c6_head::HeadConfig;
use c6_head::realms::Caller;
use serde_json::{Value, json};

#[tokio::test]
async fn a_request_you_owe_is_work_that_needs_you() {
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
    let st = head.state.clone();
    let call = |method: &'static str, params: Value| {
        let st = st.clone();
        async move { c6_head::memex::call_in(&st, "home", method, params).await.unwrap() }
    };
    let m = c6_head::knowledge::get(&head.state).expect("the head's knowledge store");
    let found = c6_memex::requests::Found {
        target: "chat:T1:D1",
        kind: "chat",
        category: "needs_you",
        reason: "asks you for the numbers",
        asks: "the Q3 numbers",
        due: None,
        person: "ana@acme.com",
        person_name: "Ana",
        title: "Send the Q3 numbers",
        at: c6_core::now_ms(),
        seen_mail: &[],
        urgent: false,
        invitation: false,
    };
    c6_memex::requests::follow(&m, &found, None).await.unwrap();
    let id = call("memex.requests.list", json!({})).await[0]["id"].as_i64().expect("a request");
    // Read in a batch, as work lists them; one that's gone is left out.
    let some = call("memex.requests.some", json!({"ids": [id, 9999]})).await;
    assert_eq!(some.as_array().map(Vec::len), Some(1), "{some}");
    assert_eq!((some[0]["person_name"].as_str(), some[0]["status"].as_str()), (Some("Ana"), Some("open")));

    let you = Caller::in_realm("home");
    let new = c6_api::work::WorkNew {
        title: "Q3 numbers for Ana".into(),
        attach: vec![c6_api::work::WorkRef { reference: format!("request:{id}"), title: String::new() }],
        ..Default::default()
    };
    let w = c6_head::work::create(&head.state, &you, &new, "you").await.unwrap();
    assert_eq!(w.state, "needs_you", "{}", w.why);
    assert_eq!(w.why, "You owe a reply: Ana: the Q3 numbers");
    assert_eq!(c6_head::work::of(&head.state, &format!("request:{id}"), &you).await.unwrap()[0].id, w.id);

    // Answered: it moved, and nothing is owed.
    let before = w.fields.moved_at;
    call("memex.requests.set", json!({"id": id, "status": "done"})).await;
    let w = c6_head::work::get(&head.state, &w.id, &you).await.unwrap();
    assert_eq!(w.live[0].state, "done");
    assert_eq!(w.state, "open", "{}", w.why);
    assert!(w.fields.moved_at >= before);
    // Snoozed in the Brief: waiting.
    call("memex.requests.set", json!({"id": id, "status": "snoozed", "until": c6_core::now_ms() + 3_600_000})).await;
    assert_eq!(c6_head::work::get(&head.state, &w.id, &you).await.unwrap().state, "waiting");
}

#[tokio::test]
async fn merged_request_work_and_messages_follow_old_ids_in_only_their_own_realm() {
    let dir = tempfile::tempdir().unwrap();
    let head = c6_head::start(HeadConfig {
        data_dir: dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let st = head.state.clone();
    c6_head::realms::create(&st, &c6_api::RealmIn { id: "team".into(), name: "Team".into(), ..Default::default() }, "you").await.unwrap();
    let call = |realm: &'static str, method: &'static str, params: Value| {
        let st = st.clone();
        async move { c6_head::memex::call_in(&st, realm, method, params).await.unwrap() }
    };
    call("team", "memex.requests.list", json!({})).await;
    let m = c6_head::knowledge::get(&st).unwrap();
    let team_pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", c6_head::knowledge::dir(&st).join("realms/team/memex.db").display()))
        .await
        .unwrap();
    // Both realms have the same request numbers, but different messages.
    for (realm, pool) in [("home", &m.store.pool), ("team", &team_pool)] {
        for id in 1..=3 {
            sqlx::query("INSERT INTO requests (id, person, person_name, title, category, created_at, updated_at) VALUES (?, 'pat@example.net', 'Pat', ?, 'needs_you', 0, 0)")
                .bind(id).bind(format!("{realm} request {id}")).execute(pool).await.unwrap();
            sqlx::query("INSERT INTO request_sources (request_id, target, kind, at) VALUES (?, ?, 'mail', ?)")
                .bind(id)
                .bind(format!("thread:import:{realm}:thread-{id}"))
                .bind(id)
                .execute(pool)
                .await
                .unwrap();
        }
    }
    let home = Caller::in_realm("home");
    let team = Caller::in_realm("team");
    let new = c6_api::work::WorkNew {
        title: "Work on the original request".into(),
        attach: vec![c6_api::work::WorkRef { reference: "request:2".into(), title: String::new() }],
        ..Default::default()
    };
    let w = c6_head::work::create(&st, &home, &new, "you").await.unwrap();
    let other = c6_head::work::create(&st, &team, &new, "you").await.unwrap();
    let original = w.fields.attached.clone();
    let original_version = w.version;
    let original_history: Vec<String> = sqlx::query_scalar("SELECT fields FROM record_history WHERE record_id = ? ORDER BY version")
        .bind(&w.id)
        .fetch_all(st.store.pool())
        .await
        .unwrap();
    let other_moved = other.fields.moved_at;

    call("home", "memex.requests.merge", json!({"into": 1, "from": 2})).await;
    let w = c6_head::work::get(&st, &w.id, &home).await.unwrap();
    assert_eq!(w.live[0].title, "Pat: home request 1");
    assert_eq!(w.live[0].state, "open");
    assert_eq!(w.state, "needs_you");
    assert_eq!(w.fields.attached, original);
    let r = call("home", "memex.requests.get", json!({"id": 2})).await;
    assert_eq!(r["id"], 1);
    assert_eq!(r["sources"].as_array().unwrap().len(), 2);
    assert!(r["sources"].as_array().unwrap().iter().any(|s| s["target"] == "thread:import:home:thread-2"));

    call("home", "memex.requests.merge", json!({"into": 3, "from": 1})).await;
    call("home", "memex.requests.set", json!({"id": 2, "status": "snoozed", "until": c6_core::now_ms() + 86_400_000})).await;
    let w = c6_head::work::get(&st, &w.id, &home).await.unwrap();
    assert_eq!((w.state.as_str(), w.live[0].state.as_str()), ("waiting", "snoozed"));
    assert_eq!(w.live[0].title, "Pat: home request 3");
    assert_eq!(w.fields.attached, original);
    assert_eq!(w.version, original_version, "merging does not rewrite Work history");
    let history: Vec<String> = sqlx::query_scalar("SELECT fields FROM record_history WHERE record_id = ? ORDER BY version")
        .bind(&w.id)
        .fetch_all(st.store.pool())
        .await
        .unwrap();
    assert_eq!(history, original_history);
    assert!(w.fields.moved_at > original[0].at, "changes to the survivor move work holding an alias");
    for id in [1, 2] {
        let r = call("home", "memex.requests.get", json!({"id": id})).await;
        assert_eq!(r["id"], 3);
        assert_eq!(r["sources"].as_array().unwrap().len(), 3);
        assert!(r["sources"].as_array().unwrap().iter().all(|s| s["target"].as_str().unwrap().starts_with("thread:import:home:")));
    }
    // An all-realms Work read must not confuse identically numbered refs.
    let all = c6_head::work::list(&st, &Caller::root(), false).await.unwrap();
    let other_now = all.iter().find(|i| i.id == other.id).unwrap();
    assert_eq!(other_now.live[0].title, "Pat: team request 2");
    assert_eq!(other_now.state, "needs_you");
    assert_eq!(other_now.fields.moved_at, other_moved, "home's request events must not touch team's work");
    let r = call("team", "memex.requests.get", json!({"id": 2})).await;
    assert_eq!(r["id"], 2);
    assert_eq!(r["sources"][0]["target"], "thread:import:team:thread-2");

    call("home", "memex.requests.set", json!({"id": 1, "status": "done"})).await;
    assert_eq!(c6_head::work::get(&st, &w.id, &home).await.unwrap().live[0].state, "done");
    assert_eq!(call("home", "memex.requests.get", json!({"id": 2})).await["sources"].as_array().unwrap().len(), 3);
    team_pool.close().await;
}
