//! Leaving the decks app (docs/docs.md §13): its decks come in once as
//! carbon6 decks. Each record of its `decks` collection becomes a deck with
//! its title, its brief as the first slide's notes and its theme; the
//! slides come from its `deck.html` (found on a connected worker, split by
//! the render kit: the head parses no HTML), each section an html slide
//! with the deck's own CSS; its PDFs are kept as past exports. The app's
//! themes come in as theme records. Then the app can be removed.

use std::collections::BTreeMap;

use base64::Engine;
use c6_api as api;
use c6_core::now_ms;
use serde_json::{Value, json};
use sqlx::Row as _;

use crate::{HeadState, bad_request};

/// The app's package name.
pub const APP: &str = "decks";
/// Calls to workers for a deck's files.
const READ: std::time::Duration = std::time::Duration::from_secs(120);

/// What bringing in would do in a realm: whether the app is there, and how
/// many of its decks are still to come in.
pub async fn status(st: &HeadState, realm: &str) -> anyhow::Result<Value> {
    let installed = installed(st, realm).await?;
    let records = if installed { records(st, realm, "decks").await? } else { Vec::new() };
    let mut left = 0;
    for (id, _) in &records {
        if brought(st, realm, id).await?.is_none() {
            left += 1;
        }
    }
    Ok(json!({"app": installed, "decks": records.len(), "left": left}))
}

async fn installed(st: &HeadState, realm: &str) -> anyhow::Result<bool> {
    Ok(crate::packages::installs(st).await?.iter().any(|i| i.package == APP && i.realm == realm && i.state == "installed"))
}

/// A collection's records in a realm: (id, fields).
async fn records(st: &HeadState, realm: &str, collection: &str) -> anyhow::Result<Vec<(String, Value)>> {
    Ok(sqlx::query("SELECT id, fields FROM records WHERE realm = ? AND collection = ? ORDER BY created_at")
        .bind(realm)
        .bind(collection)
        .fetch_all(&st.store.pool)
        .await?
        .iter()
        .map(|r| (r.get("id"), serde_json::from_str(&r.get::<String, _>("fields")).unwrap_or(Value::Null)))
        .collect())
}

async fn brought(st: &HeadState, realm: &str, record: &str) -> anyhow::Result<Option<String>> {
    let pool = super::pool(st, realm).await?;
    Ok(sqlx::query_scalar("SELECT doc_id FROM doc_sources WHERE source = ?")
        .bind(format!("decks-app:{record}"))
        .fetch_optional(&pool)
        .await?)
}

/// A file of a deck from whichever connected worker has it.
async fn fetch(st: &HeadState, realm: &str, paths: &[String]) -> Option<(String, Vec<u8>)> {
    let rows = st.store.workers().await.ok()?;
    for r in rows.iter().filter(|r| {
        r.revoked_at.is_none()
            && r.role != "memex"
            && crate::realms::serves(&serde_json::from_str::<Vec<String>>(&r.realms).unwrap_or_default(), realm)
    }) {
        let Some(w) = r.worker_id() else { continue };
        if st.registry.get(w).is_none() {
            continue;
        }
        for p in paths {
            if let Ok(v) = st.registry.call(w, "fs.read_file", json!({"path": p, "max": 50u64 << 20}), READ).await
                && let Some(data) = v["data"].as_str().and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok())
            {
                return Some((p.clone(), data));
            }
        }
    }
    None
}

/// Brings in the app's decks (those not brought in yet) and its themes.
/// What came in, and what didn't with why.
pub async fn bring_in(st: &HeadState, realm: &str, by: &str) -> anyhow::Result<Value> {
    if !installed(st, realm).await? {
        return Err(bad_request(format!("the decks app isn't installed in realm {realm}: there is nothing to bring in")));
    }
    // Its themes, as theme records (the realm's own win by name).
    let mut themes = crate::decks::themes(st, realm).await?;
    let mut themes_in = Vec::new();
    for (_, f) in records(st, realm, "themes").await? {
        let t: api::slides::DeckTheme = match serde_json::from_value(f) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if !t.name.trim().is_empty() && !t.css.trim().is_empty() && !themes.iter().any(|x| x.name.eq_ignore_ascii_case(&t.name)) {
            themes_in.push(t.name.clone());
            themes.push(t);
        }
    }
    if !themes_in.is_empty() {
        crate::decks::put(st, realm, &themes, by).await?;
    }
    let mut made = Vec::new();
    let mut skipped = Vec::new();
    for (rid, f) in records(st, realm, "decks").await? {
        if let Some(doc) = brought(st, realm, &rid).await? {
            skipped.push(json!({"record": rid, "title": f["title"], "why": "brought in already", "doc": doc}));
            continue;
        }
        match one(st, realm, &rid, &f, by).await {
            Ok(doc) => made.push(json!({"record": rid, "title": f["title"], "doc": doc})),
            Err(e) => skipped.push(json!({"record": rid, "title": f["title"], "why": format!("{e:#}")})),
        }
    }
    Ok(json!({"made": made, "skipped": skipped, "themes": themes_in}))
}

/// One record of the app's `decks` collection as a deck.
async fn one(st: &HeadState, realm: &str, record: &str, f: &Value, by: &str) -> anyhow::Result<String> {
    let title = f["title"].as_str().unwrap_or("").trim();
    let title = if title.is_empty() { "A deck from the decks app" } else { title };
    let folder = f["folder"].as_str().unwrap_or("").trim_end_matches('/').to_owned();
    let files: Vec<String> = f["files"].as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
    let at = |name: &str| -> Vec<String> {
        let mut p: Vec<String> = files.iter().filter(|x| x.ends_with(name)).cloned().collect();
        if !folder.is_empty() {
            p.push(format!("{folder}/{name}"));
        }
        p
    };
    let Some((_, html)) = fetch(st, realm, &at("deck.html")).await else {
        return Err(bad_request(format!(
            "its deck.html isn't on a connected worker (looked for {}): connect the worker it was made on and bring it in again",
            at("deck.html").join(", ")
        )));
    };
    let out = super::export::run_kit(
        st,
        realm,
        "split",
        json!({"html": "deck.html", "out": "out/split.json"}),
        BTreeMap::from([("deck.html".to_owned(), html)]),
    )
    .await?;
    let split: Value = serde_json::from_slice(out.get("split.json").ok_or_else(|| anyhow::anyhow!("the render kit split nothing"))?)?;
    let style = split["style"].as_str().unwrap_or("");
    let sections: Vec<&str> = split["sections"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    if sections.is_empty() {
        return Err(bad_request("its deck.html has no slides (<section class=\"slide\">)"));
    }
    let brief = f["brief"].as_str().unwrap_or("").trim().replace("-->", "-- >");
    let mut slides = Vec::new();
    for (n, sec) in sections.iter().enumerate() {
        // One html block: the deck's CSS, then the slide (blank lines out, so
        // it stays one block).
        let html = format!("<style>{style}</style>{sec}");
        let html: String = html.lines().filter(|l| !l.trim().is_empty()).collect::<Vec<_>>().join("\n");
        let mut md = format!("<!-- layout: html -->\n{html}");
        if n == 0 && !brief.is_empty() {
            md.push_str(&format!("\n\n<!-- notes: {brief} -->"));
        }
        slides.push(md);
    }
    let theme = f["theme"].as_str().unwrap_or("").to_owned();
    let n = api::docs::DocNew {
        kind: "deck".into(),
        title: title.to_owned(),
        markdown: slides.join("\n\n---\n\n"),
        theme,
        ..Default::default()
    };
    let (doc, _) = super::create(st, realm, &n, by).await?;
    let pool = super::pool(st, realm).await?;
    sqlx::query("INSERT OR REPLACE INTO doc_sources (source, doc_id, at) VALUES (?, ?, ?)")
        .bind(format!("decks-app:{record}"))
        .bind(&doc.summary.id)
        .bind(now_ms())
        .execute(&pool)
        .await?;
    // Its PDFs, as past exports.
    for pdf in files.iter().filter(|x| x.to_lowercase().ends_with(".pdf")) {
        if let Some((_, bytes)) = fetch(st, realm, std::slice::from_ref(pdf)).await {
            super::export::keep(st, realm, &doc, "pdf", bytes, "decks app").await?;
        }
    }
    Ok(doc.summary.id)
}
