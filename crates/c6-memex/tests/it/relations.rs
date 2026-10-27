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
    let r = http.put(format!("{api}/settings/memex")).json(&json!({"calls_at_once": 9})).send().await.unwrap();
    assert_eq!(r.status(), 400, "at most 8");
}
