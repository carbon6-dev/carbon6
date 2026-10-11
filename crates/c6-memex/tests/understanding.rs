//! Understanding: a model you choose reads sessions, documents and papers,
//! newest first and within a daily budget, for summaries, topics,
//! commitments and claims (all marked inferred); you review same-name
//! people and merge them; a thing has a graph, a timeline and an
//! on-demand summary.

use std::sync::Arc;
use std::time::Duration;

use c6_api::{Commitment, MemexGraph, MemexReview, MemexStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn memex_node(head: &c6_head::Head, stop: &CancellationToken) -> (Arc<c6_memex::Memex>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(dir.path()).unwrap();
    let w = Worker::open(dir.path(), Identity::load(dir.path()).unwrap()).await.unwrap();
    let m = c6_memex::Memex::open(&dir.path().join("memex"), w.link.clone()).await.unwrap();
    m.spawn_jobs();
    w.set_extension(m.clone());
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
    (m, dir)
}

fn item(kind: &str, id: &str, title: &str, text: &str, at: i64, labels: &[&str]) -> c6_api::MemexItemIn {
    c6_api::MemexItemIn {
        kind: kind.into(),
        source: "test".into(),
        external_id: id.into(),
        title: title.into(),
        text: text.into(),
        created_at: Some(at),
        labels: labels.iter().map(|l| l.to_string()).collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn understanding_review_graph() {
    let mock = MockLlm::start().await;
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
    let stop = CancellationToken::new();
    let status =
        http.post(format!("{api}/connections")).json(&json!({"name": "m", "base_url": mock.base_url()})).send().await.unwrap().status();
    assert!(status.is_success());
    let (m, _dir) = memex_node(&head, &stop).await;
    let rpc = |method: &str, params: Value| {
        let (http, url, method) = (http.clone(), format!("{api}/memex/rpc"), method.to_owned());
        async move {
            let r = http.post(url).json(&json!({"method": method, "params": params})).send().await.unwrap();
            let ok = r.status().is_success();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            assert!(ok, "{method}: {v}");
            v
        }
    };

    // Three items; the paper is the newest, the document the oldest.
    let day = 86_400_000;
    let base = 1_700_000_000_000;
    m.store
        .put(&item("document", "d1", "Old notes on queues", "Notes about message queues and backpressure.", base, &[]), "test")
        .await
        .unwrap();
    m.store
        .put(
            &item(
                "session",
                "s1",
                "Fix the deploy pipeline",
                "We fixed the flaky deploy step. Follow-up: rotate the CI token by Friday.",
                base + day,
                &["project:atlas"],
            ),
            "test",
        )
        .await
        .unwrap();
    m.store
        .put(
            &item(
                "paper",
                "p1",
                "Backpressure in Stream Processors",
                "Jane Doe. We show that credit-based flow control halves tail latency.",
                base + 2 * day,
                &["person:Jane Doe", "project:atlas"],
            ),
            "test",
        )
        .await
        .unwrap();
    // Someone you write to, with the same name as the paper's author.
    c6_memex::google::person(&m.store, "jane@acme.com", "Jane Doe").await.unwrap();

    // The model's answers, newest item first: the paper, then the session.
    mock.push(Reply::Text(
        json!({"summary": "Credit-based flow control halves tail latency.", "topics": ["Backpressure", "Stream processing"],
               "claims": [{"text": "Credit-based flow control halves tail latency.", "type": "finding"},
                          {"text": "Evaluated on one cluster only.", "type": "limitation"}]})
        .to_string(),
    ));
    mock.push(Reply::Text(format!(
        "Here you go:\n```json\n{}\n```",
        json!({"summary": "Fixed the flaky deploy step.", "topics": ["CI/CD", "Backpressure"],
               "commitments": [{"who": "me", "what": "Rotate the CI token", "due": "2023-11-17"}]})
    )));
    // Two calls a day: the document waits for tomorrow.
    let put = http
        .put(format!("{api}/settings/memex"))
        .json(&json!({"understanding": {"connection": "m", "model": "reader-1"}, "calls_per_day": 2}))
        .send()
        .await
        .unwrap();
    assert!(put.status().is_success());
    // A model task needs a known connection.
    let bad =
        http.put(format!("{api}/settings/memex")).json(&json!({"vision": {"connection": "nope", "model": "v"}})).send().await.unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
    // (That one was refused; the good settings stand. Put them back anyway,
    // as a client would after an error.)
    http.put(format!("{api}/settings/memex"))
        .json(&json!({"understanding": {"connection": "m", "model": "reader-1"}, "calls_per_day": 2}))
        .send()
        .await
        .unwrap();

    let status: MemexStatus = eventually("two items read", async || {
        let s: MemexStatus = http.get(format!("{api}/memex/status")).send().await.ok()?.json().await.ok()?;
        (s.understood == 2).then_some(s)
    })
    .await;
    assert_eq!((status.calls_today, status.calls_per_day), (2, 2));
    // Nothing more within the budget.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(mock.requests().len(), 2);
    let first = &mock.requests()[0];
    assert_eq!(first["model"], "reader-1");
    assert!(first.to_string().contains("credit-based flow control"), "the paper first: {first}");

    // The paper: a summary, topics and claims, all inferred.
    let paper = rpc("memex.timeline", json!({"about": "person:Jane Doe"})).await;
    let paper_id = paper[0]["id"].as_str().unwrap().to_owned();
    let got = http.get(format!("{api}/memex/get")).query(&[("id", &paper_id)]).send().await.unwrap().json::<Value>().await.unwrap();
    let about: Vec<String> = got["item"]["about"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect();
    assert!(about.contains(&"Backpressure".to_owned()) && about.contains(&"Stream processing".to_owned()), "{about:?}");
    let claims: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT l.rel, l.basis, l.made_by FROM links l JOIN things t ON t.id = l.dst_thing WHERE t.kind = 'claim' ORDER BY l.rel",
    )
    .fetch_all(&m.store.pool)
    .await
    .unwrap();
    assert_eq!(
        claims,
        vec![
            ("claims".to_owned(), "inferred".to_owned(), "understand-v1@reader-1".to_owned()),
            ("has_limitation".to_owned(), "inferred".to_owned(), "understand-v1@reader-1".to_owned())
        ]
    );
    let summary: String =
        sqlx::query_scalar("SELECT summary FROM items WHERE id = ?").bind(&paper_id).fetch_one(&m.store.pool).await.unwrap();
    assert_eq!(summary, "Credit-based flow control halves tail latency.");

    // The session: a commitment (from an answer wrapped in prose).
    let open: Vec<Commitment> = serde_json::from_value(rpc("memex.commitments", json!({})).await).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(
        (open[0].who.as_str(), open[0].what.as_str(), open[0].item_title.as_str()),
        ("me", "Rotate the CI token", "Fix the deploy pipeline")
    );
    assert!(open[0].due.is_some());
    rpc("memex.commitment", json!({"id": open[0].id, "status": "done"})).await;
    assert!(rpc("memex.commitments", json!({})).await.as_array().unwrap().is_empty());
    assert_eq!(rpc("memex.commitments", json!({"status": "done"})).await.as_array().unwrap().len(), 1);

    // The graph: the project's neighbours, with the shared topic between
    // the paper and the session.
    let g: MemexGraph = serde_json::from_value(rpc("memex.graph", json!({"id": "project:atlas"})).await).unwrap();
    assert_eq!(g.center.name, "atlas");
    let backpressure = g.nodes.iter().find(|n| n.thing.name == "Backpressure").expect("a topic in the graph");
    assert_eq!(backpressure.shared, 2);
    let months = rpc("memex.months", json!({"id": "project:atlas"})).await;
    assert_eq!(months.as_array().unwrap().iter().map(|r| r["n"].as_i64().unwrap()).sum::<i64>(), 2);

    // Review: the two Jane Does; merging moves the paper to the address.
    let review: Vec<MemexReview> = serde_json::from_value(rpc("memex.review", json!({})).await).unwrap();
    assert_eq!(review.len(), 1, "{review:?}");
    assert_eq!(review[0].kind, "merge");
    rpc("memex.review_decide", json!({"id": review[0].id, "accept": true})).await;
    assert!(rpc("memex.review", json!({})).await.as_array().unwrap().is_empty());
    let jane = rpc("memex.timeline", json!({"about": "person:jane@acme.com"})).await;
    assert_eq!(jane[0]["id"], paper_id.as_str());
    // The old key still finds her.
    let t = http.get(format!("{api}/memex/get")).query(&[("id", "person:Jane Doe")]).send().await.unwrap().json::<Value>().await.unwrap();
    assert_eq!(t["thing"]["thing"]["label"], "person:jane@acme.com");

    // Summarize what I know (on demand, beyond the day's budget), saved as
    // a note marked inferred.
    mock.push(Reply::Text(json!({"summary": "Jane Doe wrote a paper on backpressure [p]."}).to_string()));
    let s = rpc("memex.summarize", json!({"id": "person:jane@acme.com"})).await;
    assert_eq!(s["summary"], "Jane Doe wrote a paper on backpressure [p].");
    let note: (String, String) =
        sqlx::query_as("SELECT kind, meta FROM items WHERE source LIKE 'pipeline:summary%'").fetch_one(&m.store.pool).await.unwrap();
    assert_eq!(note.0, "note");
    assert!(note.1.contains("\"inferred\":true"));

    // A new model reads everything again (budget allowing).
    mock.push(Reply::Text(json!({"summary": "Queues.", "topics": ["Queues"]}).to_string()));
    http.put(format!("{api}/settings/memex"))
        .json(&json!({"understanding": {"connection": "m", "model": "reader-2"}, "calls_per_day": 3}))
        .send()
        .await
        .unwrap();
    m.set_tasks(c6_memex::Tasks {
        understanding: m.tasks().understanding.map(|t| c6_memex::llm::Task { model: "reader-2".into(), ..t }),
        per_day: 3,
        ..Default::default()
    });
    eventually("one more read with the new model", async || {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM enrich WHERE version = 'understand-v1@reader-2'")
            .fetch_one(&m.store.pool)
            .await
            .ok()?;
        (n == 1).then_some(())
    })
    .await;
    // The newest item (the paper) is read first again; the old model's
    // topics on it give way when it is.
    let made: Vec<String> = sqlx::query_scalar("SELECT DISTINCT made_by FROM links WHERE src_item = ? AND basis = 'inferred'")
        .bind(&paper_id)
        .fetch_all(&m.store.pool)
        .await
        .unwrap();
    assert_eq!(made, vec!["understand-v1@reader-2".to_owned()]);

    // An email thread (two messages) is read as one, newest first.
    for (n, (gid, text)) in [("g1", "Can you send the Q3 numbers?"), ("g2", "Sure, I will send them by Monday. Bob")].iter().enumerate() {
        let id = m.store.put(&item("email", gid, "Q3 numbers", text, base + (10 + n as i64) * day, &[]), "gmail").await.unwrap();
        sqlx::query("INSERT INTO mail (item_id, account, gmail_id, thread_id, date, from_addr, subject, blob) VALUES (?, 'a1', ?, 't1', ?, ?, 'Q3 numbers', '')")
            .bind(&id)
            .bind(gid)
            .bind(base + (10 + n as i64) * day)
            .bind(if n == 0 { "me@x.com" } else { "bob@x.com" })
            .execute(&m.store.pool)
            .await
            .unwrap();
    }
    mock.push(Reply::Text(
        json!({"summary": "Bob will send the Q3 numbers.", "topics": ["Q3"], "commitments": [{"who": "bob@x.com", "what": "Send the Q3 numbers", "due": null}]})
            .to_string(),
    ));
    m.set_tasks(c6_memex::Tasks { per_day: 4, ..m.tasks() });
    let c: Vec<Commitment> = eventually("the thread's commitment", async || {
        let v: Vec<Commitment> = serde_json::from_value(rpc("memex.commitments", json!({})).await).ok()?;
        (!v.is_empty()).then_some(v)
    })
    .await;
    assert_eq!((c[0].who.as_str(), c[0].what.as_str(), c[0].due), ("bob@x.com", "Send the Q3 numbers", None));
    let asked = mock.requests().last().unwrap().to_string();
    assert!(asked.contains("Can you send the Q3 numbers?") && asked.contains("by Monday"), "both messages: {asked}");
    let target: String =
        sqlx::query_scalar("SELECT target FROM enrich WHERE target LIKE 'thread:%'").fetch_one(&m.store.pool).await.unwrap();
    assert_eq!(target, "thread:a1:t1");
    stop.cancel();
}
