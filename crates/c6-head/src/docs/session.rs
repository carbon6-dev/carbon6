//! The `doc` tool (docs/docs.md §9): sessions write documents the way you
//! do, through the same operations and guards. A session reads any
//! document of its realm. It writes the documents it made, and those of
//! the work it belongs to, without asking; any other one needs your OK
//! once (a decision per document), unless the realm's Settings → Docs
//! says otherwise. How agents write is the editable prompt `docs.style`.

use c6_api::docs::{DocBatch, DocNew, DocQuery, DocRead, DocsSettings};
use c6_core::now_ms;
use serde_json::{Value, json};

use crate::{HeadState, bad_request, forbidden};

/// The per-realm setting (`DocsSettings`).
pub const SETTING: &str = "docs";

pub async fn settings(st: &HeadState, realm: &str) -> DocsSettings {
    st.store.setting_for(SETTING, realm).await.ok().flatten().and_then(|v| serde_json::from_str(&v).ok()).unwrap_or_default()
}

/// The work items a session belongs to (through its root).
async fn work_of(st: &HeadState, session: &str) -> Vec<String> {
    crate::work::holding(st, &format!("session:{session}")).await.unwrap_or_default()
}

/// Whether `session` may write document `id` in `realm`, and if not why.
pub async fn may_write(st: &HeadState, realm: &str, id: &str, session: &str) -> anyhow::Result<Result<(), String>> {
    let by = format!("session:{session}");
    let pool = super::pool(st, realm).await?;
    let made: Option<String> =
        sqlx::query_scalar("SELECT created_by FROM docs WHERE id = ? AND deleted_at IS NULL").bind(id).fetch_optional(&pool).await?;
    let Some(made) = made else { return Ok(Err(format!("no document {id}"))) };
    let s = settings(st, realm).await;
    if made == by || s.write == "realm" {
        return Ok(Ok(()));
    }
    if s.write != "own" {
        // Its work's documents (and, with projects, its project's).
        let reference = format!("doc:{id}");
        for w in work_of(st, session).await {
            if crate::work::holds(st, &w, &reference).await {
                return Ok(Ok(()));
            }
            // Its project's documents too (docs/projects.md §3).
            if let Some(p) = crate::work::project_of(st, &w).await
                && crate::projects::holds_doc(st, &p, id).await
            {
                return Ok(Ok(()));
            }
        }
        let granted: i64 = sqlx::query_scalar("SELECT count(*) FROM doc_writers WHERE doc_id = ? AND writer = ?")
            .bind(id)
            .bind(&by)
            .fetch_one(&pool)
            .await?;
        if granted > 0 {
            return Ok(Ok(()));
        }
    }
    Ok(Err(s.write))
}

/// Lets a session write a document (your answer to its decision).
pub async fn grant(st: &HeadState, realm: &str, id: &str, session: &str) -> anyhow::Result<()> {
    let pool = super::pool(st, realm).await?;
    sqlx::query("INSERT OR IGNORE INTO doc_writers (doc_id, writer, granted_at) VALUES (?, ?, ?)")
        .bind(id)
        .bind(format!("session:{session}"))
        .bind(now_ms())
        .execute(&pool)
        .await?;
    Ok(())
}

/// Asks you, once per document, to let the session write it.
async fn ask(st: &HeadState, realm: &str, id: &str, session: &str) -> anyhow::Result<i64> {
    let title = super::title_of(st, realm, id).await.map(|t| t.0).unwrap_or_else(|| id.to_owned());
    let who = st.store.session(session).await?.map(|s| s.title).filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "A session".into());
    let d = c6_api::DecisionIn {
        kind: "approve".into(),
        title: format!("Let {who} edit \"{title}\"?"),
        document: format!(
            "The session wants to change the document [{title}](/docs/{id}), which isn't its work's. Allowing it lets this session \
             edit that document from now on; others still ask."
        ),
        context: json!({"session": session, "doc": id}),
        source: "docs".into(),
        options: vec![
            c6_api::DecisionOption { label: "Allow".into(), ..Default::default() },
            c6_api::DecisionOption { label: "Deny".into(), ..Default::default() },
        ],
        ..Default::default()
    };
    let d = crate::decisions::open(st, &d, realm, &format!("docwrite:{realm}/{id}:{session}")).await?;
    Ok(d.id)
}

/// What a tool result says about a document.
fn about(d: &c6_api::docs::DocSummary) -> Value {
    json!({"id": d.id, "kind": d.kind, "title": d.title, "rev": d.rev, "ref": format!("doc:{}", d.id), "link": format!("/docs/{}", d.id)})
}

/// Blocks by id with what they are (kind, first words) and their hash, so
/// a model never has to guess which id is which.
fn shown(doc: &c6_api::docs::Doc, hashes: &serde_json::Map<String, Value>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut walk = |b: &c6_api::docs::DocBlock| {
        if let Some(h) = hashes.get(&b.id) {
            let words = if b.kind == "pending" {
                b.attrs["intent"].as_str().unwrap_or("").to_owned()
            } else {
                super::markdown::first_words(&b.kind, &b.text, 50)
            };
            out.push(json!({"id": b.id, "kind": b.kind, "words": words, "hash": h}));
        }
    };
    for t in &doc.tabs {
        for b in &t.blocks {
            walk(b);
            b.children.iter().for_each(&mut walk);
        }
    }
    out
}

/// The blocks still to be written: id, intent, and who is writing each.
fn pending(doc: &c6_api::docs::Doc) -> Vec<Value> {
    doc.tabs
        .iter()
        .flat_map(|t| t.blocks.iter())
        .filter(|b| b.kind == "pending")
        .map(|b| json!({"id": b.id, "intent": b.attrs["intent"], "writer": b.attrs["by"]}))
        .collect()
}

/// The `doc` tool for a session (`session` is its root for side chats and
/// subagents: they work for its conversation).
pub async fn for_session(st: &HeadState, session: &str, p: &Value) -> anyhow::Result<Value> {
    let realm = crate::realms::of_session(st, session).await?.unwrap_or_else(|| crate::realms::HOME.to_owned());
    let by = format!("session:{session}");
    let id = p["id"].as_str().map(str::trim).unwrap_or("").trim_start_matches("doc:").to_owned();
    let need_id = || if id.is_empty() { Err(bad_request("say which document: id")) } else { Ok(()) };
    // A read view named as the action (models write {"action": "outline"}).
    let mut p = p.clone();
    if let Some(v) =
        p["action"].as_str().filter(|a| matches!(*a, "outline" | "block" | "slide" | "search" | "since" | "markdown")).map(str::to_owned)
    {
        p["view"] = json!(v);
        p["action"] = json!("read");
    }
    let p = &p;
    match p["action"].as_str().unwrap_or("") {
        "guide" => {
            let style = crate::prompts::get(st, "docs.style", &realm).await;
            // A deck's flow, design and themes too.
            let deck = p["kind"] == "deck" || (!id.is_empty() && super::title_of(st, &realm, &id).await.is_some_and(|t| t.1 == "deck"));
            if deck {
                return Ok(json!({"style": style, "decks": crate::decks::guide(st, &realm).await}));
            }
            Ok(json!({"style": style, "note": "For a deck, ask the guide with kind deck."}))
        }
        "list" => {
            let q = DocQuery {
                kind: p["kind"].as_str().map(str::to_owned),
                q: p["q"].as_str().map(str::to_owned),
                limit: Some(50),
                ..Default::default()
            };
            let all = super::list(st, &realm, &q).await?;
            // Its work's documents first.
            let mut mine = Vec::new();
            for w in work_of(st, session).await {
                for r in crate::work::refs(st, &w).await {
                    if let Some(d) = r.strip_prefix("doc:") {
                        mine.push(d.to_owned());
                    }
                }
            }
            let (ours, rest): (Vec<_>, Vec<_>) = all.into_iter().partition(|d| mine.contains(&d.id) || d.created_by == by);
            let line = |d: &c6_api::docs::DocSummary| json!({"id": d.id, "kind": d.kind, "title": d.title, "rev": d.rev, "updated_by": d.updated_by, "pending": d.pending});
            Ok(json!({"yours": ours.iter().map(line).collect::<Vec<_>>(), "others": rest.iter().map(line).collect::<Vec<_>>()}))
        }
        "create" => {
            let n: DocNew =
                serde_json::from_value(p.clone()).map_err(|e| bad_request(format!("create takes kind, title, markdown, blocks: {e}")))?;
            let (doc, applied) = super::create(st, &realm, &n, &by).await?;
            // It belongs to the session's work, so it outlives the session.
            for w in work_of(st, session).await {
                let r = c6_api::work::WorkRef { reference: format!("doc:{}", doc.summary.id), title: doc.summary.title.clone() };
                if let Err(e) = crate::work::attach_by(st, &w, &r, &by).await {
                    tracing::warn!(work = %w, "attaching a document: {e:#}");
                }
                // And to its project, so it outlives the work.
                if let Some(p) = crate::work::project_of(st, &w).await
                    && let Ok(Some(row)) = crate::projects::load(st, &p).await
                {
                    let d = c6_api::projects::ProjectDocIn { doc: doc.summary.id.clone(), role: String::new() };
                    if let Err(e) = crate::projects::add_doc_in(st, &row, &d, &by).await {
                        tracing::warn!(project = %p, "attaching a document: {e:#}");
                    }
                }
            }
            Ok(json!({"did": "created", "doc": about(&doc.summary), "keys": applied.keys, "blocks": shown(&doc, &applied.hashes),
                      "pending": pending(&doc),
                      "note": "It is open for the user now. Fill each pending block with its own edit: {\"op\": \"replace\", \"id\": \
                               <its id from pending>, \"markdown\": ...}, one section or slide per call."}))
        }
        "read" => {
            need_id()?;
            let q: DocRead =
                serde_json::from_value(p.clone()).map_err(|e| bad_request(format!("read takes view, id, q, rev, tab, at: {e}")))?;
            let mut q = q;
            q.id = p["block"].as_str().map(str::to_owned).or(q.id.filter(|b| *b != id));
            let v = super::read(st, &realm, &id, &q).await?;
            Ok(v)
        }
        "edit" => {
            need_id()?;
            let b: DocBatch = serde_json::from_value(json!({"ops": p["ops"], "answering": p["answering"].as_str().unwrap_or("")}))
                .map_err(|e| {
                    bad_request(format!(
                        "edit takes id (the document) and ops, a list of operations, each naming its block by id: \
                         [{{\"op\": \"replace\", \"id\": \"<block id>\", \"markdown\": \"## Heading\\n\\nText\"}}] fills a pending block you \
                         are writing; other whole-block replaces add \"ifRev\" and \"ifHash\" from your read. ({e})"
                    ))
                })?;
            match may_write(st, &realm, &id, session).await? {
                Ok(()) => {}
                Err(why) if why.starts_with("no document") => return Err(bad_request(why)),
                Err(why) if why == "own" => {
                    return Err(forbidden("this realm lets sessions edit only the documents they made: make a new one, or tell the user"));
                }
                Err(_) => {
                    let decision = ask(st, &realm, &id, session).await?;
                    return Err(forbidden(format!(
                        "this document isn't your work's: the user is asked to let you edit it (decision {decision}). Nothing changed. \
                         Carry on with other things and try again once they allow it, or make a new document."
                    )));
                }
            }
            let a = super::apply(st, &realm, &id, &b, &by, None).await?;
            let doc = super::get(st, &realm, &id).await?;
            Ok(json!({"did": "edited", "doc": about(&doc.summary), "rev": a.rev, "keys": a.keys, "changed": shown(&doc, &a.hashes),
                      "pending": pending(&doc)}))
        }
        "restore" => {
            need_id()?;
            let r = c6_api::docs::DocRestore {
                rev: p["rev"].as_i64().ok_or_else(|| bad_request("restore takes rev (the version), and block to bring back one block"))?,
                block: p["block"].as_str().map(str::to_owned),
            };
            if let Err(why) = may_write(st, &realm, &id, session).await? {
                return Err(forbidden(format!("you may not change this document ({why}): ask the user, or edit with action edit to ask")));
            }
            let a = super::restore(st, &realm, &id, &r, &by).await?;
            let doc = super::get(st, &realm, &id).await?;
            Ok(json!({"did": "restored", "doc": about(&doc.summary), "rev": a.rev, "hashes": a.hashes}))
        }
        "history" => {
            need_id()?;
            Ok(json!({"versions": super::history(st, &realm, &id, 50).await?}))
        }
        other => Err(bad_request(format!("doc's actions are guide, list, create, read, edit, history and restore, not {other:?}"))),
    }
}

/// The documents a decision lets a session write: answered Allow.
pub async fn decided(st: &HeadState, reference: &str, option: Option<&str>) -> anyhow::Result<()> {
    // docwrite:<realm>/<doc>:<session>
    let rest = reference.strip_prefix("docwrite:").unwrap_or_default();
    let (place, session) = rest.split_once(':').ok_or_else(|| bad_request("a bad reference"))?;
    let (realm, id) = place.split_once('/').ok_or_else(|| bad_request("a bad reference"))?;
    if option == Some("Allow") {
        grant(st, realm, id, session).await?;
    }
    Ok(())
}
