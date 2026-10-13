//! One way in (docs/memex.md §4.5): `memex.ingest` takes items typed by
//! their MIME type, and each type's handler keeps its own parsing,
//! identity and storage. Everything that comes in this way is imported:
//! only connectors (Gmail sync) create mirrored items, whose changes are
//! written back, and ingested sources can't reach theirs.

use base64::Engine;
use c6_api as api;
use serde_json::{Value, json};

use crate::Memex;

/// Items per call.
const MAX_ITEMS: usize = 500;
/// The largest message.
const MAIL_LIMIT: u64 = 25 * 1024 * 1024;

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
            name: "document".into(),
            types: DOC_TYPES.iter().map(|(t, _)| t.to_string()).collect(),
            attributes: doc_schema(),
            identity: "the content's hash, across all documents".into(),
            limit: crate::MAX_FILE,
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
        "received": {"type": "integer", "description": "When it arrived (ms since the epoch), for a message without a Date"}
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
    let batch: api::MemexIngest = serde_json::from_value(params)?;
    anyhow::ensure!(batch.items.len() <= MAX_ITEMS, "at most {MAX_ITEMS} items a call");
    let mut out = Vec::with_capacity(batch.items.len());
    for item in &batch.items {
        out.push(match one(m, item).await {
            Ok(r) => r,
            Err(e) => api::IngestResult { status: "rejected".into(), error: Some(format!("{e:#}")), ..Default::default() },
        });
    }
    Ok(json!({"results": out}))
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

async fn one(m: &Memex, item: &api::IngestItem) -> anyhow::Result<api::IngestResult> {
    let mime = item.mime.split(';').next().unwrap_or("").trim().to_lowercase();
    let handler = types().into_iter().find(|h| h.types.contains(&mime));
    let Some(handler) = handler else {
        let known: Vec<String> = types().into_iter().flat_map(|h| h.types).collect();
        anyhow::bail!("no handler for {:?} (memex.types lists them: {})", item.mime, known.join(", "));
    };
    let source = source_of(&item.source)?;
    let external_id = item.external_id.trim();
    anyhow::ensure!(!external_id.is_empty() && external_id.len() <= 1000, "external_id is required (the source's own id for it)");
    check(&handler.attributes, &item.attributes).map_err(|e| anyhow::anyhow!(e))?;
    let data = match (&item.data, &item.text) {
        (Some(d), _) => {
            anyhow::ensure!((d.len() as u64) / 4 * 3 <= handler.limit + 3, "larger than {} MB", handler.limit >> 20);
            base64::engine::general_purpose::STANDARD.decode(d.trim()).map_err(|e| anyhow::anyhow!("data is not base64: {e}"))?
        }
        (None, Some(t)) => t.as_bytes().to_vec(),
        (None, None) => anyhow::bail!("data (base64) or text is required"),
    };
    anyhow::ensure!(data.len() as u64 <= handler.limit, "larger than {} MB", handler.limit >> 20);
    match handler.name.as_str() {
        "mail" => mail(m, &source, external_id, &data, &item.attributes).await,
        _ => {
            let ext = DOC_TYPES.iter().find(|(t, _)| *t == mime).map(|(_, e)| *e).unwrap_or("txt");
            let name = item.attributes["name"].as_str().unwrap_or(external_id);
            document(m, &source, external_id, name, ext, &data, &item.labels, item.attributes["created"].as_i64()).await
        }
    }
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

/// A document: its text (and, for a paper, its identity and citations),
/// recognized by the content's hash.
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
    // The extension tells the extractor what it is.
    let file = if name.to_lowercase().ends_with(&format!(".{ext}")) { name.to_owned() } else { format!("{name}.{ext}") };
    let (id, kind, _) = m.add_document(&file, data, source, external_id, labels, created).await?;
    Ok(api::IngestResult {
        status: if before.is_some() { "updated" } else { "created" }.into(),
        id: Some(id),
        kind: Some(kind),
        error: None,
    })
}

/// A message: stored with the mail, threaded by its References and
/// In-Reply-To, recognized by its Message-ID.
async fn mail(m: &Memex, source: &str, external_id: &str, raw: &[u8], attrs: &Value) -> anyhow::Result<api::IngestResult> {
    if let Some(id) = existing(m, source, external_id).await? {
        return Ok(duplicate(id, None));
    }
    let msg = mail_parser::MessageParser::default().parse(raw).ok_or_else(|| anyhow::anyhow!("not an RFC 822 message"))?;
    anyhow::ensure!(
        msg.from().is_some() || msg.message_id().is_some() || msg.subject().is_some() || msg.date().is_some(),
        "not an RFC 822 message: no headers"
    );
    let rfc_id = match msg.message_id() {
        Some(id) if !id.trim().is_empty() => id.trim().to_owned(),
        _ => {
            let key = format!(
                "{:?}|{:?}|{}|{}",
                msg.from().and_then(|a| a.first()).and_then(|a| a.address()),
                msg.date().map(|d| d.to_timestamp()),
                msg.subject().unwrap_or(""),
                msg.body_text(0).unwrap_or_default()
            );
            format!("{}@c6.import", &c6_core::sha256_hex(key.as_bytes())[..32])
        }
    };
    let same: Option<String> =
        sqlx::query_scalar("SELECT item_id FROM mail WHERE rfc_id = ? LIMIT 1").bind(&rfc_id).fetch_optional(&m.store.pool).await?;
    if let Some(id) = same {
        return Ok(duplicate(id, Some("email".into())));
    }
    // The thread's root: the first reference, else what it answers, else
    // itself; the same in any order the messages arrive.
    let root = msg
        .references()
        .as_text_list()
        .and_then(|l| l.first().map(|r| r.to_string()))
        .or_else(|| msg.in_reply_to().as_text_list().and_then(|l| l.first().map(|r| r.to_string())))
        .unwrap_or_else(|| rfc_id.clone());
    let thread_id = format!("i{}", &c6_core::sha256_hex(root.trim().as_bytes())[..16]);
    let date = msg.date().map(|d| d.to_timestamp() * 1000).or_else(|| attrs["received"].as_i64()).unwrap_or_else(c6_core::now_ms);

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
    let from = msg.from().and_then(|a| a.first()).and_then(|a| a.address()).map(str::to_lowercase).unwrap_or_default();
    if me.contains(&from) {
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
    };
    let id = crate::mail::store(m, &msg, raw, &at, &labels, date).await?;
    let kind = if labels.iter().any(|l| l == "SPAM") { "spam" } else { "email" };
    Ok(api::IngestResult { status: "created".into(), id: Some(id), kind: Some(kind.into()), error: None })
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
            "unknown attribute \"fodler\" (known: folder, labels, read, received, starred)"
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
