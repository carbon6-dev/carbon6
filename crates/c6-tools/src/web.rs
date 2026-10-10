//! web_fetch: fetch a URL and return it as Markdown.

use std::time::Duration;

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

const MAX_BODY: usize = 5 * 1024 * 1024;

pub struct WebFetch {
    claude: bool,
}

impl WebFetch {
    pub fn generic() -> Self {
        Self { claude: false }
    }

    pub fn claude() -> Self {
        Self { claude: true }
    }
}

#[async_trait]
impl Tool for WebFetch {
    fn spec(&self) -> ToolSpec {
        let mut props = json!({"url": {"type": "string", "description": "The http(s) URL to fetch"}});
        let mut required = vec!["url"];
        if self.claude {
            props["prompt"] = json!({"type": "string", "description": "What to look for in the page"});
            required.push("prompt");
        }
        ToolSpec {
            name: if self.claude { "WebFetch" } else { "web_fetch" }.into(),
            description: "Fetches a URL and returns its content as Markdown (HTML is converted; other text is returned as is). \
                          Long pages are shortened."
                .into(),
            parameters: json!({"type": "object", "properties": props, "required": required, "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let Some(url) = input["url"].as_str().filter(|u| u.starts_with("http://") || u.starts_with("https://")) else {
            return ToolOutput::err("url must start with http:// or https://");
        };
        tokio::select! {
            _ = ctx.cancel.cancelled() => ToolOutput::err("interrupted"),
            r = fetch(url) => match r {
                Ok(text) => ToolOutput::limited(text, false),
                Err(e) => ToolOutput::err(format!("fetch failed: {e}")),
            },
        }
    }
}

async fn fetch(url: &str) -> Result<String, String> {
    c6_llm::install_crypto_provider();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("carbon6/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = http.get(url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let final_url = resp.url().to_string();
    let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    let mut body = resp.bytes().await.map_err(|e| e.to_string())?.to_vec();
    body.truncate(MAX_BODY);
    if !(ctype.is_empty() || ctype.contains("text") || ctype.contains("json") || ctype.contains("xml") || ctype.contains("javascript")) {
        return Err(format!("{status}: unsupported content type {ctype}"));
    }
    let text = String::from_utf8_lossy(&body).into_owned();
    let content = if ctype.contains("html") { htmd::convert(&text).unwrap_or(text) } else { text };
    let mut out = format!("URL: {final_url}\nStatus: {status}\n\n{content}");
    if !status.is_success() {
        out.insert_str(0, "The server returned an error status.\n");
    }
    Ok(out)
}
