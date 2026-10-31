//! Documents (docs/docs.md): docs and decks in a realm's knowledge, with
//! blocks whose ids survive edits, guarded operations that refuse stale
//! edits whole, history with any version readable, and events.

use c6_api::docs::{Doc, DocApplied, DocVersion};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use reqwest::{Method as M, StatusCode};
use serde_json::{Value, json};

#[tokio::test]
async fn documents_keep_their_blocks_through_guarded_edits() {
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
    let send = |method: M, path: String, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.request(method, &url).json(&body).send().await.unwrap() }
    };
    let ok = async |r: reqwest::Response| -> Value {
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(s.is_success(), "{s}: {v}");
        v
    };
    let get = |path: String| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.get(&url).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let mut live = head.state.docs.live.subscribe();

    // A trigger on settled changes writes who wrote it.
    let schema = json!({"type": "object", "required": ["what"], "properties": {"what": {"type": "string"}}});
    ok(send(M::POST, "collections".into(), json!({"name": "log", "schema": schema})).await).await;
    ok(send(
        M::POST,
        "triggers".into(),
        json!({"name": "doc log", "trigger": "doc", "event_params": {"ops": ["changed"]},
               "action": "write", "action_params": {"collection": "log", "fields": {"what": "{{$.doc.title}} by {{$.authors}}"}}}),
    )
    .await)
    .await;
    let bad =
        json!({"name": "x", "trigger": "doc", "event_params": {"ops": ["exploded"]}, "action": "notify", "action_params": {"title": "t"}});
    assert_eq!(send(M::POST, "triggers".into(), bad).await.status(), StatusCode::BAD_REQUEST);

    // Birth: a title, then a pending block per section.
    let created = ok(send(
        M::POST,
        "docs".into(),
        json!({"title": "Realms for the team", "markdown": "# Plan\n\nWe ship realms to the team.\n\n<?c6 block s1?>\n\n<?c6 block s2?>",
               "blocks": {"s1": {"type": "pending", "intent": "Phases: what ships when"}, "s2": {"type": "pending", "intent": "Risks"}},
               "labels": ["realms"]}),
    )
    .await)
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    let applied: DocApplied = serde_json::from_value(created["applied"].clone()).unwrap();
    let (s1, s2) = (applied.keys["s1"].as_str().unwrap().to_owned(), applied.keys["s2"].as_str().unwrap().to_owned());
    let doc: Doc = serde_json::from_value(created).unwrap();
    assert_eq!((doc.summary.kind.as_str(), doc.summary.rev, doc.summary.pending), ("doc", 1, 2));
    assert_eq!(doc.summary.writers, ["you"]);
    assert_eq!(doc.tabs[0].blocks.iter().map(|b| b.kind.as_str()).collect::<Vec<_>>(), ["heading", "paragraph", "pending", "pending"]);
    let c = live.recv().await.unwrap();
    assert_eq!((c.id.as_str(), c.what.as_str()), (id.as_str(), "created"));
    // Reads stay small: the outline has ids, hashes and first words.
    let outline = get(format!("docs/{id}/read?view=outline")).await;
    assert_eq!(outline["rev"], 1);
    let blocks = outline["tabs"][0]["blocks"].as_array().unwrap().clone();
    assert_eq!(blocks[0]["text"], "Plan");
    assert_eq!(blocks[2]["intent"], "Phases: what ships when");
    let hash_of = |id: &str| blocks.iter().find(|b| b["id"] == id).unwrap()["hash"].as_str().unwrap().to_owned();

    // Filling a pending section keeps its id; one more block gets its own.
    let fill = json!({"ops": [{"op": "replace", "id": s1, "markdown": "## Phases\n\n1. Home realm\n2. The team's", "ifRev": 1, "ifHash": hash_of(&s1)}]});
    let a: DocApplied = serde_json::from_value(ok(send(M::POST, format!("docs/{id}/ops"), fill.clone()).await).await).unwrap();
    assert_eq!((a.rev, a.ids[0][0].as_str(), a.ids[0].len()), (2, s1.as_str(), 3));
    assert!(a.hashes.contains_key(&s1));
    assert_eq!(live.recv().await.unwrap().rev, 2);
    // The same guard again: their edit wins, nothing in the batch lands.
    let both = json!({"ops": [
        {"op": "insert", "markdown": "Never lands.", "after": s2},
        {"op": "replace", "id": s1, "markdown": "## Mine", "ifRev": 1, "ifHash": hash_of(&s1)},
    ]});
    let r = send(M::POST, format!("docs/{id}/ops"), both).await;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let e: Value = r.json().await.unwrap();
    assert!(e["error"].as_str().unwrap().contains("guard_mismatch"), "{e}");
    assert_eq!(get(format!("docs/{id}")).await["rev"], 2);
    // No guard on a whole-block replace: refused (a pending block's own
    // writer fills it without one).
    let r = send(M::POST, format!("docs/{id}/ops"), json!({"ops": [{"op": "replace", "id": blocks[0]["id"], "markdown": "# x"}]})).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    // Words found and replaced; a find that no longer matches is refused.
    let para = blocks[1]["id"].as_str().unwrap().to_owned();
    let words = |f: &str| json!({"ops": [{"op": "replace", "id": para, "find": f, "with": "We ship realms to everyone"}]});
    ok(send(M::POST, format!("docs/{id}/ops"), words("We ship realms to the team")).await).await;
    assert_eq!(send(M::POST, format!("docs/{id}/ops"), words("the team")).await.status(), StatusCode::CONFLICT);

    // What changed since version 1: the filled section and the paragraph.
    let since = get(format!("docs/{id}/read?view=since&rev=1")).await;
    let changed: Vec<&str> = since["changed"].as_array().unwrap().iter().map(|b| b["id"].as_str().unwrap()).collect();
    assert!(changed.contains(&s1.as_str()) && changed.contains(&para.as_str()) && changed.len() == 4, "{since}");
    let block = get(format!("docs/{id}/read?view=block&id={para}")).await;
    assert_eq!(block["markdown"], "We ship realms to everyone.");
    let hits = get(format!("docs/{id}/read?view=search&q=home%20realm")).await;
    assert_eq!(hits["hits"].as_array().unwrap().len(), 1, "{hits}");

    // History: who did what; any version reads as it was; restore one block.
    let h: Vec<DocVersion> = serde_json::from_value(get(format!("docs/{id}/history")).await).unwrap();
    assert_eq!(h.iter().map(|v| v.rev).collect::<Vec<_>>(), [3, 2, 1]);
    assert_eq!((h[1].summary.as_str(), h[1].added.len(), h[1].changed.len()), ("2 blocks added, 1 block changed", 2, 1));
    let old = get(format!("docs/{id}/read?view=markdown&at=1")).await;
    assert!(old["tabs"][0]["markdown"].as_str().unwrap().contains("to the team"), "{old}");
    assert_eq!(old["blocks"][&s1]["type"], "pending");
    ok(send(M::POST, format!("docs/{id}/restore"), json!({"rev": 1, "block": para})).await).await;
    let now = get(format!("docs/{id}/read?view=block&id={para}")).await;
    assert_eq!((now["markdown"].as_str(), now["by"].as_str()), (Some("We ship realms to the team."), Some("you")));

    // Many versions: snapshots keep any version readable.
    for n in 0..60 {
        let b = get(format!("docs/{id}/read?view=block&id={para}")).await;
        let op = json!({"ops": [{"op": "replace", "id": para, "markdown": format!("Version {n}."), "ifRev": b["rev_doc"], "ifHash": b["hash"]}]});
        ok(send(M::POST, format!("docs/{id}/ops"), op).await).await;
    }
    let v30 = get(format!("docs/{id}/read?view=block&id={para}&at=33")).await;
    assert_eq!(v30["markdown"], "Version 28.");

    // The trigger fires once the changes settle, with who wrote them.
    eventually("the settled change's record", async || {
        let recs = get("collections/log/records".into()).await;
        recs["records"].as_array()?.iter().any(|r| r["fields"]["what"] == "Realms for the team by [\"you\"]").then_some(())
    })
    .await;

    // A deck: slides with layouts, notes and regions.
    let deck = ok(send(
        M::POST,
        "docs".into(),
        json!({"kind": "deck", "title": "Q3 review",
               "markdown": "# Q3 review\n\nWhere we are\n\n<!-- notes: Open with the headline. -->\n\n---\n\n<!-- layout: two-columns -->\n## Before and after\n<!-- region: left -->\n- slow\n<!-- region: right -->\n- fast\n\n---\n\n<?c6 block n?>",
               "blocks": {"n": {"type": "pending", "intent": "Next steps"}}}),
    )
    .await)
    .await;
    let did = deck["id"].as_str().unwrap().to_owned();
    assert_eq!((deck["theme"].as_str(), deck["size"].as_str()), (Some("Paper"), Some("16:9")));
    let slides = deck["tabs"][0]["blocks"].as_array().unwrap();
    assert_eq!(slides.iter().map(|b| b["kind"].as_str().unwrap()).collect::<Vec<_>>(), ["slide", "slide", "pending"]);
    assert_eq!(
        slides[1]["children"].as_array().unwrap().iter().map(|c| c["region"].as_str().unwrap()).collect::<Vec<_>>(),
        ["title", "left", "right"]
    );
    let bullet = slides[1]["children"][1]["id"].as_str().unwrap().to_owned();
    // A bullet on slide 2 is edited like a paragraph.
    let b = get(format!("docs/{did}/read?view=block&id={bullet}")).await;
    ok(send(
        M::POST,
        format!("docs/{did}/ops"),
        json!({"ops": [{"op": "replace", "id": bullet, "markdown": "- slower", "ifRev": 1, "ifHash": b["hash"]}]}),
    )
    .await)
    .await;
    let s = get(format!("docs/{did}/read?view=slide&id={}", slides[1]["id"].as_str().unwrap())).await;
    assert_eq!(s["markdown"], "<!-- layout: two-columns -->\n\n## Before and after\n\n- slower\n\n<!-- region: right -->\n- fast");
    // Slides go in among slides; a doc takes no theme.
    ok(send(
        M::POST,
        format!("docs/{did}/ops"),
        json!({"ops": [{"op": "insert", "markdown": "<!-- layout: section -->\n# Part 2", "after": slides[0]["id"]}]}),
    )
    .await)
    .await;
    assert_eq!(get(format!("docs/{did}/read?view=outline")).await["tabs"][0]["blocks"][1]["layout"], "section");
    let r = send(M::POST, format!("docs/{id}/ops"), json!({"ops": [{"op": "set", "theme": "Night"}]})).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);

    // Listed, filtered, renamed, deleted.
    let decks = get("docs?kind=deck".into()).await;
    assert_eq!(decks.as_array().unwrap().len(), 1);
    assert_eq!(get("docs?label=realms".into()).await[0]["id"], id.as_str());
    ok(send(M::PATCH, format!("docs/{did}"), json!({"title": "Q3 business review"})).await).await;
    assert_eq!(get(format!("docs/{did}")).await["title"], "Q3 business review");
    assert_eq!(send(M::DELETE, format!("docs/{did}"), Value::Null).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(get("docs".into()).await.as_array().unwrap().len(), 1);
    assert_eq!(get("docs?by=you".into()).await.as_array().unwrap().len(), 1);
    assert_eq!(get("docs?by=session".into()).await.as_array().unwrap().len(), 0);
    // Work refers to a document by doc:<id>, with its title as it is now.
    let w = ok(send(M::POST, "work".into(), json!({"title": "Ship realms", "attach": [{"ref": format!("doc:{id}")}]})).await).await;
    assert_eq!(w["live"][0]["title"], "Realms for the team");
    let held = get(format!("work/of?ref=doc:{id}")).await;
    assert_eq!(held[0]["id"], w["id"]);
    ok(send(M::POST, format!("work/{}/attach", w["id"].as_str().unwrap()), json!({"ref": format!("doc:{did}")})).await).await;
    let w = get(format!("work/{}", w["id"].as_str().unwrap())).await;
    assert_eq!(w["live"][1]["state"], "gone", "a deleted document");
    // Root keeps no documents.
    assert_eq!(get("docs?realm=root".into()).await["error"], "documents live in a realm: root keeps none");
}

/// A session writes documents with the `doc` tool: the birth with a
/// pending section, filling it, and editing another document only once you
/// allow it.
#[tokio::test]
async fn sessions_write_documents_with_the_doc_tool() {
    use c6_testkit::{MockLlm, Reply};
    use c6_worker::{ConnState, Identity, Worker, conn, enroll};
    let mock = MockLlm::start().await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = tokio::sync::watch::channel(ConnState::Connecting);
    let stop = tokio_util::sync::CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let send = |method: M, path: String, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.request(method, &url).json(&body).send().await.unwrap() }
    };
    let get = |path: String| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.get(&url).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    // Only the session calls the model here.
    let r =
        send(M::PUT, "settings/work".into(), json!({"guess": false, "notes": false, "quiet_days": 3, "nudge": false, "needs_hours": 4}))
            .await;
    assert!(r.status().is_success());
    let conn: Value = send(M::POST, "connections".into(), json!({"name": "m", "base_url": mock.base_url()})).await.json().await.unwrap();
    let turns = |id: String, n: usize| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            eventually("the turn to end", async || {
                let d: Value = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
                let ev: Vec<Value> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
                let ended = ev.iter().filter(|e| e["type"] == "turn_ended").count();
                (d["session"]["status"] == "idle" && ended >= n).then_some(ev)
            })
            .await
        }
    };
    let results = |ev: &[Value]| -> Vec<String> {
        ev.iter().filter(|e| e["type"] == "tool_result").map(|e| e["output"].as_str().unwrap_or("").to_owned()).collect()
    };

    // 1. The birth: a title and a pending section, in a session of a work item.
    let create = json!({"action": "create", "title": "Rollout plan", "markdown": "# Rollout plan\n\n<?c6 block s1?>",
                        "blocks": {"s1": {"type": "pending", "intent": "Phases: what ships when"}}});
    mock.push(Reply::ToolCalls(vec![("doc".into(), json!({"action": "guide"}).to_string()), ("doc".into(), create.to_string())]));
    mock.push(Reply::Text("Started doc.".into()));
    let s: Value = send(
        M::POST,
        "sessions".into(),
        json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": conn["id"], "model": "m",
               "prompt": "plan the rollout", "work": "new"}),
    )
    .await
    .json()
    .await
    .unwrap();
    let sid = s["id"].as_str().unwrap().to_owned();
    let ev = turns(sid.clone(), 1).await;
    let out = results(&ev);
    assert!(out[0].contains("Birth first"), "the guide is docs.style: {}", out[0]);
    let made: Value = serde_json::from_str(&out[1]).unwrap_or_else(|_| panic!("{}", out[1]));
    let id = made["doc"]["id"].as_str().unwrap().to_owned();
    let pending = made["keys"]["s1"].as_str().unwrap().to_owned();
    assert_eq!(made["pending"][0]["id"], pending.as_str());
    assert_eq!(made["pending"][0]["intent"], "Phases: what ships when");
    assert!(made["blocks"].as_array().unwrap().iter().any(|b| b["kind"] == "heading" && b["words"] == "Rollout plan"), "{made}");
    let doc = get(format!("docs/{id}")).await;
    assert_eq!(doc["created_by"], format!("session:{sid}"));
    assert_eq!(doc["writers"][0], format!("session:{sid}"), "it shows who is writing the pending section");
    // It belongs to the session's work.
    let w = get(format!("work/of?ref=session:{sid}")).await;
    assert!(w[0]["attached"].as_array().unwrap().iter().any(|a| a["ref"] == format!("doc:{id}")), "{w}");

    // 2. Filling its own pending section needs no guard (`block` names it,
    // as models often write it).
    let fill =
        json!({"action": "edit", "id": id, "ops": [{"op": "replace", "block": pending, "markdown": "## Phases\n\n1. Staff\n2. Everyone"}]});
    mock.push(Reply::ToolCalls(vec![("doc".into(), fill.to_string())]));
    mock.push(Reply::Text("Filled.".into()));
    let r = send(M::POST, format!("sessions/{sid}/messages"), json!({"text": "fill it in"})).await;
    assert!(r.status().is_success());
    turns(sid.clone(), 2).await;
    let md = get(format!("docs/{id}/read?view=markdown")).await;
    assert_eq!(md["tabs"][0]["markdown"], "# Rollout plan\n\n## Phases\n\n1. Staff\n2. Everyone");
    let h = get(format!("docs/{id}/history")).await;
    assert_eq!(h[0]["by"], format!("session:{sid}"));

    // 3. Your document isn't its work's: the first edit asks you, nothing changes.
    let yours: Value = send(M::POST, "docs".into(), json!({"title": "Team handbook", "markdown": "Be kind."})).await.json().await.unwrap();
    let yid = yours["id"].as_str().unwrap().to_owned();
    let words = json!({"action": "edit", "id": format!("doc:{yid}"), "ops": [{"op": "replace", "id": yours["tabs"][0]["blocks"][0]["id"], "find": "kind", "with": "kind and brief"}]});
    mock.push(Reply::ToolCalls(vec![("doc".into(), words.to_string())]));
    mock.push(Reply::Text("Asked.".into()));
    send(M::POST, format!("sessions/{sid}/messages"), json!({"text": "edit the handbook"})).await;
    let ev = turns(sid.clone(), 3).await;
    let out = results(&ev);
    assert!(out.last().unwrap().contains("the user is asked to let you edit it"), "{out:?}");
    assert_eq!(get(format!("docs/{yid}")).await["rev"], 1);
    let ds = get("decisions".into()).await;
    let d = ds.as_array().unwrap().iter().find(|d| d["title"].as_str().unwrap_or("").contains("Team handbook")).unwrap().clone();
    let r = send(M::POST, format!("decisions/{}/answer", d["id"]), json!({"option": "Allow"})).await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    // Allowed: the same edit lands.
    mock.push(Reply::ToolCalls(vec![("doc".into(), words.to_string())]));
    mock.push(Reply::Text("Done.".into()));
    send(M::POST, format!("sessions/{sid}/messages"), json!({"text": "try again"})).await;
    turns(sid.clone(), 4).await;
    let b = get(format!("docs/{yid}/read?view=markdown")).await;
    assert_eq!(b["tabs"][0]["markdown"], "Be kind and brief.");
    stop.cancel();
}

/// Deck themes are the realm's records (Paper, Night and Signal to begin
/// with), editable; a session making a deck reads the decks.* prompts and
/// the themes in its guide.
#[tokio::test]
async fn deck_themes_are_records_and_sessions_read_the_deck_guide() {
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
    let themes: Vec<c6_api::slides::DeckTheme> = http.get(format!("{api}/decks/themes")).send().await.unwrap().json().await.unwrap();
    assert_eq!(themes.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["Paper", "Night", "Signal"]);
    // They're records of deck_themes, with history.
    let recs: Value = http.get(format!("{api}/collections/deck_themes/records")).send().await.unwrap().json().await.unwrap();
    assert_eq!(recs["records"].as_array().unwrap().len(), 3);
    // Edit one, add one, drop one.
    let mut t = themes.clone();
    t[0].css = ":root { --bg: #fff; --fg: #000; --accent: #c00; }".into();
    t.remove(1);
    t.push(c6_api::slides::DeckTheme { name: "Brand".into(), mood: "Ours".into(), css: ":root { --accent: #6d28d9; }".into() });
    let r = http.put(format!("{api}/decks/themes")).json(&t).send().await.unwrap();
    assert!(r.status().is_success());
    let now: Vec<c6_api::slides::DeckTheme> = http.get(format!("{api}/decks/themes")).send().await.unwrap().json().await.unwrap();
    assert_eq!(now.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["Paper", "Signal", "Brand"]);
    assert!(now[0].css.contains("#c00"));
    for bad in [
        json!([]),
        json!([{"name": "A", "css": ""}]),
        json!([{"name": "A", "css": "x"}, {"name": "a", "css": "y"}]),
        json!([{"name": "A", "css": "</style><script>"}]),
    ] {
        let r = http.put(format!("{api}/decks/themes")).json(&bad).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    assert!(c6_head::decks::theme_css(&head.state, "home", "brand").await.contains("#6d28d9"));
    assert!(c6_head::decks::theme_css(&head.state, "home", "gone").await.contains("#c00"), "the first theme otherwise");
    let guide = c6_head::decks::guide(&head.state, "home").await;
    assert!(guide.contains("## decks.flow") && guide.contains("## decks.review") && guide.contains("- Brand: Ours"), "{guide}");
}

/// Export and import (docs/docs.md §10): Markdown and HTML from the head;
/// PDF, Word, OpenDocument, PowerPoint and pictures from the render kit
/// (here a stand-in that records what it was given: the real kit runs in a
/// container, built on a worker); every export kept with its version.
#[tokio::test]
async fn documents_export_and_import() {
    use std::sync::{Arc, Mutex};
    // Each run: its tool, its command, and the deck.json it was given.
    type Runs = Arc<Mutex<Vec<(String, Vec<String>, Value)>>>;
    let runs: Runs = Arc::default();
    let seen = runs.clone();
    c6_head::kitjobs::run_locally(
        c6_head::docs::export::KIT,
        Arc::new(move |run: c6_head::kitjobs::LocalRun| {
            let deck = run.inputs.get("deck.json").map(|d| serde_json::from_slice::<Value>(d).unwrap()).unwrap_or(Value::Null);
            seen.lock().unwrap().push((run.tool.clone(), run.command.clone(), deck));
            Box::pin(async move {
                let mut files = std::collections::BTreeMap::new();
                match run.tool.as_str() {
                    "slides" => {
                        let n = run.inputs["deck.html"].windows(8).filter(|w| w == b"<section").count();
                        let arg = |i: usize| run.command.get(i).and_then(|a| a.parse::<usize>().ok()).filter(|n| *n > 0);
                        let (first, last) = (arg(5).unwrap_or(1), arg(6).or(arg(5)).unwrap_or(n));
                        for i in (1..=n).filter(|i| (first..=last).contains(i)) {
                            files.insert(format!("slides/slide-{i:02}.png"), format!("png {i}").into_bytes());
                        }
                    }
                    "import" => {
                        files.insert("doc.md".into(), b"# From Word\n\nHello from the file.".to_vec());
                    }
                    t => {
                        files.insert(
                            format!("file.{t}"),
                            format!("{t} of {} bytes", run.inputs.values().map(Vec::len).sum::<usize>()).into_bytes(),
                        );
                    }
                }
                Ok(c6_head::kitjobs::Output { exit: Some(0), files, ..Default::default() })
            })
        }),
    );
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
    let post = |path: String, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move {
            let r = http.post(&url).json(&body).send().await.unwrap();
            let s = r.status();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            assert!(s.is_success(), "{s}: {v}");
            v
        }
    };
    let bytes = |url: String| {
        let (http, base) = (http.clone(), api.trim_end_matches("/api/v1").to_owned());
        async move {
            let r = http.get(format!("{base}{url}")).send().await.unwrap();
            assert!(r.status().is_success());
            let ct = r.headers()["content-type"].to_str().unwrap().to_owned();
            (ct, r.bytes().await.unwrap().to_vec())
        }
    };
    let doc = post(
        "docs".into(),
        json!({"title": "Launch plan", "markdown": "# Launch plan\n\nDue <?c6 block d?>.\n\n<?c6 block p?>\n\n<div onclick=\"x()\">raw</div>",
               "blocks": {"d": {"type": "date", "value": "2026-11-15"}, "p": {"type": "pending", "intent": "Risks"}}}),
    )
    .await;
    let id = doc["id"].as_str().unwrap().to_owned();

    // Markdown and HTML: the head's own, chips as text, pending blocks left out.
    let md = post(format!("docs/{id}/export"), json!({"format": "md"})).await;
    assert_eq!((md["name"].as_str(), md["rev"].as_i64()), (Some("Launch plan.md"), Some(1)));
    let (ct, body) = bytes(md["url"].as_str().unwrap().to_owned()).await;
    assert!(ct.starts_with("text/markdown"));
    let text = String::from_utf8(body).unwrap();
    assert!(text.contains("Due 2026-11-15.") && !text.contains("<?c6") && !text.contains("Risks"), "{text}");
    let html = post(format!("docs/{id}/export"), json!({"format": "html"})).await;
    let (_, body) = bytes(html["url"].as_str().unwrap().to_owned()).await;
    let page = String::from_utf8(body).unwrap();
    assert!(page.contains("<h1>Launch plan</h1>") && page.contains("raw") && !page.contains("onclick"), "{page}");
    assert!(runs.lock().unwrap().is_empty(), "no kit for Markdown and HTML");
    // Word: the kit, given the Markdown.
    let docx = post(format!("docs/{id}/export"), json!({"format": "docx"})).await;
    let (ct, body) = bytes(docx["url"].as_str().unwrap().to_owned()).await;
    assert!(ct.contains("wordprocessingml"), "{ct}");
    assert!(String::from_utf8(body).unwrap().starts_with("docx of"));
    assert_eq!(runs.lock().unwrap()[0].1, ["python3", "/opt/kit/render.py", "docx", "/work/doc.md", "/work/out/file.docx"]);
    let r = http.post(format!("{api}/docs/{id}/export")).json(&json!({"format": "pptx"})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST, "a doc isn't a deck");

    // A deck: PowerPoint from deck.json, its html slides as pictures.
    let deck = post(
        "docs".into(),
        json!({"kind": "deck", "title": "Q3", "markdown": "# Q3\n\n---\n\n## Numbers\n\n- up <?c6 block d?>\n\n<!-- notes: say it -->\n\n---\n\n<!-- layout: html -->\n<svg></svg>\n\n---\n\n<!-- hidden -->\n## Spare",
               "blocks": {"d": {"type": "date", "value": "Nov 1"}}}),
    )
    .await;
    let did = deck["id"].as_str().unwrap().to_owned();
    post(format!("docs/{did}/export"), json!({"format": "pptx"})).await;
    let (tool, _, sent) = runs.lock().unwrap().last().cloned().unwrap();
    assert_eq!(tool, "pptx");
    let slides = sent["slides"].as_array().unwrap();
    assert_eq!(slides.len(), 3, "hidden slides stay out: {sent}");
    assert_eq!((slides[1]["regions"]["body"].as_str(), slides[1]["notes"].as_str()), (Some("- up Nov 1"), Some("say it")));
    assert_eq!((slides[1]["picture"].as_bool(), slides[2]["picture"].as_bool()), (Some(false), Some(true)));
    // Pictures of every slide, as a zip.
    let png = post(format!("docs/{did}/export"), json!({"format": "png"})).await;
    let (ct, zip) = bytes(png["url"].as_str().unwrap().to_owned()).await;
    assert_eq!((ct.as_str(), &zip[..4]), ("application/zip", &b"PK\x03\x04"[..]));
    // Past exports, newest first.
    let past: Vec<Value> = http.get(format!("{api}/docs/{id}/exports")).send().await.unwrap().json().await.unwrap();
    assert_eq!(past.iter().map(|e| e["format"].as_str().unwrap()).collect::<Vec<_>>(), ["docx", "html", "md"]);

    // An agent looks at slides: pictures for the model (hidden ones count).
    let looked = c6_head::docs::export::pictures(&head.state, "home", &did, 2, 3).await.unwrap();
    assert_eq!(looked, [b"png 2".to_vec(), b"png 3".to_vec()]);
    let (_, cmd, _) = runs.lock().unwrap().last().cloned().unwrap();
    assert_eq!(cmd[5..], ["2".to_owned(), "3".to_owned()]);

    // Google Docs and Slides (docs/docs.md §10), against a stand-in Google:
    // a sign-in with drive.file only, uploads converted, the file
    // remembered, updates that say when Google's copy changed.
    type Calls = Arc<Mutex<Vec<(String, String, String, Vec<u8>)>>>;
    let calls: Calls = Arc::default();
    let modified = Arc::new(Mutex::new(String::from("2026-10-31T10:00:00.000Z")));
    let google = {
        use axum::extract::{Path as P, RawQuery};
        use axum::http::HeaderMap;
        use axum::routing::{get, patch, post as on_post};
        let (c1, c2, c3) = (calls.clone(), calls.clone(), calls.clone());
        let (m1, m2, m3) = (modified.clone(), modified.clone(), modified.clone());
        axum::Router::new()
            .route(
                "/token",
                on_post(|axum::extract::Form(f): axum::extract::Form<std::collections::HashMap<String, String>>| async move {
                    assert_eq!(f["client_secret"], "s3cret");
                    axum::Json(json!({"access_token": "drive-token", "refresh_token": "r1", "expires_in": 3600,
                                      "scope": "openid email profile https://www.googleapis.com/auth/drive.file"}))
                }),
            )
            .route("/me", get(|| async { axum::Json(json!({"email": "ada@example.com", "name": "Ada"})) }))
            .route(
                "/upload/drive/v3/files",
                on_post(move |q: RawQuery, h: HeaderMap, body: axum::body::Bytes| {
                    let (c, m) = (c1.clone(), m1.clone());
                    async move {
                        let auth = h["authorization"].to_str().unwrap().to_owned();
                        let ct = h["content-type"].to_str().unwrap().to_owned();
                        c.lock().unwrap().push((format!("POST ?{}", q.0.unwrap_or_default()), auth, ct, body.to_vec()));
                        let n = c.lock().unwrap().len();
                        axum::Json(json!({"id": format!("g{n}"), "webViewLink": format!("https://docs.google.com/document/d/g{n}/edit"),
                                          "modifiedTime": *m.lock().unwrap()}))
                    }
                }),
            )
            .route(
                "/upload/drive/v3/files/{id}",
                patch(move |P(id): P<String>, q: RawQuery, h: HeaderMap, body: axum::body::Bytes| {
                    let (c, m) = (c2.clone(), m2.clone());
                    async move {
                        let ct = h["content-type"].to_str().unwrap().to_owned();
                        c.lock().unwrap().push((format!("PATCH {id} ?{}", q.0.unwrap_or_default()), String::new(), ct, body.to_vec()));
                        *m.lock().unwrap() = "2026-10-31T11:00:00.000Z".into();
                        axum::Json(json!({"id": id, "webViewLink": format!("https://docs.google.com/document/d/{id}/edit"),
                                          "modifiedTime": *m.lock().unwrap()}))
                    }
                }),
            )
            .route(
                "/drive/v3/files/{id}",
                get(move |P(id): P<String>| {
                    let (c, m) = (c3.clone(), m3.clone());
                    async move {
                        c.lock().unwrap().push((format!("GET {id}"), String::new(), String::new(), Vec::new()));
                        axum::Json(json!({"id": id, "modifiedTime": *m.lock().unwrap(), "trashed": false}))
                    }
                }),
            )
    };
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gbase = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, google).await });
    head.state
        .store
        .set_setting(
            "google_endpoints",
            &json!({"authorize_url": format!("{gbase}/auth"), "token_url": format!("{gbase}/token"),
                    "userinfo_url": format!("{gbase}/me"), "api_url": gbase})
            .to_string(),
        )
        .await
        .unwrap();
    // No account yet: it says where to connect one.
    let r = http.post(format!("{api}/docs/{id}/google")).json(&json!({})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(r.text().await.unwrap().contains("Connect a Google account"));
    for (k, v) in [("GOOGLE_CLIENT_ID", "client-1"), ("GOOGLE_CLIENT_SECRET", "s3cret")] {
        let r = http.put(format!("{api}/settings/secrets/{k}")).json(&json!({"value": v})).send().await.unwrap();
        assert!(r.status().is_success());
    }
    let start = post("docs/google/connect".into(), json!({"back": format!("/docs/{id}")})).await;
    let url = reqwest::Url::parse(start["authorize_url"].as_str().unwrap()).unwrap();
    let q: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert!(url.as_str().starts_with(&format!("{gbase}/auth")));
    assert_eq!(q["scope"], "openid email profile https://www.googleapis.com/auth/drive.file", "drive.file only");
    let quiet = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
    let back =
        quiet.get(format!("{}/auth/google/callback?code=c&state={}", api.trim_end_matches("/api/v1"), q["state"])).send().await.unwrap();
    let to = back.headers()["location"].to_str().unwrap().to_owned();
    assert!(to.starts_with(&format!("/docs/{id}?oauth=connected")), "{to}");
    let g: Value = http.get(format!("{api}/docs/{id}/google")).send().await.unwrap().json().await.unwrap();
    assert_eq!((g["accounts"][0].as_str(), g["file"].is_null(), g["app"].as_str()), (Some("ada@example.com"), true, Some("Google Docs")));

    // First export: the kit's Word file, uploaded converted to a Google Doc.
    let f = post(format!("docs/{id}/google"), json!({})).await;
    assert_eq!((f["id"].as_str(), f["account"].as_str(), f["rev"].as_i64()), (Some("g1"), Some("ada@example.com"), Some(1)));
    assert_eq!(f["url"], "https://docs.google.com/document/d/g1/edit");
    {
        let c = calls.lock().unwrap();
        let (what, auth, ct, body) = &c[0];
        assert!(what.starts_with("POST ?uploadType=multipart"), "{what}");
        assert_eq!(auth, "Bearer drive-token");
        assert!(ct.starts_with("multipart/related; boundary="), "{ct}");
        let body = String::from_utf8_lossy(body);
        assert!(
            body.contains(r#""mimeType":"application/vnd.google-apps.document""#) && body.contains(r#""name":"Launch plan""#),
            "{body}"
        );
        assert!(body.contains("wordprocessingml") && body.contains("docx of"), "{body}");
    }
    let g: Value = http.get(format!("{api}/docs/{id}/google")).send().await.unwrap().json().await.unwrap();
    assert_eq!(g["file"]["id"], "g1", "the document remembers its Google file");
    // Again: the same file, updated.
    let f = post(format!("docs/{id}/google"), json!({})).await;
    assert_eq!(f["id"], "g1");
    {
        let c = calls.lock().unwrap();
        assert_eq!(c[1].0, "GET g1");
        assert!(c[2].0.starts_with("PATCH g1 ?uploadType=media"), "{}", c[2].0);
        assert!(c[2].2.contains("wordprocessingml"));
    }
    // Changed in Google since: it says so, and updates only when you say.
    *modified.lock().unwrap() = "2026-10-31T12:00:00.000Z".into();
    let r = http.post(format!("{api}/docs/{id}/google")).json(&json!({})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
    assert!(r.text().await.unwrap().contains("changed in Google since the last export"));
    let n = calls.lock().unwrap().len();
    post(format!("docs/{id}/google"), json!({"anyway": true})).await;
    assert!(calls.lock().unwrap()[n + 1].0.starts_with("PATCH g1"));
    // Or a new one; a deck becomes Google Slides from PowerPoint.
    let f = post(format!("docs/{id}/google"), json!({"new": true})).await;
    assert_ne!(f["id"], "g1");
    let f = post(format!("docs/{did}/google"), json!({})).await;
    let c = calls.lock().unwrap().last().cloned().unwrap();
    let body = String::from_utf8_lossy(&c.3);
    assert!(body.contains("application/vnd.google-apps.presentation") && body.contains("pptx of"), "{body}");
    assert!(f["id"].is_string());

    // Import: Markdown read here; Word through the kit.
    use base64::Engine;
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let m = post("docs/import".into(), json!({"name": "notes.md", "content": b64(b"# Notes\n\n- one\n- <?c6 block x?>")})).await;
    assert_eq!((m["title"].as_str(), m["kind"].as_str()), (Some("notes"), Some("doc")));
    assert!(m["tabs"][0]["blocks"][2]["text"].as_str().unwrap().contains("&lt;?c6 block x?>"), "a file's text, not tokens: {m}");
    let d = post("docs/import".into(), json!({"name": "Brief.docx", "content": b64(b"PK fake docx")})).await;
    assert_eq!((d["title"].as_str(), d["tabs"][0]["blocks"][1]["text"].as_str()), (Some("Brief"), Some("Hello from the file.")));
    assert_eq!(runs.lock().unwrap().last().unwrap().0, "import");
    let p = post("docs/import".into(), json!({"name": "Deck.pptx", "content": b64(b"PK fake")})).await;
    assert_eq!(p["kind"], "deck");
    let r = http.post(format!("{api}/docs/import")).json(&json!({"name": "x.exe", "content": b64(b"MZ")})).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
}

/// Leaving the decks app (docs/docs.md §13): its decks come in once as
/// carbon6 decks (html slides from its deck.html on a worker, split by the
/// render kit, here a stand-in; its brief as notes; its PDF a past export),
/// and its themes as themes.
#[tokio::test]
async fn the_decks_apps_decks_come_in_once() {
    use c6_worker::{ConnState, Identity, Worker, conn, enroll};
    // A decks app as it was installed (its decks and themes collections).
    const APP: &str = r#"{"package": "decks", "version": "1.2.0", "description": "Presentation decks from a brief: an outline you approve, a theme you choose, then slides built as HTML, checked by eye, and handed to you as HTML and PDF. Install it in the realm whose decks they are (home, a business).", "permissions": {}, "contents": {"collections": [{"name": "decks", "description": "Finished decks: what they are about, and their files. Handed to you by the decks app; yours to keep and change.", "handoff": true, "schema": {"type": "object", "required": ["title", "files"], "properties": {"title": {"type": "string"}, "brief": {"type": "string"}, "audience": {"type": "string"}, "theme": {"type": "string"}, "folder": {"type": "string", "description": "Where its files are made, on the worker"}, "files": {"type": "array", "items": {"type": "string"}, "description": "The published files: HTML, PDF, and PowerPoint when asked"}, "made_by": {"type": "string", "description": "What made it, e.g. decks 1.2.0"}}}, "card": {"sources": "Handed off by the decks app when a deck is delivered.", "freshness": "once per deck"}}, {"name": "themes", "description": "Looks for decks: colours, type and accents as CSS variables over the base CSS (skill deck-html).", "schema": {"type": "object", "required": ["name", "css"], "properties": {"name": {"type": "string"}, "mood": {"type": "string"}, "css": {"type": "string"}}}, "keys": ["name"], "card": {"sources": "Shipped with the decks app; add your own with its add_theme operation.", "invariants": ["each theme's name is unique", "css sets the CSS variables the base CSS uses"]}}], "records": {"themes": [{"name": "Old Paper", "mood": "The app's", "css": ":root { --bg: #fffdf8; --accent: #8a5a00; }"}]}}}"#;
    c6_head::kitjobs::run_locally(
        c6_head::docs::export::KIT,
        std::sync::Arc::new(|run: c6_head::kitjobs::LocalRun| {
            Box::pin(async move {
                assert_eq!(run.tool, "split");
                let html = String::from_utf8(run.inputs["deck.html"].clone()).unwrap();
                let cut = |open: &str, close: &str| -> Vec<String> {
                    html.split(open).skip(1).filter_map(|r| r.split_once(close).map(|(a, _)| a.to_owned())).collect()
                };
                let style = cut("<style>", "</style>").join("\n");
                let sections: Vec<String> =
                    cut("<section class=\"slide", "</section>").iter().map(|s| format!("<section class=\"slide{s}</section>")).collect();
                let out = serde_json::to_vec(&json!({"style": style, "sections": sections})).unwrap();
                Ok(c6_head::kitjobs::Output { exit: Some(0), files: [("split.json".to_owned(), out)].into(), ..Default::default() })
            })
        }),
    );
    let (head_dir, state, folder) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = tokio::sync::watch::channel(ConnState::Connecting);
    let stop = tokio_util::sync::CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let send = |method: M, path: String, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move {
            let r = http.request(method, &url).json(&body).send().await.unwrap();
            let s = r.status();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            assert!(s.is_success(), "{s}: {v}");
            v
        }
    };
    // Nothing to bring in without the app.
    assert_eq!(send(M::GET, "decks/bring-in".into(), Value::Null).await["app"], false);
    let i = send(M::POST, "apps?realm=home".into(), serde_json::from_str(APP).unwrap()).await;
    send(M::POST, format!("decisions/{}/answer", i["decision"]), json!({"option": "Install"})).await;
    // Its decks collection is yours (handed off), its themes its own.
    let cols = send(M::GET, "collections?realm=home".into(), Value::Null).await;
    let owner = |n: &str| cols.as_array().unwrap().iter().find(|c| c["name"] == n).map(|c| c["owner"].as_str().unwrap_or("").to_owned());
    assert_eq!((owner("decks"), owner("themes")), (Some(String::new()), Some("decks".into())));
    // A deck it made: deck.html and deck.pdf in a worker's folder.
    let dir = folder.path().join("decks/q3");
    std::fs::create_dir_all(&dir).unwrap();
    let html = "<!doctype html><html><head><style>.slide { width: 1280px; background: #faf7f2; }</style></head><body>\n\
                <section class=\"slide title\"><h1>Q3 review</h1>\n\n<p class=\"sub\">Where we are</p></section>\n\
                <section class=\"slide\"><h2>Churn halved</h2><svg viewBox=\"0 0 10 10\"><rect width=\"5\" height=\"5\"/></svg></section></body></html>";
    std::fs::write(dir.join("deck.html"), html).unwrap();
    std::fs::write(dir.join("deck.pdf"), b"%PDF-1.7 the app's").unwrap();
    let pdf = dir.join("deck.pdf").display().to_string();
    send(
        M::POST,
        "collections/decks/records?realm=home".into(),
        json!({"fields": {"title": "Q3 review", "brief": "For the board, ten minutes", "theme": "Old Paper", "folder": dir.display().to_string(),
                          "files": [pdf]}}),
    )
    .await;
    send(
        M::POST,
        "collections/decks/records?realm=home".into(),
        json!({"fields": {"title": "Lost deck", "folder": "/nowhere", "files": []}}),
    )
    .await;
    let s = send(M::GET, "decks/bring-in".into(), Value::Null).await;
    assert_eq!((s["app"].as_bool(), s["decks"].as_u64(), s["left"].as_u64()), (Some(true), Some(2), Some(2)));

    let r = send(M::POST, "decks/bring-in".into(), Value::Null).await;
    assert_eq!(r["made"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["themes"], json!(["Old Paper"]));
    assert!(r["skipped"][0]["why"].as_str().unwrap().contains("isn't on a connected worker"), "{r}");
    let id = r["made"][0]["doc"].as_str().unwrap().to_owned();
    let deck = send(M::GET, format!("docs/{id}"), Value::Null).await;
    assert_eq!(
        (deck["kind"].as_str(), deck["title"].as_str(), deck["theme"].as_str()),
        (Some("deck"), Some("Q3 review"), Some("Old Paper"))
    );
    let slides = deck["tabs"][0]["blocks"].as_array().unwrap();
    assert_eq!(slides.len(), 2);
    assert_eq!(slides[0]["attrs"]["layout"], "html");
    assert_eq!(slides[0]["attrs"]["notes"], "For the board, ten minutes");
    let first = &slides[0]["children"][0];
    assert_eq!(first["kind"], "html", "one html block, its blank lines out: {first}");
    assert!(first["text"].as_str().unwrap().starts_with("<style>.slide { width: 1280px"), "{first}");
    assert!(slides[1]["children"][0]["text"].as_str().unwrap().contains("<rect width=\"5\""));
    // Its PDF, as a past export.
    let past = send(M::GET, format!("docs/{id}/exports"), Value::Null).await;
    assert_eq!((past[0]["format"].as_str(), past[0]["by"].as_str()), (Some("pdf"), Some("decks app")));
    // Its theme came in.
    let themes = send(M::GET, "decks/themes".into(), Value::Null).await;
    assert!(themes.as_array().unwrap().iter().any(|t| t["name"] == "Old Paper"));
    // Again: nothing more comes in.
    let again = send(M::POST, "decks/bring-in".into(), Value::Null).await;
    assert!(again["made"].as_array().unwrap().is_empty());
    assert_eq!(again["skipped"][0]["why"], "brought in already");
    assert_eq!(send(M::GET, "decks/bring-in".into(), Value::Null).await["left"], 1, "the lost one is still left");
    stop.cancel();
}

/// Comments (docs/docs.md §8): a thread on a block, sent to the session
/// that wrote it (idle, so it gets the thread as a message by
/// `docs.comment`); it edits answering the thread and replies; the thread,
/// and the work holding the document, need you until you resolve it.
#[tokio::test]
async fn comments_go_to_the_agent_that_wrote_the_part() {
    use c6_testkit::{MockLlm, Reply};
    use c6_worker::{ConnState, Identity, Worker, conn, enroll};
    let mock = MockLlm::start().await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = tokio::sync::watch::channel(ConnState::Connecting);
    let stop = tokio_util::sync::CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let send = |method: M, path: String, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move {
            let r = http.request(method, &url).json(&body).send().await.unwrap();
            let s = r.status();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            assert!(s.is_success(), "{s}: {v}");
            v
        }
    };
    send(M::PUT, "settings/work".into(), json!({"guess": false, "notes": false, "quiet_days": 3, "nudge": false, "needs_hours": 4})).await;
    let conn = send(M::POST, "connections".into(), json!({"name": "m", "base_url": mock.base_url()})).await;
    let turns = |id: String, n: usize| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            eventually("the turn to end", async || {
                let d: Value = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
                let ev: Vec<Value> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
                let ended = ev.iter().filter(|e| e["type"] == "turn_ended").count();
                (d["session"]["status"] == "idle" && ended >= n).then_some(ev)
            })
            .await
        }
    };
    // The session writes a doc, in its work.
    let create = json!({"action": "create", "title": "Pricing", "markdown": "# Pricing\n\nWe charge per seat.\n\nAnnual plans get two months free."});
    mock.push(Reply::ToolCalls(vec![("doc".into(), create.to_string())]));
    mock.push(Reply::Text("Wrote it.".into()));
    let s = send(
        M::POST,
        "sessions".into(),
        json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": conn["id"], "model": "m",
               "prompt": "write the pricing doc", "work": "new"}),
    )
    .await;
    let sid = s["id"].as_str().unwrap().to_owned();
    turns(sid.clone(), 1).await;
    let doc = send(M::GET, "docs?q=Pricing".into(), Value::Null).await[0].clone();
    let id = doc["id"].as_str().unwrap().to_owned();
    let full = send(M::GET, format!("docs/{id}"), Value::Null).await;
    let para = full["tabs"][0]["blocks"][1]["id"].as_str().unwrap().to_owned();

    // You comment on words of it; a quote that isn't there is refused.
    let r = http
        .post(format!("{api}/docs/{id}/threads"))
        .json(&json!({"anchor": {"block": para, "quote": "per user"}, "text": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let t = send(
        M::POST,
        format!("docs/{id}/threads"),
        json!({"anchor": {"block": para, "quote": "per seat"}, "text": "Say the price: $12 a seat."}),
    )
    .await;
    let tid = t["id"].as_str().unwrap().to_owned();
    assert_eq!((t["about"].as_str(), t["needs_you"].as_bool(), t["detached"].as_bool()), (Some("“per seat”"), Some(false), Some(false)));
    // Sent to the agent: the session that wrote it, idle, gets the thread.
    let edit = json!({"action": "edit", "id": id, "answering": tid, "ops": [{"op": "replace", "id": para, "find": "per seat", "with": "$12 per seat a month"}]});
    let answer = json!({"action": "comment", "id": id, "op": "reply", "thread": tid, "text": "Done: it says $12 per seat a month."});
    let resolve = json!({"action": "comment", "id": id, "op": "resolve", "thread": tid});
    mock.push(Reply::ToolCalls(vec![
        ("doc".into(), edit.to_string()),
        ("doc".into(), answer.to_string()),
        ("doc".into(), resolve.to_string()),
    ]));
    mock.push(Reply::Text("Answered.".into()));
    send(M::POST, format!("docs/{id}/threads/{tid}/comments"), json!({"text": "Please do it.", "to": "agent"})).await;
    let ev = turns(sid.clone(), 2).await;
    let asked =
        mock.requests().into_iter().rev().find(|r| r["messages"].to_string().contains(&format!("Thread {tid}"))).expect("docs.comment");
    assert!(asked["messages"].to_string().contains("Say the price"), "the thread is in it");
    let outs: Vec<&str> = ev.iter().filter(|e| e["type"] == "tool_result").filter_map(|e| e["output"].as_str()).collect();
    assert!(outs.last().unwrap().contains("only the user resolves a thread they wrote"), "{outs:?}");
    let t = send(M::GET, format!("docs/{id}/threads"), Value::Null).await[0].clone();
    assert_eq!(t["comments"].as_array().unwrap().len(), 3);
    assert_eq!(t["comments"][2]["by"], format!("session:{sid}"));
    assert_eq!((t["to_agent"].as_bool(), t["agent"].as_str()), (Some(true), Some(sid.as_str())));
    assert_eq!((t["needs_you"].as_bool(), t["state"].as_str()), (Some(true), Some("open")));
    assert_eq!(t["edits"].as_array().unwrap().len(), 1, "its edit links to the thread");
    assert_eq!(t["detached"], false, "the words it quoted are still there");
    // Its work needs you, for the answered comment.
    let w = send(M::GET, format!("work/of?ref=session:{sid}"), Value::Null).await[0].clone();
    assert_eq!(w["state"], "needs_you", "{w}");
    assert!(w["why"].as_str().unwrap().contains("Pricing"), "{w}");
    // You resolve it: nothing waits.
    send(M::POST, format!("docs/{id}/threads/{tid}/resolve"), Value::Null).await;
    let t = send(M::GET, format!("docs/{id}/threads"), Value::Null).await[0].clone();
    assert_eq!((t["state"].as_str(), t["needs_you"].as_bool(), t["resolved_by"].as_str()), (Some("resolved"), Some(false), Some("you")));
    let w = send(M::GET, format!("work/of?ref=session:{sid}"), Value::Null).await[0].clone();
    assert!(!w["why"].as_str().unwrap().contains("Pricing"), "not for the comment now (its turn ended: your move): {w}");
    // A thread on a block that goes is detached, and stays.
    let t2 = send(M::POST, format!("docs/{id}/threads"), json!({"anchor": {"block": para}, "text": "Too long?"})).await;
    let b = send(M::GET, format!("docs/{id}/read?view=block&id={para}"), Value::Null).await;
    send(M::POST, format!("docs/{id}/ops"), json!({"ops": [{"op": "delete", "id": para, "ifRev": b["rev_doc"], "ifHash": b["hash"]}]}))
        .await;
    let all = send(M::GET, format!("docs/{id}/threads"), Value::Null).await;
    let t2 = all.as_array().unwrap().iter().find(|t| t["id"] == t2["id"]).unwrap().clone();
    assert_eq!((t2["detached"].as_bool(), t2["about"].as_str()), (Some(true), Some("“We charge $12 per seat a month.”")));
    stop.cancel();
}
