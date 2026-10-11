//! The memex (`c6m`): carbon6's knowledge service. A node on the fabric
//! (it attaches to the head like a worker, with the role `memex`) that
//! keeps items, passages, vectors and a graph of things, and answers the
//! head's `memex.*` calls. See docs/memex.md.

pub mod embed;
pub mod store;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use c6_api as api;
use c6_worker::link::HeadLink;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::store::{Store, clip};

/// Reciprocal-rank fusion constant: how much rank 1 beats rank 10.
const RRF_K: f64 = 60.0;
/// Candidates from each of keyword and meaning search.
const CANDIDATES: u32 = 60;

pub struct Memex {
    pub store: Store,
    pub embed: embed::Embedder,
    link: Arc<HeadLink>,
    last_error: Mutex<Option<String>>,
}

impl Memex {
    pub async fn open(dir: &Path, link: Arc<HeadLink>) -> anyhow::Result<Arc<Self>> {
        let store = Store::open(&dir.join("memex.db")).await?;
        Ok(Arc::new(Self { store, embed: embed::Embedder::new(link.clone()), link, last_error: Mutex::new(None) }))
    }

    /// Background work: keeps the settings fresh from the head, and embeds
    /// passages that have no vector yet.
    pub fn spawn_jobs(self: &Arc<Self>) {
        let m = self.clone();
        tokio::spawn(async move {
            let mut refreshed: Option<std::time::Instant> = None;
            loop {
                if m.link.is_connected() && refreshed.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
                    match m.link.call("config.get", json!({})).await {
                        Ok(c) => {
                            m.embed.set(c["embedding"]["connection_id"].as_str().map(|id| embed::Config {
                                connection_id: id.to_owned(),
                                model: c["embedding"]["model"].as_str().unwrap_or_default().to_owned(),
                            }));
                            refreshed = Some(std::time::Instant::now());
                        }
                        Err(e) => warn!("memex settings from the head: {e:#}"),
                    }
                }
                let wait = match m.embed_some().await {
                    Ok(0) => Duration::from_secs(2),
                    Ok(_) => Duration::from_millis(50),
                    Err(e) => {
                        warn!("embedding: {e:#}");
                        *m.last_error.lock().unwrap() = Some(format!("{e:#}"));
                        Duration::from_secs(30)
                    }
                };
                tokio::time::sleep(wait).await;
            }
        });
    }

    /// Embeds a batch of passages; how many.
    async fn embed_some(&self) -> anyhow::Result<usize> {
        let space = self.embed.space();
        if space.is_empty() || !self.link.is_connected() {
            return Ok(0);
        }
        let batch = self.store.unembedded(&space, 32).await?;
        if batch.is_empty() {
            return Ok(0);
        }
        let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
        let vectors = self.embed.embed(&texts).await?;
        for ((part, _), v) in batch.iter().zip(vectors) {
            self.store.put_vector(*part, &space, &v).await?;
        }
        *self.last_error.lock().unwrap() = None;
        Ok(batch.len())
    }

    /// The kinds a call may see: its scope (stamped by the head for
    /// sessions), else everything (you, through the UI).
    fn allowed(params: &Value) -> Vec<String> {
        match params["scope"].as_array() {
            Some(a) => a.iter().filter_map(|k| k.as_str().map(str::to_owned)).collect(),
            None => api::MEMEX_KINDS.iter().map(|k| k.to_string()).collect(),
        }
    }

    pub async fn handle_method(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let allowed = Self::allowed(&params);
        match method {
            "memex.ingest" => {
                let item: api::MemexItemIn = serde_json::from_value(params)?;
                let id = self.store.put(&item, &item.source).await?;
                Ok(json!({"id": id}))
            }
            "memex.delete" => {
                let id = match params["id"].as_str() {
                    Some(id) => Some(id.to_owned()),
                    None => self.store.id_of(params["source"].as_str().unwrap_or(""), params["external_id"].as_str().unwrap_or("")).await?,
                };
                let deleted = match id {
                    Some(id) => self.store.delete(&id).await?,
                    None => false,
                };
                Ok(json!({"deleted": deleted}))
            }
            "memex.search" => {
                let q: api::MemexQuery = serde_json::from_value(params.clone())?;
                Ok(json!(self.search(&q, &allowed).await?))
            }
            "memex.get" => {
                let id = params["id"].as_str().unwrap_or_default();
                if let Some(item) = self.store.item(id, &allowed).await? {
                    return Ok(json!({"item": item}));
                }
                match self.thing(id, &allowed).await? {
                    Some(t) => Ok(json!({"thing": t})),
                    None => anyhow::bail!("nothing in the memex is {id:?} (or it is outside what this session may see)"),
                }
            }
            "memex.related" => {
                let id = params["id"].as_str().unwrap_or_default();
                let thing = self.store.thing(id).await?.ok_or_else(|| anyhow::anyhow!("no thing {id:?}"))?;
                Ok(json!(self.store.related(&thing.id, &allowed, 20).await?))
            }
            "memex.timeline" => {
                let id = params["about"].as_str().unwrap_or_default();
                let thing = self.store.thing(id).await?.ok_or_else(|| anyhow::anyhow!("no thing {id:?}"))?;
                let limit = params["limit"].as_u64().unwrap_or(30).min(200) as u32;
                Ok(json!(self.store.about(Some(&thing.id), &allowed, limit).await?))
            }
            "memex.note" => {
                anyhow::ensure!(allowed.iter().any(|k| k == "note"), "notes are outside what this session may write");
                let text = params["text"].as_str().unwrap_or_default().trim().to_owned();
                anyhow::ensure!(!text.is_empty(), "a note needs text");
                let labels: Vec<String> = ["labels", "about"]
                    .iter()
                    .flat_map(|k| params[*k].as_array().cloned().unwrap_or_default())
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect();
                let item = api::MemexItemIn {
                    kind: "note".into(),
                    source: params["made_by"].as_str().unwrap_or("you").to_owned(),
                    external_id: String::new(),
                    title: clip(text.lines().next().unwrap_or(""), 80),
                    text,
                    labels,
                    ..Default::default()
                };
                let id = self.store.put(&item, &item.source).await?;
                Ok(json!({"id": id}))
            }
            "memex.brief" => {
                let labels: Vec<String> = params["labels"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                    .unwrap_or_default();
                let chars = params["chars"].as_u64().unwrap_or(4000) as usize;
                Ok(json!(self.brief(&labels, params["text"].as_str().unwrap_or(""), chars, &allowed).await?))
            }
            "memex.mention" => Ok(json!(self.store.find_things(params["q"].as_str().unwrap_or(""), 20).await?)),
            "memex.status" => {
                let mut s = self.store.status(&self.embed.space()).await?;
                s.last_error = self.last_error.lock().unwrap().clone();
                Ok(json!(s))
            }
            other => anyhow::bail!("the memex has no method {other}"),
        }
    }

    /// Keyword and meaning search, merged by reciprocal rank.
    pub async fn search(&self, q: &api::MemexQuery, allowed: &[String]) -> anyhow::Result<Vec<api::MemexHit>> {
        let kinds: Vec<String> =
            if q.kinds.is_empty() { allowed.to_vec() } else { q.kinds.iter().filter(|k| allowed.contains(k)).cloned().collect() };
        let about = self.store.things_for(&q.about).await?;
        if kinds.is_empty() || q.query.trim().is_empty() || (!q.about.is_empty() && about.len() < q.about.len()) {
            return Ok(Vec::new());
        }
        let limit = q.limit.unwrap_or(10).clamp(1, 50) as usize;
        // part → (score, item, how it matched)
        let mut parts: HashMap<i64, (f64, String, Vec<String>)> = HashMap::new();
        for (rank, f) in self.store.keyword(&q.query, &kinds, &about, CANDIDATES).await?.into_iter().enumerate() {
            let e = parts.entry(f.part_id).or_insert((0.0, f.item_id, Vec::new()));
            e.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
            e.2.push("keyword".into());
        }
        let space = self.embed.space();
        if !space.is_empty() {
            match self.embed.embed(std::slice::from_ref(&q.query)).await {
                Ok(v) => {
                    let mut scored: Vec<(i64, String, f32)> = self
                        .store
                        .vectors(&space, &kinds, &about)
                        .await?
                        .into_iter()
                        .map(|(p, i, vec)| (p, i, embed::similarity(&v[0], &vec)))
                        .filter(|(_, _, s)| *s > 0.0)
                        .collect();
                    scored.sort_by(|a, b| b.2.total_cmp(&a.2));
                    for (rank, (p, i, _)) in scored.into_iter().take(CANDIDATES as usize).enumerate() {
                        let e = parts.entry(p).or_insert((0.0, i, Vec::new()));
                        e.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
                        e.2.push("meaning".into());
                    }
                }
                Err(e) => warn!("embedding a query (keyword search only): {e:#}"),
            }
        }
        // Each item at its best passage.
        let mut items: HashMap<String, (f64, i64, Vec<String>)> = HashMap::new();
        for (part, (score, item, how)) in parts {
            let e = items.entry(item).or_insert((0.0, part, Vec::new()));
            if score > e.0 {
                e.0 = score;
                e.1 = part;
            }
            for h in how {
                if !e.2.contains(&h) {
                    e.2.push(h);
                }
            }
        }
        let mut ranked: Vec<(String, f64, Option<i64>, Vec<String>)> =
            items.into_iter().map(|(id, (s, p, how))| (id, s, Some(p), how)).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.truncate(limit);
        self.store.hits(&ranked, &q.query).await
    }

    async fn thing(&self, key_or_id: &str, allowed: &[String]) -> anyhow::Result<Option<api::MemexThing>> {
        let Some(t) = self.store.thing(key_or_id).await? else { return Ok(None) };
        let notes =
            if allowed.iter().any(|k| k == "note") { self.store.about(Some(&t.id), &["note".to_owned()], 20).await? } else { Vec::new() };
        let others: Vec<String> = allowed.iter().filter(|k| *k != "note").cloned().collect();
        Ok(Some(api::MemexThing {
            notes,
            items: self.store.about(Some(&t.id), &others, 20).await?,
            related: self.store.related(&t.id, allowed, 10).await?,
            thing: t,
        }))
    }

    /// Context for a session about `labels` (and its first message), within
    /// `chars`: notes about them (and notes for every session), recent
    /// sessions about them, and what else matches the message.
    pub async fn brief(&self, labels: &[String], text: &str, chars: usize, allowed: &[String]) -> anyhow::Result<api::MemexBrief> {
        let mut refs: Vec<String> = Vec::new();
        let mut sections: Vec<String> = Vec::new();
        let notes_ok = allowed.iter().any(|k| k == "note");
        // Not worker: (every session on it would match).
        let labels: Vec<String> = labels.iter().filter(|l| !l.starts_with("worker:")).cloned().collect();
        let things = self.store.things_for(&labels).await?;
        let mut names = Vec::new();
        let mut notes = Vec::new();
        let mut recent = Vec::new();
        if notes_ok {
            notes.extend(self.store.about(None, &["note".to_owned()], 15).await?);
        }
        for t in &things {
            let Some(r) = self.store.thing(t).await? else { continue };
            names.push(r.label.clone());
            if notes_ok {
                notes.extend(self.store.about(Some(t), &["note".to_owned()], 15).await?);
            }
            if allowed.iter().any(|k| k == "session") {
                recent.extend(self.store.about(Some(t), &["session".to_owned()], 5).await?);
            }
        }
        let mut seen = std::collections::HashSet::new();
        notes.retain(|n| seen.insert(n.id.clone()));
        notes.sort_by_key(|n| std::cmp::Reverse(n.created_at));
        if !notes.is_empty() {
            let lines: Vec<String> = notes
                .iter()
                .map(|n| {
                    refs.push(n.id.clone());
                    format!("- {} [{}]", clip(&n.snippet, 400), n.id)
                })
                .collect();
            sections.push(format!("## Notes\n{}", lines.join("\n")));
        }
        recent.retain(|r| seen.insert(r.id.clone()));
        recent.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        recent.truncate(6);
        if !recent.is_empty() {
            let lines: Vec<String> = recent
                .iter()
                .map(|r| {
                    refs.push(r.id.clone());
                    format!("- {} — {} [{}]", r.title, clip(&last_answer(&r.snippet), 200), r.id)
                })
                .collect();
            sections.push(format!("## Recent sessions about these\n{}", lines.join("\n")));
        }
        if !text.trim().is_empty() {
            let q = api::MemexQuery { query: text.chars().take(500).collect(), limit: Some(5), ..Default::default() };
            let hits: Vec<api::MemexHit> =
                self.search(&q, allowed).await.unwrap_or_default().into_iter().filter(|h| seen.insert(h.id.clone())).collect();
            if !hits.is_empty() {
                let lines: Vec<String> = hits
                    .iter()
                    .map(|h| {
                        refs.push(h.id.clone());
                        format!("- ({}) {}: {} [{}]", h.kind, h.title, clip(&h.snippet, 240), h.id)
                    })
                    .collect();
                sections.push(format!("## Possibly relevant\n{}", lines.join("\n")));
            }
        }
        if sections.is_empty() {
            return Ok(api::MemexBrief::default());
        }
        let about = if names.is_empty() { String::new() } else { format!(" about {}", names.join(", ")) };
        let mut out = format!(
            "From your memex{about}. Items are cited by [id]; memex_get fetches one in full and memex_search finds more. They may be out of date: trust what you observe.\n\n{}",
            sections.join("\n\n")
        );
        if out.chars().count() > chars {
            out = format!("{}…", out.chars().take(chars).collect::<String>());
        }
        Ok(api::MemexBrief { text: out, refs })
    }
}

/// A session digest's last answer line (what it concluded).
fn last_answer(snippet: &str) -> String {
    snippet.rsplit("Answer:").next().unwrap_or(snippet).trim().to_owned()
}

#[async_trait::async_trait]
impl c6_worker::Extension for Memex {
    fn capabilities(&self) -> Vec<String> {
        vec!["memex".into()]
    }

    async fn handle(&self, method: &str, params: Value) -> Option<anyhow::Result<Value>> {
        if !method.starts_with("memex.") {
            return None;
        }
        let r = self.handle_method(method, params).await;
        if let Err(e) = &r {
            info!(%method, "memex call failed: {e:#}");
        }
        Some(r)
    }
}
