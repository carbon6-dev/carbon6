//! publish_file: the agent hands the user a deliverable (a report, a slide
//! deck, a chart, a document). The runtime records the file's content in
//! the session, where the UI previews it under Outputs; publishing the same
//! path again adds a version.

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

/// Largest file that can be published.
pub const MAX_OUTPUT: usize = 20 * 1024 * 1024;

/// A published file, for the runtime to record.
#[derive(Clone, Debug)]
pub struct Published {
    /// Its path (relative to the session's folder when inside it).
    pub name: String,
    pub title: String,
    pub mime: String,
    pub data: Vec<u8>,
}

pub type PublishSink = tokio::sync::mpsc::UnboundedSender<Published>;

/// The media type for a file name.
pub fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "txt" | "log" => "text/plain",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "zip" => "application/zip",
        "mp4" => "video/mp4",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

pub struct Publish {
    claude: bool,
}

impl Publish {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

#[async_trait]
impl Tool for Publish {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "PublishFile" } else { "publish_file" }.into(),
            description: "Publishes a finished deliverable (a report, slide deck, chart, document, data file) so the user \
                          can see it under the session's Outputs, previewed: HTML, PDF, images, Markdown and text are shown, \
                          other files offered for download. Publish again after changes to add a new version. Use it for \
                          what the user asked you to produce, not for intermediate files."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "The file to publish"},
                "title": {"type": "string", "description": "A short title for it (default: the file name)"}
            }, "required": ["path"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let Some(path) = input["path"].as_str().filter(|p| !p.trim().is_empty()) else {
            return ToolOutput::err("path is required");
        };
        let Some(sink) = ctx.session.publisher() else {
            return ToolOutput::err("publishing is not available here");
        };
        let full = ctx.session.resolve(path);
        let (kind, _) = ctx.session.fs_stat(&full).await;
        if kind != crate::remote::Kind::File {
            return ToolOutput::err(format!("{} is not a file", full.display()));
        }
        let data = match ctx.session.fs_read(&full).await {
            Ok(d) => d,
            Err(e) => return ToolOutput::err(format!("cannot read {}: {e}", full.display())),
        };
        if data.len() > MAX_OUTPUT {
            return ToolOutput::err(format!(
                "{} is {} MB; outputs are limited to {} MB",
                full.display(),
                data.len() >> 20,
                MAX_OUTPUT >> 20
            ));
        }
        let cwd = ctx.session.cwd();
        let name = full.strip_prefix(&cwd).unwrap_or(&full).display().to_string();
        let title = input["title"].as_str().map(str::trim).filter(|t| !t.is_empty()).unwrap_or(&name).to_owned();
        let mime = mime_for(&name).to_owned();
        let size = data.len();
        if sink.send(Published { name: name.clone(), title: title.clone(), mime, data }).is_err() {
            return ToolOutput::err("publishing is not available here");
        }
        ToolOutput::ok(format!("Published {title} ({name}, {size} bytes). The user can see it under Outputs."))
    }
}
