//! web_fetch: fetch a URL and return it as Markdown. web_search: search the
//! web through a configured provider (for models whose provider has no
//! search of its own).

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

/// A web search provider (the head's "web search" setting).
#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
pub struct SearchConfig {
    /// "brave", "tavily" or "searxng"; empty: none.
    #[serde(default)]
    pub provider: String,
    /// SearXNG: the instance's base URL. (Brave and Tavily: optional
    /// override, for tests.)
    #[serde(default)]
    pub url: String,
    /// The secret holding the API key (Brave, Tavily).
    #[serde(default)]
    pub secret: String,
    /// Offer it to every model, not just those whose provider has no web
    /// search of its own.
    #[serde(default)]
    pub always: bool,
}

pub struct WebSearch {
    claude: bool,
    config: SearchConfig,
}

impl WebSearch {
    pub fn new(claude: bool, config: SearchConfig) -> Self {
        Self { claude, config }
    }
}

#[async_trait]
impl Tool for WebSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "WebSearch" } else { "web_search" }.into(),
            description: "Searches the web and returns the top results (title, URL, snippet). Use it for current information, \
                          documentation and research; then read promising pages with the fetch tool, and cite their URLs."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "query": {"type": "string", "description": "The search query"},
                "count": {"type": "integer", "minimum": 1, "maximum": 20, "description": "How many results (default 8)"}
            }, "required": ["query"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let Some(query) = input["query"].as_str().map(str::trim).filter(|q| !q.is_empty()) else {
            return ToolOutput::err("query is required");
        };
        let count = input["count"].as_u64().unwrap_or(8).clamp(1, 20) as usize;
        let key = ctx.session.secrets().get(&self.config.secret);
        if matches!(self.config.provider.as_str(), "brave" | "tavily") && key.is_none() {
            return ToolOutput::err(format!(
                "web search is not available: the secret {:?} with its API key does not apply to this session",
                self.config.secret
            ));
        }
        tokio::select! {
            _ = ctx.cancel.cancelled() => ToolOutput::err("interrupted"),
            r = search(&self.config, key.as_deref().unwrap_or(""), query, count) => match r {
                Ok(results) if results.is_empty() => ToolOutput::ok("No results."),
                Ok(results) => {
                    let text = results
                        .iter()
                        .enumerate()
                        .map(|(i, (title, url, snippet))| format!("{}. {title}\n   {url}\n   {}", i + 1, snippet.replace('\n', " ")))
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    ToolOutput::limited(ctx.session.secrets().redact(&text), false)
                }
                Err(e) => ToolOutput::err(format!("web search failed: {}", ctx.session.secrets().redact(&e))),
            },
        }
    }
}

/// (title, url, snippet) for each result.
async fn search(cfg: &SearchConfig, key: &str, query: &str, count: usize) -> Result<Vec<(String, String, String)>, String> {
    c6_llm::install_crypto_provider();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("carbon6/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?;
    let base = |default: &str| if cfg.url.trim().is_empty() { default.to_owned() } else { cfg.url.trim().trim_end_matches('/').to_owned() };
    let (resp, list, snippet) = match cfg.provider.as_str() {
        "brave" => {
            let url = reqwest::Url::parse_with_params(
                &format!("{}/res/v1/web/search", base("https://api.search.brave.com")),
                &[("q", query), ("count", &count.to_string())],
            )
            .map_err(|e| e.to_string())?;
            (
                http.get(url).header("X-Subscription-Token", key).header("Accept", "application/json").send().await,
                "/web/results",
                "description",
            )
        }
        "tavily" => {
            let body = json!({"api_key": key, "query": query, "max_results": count}).to_string();
            let url = format!("{}/search", base("https://api.tavily.com"));
            (http.post(url).header("Content-Type", "application/json").body(body).send().await, "/results", "content")
        }
        "searxng" => {
            if cfg.url.trim().is_empty() {
                return Err("SearXNG needs the instance's URL".into());
            }
            let url = reqwest::Url::parse_with_params(&format!("{}/search", base("")), &[("q", query), ("format", "json")])
                .map_err(|e| e.to_string())?;
            (http.get(url).send().await, "/results", "content")
        }
        other => return Err(format!("unknown provider {other:?}")),
    };
    let resp = resp.map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.bytes().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("{status}: {}", String::from_utf8_lossy(&body).chars().take(300).collect::<String>()));
    }
    let v: Value = serde_json::from_slice(&body).map_err(|e| format!("unexpected response: {e}"))?;
    Ok(v.pointer(list)
        .and_then(|r| r.as_array())
        .into_iter()
        .flatten()
        .take(count)
        .map(|r| {
            let s = |k: &str| r[k].as_str().unwrap_or("").to_owned();
            (s("title"), s("url"), s(snippet))
        })
        .filter(|(_, url, _)| !url.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::SessionTools;

    /// A one-shot HTTP server answering every request with `body`, and the
    /// requests it saw.
    async fn serve(body: &'static str) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let mut buf = vec![0u8; 16384];
                let n = s.read(&mut buf).await.unwrap_or(0);
                log.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes()).await;
            }
        });
        (url, seen)
    }

    #[tokio::test]
    async fn searches_through_providers() {
        let session = Arc::new(SessionTools::new(std::env::temp_dir()));
        session.set_secrets(vec![("BRAVE_KEY".into(), "brave-key-123".into())]);
        let ctx = ToolCtx { session: session.clone(), cancel: Default::default(), output: Arc::new(|_: &str| {}), call_id: "c".into() };

        let (url, seen) = serve(r#"{"web":{"results":[{"title":"Rust","url":"https://rust-lang.org","description":"A language"}]}}"#).await;
        let brave = WebSearch::new(false, SearchConfig { provider: "brave".into(), url, secret: "BRAVE_KEY".into(), always: false });
        let out = brave.call(&ctx, json!({"query": "rust lang", "count": 3})).await;
        assert_eq!(out.content, "1. Rust\n   https://rust-lang.org\n   A language", "{out:?}");
        let req = seen.lock().unwrap()[0].to_lowercase();
        assert!(
            req.starts_with("get /res/v1/web/search?q=rust+lang&count=3") && req.contains("x-subscription-token: brave-key-123"),
            "{req}"
        );

        let (url, _) = serve(r#"{"results":[{"title":"A","url":"https://a.example","content":"aaa"},{"title":"B","url":"https://b.example","content":"bbb"}]}"#).await;
        let searx = WebSearch::new(false, SearchConfig { provider: "searxng".into(), url, ..Default::default() });
        let out = searx.call(&ctx, json!({"query": "x", "count": 1})).await;
        assert_eq!(out.content, "1. A\n   https://a.example\n   aaa");

        // Without its key (the secret does not apply here) it says so.
        let tavily = WebSearch::new(false, SearchConfig { provider: "tavily".into(), secret: "TAVILY_KEY".into(), ..Default::default() });
        let out = tavily.call(&ctx, json!({"query": "x"})).await;
        assert!(out.is_error && out.content.contains("TAVILY_KEY"), "{out:?}");
    }
}
