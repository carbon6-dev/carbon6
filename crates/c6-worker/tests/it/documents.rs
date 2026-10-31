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
    // No guard on a whole-block replace: refused.
    let r = send(M::POST, format!("docs/{id}/ops"), json!({"ops": [{"op": "replace", "id": s2, "markdown": "x"}]})).await;
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
    // Root keeps no documents.
    assert_eq!(get("docs?realm=root".into()).await["error"], "documents live in a realm: root keeps none");
}
