//! The same cleanup candidates for session archive and agent approvals.
//! A decision freezes ids, never a query; approval grants no future writes.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use c6_api::docs_archive::{ArchiveDocCandidate, ArchiveDocSkipped, ArchiveDocsIn, ArchiveDocsResult};
use serde_json::{Value, json};

use crate::api::ApiResult;
use crate::realms::Caller;
use crate::{HeadState, bad_request};

pub fn routes() -> Router<Arc<HeadState>> {
    Router::new()
        // Shared with the session archive dialog.
        .route("/api/v1/sessions/{id}/archive-docs", get(session_http))
        // Explicit selections, including approved agent cleanup calls.
        .route("/api/v1/docs/archive", post(archive_http))
}

/// A session and all of its descendants, including remote subagents.
pub async fn family(st: &HeadState, session: &str) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "WITH RECURSIVE family(id) AS (SELECT ? UNION SELECT s.id FROM sessions s JOIN family f \
         ON s.parent_id = f.id OR s.root_id = f.id) SELECT id FROM family",
    )
    .bind(session)
    .fetch_all(&st.store.pool)
    .await?)
}

/// Live docs made by this session or its subagents. No linked document is
/// pulled in implicitly, and archived/deleted documents never appear.
pub async fn session_candidates(st: &HeadState, session: &str) -> anyhow::Result<Vec<ArchiveDocCandidate>> {
    let realm = crate::realms::of_session(st, session).await?.ok_or_else(|| crate::not_found("no such session"))?;
    if !crate::memex::has_memex(&realm) {
        // Administrative sessions have no document store to clean up.
        return Ok(Vec::new());
    }
    let family = family(st, session).await?;
    let pool = super::pool(st, &realm).await?;
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, created_by FROM docs WHERE deleted_at IS NULL AND archived_at IS NULL ORDER BY updated_at DESC, id")
            .fetch_all(&pool)
            .await?;
    let ids = rows.into_iter().filter(|(_, by)| family.contains(&crate::agent_ops::made_by(by))).map(|(id, _)| id).collect::<Vec<_>>();
    candidates(st, &realm, &ids, &family).await
}

/// Safety notes for an explicit selection. Empty reasons = safe to
/// preselect; a caller may still explicitly select a warned document.
pub async fn candidates(st: &HeadState, realm: &str, ids: &[String], family: &[String]) -> anyhow::Result<Vec<ArchiveDocCandidate>> {
    let pool = super::pool(st, realm).await?;
    let work: Vec<String> = sqlx::query_scalar(
        "SELECT fields FROM records WHERE realm = ? AND collection = 'work' AND json_extract(fields, '$.status') = 'open'",
    )
    .bind(realm)
    .fetch_all(&st.store.pool)
    .await?;
    let projects: Vec<String> = sqlx::query_scalar("SELECT fields FROM records WHERE realm = ? AND collection = 'projects'")
        .bind(realm)
        .fetch_all(&st.store.pool)
        .await?;
    let work = work.iter().map(|s| serde_json::from_str::<Value>(s)).collect::<Result<Vec<_>, _>>()?;
    let projects = projects.iter().map(|s| serde_json::from_str::<Value>(s)).collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::new();
    for id in ids {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT title, kind FROM docs WHERE id = ? AND deleted_at IS NULL AND archived_at IS NULL")
                .bind(id)
                .fetch_optional(&pool)
                .await?;
        let Some((title, kind)) = row else { continue };
        let reference = format!("doc:{id}");
        let mut reasons = Vec::new();
        for w in &work {
            if w["attached"].as_array().into_iter().flatten().any(|a| a["ref"] == reference) {
                reasons.push(format!("Referenced by open work: {}", w["title"].as_str().unwrap_or("Untitled")));
            }
        }
        for p in &projects {
            if p["documents"].as_array().into_iter().flatten().any(|d| d["doc"] == *id)
                || p["links"].as_array().into_iter().flatten().any(|l| l["ref"] == reference)
            {
                reasons.push(format!("Project refers to it: {}", p["title"].as_str().unwrap_or("Untitled")));
            }
        }
        let writers: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT \"by\" FROM doc_ops WHERE doc_id = ?").bind(id).fetch_all(&pool).await?;
        if writers.iter().any(|by| by == "you") {
            reasons.push("You wrote in it".into());
        }
        if writers.iter().any(|by| {
            let session = crate::agent_ops::made_by(by);
            !session.is_empty() && !family.contains(&session)
        }) {
            reasons.push("Another session wrote in it".into());
        }
        if writers.iter().any(|by| by != "you" && crate::agent_ops::made_by(by).is_empty()) {
            reasons.push("Edited outside this session family".into());
        }
        let open: i64 =
            sqlx::query_scalar("SELECT count(*) FROM doc_threads WHERE doc_id = ? AND state = 'open'").bind(id).fetch_one(&pool).await?;
        if open > 0 {
            reasons.push(format!("{open} open comment thread{}", if open == 1 { "" } else { "s" }));
        }
        out.push(ArchiveDocCandidate { id: id.clone(), title, kind, reasons });
    }
    Ok(out)
}

/// Keep order, reject empty ids, and never treat a selection as a query.
pub fn selection(ids: &[String]) -> anyhow::Result<Vec<String>> {
    if ids.is_empty() {
        return Err(bad_request("archive_many takes an explicit, nonempty list of document ids"));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for id in ids {
        let id = id.trim().strip_prefix("doc:").unwrap_or(id.trim()).trim();
        if id.is_empty() {
            return Err(bad_request("archive_many takes explicit nonempty document ids"));
        }
        if id.starts_with("doc:") {
            return Err(bad_request("document ids may have at most one doc: prefix; nested prefixes are not allowed"));
        }
        if seen.insert(id.to_owned()) {
            out.push(id.to_owned());
        }
    }
    Ok(out)
}

/// The fixed selection is intentionally not expanded or re-filtered by
/// safety warnings. Missing/already archived docs are skipped and reported.
pub async fn archive_many(st: &HeadState, realm: &str, ids: &[String], by: &str) -> anyhow::Result<ArchiveDocsResult> {
    let ids = selection(ids)?;
    let pool = super::pool(st, realm).await?;
    let mut result = ArchiveDocsResult::default();
    for id in ids {
        let live: Option<(Option<i64>, Option<i64>)> =
            match sqlx::query_as("SELECT deleted_at, archived_at FROM docs WHERE id = ?").bind(&id).fetch_optional(&pool).await {
                Ok(live) => live,
                Err(e) => {
                    result.skipped.push(ArchiveDocSkipped { id, reason: format!("Could not check document: {e}") });
                    continue;
                }
            };
        let why = match live {
            None | Some((Some(_), _)) => Some("gone"),
            Some((_, Some(_))) => Some("already archived"),
            _ => None,
        };
        if let Some(why) = why {
            result.skipped.push(ArchiveDocSkipped { id, reason: why.into() });
            continue;
        }
        match crate::agent_ops::ACTOR.scope(by.to_owned(), super::set_archived(st, realm, &id, true)).await {
            Ok(true) => result.archived.push(id),
            Ok(false) => result.skipped.push(ArchiveDocSkipped { id, reason: "already archived".into() }),
            Err(e) => {
                // Deletion can race the preliminary check. Report that
                // stale id. Other per-id failures are reported too: a
                // later failure must not lose the earlier partial result.
                let exists = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM docs WHERE id = ? AND deleted_at IS NULL")
                    .bind(&id)
                    .fetch_one(&pool)
                    .await;
                if matches!(exists, Ok(0)) {
                    result.skipped.push(ArchiveDocSkipped { id, reason: "gone".into() });
                } else {
                    result.skipped.push(ArchiveDocSkipped { id, reason: format!("Could not archive document: {e:#}") });
                }
            }
        }
    }
    Ok(result)
}

pub async fn session_http(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> ApiResult<Json<Vec<ArchiveDocCandidate>>> {
    let realm = crate::realms::of_session(&st, &id).await?.ok_or_else(|| crate::not_found("no such session"))?;
    if !caller.sees(&realm) {
        return Err(crate::not_found("no such session").into());
    }
    Ok(Json(session_candidates(&st, &id).await?))
}

pub async fn archive_http(
    State(st): State<Arc<HeadState>>,
    Extension(caller): Extension<Caller>,
    Query(q): Query<c6_api::InRealm>,
    Json(input): Json<ArchiveDocsIn>,
) -> ApiResult<Json<ArchiveDocsResult>> {
    let realm = super::realm_for(&st, &caller, input.realm.as_deref().or(q.realm.as_deref())).await?;
    Ok(Json(archive_many(&st, &realm, &input.ids, &super::by_of(&caller)).await?))
}

/// Narrow doc-tool actions go through the same agent operation gate as
/// carbon6, not a second approval scheme in docs/session.rs.
pub async fn for_session(st: &HeadState, session: &str, p: &Value) -> anyhow::Result<Value> {
    match p["action"].as_str().unwrap_or("") {
        "archive" | "unarchive" => {
            let id = p["id"].as_str().unwrap_or("").trim().trim_start_matches("doc:");
            if id.is_empty() {
                return Err(bad_request("say which document: id"));
            }
            crate::agent_ops::call_for(st, session, "docs.rename", json!({"id": id, "body": {"archived": p["action"] == "archive"}})).await
        }
        "archive_many" => {
            crate::agent_ops::call_for(st, session, "docs.archive_many", json!({"body": {"ids": p["ids"], "reason": p["reason"]}})).await
        }
        _ => unreachable!("archive bridge handles only archive actions"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docs::{DocNew, DocQuery};
    use futures::FutureExt as _;

    async fn head() -> (tempfile::TempDir, crate::Head) {
        let dir = tempfile::tempdir().unwrap();
        let head = crate::start(crate::HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
        .await
        .unwrap();
        for (id, parent, root) in [
            ("cleanup", None, None),
            ("child", Some("cleanup"), Some("cleanup")),
            ("grandchild", Some("child"), Some("cleanup")),
            ("unrelated", None, None),
        ] {
            sqlx::query("INSERT INTO sessions (id, worker_id, title, cwd, connection_id, model, status, created_at, updated_at, parent_id, root_id) \
                         VALUES (?, '00000000-0000-0000-0000-000000000001', ?, '', '', '', 'idle', 0, 0, ?, ?)")
                .bind(id).bind(id).bind(parent).bind(root).execute(&head.state.store.pool).await.unwrap();
        }
        (dir, head)
    }

    async fn doc(st: &HeadState, title: &str, by: &str) -> String {
        super::super::create(st, "home", &DocNew { title: title.into(), markdown: "A paragraph.".into(), ..Default::default() }, by)
            .await
            .unwrap()
            .0
            .summary
            .id
    }

    async fn archived(st: &HeadState, id: &str) -> bool {
        super::super::get(st, "home", id).await.unwrap().summary.archived_at.is_some()
    }

    async fn batch(st: &HeadState, ids: &[String]) -> Value {
        for_session(st, "cleanup", &json!({"action": "archive_many", "ids": ids, "reason": "Superseded drafts"})).await.unwrap()
    }

    async fn answer(st: &HeadState, asked: &Value, option: &str) -> c6_api::Decision {
        crate::decisions::answer(
            st,
            asked["asked"].as_i64().unwrap(),
            &c6_api::DecisionAnswer { option: Some(option.into()), ..Default::default() },
            "test",
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn one_archive_approval_is_exact_skips_stale_and_never_grants_future_writes() {
        let (_dir, head) = head().await;
        let st = &head.state;
        let ours = doc(st, "Our draft", "session:child").await;
        let theirs = doc(st, "Your draft", "you").await;
        let gone = doc(st, "Deleted before approval", "you").await;
        let stale = doc(st, "Archived before approval", "you").await;
        // Ownership includes subagents, and needs no decision.
        let free = batch(st, std::slice::from_ref(&ours)).await;
        assert!(free.get("asked").is_none(), "{free}");
        assert!(archived(st, &ours).await);
        super::super::set_archived(st, "home", &ours, false).await.unwrap();
        let ids = vec![ours.clone(), theirs.clone(), gone.clone(), stale.clone()];
        let asked = batch(st, &ids).await;
        let open: i64 = sqlx::query_scalar("SELECT count(*) FROM decisions WHERE state = 'open' AND ref LIKE 'agentop:%'")
            .fetch_one(&st.store.pool)
            .await
            .unwrap();
        assert_eq!(open, 1, "one card for the mixed selection");
        assert!(!archived(st, &ours).await && !archived(st, &theirs).await, "nothing changes before approval");
        let d = crate::decisions::get(st, asked["asked"].as_i64().unwrap()).await.unwrap().unwrap();
        assert_eq!(d.context["input"]["body"]["ids"], json!(ids));
        assert_eq!(d.context["archive_docs"].as_array().unwrap().len(), 4);
        assert!(d.title.contains('4') && d.document.contains(&format!("doc:{theirs}")));
        assert_eq!(d.context["once"], true);
        assert_eq!(d.options[1].label, "Not now");
        answer(st, &asked, "Not now").await;
        assert!(!archived(st, &ours).await && !archived(st, &theirs).await);
        let asked = batch(st, &ids).await;
        let later = doc(st, "Newer, not selected", "you").await;
        super::super::delete(st, "home", &gone, "you").await.unwrap();
        super::super::set_archived(st, "home", &stale, true).await.unwrap();
        let answered = answer(st, &asked, "Allow").await;
        assert_eq!(answered.context["archive_result"]["archived"], json!([ours, theirs]));
        assert_eq!(
            answered.context["archive_result"]["skipped"],
            json!([
                {"id": gone, "reason": "gone"}, {"id": stale, "reason": "already archived"}
            ])
        );
        assert!(archived(st, &ours).await && archived(st, &theirs).await && archived(st, &stale).await);
        assert!(!archived(st, &later).await, "approval never expands to newer documents");
        let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM agent_writers").fetch_one(&st.store.pool).await.unwrap();
        assert_eq!(grants, 0, "one-shot approval grants no future change permission");
        let pool = super::super::pool(st, "home").await.unwrap();
        let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM doc_writers").fetch_one(&pool).await.unwrap();
        assert_eq!(grants, 0);
        let reports: Vec<String> = sqlx::query_scalar("SELECT text FROM commands WHERE session_id = 'cleanup' AND text IS NOT NULL")
            .fetch_all(&st.store.pool)
            .await
            .unwrap();
        assert!(reports.iter().any(|t| t.contains("skipped") && t.contains(&gone) && t.contains(&stale)), "{reports:?}");
        let already_archived = batch(st, std::slice::from_ref(&theirs)).await;
        assert!(already_archived.get("asked").is_some(), "permission checks the selected id, not only currently live docs");
        let d = answer(st, &already_archived, "Not now").await;
        assert_eq!(d.context["archive_docs"][0]["id"], theirs);
        assert!(d.context["archive_docs"][0]["reasons"][0].as_str().unwrap().contains("already archived"));
        super::super::set_archived(st, "home", &theirs, false).await.unwrap();
        assert!(batch(st, std::slice::from_ref(&theirs)).await.get("asked").is_some(), "asks anew even for the same id");
        let rename =
            crate::agent_ops::call_for(st, "cleanup", "docs.rename", json!({"id": theirs, "body": {"title": "Changed"}})).await.unwrap();
        assert!(rename.get("asked").is_some(), "batch approval doesn't allow a later edit: {rename}");
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn archive_state_needs_explicit_doc_consent_even_for_its_works_docs() {
        let (_dir, head) = head().await;
        let st = &head.state;
        let id = doc(st, "Work-owned by you", "you").await;
        let work = crate::work::create(
            st,
            &Caller::in_realm("home"),
            &c6_api::work::WorkNew {
                title: "Shared work".into(),
                attach: vec![
                    c6_api::work::WorkRef { reference: "session:cleanup".into(), title: "cleanup".into() },
                    c6_api::work::WorkRef { reference: format!("doc:{id}"), title: "doc".into() },
                ],
                ..Default::default()
            },
            "you",
        )
        .await
        .unwrap();
        assert!(!work.id.is_empty());
        assert!(super::super::session::may_write(st, "home", &id, "cleanup").await.unwrap().is_ok());
        let project_doc = doc(st, "Project doc by you", "you").await;
        let project = crate::projects::create(
            st,
            &Caller::in_realm("home"),
            &c6_api::projects::ProjectNew { title: "Work's project".into(), plan: false, ..Default::default() },
            "you",
        )
        .await
        .unwrap();
        crate::projects::add_doc(
            st,
            &project.id,
            &c6_api::projects::ProjectDocIn { doc: project_doc.clone(), role: String::new() },
            &Caller::in_realm("home"),
            "you",
        )
        .await
        .unwrap();
        sqlx::query("UPDATE records SET fields = json_set(fields, '$.project', ?) WHERE id = ?")
            .bind(&project.id)
            .bind(&work.id)
            .execute(&st.store.pool)
            .await
            .unwrap();
        assert!(super::super::session::may_write(st, "home", &project_doc, "cleanup").await.unwrap().is_ok());
        let project_asked = for_session(st, "cleanup", &json!({"action": "archive", "id": project_doc})).await.unwrap();
        assert!(project_asked.get("asked").is_some(), "project doc still asks: {project_asked}");
        answer(st, &project_asked, "Deny").await;
        assert!(!archived(st, &project_doc).await);
        let asked = for_session(st, "cleanup", &json!({"action": "archive", "id": id})).await.unwrap();
        assert!(asked.get("asked").is_some(), "work doc still asks: {asked}");
        assert!(!archived(st, &id).await);
        answer(st, &asked, "Allow").await;
        assert!(!archived(st, &id).await, "existing patch model grants, then agent retries");
        let done = for_session(st, "child", &json!({"action": "archive", "id": id})).await.unwrap();
        assert!(done.get("asked").is_none(), "explicit consent covers family: {done}");
        assert!(archived(st, &id).await);
        let done = for_session(st, "cleanup", &json!({"action": "unarchive", "id": id})).await.unwrap();
        assert!(done.get("asked").is_none());
        assert!(!archived(st, &id).await);
        // Persistent consent for single-document patches must not become
        // permission for a later cleanup call.
        assert!(batch(st, std::slice::from_ref(&id)).await.get("asked").is_some());
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn session_archive_candidates_share_use_reasons_and_exclude_archived_or_other_creators() {
        let (_dir, head) = head().await;
        let st = &head.state;
        let safe = doc(st, "Safe draft", "session:grandchild").await;
        let used = doc(st, "Used draft", "session:cleanup").await;
        let hidden = doc(st, "Archived draft", "session:cleanup").await;
        let unrelated = doc(st, "Unrelated", "session:unrelated").await;
        super::super::set_archived(st, "home", &hidden, true).await.unwrap();
        // A human and another session wrote; these must be sourced from
        // doc_ops, not the document's last writer or active-writer list.
        super::super::patch(st, "home", &used, &c6_api::docs::DocPatch { title: Some("Human edit".into()), ..Default::default() }, "you")
            .await
            .unwrap();
        super::super::patch(
            st,
            "home",
            &used,
            &c6_api::docs::DocPatch { title: Some("Other edit".into()), ..Default::default() },
            "session:unrelated",
        )
        .await
        .unwrap();
        let w = crate::work::create(
            st,
            &Caller::in_realm("home"),
            &c6_api::work::WorkNew {
                title: "Open use".into(),
                attach: vec![c6_api::work::WorkRef { reference: format!("doc:{used}"), title: "Used".into() }],
                ..Default::default()
            },
            "you",
        )
        .await
        .unwrap();
        let p = crate::projects::create(
            st,
            &Caller::in_realm("home"),
            &c6_api::projects::ProjectNew { title: "Closed project still refers".into(), plan: false, ..Default::default() },
            "you",
        )
        .await
        .unwrap();
        sqlx::query("UPDATE records SET fields = json_set(fields, '$.status', 'done', '$.documents', json(?)) WHERE id = ?")
            .bind(json!([{"doc": used}]).to_string())
            .bind(&p.id)
            .execute(&st.store.pool)
            .await
            .unwrap();
        let pool = super::super::pool(st, "home").await.unwrap();
        sqlx::query("INSERT INTO doc_threads (id, doc_id, anchor, created_by, created_at) VALUES ('open-comment', ?, '{}', 'you', 0)")
            .bind(&used)
            .execute(&pool)
            .await
            .unwrap();
        let candidates = session_candidates(st, "cleanup").await.unwrap();
        assert_eq!(candidates.len(), 2, "{candidates:?}; unrelated {unrelated}");
        assert!(candidates.iter().find(|d| d.id == safe).unwrap().reasons.is_empty());
        let reasons = &candidates.iter().find(|d| d.id == used).unwrap().reasons;
        for expected in [
            "Referenced by open work: Open use",
            "Project refers to it: Closed project still refers",
            "You wrote in it",
            "Another session wrote in it",
            "1 open comment thread",
        ] {
            assert!(reasons.iter().any(|r| r == expected), "missing {expected}: {reasons:?}");
        }
        let http = reqwest::Client::new();
        let response =
            http.get(format!("http://127.0.0.1:{}/api/v1/sessions/cleanup/archive-docs", head.ui_addrs[0].port())).send().await.unwrap();
        assert!(response.status().is_success(), "{}", response.text().await.unwrap());
        let via_http: Vec<ArchiveDocCandidate> = response.json().await.unwrap();
        assert_eq!(via_http, candidates);
        let denied = session_http(State(st.clone()), Extension(Caller::in_realm("elsewhere")), Path("cleanup".into())).await;
        assert!(matches!(denied, Err(crate::api::ApiErr(axum::http::StatusCode::NOT_FOUND, _))));
        let normal = super::super::session::for_session(st, "cleanup", &json!({"action": "list"})).await.unwrap();
        assert!(!normal.to_string().contains(&hidden));
        let archived = super::super::session::for_session(st, "cleanup", &json!({"action": "list", "archived": true})).await.unwrap();
        assert!(archived.to_string().contains(&hidden));
        let all = super::super::list(st, "home", &DocQuery { archived: true, ..Default::default() }).await.unwrap();
        assert_eq!(all.len(), 4);
        assert!(!w.id.is_empty());
        head.shutdown().await.unwrap();
    }

    #[test]
    fn cleanup_selection_is_explicit_nonempty_and_deduplicated() {
        assert!(selection(&[]).is_err());
        assert!(selection(&[" ".into()]).is_err());
        for invalid in ["doc:doc:a", "doc: doc:a", " doc:doc:doc:a ", "doc: "] {
            assert!(selection(&[invalid.into()]).is_err(), "{invalid}");
        }
        let canonical = selection(&["doc:a".into(), "a".into(), " b ".into(), " doc: c ".into()]).unwrap();
        assert_eq!(canonical, ["a", "b", "c"]);
        assert_eq!(selection(&canonical).unwrap(), canonical, "accepted selections must be idempotent");
        assert_eq!(selection(&vec!["a".into(); 501]).unwrap(), ["a"]);
    }

    #[tokio::test]
    async fn nested_doc_prefix_cannot_change_the_previewed_and_approved_archive_target() {
        let (_dir, head) = head().await;
        let st = &head.state;
        let id = doc(st, "Prefix target", "you").await;
        let nested = format!("doc:doc:{id}");
        let p = json!({"action": "archive_many", "ids": [nested], "reason": "Clean up"});
        assert!(for_session(st, "cleanup", &p).await.unwrap_err().to_string().contains("nested prefixes"));
        let args = json!({"body": {"ids": [nested], "reason": "Clean up"}});
        assert!(
            crate::agent_ops::call_for(st, "cleanup", "docs.archive_many", args).await.unwrap_err().to_string().contains("nested prefixes")
        );
        assert!(archive_many(st, "home", &[nested], "you").await.is_err());
        let cards: i64 =
            sqlx::query_scalar("SELECT count(*) FROM decisions WHERE ref LIKE 'agentop:%'").fetch_one(&st.store.pool).await.unwrap();
        assert_eq!(cards, 0, "a malformed selection must never produce a misleading approval");
        assert!(!archived(st, &id).await);

        let asked = batch(st, &[format!(" doc: {id} ")]).await;
        let d = crate::decisions::get(st, asked["asked"].as_i64().unwrap()).await.unwrap().unwrap();
        assert_eq!(d.context["input"]["body"]["ids"], json!([id]));
        assert_eq!(d.context["archive_docs"][0]["id"], id);
        assert_eq!(d.context["archive_docs"][0]["title"], "Prefix target");
        assert!(d.context["archive_docs"][0]["reasons"].as_array().unwrap().contains(&json!("You wrote in it")));
        // A card persisted by the old gate could already contain one
        // unstripped prefix. Replay must reject it, not retarget its ids.
        let mut legacy = d.context.clone();
        legacy["input"]["body"]["ids"] = json!([format!("doc:{id}")]);
        legacy["archive_docs"][0]["id"] = json!(format!("doc:{id}"));
        sqlx::query("UPDATE decisions SET context = ? WHERE id = ?")
            .bind(legacy.to_string())
            .bind(d.id)
            .execute(&st.store.pool)
            .await
            .unwrap();
        let e = crate::decisions::answer(st, d.id, &c6_api::DecisionAnswer { option: Some("Allow".into()), ..Default::default() }, "test")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not canonical"), "{e:#}");
        assert!(!archived(st, &id).await, "legacy previews cannot be normalized into another target");
        answer(st, &asked, "Not now").await;
        let asked = batch(st, &[format!(" doc: {id} ")]).await;
        answer(st, &asked, "Allow").await;
        assert!(archived(st, &id).await, "the permitted prefix freezes and executes exactly the same canonical id");
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn archived_mutations_refuse_before_opening_permission_cards() {
        let (_dir, head) = head().await;
        let st = &head.state;
        let id = doc(st, "Your archived doc", "you").await;
        super::super::set_archived(st, "home", &id, true).await.unwrap();
        for (op, body) in [
            ("docs.edit", json!({"ops": []})),
            ("docs.rename", json!({"title": "New title"})),
            ("docs.rename", json!({"archived": false, "title": "New title"})),
            ("docs.restore", json!({"rev": 1})),
            ("docs.comment", json!({"text": "Comment"})),
            ("docs.export", json!({"format": "md"})),
        ] {
            let e = crate::agent_ops::call_for(st, "cleanup", op, json!({"id": id, "body": body})).await.unwrap_err();
            assert!(e.to_string().contains("Unarchive it first"), "{op}: {e:#}");
        }
        for p in [json!({"action": "edit", "id": id, "ops": []}), json!({"action": "restore", "id": id, "rev": 1})] {
            let e = super::super::session::for_session(st, "cleanup", &p).await.unwrap_err();
            assert!(e.to_string().contains("Unarchive it first"), "{e:#}");
        }
        let cards: i64 = sqlx::query_scalar("SELECT count(*) FROM decisions WHERE ref LIKE 'agentop:%' OR ref LIKE 'docwrite:%'")
            .fetch_one(&st.store.pool)
            .await
            .unwrap();
        assert_eq!(cards, 0, "state refusal never opens permission cards");
        let unarchive = for_session(st, "cleanup", &json!({"action": "unarchive", "id": id})).await.unwrap();
        assert!(unarchive.get("asked").is_some(), "pure archive state patch still asks: {unarchive}");
        let delete = crate::agent_ops::call_for(st, "cleanup", "docs.delete", json!({"id": id})).await.unwrap();
        assert!(delete.get("asked").is_some(), "unchanged delete permission path remains available: {delete}");
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_reports_per_document_errors_without_losing_partial_successes() {
        let (_dir, head) = head().await;
        let st = &head.state;
        let first = doc(st, "First", "you").await;
        let blocked = doc(st, "Broken", "you").await;
        let last = doc(st, "Last", "you").await;
        let pool = super::super::pool(st, "home").await.unwrap();
        sqlx::query(
            "CREATE TRIGGER fail_one_archive BEFORE UPDATE OF archived_at ON docs \
            WHEN OLD.title = 'Broken' BEGIN SELECT RAISE(ABORT, 'test archive failure'); END",
        )
        .execute(&pool)
        .await
        .unwrap();
        let result = archive_many(st, "home", &[first.clone(), blocked.clone(), last.clone()], "you").await.unwrap();
        assert_eq!(result.archived, [first, last]);
        assert_eq!(result.skipped.len(), 1);
        assert_eq!(result.skipped[0].id, blocked);
        assert!(result.skipped[0].reason.contains("test archive failure"), "{result:?}");
        assert!(!archived(st, &blocked).await);
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn archived_requester_keeps_partial_cleanup_result_without_input_or_wake() {
        let (_dir, head) = head().await;
        let st = &head.state;
        let good = doc(st, "Archive succeeds", "you").await;
        let blocked = doc(st, "Broken", "you").await;
        let gone = doc(st, "Deleted after preview", "you").await;
        let stale = doc(st, "Archived after preview", "you").await;
        let ids = vec![good.clone(), blocked.clone(), gone.clone(), stale.clone()];
        let asked = batch(st, &ids).await;
        let pool = super::super::pool(st, "home").await.unwrap();
        sqlx::query(
            "CREATE TRIGGER fail_one_archive BEFORE UPDATE OF archived_at ON docs \
             WHEN OLD.title = 'Broken' BEGIN SELECT RAISE(ABORT, 'test archive failure'); END",
        )
        .execute(&pool)
        .await
        .unwrap();
        super::super::delete(st, "home", &gone, "you").await.unwrap();
        super::super::set_archived(st, "home", &stale, true).await.unwrap();
        st.store.set_archived("cleanup", true).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let connection = st.registry.attach("00000000-0000-0000-0000-000000000001".parse().unwrap(), 0, tx, Vec::new(), "worker");

        let answered = answer(st, &asked, "Allow").await;
        let saved = crate::decisions::get(st, answered.id).await.unwrap().unwrap();
        assert_eq!(saved.state, "answered");
        assert_eq!(saved.context["archive_consumed"], true);
        assert_eq!(saved.context["input"]["body"]["ids"], json!(ids));
        assert_eq!(saved.context["archive_result"]["archived"], json!([good]));
        let skipped = saved.context["archive_result"]["skipped"].as_array().unwrap();
        assert_eq!(skipped.len(), 3);
        assert_eq!(skipped[0]["id"], blocked);
        assert!(skipped[0]["reason"].as_str().unwrap().contains("test archive failure"));
        assert_eq!(skipped[1], json!({"id": gone, "reason": "gone"}));
        assert_eq!(skipped[2], json!({"id": stale, "reason": "already archived"}));
        assert!(saved.context["archive_error"].is_null());
        assert!(archived(st, &good).await && !archived(st, &blocked).await);
        let response = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}/api/v1/decisions/{}", head.ui_addrs[0].port(), saved.id))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let visible: Value = response.json().await.unwrap();
        assert_eq!(visible["context"]["archive_result"], saved.context["archive_result"], "the decision API exposes the saved report");
        let requester = st.store.session("cleanup").await.unwrap().unwrap();
        assert_eq!((requester.archived, requester.status.as_str(), requester.last_seq), (1, "idle", 0));
        let queued: i64 =
            sqlx::query_scalar("SELECT count(*) FROM commands WHERE session_id = 'cleanup'").fetch_one(&st.store.pool).await.unwrap();
        assert_eq!(queued, 0, "the saved report must not enqueue input for an archived session");
        assert!(connection.deliver.notified().now_or_never().is_none(), "no worker delivery wake");
        assert!(matches!(rx.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)), "no direct worker command");
        drop(connection);
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn archived_requester_keeps_top_level_cleanup_error_without_input_or_wake() {
        let (_dir, head) = head().await;
        let st = &head.state;
        let id = doc(st, "Execution cannot start", "you").await;
        let asked = batch(st, std::slice::from_ref(&id)).await;
        st.store.set_archived("cleanup", true).await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let connection = st.registry.attach("00000000-0000-0000-0000-000000000001".parse().unwrap(), 0, tx, Vec::new(), "worker");
        // Make the approved realm temporarily unavailable so dispatch
        // fails before archive_many can return any per-document result.
        sqlx::query("UPDATE realms SET id = 'home-unavailable' WHERE id = 'home'").execute(&st.store.pool).await.unwrap();
        let answered = answer(st, &asked, "Allow").await;
        sqlx::query("UPDATE realms SET id = 'home' WHERE id = 'home-unavailable'").execute(&st.store.pool).await.unwrap();

        let saved = crate::decisions::get(st, answered.id).await.unwrap().unwrap();
        assert_eq!(saved.state, "answered");
        assert_eq!(saved.context["archive_consumed"], true);
        assert_eq!(saved.context["input"]["body"]["ids"], json!([id]));
        assert!(saved.context["archive_result"].is_null(), "execution never produced a result");
        assert!(saved.context["archive_error"].as_str().unwrap().contains("home"), "the top-level failure is saved: {saved:?}");
        assert!(!archived(st, &id).await);
        let response = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{}/api/v1/decisions/{}", head.ui_addrs[0].port(), saved.id))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let visible: Value = response.json().await.unwrap();
        assert_eq!(visible["context"]["archive_error"], saved.context["archive_error"], "the decision API exposes the saved error");
        let requester = st.store.session("cleanup").await.unwrap().unwrap();
        assert_eq!((requester.archived, requester.status.as_str(), requester.last_seq), (1, "idle", 0));
        let queued: i64 =
            sqlx::query_scalar("SELECT count(*) FROM commands WHERE session_id = 'cleanup'").fetch_one(&st.store.pool).await.unwrap();
        assert_eq!(queued, 0, "failure reporting must not enqueue input for an archived session");
        assert!(connection.deliver.notified().now_or_never().is_none(), "no worker delivery wake");
        assert!(matches!(rx.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)), "no direct worker command");
        drop(connection);
        head.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_approval_cannot_be_replayed_concurrently_or_after_reopening() {
        let (_dir, head) = head().await;
        let st = &head.state;
        let id = doc(st, "One-use selection", "you").await;
        let asked = batch(st, std::slice::from_ref(&id)).await;
        let decision = asked["asked"].as_i64().unwrap();
        let d = crate::decisions::get(st, decision).await.unwrap().unwrap();
        let allow = c6_api::DecisionAnswer { option: Some("Allow".into()), ..Default::default() };
        let (a, b) =
            tokio::join!(crate::decisions::answer(st, decision, &allow, "test"), crate::decisions::answer(st, decision, &allow, "test"));
        assert_ne!(a.is_ok(), b.is_ok(), "exactly one answer may consume the selection: {a:?} {b:?}");
        assert!(archived(st, &id).await);
        super::super::set_archived(st, "home", &id, false).await.unwrap();
        let reopen = crate::decisions::edit(st, decision, &c6_api::DecisionEdit { state: Some("open".into()), ..Default::default() }).await;
        assert!(reopen.is_err(), "cleanup cannot be reopened");
        assert!(crate::agent_ops::decided(st, &d, Some("Allow")).await.is_err(), "stale concurrent snapshot cannot replay");
        assert!(!archived(st, &id).await);
        let asked = batch(st, std::slice::from_ref(&id)).await;
        let denied = answer(st, &asked, "Not now").await;
        assert_eq!(denied.context["archive_consumed"], true);
        assert!(crate::agent_ops::decided(st, &denied, Some("Allow")).await.is_err(), "Not now consumes the approval too");
        assert!(!archived(st, &id).await);
        head.shutdown().await.unwrap();
    }
}
