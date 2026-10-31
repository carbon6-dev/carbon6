//! Pictures and files in documents, and ref chips as they are now
//! (docs/docs.md §3). A picture or file is a blob in the realm's knowledge
//! folder, listed for its document, so it is served only through the
//! document that holds it; an image or file embed names it.

use base64::Engine;
use c6_api::docs::{DocFile, DocFileIn};
use c6_api::work::LiveAttached;
use c6_core::now_ms;
use sqlx::Row as _;

use crate::{HeadState, bad_request, not_found};

/// The largest picture or file.
pub const MAX_FILE: usize = 25 * 1024 * 1024;

fn mime_of(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "txt" | "md" => "text/plain",
        "csv" => "text/csv",
        "json" => "application/json",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

fn url_of(doc: &str, blob: &str) -> String {
    format!("/api/v1/docs/{doc}/files/{blob}")
}

/// Keeps a picture or file for a document.
pub async fn add(st: &HeadState, realm: &str, doc: &str, f: &DocFileIn, by: &str) -> anyhow::Result<DocFile> {
    super::get(st, realm, doc).await?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(f.content.trim())
        .map_err(|e| bad_request(format!("content is the file in base64: {e}")))?;
    if bytes.is_empty() || bytes.len() > MAX_FILE {
        return Err(bad_request(format!("a file is up to {} MB", MAX_FILE >> 20)));
    }
    let name: String = f.name.trim().chars().filter(|c| !c.is_control() && *c != '/' && *c != '\\').take(200).collect();
    if name.is_empty() {
        return Err(bad_request("a file needs its name"));
    }
    let mime = if f.mime.trim().is_empty() { mime_of(&name).to_owned() } else { f.mime.trim().to_owned() };
    let m = crate::knowledge::get(st).ok_or_else(|| anyhow::anyhow!("the knowledge store is not open"))?;
    let rm = m.of_realm(realm).await?;
    let blob = rm.put_blob(&bytes).await?;
    sqlx::query(
        "INSERT INTO doc_files (doc_id, blob, name, mime, size, by, at) VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (doc_id, blob) DO UPDATE SET name = excluded.name, mime = excluded.mime",
    )
    .bind(doc)
    .bind(&blob)
    .bind(&name)
    .bind(&mime)
    .bind(bytes.len() as i64)
    .bind(by)
    .bind(now_ms())
    .execute(&rm.store.pool)
    .await?;
    Ok(DocFile { url: url_of(doc, &blob), blob, name, mime, size: bytes.len() as u64 })
}

/// A document's picture or file: its name, media type and bytes.
pub async fn get(st: &HeadState, realm: &str, doc: &str, blob: &str) -> anyhow::Result<(String, String, Vec<u8>)> {
    let m = crate::knowledge::get(st).ok_or_else(|| anyhow::anyhow!("the knowledge store is not open"))?;
    let rm = m.of_realm(realm).await?;
    let r = sqlx::query("SELECT name, mime FROM doc_files WHERE doc_id = ? AND blob = ?")
        .bind(doc)
        .bind(blob)
        .fetch_optional(&rm.store.pool)
        .await?
        .ok_or_else(|| not_found("no such file in this document"))?;
    let bytes = tokio::fs::read(rm.blob_path(blob)).await?;
    Ok((r.get("name"), r.get("mime"), bytes))
}

/// carbon6 things as they are now (title and state), for ref chips.
pub async fn refs(st: &HeadState, realm: &str, refs: &[String]) -> anyhow::Result<Vec<LiveAttached>> {
    let mut out = Vec::new();
    for r in refs.iter().take(200) {
        out.push(crate::work::describe(st, realm, r).await?);
    }
    Ok(out)
}
