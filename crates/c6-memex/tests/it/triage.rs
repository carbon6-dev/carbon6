//! Triage (docs/brief.md §3–4): recent mail threads and chat conversations
//! sorted for the brief. Strangers' mail from a sales-sequence tool, or a
//! sequence's follow-up, is a pitch with no model; people you know are
//! sorted by the model, strangers by its pitch prompt; lists and your own
//! last word by rule. Your marks settle a sender, your corrections become
//! the model's examples, and a realm's memex reads with its own prompts.
//! What needs you becomes requests: one per ask, joined across mail and
//! chat for the same person, closed when you answer; each waits with a
//! draft reply, drafted again when its conversation moves on. An urgent
//! one tells you at once and opens as an event for triggers; the brief
//! comes at its times, and the close of day.

use c6_api::{MailThread, Request, Triage};
use c6_head::HeadConfig;
use c6_memex::mail::{Parsed, Stored};
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use serde_json::{Value, json};

const HOUR: i64 = 3_600_000;

/// A message in a thread, kept as a mail app's sync keeps it.
#[allow(clippy::too_many_arguments)]
async fn mail(
    m: &c6_memex::Memex,
    id: &str,
    thread: &str,
    from: (&str, &str),
    to: &str,
    subject: &str,
    text: &str,
    html: Option<&str>,
    labels: &[&str],
    ago: i64,
) {
    let parsed = Parsed {
        message_id: format!("<{id}@test>"),
        from: (from.0.into(), from.1.into()),
        to: vec![(to.into(), String::new())],
        subject: subject.into(),
        text: text.into(),
        html: html.map(|h| h.as_bytes().to_vec()),
        list: from.0.starts_with("news@"),
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
    let labels: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
    c6_memex::mail::store(m, &parsed, "", &at, &labels, c6_core::now_ms() - ago).await.unwrap();
}

#[tokio::test]
async fn mail_and_chat_sorted_for_the_brief() {
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
    assert!(
        http.post(format!("{api}/connections"))
            .json(&json!({"name": "m", "base_url": mock.base_url()}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
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
    // Model calls allowed today: each phase raises it by what it needs, so
    // the reading that follows triage never takes a reply meant for it.
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
    let sorted = |target: &str| {
        let target = target.to_owned();
        async move {
            let all: Vec<Triage> = serde_json::from_value(rpc("memex.triage.list", json!({"limit": 100})).await).unwrap();
            all.into_iter().find(|t| t.target == target)
        }
    };
    let me = ("me@mine.io", "Me");

    // You wrote to Ana at Acme once; she asks for something now.
    mail(&m, "s0", "t0", me, "ana@acme.com", "Contract", "Here's the draft.", None, &["SENT"], 300 * HOUR).await;
    mail(
        &m,
        "a1",
        "t1",
        ("ana@acme.com", "Ana"),
        "me@mine.io",
        "Redlines",
        "Can you approve the redlines by Thursday?",
        None,
        &["INBOX", "UNREAD"],
        HOUR,
    )
    .await;
    // Bo, at Acme too (you never wrote to him, but to his company).
    mail(&m, "b1", "t2", ("bo@acme.com", "Bo"), "me@mine.io", "Deploy", "FYI: the deploy is done.", None, &["INBOX"], 3 * HOUR).await;
    // Strangers: a sequence tool's tracking link; a follow-up in a sequence;
    // a plain pitch; a real first contact.
    mail(
        &m,
        "c1",
        "t3",
        ("sam@growfast.io", "Sam"),
        "me@mine.io",
        "Pipeline",
        "Hi! Loved your post.",
        Some(r#"<a href="https://links.apollo.io/click/abc">book a time</a>"#),
        &["INBOX"],
        6 * HOUR,
    )
    .await;
    mail(
        &m,
        "d1",
        "t4",
        ("kim@leadsy.co", "Kim"),
        "me@mine.io",
        "Re: quick question",
        "Just bumping this to the top of your inbox.",
        None,
        &["INBOX"],
        7 * HOUR,
    )
    .await;
    mail(
        &m,
        "e1",
        "t5",
        ("lee@sdrhire.com", "Lee"),
        "me@mine.io",
        "Scale outbound",
        "We help teams like yours scale SDRs. 15 min?",
        None,
        &["INBOX"],
        4 * HOUR,
    )
    .await;
    mail(
        &m,
        "f1",
        "t6",
        ("pat@northwind.com", "Pat"),
        "me@mine.io",
        "Pilot",
        "We use your scheduler and want a pilot for 40 seats.",
        None,
        &["INBOX"],
        5 * HOUR,
    )
    .await;
    // A newsletter; and a thread you answered last.
    mail(&m, "g1", "t7", ("news@weekly.dev", "Weekly"), "me@mine.io", "Issue 12", "This week in Rust.", None, &["INBOX"], 8 * HOUR).await;
    mail(&m, "h1", "t8", ("ana@acme.com", "Ana"), "me@mine.io", "Lunch", "Lunch Friday?", None, &["INBOX"], 10 * HOUR).await;
    mail(&m, "h2", "t8", me, "ana@acme.com", "Re: Lunch", "Yes!", None, &["SENT"], 9 * HOUR).await;
    // Chat: a DM asking you something; a channel message that isn't to you.
    let chat = |id: &str, channel: &str, kind: &str, text: &str, ago: i64, mentions_me: bool| c6_api::MemexItemIn {
        kind: "chat".into(),
        source: "test-slack".into(),
        external_id: id.into(),
        title: format!("{channel} · Raj"),
        text: format!("Raj: {text}"),
        created_at: Some(c6_core::now_ms() - ago),
        meta: json!({"service": "slack", "workspace": "T1", "channel": channel, "channel_name": channel, "channel_kind": kind,
                     "ts": format!("{}.0001", 1000 + ago / 1000), "user": "U2", "user_name": "Raj", "from_me": false, "mentions_me": mentions_me}),
        ..Default::default()
    };
    m.store.put(&chat("k1", "D9", "dm", "Can we move our 1:1 to Wednesday?", 2 * HOUR, false), "test-slack").await.unwrap();
    m.store.put(&chat("k2", "C9", "public", "Lunch is here.", 2 * HOUR, false), "test-slack").await.unwrap();

    // With no model, what rules settle is sorted at once.
    eventually("rules first", async || (sorted("thread:acct:t8").await?.category == "yours").then_some(())).await;
    let pitch = sorted("thread:acct:t3").await.unwrap();
    assert_eq!((pitch.category.as_str(), pitch.by.as_str()), ("pitch", "signals"), "{pitch:?}");
    assert!(pitch.reason.contains("apollo.io"), "{pitch:?}");
    let follow = sorted("thread:acct:t4").await.unwrap();
    assert_eq!((follow.category.as_str(), follow.by.as_str()), ("pitch", "sequence"), "{follow:?}");
    assert_eq!(sorted("thread:acct:t7").await.unwrap().category, "newsletter");
    assert!(sorted("thread:acct:t1").await.is_none(), "waits for a model");
    let status = rpc("memex.triage.status", json!({})).await;
    assert_eq!((status["model"].as_bool(), status["waiting"].as_u64()), (Some(false), Some(5)), "the brief says why: {status}");

    // Then the model, newest first: Ana (known), Raj's DM, Bo (known by his
    // company), Lee and Pat (strangers: the pitch prompt).
    for r in [
        json!({"category": "needs_you", "reason": "Ana needs your approval by Thursday", "asks": "Approve the redlines", "due": "2031-01-09"}),
        json!({"category": "needs_you", "reason": "Raj wants to move your 1:1", "asks": "Move the 1:1 to Wednesday", "due": null}),
        json!({"category": "fyi", "reason": "The deploy is done", "asks": ""}),
        json!({"category": "pitch", "reason": "Sells SDR outsourcing"}),
        json!({"category": "first_contact", "reason": "A customer asks for a pilot", "asks": "A pilot for 40 seats"}),
    ] {
        mock.push(Reply::Text(r.to_string()));
    }
    budget(5).await;
    eventually("the model's sorting", async || (sorted("thread:acct:t6").await?.category == "first_contact").then_some(())).await;
    let want = [
        ("thread:acct:t1", "needs_you"),
        ("chat:T1:D9", "needs_you"),
        ("thread:acct:t2", "fyi"),
        ("thread:acct:t5", "pitch"),
        ("thread:acct:t6", "first_contact"),
    ];
    for (t, c) in want {
        let s = sorted(t).await.unwrap_or_else(|| panic!("{t} sorted"));
        assert_eq!((s.category.as_str(), s.by.as_str()), (c, "model"), "{s:?}");
    }
    assert!(sorted("chat:T1:C9:8200.0001").await.is_none(), "a channel message not to you isn't sorted");
    let asked: Vec<String> = mock.requests().iter().map(|r| r["messages"].to_string()).collect();
    assert_eq!(asked.len(), 5, "rules needed no model");
    assert!(asked[0].contains("Sort this conversation") && asked[0].contains("approve the redlines"), "{}", asked[0]);
    assert!(asked[3].contains("never corresponded") && asked[3].contains("scale SDRs"), "{}", asked[3]);
    assert_eq!(sorted("thread:acct:t1").await.unwrap().reason, "Ana needs your approval by Thursday");

    // What needs you, as requests: the one due soonest first.
    let open = |status: &'static str| async move {
        serde_json::from_value::<Vec<Request>>(rpc("memex.requests.list", json!({"status": status})).await).unwrap()
    };
    let reqs = open("open").await;
    let titles: Vec<&str> = reqs.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles, ["Approve the redlines", "A pilot for 40 seats", "Move the 1:1 to Wednesday"], "{reqs:?}");
    assert_eq!((reqs[0].person.as_str(), reqs[0].sources[0].target.as_str()), ("ana@acme.com", "thread:acct:t1"));
    assert!(reqs[0].due.is_some());
    assert_eq!((reqs[1].category.as_str(), reqs[2].person.as_str()), ("first_contact", "U2"));
    let (ana, raj) = (reqs[0].id, reqs[2].id);

    // The mail list's views and threads say how they're sorted.
    let pitches: Vec<MailThread> = serde_json::from_value(rpc("memex.mail.threads", json!({"view": "triage:pitch"})).await).unwrap();
    let ids: Vec<&str> = pitches.iter().map(|t| t.thread_id.as_str()).collect();
    assert_eq!(ids, ["t5", "t3", "t4"]);
    assert_eq!(pitches[0].triage.as_ref().unwrap().reason, "Sells SDR outsourcing");
    let counts = rpc("memex.triage.counts", json!({})).await;
    assert_eq!((counts["pitch"].as_i64(), counts["needs_you"].as_i64()), (Some(3), Some(2)), "{counts}");

    // You mark Lee a pitch: his next mail is one, with no model.
    rpc("memex.triage.mark", json!({"target": "thread:acct:t5", "mark": "pitch"})).await;
    mail(
        &m,
        "e2",
        "t9",
        ("lee@sdrhire.com", "Lee"),
        "me@mine.io",
        "One more idea",
        "Have you thought about outbound?",
        None,
        &["INBOX"],
        HOUR / 2,
    )
    .await;
    let again = eventually("Lee's mail, settled", async || sorted("thread:acct:t9").await).await;
    assert_eq!((again.category.as_str(), again.by.as_str()), ("pitch", "mark"));
    // Sam is someone you know after all: sorted again, as such.
    mock.push(Reply::Text(json!({"category": "fyi", "reason": "Sam shares a post"}).to_string()));
    rpc("memex.triage.mark", json!({"target": "thread:acct:t3", "mark": "known"})).await;
    budget(6).await;
    eventually("Sam, sorted again", async || (sorted("thread:acct:t3").await?.category == "fyi").then_some(())).await;
    let marks = rpc("memex.triage.marks", json!({})).await;
    assert_eq!(marks.as_array().unwrap().len(), 2, "{marks}");

    // You correct Bo's: deploys need your sign-off. His next is sorted with
    // that as an example.
    let set =
        rpc("memex.triage.set", json!({"target": "thread:acct:t2", "category": "needs_you", "note": "deploys need my sign-off"})).await;
    assert_eq!((set["category"].as_str(), set["by"].as_str()), (Some("needs_you"), Some("you")), "{set}");
    mock.push(Reply::Text(json!({"category": "needs_you", "reason": "Bo's deploy needs your sign-off"}).to_string()));
    mail(&m, "b2", "t2", ("bo@acme.com", "Bo"), "me@mine.io", "Re: Deploy", "And the hotfix is out too.", None, &["INBOX"], HOUR / 3).await;
    budget(7).await;
    eventually("Bo's, sorted again", async || (sorted("thread:acct:t2").await?.reason == "Bo's deploy needs your sign-off").then_some(()))
        .await;
    let last = mock.requests().last().unwrap()["messages"].to_string();
    assert!(last.contains("deploys need my sign-off") && last.contains("hotfix"), "{last}");
    assert!(open("open").await.iter().any(|r| r.person == "bo@acme.com" && r.sources.len() == 1), "sorted by you: a request");

    // Ana asks again on Slack: the same request, now from both.
    let ana_chat = |id: &str, channel: &str, kind: &str, text: &str, ago: i64| {
        let mut c = chat(id, channel, kind, text, ago, false);
        c.meta["user"] = json!("UA");
        c.meta["user_name"] = json!("Ana");
        c.meta["user_email"] = json!("ana@acme.com");
        c
    };
    mock.push(Reply::Text(
        json!({"category": "needs_you", "reason": "Ana is waiting on the redlines", "asks": "Approve the redlines"}).to_string(),
    ));
    mock.push(Reply::Text(json!({"same": true, "sure": true}).to_string()));
    m.store.put(&ana_chat("k3", "D7", "dm", "Did you see my email about the redlines?", HOUR / 4), "test-slack").await.unwrap();
    budget(9).await;
    let joined = eventually("joined", async || {
        let r: Request = serde_json::from_value(rpc("memex.requests.get", json!({"id": ana})).await).ok()?;
        (r.sources.len() == 2).then_some(r)
    })
    .await;
    assert_eq!(joined.sources[1].target, "chat:T1:D7");
    // Another ask the model isn't sure about: two requests, marked as
    // possibly one; you merge them.
    mock.push(Reply::Text(
        json!({"category": "needs_you", "reason": "Ana wants a budget review", "asks": "Review the Q4 budget"}).to_string(),
    ));
    mock.push(Reply::Text(json!({"same": true, "sure": false}).to_string()));
    m.store.put(&ana_chat("k4", "G7", "group", "Also, can you review the Q4 budget?", HOUR / 5), "test-slack").await.unwrap();
    budget(11).await;
    let maybe = eventually("possibly the same", async || {
        open("open").await.into_iter().find(|r| r.title == "Review the Q4 budget" && r.maybe_same == [ana])
    })
    .await;
    let merged: Request = serde_json::from_value(rpc("memex.requests.merge", json!({"into": ana, "from": maybe.id})).await).unwrap();
    assert_eq!(merged.sources.len(), 3);
    assert!(open("open").await.iter().all(|r| r.id != maybe.id));

    // You answer Ana by mail: her request is done.
    mail(&m, "a2", "t1", me, "ana@acme.com", "Re: Redlines", "Approved.", None, &["SENT"], HOUR / 6).await;
    let done = eventually("answered", async || open("done").await.into_iter().find(|r| r.id == ana)).await;
    assert_eq!(done.closed_by.as_deref(), Some("replied"));
    // Raj's, snoozed for an hour, is out of the way; one whose time came is back.
    rpc("memex.requests.set", json!({"id": raj, "status": "snoozed", "until": c6_core::now_ms() + HOUR})).await;
    assert!(open("open").await.iter().all(|r| r.id != raj));
    rpc("memex.requests.set", json!({"id": raj, "status": "snoozed", "until": c6_core::now_ms() - 1})).await;
    assert!(open("open").await.iter().any(|r| r.id == raj));

    // Raj's request gets a draft, with the choice left to you; it can't go
    // until you choose.
    mock.push(Reply::Text(
        json!({"target": "chat:T1:D9", "text": "Wednesday works. [[10:00 | 14:00]]?", "note": "Check the offsite"}).to_string(),
    ));
    budget(12).await;
    let drafted = eventually("a draft", async || {
        let r: Request = serde_json::from_value(rpc("memex.requests.get", json!({"id": raj})).await).ok()?;
        r.draft.filter(|d| d.error.is_none())
    })
    .await;
    assert_eq!((drafted.target.as_str(), drafted.kind.as_str(), drafted.note.as_str()), ("chat:T1:D9", "chat", "Check the offsite"));
    let read = mock.requests().last().unwrap()["messages"].to_string();
    assert!(read.contains("Move the 1:1 to Wednesday") && read.contains("move our 1:1") && read.contains("calendar"), "{read}");
    let r = http
        .post(format!("{api}/memex/rpc"))
        .json(&json!({"method": "memex.requests.send", "params": {"id": raj, "target": "chat:T1:D9", "text": drafted.text}}))
        .send()
        .await
        .unwrap();
    assert!(r.text().await.unwrap().contains("choose first"));
    let to = rpc("memex.requests.reply_to", json!({"target": "chat:T1:D9"})).await;
    assert_eq!((to["kind"].as_str(), to["channel"].as_str()), (Some("chat"), Some("D9")));
    let to = rpc("memex.requests.reply_to", json!({"target": "thread:acct:t2"})).await;
    assert_eq!((to["to"][0].as_str(), to["subject"].as_str()), (Some("bo@acme.com"), Some("Re: Deploy")), "{to}");
    // Raj says more: sorted, then drafted again.
    mock.push(Reply::Text(json!({"category": "needs_you", "reason": "Raj asks again", "asks": "Move the 1:1 to Wednesday"}).to_string()));
    mock.push(Reply::Text(json!({"target": "chat:T1:D9", "text": "Wednesday at 14:00 it is.", "note": ""}).to_string()));
    m.store.put(&chat("k5", "D9", "dm", "Or Thursday afternoon, if that's easier", HOUR / 10, false), "test-slack").await.unwrap();
    budget(14).await;
    eventually("drafted again", async || {
        let r: Request = serde_json::from_value(rpc("memex.requests.get", json!({"id": raj})).await).ok()?;
        (r.draft?.text == "Wednesday at 14:00 it is.").then_some(())
    })
    .await;

    // A new request is an event: a trigger keeps a record of Acme's.
    let r = http
        .post(format!("{api}/collections"))
        .json(&json!({"name": "opened", "description": "Requests from Acme", "schema": {"type": "object", "properties": {"who": {"type": "string"}, "what": {"type": "string"}}}}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let r = http
        .post(format!("{api}/triggers"))
        .json(&json!({"name": "acme asks", "trigger": "memex", "cron": "request.opened from:@acme.com", "action": "write",
                      "action_params": {"collection": "opened", "fields": {"who": "$.data.from", "what": "$.data.subject"}}}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    // Urgent by your rule: you're told at once.
    mock.push(Reply::Text(
        json!({"category": "needs_you", "reason": "Checkout is down", "asks": "Approve the rollback", "urgent": true}).to_string(),
    ));
    mail(
        &m,
        "a9",
        "t10",
        ("ana@acme.com", "Ana"),
        "me@mine.io",
        "Prod down",
        "The deploy broke checkout. OK to roll back now?",
        None,
        &["INBOX"],
        HOUR / 20,
    )
    .await;
    budget(15).await;
    eventually("told at once", async || {
        open("open").await.into_iter().find(|r| r.title == "Approve the rollback" && r.notified_at.is_some())
    })
    .await;
    assert!(mock.requests().last().unwrap()["messages"].to_string().contains("production outage"), "the rule is in the prompt");
    let kept = eventually("the trigger's record", async || {
        let v: Value = http.get(format!("{api}/collections/opened/records")).send().await.ok()?.json().await.ok()?;
        v["records"].as_array().filter(|r| !r.is_empty()).cloned()
    })
    .await;
    assert_eq!(
        (kept[0]["fields"]["who"].as_str(), kept[0]["fields"]["what"].as_str()),
        (Some("Ana <ana@acme.com>"), Some("Approve the rollback"))
    );
    // The brief at its time (13:30, with no calendar: UTC), once; the close
    // of day at 17:30.
    let on = |hm: &str| {
        chrono::NaiveDateTime::parse_from_str(&format!("2031-01-08 {hm}"), "%Y-%m-%d %H:%M").unwrap().and_utc().timestamp_millis()
    };
    let sent = c6_memex::briefs::due(&m, on("13:31")).await.unwrap();
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(sent[0].0.starts_with("Brief: 3 requests need you"), "{sent:?}");
    assert!(sent[0].1.contains("Ana: Approve the rollback") && sent[0].1.contains("1 first contact"), "{sent:?}");
    assert!(c6_memex::briefs::due(&m, on("13:40")).await.unwrap().is_empty(), "once");
    let close = c6_memex::briefs::due(&m, on("17:31")).await.unwrap();
    assert_eq!(close[0].0, "Close of day");
    assert!(close[0].1.contains("Still yours: 3 requests"), "{close:?}");

    // A realm's memex reads with its own prompts.
    assert!(http.post(format!("{api}/realms")).json(&json!({"id": "lab"})).send().await.unwrap().status().is_success());
    c6_head::store::SETTINGS_REALM
        .scope("lab".into(), c6_head::prompts::set(&head.state, "memex.triage", "Lab's own sorting"))
        .await
        .unwrap();
    let config = c6_head::memex::config(&head.state).await.unwrap();
    assert_eq!(config["realm_prompts"]["lab"]["memex.triage"], "Lab's own sorting");
    assert_ne!(config["prompts"]["memex.triage"], "Lab's own sorting", "home's stays");

    head.shutdown().await.unwrap();
}
