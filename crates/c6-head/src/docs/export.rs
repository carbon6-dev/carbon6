//! Export and import (docs/docs.md §10). The head writes Markdown and HTML
//! itself, from its own blocks. PDF, Word, OpenDocument and PowerPoint
//! come from the render kit (`c6-render`), which ships with carbon6 as a
//! system kit: nothing to install, built on a worker with Docker or Podman
//! the first time it's needed. Every export is kept as a blob with the
//! version it was made from. Import goes the other way: the kit reads a
//! Word or PowerPoint file into carbon6 Markdown, and the head stores it.

use std::collections::BTreeMap;

use base64::Engine;
use c6_api::docs::{Doc, DocBlock, DocExport, DocImport};
use c6_core::now_ms;
use serde_json::{Value, json};
use sqlx::Row as _;

use crate::{HeadState, bad_request, not_found};

/// The kit, as carbon6 ships it.
pub const KIT: &str = "c6-render";

/// The kit's manifest and the files its image is built from.
pub fn package() -> (Value, BTreeMap<String, Vec<u8>>) {
    let files = BTreeMap::from([
        ("Dockerfile".to_owned(), include_bytes!("../../kits/render/Dockerfile").to_vec()),
        ("render.py".to_owned(), include_bytes!("../../kits/render/render.py").to_vec()),
    ]);
    (serde_json::from_str(include_str!("../../kits/render/kit.json")).expect("the render kit's manifest"), files)
}

/// What a worker needs to build and run the kit (as `packages::kit_spec`
/// gives an installed kit's): it is carbon6's own, so no install and no
/// approval; its image is named after its content.
pub fn spec() -> Value {
    let (m, files) = package();
    let mut h = String::new();
    for (path, data) in &files {
        h.push_str(&format!("{path} {}\n", c6_core::sha256_hex(data)));
    }
    let tag = format!("c6kit/{KIT}:{}", &c6_core::sha256_hex(h.as_bytes())[..16]);
    let b64 = base64::engine::general_purpose::STANDARD;
    let enc: serde_json::Map<String, Value> = files.iter().map(|(k, v)| (k.clone(), json!(b64.encode(v)))).collect();
    json!({
        "name": KIT, "version": m["version"], "image": tag,
        "builds": [{"name": KIT, "version": m["version"], "tag": tag, "dockerfile": String::from_utf8_lossy(&files["Dockerfile"]), "files": enc}],
        "tools": m["kit"]["tools"], "permissions": m["permissions"],
    })
}

/// Which formats a kind exports to, and whether the kit makes them.
pub fn formats(kind: &str) -> &'static [(&'static str, bool)] {
    if kind == "deck" {
        &[("md", false), ("html", false), ("pdf", true), ("pptx", true), ("odp", true), ("png", true)]
    } else {
        &[("md", false), ("html", false), ("pdf", true), ("docx", true), ("odt", true)]
    }
}

fn media_type(format: &str) -> &'static str {
    match format {
        "md" => "text/markdown; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "odt" => "application/vnd.oasis.opendocument.text",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "odp" => "application/vnd.oasis.opendocument.presentation",
        "png" => "image/png",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// A document's Markdown for files: chips as text, pending blocks left out.
pub fn plain_markdown(state: &super::model::State) -> String {
    let mut out = Vec::new();
    for t in state.tabs_in_order() {
        let mut blocks = serde_json::Map::new();
        let md = state.markdown(&t.id, &mut blocks);
        let md = super::markdown::rename_tokens(&md, |_| None);
        let mut text = String::new();
        let mut rest = md.as_str();
        // Each token: a chip's text, an embed's name, a pending block gone.
        while let Some(i) = rest.find("<?c6 block ") {
            text.push_str(&rest[..i]);
            let after = &rest[i + 11..];
            let Some(j) = after.find("?>") else { break };
            let v = blocks.get(after[..j].trim()).cloned().unwrap_or(Value::Null);
            let s = |k: &str| v[k].as_str().unwrap_or("").to_owned();
            text.push_str(&match v["type"].as_str().unwrap_or("") {
                "pending" => String::new(),
                "date" | "dropdown" => s("value"),
                "mention" => format!("@{}", if s("name").is_empty() { s("user") } else { s("name") }),
                "ref" => {
                    if s("title").is_empty() {
                        s("ref")
                    } else {
                        s("title")
                    }
                }
                other => format!("[{other}]"),
            });
            rest = &after[j + 2..];
        }
        text.push_str(rest);
        if state.tabs.len() > 1 {
            out.push(format!("# {}\n\n{}", t.title, text.trim()));
        } else {
            out.push(text.trim().to_owned());
        }
    }
    out.join("\n\n").replace("\n\n\n\n", "\n\n") + "\n"
}

/// Markdown to HTML for files (raw HTML in a doc is sanitized).
pub fn md_html(md: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, html};
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let events = Parser::new_ext(md, opts).map(|ev| match ev {
        Event::Html(h) => Event::Html(c6_api::slides::sanitize_html(&h).into()),
        Event::InlineHtml(h) => Event::InlineHtml(c6_api::slides::sanitize_html(&h).into()),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, events);
    out
}

/// A doc as one HTML file, styled to read and print well.
pub fn doc_html(title: &str, md: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title><style>\
         @page {{ size: A4; margin: 22mm 20mm; }} body {{ font: 16px/1.6 'Helvetica Neue', Inter, Arial, sans-serif; color: #1d1d1f; \
         max-width: 760px; margin: 40px auto; padding: 0 24px; }} h1 {{ font-size: 2em; line-height: 1.2; }} h2 {{ margin-top: 1.6em; }} \
         table {{ border-collapse: collapse; }} th, td {{ border: 1px solid #ccc; padding: 6px 10px; text-align: left; }} \
         pre {{ background: #f4f4f5; padding: 12px; overflow: auto; }} code {{ font-size: .9em; }} blockquote {{ color: #555; \
         border-left: 3px solid #ddd; margin-left: 0; padding-left: 16px; }} img {{ max-width: 100%; }} @media print {{ body {{ margin: 0; }} }}\
         </style></head><body>{}</body></html>",
        title.replace('<', "&lt;"),
        md_html(md)
    )
}

/// A deck's slides as carbon6 draws them, rendered with the head's Markdown.
fn deck_html(doc: &Doc, css: &str, hidden: bool) -> String {
    let slides: Vec<DocBlock> = doc.tabs.first().map(|t| t.blocks.clone()).unwrap_or_default();
    c6_api::slides::deck_html(&doc.summary.title, &slides, css, hidden, &md_html)
}

/// A deck as the kit's deck.json: each shown slide's layout, regions in
/// Markdown (chips as text), notes, and whether it goes as a picture.
fn deck_json(doc: &Doc) -> Value {
    let slides: Vec<Value> = doc
        .tabs
        .first()
        .map(|t| t.blocks.clone())
        .unwrap_or_default()
        .iter()
        .filter(|s| s.attrs["hidden"] != true)
        .map(|s| {
            let layout = s.attrs["layout"].as_str().unwrap_or("body");
            let mut regions = serde_json::Map::new();
            for c in &s.children {
                let text = c6_api::slides::chips_shown(&c.text, &c.attrs);
                let entry = regions.entry(c.region.clone()).or_insert(json!(""));
                let joined =
                    if entry.as_str().unwrap_or("").is_empty() { text } else { format!("{}\n{text}", entry.as_str().unwrap_or("")) };
                *entry = json!(joined);
            }
            // What PowerPoint can't hold as text goes as a picture.
            let picture = s.kind == "pending"
                || matches!(layout, "html" | "chart" | "diagram" | "image")
                || s.children.iter().any(|c| matches!(c.kind.as_str(), "html" | "embed"));
            json!({"layout": layout, "regions": regions, "notes": s.attrs["notes"], "picture": picture})
        })
        .collect();
    json!({"title": doc.summary.title, "size": doc.summary.size, "html": "deck.html", "slides": slides})
}

/// Runs one of the kit's tools; its files by path under `out/`.
async fn run_kit(
    st: &HeadState,
    realm: &str,
    tool: &str,
    args: Value,
    inputs: BTreeMap<String, Vec<u8>>,
) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let job = crate::kitjobs::Job {
        spec: spec(),
        tool: tool.into(),
        args: args.as_object().cloned().unwrap_or_default(),
        inputs,
        realm: realm.to_owned(),
        ..Default::default()
    };
    let out = crate::kitjobs::run(st, job).await.map_err(|e| {
        bad_request(format!(
            "{e:#}. Markdown and HTML need no worker; PDF, Word, OpenDocument and PowerPoint are made by the render kit on a worker \
             with Docker or Podman"
        ))
    })?;
    if !out.ok() {
        return Err(bad_request(format!(
            "the render kit failed ({:?}): {}",
            out.exit,
            out.stderr.trim().chars().take(2000).collect::<String>()
        )));
    }
    Ok(out.files)
}

/// Pictures of a deck's slides (`first` to `last`, from 1; 0: all), as PNGs.
pub async fn pictures(st: &HeadState, realm: &str, id: &str, first: usize, last: usize) -> anyhow::Result<Vec<Vec<u8>>> {
    let doc = super::get(st, realm, id).await?;
    let html = if doc.summary.kind == "deck" {
        deck_html(&doc, &crate::decks::theme_css(st, realm, &doc.summary.theme).await, true)
    } else {
        let state = load_state(st, realm, id).await?;
        doc_html(&doc.summary.title, &plain_markdown(&state))
    };
    let inputs = BTreeMap::from([("deck.html".to_owned(), html.into_bytes())]);
    let args = json!({"html": "deck.html", "out": "out/slides", "first": first.to_string(), "last": last.to_string()});
    let files = run_kit(st, realm, "slides", args, inputs).await?;
    Ok(files.into_iter().filter(|(k, _)| k.ends_with(".png")).map(|(_, v)| v).collect())
}

async fn load_state(st: &HeadState, realm: &str, id: &str) -> anyhow::Result<super::model::State> {
    let pool = super::pool(st, realm).await?;
    let mut conn = pool.acquire().await?;
    Ok(super::load_head(&mut conn, id).await?.0)
}

fn file_name(title: &str, format: &str) -> String {
    let base: String = title.chars().map(|c| if c.is_alphanumeric() || c == ' ' || c == '-' { c } else { ' ' }).collect();
    let base = base.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{}.{}", if base.is_empty() { "document".to_owned() } else { base }, if format == "png" { "zip" } else { format })
}

/// Exports a document; the file is kept as a blob, listed with the version
/// it was made from.
pub async fn export(st: &HeadState, realm: &str, id: &str, format: &str, by: &str) -> anyhow::Result<DocExport> {
    let doc = super::get(st, realm, id).await?;
    let kind = doc.summary.kind.clone();
    let Some((_, kit)) = formats(&kind).iter().find(|(f, _)| *f == format) else {
        return Err(bad_request(format!("a {kind} exports to {}", formats(&kind).iter().map(|(f, _)| *f).collect::<Vec<_>>().join(", "))));
    };
    let state = load_state(st, realm, id).await?;
    let md = if kind == "deck" {
        let mut blocks = serde_json::Map::new();
        let tab = state.tabs_in_order().first().map(|t| t.id.clone()).unwrap_or_default();
        state.markdown(&tab, &mut blocks)
    } else {
        plain_markdown(&state)
    };
    let html = if kind == "deck" {
        deck_html(&doc, &crate::decks::theme_css(st, realm, &doc.summary.theme).await, false)
    } else {
        doc_html(&doc.summary.title, &md)
    };
    let bytes: Vec<u8> = if !kit {
        match format {
            "md" => md.into_bytes(),
            _ => html.into_bytes(),
        }
    } else {
        let mut inputs = BTreeMap::from([("doc.md".to_owned(), md.into_bytes()), ("deck.html".to_owned(), html.into_bytes())]);
        if kind == "deck" {
            inputs.insert("deck.json".into(), serde_json::to_vec(&deck_json(&doc))?);
        }
        let (tool, args) = match format {
            "pdf" => ("pdf", json!({"html": "deck.html", "out": "out/file.pdf"})),
            "docx" | "odt" => (format, json!({"md": "doc.md", "out": format!("out/file.{format}")})),
            "pptx" | "odp" => (format, json!({"deck": "deck.json", "out": format!("out/file.{format}")})),
            _ => ("slides", json!({"html": "deck.html", "out": "out/slides", "first": "0", "last": "0"})),
        };
        let files = run_kit(st, realm, tool, args, inputs).await?;
        if format == "png" {
            zip(&files)?
        } else {
            files.get(&format!("file.{format}")).cloned().ok_or_else(|| anyhow::anyhow!("the render kit made no {format}"))?
        }
    };
    keep(st, realm, &doc, format, bytes, by).await
}

/// A zip of the slides' pictures (stored, not compressed: PNGs are already).
fn zip(files: &BTreeMap<String, Vec<u8>>) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    let mut count = 0u16;
    for (name, data) in files.iter().filter(|(k, _)| k.ends_with(".png")) {
        let name = name.rsplit('/').next().unwrap_or(name).as_bytes();
        let crc = crc32(data);
        let offset = out.len() as u32;
        let header = |sig: u32, central: bool| -> Vec<u8> {
            let mut h = Vec::new();
            h.extend(sig.to_le_bytes());
            if central {
                h.extend(20u16.to_le_bytes());
            }
            h.extend([20, 0, 0, 0, 0, 0, 0, 0, 0x21, 0]);
            h.extend(crc.to_le_bytes());
            h.extend((data.len() as u32).to_le_bytes());
            h.extend((data.len() as u32).to_le_bytes());
            h.extend((name.len() as u16).to_le_bytes());
            h.extend(0u16.to_le_bytes());
            if central {
                h.extend([0u8; 8]);
                h.extend(0u32.to_le_bytes());
                h.extend(offset.to_le_bytes());
            }
            h.extend(name);
            h
        };
        out.extend(header(0x0403_4b50, false));
        out.extend(data);
        central.extend(header(0x0201_4b50, true));
        count += 1;
    }
    let start = out.len() as u32;
    out.extend(&central);
    out.extend(0x0605_4b50u32.to_le_bytes());
    out.extend([0u8; 4]);
    out.extend(count.to_le_bytes());
    out.extend(count.to_le_bytes());
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(start.to_le_bytes());
    out.extend(0u16.to_le_bytes());
    Ok(out)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for b in data {
        crc ^= u32::from(*b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

async fn keep(st: &HeadState, realm: &str, doc: &Doc, format: &str, bytes: Vec<u8>, by: &str) -> anyhow::Result<DocExport> {
    let m = crate::knowledge::get(st).ok_or_else(|| anyhow::anyhow!("the knowledge store is not open"))?;
    let rm = m.of_realm(realm).await?;
    let blob = rm.put_blob(&bytes).await?;
    let e = DocExport {
        id: uuid::Uuid::now_v7().to_string(),
        doc: doc.summary.id.clone(),
        format: format.to_owned(),
        rev: doc.summary.rev,
        name: file_name(&doc.summary.title, format),
        size: bytes.len() as u64,
        by: by.to_owned(),
        at: now_ms(),
        url: String::new(),
    };
    sqlx::query("INSERT INTO doc_exports (id, doc_id, format, rev, name, size, blob, by, at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(&e.id)
        .bind(&e.doc)
        .bind(&e.format)
        .bind(e.rev)
        .bind(&e.name)
        .bind(e.size as i64)
        .bind(&blob)
        .bind(&e.by)
        .bind(e.at)
        .execute(&rm.store.pool)
        .await?;
    let event = json!({"type": "doc", "op": "exported", "doc": {"id": doc.summary.id, "title": doc.summary.title, "kind": doc.summary.kind,
                       "realm": realm, "rev": doc.summary.rev}, "format": format, "export": e.id, "by": by});
    crate::triggers::dispatch(st, realm, "doc", "exported", event).await;
    Ok(with_url(e))
}

fn with_url(mut e: DocExport) -> DocExport {
    e.url = format!("/api/v1/docs/{}/exports/{}/file", e.doc, e.id);
    e
}

/// A document's past exports, newest first.
pub async fn list(st: &HeadState, realm: &str, id: &str) -> anyhow::Result<Vec<DocExport>> {
    let pool = super::pool(st, realm).await?;
    let rows = sqlx::query("SELECT * FROM doc_exports WHERE doc_id = ? ORDER BY at DESC LIMIT 100").bind(id).fetch_all(&pool).await?;
    Ok(rows
        .iter()
        .map(|r| {
            with_url(DocExport {
                id: r.get("id"),
                doc: r.get("doc_id"),
                format: r.get("format"),
                rev: r.get("rev"),
                name: r.get("name"),
                size: r.get::<i64, _>("size") as u64,
                by: r.get("by"),
                at: r.get("at"),
                url: String::new(),
            })
        })
        .collect())
}

/// An export's file: its name, media type and bytes.
pub async fn file(st: &HeadState, realm: &str, id: &str, export: &str) -> anyhow::Result<(String, &'static str, Vec<u8>)> {
    let pool = super::pool(st, realm).await?;
    let r = sqlx::query("SELECT * FROM doc_exports WHERE doc_id = ? AND id = ?")
        .bind(id)
        .bind(export)
        .fetch_optional(&pool)
        .await?
        .ok_or_else(|| not_found(format!("no export {export}")))?;
    let m = crate::knowledge::get(st).ok_or_else(|| anyhow::anyhow!("the knowledge store is not open"))?;
    let rm = m.of_realm(realm).await?;
    let blob: String = r.get("blob");
    let bytes = tokio::fs::read(rm.blob_path(&blob)).await?;
    let format: String = r.get("format");
    Ok((r.get("name"), media_type(if format == "png" { "zip" } else { &format }), bytes))
}

/// A file becomes a document: Markdown is read here (it is carbon6's own
/// exchange format); Word, OpenDocument and PowerPoint by the render kit.
pub async fn import(st: &HeadState, realm: &str, i: &DocImport, by: &str) -> anyhow::Result<(Doc, c6_api::docs::DocApplied)> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(i.content.trim())
        .map_err(|e| bad_request(format!("content is the file in base64: {e}")))?;
    if bytes.len() > 50 * 1024 * 1024 {
        return Err(bad_request("files up to 50 MB come in"));
    }
    let name = i.name.trim();
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    let markdown = match ext.as_str() {
        "md" | "markdown" | "txt" => String::from_utf8(bytes).map_err(|_| bad_request("that Markdown isn't UTF-8"))?,
        "docx" | "odt" | "pptx" | "html" | "htm" => {
            let input = format!("in.{ext}");
            let files =
                run_kit(st, realm, "import", json!({"file": input, "out": "out/doc.md"}), BTreeMap::from([(input.clone(), bytes)])).await?;
            String::from_utf8_lossy(files.get("doc.md").ok_or_else(|| anyhow::anyhow!("the render kit read nothing"))?).into_owned()
        }
        _ => return Err(bad_request("a Word (.docx), OpenDocument (.odt), PowerPoint (.pptx), HTML or Markdown (.md) file")),
    };
    let kind = if !i.kind.is_empty() {
        i.kind.clone()
    } else if ext == "pptx" {
        "deck".into()
    } else {
        "doc".into()
    };
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let title = if i.title.trim().is_empty() { stem.to_owned() } else { i.title.trim().to_owned() };
    // Tokens in a file from outside are its text, not carbon6's.
    let markdown = markdown.replace("<?c6 block", "&lt;?c6 block");
    let n = c6_api::docs::DocNew { kind, title, markdown, labels: i.labels.clone(), ..Default::default() };
    super::create(st, realm, &n, by).await
}
