//! c6-extract: what carbon6's knowledge store needs from files it must not
//! parse itself (docs/knowledge.md §4). It runs as the `c6-extract` kit on
//! a worker, sandboxed with no network: the head writes the files (or
//! mounts a worker's own, read-only) and `jobs.json`, and reads back
//! `out/result.json` and the files it made, under `out/<job id>/`.
//!
//! `jobs.json`: `{"tool", "jobs": [{"id", "path", "name", …}], …}`, tools:
//! - `document`: text, a PDF's metadata title, and a paper's identity and
//!   citations;
//! - `mail`: a message's headers, text and attachments' names (`html`);
//! - `attachment`: a message's attachment `index` (`data`);
//! - `media`: a photo's EXIF and size, or a video's container (`thumb.jpg`);
//! - `faces`: faces in a photo, with the models `detector` and `embedder`
//!   (`face-N.jpg`);
//! - `video`: keyframes (`fNN.jpg`) and, with `sound`, the sound (`a.m4a`).

pub mod docs;
pub mod faces;
pub mod mail;
pub mod media;

use std::path::Path;

use serde_json::{Value, json};

/// The largest file read whole.
const MAX_FILE: u64 = 200 * 1024 * 1024;

/// Runs a request: each job's result (or its error) by id, and its files
/// under `out/<id>/`. `faces` stands in for the request's face models.
pub fn run(req: &Value, out: &Path, faces: Option<&dyn faces::FaceModel>) -> Value {
    let tool = req["tool"].as_str().unwrap_or_default();
    let loaded;
    let model: Option<&dyn faces::FaceModel> = match (tool, faces) {
        (_, Some(f)) => Some(f),
        ("faces", None) => {
            match faces::Onnx::load(req["detector"].as_str().unwrap_or_default(), req["embedder"].as_str().unwrap_or_default()) {
                Ok(m) => {
                    loaded = m;
                    Some(&loaded)
                }
                Err(e) => return json!({"error": format!("face models: {e:#}")}),
            }
        }
        _ => None,
    };
    let mut results = serde_json::Map::new();
    for job in req["jobs"].as_array().into_iter().flatten() {
        let id = job["id"].as_str().unwrap_or_default();
        if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
            continue;
        }
        let dir = out.join(id);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| one(tool, job, &dir, model))).unwrap_or_else(|p| {
            let why = p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_default();
            Err(anyhow::anyhow!("could not read it ({why})"))
        });
        results.insert(id.to_owned(), r.unwrap_or_else(|e| json!({"error": format!("{e:#}")})));
    }
    json!({"results": results})
}

fn read(path: &Path) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(std::fs::metadata(path)?.len() <= MAX_FILE, "larger than {} MB", MAX_FILE >> 20);
    Ok(std::fs::read(path)?)
}

fn write(dir: &Path, name: &str, data: &[u8]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    Ok(std::fs::write(dir.join(name), data)?)
}

fn one(tool: &str, job: &Value, dir: &Path, model: Option<&dyn faces::FaceModel>) -> anyhow::Result<Value> {
    let path = Path::new(job["path"].as_str().unwrap_or_default());
    let name = job["name"].as_str().filter(|n| !n.is_empty()).unwrap_or_else(|| path.file_name().and_then(|n| n.to_str()).unwrap_or(""));
    match tool {
        "document" => docs::document(name, &read(path)?),
        "mail" => {
            let (v, html) = mail::message(&read(path)?)?;
            if let Some(h) = html {
                write(dir, "html", h.as_bytes())?;
            }
            Ok(v)
        }
        "attachment" => {
            let (name, mime, data) = mail::attachment(&read(path)?, job["index"].as_u64().unwrap_or(0) as usize)?;
            write(dir, "data", &data)?;
            Ok(json!({"name": name, "mime": mime}))
        }
        "media" => {
            let (v, thumb) = media::media(path)?;
            if let Some(t) = thumb {
                write(dir, "thumb.jpg", &t)?;
            }
            Ok(v)
        }
        "faces" => {
            let model = model.ok_or_else(|| anyhow::anyhow!("no face models"))?;
            let found = faces::faces(model, path)?;
            let mut list = Vec::new();
            for (i, (f, v, crop)) in found.into_iter().enumerate() {
                write(dir, &format!("face-{i}.jpg"), &crop)?;
                list.push(json!({"x": f.x, "y": f.y, "w": f.w, "h": f.h, "score": f.score, "vec": v}));
            }
            Ok(json!({"faces": list}))
        }
        "video" => {
            std::fs::create_dir_all(dir)?;
            let sound = media::video(path, dir, job["sound"].as_bool().unwrap_or(false))?;
            let frames = (1..=6).filter(|i| dir.join(format!("f{i:02}.jpg")).exists()).count();
            Ok(json!({"frames": frames, "sound": sound}))
        }
        _ => anyhow::bail!("no tool {tool:?}"),
    }
}
