//! Archiving hides a document from discovery, not from its existing links.
//! It freezes every write path until unarchived; session cleanup is an
//! explicit selection from that session's family, never a cascading delete.

use std::collections::BTreeSet;

use c6_api::docs::DocBatch;
use c6_head::{Head, HeadConfig, store::SessionRow};
use c6_testkit::eventually;
use reqwest::{Method as M, StatusCode};
use serde_json::{Value, json};

struct Env {
    head: Head,
    http: reqwest::Client,
    api: String,
    _dir: tempfile::TempDir,
}

impl Env {
    async fn new() -> Self {
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
        Self { head, http: reqwest::Client::new(), api, _dir: dir }
    }

    async fn request(&self, method: M, path: &str, body: Value) -> (StatusCode, Value) {
        let r = self.http.request(method, format!("{}/{path}", self.api)).json(&body).send().await.unwrap();
        let status = r.status();
        let text = r.text().await.unwrap();
        let value = serde_json::from_str(&text).unwrap_or_else(|_| json!(text));
        (status, value)
    }

    async fn ok(&self, method: M, path: &str, body: Value) -> Value {
        let (status, value) = self.request(method.clone(), path, body).await;
        assert!(status.is_success(), "{method} {path}: {status}: {value}");
        value
    }

    async fn get(&self, path: &str) -> Value {
        self.ok(M::GET, path, Value::Null).await
    }

    async fn archive(&self, id: &str, archived: bool) -> Value {
        self.ok(M::PATCH, &format!("docs/{id}"), json!({"archived": archived})).await
    }

    async fn doc(&self, title: &str) -> String {
        let d = self.ok(M::POST, "docs", json!({"title": title, "markdown": "Keep this text.", "labels": ["cleanup"]})).await;
        d["id"].as_str().unwrap().to_owned()
    }

    /// No worker or LLM is needed: archive and document ownership consult
    /// the head's durable session rows, not a live worker process.
    async fn session(&self, id: &str, parent: Option<&str>, root: Option<&str>, realm: &str) {
        let row = SessionRow {
            id: id.into(),
            worker_id: c6_core::WorkerId::new().to_string(),
            title: id.into(),
            cwd: String::new(),
            connection_id: String::new(),
            model: "unused".into(),
            status: "idle".into(),
            error: None,
            effort: None,
            parent_id: parent.map(str::to_owned),
            parent_call_id: None,
            fork_seq: None,
            created_at: c6_core::now_ms(),
            updated_at: c6_core::now_ms(),
            last_seq: 0,
            archived: 0,
            labels: "[]".into(),
            worker_name: "archive-fixture".into(),
            repo: String::new(),
            folder: String::new(),
            home: String::new(),
            ssh_target: String::new(),
            awaiting: 0,
            subagent_model: String::new(),
            subagent_connection_id: String::new(),
            subagent_effort: String::new(),
            tools: "[]".into(),
            realm: realm.into(),
            agent: String::new(),
            end_reason: String::new(),
            root_id: root.map(str::to_owned),
            observed: false,
            instance: String::new(),
            colour: String::new(),
            remote_template: String::new(),
        };
        self.head.state.store.create_session(&row, &[], false).await.unwrap();
    }

    async fn session_doc(&self, session: &str, title: &str, kind: &str) -> String {
        let d = c6_head::docs::session::for_session(
            &self.head.state,
            session,
            &json!({"action": "create", "title": title, "kind": kind, "markdown": "Written by its session."}),
        )
        .await
        .unwrap();
        d["doc"]["id"].as_str().unwrap().to_owned()
    }

    async fn session_archived(&self, id: &str) -> bool {
        self.head.state.store.session(id).await.unwrap().unwrap().archived != 0
    }
}

fn ids(value: &Value) -> BTreeSet<String> {
    value.as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_owned()).collect()
}

#[tokio::test]
async fn archived_docs_leave_lists_and_memex_but_keep_content_files_and_refs() {
    let e = Env::new().await;
    let id = e.doc("Quokka archive tracker").await;
    // Several tabs must all leave search and all return when unarchived.
    e.ok(
        M::POST,
        &format!("docs/{id}/ops"),
        json!({"ops": [{"op": "add_tab", "title": "Appendix", "markdown": "Quokka archive appendix."}]}),
    )
    .await;
    let deck = e.ok(M::POST, "docs", json!({"kind": "deck", "title": "Live deck", "markdown": "# Still live"})).await;
    let live = deck["id"].as_str().unwrap();
    let file = e.ok(M::POST, &format!("docs/{id}/files"), json!({"name": "notes.txt", "content": "c2F2ZWQ="})).await;
    let export = e.ok(M::POST, &format!("docs/{id}/export"), json!({"format": "md"})).await;
    let work = e.ok(M::POST, "work", json!({"title": "Keep the reference", "attach": [{"ref": format!("doc:{id}")}]})).await;
    let project = e.ok(M::POST, "projects", json!({"title": "Keep the project reference", "plan": false})).await;
    e.ok(M::POST, &format!("projects/{}/docs", project["id"].as_str().unwrap()), json!({"doc": id, "role": "notes"})).await;
    let mirrored = |hit: &&Value| hit["source"] == "docs" && hit["external_id"].as_str().is_some_and(|s| s.starts_with(&format!("{id}#")));
    eventually("both document tabs mirrored", async || {
        let hits = e.get("memex/search?q=quokka%20archive").await;
        (hits.as_array()?.iter().filter(mirrored).count() == 2).then_some(())
    })
    .await;
    let before = e.get(&format!("docs/{id}")).await;
    let history = e.get(&format!("docs/{id}/history")).await;
    let archived = e.archive(&id, true).await;
    assert!(archived["archived_at"].is_i64(), "{archived}");
    assert_eq!(archived["rev"], before["rev"], "archiving is not a content edit");
    assert_eq!(archived["tabs"], before["tabs"]);
    assert_eq!(e.archive(&id, true).await["archived_at"], archived["archived_at"], "idempotent archive");
    assert_eq!(ids(&e.get("docs").await), BTreeSet::from([live.to_owned()]));
    for query in ["kind=doc", "label=cleanup", "q=Quokka", "by=you"] {
        assert!(!ids(&e.get(&format!("docs?{query}")).await).contains(&id), "{query}");
        assert!(ids(&e.get(&format!("docs?archived=true&{query}")).await).contains(&id), "{query}");
    }
    assert_eq!(ids(&e.get("docs?archived=true").await), BTreeSet::from([id.clone(), live.to_owned()]));
    assert_eq!(e.get(&format!("docs/{id}")).await["tabs"], before["tabs"]);
    assert_eq!(e.get(&format!("docs/{id}/history")).await, history);
    let read = e.get(&format!("docs/{id}/read?view=markdown")).await;
    assert!(read["tabs"][0]["markdown"].as_str().unwrap().contains("Keep this text."), "{read}");
    let root = e.api.trim_end_matches("/api/v1");
    for (url, expected) in [(file["url"].as_str().unwrap(), "saved"), (export["url"].as_str().unwrap(), "Keep this text.")] {
        let response = e.http.get(format!("{root}{url}")).send().await.unwrap();
        assert!(response.status().is_success(), "{url}: {}", response.status());
        assert!(response.text().await.unwrap().contains(expected));
    }
    let refs = e.ok(M::POST, "docs/refs", json!({"refs": [format!("doc:{id}")]})).await;
    assert_eq!(refs[0]["title"], before["title"]);
    assert_eq!(refs[0]["state"], "archived", "{refs}");
    let held = e.get(&format!("work/{}", work["id"].as_str().unwrap())).await;
    assert_eq!(held["live"][0]["title"], before["title"]);
    assert_eq!(held["live"][0]["state"], "archived", "{held}");
    let project = e.get(&format!("projects/{}", project["id"].as_str().unwrap())).await;
    assert!(
        project["docs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["doc"] == id && d["title"] == before["title"] && d["archived_at"] == archived["archived_at"]),
        "{project}"
    );
    eventually("every archived tab removed from memex", async || {
        let hits = e.get("memex/search?q=quokka%20archive").await;
        (!hits.as_array()?.iter().any(|h| mirrored(&h))).then_some(())
    })
    .await;
    assert!(e.archive(&id, false).await["archived_at"].is_null());
    assert!(ids(&e.get("docs").await).contains(&id));
    eventually("unarchiving mirrors both tabs again", async || {
        let hits = e.get("memex/search?q=quokka%20archive").await;
        (hits.as_array()?.iter().filter(mirrored).count() == 2).then_some(())
    })
    .await;
    e.ok(M::POST, &format!("docs/{id}/ops"), json!({"ops": [{"op": "insert", "markdown": "Writable again."}]})).await;
    assert_eq!(e.get(&format!("docs/{id}")).await["rev"], before["rev"].as_i64().unwrap() + 1);
    // Archiving is not deletion, and the existing DELETE contract is unchanged.
    e.archive(&id, true).await;
    assert_eq!(e.request(M::DELETE, &format!("docs/{id}"), Value::Null).await.0, StatusCode::NO_CONTENT);
    assert_eq!(e.request(M::GET, &format!("docs/{id}"), Value::Null).await.0, StatusCode::NOT_FOUND);
    assert!(!ids(&e.get("docs?archived=true").await).contains(&id));
    e.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn every_archived_document_write_requires_unarchiving_first() {
    let e = Env::new().await;
    let id = e.doc("Frozen document").await;
    let thread = e.ok(M::POST, &format!("docs/{id}/threads"), json!({"text": "An unsent comment."})).await;
    let tid = thread["id"].as_str().unwrap();
    let cid = thread["comments"][0]["id"].as_str().unwrap();
    let base = format!("docs/{id}");
    e.archive(&id, true).await;
    let before = e.get(&base).await;
    let history = e.get(&format!("{base}/history")).await;
    let threads = e.get(&format!("{base}/threads")).await;
    // Each body would be valid on a live document. In particular Google
    // must refuse before checking credentials or contacting Google.
    let writes = [
        (M::POST, format!("{base}/ops"), json!({"ops": [{"op": "insert", "markdown": "Must not land."}]})),
        (M::POST, format!("{base}/restore"), json!({"rev": 1})),
        (M::PATCH, base.clone(), json!({"title": "Must not rename"})),
        (M::PATCH, base.clone(), json!({"labels": ["must-not-land"]})),
        (M::PATCH, base.clone(), json!({"archived": false, "title": "Unarchive separately first"})),
        (M::POST, format!("{base}/threads"), json!({"text": "Must not create."})),
        (M::POST, format!("{base}/threads/{tid}/comments"), json!({"text": "Must not reply."})),
        (M::PATCH, format!("{base}/threads/{tid}/comments/{cid}"), json!({"text": "Must not edit."})),
        (M::DELETE, format!("{base}/threads/{tid}/comments/{cid}"), Value::Null),
        (M::POST, format!("{base}/threads/send"), json!({"threads": [tid]})),
        (M::POST, format!("{base}/threads/{tid}/resolve"), json!({})),
        (M::POST, format!("{base}/files"), json!({"name": "new.txt", "content": "bm8="})),
        (M::POST, format!("{base}/export"), json!({"format": "md"})),
        (M::POST, format!("{base}/google"), json!({"new": true})),
    ];
    for (method, path, body) in writes {
        let (status, value) = e.request(method.clone(), &path, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{method} {path}: {value}");
        assert!(value["error"].as_str().unwrap_or("").to_ascii_lowercase().contains("unarchive"), "{method} {path}: {value}");
    }
    assert_eq!(e.get(&base).await, before, "all rejected writes left the document intact");
    assert_eq!(e.get(&format!("{base}/history")).await, history);
    assert_eq!(e.get(&format!("{base}/threads")).await, threads);
    assert!(e.get(&format!("{base}/exports")).await.as_array().unwrap().is_empty());
    e.archive(&id, false).await;
    e.ok(M::PATCH, &base, json!({"title": "Live again", "labels": ["live"]})).await;
    e.ok(M::PATCH, &format!("{base}/threads/{tid}/comments/{cid}"), json!({"text": "Editable again."})).await;
    e.ok(M::POST, &format!("{base}/threads/{tid}/resolve"), json!({})).await;
    e.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn archived_comment_answers_leave_attention_and_return_on_unarchive() {
    let e = Env::new().await;
    let id = e.doc("An answer worth a look").await;
    let thread = e.ok(M::POST, &format!("docs/{id}/threads"), json!({"text": "What changed?"})).await;
    let tid = thread["id"].as_str().unwrap();
    c6_head::docs::comments::reply(
        &e.head.state,
        "home",
        &id,
        tid,
        &serde_json::from_value(json!({"text": "Here is the answer."})).unwrap(),
        "session:answering-agent",
    )
    .await
    .unwrap();
    let has_answer = |attention: &Value| attention["worth"].as_array().unwrap().iter().any(|a| a["doc_id"] == id);
    assert!(has_answer(&e.get("attention?realm=home").await));
    assert!(c6_head::docs::comments::answered_in(&e.head.state, "home", 100).await.unwrap().iter().any(|a| a.doc == id));
    e.archive(&id, true).await;
    assert!(!has_answer(&e.get("attention?realm=home").await));
    assert!(c6_head::docs::comments::answered_in(&e.head.state, "home", 100).await.unwrap().is_empty());
    assert_eq!(e.get(&format!("docs/{id}/threads")).await[0]["comments"].as_array().unwrap().len(), 2);
    e.archive(&id, false).await;
    assert!(has_answer(&e.get("attention?realm=home").await), "archiving did not mark the answer seen or resolve its thread");
    e.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_archive_candidates_include_descendants_and_explain_shared_use() {
    let e = Env::new().await;
    e.session("root", None, None, "home").await;
    e.session("child", Some("root"), Some("root"), "home").await;
    e.session("grandchild", Some("child"), Some("root"), "home").await;
    e.session("outside", None, None, "home").await;
    let own = e.session_doc("root", "Root document", "doc").await;
    let child = e.session_doc("child", "Child deck", "deck").await;
    let grandchild = e.session_doc("grandchild", "Grandchild document", "doc").await;
    let unrelated = e.session_doc("outside", "Unrelated document", "doc").await;
    let human = e.doc("Human document").await;
    let archived = e.session_doc("root", "Already archived", "doc").await;
    let deleted = e.session_doc("root", "Already deleted", "doc").await;
    e.archive(&archived, true).await;
    e.ok(M::DELETE, &format!("docs/{deleted}"), Value::Null).await;
    let work = e.ok(M::POST, "work", json!({"title": "Shared work", "attach": [{"ref": format!("doc:{own}")}]})).await;
    let project = e.ok(M::POST, "projects", json!({"title": "Shared project", "plan": false})).await;
    e.ok(M::POST, &format!("projects/{}/docs", project["id"].as_str().unwrap()), json!({"doc": own, "role": "notes"})).await;
    e.ok(M::POST, &format!("docs/{own}/threads"), json!({"text": "Still discussing."})).await;
    let batch: DocBatch = serde_json::from_value(json!({"ops": [{"op": "insert", "markdown": "An outside writer."}]})).unwrap();
    c6_head::docs::apply(&e.head.state, "home", &own, &batch, "session:outside", None).await.unwrap();
    let candidates = e.get("sessions/root/archive-docs").await;
    assert_eq!(ids(&candidates), BTreeSet::from([own.clone(), child.clone(), grandchild.clone()]));
    let root = candidates.as_array().unwrap().iter().find(|d| d["id"] == own).unwrap();
    assert_eq!((root["title"].as_str(), root["kind"].as_str()), (Some("Root document"), Some("doc")));
    let reasons = root["reasons"].as_array().unwrap();
    for expected in ["work", "project", "thread"] {
        assert!(reasons.iter().any(|r| r.as_str().unwrap().to_ascii_lowercase().contains(expected)), "missing {expected}: {root}");
    }
    assert!(
        reasons.iter().any(|r| {
            let r = r.as_str().unwrap().to_ascii_lowercase();
            r.contains("outside") || r.contains("another session")
        }),
        "missing outside-writer warning: {root}"
    );
    let deck = candidates.as_array().unwrap().iter().find(|d| d["id"] == child).unwrap();
    assert_eq!((deck["title"].as_str(), deck["kind"].as_str()), (Some("Child deck"), Some("deck")));
    assert!(deck["reasons"].as_array().unwrap().is_empty(), "family-only authors are safe: {deck}");
    // Warning reasons do not silently remove an explicit selection. Nor
    // do references cause unselected or linked docs to be archived.
    e.ok(M::PATCH, "sessions/root", json!({"archived": true, "archive_docs": [own, child, archived, deleted]})).await;
    assert!(e.session_archived("root").await);
    for id in [&own, &child, &archived] {
        assert!(e.get(&format!("docs/{id}")).await["archived_at"].is_i64(), "{id}");
    }
    for id in [&grandchild, &unrelated, &human] {
        assert!(e.get(&format!("docs/{id}")).await["archived_at"].is_null(), "{id}");
    }
    assert_eq!(ids(&e.get("sessions/root/archive-docs").await), BTreeSet::from([grandchild]));
    assert!(
        e.get(&format!("work/{}", work["id"].as_str().unwrap())).await["attached"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["ref"] == format!("doc:{own}"))
    );
    e.ok(M::PATCH, "sessions/root", json!({"archived": false})).await;
    assert!(e.get(&format!("docs/{own}")).await["archived_at"].is_i64(), "unarchiving a session does not restore documents implicitly");
    e.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn archiving_a_session_without_a_selection_keeps_all_documents() {
    let e = Env::new().await;
    e.session("empty", None, None, "home").await;
    assert!(e.get("sessions/empty/archive-docs").await.as_array().unwrap().is_empty());
    e.ok(M::PATCH, "sessions/empty", json!({"archived": true})).await;
    assert!(e.session_archived("empty").await);
    e.session("owner", None, None, "home").await;
    let id = e.session_doc("owner", "Keep unless selected", "doc").await;
    let refused = c6_head::agent_ops::call_for(
        &e.head.state,
        "owner",
        "sessions.update",
        json!({"id": "owner", "body": {"archived": true, "archive_docs": [id]}}),
    )
    .await;
    assert!(refused.is_err(), "an agent cannot select documents through session archive: {refused:?}");
    assert!(!e.session_archived("owner").await);
    assert!(e.get(&format!("docs/{id}")).await["archived_at"].is_null());
    for selection in [json!({"archived": true}), json!({"archived": true, "archive_docs": []})] {
        e.ok(M::PATCH, "sessions/owner", selection).await;
        assert!(e.session_archived("owner").await);
        assert!(e.get(&format!("docs/{id}")).await["archived_at"].is_null());
        e.ok(M::PATCH, "sessions/owner", json!({"archived": false})).await;
    }
    // The actual agent operation, without running an LLM: archiving its
    // own session must not implicitly archive documents either.
    let result =
        c6_head::agent_ops::call_for(&e.head.state, "owner", "sessions.update", json!({"id": "owner", "body": {"archived": true}}))
            .await
            .unwrap();
    assert!(result.get("asked").is_none(), "an agent may archive its own session: {result}");
    assert!(e.session_archived("owner").await);
    assert!(e.get(&format!("docs/{id}")).await["archived_at"].is_null());
    e.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_document_selection_rejects_unrelated_and_cross_realm_ids_atomically() {
    let e = Env::new().await;
    e.session("owner", None, None, "home").await;
    e.session("other", None, None, "home").await;
    let own = e.session_doc("owner", "Eligible", "doc").await;
    let other = e.session_doc("other", "Not this session's", "doc").await;
    e.ok(M::POST, "realms", json!({"id": "other-realm", "name": "Other realm"})).await;
    e.session("foreign", None, None, "other-realm").await;
    let foreign = e.session_doc("foreign", "Foreign document", "doc").await;
    let token = e.ok(M::POST, "auth/tokens", json!({"name": "home archive reader", "realm": "home"})).await;
    let token = token["token"].as_str().unwrap();
    let home = e.http.get(format!("{}/sessions/owner/archive-docs", e.api)).bearer_auth(token).send().await.unwrap();
    let status = home.status();
    let home: Value = home.json().await.unwrap();
    assert!(status.is_success(), "home token's cleanup candidates: {status}: {home}");
    assert_eq!(ids(&home), BTreeSet::from([own.clone()]));
    let foreign_session = e.http.get(format!("{}/sessions/foreign/archive-docs", e.api)).bearer_auth(token).send().await.unwrap();
    assert_eq!(foreign_session.status(), StatusCode::NOT_FOUND, "a realm token cannot inspect another realm's cleanup candidates");
    for invalid in [other.as_str(), foreign.as_str(), "missing-document"] {
        let (status, value) = e
            .request(M::PATCH, "sessions/owner", json!({"title": "Must not rename", "archived": true, "archive_docs": [own, invalid]}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "unrelated selection {invalid}: {value}");
        assert!(!e.session_archived("owner").await, "refused selection must not archive the session");
        assert_eq!(e.head.state.store.session("owner").await.unwrap().unwrap().title, "owner", "validate before renaming too");
        assert!(e.get(&format!("docs/{own}")).await["archived_at"].is_null(), "validate the entire selection before archiving anything");
    }
    assert!(e.get(&format!("docs/{other}")).await["archived_at"].is_null());
    assert!(e.get(&format!("docs/{foreign}?realm=other-realm")).await["archived_at"].is_null());
    let (status, value) = e.request(M::PATCH, "sessions/owner", json!({"archived": false, "archive_docs": [own]})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "document selection is only valid while archiving: {value}");
    assert!(e.get(&format!("docs/{own}")).await["archived_at"].is_null());
    e.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_archive_storage_failure_keeps_session_open_and_reports_partial_cleanup() {
    let e = Env::new().await;
    e.session("owner", None, None, "home").await;
    let first = e.session_doc("owner", "Archived before the failure", "doc").await;
    let blocked = e.session_doc("owner", "Injected archive failure", "doc").await;
    let memex = c6_head::knowledge::get(&e.head.state).unwrap().of_realm("home").await.unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_selected_archive BEFORE UPDATE OF archived_at ON docs \
         WHEN NEW.archived_at IS NOT NULL AND OLD.title = 'Injected archive failure' \
         BEGIN SELECT RAISE(ABORT, 'selected archive storage failure'); END",
    )
    .execute(&memex.store.pool)
    .await
    .unwrap();

    let (status, body) = e.request(M::PATCH, "sessions/owner", json!({"archived": true, "archive_docs": [first, blocked]})).await;
    assert_eq!(status, StatusCode::CONFLICT, "a storage failure is not a harmless stale selection: {body}");
    let error = body["error"].as_str().expect("a clear error message");
    assert!(error.contains("Session kept open"), "{body}");
    let (successes, failures) = error.split_once("Failures:").expect("the response distinguishes partial successes from failures");
    assert!(successes.contains("Archived ids:") && successes.contains(&first), "the committed document is reported: {body}");
    assert!(
        failures.contains(&blocked) && failures.contains("selected archive storage failure"),
        "the failed document and cause are reported: {body}"
    );
    assert!(!e.session_archived("owner").await, "do not archive the session after incomplete cleanup");
    // Cleanup is per document, not one transaction across the selection:
    // the response must not imply that the earlier success rolled back.
    assert!(e.get(&format!("docs/{first}")).await["archived_at"].is_i64());
    assert!(e.get(&format!("docs/{blocked}")).await["archived_at"].is_null());
    e.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn session_archive_update_failure_reports_documents_already_archived() {
    let e = Env::new().await;
    e.session("owner", None, None, "home").await;
    let first = e.session_doc("owner", "First archived document", "doc").await;
    let second = e.session_doc("owner", "Second archived document", "doc").await;
    // Sessions use the head store's integer `archived`, not the knowledge
    // database's document `archived_at` timestamp. Open only our own DB.
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(e._dir.path().join("c6h.db")))
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_owner_session_archive BEFORE UPDATE OF archived ON sessions \
         WHEN NEW.archived = 1 AND OLD.id = 'owner' \
         BEGIN SELECT RAISE(ABORT, 'session archive storage failure'); END",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (status, body) = e.request(M::PATCH, "sessions/owner", json!({"archived": true, "archive_docs": [first, second]})).await;
    assert_eq!(status, StatusCode::CONFLICT, "report partial cleanup even when the final session write fails: {body}");
    let error = body["error"].as_str().expect("a clear error message");
    assert!(error.contains("Session kept open") && error.contains("Archived ids:"), "{body}");
    assert!(error.contains(&first) && error.contains(&second), "both committed document archives are reported: {body}");
    assert!(error.contains("session archive storage failure"), "the session write's failure cause is reported: {body}");
    assert!(!e.session_archived("owner").await);
    for id in [&first, &second] {
        assert!(e.get(&format!("docs/{id}")).await["archived_at"].is_i64(), "the reported archive must actually be committed: {id}");
    }
    pool.close().await;
    e.head.shutdown().await.unwrap();
}
