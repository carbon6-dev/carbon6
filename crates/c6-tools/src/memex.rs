//! The memex tools: search your knowledge store, read an item or a thing in
//! full, and save a note. They call the memex through the head, which
//! applies the session's scope.

use async_trait::async_trait;
use c6_llm::ToolSpec;
use serde_json::{Value, json};

use crate::{Concurrency, Tool, ToolCtx, ToolOutput};

pub struct MemexSearch {
    claude: bool,
}

pub struct MemexGet {
    claude: bool,
}

pub struct MemexNote {
    claude: bool,
}

impl MemexSearch {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

impl MemexGet {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

impl MemexNote {
    pub fn new(claude: bool) -> Self {
        Self { claude }
    }
}

fn date(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

async fn call(ctx: &ToolCtx, method: &str, params: Value) -> Result<Value, String> {
    let head = ctx.session.head().ok_or("the memex is not available here")?;
    head.call(method, params).await
}

#[async_trait]
impl Tool for MemexSearch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "MemexSearch" } else { "memex_search" }.into(),
            description: "Searches the user's memex (their knowledge store: past carbon6 sessions, outputs, web pages read, notes, \
                          and whatever else this session may see) by keywords and by meaning. Use it to recall earlier work, \
                          decisions and findings before redoing them. Results carry ids for memex_get."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "query": {"type": "string"},
                "kinds": {"type": "array", "items": {"type": "string"}, "description": "Only these kinds: session, output, web, note, document, paper, email, photo"},
                "about": {"type": "array", "items": {"type": "string"}, "description": "Only items about all of these labels or thing ids, e.g. host:web1, client:acme"},
                "limit": {"type": "integer", "description": "Up to 50; default 10"}
            }, "required": ["query"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        match call(ctx, "memex.search", input).await {
            Ok(v) => {
                let hits = v.as_array().cloned().unwrap_or_default();
                if hits.is_empty() {
                    return ToolOutput::ok("Nothing in the memex matches.");
                }
                let lines: Vec<String> = hits
                    .iter()
                    .map(|h| {
                        // A carbon6 session also by its own id.
                        let session = match (h["kind"].as_str(), h["source"].as_str()) {
                            (Some("session"), Some("carbon6")) => format!(", carbon6 session {}", h["external_id"].as_str().unwrap_or("")),
                            _ => String::new(),
                        };
                        format!(
                            "[{}] ({}, {}{session}) {}\n  {}",
                            h["id"].as_str().unwrap_or(""),
                            h["kind"].as_str().unwrap_or(""),
                            date(h["created_at"].as_i64().unwrap_or(0)),
                            h["title"].as_str().unwrap_or(""),
                            h["snippet"].as_str().unwrap_or("")
                        )
                    })
                    .collect();
                ToolOutput::limited(lines.join("\n"), false)
            }
            Err(e) => ToolOutput::err(e),
        }
    }
}

#[async_trait]
impl Tool for MemexGet {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "MemexGet" } else { "memex_get" }.into(),
            description: "Reads one thing from the memex in full: an item by its id (from memex_search or a brief), or a thing \
                          (a label like host:web1 or client:acme, or a thing id) with its notes, items and related things."
                .into(),
            parameters: json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let v = match call(ctx, "memex.get", input).await {
            Ok(v) => v,
            Err(e) => return ToolOutput::err(e),
        };
        if let Some(item) = v.get("item") {
            let about: Vec<&str> =
                item["about"].as_array().map(|a| a.iter().filter_map(|t| t["label"].as_str()).collect()).unwrap_or_default();
            let mut out = format!(
                "{} ({}, {}, from {})\n",
                item["title"].as_str().unwrap_or(""),
                item["kind"].as_str().unwrap_or(""),
                date(item["created_at"].as_i64().unwrap_or(0)),
                item["source"].as_str().unwrap_or("")
            );
            if let Some(u) = item["url"].as_str() {
                out.push_str(&format!("URL: {u}\n"));
            }
            if !about.is_empty() {
                out.push_str(&format!("About: {}\n", about.join(", ")));
            }
            out.push('\n');
            out.push_str(item["text"].as_str().unwrap_or(""));
            return ToolOutput::limited(out, false);
        }
        let t = &v["thing"];
        let mut out = format!(
            "{} ({}: {})\n",
            t["thing"]["name"].as_str().unwrap_or(""),
            t["thing"]["kind"].as_str().unwrap_or(""),
            t["thing"]["label"].as_str().unwrap_or("")
        );
        let list = |key: &str| -> Vec<String> {
            t[key]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|h| {
                            format!(
                                "- [{}] ({}) {}: {}",
                                h["id"].as_str().unwrap_or(""),
                                h["kind"].as_str().unwrap_or(""),
                                h["title"].as_str().unwrap_or(""),
                                h["snippet"].as_str().unwrap_or("")
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        for (title, key) in [("Notes", "notes"), ("Items", "items")] {
            let l = list(key);
            if !l.is_empty() {
                out.push_str(&format!("\n{title}:\n{}\n", l.join("\n")));
            }
        }
        let related: Vec<String> = t["related"]
            .as_array()
            .map(|a| a.iter().filter_map(|p| Some(format!("{} ({})", p[0]["label"].as_str()?, p[1]))).collect())
            .unwrap_or_default();
        if !related.is_empty() {
            out.push_str(&format!("\nRelated (shared items): {}\n", related.join(", ")));
        }
        ToolOutput::limited(out, false)
    }
}

#[async_trait]
impl Tool for MemexNote {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: if self.claude { "MemexNote" } else { "memex_note" }.into(),
            description: "Saves a note to the user's memex: an insight, a decision, or a finding worth keeping, linked to what it \
                          is about (labels like host:web1 or client:acme; default: this session's labels). Not for this task's \
                          progress."
                .into(),
            parameters: json!({"type": "object", "properties": {
                "text": {"type": "string"},
                "about": {"type": "array", "items": {"type": "string"}}
            }, "required": ["text"], "additionalProperties": false}),
        }
    }

    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }

    async fn call(&self, ctx: &ToolCtx, input: Value) -> ToolOutput {
        let params = json!({"text": input["text"], "labels": input["about"]});
        match call(ctx, "memex.note", params).await {
            Ok(v) => ToolOutput::ok(format!("Saved note {}.", v["id"].as_str().unwrap_or(""))),
            Err(e) => ToolOutput::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dates() {
        assert_eq!(super::date(0), "1970-01-01");
        assert_eq!(super::date(1_791_731_393_132), "2026-10-11");
    }
}
