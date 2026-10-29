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

/// The sorting prompt asked about a conversation (by its text).
fn about_card(asked: &[String], text: &str) -> String {
    asked.iter().find(|a| a.contains(text) && a.contains("Sort this conversation")).cloned().unwrap_or_default()
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
    // company), Lee and Pat (strangers: the pitch prompt); and what the rules
    // sorted without it (Sam, Kim, the newsletter) the model sorts again,
    // with what the rules noticed as hints.
    // Each answer for its conversation, whatever order they're asked in.
    for (about, r) in [
        (
            "approve the redlines",
            json!({"category": "needs_you", "reason": "Ana needs your approval by Thursday", "asks": "Approve the redlines", "due": "2031-01-09"}),
        ),
        (
            "move our 1:1",
            json!({"category": "needs_you", "reason": "Raj wants to move your 1:1", "asks": "Move the 1:1 to Wednesday", "due": null}),
        ),
        ("the deploy is done", json!({"category": "fyi", "reason": "The deploy is done", "asks": ""})),
        ("scale SDRs", json!({"category": "pitch", "reason": "Sells SDR outsourcing"})),
        (
            "pilot for 40 seats",
            json!({"category": "first_contact", "reason": "A customer asks for a pilot", "asks": "A pilot for 40 seats"}),
        ),
        ("Loved your post", json!({"category": "pitch", "reason": "A sales sequence"})),
        ("bumping this", json!({"category": "pitch", "reason": "A sequence's follow-up"})),
        ("This week in Rust", json!({"category": "newsletter", "reason": "A weekly newsletter"})),
        // Ana has history: before her mail is sorted, her three threads are
        // read and her card written; then Acme's (you write to Acme), after
        // reading Bo's thread.
        (
            "Write the owner's card for Ana",
            json!({"relationship": "customer", "role": "Legal at Acme", "story": "Negotiating Acme's contract.", "open": "The redlines", "how": "Answers within a day"}),
        ),
        (
            "Write the owner's card for acme.com",
            json!({"relationship": "customer", "role": "", "story": "Acme is a customer.", "open": "", "how": ""}),
        ),
    ] {
        // Sorting (mail comes with its history; chat doesn't), or a card.
        let kind = if about.starts_with("Write the") {
            "Write the owner's card"
        } else if about == "move our 1:1" {
            "Sort this conversation"
        } else {
            "History with the sender"
        };
        mock.push_about_all(&[kind, about], Reply::Text(r.to_string()));
    }
    for _ in 0..4 {
        mock.push_about(
            "This is an email thread",
            Reply::Text(json!({"summary": "A thread with Acme.", "topics": [], "commitments": []}).to_string()),
        );
    }
    budget(14).await;
    eventually("the model's sorting", async || (sorted("thread:acct:t7").await?.by == "model").then_some(())).await;
    let want = [
        ("thread:acct:t1", "needs_you"),
        ("chat:T1:D9", "needs_you"),
        ("thread:acct:t2", "fyi"),
        ("thread:acct:t5", "pitch"),
        ("thread:acct:t6", "first_contact"),
        ("thread:acct:t3", "pitch"),
        ("thread:acct:t4", "pitch"),
        ("thread:acct:t7", "newsletter"),
    ];
    for (t, c) in want {
        let s = sorted(t).await.unwrap_or_else(|| panic!("{t} sorted"));
        assert_eq!((s.category.as_str(), s.by.as_str()), (c, "model"), "{s:?}");
    }
    assert!(sorted("chat:T1:C9:8200.0001").await.is_none(), "a channel message not to you isn't sorted");
    let asked: Vec<String> = mock.requests().iter().map(|r| r["messages"].to_string()).collect();
    assert_eq!(asked.len(), 14, "your last word needed no model");
    // Who Ana is came in with her mail; the cards are kept.
    assert!(
        about_card(&asked, "approve the redlines").contains("Ana <ana@acme.com> — customer (Legal at Acme). Negotiating Acme's contract.")
    );
    let cards = rpc("memex.relations.cards", json!({"address": "ana@acme.com"})).await;
    let names: Vec<&str> = cards.as_array().unwrap().iter().map(|c| c["key"].as_str().unwrap()).collect();
    assert_eq!(names, ["person:ana@acme.com", "org:acme.com"], "{cards}");
    assert_eq!(mock.requests()[0]["reasoning_effort"], "none", "sorting doesn't reason by default");
    // Each call shows in the activity panel: what for, about what, how it went.
    let activity: Value = http.get(format!("{api}/system/activity")).send().await.unwrap().json().await.unwrap();
    let call = activity["calls"].as_array().unwrap().iter().find(|c| c["what"] == "sorting" && c["about"] == "Redlines").cloned();
    let call = call.unwrap_or_else(|| panic!("the sorting of Ana's mail in {activity}"));
    assert_eq!(
        (call["state"].as_str(), call["connection_name"].as_str(), call["effort"].as_str()),
        (Some("done"), Some("m"), Some("none"))
    );
    assert!(call.get("prompt").is_none(), "the list leaves the text out");
    let whole: Value = http.get(format!("{api}/system/activity/calls/{}", call["id"])).send().await.unwrap().json().await.unwrap();
    assert!(
        whole["prompt"].as_str().unwrap().contains("approve the redlines") && whole["text"].as_str().unwrap().contains("needs_you"),
        "{whole}"
    );
    // The sorting of a conversation (by its text), not its reading.
    let about = |text: &str| {
        let sorting = |a: &&String| a.contains("Sort this conversation") || a.contains("has never written to the sender");
        asked.iter().filter(sorting).find(|a| a.contains(text)).unwrap_or_else(|| panic!("asked about {text}")).clone()
    };
    assert!(about("approve the redlines").contains("Sort this conversation"));
    assert!(about("scale SDRs").contains("has never written to the sender"), "strangers get the pitch prompt");
    // Each with its history, counted from the mail, and the rules' hints.
    assert!(
        about("approve the redlines").contains("The owner wrote to ana@acme.com 2 times; Ana <ana@acme.com> wrote 2 messages in 2 threads"),
        "{}",
        about("approve the redlines")
    );
    assert!(
        about("the deploy is done")
            .contains("At acme.com (2 senders): the owner wrote 2 times, they wrote 3 messages in 3 threads, answered in 1"),
        "{}",
        about("the deploy is done")
    );
    assert!(about("Loved your post").contains("sales-sequence tool's mark (apollo.io)"), "{}", about("Loved your post"));
    assert!(about("This week in Rust").contains("mailing-list headers"), "{}", about("This week in Rust"));
    assert_eq!(sorted("thread:acct:t1").await.unwrap().reason, "Ana needs your approval by Thursday");
    // The participant index counts the same, for every address at once.
    c6_memex::relations::rebuild(&m).await.unwrap();
    let ana = c6_memex::relations::of(&m, "Ana@acme.com").await.unwrap();
    assert_eq!((ana.received, ana.sent_to, ana.threads, ana.answered), (2, 2, 2, 1), "{ana:?}");
    let (acme, people) = c6_memex::relations::of_domain(&m, "acme.com").await.unwrap().unwrap();
    assert_eq!((acme.received, people), (3, 2), "{acme:?}");
    assert!(c6_memex::relations::of_domain(&m, "gmail.com").await.unwrap().is_none(), "a shared domain has no company");
    let weekly = c6_memex::relations::of(&m, "news@weekly.dev").await.unwrap();
    assert_eq!((weekly.received, weekly.bulk, weekly.sent_to), (1, 1, 0));
    // The brief's numbers: what was read, by kind, and how much by the model.
    let status = rpc("memex.triage.status", json!({})).await;
    assert_eq!(status["sorted"]["chat"]["by_model"], 1, "{status}");
    assert!(status["sorted"]["mail"]["n"].as_i64() > status["sorted"]["mail"]["by_model"].as_i64(), "rules sorted mail too: {status}");
    // FYI you've seen leaves the brief.
    let fyi = async || rpc("memex.triage.list", json!({"category": "fyi", "unseen": true})).await.as_array().unwrap().len();
    assert_eq!(fyi().await, 1);
    rpc("memex.triage.seen", json!({"target": "thread:acct:t2"})).await;
    assert_eq!(fyi().await, 0, "seen");

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
    budget(15).await;
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
    budget(16).await;
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
    budget(18).await;
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
    budget(20).await;
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
    budget(21).await;
    let drafted = c6_testkit::eventually_or(
        "a draft",
        async || {
            let r: Request = serde_json::from_value(rpc("memex.requests.get", json!({"id": raj})).await).ok()?;
            r.draft.filter(|d| d.error.is_none())
        },
        || mock.log(),
    )
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
    budget(23).await;
    c6_testkit::eventually_or(
        "drafted again",
        async || {
            let r: Request = serde_json::from_value(rpc("memex.requests.get", json!({"id": raj})).await).ok()?;
            (r.draft?.text == "Wednesday at 14:00 it is.").then_some(())
        },
        || mock.log(),
    )
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
    budget(24).await;
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

/// A model that fails for now (5xx, 429, unreachable) doesn't sort a
/// conversation: it waits, its call goes back to the day's budget, and it
/// is sorted once the model answers. With an effort, the answer has room
/// for the model's thinking.
#[tokio::test]
async fn sorting_waits_out_a_model_that_fails_for_now() {
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
    let sorted = async || -> Option<Triage> {
        let v: Value = http
            .post(format!("{api}/memex/rpc"))
            .json(&json!({"method": "memex.triage.list", "params": {}}))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        serde_json::from_value::<Vec<Triage>>(v).ok()?.into_iter().find(|t| t.target == "thread:acct:p1")
    };

    // Busy for every retry of the first call; then the answer.
    for _ in 0..5 {
        mock.push_about("pilot for 40 seats", Reply::Retry(503));
    }
    mock.push_about("pilot for 40 seats", Reply::Text(json!({"category": "fyi", "reason": "A pilot, later"}).to_string()));
    mail(
        &m,
        "p1",
        "p1",
        ("pat@northwind.com", "Pat"),
        "me@mine.io",
        "Pilot",
        "We use your scheduler and want a pilot for 40 seats.",
        None,
        &["INBOX"],
        HOUR,
    )
    .await;
    // One call a day: only the one given back lets it sort.
    let r = http
        .put(format!("{api}/settings/memex"))
        .json(&json!({"understanding": {"connection": "m", "model": "reader-1", "effort": "low"}, "calls_per_day": 1}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());

    let s = eventually("sorted", async || sorted().await).await;
    assert_eq!((s.category.as_str(), s.by.as_str()), ("fyi", "model"), "never the failure: {s:?}");
    let asked = mock.requests();
    assert_eq!(asked.len(), 6, "five busy, then the answer");
    assert_eq!(asked[5]["max_tokens"], 400 + c6_memex::llm::THINKING_ROOM, "room to think at effort low");
    head.shutdown().await.unwrap();
}

/// A model that stops answering mid-stream holds no lane: past the pause a
/// call may take, the call failed for now and is asked again. And while a
/// call hangs, choosing another model takes over at once.
#[tokio::test]
async fn a_stalled_answer_lets_go() {
    let (a, b, c) = (MockLlm::start().await, MockLlm::start().await, MockLlm::start().await);
    let answer = |from: &str| Reply::Text(json!({"category": "fyi", "reason": from, "summary": "s", "topics": []}).to_string());
    // a stalls in the middle of its first answer; then answers. c only stalls.
    let _stalled = a.push_gated(answer("a"));
    let _hung = c.push_gated(answer("c"));
    for _ in 0..20 {
        a.push(answer("a"));
        b.push(answer("b"));
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
    for (name, mock) in [("a", &a), ("b", &b), ("c", &c)] {
        let r = http.post(format!("{api}/connections")).json(&json!({"name": name, "base_url": mock.base_url()})).send().await.unwrap();
        assert!(r.status().is_success());
    }
    let m = c6_head::knowledge::get(&head.state).expect("the head's knowledge store");
    let understand_with = async |connection: &str| {
        let body = json!({"understanding": {"connection": connection, "model": "reader-1"}, "calls_per_day": 100});
        assert!(http.put(format!("{api}/settings/memex")).json(&body).send().await.unwrap().status().is_success());
    };
    let sorted = async |thread: &str| -> Option<Triage> {
        let v: Value = http
            .post(format!("{api}/memex/rpc"))
            .json(&json!({"method": "memex.triage.list", "params": {"limit": 100}}))
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        serde_json::from_value::<Vec<Triage>>(v).ok()?.into_iter().find(|t| t.target == format!("thread:acct:{thread}"))
    };

    // A pause of a second is a stall here.
    let limits =
        |idle: u64| c6_llm::limit::Limits { total: std::time::Duration::from_secs(3600), idle: std::time::Duration::from_secs(idle) };
    m.set_call_limits(limits(1));
    mail(&m, "p1", "p1", ("pat@northwind.com", "Pat"), "me@mine.io", "Pilot", "A pilot for 40 seats?", None, &["INBOX"], HOUR).await;
    understand_with("a").await;
    let s = eventually("sorted once a answers", async || sorted("p1").await).await;
    assert_eq!((s.by.as_str(), s.reason.as_str()), ("model", "a"), "never the stall: {s:?}");
    let calls = c6_llm::activity::calls();
    assert!(calls.iter().any(|c| c.state == "failed" && c.error.contains("stalled")), "the stalled call is shown");

    // c hangs (no stall is short enough to notice now); you choose b instead.
    // The new mail comes once c is chosen: before, a (reading on after p1)
    // could sort it first.
    m.set_call_limits(limits(3600));
    understand_with("c").await;
    mail(&m, "p2", "p2", ("kim@tailspin.com", "Kim"), "me@mine.io", "Lunch", "Lunch next week?", None, &["INBOX"], HOUR).await;
    eventually("a call hanging on c", async || (c.requests().len() == 1).then_some(())).await;
    understand_with("b").await;
    let s = eventually("sorted by b", async || sorted("p2").await.filter(|s| s.reason == "b")).await;
    assert_eq!(s.by, "model");
    head.shutdown().await.unwrap();
}

/// Your Google account (me@mine.io), as the Google apps keep it, so the
/// memex knows which mail is to you: the apps installed, the account kept
/// (the sign-in itself is the providers test's), the memex told.
pub(crate) async fn my_account(head: &c6_head::Head, api: &str, m: &c6_memex::Memex) {
    let http = reqwest::Client::new();
    let installs: Vec<c6_api::Install> = http.post(format!("{api}/google/install")).send().await.unwrap().json().await.unwrap();
    for i in &installs {
        let r =
            http.post(format!("{api}/decisions/{}/answer", i.decision.unwrap())).json(&json!({"option": "Install"})).send().await.unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    }
    for (app, _) in c6_head::google::APPS {
        let a = c6_head::providers::Kept {
            realm: "home",
            instance: app,
            provider: "google",
            account: "me@mine.io",
            name: "Me",
            refresh: "rt-1",
            access: "at-1",
            expires: c6_core::now_ms() + 3_600_000,
            scopes: "all",
        };
        c6_head::providers::keep(&head.state, &a, None).await.unwrap();
    }
    c6_head::memex::reload(&head.state).await;
    eventually("the memex knows your account", async || m.accounts().iter().any(|a| a.email == "me@mine.io").then_some(())).await;
}

/// A conversation as the model sorted it, at its latest message; what
/// needs you (or is a first contact) opens its request, as sorting does.
pub(crate) async fn sorted_as(m: &c6_memex::Memex, thread: &str, category: &str) {
    let (at, sender): (i64, String) = sqlx::query_as("SELECT date, from_addr FROM mail WHERE thread_id = ? ORDER BY date DESC LIMIT 1")
        .bind(thread)
        .fetch_one(&m.store.pool)
        .await
        .unwrap();
    let t = c6_api::Triage {
        target: format!("thread:acct:{thread}"),
        kind: "mail".into(),
        category: category.into(),
        reason: "as sorted".into(),
        by: "model".into(),
        sender,
        title: thread.into(),
        at,
        ..Default::default()
    };
    sqlx::query(
        "INSERT OR REPLACE INTO triage (target, kind, category, reason, by, sender, title, at, done_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&t.target)
    .bind(&t.kind)
    .bind(&t.category)
    .bind(&t.reason)
    .bind(&t.by)
    .bind(&t.sender)
    .bind(&t.title)
    .bind(at)
    .bind(c6_core::now_ms())
    .execute(&m.store.pool)
    .await
    .unwrap();
    c6_memex::requests::sorted_by_you(m, &t).await.unwrap();
}

/// "Needs reply" (Mail's view, the Activity page) follows the sorting:
/// mail straight to you that the model sorted as a notification doesn't
/// need a reply, mail that needs you does. With a model, that's all (what
/// isn't sorted yet waits for it); with none, mail not sorted does too,
/// unless an automated sender wrote it. Either way, only what's as recent
/// as what's sorted.
#[tokio::test]
async fn needs_reply_follows_the_sorting() {
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
    let m = c6_head::knowledge::get(&head.state).expect("the head's knowledge store");
    my_account(&head, &api, &m).await;
    // A model that can't answer now: what isn't sorted yet stays so.
    let r =
        http.post(format!("{api}/connections")).json(&json!({"name": "down", "base_url": "http://127.0.0.1:9/v1"})).send().await.unwrap();
    assert!(r.status().is_success());
    let body = json!({"understanding": {"connection": "down", "model": "reader-1"}, "calls_per_day": 100});
    let r = http.put(format!("{api}/settings/memex")).json(&body).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    eventually("the memex has its model", async || m.understanding().map(drop)).await;

    let me = "me@mine.io";
    mail(
        &m,
        "n1",
        "n1",
        ("orders@shop.example", "Shop"),
        me,
        "Your order has shipped",
        "Order 1182 is on its way.",
        None,
        &["INBOX"],
        HOUR,
    )
    .await;
    mail(&m, "n2", "n2", ("carol@acme.com", "Carol"), me, "Contract", "Can you sign it by Friday?", None, &["INBOX"], 2 * HOUR).await;
    mail(&m, "n3", "n3", ("no-reply@bank.example", "Bank"), me, "Transfer confirmed", "Your $40 went through.", None, &["INBOX"], 3 * HOUR)
        .await;
    mail(&m, "n4", "n4", ("dan@other.org", "Dan"), me, "Coffee", "Coffee next week?", None, &["INBOX"], 4 * HOUR).await;
    // Older than what's sorted: never sorted, so never shown.
    mail(&m, "n5", "n5", ("erin@other.org", "Erin"), me, "Visit", "Can we visit in May?", None, &["INBOX"], 20 * 24 * HOUR).await;
    sorted_as(&m, "n1", "notification").await;
    sorted_as(&m, "n2", "needs_you").await;

    let rpc = async |method: &str, params: Value| -> Value {
        let r = http.post(format!("{api}/memex/rpc")).json(&json!({"method": method, "params": params})).send().await.unwrap();
        assert!(r.status().is_success());
        r.json().await.unwrap()
    };
    let shown = async || -> Vec<String> {
        let v = rpc("memex.mail.threads", json!({"view": "needs_reply"})).await;
        serde_json::from_value::<Vec<MailThread>>(v).unwrap().into_iter().map(|t| t.subject).collect()
    };
    assert_eq!(shown().await, ["Contract"], "only what the model sorted as needing you");
    // The rest weren't sorted.
    for t in ["n3", "n4", "n5"] {
        assert_eq!(rpc("memex.triage.get", json!({"target": format!("thread:acct:{t}")})).await, Value::Null, "{t}");
    }
    // No model: what isn't sorted needs a reply, unless an automated
    // sender wrote it; not what's older.
    let r = http.put(format!("{api}/settings/memex")).json(&json!({"calls_per_day": 100})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    eventually("the memex has no model", async || m.understanding().is_none().then_some(())).await;
    assert_eq!(shown().await, ["Contract", "Coffee"], "not the shipping notice nor the bank's");
    head.shutdown().await.unwrap();
}

/// The kit, reading only once let (as a worker busy elsewhere, or asleep).
struct Held {
    asked: std::sync::atomic::AtomicBool,
    go: tokio::sync::Semaphore,
    kit: c6_memex::extract::InProcess,
}

impl Default for Held {
    fn default() -> Self {
        Held { asked: Default::default(), go: tokio::sync::Semaphore::new(0), kit: Default::default() }
    }
}

#[async_trait::async_trait]
impl c6_memex::extract::Extractor for Held {
    async fn extract(&self, req: c6_memex::extract::Request) -> anyhow::Result<c6_memex::extract::Response> {
        self.asked.store(true, std::sync::atomic::Ordering::Relaxed);
        drop(self.go.acquire().await?);
        c6_memex::extract::Extractor::extract(&self.kit, req).await
    }
    async fn walk(&self, worker: &str, root: &str) -> anyhow::Result<(String, Vec<(String, u64, i64)>)> {
        c6_memex::extract::Extractor::walk(&self.kit, worker, root).await
    }
    async fn read(&self, worker: &str, path: &str, max: u64) -> anyhow::Result<Vec<u8>> {
        c6_memex::extract::Extractor::read(&self.kit, worker, path, max).await
    }
}

/// Mail moved to spam in Gmail leaves Mail's inbox views (the inbox,
/// "Needs reply", what's sorted as needing you): moved once it's kept, or
/// while it still waited to be read (that change was lost, and it stayed
/// in the inbox).
#[tokio::test]
async fn mail_moved_to_spam_leaves_the_inbox() {
    let kit = std::sync::Arc::new(Held::default());
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
    let m = c6_head::knowledge::get(&head.state).expect("the head's knowledge store");
    // This store's only: other tests in the process read as they do.
    m.read_with(kit.clone());
    my_account(&head, &api, &m).await;
    use base64::Engine;
    // As the gmail app's sync gives them: a message, and a label change
    // (the same item, without its bytes).
    let message = |id: &str, subject: &str, ago: i64| {
        let raw = format!(
            "From: Carol <carol@acme.com>\r\nTo: me@mine.io\r\nSubject: {subject}\r\nMessage-ID: <{id}@test>\r\nContent-Type: text/plain\r\n\r\nCan you look at this?\r\n"
        );
        json!({"type": "message/rfc822", "source": {"kind": "app"}, "external_id": format!("acct:{id}"), "data": base64::engine::general_purpose::STANDARD.encode(raw),
               "attributes": {"account": "acct", "message": id, "thread": id, "labels": ["INBOX", "UNREAD"], "received": c6_core::now_ms() - ago}})
    };
    let to_spam = |id: &str| {
        json!({"type": "message/rfc822", "source": {"kind": "app"}, "external_id": format!("acct:{id}"),
               "attributes": {"account": "acct", "message": id, "labels": ["SPAM", "UNREAD", "CATEGORY_PERSONAL"]}})
    };
    let ingest =
        async |items: Value| c6_memex::ingest::ingest(&m, json!({"as": "app:gmail", "wait": false, "items": items})).await.unwrap();
    let shown = async |view: &str| -> Vec<String> {
        c6_memex::mail::threads(&m, view, None, None, 50).await.unwrap().into_iter().map(|t| t.subject).collect()
    };

    // Moved while the kit reads it.
    ingest(json!([message("s2", "Moved", HOUR)])).await;
    eventually("the kit reading it", async || kit.asked.load(std::sync::atomic::Ordering::Relaxed).then_some(())).await;
    let r = ingest(json!([to_spam("s2")])).await;
    assert_eq!(r["results"][0]["status"], "queued", "kept with its new labels once read: {r}");
    kit.go.add_permits(1);
    ingest(json!([message("s1", "Kept", 2 * HOUR)])).await;
    eventually("both read", async || {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mail WHERE thread_id IN ('s1', 's2')").fetch_one(&m.store.pool).await.ok()?;
        (n == 2).then_some(())
    })
    .await;
    assert_eq!(shown("inbox").await, ["Kept"]);
    assert_eq!(shown("needs_reply").await, ["Kept"]);
    assert_eq!(shown("spam").await, ["Moved"]);

    // Kept, then moved.
    sorted_as(&m, "s1", "needs_you").await;
    assert_eq!(shown("triage:needs_you").await, ["Kept"]);
    let r = ingest(json!([to_spam("s1")])).await;
    assert_eq!(r["results"][0]["status"], "updated", "{r}");
    for view in ["inbox", "needs_reply", "triage:needs_you", "all", "starred", "label:CATEGORY_PERSONAL"] {
        assert!(shown(view).await.is_empty(), "{view}");
    }
    assert_eq!(shown("spam").await, ["Moved", "Kept"]);
    head.shutdown().await.unwrap();
}

/// Mail's "Needs you" (and "First contact") is what waits on you in the
/// Brief: in the inbox, its request open. One done or dropped there leaves
/// it, so does one snoozed until later (it comes back once its time has
/// passed), and so does one archived; so does "Needs reply".
#[tokio::test]
async fn needs_you_is_what_waits_on_you() {
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
    let m = c6_head::knowledge::get(&head.state).expect("the head's knowledge store");
    my_account(&head, &api, &m).await;
    let me = "me@mine.io";
    for (i, (from, subject)) in
        [("ana@acme.com", "Open"), ("bo@acme.com", "Done"), ("cy@acme.com", "Snoozed"), ("di@acme.com", "Archived")].into_iter().enumerate()
    {
        let id = format!("q{i}");
        mail(&m, &id, &id, (from, ""), me, subject, "Can you look at this?", None, &["INBOX"], (i as i64 + 1) * HOUR).await;
        sorted_as(&m, &id, "needs_you").await;
    }
    let rpc = async |method: &str, params: Value| -> Value {
        let r = http.post(format!("{api}/memex/rpc")).json(&json!({"method": method, "params": params})).send().await.unwrap();
        assert!(r.status().is_success(), "{method}: {}", r.text().await.unwrap());
        r.json().await.unwrap()
    };
    let shown = async |view: &str| -> Vec<String> {
        let v = rpc("memex.mail.threads", json!({"view": view})).await;
        serde_json::from_value::<Vec<MailThread>>(v).unwrap().into_iter().map(|t| t.subject).collect()
    };
    let request = async |thread: &str| -> i64 {
        let t: Triage = serde_json::from_value(rpc("memex.triage.get", json!({"target": format!("thread:acct:{thread}")})).await).unwrap();
        t.request.expect("its open request")
    };
    assert_eq!(shown("triage:needs_you").await, ["Open", "Done", "Snoozed", "Archived"]);

    let (done, snoozed) = (request("q1").await, request("q2").await);
    rpc("memex.requests.set", json!({"id": done, "status": "done"})).await;
    let later = c6_core::now_ms() + 3_600_000;
    rpc("memex.requests.set", json!({"id": snoozed, "status": "snoozed", "until": later})).await;
    // Archived in Gmail, as its sync keeps it.
    sqlx::query("UPDATE mail SET inbox = 0 WHERE thread_id = 'q3'").execute(&m.store.pool).await.unwrap();
    assert_eq!(shown("triage:needs_you").await, ["Open"], "done, snoozed and archived leave");
    assert_eq!(shown("needs_reply").await, ["Open"]);
    let t: Value = rpc("memex.triage.get", json!({"target": "thread:acct:q1"})).await;
    assert!(t["request"].is_null(), "a done request isn't offered: {t}");
    // The snooze's time has passed: it's back.
    rpc("memex.requests.set", json!({"id": snoozed, "status": "snoozed", "until": c6_core::now_ms() - 1})).await;
    assert_eq!(shown("triage:needs_you").await, ["Open", "Snoozed"]);
    assert_eq!(shown("needs_reply").await, ["Open", "Snoozed"]);
    head.shutdown().await.unwrap();
}

#[tokio::test]
async fn requests_use_the_drafting_model_automatically_and_on_demand() {
    let (reader, writer) = (MockLlm::start().await, MockLlm::start().await);
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
    for (name, mock) in [("reader", &reader), ("writer", &writer)] {
        let r = http.post(format!("{api}/connections")).json(&json!({"name": name, "base_url": mock.base_url()})).send().await.unwrap();
        assert!(r.status().is_success());
    }
    let m = c6_head::knowledge::get(&head.state).unwrap();
    let put = async |settings: &Value| {
        let r = http.put(format!("{api}/settings/memex")).json(settings).send().await.unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    };
    reader.push(Reply::Text(json!({"category": "needs_you", "reason": "Pat asks for a pilot", "asks": "Approve a pilot"}).to_string()));
    writer.push(Reply::Text(json!({"target": "thread:acct:p1", "text": "The pilot works for us."}).to_string()));
    mail(&m, "p1", "p1", ("pat@mine.io", "Pat"), "me@mine.io", "Pilot", "Can you approve a pilot for 40 seats?", None, &["INBOX"], HOUR)
        .await;
    sqlx::query("INSERT INTO sender_marks (pattern, mark, created_at) VALUES ('pat@mine.io', 'known', ?)")
        .bind(c6_core::now_ms())
        .execute(&m.store.pool)
        .await
        .unwrap();
    let mut settings = json!({
        "understanding": {"connection": "reader", "model": "reader-1"},
        "drafting": {"connection": "writer", "model": "writer-1"},
        "calls_per_day": 1
    });
    put(&settings).await;
    let saved: Value = http.get(format!("{api}/settings/memex")).send().await.unwrap().json().await.unwrap();
    assert_eq!(saved["drafting"], settings["drafting"]);
    let config = c6_head::memex::config(&head.state).await.unwrap();
    assert_eq!(config["drafting"]["model"], "writer-1");
    assert_eq!(config["drafting"]["effort"], "low");
    assert_eq!(config["understanding"]["effort"], "none");

    eventually("request sorted before drafting", async || c6_memex::requests::list(&m, "open", 100).await.ok()?.into_iter().next()).await;
    settings["calls_per_day"] = json!(2);
    settings["drafting"]["effort"] = json!("high");
    put(&settings).await;
    let request = eventually("automatically drafted request", async || {
        let requests = c6_memex::requests::list(&m, "open", 100).await.ok()?;
        requests.into_iter().find(|r| !r.drafting && r.draft.as_ref().is_some_and(|d| d.text == "The pilot works for us."))
    })
    .await;
    assert_eq!(reader.requests().len(), 1);
    assert_eq!(reader.requests()[0]["model"], "reader-1");
    assert_eq!(reader.requests()[0]["reasoning_effort"], "none");
    assert_eq!(writer.requests().len(), 1);
    let calls: i64 = sqlx::query_scalar("SELECT SUM(calls) FROM model_calls").fetch_one(&m.store.pool).await.unwrap();
    assert_eq!(calls, 2, "sorting and drafting share the budget");

    writer.push(Reply::Text(json!({"target": "thread:acct:p1", "text": "Let's start the pilot Monday."}).to_string()));
    let r = http
        .post(format!("{api}/memex/rpc"))
        .json(&json!({"method": "memex.requests.draft", "params": {"id": request.id}}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let pending: Request = r.json().await.unwrap();
    assert!(pending.drafting);
    eventually("Draft again finished", async || {
        let r = c6_memex::requests::get(&m, request.id).await.ok()?;
        (!r.drafting && r.draft?.text == "Let's start the pilot Monday.").then_some(())
    })
    .await;
    assert_eq!(writer.requests().len(), 2);
    for body in writer.requests() {
        assert_eq!(body["model"], "writer-1");
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body["max_tokens"].as_u64().unwrap() >= 1500 + 8192);
    }
    let calls: i64 = sqlx::query_scalar("SELECT SUM(calls) FROM model_calls").fetch_one(&m.store.pool).await.unwrap();
    assert_eq!(calls, 3, "Draft again counts against the same budget");

    settings["understanding"] = Value::Null;
    settings["calls_per_day"] = json!(4);
    writer.push(Reply::Text(json!({"target": "thread:acct:p1", "text": "Monday is confirmed."}).to_string()));
    sqlx::query("UPDATE requests SET drafted_at = NULL WHERE id = ?").bind(request.id).execute(&m.store.pool).await.unwrap();
    put(&settings).await;
    eventually("drafted without understanding", async || {
        let r = c6_memex::requests::get(&m, request.id).await.ok()?;
        (!r.drafting && r.draft?.text == "Monday is confirmed.").then_some(())
    })
    .await;
    assert!(m.understanding().is_none());
    assert_eq!(reader.requests().len(), 1);
    assert_eq!(writer.requests().len(), 3);
    head.shutdown().await.unwrap();
}
