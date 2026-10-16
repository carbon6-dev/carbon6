//! One way in (docs/memex.md §4.5): `memex.ingest` takes items typed by
//! their MIME type, and each type's handler keeps its own identity and
//! storage. What needs reading (mail, PDF, Word, HTML) is kept as a blob
//! and read by the c6-extract kit (extract.rs): its result says `queued`. Everything that comes in this way is imported:
//! only connectors (Gmail sync) create mirrored items, whose changes are
//! written back, and ingested sources can't reach theirs.

use base64::Engine;
use c6_api as api;
use serde_json::{Value, json};

use crate::Memex;

/// Items per call.
const MAX_ITEMS: usize = 500;
/// How long a call waits for what the kit reads.
const WAIT: std::time::Duration = std::time::Duration::from_secs(60);
/// The largest message.
const MAIL_LIMIT: u64 = 25 * 1024 * 1024;

/// What only an app's mail, calendar and contacts sync writes: a mail
/// account's label names, a calendar, an event and a contact, in the
/// shape of Google's APIs (another provider's app converts to it).
pub const LABELS: &str = "application/vnd.c6.mail-labels+json";
pub const CALENDAR: &str = "application/vnd.c6.calendar+json";
pub const EVENT: &str = "application/vnd.c6.event+json";
pub const CONTACT: &str = "application/vnd.c6.contact+json";

/// The internal generic item (the head's session digests and memory):
/// only the head sends it, never a token.
pub const ITEM_TYPE: &str = "application/vnd.c6.item+json";

const DOC_TYPES: [(&str, &str); 6] = [
    ("application/pdf", "pdf"),
    ("application/vnd.openxmlformats-officedocument.wordprocessingml.document", "docx"),
    ("text/markdown", "md"),
    ("text/plain", "txt"),
    ("text/html", "html"),
    ("text/x-rst", "rst"),
];

/// The handlers anyone may use.
pub fn types() -> Vec<api::IngestType> {
    vec![
        api::IngestType {
            name: "mail".into(),
            types: vec!["message/rfc822".into()],
            attributes: mail_schema(),
            identity: "Message-ID, across all mail (synced or imported); without one, a hash of From, Date, Subject and the body".into(),
            limit: MAIL_LIMIT,
            produces: vec!["email".into(), "spam".into()],
        },
        api::IngestType {
            name: "mail-labels".into(),
            types: vec![LABELS.into()],
            attributes: json!({"type": "object"}),
            identity: "the account: its labels' names (apps only)".into(),
            limit: 1024 * 1024,
            produces: vec![],
        },
        api::IngestType {
            name: "calendar".into(),
            types: vec![CALENDAR.into()],
            attributes: json!({"type": "object"}),
            identity: "the account and the calendar's id (apps only)".into(),
            limit: 1024 * 1024,
            produces: vec![],
        },
        api::IngestType {
            name: "event".into(),
            types: vec![EVENT.into()],
            attributes: json!({"type": "object"}),
            identity: "the account, calendar and event id (apps only)".into(),
            limit: 4 * 1024 * 1024,
            produces: vec!["event".into()],
        },
        api::IngestType {
            name: "contact".into(),
            types: vec![CONTACT.into()],
            attributes: json!({"type": "object"}),
            identity: "the account and the contact's resource name (apps only)".into(),
            limit: 1024 * 1024,
            produces: vec![],
        },
        api::IngestType {
            name: "document".into(),
            types: DOC_TYPES.iter().map(|(t, _)| t.to_string()).collect(),
            attributes: doc_schema(),
            identity: "the content's hash, across all documents".into(),
            limit: crate::MAX_HELD,
            produces: vec!["document".into(), "paper".into()],
        },
    ]
}

fn mail_schema() -> Value {
    json!({"type": "object", "additionalProperties": false, "properties": {
        "folder": {"type": "string", "maxLength": 300, "description": "The folder it was in; it becomes a label"},
        "labels": {"type": "array", "items": {"type": "string", "maxLength": 300}, "description": "More labels"},
        "read": {"type": "boolean", "description": "Default true"},
        "starred": {"type": "boolean"},
        "received": {"type": "integer", "description": "When it arrived (ms since the epoch), for a message without a Date"},
        "account": {"type": "string", "description": "A mail app's account (apps only): the message is mirrored, its labels the mailbox's"},
        "message": {"type": "string", "description": "The mailbox's id for it (apps only)"},
        "thread": {"type": "string", "description": "The mailbox's thread (apps only)"},
        "tell": {"type": "boolean", "description": "Say it arrived (apps only)"}
    }})
}

fn doc_schema() -> Value {
    json!({"type": "object", "additionalProperties": false, "properties": {
        "name": {"type": "string", "maxLength": 300, "description": "Its file name (the title when the document has none)"},
        "created": {"type": "integer", "description": "When it was written (ms since the epoch)"}
    }})
}

/// Checks attributes against a handler's schema: only its keys, of their
/// types (the subset of JSON Schema the handlers use).
pub fn check(schema: &Value, v: &Value) -> Result<(), String> {
    fn ty(schema: &Value, v: &Value, at: &str) -> Result<(), String> {
        let ok = match schema["type"].as_str().unwrap_or("") {
            "string" => {
                let s = v.as_str().ok_or_else(|| format!("{at} is a string"))?;
                if let Some(max) = schema["maxLength"].as_u64()
                    && s.chars().count() as u64 > max
                {
                    return Err(format!("{at} is at most {max} characters"));
                }
                true
            }
            "boolean" => v.is_boolean(),
            "integer" => v.is_i64() || v.is_u64(),
            "array" => {
                let a = v.as_array().ok_or_else(|| format!("{at} is a list"))?;
                for (i, x) in a.iter().enumerate() {
                    ty(&schema["items"], x, &format!("{at}[{i}]"))?;
                }
                true
            }
            "object" => {
                let o = v.as_object().ok_or_else(|| format!("{at} is an object"))?;
                let props = schema["properties"].as_object().cloned().unwrap_or_default();
                for (k, x) in o {
                    let Some(p) = props.get(k) else {
                        let known: Vec<&str> = props.keys().map(String::as_str).collect();
                        return Err(format!("unknown attribute {k:?} (known: {})", known.join(", ")));
                    };
                    ty(p, x, k)?;
                }
                for r in schema["required"].as_array().into_iter().flatten().filter_map(|r| r.as_str()) {
                    if !o.contains_key(r) {
                        return Err(format!("{r} is required"));
                    }
                }
                true
            }
            _ => true,
        };
        if ok {
            Ok(())
        } else {
            Err(format!("{at} is a{} {}", if schema["type"] == "integer" { "n" } else { "" }, schema["type"].as_str().unwrap_or("")))
        }
    }
    let empty = json!({});
    ty(schema, if v.is_null() { &empty } else { v }, "attributes")
}

/// `memex.ingest`: a batch of typed items, or (from the head) one generic
/// item in the older form.
pub async fn ingest(m: &Memex, params: Value) -> anyhow::Result<Value> {
    if params.get("items").is_none() {
        let item: api::MemexItemIn = serde_json::from_value(params)?;
        let id = m.store.put(&item, &item.source).await?;
        return Ok(json!({"id": id}));
    }
    // The head writes its apps' kit output as the app (`as`); nobody else can.
    let app = params["as"].as_str().filter(|a| a.starts_with("app:")).map(str::to_owned);
    let wait = params["wait"].as_bool() != Some(false);
    let batch: api::MemexIngest = serde_json::from_value(params)?;
    anyhow::ensure!(batch.items.len() <= MAX_ITEMS, "at most {MAX_ITEMS} items a call");
    let mut out = Vec::with_capacity(batch.items.len());
    for item in &batch.items {
        out.push(match one(m, item, app.as_deref()).await {
            Ok(r) => r,
            Err(e) => (api::IngestResult { status: "rejected".into(), error: Some(format!("{e:#}")), ..Default::default() }, None),
        });
    }
    // What the kit reads: waited for a while, else still `queued`.
    let deadline = tokio::time::Instant::now() + if wait { WAIT } else { std::time::Duration::ZERO };
    for (r, key) in out.iter_mut() {
        let Some(key) = key else { continue };
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match crate::extract::outcome(m, key, left).await? {
            Some(Ok(k)) => {
                r.kind = sqlx::query_scalar("SELECT kind FROM items WHERE id = ?").bind(&k.id).fetch_optional(&m.store.pool).await?;
                // A document from this source and id before is updated.
                let status = if r.id.is_some() && k.status == "created" { "updated" } else { k.status };
                (r.status, r.id) = (status.into(), Some(k.id));
            }
            Some(Err(e)) => (r.status, r.error) = ("rejected".into(), Some(e)),
            None => {}
        }
    }
    Ok(json!({"results": out.into_iter().map(|(r, _)| r).collect::<Vec<_>>()}))
}

/// An ingested item's source: always under `import:`, so it can never name
/// a connector's source (`gmail:…`) or the head's.
fn source_of(s: &api::IngestSource) -> anyhow::Result<String> {
    let kind = s.kind.trim();
    anyhow::ensure!(
        !kind.is_empty() && kind.len() <= 40 && kind.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'),
        "source.kind is a short lowercase name, e.g. mbox"
    );
    let id = s.id.trim();
    anyhow::ensure!(id.len() <= 120 && !id.chars().any(char::is_control), "source.id is at most 120 characters");
    Ok(if id.is_empty() { format!("import:{kind}") } else { format!("import:{kind}:{id}") })
}

/// An item in, and the extraction job it waits for (if any).
async fn one(m: &Memex, item: &api::IngestItem, app: Option<&str>) -> anyhow::Result<(api::IngestResult, Option<String>)> {
    let mime = item.mime.split(';').next().unwrap_or("").trim().to_lowercase();
    let handler = types().into_iter().find(|h| h.types.contains(&mime));
    let Some(handler) = handler else {
        let known: Vec<String> = types().into_iter().flat_map(|h| h.types).collect();
        anyhow::bail!("no handler for {:?} (memex.types lists them: {})", item.mime, known.join(", "));
    };
    let source = match app {
        Some(a) => a.to_owned(),
        None => source_of(&item.source)?,
    };
    let external_id = item.external_id.trim();
    anyhow::ensure!(!external_id.is_empty() && external_id.len() <= 1000, "external_id is required (the source's own id for it)");
    check(&handler.attributes, &item.attributes).map_err(|e| anyhow::anyhow!(e))?;
    if handler.name != "mail" && handler.name != "document" && app.is_none() {
        anyhow::bail!("{} is written by apps (their kits), not imported", item.mime);
    }
    let data = match (&item.data, &item.text) {
        (Some(d), _) => {
            anyhow::ensure!((d.len() as u64) / 4 * 3 <= handler.limit + 3, "larger than {} MB", handler.limit >> 20);
            base64::engine::general_purpose::STANDARD.decode(d.trim()).map_err(|e| anyhow::anyhow!("data is not base64: {e}"))?
        }
        (None, Some(t)) => t.as_bytes().to_vec(),
        // A mirrored message's new labels: the same item without its bytes.
        (None, None) if handler.name == "mail" && app.is_some() => {
            return Ok((relabel(m, &source, external_id, &item.attributes).await?, None));
        }
        (None, None) => anyhow::bail!("data (base64) or text is required"),
    };
    anyhow::ensure!(data.len() as u64 <= handler.limit, "larger than {} MB", handler.limit >> 20);
    let (r, key) = match handler.name.as_str() {
        "mail" => (mail(m, &source, external_id, &data, &item.attributes, app.is_some()).await?, format!("mail:{source}:{external_id}")),
        "mail-labels" | "calendar" | "event" | "contact" => {
            let v: Value = serde_json::from_slice(&data).map_err(|e| anyhow::anyhow!("not JSON: {e}"))?;
            (kept(m, &handler.name, &source, &v).await?, String::new())
        }
        _ => {
            let ext = DOC_TYPES.iter().find(|(t, _)| *t == mime).map(|(_, e)| *e).unwrap_or("txt");
            let name = item.attributes["name"].as_str().unwrap_or(external_id);
            let r = document(m, &source, external_id, name, ext, &data, &item.labels, item.attributes["created"].as_i64()).await?;
            (r, format!("doc:{source}:{external_id}"))
        }
    };
    let waits = r.status == "queued";
    Ok((r, waits.then_some(key)))
}

/// A mirrored message's labels changed (its app said so, without the
/// message again).
async fn relabel(m: &Memex, source: &str, external_id: &str, attrs: &Value) -> anyhow::Result<api::IngestResult> {
    let id = existing(m, source, external_id).await?.ok_or_else(|| anyhow::anyhow!("no message {external_id} to change"))?;
    let labels: Vec<String> = serde_json::from_value(attrs["labels"].clone()).map_err(|_| anyhow::anyhow!("labels is a list"))?;
    crate::mail::set_labels(m, &id, &labels).await?;
    Ok(api::IngestResult { status: "updated".into(), id: Some(id), kind: Some("email".into()), error: None })
}

/// What an app's sync keeps in the mail, calendar and contacts stores.
async fn kept(m: &Memex, handler: &str, source: &str, v: &Value) -> anyhow::Result<api::IngestResult> {
    let account = v["account"].as_str().filter(|a| !a.is_empty()).ok_or_else(|| anyhow::anyhow!("account is required"))?;
    match handler {
        "mail-labels" => {
            for l in v["labels"].as_array().into_iter().flatten() {
                sqlx::query("INSERT OR REPLACE INTO label_names (account, id, name, type) VALUES (?, ?, ?, ?)")
                    .bind(account)
                    .bind(l["id"].as_str().unwrap_or_default())
                    .bind(l["name"].as_str().unwrap_or_default())
                    .bind(l["type"].as_str().unwrap_or("user"))
                    .execute(&m.store.pool)
                    .await?;
            }
        }
        "calendar" => crate::calendar::store_calendar(m, account, &v["calendar"]).await?,
        "event" => {
            let cal = v["calendar"].as_str().unwrap_or("primary");
            crate::calendar::store_event(m, source, account, v["email"].as_str().unwrap_or_default(), cal, &v["event"]).await?
        }
        _ => crate::contacts::store(m, account, &v["person"]).await?,
    }
    Ok(api::IngestResult { status: "created".into(), id: None, kind: None, error: None })
}

/// An existing item from this source with this id.
async fn existing(m: &Memex, source: &str, external_id: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT id FROM items WHERE source = ? AND external_id = ?")
        .bind(source)
        .bind(external_id)
        .fetch_optional(&m.store.pool)
        .await?)
}

fn duplicate(id: String, kind: Option<String>) -> api::IngestResult {
    api::IngestResult { status: "duplicate".into(), id: Some(id), kind, error: None }
}

/// Text the store keeps as it is (no reading needed).
pub const PLAIN: [&str; 5] = ["md", "markdown", "txt", "text", "rst"];

/// A document: its text (and, for a paper, its identity and citations),
/// recognized by the content's hash. Plain text is kept now; the rest is
/// read by the kit (status `queued`).
#[allow(clippy::too_many_arguments)]
pub async fn document(
    m: &Memex,
    source: &str,
    external_id: &str,
    name: &str,
    ext: &str,
    data: &[u8],
    labels: &[String],
    created: Option<i64>,
) -> anyhow::Result<api::IngestResult> {
    let hash = c6_core::sha256_hex(data);
    let before = existing(m, source, external_id).await?;
    let same: Option<(String, String)> =
        sqlx::query_as("SELECT id, kind FROM items WHERE json_extract(meta, '$.blob') = ? AND kind IN ('document', 'paper') LIMIT 1")
            .bind(&hash)
            .fetch_optional(&m.store.pool)
            .await?;
    if let Some((id, kind)) = same {
        return Ok(duplicate(id, Some(kind)));
    }
    // The extension tells the reader what it is.
    let file = if name.to_lowercase().ends_with(&format!(".{ext}")) { name.to_owned() } else { format!("{name}.{ext}") };
    m.put_blob(data).await?;
    let input = format!("head:{hash}");
    let params = json!({"source": source, "external_id": external_id, "labels": labels, "created": created, "size": data.len()});
    let key = format!("doc:{source}:{external_id}");
    if PLAIN.contains(&ext) {
        let files = Default::default();
        let job = crate::extract::Done { key: &key, input: &input, name: &file, params: &params, files: &files };
        let id = m.document_from(&job, &json!({"text": String::from_utf8_lossy(data)})).await?;
        let kind: String = sqlx::query_scalar("SELECT kind FROM items WHERE id = ?").bind(&id).fetch_one(&m.store.pool).await?;
        return Ok(api::IngestResult {
            status: if before.is_some() { "updated" } else { "created" }.into(),
            id: Some(id),
            kind: Some(kind),
            error: None,
        });
    }
    crate::extract::queue(m, &key, "document", &input, &file, params).await?;
    Ok(api::IngestResult { status: "queued".into(), id: before, kind: None, error: None })
}

/// A message: kept as a blob, read by the kit, then stored with the mail
/// (mail_from).
async fn mail(m: &Memex, source: &str, external_id: &str, raw: &[u8], attrs: &Value, app: bool) -> anyhow::Result<api::IngestResult> {
    if let Some(id) = existing(m, source, external_id).await? {
        if app && attrs["labels"].is_array() {
            crate::mail::set_labels(m, &id, &serde_json::from_value::<Vec<String>>(attrs["labels"].clone())?).await?;
        }
        return Ok(duplicate(id, None));
    }
    let blob = m.put_blob(raw).await?;
    // An app's account: mirrored (its labels are the mailbox's); else imported.
    let kind = if app && attrs["account"].is_string() { "mirror" } else { "import" };
    let params = json!({"kind": kind, "source": source, "external_id": external_id, "attributes": attrs});
    crate::extract::queue(m, &format!("mail:{source}:{external_id}"), "mail", &format!("head:{blob}"), "", params).await?;
    Ok(api::IngestResult { status: "queued".into(), id: None, kind: None, error: None })
}

/// A message the kit read: stored as its job says (imported, synced from
/// Gmail, or kept before and read again).
pub async fn mail_from(m: &Memex, job: &crate::extract::Done<'_>, v: &Value) -> anyhow::Result<crate::extract::Kept> {
    use crate::extract::Kept;
    let parsed = crate::mail::Parsed::of(v, job.files);
    match job.params["kind"].as_str().unwrap_or_default() {
        "mirror" => return crate::mail::store_message(m, job, &parsed).await.map(Kept::from),
        "reparse" => return crate::mail::reparsed(m, job, &parsed).await.map(Kept::from),
        _ => {}
    }
    let p = job.params;
    let (source, external_id, attrs) =
        (p["source"].as_str().unwrap_or_default(), p["external_id"].as_str().unwrap_or_default(), &p["attributes"]);
    if let Some(id) = existing(m, source, external_id).await? {
        return Ok(Kept { id, status: "duplicate" });
    }
    let rfc_id = match v["message_id"].as_str() {
        Some(id) if !id.trim().is_empty() => id.trim().to_owned(),
        _ => {
            // As it always was: From, Date, Subject and the text part.
            let key = format!(
                "{:?}|{:?}|{}|{}",
                v["from_raw"].as_str(),
                v["date"].as_i64(),
                v["subject"].as_str().unwrap_or(""),
                v["plain"].as_str().unwrap_or("")
            );
            format!("{}@c6.import", &c6_core::sha256_hex(key.as_bytes())[..32])
        }
    };
    let same: Option<String> =
        sqlx::query_scalar("SELECT item_id FROM mail WHERE rfc_id = ? LIMIT 1").bind(&rfc_id).fetch_optional(&m.store.pool).await?;
    if let Some(id) = same {
        return Ok(Kept { id, status: "duplicate" });
    }
    // The thread's root: the first reference, else what it answers, else
    // itself; the same in any order the messages arrive.
    let first = |k: &str| v[k].as_array().and_then(|l| l.first()).and_then(|r| r.as_str()).map(str::to_owned);
    let root = first("references").or_else(|| first("in_reply_to")).unwrap_or_else(|| rfc_id.clone());
    let thread_id = format!("i{}", &c6_core::sha256_hex(root.trim().as_bytes())[..16]);
    let date = v["date"].as_i64().map(|d| d * 1000).or_else(|| attrs["received"].as_i64()).unwrap_or_else(c6_core::now_ms);

    let mut labels: Vec<String> =
        attrs["folder"].as_str().filter(|f| !f.trim().is_empty()).map(|f| f.trim().to_owned()).into_iter().collect();
    labels.extend(attrs["labels"].as_array().into_iter().flatten().filter_map(|l| l.as_str()).map(|l| l.trim().to_owned()));
    labels.retain(|l| !l.is_empty());
    labels.dedup();
    let user_labels = labels.clone();
    let lower: Vec<String> = labels.iter().map(|l| l.to_lowercase()).collect();
    if lower.iter().any(|l| l.split(['/', '.']).any(|p| p == "spam" || p == "junk")) {
        labels.push("SPAM".into());
    }
    if attrs["read"].as_bool() == Some(false) {
        labels.push("UNREAD".into());
    }
    if attrs["starred"].as_bool() == Some(true) {
        labels.push("STARRED".into());
    }
    let me: Vec<String> = m.accounts().iter().map(|a| a.email.to_lowercase()).collect();
    if me.contains(&parsed.from.0) {
        labels.push("SENT".into());
    }
    for l in &user_labels {
        sqlx::query("INSERT OR IGNORE INTO label_names (account, id, name, type) VALUES (?, ?, ?, 'user')")
            .bind(source)
            .bind(l)
            .bind(l)
            .execute(&m.store.pool)
            .await?;
    }
    let at = crate::mail::Stored {
        account: source,
        external_id,
        thread_id: &thread_id,
        source,
        rfc_id: &rfc_id,
        origin: "imported",
        owner: source,
        message: "",
    };
    let blob = job.input.strip_prefix("head:").unwrap_or_default();
    crate::mail::store(m, &parsed, blob, &at, &labels, date).await.map(Kept::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributes_are_strict() {
        let s = mail_schema();
        assert!(check(&s, &Value::Null).is_ok());
        assert!(check(&s, &json!({"folder": "Work/2009", "read": false, "labels": ["a"], "received": 1})).is_ok());
        assert_eq!(
            check(&s, &json!({"fodler": "x"})).unwrap_err(),
            "unknown attribute \"fodler\" (known: account, folder, labels, message, read, received, starred, tell, thread)"
        );
        assert_eq!(check(&s, &json!({"read": "yes"})).unwrap_err(), "read is a boolean");
        assert_eq!(check(&s, &json!({"labels": ["a", 3]})).unwrap_err(), "labels[1] is a string");
        assert_eq!(check(&s, &json!({"received": 1.5})).unwrap_err(), "received is an integer");
        assert_eq!(check(&s, &json!([1])).unwrap_err(), "attributes is an object");
    }

    #[test]
    fn sources_stay_under_import() {
        let src = |kind: &str, id: &str| source_of(&api::IngestSource { kind: kind.into(), id: id.into() });
        assert_eq!(src("mbox", "archive-2009").unwrap(), "import:mbox:archive-2009");
        assert_eq!(src("takeout", "").unwrap(), "import:takeout");
        assert!(src("gmail:abc", "").is_err() && src("", "x").is_err() && src("Mbox", "").is_err());
    }
}
