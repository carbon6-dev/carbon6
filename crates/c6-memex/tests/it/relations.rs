//! Relationships (docs/relationships.md §4): the reading walk goes back
//! through the mail newest first, skipping automated senders; each thread
//! it reads is news for its people's cards, folded in afterwards, so a card
//! grows to cover the whole history.

use c6_head::HeadConfig;
use c6_memex::mail::{Parsed, Stored};
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use serde_json::{Value, json};

const DAY: i64 = 86_400_000;

#[allow(clippy::too_many_arguments)]
async fn mail(m: &c6_memex::Memex, id: &str, thread: &str, from: (&str, &str), to: &str, subject: &str, text: &str, sent: bool, ago: i64) {
    let parsed = Parsed {
        message_id: format!("<{id}@test>"),
        from: (from.0.into(), from.1.into()),
        to: vec![(to.into(), String::new())],
        subject: subject.into(),
        text: text.into(),
        ..Default::default()
    };
    let at = Stored {
        account: "acct",
        external_id: id,
        thread_id: thread,
        source: "test-mail",
        rfc_id: &parsed.message_id,
        origin: "mirrored",
        owner: "me@mine.io",
        message: id,
    };
    let labels: Vec<String> = if sent { vec!["SENT".into()] } else { vec!["INBOX".into()] };
    c6_memex::mail::store(m, &parsed, "", &at, &labels, c6_core::now_ms() - ago).await.unwrap();
}

#[tokio::test]
async fn the_walk_grows_cards_back_through_time() {
    crate::extract_here();
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
    let r = http.post(format!("{api}/connections")).json(&json!({"name": "m", "base_url": mock.base_url()})).send().await.unwrap();
    assert!(r.status().is_success());
    let m = c6_head::knowledge::get(&head.state).expect("the head's knowledge store");
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
    let budget = |n: u32| {
        let (http, url) = (http.clone(), format!("{api}/settings/memex"));
        async move {
            let r = http
                .put(url)
                .json(&json!({"understanding": {"connection": "m", "model": "reader-1"}, "calls_per_day": n}))
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success());
        }
    };
    let me = ("me@mine.io", "Me");
    // Two years with Dana at Globex, newest first: a renewal, a support
    // case, the first deal; and a no-reply receipt in between.
    mail(&m, "a2", "renewal", ("dana@globex.com", "Dana"), "me@mine.io", "Renewal", "Shall we renew for another year?", false, 2 * DAY)
        .await;
    mail(&m, "a1", "renewal", me, "dana@globex.com", "Re: Renewal", "Let's talk next week.", true, DAY).await;
    mail(&m, "r1", "receipt", ("no-reply@billing.example", "Billing"), "me@mine.io", "Receipt", "Your receipt.", false, 100 * DAY).await;
    mail(&m, "b2", "support", ("dana@globex.com", "Dana"), "me@mine.io", "Outage", "Our cluster is down, can you help?", false, 200 * DAY)
        .await;
    mail(&m, "b1", "support", me, "dana@globex.com", "Re: Outage", "On it.", true, 199 * DAY).await;
    mail(&m, "c1", "deal", me, "dana@globex.com", "Proposal", "Here's our proposal for Globex.", true, 700 * DAY).await;
    mail(&m, "c2", "deal", ("dana@globex.com", "Dana"), "me@mine.io", "Re: Proposal", "Signed.", false, 699 * DAY).await;

    // Reading, newest first; each read thread is news for Dana's card and
    // Globex's, folded in right after (the cards before the next read).
    for (about, summary) in [
        ("renew for another year", "Dana asks to renew."),
        ("cluster is down", "Globex had an outage."),
        ("our proposal", "Globex signed the first deal."),
    ] {
        mock.push_about_all(
            &["This is an email thread", about],
            Reply::Text(json!({"summary": summary, "topics": [], "commitments": []}).to_string()),
        );
    }
    // Each card folds in the thread just read (told apart by its summary).
    for (folded, story) in [
        ("Dana asks to renew.", "Renewing."),
        ("Globex had an outage.", "An outage, then renewing."),
        ("Globex signed the first deal.", "Signed two years ago; an outage; renewing."),
    ] {
        let card = json!({"relationship": "customer", "role": "Globex", "story": story, "open": "", "how": "Answers within a day"});
        mock.push_about_all(&["Write the owner's card for Dana", folded], Reply::Text(card.to_string()));
        mock.push_about_all(&["Write the owner's card for globex.com", folded], Reply::Text(card.to_string()));
    }
    budget(9).await;
    let story = async || rpc("memex.relations.cards", json!({"address": "dana@globex.com"})).await;
    let v = eventually("both cards, from all three threads", async || {
        let v = story().await;
        let all = v.as_array()?;
        (all.len() == 2 && all.iter().all(|c| c["threads"] == 3)).then_some(v)
    })
    .await;
    assert_eq!(v[0]["card"]["story"], "Signed two years ago; an outage; renewing.", "{v}");
    // The receipt wasn't read; each card took in each thread once.
    let asked: Vec<String> = mock.requests().iter().map(|r| r["messages"].to_string()).collect();
    assert!(!asked.iter().any(|a| a.contains("Your receipt")), "automated mail isn't read");
    assert_eq!(asked.len(), 9, "three reads, three times two cards");
    for body in mock.requests() {
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
    }
    let last_card = asked.iter().rfind(|a| a.contains("Write the owner's card for Dana")).unwrap();
    assert!(last_card.contains("The card so far") && last_card.contains("Globex signed the first deal."), "{last_card}");
    // A card written for a newsletter before the rule (its news too) is
    // removed, once; Dana's stays.
    for i in 0..3 {
        mail(
            &m,
            &format!("n{i}"),
            &format!("news{i}"),
            ("hi@news.example", "News"),
            "me@mine.io",
            "Issue",
            "This week.",
            false,
            (300 + i) * DAY,
        )
        .await;
    }
    sqlx::query("UPDATE mail SET list = 1 WHERE from_addr = 'hi@news.example'").execute(&m.store.pool).await.unwrap();
    sqlx::query(
        "INSERT INTO cards (key, name, card, threads, model, updated_at) VALUES ('person:hi@news.example', 'News', '{}', 3, 'm', 0)",
    )
    .execute(&m.store.pool)
    .await
    .unwrap();
    // Then the clean-up may run again: the reading loop runs it (or this
    // call does, first).
    sqlx::query("DELETE FROM memex_once").execute(&m.store.pool).await.unwrap();
    let _ = c6_memex::cards::prune(&m).await.unwrap();
    eventually("the newsletter's card gone", async || {
        c6_memex::cards::get(&m, "person:hi@news.example").await.ok()?.is_none().then_some(())
    })
    .await;
    assert!(c6_memex::cards::get(&m, "person:dana@globex.com").await.unwrap().is_some());
    assert_eq!(c6_memex::cards::prune(&m).await.unwrap(), 0, "once");
    let progress = rpc("memex.relations.progress", json!({})).await;
    assert_eq!(
        (progress["cards"].as_i64(), progress["threads_read"].as_i64(), progress["cards_waiting"].as_i64()),
        (Some(2), Some(3), Some(0)),
        "{progress}"
    );
}

#[tokio::test]
async fn lanes_side_by_side_never_take_the_same_thread() {
    crate::extract_here();
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
    let r = http.post(format!("{api}/connections")).json(&json!({"name": "m", "base_url": mock.base_url()})).send().await.unwrap();
    assert!(r.status().is_success());
    let m = c6_head::knowledge::get(&head.state).expect("the head's knowledge store");
    // Old mail from six strangers (nothing to sort, no cards): six threads.
    for i in 0..6 {
        let from = format!("p{i}@firm{i}.com");
        mail(&m, &format!("x{i}"), &format!("t{i}"), (&from, "P"), "me@mine.io", &format!("Topic {i}"), "Hello.", false, (30 + i) * DAY)
            .await;
        mock.push_about("This is an email thread", Reply::Text(json!({"summary": "A note.", "topics": [], "commitments": []}).to_string()));
    }
    let r = http
        .put(format!("{api}/settings/memex"))
        .json(&json!({"understanding": {"connection": "m", "model": "reader-1"}, "calls_per_day": 6, "calls_at_once": 4}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let read = async || {
        let v: Value = http
            .post(format!("{api}/memex/rpc"))
            .json(&json!({"method": "memex.relations.progress", "params": {}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v["threads_read"].as_i64()
    };
    eventually("all six read", async || (read().await == Some(6)).then_some(())).await;
    let asked: Vec<String> = mock.requests().iter().map(|r| r["messages"].to_string()).collect();
    assert_eq!(asked.len(), 6, "each thread once");
    for i in 0..6 {
        assert_eq!(asked.iter().filter(|a| a.contains(&format!("Topic {i} "))).count(), 1, "Topic {i}");
    }
    let r = http.put(format!("{api}/settings/memex")).json(&json!({"calls_at_once": 64})).send().await.unwrap();
    assert!(r.status().is_success(), "64 lanes accepted");
    let r = http.put(format!("{api}/settings/memex")).json(&json!({"calls_at_once": 65})).send().await.unwrap();
    assert_eq!(r.status(), 400, "at most 64");
}

#[tokio::test]
async fn eleven_reads_run_side_by_side_past_long_threads() {
    crate::extract_here();
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
    let r = http.post(format!("{api}/connections")).json(&json!({"name": "m", "base_url": mock.base_url()})).send().await.unwrap();
    assert!(r.status().is_success());
    let m = c6_head::knowledge::get(&head.state).unwrap();
    let mut gates = Vec::new();
    // Forty message rows cover only five of these threads. No recent mail or cards.
    for i in 0..11 {
        let from = format!("p{i}@firm{i}.com");
        for j in 0..8 {
            mail(
                &m,
                &format!("x{i}-{j}"),
                &format!("t{i}"),
                (&from, "P"),
                "me@mine.io",
                &format!("Topic {i}"),
                "Hello.",
                false,
                (30 + i) * DAY + j * 1000,
            )
            .await;
        }
        gates.push(mock.push_gated(Reply::Text(json!({"summary": "A note.", "topics": [], "commitments": []}).to_string())));
    }
    let r = http
        .put(format!("{api}/settings/memex"))
        .json(&json!({"understanding": {"connection": "m", "model": "reader-1"}, "calls_per_day": 11, "calls_at_once": 12}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let asked = eventually("eleven reading calls held at once", async || {
        m.wake.notify_waiters();
        let requests = mock.requests();
        (requests.len() >= 11).then_some(requests)
    })
    .await;
    for gate in gates {
        gate.notify_one();
    }
    assert_eq!(asked.len(), 11);
    let asked: Vec<String> = asked.iter().map(|r| r["messages"].to_string()).collect();
    assert!(asked.iter().all(|a| a.contains("This is an email thread")));
    for i in 0..11 {
        assert_eq!(asked.iter().filter(|a| a.contains(&format!("Topic {i} "))).count(), 1, "Topic {i}");
    }
    eventually("all eleven reads recorded", async || {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM enrich WHERE target LIKE 'thread:%'").fetch_one(&m.store.pool).await.unwrap();
        (n == 11).then_some(())
    })
    .await;
}

#[tokio::test]
async fn failed_reads_retry_after_unread_work_and_stop_after_three_tries() {
    crate::extract_here();
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
    let r = http.post(format!("{api}/connections")).json(&json!({"name": "m", "base_url": mock.base_url()})).send().await.unwrap();
    assert!(r.status().is_success());
    let m = c6_head::knowledge::get(&head.state).unwrap();
    let budget = async |n| {
        let r = http
            .put(format!("{api}/settings/memex"))
            .json(&json!({"understanding": {"connection": "m", "model": "reader-1"}, "calls_per_day": n}))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
        eventually("the reading budget loaded", async || (m.tasks().per_day == n).then_some(())).await;
    };
    let status = async || -> c6_api::MemexStatus { http.get(format!("{api}/memex/status")).send().await.unwrap().json().await.unwrap() };
    let progress = async || m.handle_method("memex.relations.progress", json!({})).await.unwrap();
    for (i, thread) in ["bad-thread", "recover-thread", "older-thread"].into_iter().enumerate() {
        let from = format!("p{i}@firm{i}.com");
        mail(&m, thread, thread, (&from, "P"), "me@mine.io", thread, "Notes.", false, (30 + i as i64) * DAY).await;
    }
    let document = m
        .store
        .put(
            &c6_api::MemexItemIn {
                kind: "document".into(),
                source: "test".into(),
                external_id: "retry-document".into(),
                title: "retry-document".into(),
                text: "Notes.".into(),
                created_at: Some(c6_core::now_ms() - 33 * DAY),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let broken = || Reply::Text("{broken}".into());
    let answer = |summary| Reply::Text(json!({"summary": summary, "topics": ["Recovery"], "commitments": []}).to_string());
    mock.push(broken());
    let gate = mock.push_gated(broken());
    mock.push(answer("Older notes."));
    mock.push(broken());
    mock.push(broken());
    mock.push(broken());
    mock.push(answer("Recovered thread."));
    mock.push(answer("Recovered document."));
    budget(8).await;
    eventually("the second unread thread started", async || (mock.requests().len() == 2).then_some(())).await;
    let pending = status().await;
    let pending_walk = progress().await;
    gate.notify_one();
    assert_eq!((pending.understood, pending.understanding_failed), (0, 0));
    assert_eq!((pending_walk["threads_read"].as_i64(), pending_walk["threads_failed"].as_i64()), (Some(0), Some(0)));

    eventually("two threads and a document read, one thread exhausted", async || {
        let s = status().await;
        (s.understood == 3 && s.understanding_failed == 1 && s.calls_today == 8).then_some(())
    })
    .await;
    let walk = progress().await;
    assert_eq!((walk["threads_read"].as_i64(), walk["threads_failed"].as_i64()), (Some(2), Some(1)));
    let asked = mock.requests();
    for (request, title) in asked.iter().zip([
        "bad-thread",
        "recover-thread",
        "older-thread",
        "retry-document",
        "bad-thread",
        "bad-thread",
        "recover-thread",
        "retry-document",
    ]) {
        assert!(request["messages"].to_string().contains(title), "expected {title}: {request}");
    }
    assert_eq!(asked.len(), 8);
    assert_eq!(asked.iter().filter(|r| r["messages"].to_string().contains("bad-thread")).count(), 3);
    let recovered: (String, String, i64, Option<String>) = sqlx::query_as(
        "SELECT i.id, i.summary, e.tries, e.error FROM items i JOIN mail m ON m.item_id = i.id \
         JOIN enrich e ON e.target = 'thread:' || m.account || ':' || m.thread_id WHERE m.thread_id = 'recover-thread'",
    )
    .fetch_one(&m.store.pool)
    .await
    .unwrap();
    assert_eq!((recovered.1.as_str(), recovered.2, recovered.3), ("Recovered thread.", 0, None));
    let topic: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM links l JOIN things t ON t.id = l.dst_thing WHERE l.src_item = ? AND t.kind = 'topic' AND t.name = 'Recovery')"
    ).bind(&recovered.0).fetch_one(&m.store.pool).await.unwrap();
    assert!(topic);
    let doc: (String, i64, Option<String>) =
        sqlx::query_as("SELECT i.summary, e.tries, e.error FROM items i JOIN enrich e ON e.target = 'item:' || i.id WHERE i.id = ?")
            .bind(&document)
            .fetch_one(&m.store.pool)
            .await
            .unwrap();
    assert_eq!(doc, ("Recovered document.".into(), 0, None));

    // Budget remains, but waking and taking another step finds no fourth try.
    budget(9).await;
    m.wake.notify_waiters();
    assert!(!c6_memex::understand::step(&m).await.unwrap());
    assert_eq!(mock.requests().len(), 8);
    mock.push(broken());
    mail(&m, "new-mail", "bad-thread", ("p0@firm0.com", "P"), "me@mine.io", "bad-thread", "More notes.", false, 29 * DAY).await;
    m.wake.notify_waiters();
    eventually("new mail starts the failure count over", async || {
        let tries: i64 =
            sqlx::query_scalar("SELECT tries FROM enrich WHERE target = 'thread:acct:bad-thread'").fetch_one(&m.store.pool).await.unwrap();
        (tries == 1 && status().await.calls_today == 9).then_some(())
    })
    .await;
    assert_eq!(mock.requests().len(), 9);
    let s = status().await;
    assert_eq!((s.understood, s.understanding_failed), (3, 0));
    assert_eq!(progress().await["threads_failed"], 0);
}

#[tokio::test]
async fn mail_arriving_during_a_third_read_starts_over() {
    crate::extract_here();
    let mock = MockLlm::start().await;
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
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    http.post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let m = c6_head::knowledge::get(&head.state).unwrap();
    let budget = async |n| {
        http.put(format!("{api}/settings/memex"))
            .json(&json!({"understanding": {"connection": "m", "model": "reader-1"}, "calls_per_day": n}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    };
    mail(&m, "old", "notes", ("pat@firm.com", "Pat"), "me@mine.io", "Notes", "Original notes.", false, 30 * DAY).await;
    sqlx::query("UPDATE items SET added_at = 10").execute(&m.store.pool).await.unwrap();
    sqlx::query("INSERT INTO enrich (target, version, done_at, error, tries) VALUES ('thread:acct:notes', 'understand-v1@reader-1', 10, 'no JSON', 2)")
        .execute(&m.store.pool).await.unwrap();
    let gate = mock.push_gated(Reply::Text("{broken}".into()));
    mock.push(Reply::Text("{broken again}".into()));
    budget(1).await;
    eventually("the third try held at the model", async || (mock.requests().len() == 1).then_some(())).await;
    mail(&m, "new", "notes", ("pat@firm.com", "Pat"), "me@mine.io", "Notes", "Additional notes.", false, 29 * DAY).await;
    let newest: i64 = sqlx::query_scalar("SELECT MAX(added_at) FROM items").fetch_one(&m.store.pool).await.unwrap();
    gate.notify_one();
    let recorded = async || -> (i64, i64) {
        sqlx::query_as("SELECT done_at, tries FROM enrich WHERE target = 'thread:acct:notes'").fetch_one(&m.store.pool).await.unwrap()
    };
    let exhausted = eventually("the held read recorded its third failure", async || {
        let row = recorded().await;
        (row.1 == 3).then_some(row)
    })
    .await;
    assert_eq!(exhausted.0, 10, "only the original message was read");
    assert!(newest > exhausted.0);
    budget(2).await;
    eventually("new mail starts the failure count over", async || (recorded().await == (newest, 1)).then_some(())).await;
    let asked = mock.requests();
    assert_eq!(asked.len(), 2);
    assert!(!asked[0]["messages"].to_string().contains("Additional notes."));
    assert!(asked[1]["messages"].to_string().contains("Original notes."));
    assert!(asked[1]["messages"].to_string().contains("Additional notes."));
    head.shutdown().await.unwrap();
}
