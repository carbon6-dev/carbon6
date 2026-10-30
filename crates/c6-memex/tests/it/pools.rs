//! Pools (docs/pools.md): the memex sorts mail through a pool of two model
//! servers. A call goes to a member with a free slot (one each here), so
//! while one member is busy the next goes to the other; when a member goes
//! away mid-run, its call goes to the other and nothing is misfiled. Each
//! call shows with the member that took it.

use c6_api::{Connection, Triage};
use c6_head::HeadConfig;
use c6_memex::mail::{Parsed, Stored};
use c6_testkit::{MockLlm, Reply, eventually};
use serde_json::{Value, json};

/// A stranger's message in a thread of its own (the model sorts it in one call).
async fn mail(m: &c6_memex::Memex, id: &str, from: &str, text: &str) {
    let parsed = Parsed {
        message_id: format!("<{id}@test>"),
        from: (from.into(), String::new()),
        to: vec![("me@mine.io".into(), String::new())],
        subject: format!("About {id}"),
        text: text.into(),
        ..Default::default()
    };
    let at = Stored {
        account: "acct",
        external_id: id,
        thread_id: id,
        source: "test-mail",
        rfc_id: &parsed.message_id,
        origin: "mirrored",
        owner: "me@mine.io",
        message: id,
    };
    c6_memex::mail::store(m, &parsed, "", &at, &["INBOX".to_owned()], c6_core::now_ms() - 3_600_000).await.unwrap();
}

#[tokio::test]
async fn the_memex_sorts_through_a_pool_and_survives_a_member_going_away() {
    let (a, b) = (MockLlm::start().await, MockLlm::start().await);
    // Any call (sorting, or reading after) gets an answer from either.
    let answer = json!({"category": "fyi", "reason": "For later", "summary": "A note", "topics": []}).to_string();
    let gate = a.push_gated(Reply::Text(answer.clone()));
    for _ in 0..40 {
        a.push(Reply::Text(answer.clone()));
        b.push(Reply::Text(answer.clone()));
    }
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
    let post =
        async |path: &str, body: Value| -> reqwest::Response { http.post(format!("{api}/{path}")).json(&body).send().await.unwrap() };
    let mac: Connection = post("connections", json!({"name": "mac", "base_url": a.base_url()})).await.json().await.unwrap();
    let mumuzz: Connection = post("connections", json!({"name": "mumuzz", "base_url": b.base_url()})).await.json().await.unwrap();

    // A pool's members are other API connections, each once, with 1 to 64 slots.
    let bad = [
        json!([]),
        json!([{"connection_id": "nope", "slots": 1}]),
        json!([{"connection_id": "mac", "slots": 0}]),
        json!([{"connection_id": "mac", "slots": 1}, {"connection_id": mac.id, "slots": 2}]),
    ];
    for members in bad {
        let r = post("connections", json!({"kind": "pool", "name": "p", "members": members})).await;
        assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST, "{members}");
    }
    let r = post("connections", json!({"kind": "pool", "name": "ollamas", "members": [{"connection_id": "mac", "slots": 1}]})).await;
    let pool: Connection = r.json().await.unwrap();
    assert_eq!((pool.kind.as_str(), pool.members.len()), ("pool", 1));
    let r = post("connections", json!({"kind": "pool", "name": "nested", "members": [{"connection_id": "ollamas", "slots": 1}]})).await;
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST, "a pool of pools");
    let pool: Connection = http
        .patch(format!("{api}/connections/{}", pool.id))
        .json(&json!({"members": [{"connection_id": "mac", "slots": 1}, {"connection_id": "mumuzz", "slots": 1}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pool.members.iter().map(|m| m.connection_id.as_str()).collect::<Vec<_>>(), [mac.id.as_str(), mumuzz.id.as_str()]);
    let models: Vec<String> = http.get(format!("{api}/connections/{}/models", pool.id)).send().await.unwrap().json().await.unwrap();
    assert_eq!(models, ["mock-1", "mock-2"], "its members' models");
    // Sessions dial their connection from their worker: no pools there.
    let r = http
        .put(format!("{api}/settings/routes"))
        .json(&json!([{"name": "x", "connection": "ollamas", "model": "mock-1"}]))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);

    // Two lanes, a call each at once.
    let m = c6_head::knowledge::get(&head.state).expect("the head's knowledge store");
    mail(&m, "one", "pat@northwind.com", "We want a pilot for 40 seats.").await;
    mail(&m, "two", "sam@contoso.com", "Can we meet at the conference?").await;
    let r = http
        .put(format!("{api}/settings/memex"))
        .json(&json!({"understanding": {"connection": "ollamas", "model": "mock-1"}, "calls_per_day": 100, "calls_at_once": 2}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let sorted = async |target: &str| -> Option<Triage> {
        let v: Value = post("memex/rpc", json!({"method": "memex.triage.list", "params": {"limit": 100}})).await.json().await.ok()?;
        serde_json::from_value::<Vec<Triage>>(v).ok()?.into_iter().find(|t| t.target == format!("thread:acct:{target}"))
    };
    // The first call holds mac's one slot: the second goes to mumuzz.
    eventually("one sorted on mumuzz while mac is busy", async || {
        (a.requests().len() == 1 && !b.requests().is_empty() && (sorted("one").await.is_some() || sorted("two").await.is_some()))
            .then_some(())
    })
    .await;
    gate.notify_one();
    for t in ["one", "two"] {
        let s = eventually("both sorted", async || sorted(t).await).await;
        assert_eq!(s.by, "model", "{s:?}");
    }

    // mac goes away; mail keeps coming. Its calls go to mumuzz, none misfiled.
    a.stop();
    let before = b.requests().len();
    mail(&m, "three", "lee@fabrikam.com", "Our invoice is attached.").await;
    mail(&m, "four", "kim@tailspin.com", "Lunch next week?").await;
    for t in ["three", "four"] {
        let s = eventually("sorted after mac went away", async || sorted(t).await).await;
        assert_eq!((s.category.as_str(), s.by.as_str()), ("fyi", "model"), "{s:?}");
    }
    assert!(b.requests().len() >= before + 2);
    for body in a.requests().into_iter().chain(b.requests()) {
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
    }

    // The activity panel says which member took each call.
    let calls = c6_llm::activity::calls();
    let through = |member: &str, state: &str| calls.iter().any(|c| c.via == pool.id && c.connection == member && c.state == state);
    assert!(through(&mac.id, "done") && through(&mumuzz.id, "done"), "both served");
    assert!(through(&mac.id, "failed"), "mac's refused call is shown");
    head.shutdown().await.unwrap();
}
