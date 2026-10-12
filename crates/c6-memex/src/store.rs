//! The memex's store: SQLite (WAL) with full-text search over passages,
//! vectors per embedding space, and the graph of things and links.

use std::path::Path;
use std::str::FromStr;

use anyhow::Context;
use c6_api as api;
use sqlx::Row;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

pub struct Store {
    pub pool: SqlitePool,
}

/// Passages aim for this many characters (split at paragraphs).
const PART_CHARS: usize = 1200;
/// Kinds of label that name a run, not a thing worth a node.
const NOT_THINGS: [&str; 3] = ["run", "wfrun", "model"];

/// A passage found by search.
pub struct Found {
    pub part_id: i64,
    pub item_id: String,
}

impl Store {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let opts = SqliteConnectOptions::from_str("sqlite:")?
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(10));
        let pool =
            SqlitePoolOptions::new().max_connections(8).connect_with(opts).await.with_context(|| format!("opening {}", path.display()))?;
        sqlx::migrate!("./migrations").run(&pool).await.context("memex migrations")?;
        Ok(Self { pool })
    }

    /// Stores an item (replacing the one with its source and external id),
    /// splits its text into passages, and links it to the things its labels
    /// stand for. Its id.
    pub async fn put(&self, it: &api::MemexItemIn, made_by: &str) -> anyhow::Result<String> {
        anyhow::ensure!(!it.kind.trim().is_empty() && !it.source.trim().is_empty(), "an item needs a kind and a source");
        let now = c6_core::now_ms();
        let external = if it.external_id.is_empty() { uuid::Uuid::now_v7().to_string() } else { it.external_id.clone() };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let existing: Option<(String, String)> = sqlx::query_as("SELECT id, body FROM items WHERE source = ? AND external_id = ?")
            .bind(&it.source)
            .bind(&external)
            .fetch_optional(&mut *tx)
            .await?;
        let id = match &existing {
            Some((id, _)) => id.clone(),
            None => uuid::Uuid::now_v7().to_string(),
        };
        let text_changed = existing.as_ref().is_none_or(|(_, body)| *body != it.text);
        sqlx::query(
            "INSERT INTO items (id, kind, source, external_id, title, body, url, created_at, added_at, updated_at, meta) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(source, external_id) DO UPDATE SET \
             kind = excluded.kind, title = excluded.title, body = excluded.body, url = excluded.url, updated_at = excluded.updated_at, meta = excluded.meta",
        )
        .bind(&id)
        .bind(&it.kind)
        .bind(&it.source)
        .bind(&external)
        .bind(&it.title)
        .bind(&it.text)
        .bind(&it.url)
        .bind(it.created_at.unwrap_or(now))
        .bind(now)
        .bind(now)
        .bind(if it.meta.is_null() { "{}".to_owned() } else { it.meta.to_string() })
        .execute(&mut *tx)
        .await?;
        if text_changed {
            sqlx::query("DELETE FROM vectors WHERE part_id IN (SELECT id FROM parts WHERE item_id = ?)")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM parts WHERE item_id = ?").bind(&id).execute(&mut *tx).await?;
            // The title is searchable too.
            let text = if it.title.is_empty() || it.text.starts_with(it.title.trim_end_matches('…')) {
                it.text.clone()
            } else {
                format!("{}\n\n{}", it.title, it.text)
            };
            for (i, p) in chunk(&text).into_iter().enumerate() {
                sqlx::query("INSERT INTO parts (item_id, ordinal, text) VALUES (?, ?, ?)")
                    .bind(&id)
                    .bind(i as i64)
                    .bind(p)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        // Its labels, as observed links (replacing what this source said).
        sqlx::query("DELETE FROM links WHERE src_item = ? AND rel = 'about' AND made_by = ?")
            .bind(&id)
            .bind(made_by)
            .execute(&mut *tx)
            .await?;
        for l in &it.labels {
            if let Some(thing) = thing_for_label(&mut tx, l, now).await? {
                sqlx::query(
                    "INSERT INTO links (src_item, rel, dst_thing, basis, made_by, created_at) VALUES (?, 'about', ?, 'observed', ?, ?)",
                )
                .bind(&id)
                .bind(&thing)
                .bind(made_by)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(id)
    }

    /// Replaces an item's title and text (a caption, a transcript), with
    /// its passages.
    pub async fn set_text(&self, id: &str, title: &str, text: &str) -> anyhow::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE items SET title = ?, body = ?, updated_at = ? WHERE id = ?")
            .bind(title)
            .bind(text)
            .bind(c6_core::now_ms())
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM vectors WHERE part_id IN (SELECT id FROM parts WHERE item_id = ?)").bind(id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM parts WHERE item_id = ?").bind(id).execute(&mut *tx).await?;
        for (i, p) in chunk(&format!("{title}\n\n{text}")).into_iter().enumerate() {
            sqlx::query("INSERT INTO parts (item_id, ordinal, text) VALUES (?, ?, ?)")
                .bind(id)
                .bind(i as i64)
                .bind(p)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Deletes an item and what derives from it. False: no such item.
    pub async fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM faces WHERE item_id = ?").bind(id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM media WHERE item_id = ?").bind(id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM vectors WHERE part_id IN (SELECT id FROM parts WHERE item_id = ?)").bind(id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM parts WHERE item_id = ?").bind(id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM evidence WHERE item_id = ?").bind(id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM links WHERE src_item = ? OR dst_item = ?").bind(id).bind(id).execute(&mut *tx).await?;
        let n = sqlx::query("DELETE FROM items WHERE id = ?").bind(id).execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(n > 0)
    }

    pub async fn id_of(&self, source: &str, external_id: &str) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT id FROM items WHERE source = ? AND external_id = ?")
            .bind(source)
            .bind(external_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// The things these labels or thing ids stand for (existing ones only).
    pub async fn things_for(&self, refs: &[String]) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        for r in refs {
            let id: Option<String> = sqlx::query_scalar(
                "SELECT id FROM things WHERE key = ?1 OR id = ?1 UNION SELECT thing_id FROM thing_keys WHERE key = ?1 LIMIT 1",
            )
            .bind(r.trim())
            .fetch_optional(&self.pool)
            .await?;
            if let Some(id) = id.filter(|i| !out.contains(i)) {
                out.push(id);
            }
        }
        Ok(out)
    }

    /// Keyword search: passages of items of `kinds` (about all of `about`,
    /// if given), best first.
    pub async fn keyword(&self, query: &str, kinds: &[String], about: &[String], limit: u32) -> anyhow::Result<Vec<Found>> {
        let words: Vec<String> =
            query.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(|w| format!("\"{}\"", w.replace('"', ""))).collect();
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let and = words.join(" ");
        let mut found = self.fts(&and, kinds, about, limit).await?;
        // Every word, else any of them.
        if found.is_empty() && words.len() > 1 {
            found = self.fts(&words.join(" OR "), kinds, about, limit).await?;
        }
        Ok(found)
    }

    async fn fts(&self, expr: &str, kinds: &[String], about: &[String], limit: u32) -> anyhow::Result<Vec<Found>> {
        let rows = sqlx::query(
            "SELECT p.id AS part_id, p.item_id AS item_id FROM parts_fts f JOIN parts p ON p.id = f.rowid JOIN items i ON i.id = p.item_id \
             WHERE parts_fts MATCH ?1 AND i.kind IN (SELECT value FROM json_each(?2)) \
             AND (?4 = 0 OR (SELECT COUNT(DISTINCT l.dst_thing) FROM links l WHERE l.src_item = i.id AND l.rel = 'about' \
                  AND l.dst_thing IN (SELECT value FROM json_each(?3))) = ?4) \
             ORDER BY bm25(parts_fts) LIMIT ?5",
        )
        .bind(expr)
        .bind(serde_json::to_string(kinds)?)
        .bind(serde_json::to_string(about)?)
        .bind(about.len() as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| Found { part_id: r.get("part_id"), item_id: r.get("item_id") }).collect())
    }

    /// Every vector in `space` for items of `kinds` (about `about`):
    /// (part, item, vector). Brute force is fine at personal scale for now.
    pub async fn vectors(&self, space: &str, kinds: &[String], about: &[String]) -> anyhow::Result<Vec<(i64, String, Vec<f32>)>> {
        let rows = sqlx::query(
            "SELECT v.part_id AS part_id, p.item_id AS item_id, v.data AS data FROM vectors v JOIN parts p ON p.id = v.part_id \
             JOIN items i ON i.id = p.item_id WHERE v.space = ?1 AND i.kind IN (SELECT value FROM json_each(?2)) \
             AND (?4 = 0 OR (SELECT COUNT(DISTINCT l.dst_thing) FROM links l WHERE l.src_item = i.id AND l.rel = 'about' \
                  AND l.dst_thing IN (SELECT value FROM json_each(?3))) = ?4)",
        )
        .bind(space)
        .bind(serde_json::to_string(kinds)?)
        .bind(serde_json::to_string(about)?)
        .bind(about.len() as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| (r.get("part_id"), r.get("item_id"), from_blob(&r.get::<Vec<u8>, _>("data")))).collect())
    }

    pub async fn part_text(&self, part_id: i64) -> anyhow::Result<String> {
        Ok(sqlx::query_scalar("SELECT text FROM parts WHERE id = ?").bind(part_id).fetch_optional(&self.pool).await?.unwrap_or_default())
    }

    /// Passages with no vector in `space` yet.
    pub async fn unembedded(&self, space: &str, limit: u32) -> anyhow::Result<Vec<(i64, String)>> {
        let rows = sqlx::query(
            "SELECT p.id AS id, p.text AS text FROM parts p LEFT JOIN vectors v ON v.part_id = p.id AND v.space = ? WHERE v.part_id IS NULL ORDER BY p.id DESC LIMIT ?",
        )
        .bind(space)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| (r.get("id"), r.get("text"))).collect())
    }

    pub async fn put_vector(&self, part_id: i64, space: &str, v: &[f32]) -> anyhow::Result<()> {
        sqlx::query("INSERT OR REPLACE INTO vectors (part_id, space, data) VALUES (?, ?, ?)")
            .bind(part_id)
            .bind(space)
            .bind(to_blob(v))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Search results for items, in this order, each with a snippet.
    pub async fn hits(&self, items: &[(String, f64, Option<i64>, Vec<String>)], query: &str) -> anyhow::Result<Vec<api::MemexHit>> {
        let mut out = Vec::new();
        for (id, score, part, matched) in items {
            let Some(r) = sqlx::query("SELECT * FROM items WHERE id = ?").bind(id).fetch_optional(&self.pool).await? else { continue };
            let text = match part {
                Some(p) => self.part_text(*p).await?,
                None => r.get("body"),
            };
            out.push(api::MemexHit {
                id: id.clone(),
                kind: r.get("kind"),
                title: r.get("title"),
                snippet: snippet(&text, query),
                created_at: r.get("created_at"),
                source: r.get("source"),
                external_id: r.get("external_id"),
                url: r.get("url"),
                score: *score,
                matched: matched.clone(),
            });
        }
        Ok(out)
    }

    /// The newest items of `kinds` about a thing (or about nothing at all,
    /// with `None`: notes for every session).
    pub async fn about(&self, thing: Option<&str>, kinds: &[String], limit: u32) -> anyhow::Result<Vec<api::MemexHit>> {
        let rows = match thing {
            Some(t) => {
                sqlx::query(
                    "SELECT DISTINCT i.* FROM items i JOIN links l ON l.src_item = i.id WHERE l.dst_thing = ? \
                     AND i.kind IN (SELECT value FROM json_each(?)) ORDER BY i.created_at DESC LIMIT ?",
                )
                .bind(t)
                .bind(serde_json::to_string(kinds)?)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT i.* FROM items i WHERE i.kind IN (SELECT value FROM json_each(?)) \
                     AND NOT EXISTS (SELECT 1 FROM links l WHERE l.src_item = i.id AND l.rel = 'about') ORDER BY i.created_at DESC LIMIT ?",
                )
                .bind(serde_json::to_string(kinds)?)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows
            .iter()
            .map(|r| api::MemexHit {
                id: r.get("id"),
                kind: r.get("kind"),
                title: r.get("title"),
                snippet: clip(&r.get::<String, _>("body"), 300),
                created_at: r.get("created_at"),
                source: r.get("source"),
                external_id: r.get("external_id"),
                url: r.get("url"),
                score: 0.0,
                matched: Vec::new(),
            })
            .collect())
    }

    pub async fn item(&self, id: &str, kinds: &[String]) -> anyhow::Result<Option<api::MemexItem>> {
        let Some(r) = sqlx::query("SELECT * FROM items WHERE id = ? AND kind IN (SELECT value FROM json_each(?))")
            .bind(id)
            .bind(serde_json::to_string(kinds)?)
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let about = sqlx::query(
            "SELECT t.* FROM things t JOIN links l ON l.dst_thing = t.id WHERE l.src_item = ? AND l.rel = 'about' ORDER BY t.kind, t.name",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(Some(api::MemexItem {
            id: r.get("id"),
            kind: r.get("kind"),
            title: r.get("title"),
            text: r.get("body"),
            created_at: r.get("created_at"),
            added_at: r.get("added_at"),
            source: r.get("source"),
            external_id: r.get("external_id"),
            url: r.get("url"),
            about: about.iter().map(thing_ref).collect(),
            meta: serde_json::from_str(&r.get::<String, _>("meta")).unwrap_or_default(),
        }))
    }

    pub async fn thing(&self, key_or_id: &str) -> anyhow::Result<Option<api::MemexThingRef>> {
        let r = sqlx::query("SELECT * FROM things WHERE id = ?1 OR key = ?1 OR id = (SELECT thing_id FROM thing_keys WHERE key = ?1)")
            .bind(key_or_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(r.as_ref().map(thing_ref))
    }

    /// Things sharing items with `thing`, most shared first.
    pub async fn related(&self, thing: &str, kinds: &[String], limit: u32) -> anyhow::Result<Vec<(api::MemexThingRef, u32)>> {
        let rows = sqlx::query(
            "SELECT t.*, COUNT(DISTINCT a.src_item) AS shared FROM links a JOIN links b ON b.src_item = a.src_item \
             JOIN things t ON t.id = b.dst_thing JOIN items i ON i.id = a.src_item \
             WHERE a.dst_thing = ? AND b.dst_thing != ? AND b.rel != 'cites' AND i.kind IN (SELECT value FROM json_each(?)) \
             GROUP BY t.id ORDER BY shared DESC LIMIT ?",
        )
        .bind(thing)
        .bind(thing)
        .bind(serde_json::to_string(kinds)?)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| (thing_ref(r), r.get::<i64, _>("shared") as u32)).collect())
    }

    /// Things whose name or label starts with (or contains) `q`.
    pub async fn find_things(&self, q: &str, limit: u32) -> anyhow::Result<Vec<api::MemexThingRef>> {
        let like = format!("%{}%", q.replace(['%', '_'], ""));
        let rows = sqlx::query("SELECT * FROM things WHERE name LIKE ? OR key LIKE ? ORDER BY (name LIKE ?) DESC, length(name) LIMIT ?")
            .bind(&like)
            .bind(&like)
            .bind(format!("{}%", q.replace(['%', '_'], "")))
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(thing_ref).collect())
    }

    /// The thing a key leads to: its own key, or one of its other keys.
    pub async fn thing_by_key(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT id FROM things WHERE key = ?1 UNION SELECT thing_id FROM thing_keys WHERE key = ?1 LIMIT 1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Another key for a thing (a person's other address).
    pub async fn add_key(&self, key: &str, thing: &str) -> anyhow::Result<()> {
        if self.thing_by_key(key).await?.as_deref() == Some(thing) {
            return Ok(());
        }
        sqlx::query("INSERT OR REPLACE INTO thing_keys (key, thing_id) VALUES (?, ?)").bind(key).bind(thing).execute(&self.pool).await?;
        Ok(())
    }

    /// Names a thing still called by its address.
    pub async fn rename_if_unnamed(&self, id: &str, name: &str, placeholder: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE things SET name = ? WHERE id = ? AND (name = ? OR name = '' OR name LIKE '%@%')")
            .bind(name)
            .bind(id)
            .bind(placeholder)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A link between two things (once).
    pub async fn link_things(&self, src: &str, rel: &str, dst: &str, basis: &str, made_by: &str) -> anyhow::Result<()> {
        let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM links WHERE src_thing = ? AND rel = ? AND dst_thing = ?")
            .bind(src)
            .bind(rel)
            .bind(dst)
            .fetch_optional(&self.pool)
            .await?;
        if exists.is_none() {
            sqlx::query("INSERT INTO links (src_thing, rel, dst_thing, basis, made_by, created_at) VALUES (?, ?, ?, ?, ?, ?)")
                .bind(src)
                .bind(rel)
                .bind(dst)
                .bind(basis)
                .bind(made_by)
                .bind(c6_core::now_ms())
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    /// The thing with this key, made (or named) if need be.
    pub async fn ensure_thing(&self, key: &str, kind: &str, name: &str) -> anyhow::Result<String> {
        if let Some(id) =
            sqlx::query_scalar::<_, String>("SELECT thing_id FROM thing_keys WHERE key = ?").bind(key).fetch_optional(&self.pool).await?
        {
            return Ok(id);
        }
        if let Some((id, old)) =
            sqlx::query_as::<_, (String, String)>("SELECT id, name FROM things WHERE key = ?").bind(key).fetch_optional(&self.pool).await?
        {
            // A better name than a key-derived one.
            // (or a citation's whole text, once its title is known).
            let better = old.is_empty() || key.ends_with(&old) || (old.len() > name.len() && old.contains(name));
            if !name.is_empty() && better && old != name {
                sqlx::query("UPDATE things SET name = ? WHERE id = ?").bind(name).bind(&id).execute(&self.pool).await?;
            }
            return Ok(id);
        }
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO things (id, kind, name, key, created_at) VALUES (?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(kind)
            .bind(if name.is_empty() { key } else { name })
            .bind(key)
            .bind(c6_core::now_ms())
            .execute(&self.pool)
            .await?;
        Ok(id)
    }

    /// Links an item to a thing, with what supports it (a quote).
    pub async fn link(&self, item: &str, rel: &str, thing: &str, basis: &str, made_by: &str, quote: Option<&str>) -> anyhow::Result<()> {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO links (src_item, rel, dst_thing, basis, made_by, created_at) VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(item)
        .bind(rel)
        .bind(thing)
        .bind(basis)
        .bind(made_by)
        .bind(c6_core::now_ms())
        .fetch_one(&self.pool)
        .await?;
        sqlx::query("INSERT INTO evidence (link_id, item_id, quote) VALUES (?, ?, ?)")
            .bind(id)
            .bind(item)
            .bind(quote)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// A link a model inferred, with how sure it is and what supports it.
    pub async fn link_inferred(
        &self,
        item: &str,
        rel: &str,
        thing: &str,
        confidence: f64,
        made_by: &str,
        quote: Option<&str>,
    ) -> anyhow::Result<()> {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO links (src_item, rel, dst_thing, basis, confidence, made_by, created_at) VALUES (?, ?, ?, 'inferred', ?, ?, ?) RETURNING id",
        )
        .bind(item)
        .bind(rel)
        .bind(thing)
        .bind(confidence)
        .bind(made_by)
        .bind(c6_core::now_ms())
        .fetch_one(&self.pool)
        .await?;
        sqlx::query("INSERT INTO evidence (link_id, item_id, quote) VALUES (?, ?, ?)")
            .bind(id)
            .bind(item)
            .bind(quote)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Removes the links an item's analysis made (before making them again).
    pub async fn clear_links(&self, item: &str, made_by: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM evidence WHERE link_id IN (SELECT id FROM links WHERE src_item = ? AND made_by = ?)")
            .bind(item)
            .bind(made_by)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM links WHERE src_item = ? AND made_by = ?").bind(item).bind(made_by).execute(&self.pool).await?;
        Ok(())
    }

    /// How a thing is linked from items: (relation, count).
    pub async fn rels_to(&self, thing: &str) -> anyhow::Result<Vec<(String, i64)>> {
        Ok(sqlx::query_as("SELECT rel, COUNT(*) FROM links WHERE dst_thing = ? GROUP BY rel ORDER BY 2 DESC")
            .bind(thing)
            .fetch_all(&self.pool)
            .await?)
    }

    // --- Watched files ---

    pub async fn file_state(&self, path: &str) -> anyhow::Result<Option<(i64, i64)>> {
        Ok(sqlx::query_as("SELECT mtime, size FROM files WHERE path = ?").bind(path).fetch_optional(&self.pool).await?)
    }

    pub async fn set_file(&self, path: &str, mtime: i64, size: i64, item: Option<&str>, error: Option<&str>) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO files (path, mtime, size, item_id, error, seen_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(path) DO UPDATE SET \
             mtime = excluded.mtime, size = excluded.size, item_id = excluded.item_id, error = excluded.error, seen_at = excluded.seen_at",
        )
        .bind(path)
        .bind(mtime)
        .bind(size)
        .bind(item)
        .bind(error)
        .bind(c6_core::now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Files under `root` not seen since `before` (gone from disk): their
    /// items.
    pub async fn files_gone(&self, root: &str, before: i64) -> anyhow::Result<Vec<(String, Option<String>)>> {
        Ok(sqlx::query_as("SELECT path, item_id FROM files WHERE path LIKE ? AND seen_at < ?")
            .bind(format!("{}%", root.trim_end_matches('/')))
            .bind(before)
            .fetch_all(&self.pool)
            .await?)
    }

    pub async fn forget_file(&self, path: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM files WHERE path = ?").bind(path).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn touch_file(&self, path: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE files SET seen_at = ? WHERE path = ?").bind(c6_core::now_ms()).bind(path).execute(&self.pool).await?;
        Ok(())
    }

    // --- Trails ---

    pub async fn trails(&self) -> anyhow::Result<Vec<api::MemexTrail>> {
        let rows = sqlx::query(
            "SELECT t.*, (SELECT COUNT(*) FROM trail_steps s WHERE s.trail_id = t.id) AS steps FROM trails t ORDER BY t.updated_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| api::MemexTrail {
                id: r.get("id"),
                name: r.get("name"),
                description: r.get("description"),
                made_by: r.get("made_by"),
                updated_at: r.get("updated_at"),
                steps: Vec::new(),
                step_count: r.get::<i64, _>("steps") as u32,
            })
            .collect())
    }

    /// A trail by id or name, with its steps.
    pub async fn trail(&self, id_or_name: &str) -> anyhow::Result<Option<api::MemexTrail>> {
        let Some(r) = sqlx::query("SELECT * FROM trails WHERE id = ? OR name = ?")
            .bind(id_or_name)
            .bind(id_or_name)
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let id: String = r.get("id");
        let steps = sqlx::query("SELECT * FROM trail_steps WHERE trail_id = ? ORDER BY ordinal").bind(&id).fetch_all(&self.pool).await?;
        Ok(Some(api::MemexTrail {
            id: id.clone(),
            name: r.get("name"),
            description: r.get("description"),
            made_by: r.get("made_by"),
            updated_at: r.get("updated_at"),
            step_count: steps.len() as u32,
            steps: steps
                .iter()
                .map(|s| api::MemexTrailStep {
                    id: s.get("id"),
                    target: s.get("target"),
                    comment: s.get("comment"),
                    made_by: s.get("made_by"),
                    created_at: s.get("created_at"),
                    title: String::new(),
                    kind: String::new(),
                })
                .collect(),
        }))
    }

    /// Adds a step to a trail, making the trail if need be. The trail's id.
    pub async fn trail_append(&self, trail: &str, target: &str, comment: &str, made_by: &str) -> anyhow::Result<String> {
        let now = c6_core::now_ms();
        let id = match sqlx::query_scalar::<_, String>("SELECT id FROM trails WHERE id = ? OR name = ?")
            .bind(trail)
            .bind(trail)
            .fetch_optional(&self.pool)
            .await?
        {
            Some(id) => id,
            None => {
                let id = uuid::Uuid::now_v7().to_string();
                sqlx::query("INSERT INTO trails (id, name, made_by, created_at, updated_at) VALUES (?, ?, ?, ?, ?)")
                    .bind(&id)
                    .bind(trail.trim())
                    .bind(made_by)
                    .bind(now)
                    .bind(now)
                    .execute(&self.pool)
                    .await?;
                id
            }
        };
        let next: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(ordinal), 0) + 1 FROM trail_steps WHERE trail_id = ?")
            .bind(&id)
            .fetch_one(&self.pool)
            .await?;
        sqlx::query("INSERT INTO trail_steps (trail_id, ordinal, target, comment, made_by, created_at) VALUES (?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(next)
            .bind(target)
            .bind(comment)
            .bind(made_by)
            .bind(now)
            .execute(&self.pool)
            .await?;
        sqlx::query("UPDATE trails SET updated_at = ? WHERE id = ?").bind(now).bind(&id).execute(&self.pool).await?;
        Ok(id)
    }

    pub async fn trail_update(&self, id: &str, name: Option<&str>, description: Option<&str>) -> anyhow::Result<()> {
        if let Some(n) = name {
            sqlx::query("UPDATE trails SET name = ? WHERE id = ?").bind(n).bind(id).execute(&self.pool).await?;
        }
        if let Some(d) = description {
            sqlx::query("UPDATE trails SET description = ? WHERE id = ?").bind(d).bind(id).execute(&self.pool).await?;
        }
        Ok(())
    }

    pub async fn trail_remove_step(&self, step: i64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM trail_steps WHERE id = ?").bind(step).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn trail_delete(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM trail_steps WHERE trail_id = ?").bind(id).execute(&self.pool).await?;
        sqlx::query("DELETE FROM trails WHERE id = ?").bind(id).execute(&self.pool).await?;
        Ok(())
    }

    /// (kind, title) of a trail step's target: an item, or a thing.
    pub async fn describe(&self, target: &str) -> anyhow::Result<(String, String)> {
        if let Some(r) = sqlx::query_as::<_, (String, String)>("SELECT kind, title FROM items WHERE id = ?")
            .bind(target)
            .fetch_optional(&self.pool)
            .await?
        {
            return Ok(r);
        }
        Ok(self.thing(target).await?.map(|t| (t.kind, t.name)).unwrap_or_else(|| ("missing".into(), target.into())))
    }

    pub async fn status(&self, space: &str) -> anyhow::Result<api::MemexStatus> {
        let kinds = sqlx::query("SELECT kind, COUNT(*) AS n FROM items GROUP BY kind").fetch_all(&self.pool).await?;
        let count = |sql: &'static str| async move { sqlx::query_scalar::<_, i64>(sql).fetch_one(&self.pool).await };
        let embedded: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vectors WHERE space = ?").bind(space).fetch_one(&self.pool).await?;
        Ok(api::MemexStatus {
            items: kinds.iter().map(|r| (r.get::<String, _>("kind"), r.get::<i64, _>("n") as u64)).collect(),
            things: count("SELECT COUNT(*) FROM things").await? as u64,
            passages: count("SELECT COUNT(*) FROM parts").await? as u64,
            embedded: embedded as u64,
            embedding_model: space.to_owned(),
            ..Default::default()
        })
    }
}

/// The thing a label stands for, made on first sight. `None` for labels
/// that name a run.
async fn thing_for_label(tx: &mut sqlx::SqliteConnection, label: &str, now: i64) -> anyhow::Result<Option<String>> {
    let label = label.trim();
    if label.is_empty() {
        return Ok(None);
    }
    let (kind, name) = match label.split_once(':') {
        Some((k, _)) if NOT_THINGS.contains(&k) => return Ok(None),
        Some((k, v)) if !v.is_empty() => (k.to_owned(), v.to_owned()),
        _ => ("topic".to_owned(), label.to_owned()),
    };
    if let Some(id) = sqlx::query_scalar::<_, String>("SELECT id FROM things WHERE key = ?").bind(label).fetch_optional(&mut *tx).await? {
        return Ok(Some(id));
    }
    let id = uuid::Uuid::now_v7().to_string();
    sqlx::query("INSERT INTO things (id, kind, name, key, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(kind)
        .bind(name)
        .bind(label)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    Ok(Some(id))
}

fn thing_ref(r: &sqlx::sqlite::SqliteRow) -> api::MemexThingRef {
    api::MemexThingRef {
        id: r.get("id"),
        kind: r.get("kind"),
        name: r.get("name"),
        label: r.get::<Option<String>, _>("key").unwrap_or_default(),
    }
}

/// Splits text into passages of about `PART_CHARS`, at paragraph (then
/// line, then word) boundaries.
pub fn chunk(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for para in text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        if !cur.is_empty() && cur.len() + para.len() + 2 > PART_CHARS {
            out.push(std::mem::take(&mut cur));
        }
        if para.len() > PART_CHARS * 3 / 2 {
            // A long paragraph: by words.
            for w in para.split_whitespace() {
                if cur.len() + w.len() + 1 > PART_CHARS && !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                if !cur.is_empty() {
                    cur.push(' ');
                }
                cur.push_str(w);
            }
            continue;
        }
        if !cur.is_empty() {
            cur.push_str("\n\n");
        }
        cur.push_str(para);
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

pub fn clip(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n { s.to_owned() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// About 300 characters of `text` around the first query word it has.
fn snippet(text: &str, query: &str) -> String {
    let lower = text.to_lowercase();
    let at = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .filter_map(|w| lower.find(&w.to_lowercase()))
        .min()
        .unwrap_or(0);
    let start = text.char_indices().map(|(i, _)| i).filter(|i| *i <= at).rev().nth(120).unwrap_or(0);
    let body: String = text[start..].chars().take(300).collect();
    let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{}{body}{}", if start > 0 { "…" } else { "" }, if text[start..].chars().count() > 300 { "…" } else { "" })
}

fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

fn from_blob(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_at_paragraphs() {
        let p = "word ".repeat(100); // 500 chars
        let text = format!("{p}\n\n{p}\n\n{p}");
        let parts = chunk(&text);
        assert_eq!(parts.len(), 2, "two paragraphs fit in one passage, the third does not");
        assert!(chunk(&"x ".repeat(2000)).iter().all(|c| c.len() <= PART_CHARS));
        assert!(chunk("  \n\n ").is_empty());
    }

    #[test]
    fn snippets_center_on_the_query() {
        let text = format!("{} the payments API is timing out {}", "a ".repeat(400), "b ".repeat(400));
        let s = snippet(&text, "payments timeout");
        assert!(s.contains("payments API") && s.starts_with('…'), "{s}");
    }
}
