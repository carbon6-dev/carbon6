//! `@path` mentions in a user message: the mentioned files (or folder
//! listings) are attached to the message when it is received, so the model
//! has them without a tool call, and replays see the same content.

use std::path::{Path, PathBuf};

use c6_agent::PendingInput;
use c6_proto::Attachment;

/// Larger text files are not inlined (the model can still read them).
const MAX_TEXT: u64 = 256 * 1024;
const MAX_IMAGE: u64 = 5 * 1024 * 1024;
const MAX_LISTING: usize = 300;

fn image_type(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Attaches what the message's `@path` tokens name, relative to `cwd` (or
/// absolute). Tokens that name nothing are left as text.
pub fn resolve(cwd: &Path, input: &mut PendingInput) {
    // A slash command's expansion can mention files too.
    let tokens: Vec<String> = [input.text.as_str(), input.prompt.as_str()]
        .into_iter()
        .flat_map(str::split_whitespace)
        .filter_map(|w| w.strip_prefix('@'))
        .map(|t| t.trim_end_matches([',', '.', ';', ':', ')', '!', '?', '"', '\'', '`']).to_owned())
        .filter(|t| !t.is_empty())
        .collect();
    for token in tokens {
        if input.attachments.iter().any(|a| a.name == token) {
            continue;
        }
        let path = if Path::new(&token).is_absolute() { PathBuf::from(&token) } else { cwd.join(&token) };
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        let attachment = if meta.is_dir() {
            Some(listing(&token, &path))
        } else if let Some(mime) = image_type(&path) {
            (meta.len() <= MAX_IMAGE).then(|| std::fs::read(&path).ok()).flatten().map(|data| Attachment {
                name: token.clone(),
                mime: mime.into(),
                data,
            })
        } else if meta.len() > MAX_TEXT {
            Some(text(&token, format!("({} bytes: too large to include; read it with a tool)", meta.len())))
        } else {
            std::fs::read(&path).ok().and_then(|data| String::from_utf8(data).ok()).map(|t| text(&token, t))
        };
        if let Some(a) = attachment {
            input.attachments.push(a);
        }
    }
}

fn text(name: &str, body: String) -> Attachment {
    Attachment { name: name.to_owned(), mime: "text/plain".into(), data: body.into_bytes() }
}

/// A folder as a listing of its entries (folders end in /).
fn listing(name: &str, dir: &Path) -> Attachment {
    let mut entries: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if e.file_type().is_ok_and(|t| t.is_dir()) { format!("{n}/") } else { n }
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    let more = entries.len().saturating_sub(MAX_LISTING);
    entries.truncate(MAX_LISTING);
    let mut body = format!("Folder {name} contains:\n{}", entries.join("\n"));
    if more > 0 {
        body.push_str(&format!("\n… and {more} more"));
    }
    text(name, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attaches_files_folders_and_images() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::create_dir(d.join("src")).unwrap();
        std::fs::write(d.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(d.join("shot.png"), [0x89, b'P', b'N', b'G']).unwrap();
        let mut input =
            PendingInput { text: "Look at @src/main.rs, @src and @shot.png; not @missing.txt or me@example".into(), ..Default::default() };
        resolve(d, &mut input);
        let names: Vec<(&str, &str)> = input.attachments.iter().map(|a| (a.name.as_str(), a.mime.as_str())).collect();
        assert_eq!(names, [("src/main.rs", "text/plain"), ("src", "text/plain"), ("shot.png", "image/png")]);
        assert_eq!(input.attachments[0].data, b"fn main() {}");
        assert!(String::from_utf8_lossy(&input.attachments[1].data).contains("main.rs"));
    }
}
