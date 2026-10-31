//! Google Docs and Google Slides (docs/docs.md §10). The render kit makes a
//! Word file of a doc or a PowerPoint file of a deck, and the head uploads
//! it to Google Drive, converted to a Google Doc or Slides. The sign-in is
//! the realm's own for documents (instance `c6:docs` among the OAuth
//! accounts), with the Google provider the Google apps declare and your
//! OAuth client, asking only for `drive.file`: carbon6 reaches the files it
//! made, nothing else in your Drive. The document remembers its Google
//! file; exporting again updates it (saying so first if it changed in
//! Google since), or makes a new one.

use c6_api as api;
use c6_api::docs::{DocGoogle, GoogleExportIn, GoogleFile};
use c6_core::now_ms;
use serde_json::{Value, json};
use sqlx::Row as _;

use crate::{HeadState, bad_request, conflict, not_found};

/// Where the documents' Google accounts are kept (no app has a name with a
/// colon).
pub const INSTANCE: &str = "c6:docs";
pub const SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
/// A head setting: Google's addresses, for a stand-in or a proxy:
/// {authorize_url, token_url, userinfo_url, api_url}.
pub const ENDPOINTS: &str = "google_endpoints";
const API: &str = "https://www.googleapis.com";

async fn endpoints(st: &HeadState) -> Value {
    st.store.setting(ENDPOINTS).await.ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null)
}

async fn api_url(st: &HeadState) -> String {
    endpoints(st).await["api_url"].as_str().unwrap_or(API).trim_end_matches('/').to_owned()
}

/// The provider: the Google apps' (same client, same addresses), asking
/// for drive.file only.
pub async fn provider(st: &HeadState) -> anyhow::Result<api::OAuthProvider> {
    let gmail = crate::google::packages()
        .into_iter()
        .map(|(m, _)| m)
        .find(|m| m["package"] == "gmail")
        .ok_or_else(|| anyhow::anyhow!("the gmail package is missing"))?;
    let mut p: api::OAuthProvider = serde_json::from_value(gmail["providers"][0].clone())?;
    p.scopes = ["openid", "email", "profile", SCOPE].map(str::to_owned).to_vec();
    let e = endpoints(st).await;
    for (k, slot) in [("authorize_url", &mut p.authorize_url), ("token_url", &mut p.token_url), ("userinfo_url", &mut p.userinfo_url)] {
        if let Some(u) = e[k].as_str() {
            *slot = u.to_owned();
        }
    }
    Ok(p)
}

/// Starts the sign-in; you come back to `back`.
pub async fn connect(st: &HeadState, realm: &str, origin: Option<&str>, back: &str) -> anyhow::Result<String> {
    let redirect = crate::google::redirect_uri(st, origin).await;
    let back = if back.starts_with('/') { back } else { "/settings" };
    crate::providers::connect_many(st, realm, &[INSTANCE.to_owned()], "google", &redirect, back).await
}

pub async fn accounts(st: &HeadState, realm: &str) -> anyhow::Result<Vec<api::OAuthAccount>> {
    crate::providers::accounts(st, realm, INSTANCE).await
}

pub async fn remove(st: &HeadState, realm: &str, id: &str) -> anyhow::Result<()> {
    crate::providers::remove(st, realm, INSTANCE, id).await
}

fn google_kind(kind: &str) -> (&'static str, &'static str, &'static str) {
    if kind == "deck" {
        ("pptx", "application/vnd.google-apps.presentation", "application/vnd.openxmlformats-officedocument.presentationml.presentation")
    } else {
        ("docx", "application/vnd.google-apps.document", "application/vnd.openxmlformats-officedocument.wordprocessingml.document")
    }
}

async fn remembered(st: &HeadState, realm: &str, id: &str) -> anyhow::Result<Option<GoogleFile>> {
    let pool = super::pool(st, realm).await?;
    let r = sqlx::query("SELECT * FROM doc_google WHERE doc_id = ?").bind(id).fetch_optional(&pool).await?;
    Ok(r.map(|r| GoogleFile {
        id: r.get("file_id"),
        url: r.get("url"),
        account: r.get("account"),
        rev: r.get("rev"),
        modified: r.get("modified"),
        by: r.get("by"),
        at: r.get("at"),
    }))
}

/// A document's Google file, and the accounts it can go with.
pub async fn of_doc(st: &HeadState, realm: &str, id: &str) -> anyhow::Result<DocGoogle> {
    let doc = super::get(st, realm, id).await?;
    Ok(DocGoogle {
        accounts: accounts(st, realm).await?.into_iter().map(|a| a.account).collect(),
        file: remembered(st, realm, id).await?,
        app: if doc.summary.kind == "deck" { "Google Slides" } else { "Google Docs" }.into(),
    })
}

/// What Drive said, or why it refused.
async fn drive(r: reqwest::Response) -> anyhow::Result<Value> {
    let status = r.status();
    let body: Value = r.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(body);
    }
    let why = body["error"]["message"].as_str().unwrap_or("no reason given").to_owned();
    Err(match status.as_u16() {
        401 | 403 => bad_request(format!("Google Drive refused ({status}): {why}. Connect the Google account again in Settings → Docs.")),
        404 => not_found(format!("The Google file is gone ({why}); make a new one")),
        _ => anyhow::anyhow!("Google Drive: {status}: {why}"),
    })
}

const FIELDS: &str = "id,name,webViewLink,modifiedTime,trashed";

/// Exports a document to Google: a new file, or its Google file updated.
pub async fn export(st: &HeadState, realm: &str, id: &str, g: &GoogleExportIn, by: &str) -> anyhow::Result<GoogleFile> {
    let doc = super::get(st, realm, id).await?;
    let accounts = accounts(st, realm).await?;
    let before = remembered(st, realm, id).await?;
    // The account it went with before, else the one named, else the first.
    let want = before.as_ref().filter(|_| !g.new).map(|f| f.account.clone()).or_else(|| g.account.clone());
    let account = match want.filter(|w| !w.is_empty()) {
        Some(w) => accounts.iter().find(|a| a.account.eq_ignore_ascii_case(&w)).ok_or_else(|| {
            bad_request(format!("{w} isn't connected for documents in {realm} any more: connect it in Settings → Docs, or make a new file"))
        })?,
        None => accounts.first().ok_or_else(|| {
            bad_request(format!("Connect a Google account for documents in {realm} first (Settings → Docs, or Connect Google here)"))
        })?,
    };
    let token = crate::providers::token_of(st, &account.id).await?;
    let api = api_url(st).await;
    // Updating: not over changes made in Google since, unless you say so.
    let update = match before.as_ref().filter(|_| !g.new) {
        Some(f) => {
            let now =
                drive(st.http.get(format!("{api}/drive/v3/files/{}?fields={FIELDS}", f.id)).bearer_auth(&token).send().await?).await?;
            if now["trashed"].as_bool() == Some(true) {
                return Err(not_found("The Google file is in the trash; make a new one"));
            }
            let modified = now["modifiedTime"].as_str().unwrap_or_default();
            if !f.modified.is_empty() && modified != f.modified && !g.anyway {
                return Err(conflict(format!(
                    "The Google file changed in Google since the last export (it was modified {modified}): updating replaces those \
                     changes. Update anyway, or make a new one."
                )));
            }
            Some(f.id.clone())
        }
        None => None,
    };
    let (format, google_type, media) = google_kind(&doc.summary.kind);
    let bytes = super::export::render(st, realm, &doc, format).await?;
    let made = match &update {
        // Whether Drive converts the new content of a file that is already
        // a Google Doc or Slides is unverified against Google itself (see
        // docs/docs.md §16).
        Some(file) => {
            let url = format!("{api}/upload/drive/v3/files/{file}?uploadType=media&fields={FIELDS}");
            drive(st.http.patch(url).bearer_auth(&token).header("content-type", media).body(bytes).send().await?).await?
        }
        None => {
            let meta = json!({"name": doc.summary.title, "mimeType": google_type});
            let boundary = format!("c6-{}", hex::encode(rand::random::<[u8; 12]>()));
            let mut body = format!("--{boundary}\r\ncontent-type: application/json; charset=utf-8\r\n\r\n{meta}\r\n--{boundary}\r\ncontent-type: {media}\r\n\r\n")
                .into_bytes();
            body.extend_from_slice(&bytes);
            body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
            let url = format!("{api}/upload/drive/v3/files?uploadType=multipart&fields={FIELDS}");
            let ct = format!("multipart/related; boundary={boundary}");
            drive(st.http.post(url).bearer_auth(&token).header("content-type", ct).body(body).send().await?).await?
        }
    };
    let file_id = made["id"].as_str().ok_or_else(|| anyhow::anyhow!("Google Drive gave no file id: {made}"))?.to_owned();
    let url = made["webViewLink"].as_str().map(str::to_owned).unwrap_or_else(|| {
        format!("https://docs.google.com/{}/d/{file_id}/edit", if doc.summary.kind == "deck" { "presentation" } else { "document" })
    });
    let f = GoogleFile {
        id: file_id,
        url,
        account: account.account.clone(),
        rev: doc.summary.rev,
        modified: made["modifiedTime"].as_str().unwrap_or_default().to_owned(),
        by: by.to_owned(),
        at: now_ms(),
    };
    let pool = super::pool(st, realm).await?;
    sqlx::query(
        "INSERT INTO doc_google (doc_id, file_id, url, account, rev, modified, by, at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (doc_id) DO UPDATE SET file_id = excluded.file_id, url = excluded.url, account = excluded.account, \
         rev = excluded.rev, modified = excluded.modified, by = excluded.by, at = excluded.at",
    )
    .bind(id)
    .bind(&f.id)
    .bind(&f.url)
    .bind(&f.account)
    .bind(f.rev)
    .bind(&f.modified)
    .bind(&f.by)
    .bind(f.at)
    .execute(&pool)
    .await?;
    let event = json!({"type": "doc", "op": "exported", "doc": {"id": doc.summary.id, "title": doc.summary.title, "kind": doc.summary.kind,
                       "realm": realm, "rev": doc.summary.rev}, "format": "google", "google": f, "updated": update.is_some(), "by": by});
    crate::triggers::dispatch(st, realm, "doc", "exported", event).await;
    Ok(f)
}
