//! The Chat page's data (docs/chat.md §2): conversations with unread and
//! mention counts that follow where you've read to; a DM's person; your
//! stars, mutes and sections; messages around one; threads you're in with
//! their new replies; activity; saved messages; search with filters; and
//! unreads.

use c6_head::HeadConfig;
use serde_json::{Value, json};

#[tokio::test]
async fn chat_as_a_client_sees_it() {
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
    let st = head.state.clone();
    let put = |params: Value| {
        let st = st.clone();
        async move { c6_head::memex::call_in(&st, "home", "memex.chat.put", params).await.unwrap() }
    };
    // Only an app's kit says what's in a workspace.
    let r = http.post(format!("{api}/memex/rpc")).json(&json!({"method": "memex.chat.put", "params": {}})).send().await.unwrap();
    assert!(!r.status().is_success());

    let base = c6_core::now_ms() / 1000 - 3600;
    let ts = |n: i64| format!("{}.000100", base + n * 60);
    let msg = |channel: &str, name: &str, kind: &str, t: String, user: &str, text: &str, extra: Value| {
        let st = st.clone();
        let mut meta = json!({"service": "slack", "workspace": "T1", "workspace_name": "Acme", "channel": channel, "channel_name": name,
                              "channel_kind": kind, "ts": t, "user": user, "user_name": user, "from_me": user == "UME"});
        for (k, v) in extra.as_object().cloned().unwrap_or_default() {
            meta[k] = v;
        }
        let item = json!({"kind": "chat", "source": "app:slack", "external_id": format!("T1/{channel}/{t}"), "title": format!("{name} · {user}"),
                          "text": format!("{user}: {text}"), "created_at": (t.parse::<f64>().unwrap() * 1000.0) as i64, "meta": meta});
        async move { c6_head::memex::call_in(&st, "home", "memex.ingest", item).await.unwrap() }
    };
    msg("C1", "#eng", "public", ts(1), "U1", "kickoff plan", json!({})).await;
    msg("C1", "#eng", "public", ts(2), "U2", "@Me review please", json!({"mentions_me": true, "thread_ts": ts(2), "reply_count": 2})).await;
    msg("C1", "#eng", "public", ts(3), "UME", "I'll look", json!({"thread_ts": ts(2)})).await;
    msg("C1", "#eng", "public", ts(4), "U1", "thanks", json!({"thread_ts": ts(2)})).await;
    msg("C1", "#eng", "public", ts(5), "UME", "shipped https://x.io", json!({"reactions": {"tada": 2}, "my_reactions": []})).await;
    msg("D1", "@Ana Lima", "dm", ts(6), "U1", "lunch?", json!({})).await;
    msg("C2", "#random", "public", ts(7), "U2", "random stuff", json!({})).await;
    put(json!({
        "conversations": [
            {"workspace": "T1", "channel": "C1", "name": "#eng", "kind": "public", "topic": "Ship it", "members": 3, "last_read": ts(1)},
            {"workspace": "T1", "channel": "D1", "name": "@Ana Lima", "kind": "dm", "user": "U1", "last_read": "0000000000.000000"},
        ],
        "people": [{"workspace": "T1", "id": "U1", "name": "Ana Lima", "email": "ana@acme.com", "avatar": "https://a/ana.png", "status": ":palm_tree: away"},
                   {"workspace": "T1", "id": "UME", "name": "Me Myself"}],
    }))
    .await;

    let channels = || async {
        let v = rpc("memex.chat.channels", json!({})).await;
        v.as_array()
            .unwrap()
            .iter()
            .map(|c| (c["channel"].as_str().unwrap().to_owned(), c.clone()))
            .collect::<std::collections::HashMap<_, _>>()
    };
    let cs = channels().await;
    assert_eq!(cs.len(), 3, "{cs:?}");
    let (c1, d1, c2) = (&cs["C1"], &cs["D1"], &cs["C2"]);
    assert_eq!(
        (c1["unread"].as_i64(), c1["mentions"].as_i64(), c1["topic"].as_str(), c1["members"].as_i64()),
        (Some(1), Some(1), Some("Ship it"), Some(3)),
        "{c1}"
    );
    assert_eq!((d1["unread"].as_i64(), d1["mentions"].as_i64()), (Some(1), Some(1)), "a DM needs you: {d1}");
    assert_eq!((d1["user_name"].as_str(), d1["user_avatar"].as_str()), (Some("Ana Lima"), Some("https://a/ana.png")));
    assert_eq!(c2["unread"], 0, "never read anywhere: nothing counted");
    assert_eq!(c2["channel_name"], "#random", "known from its messages");
    // Read in Slack: the count follows; the same again changes nothing.
    put(json!({"conversations": [{"workspace": "T1", "channel": "C1", "last_read": ts(5)}]})).await;
    assert_eq!(channels().await["C1"]["unread"], 0);

    // Your organization.
    rpc("memex.chat.prefs", json!({"workspace": "T1", "channel": "C1", "starred": true})).await;
    rpc("memex.chat.prefs", json!({"workspace": "T1", "channel": "C2", "muted": true})).await;
    rpc("memex.chat.prefs", json!({"workspace": "T1", "channel": "D1", "section": "Work"})).await;
    let cs = channels().await;
    assert_eq!(
        (cs["C1"]["starred"].as_bool(), cs["C2"]["muted"].as_bool(), cs["D1"]["section"].as_str()),
        (Some(true), Some(true), Some("Work"))
    );
    let sections = rpc("memex.chat.sections", json!({"workspace": "T1", "names": ["Work", " ", "Later"]})).await;
    assert_eq!(sections, json!({"T1": ["Work", "Later"]}));

    // A conversation's messages (no replies), and around one.
    let all = rpc("memex.chat.messages", json!({"workspace": "T1", "channel": "C1"})).await;
    let texts = |v: &Value| v.as_array().unwrap().iter().map(|m| m["text"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    assert_eq!(texts(&all), ["UME: shipped https://x.io", "U2: @Me review please", "U1: kickoff plan"]);
    let around = rpc("memex.chat.messages", json!({"workspace": "T1", "channel": "C1", "around": ts(2), "limit": 2})).await;
    assert_eq!(texts(&around), ["UME: shipped https://x.io", "U2: @Me review please", "U1: kickoff plan"], "one each side");

    // Threads you're in: a reply since yours is new, until you look.
    let threads = rpc("memex.chat.threads", json!({})).await;
    assert_eq!(threads.as_array().unwrap().len(), 1, "{threads}");
    assert_eq!((threads[0]["replies"].as_u64(), threads[0]["new"].as_u64()), (Some(2), Some(1)), "{threads}");
    assert_eq!(threads[0]["first"]["text"], "U2: @Me review please");
    rpc("memex.chat.thread_read", json!({"workspace": "T1", "channel": "C1", "thread_ts": ts(2), "ts": ts(4)})).await;
    assert_eq!(rpc("memex.chat.threads", json!({})).await[0]["new"], 0);

    // Activity: the mention, and reactions to yours, newest first.
    let act = rpc("memex.chat.activity", json!({})).await;
    let why: Vec<&str> = act.as_array().unwrap().iter().map(|a| a["why"].as_str().unwrap()).collect();
    assert_eq!(why, ["reaction", "mention"], "{act}");

    // Saved for later, then done.
    let kickoff = all[2]["id"].as_str().unwrap().to_owned();
    rpc("memex.chat.save", json!({"id": kickoff})).await;
    assert_eq!(rpc("memex.chat.saved", json!({})).await[0]["id"], kickoff.as_str());
    rpc("memex.chat.save", json!({"id": kickoff, "done": true})).await;
    assert_eq!(rpc("memex.chat.saved", json!({})).await, json!([]));
    assert_eq!(rpc("memex.chat.saved", json!({"done": true})).await.as_array().unwrap().len(), 1);

    // Search: words, and filters.
    let search = |p: Value| async move { texts(&rpc("memex.chat.search", p).await) };
    assert_eq!(search(json!({"q": "review"})).await, ["U2: @Me review please"]);
    assert_eq!(search(json!({"from": "me"})).await, ["UME: shipped https://x.io", "UME: I'll look"]);
    assert_eq!(search(json!({"has": "link"})).await, ["UME: shipped https://x.io"]);
    assert_eq!(search(json!({"in": "#eng", "q": "kickoff"})).await, ["U1: kickoff plan"]);
    assert_eq!(search(json!({"in": "#random", "q": "kickoff"})).await, Vec::<String>::new());
    assert_eq!(search(json!({"thread": true})).await, ["U1: thanks", "UME: I'll look"]);
    assert_eq!(search(json!({"from": "Ana Lima"})).await, Vec::<String>::new(), "by id or the name on the message");
    assert_eq!(search(json!({"from": "U1", "in": "D1"})).await, ["U1: lunch?"]);

    // Unreads: the DM; not the muted channel (it doesn't mention you).
    put(json!({"conversations": [{"workspace": "T1", "channel": "C2", "last_read": "0000000000.000000"}]})).await;
    let unreads = rpc("memex.chat.unreads", json!({})).await;
    let names: Vec<&str> = unreads.as_array().unwrap().iter().map(|c| c["channel_name"].as_str().unwrap()).collect();
    assert_eq!(names, ["@Ana Lima"], "{unreads}");
    assert_eq!(unreads[0]["messages"][0]["text"], "U1: lunch?");

    let people = rpc("memex.chat.people", json!({"workspace": "T1"})).await;
    assert_eq!(people[0]["status"], ":palm_tree: away");
    let me: Vec<&str> = people.as_array().unwrap().iter().filter(|p| p["me"] == true).map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(me, ["Me Myself"], "who your messages are from");
}
