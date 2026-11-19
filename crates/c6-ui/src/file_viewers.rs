//! Shared, escaped read-only renderers for files, mentions and fenced blocks.

use std::fmt::Write;

use leptos::prelude::*;
use serde::Deserialize;
use serde_json::Value;

pub(crate) fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

pub(crate) fn clipped(s: &str) -> String {
    if s.len() <= 8192 {
        s.to_owned()
    } else {
        let mut end = 8192;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}… [display capped; Download for full text]", &s[..end])
    }
}

fn prefix(s: &str, cap: usize) -> &str {
    let mut end = s.len().min(cap);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Input {
    pub path: String,
    pub text: String,
    pub url: String,
    pub mode: String,
    pub line: usize,
    pub mime: String,
    pub language: String,
    pub limit: usize,
    pub filter: String,
    pub sort: Option<usize>,
    pub descending: bool,
}

/// Location syntax belongs to the link, never the worker filename.
pub(crate) fn location(token: &str) -> (&str, usize, usize) {
    let mut path = token;
    let mut nums = Vec::new();
    for _ in 0..2 {
        if let Some((p, n)) = path.rsplit_once(':')
            && !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
            && let Ok(n) = n.parse::<usize>()
            && n > 0
        {
            path = p;
            nums.push(n);
        } else {
            break;
        }
    }
    match nums.as_slice() {
        [line] => (path, *line, 0),
        [col, line] => (path, *line, *col),
        _ => (token, 0, 0),
    }
}

/// RFC 4180 quoting also applies to TSV; records may contain newlines.
pub(crate) fn delimited(text: &str, delimiter: char) -> Result<Vec<Vec<String>>, String> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    let (mut quoted, mut closed, mut start) = (false, false, true);
    let mut cells = 0usize;
    while let Some(c) = chars.next() {
        if cells > 1_000_000 {
            return Err("Table exceeds one million cells; use Download".into());
        }
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                    closed = true;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' && start {
            quoted = true;
            start = false;
        } else if c == delimiter {
            cells += 1;
            row.push(std::mem::take(&mut field));
            start = true;
            closed = false;
        } else if matches!(c, '\r' | '\n') {
            cells += 1;
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            row.push(std::mem::take(&mut field));
            rows.push(std::mem::take(&mut row));
            start = true;
            closed = false;
        } else if closed {
            return Err("Unexpected character after a quoted field".into());
        } else {
            field.push(c);
            start = false;
        }
    }
    if quoted {
        return Err("Unclosed quoted field".into());
    }
    if !start || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}

pub(crate) fn pointer(parent: &str, key: &str) -> String {
    format!("{parent}/{}", key.replace('~', "~0").replace('/', "~1"))
}

pub(crate) fn json_lines(text: &str) -> impl Iterator<Item = (usize, Result<Value, String>)> + '_ {
    text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()).map(|(i, l)| (i + 1, serde_json::from_str(l).map_err(|e| e.to_string())))
}

fn button(action: &str, label: &str) -> String {
    format!("<button type=\"button\" data-fv-action=\"{action}\">{label}</button>")
}

fn code(text: &str, language: &str, line: usize, limit: usize) -> String {
    let language: String = language.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(30).collect();
    let mut out = String::from("<div class=\"fv-code\" tabindex=\"0\">");
    let total = text.lines().count();
    let start = if line > 0 { line.saturating_sub(if limit <= 20 { 4 } else { limit / 2 }).min(total) } else { 0 };
    let end = start.saturating_add(limit).min(total);
    if start > 0 {
        out.push_str("<p>Earlier lines omitted; Download for the complete source.</p>");
    }
    let mut bytes = 0usize;
    for (i, l) in text.lines().enumerate().take(end).skip(start) {
        bytes += l.len().min(8192);
        if bytes > 2 * 1024 * 1024 {
            out.push_str("<p>Display byte limit reached; use Download.</p>");
            break;
        }
        let selected = if i + 1 == line { " fv-selected" } else { "" };
        let _ = write!(
            out,
            "<div class=\"fv-line{selected}\" data-line=\"{}\"><span class=\"fv-line-number\">{}</span><code class=\"language-{language}\">{}</code></div>",
            i + 1,
            i + 1,
            esc(&clipped(l))
        );
    }
    out.push_str("</div>");
    if end < total {
        out.push_str(&button("more", "Load more lines"));
    }
    out
}

fn table(input: &Input, delimiter: char) -> String {
    let rows = match delimited(&input.text, delimiter) {
        Ok(r) => r,
        Err(e) => return format!("<p role=\"alert\">{}</p>{}", esc(&e), code(&input.text, "text", 0, 100)),
    };
    let Some(header) = rows.first() else {
        return "<p>Empty table</p>".into();
    };
    let filter = input.filter.to_lowercase();
    let mut body: Vec<_> =
        rows.iter().skip(1).filter(|r| filter.is_empty() || r.iter().any(|c| c.to_lowercase().contains(&filter))).collect();
    if let Some(col) = input.sort {
        body.sort_by(|a, b| {
            let (a, b) = (a.get(col).map(String::as_str).unwrap_or(""), b.get(col).map(String::as_str).unwrap_or(""));
            let cmp = match (a.parse::<f64>(), b.parse::<f64>()) {
                (Ok(a), Ok(b)) => a.total_cmp(&b),
                (Ok(_), Err(_)) => std::cmp::Ordering::Less,
                (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
                _ => a.cmp(b),
            };
            if input.descending { cmp.reverse() } else { cmp }
        });
    }
    let limit = if input.limit > 0 {
        input.limit
    } else if input.mode == "inline" {
        20
    } else {
        10_000
    };
    let mut out = format!(
        "<label>Filter <input data-fv-filter type=\"search\" value=\"{}\"></label><p>{} rows</p><div class=\"fv-table-scroll\" tabindex=\"0\"><table><thead><tr>",
        esc(&input.filter),
        body.len()
    );
    for (i, cell) in header.iter().enumerate().take(256) {
        let _ = write!(out, "<th><button type=\"button\" data-fv-sort=\"{i}\" title=\"Sort column\">{}</button></th>", esc(&clipped(cell)));
    }
    out.push_str("</tr></thead><tbody>");
    let (mut cells, mut bytes) = (0usize, 0usize);
    for row in body.iter().take(limit) {
        if cells >= 100_000 || bytes >= 2 * 1024 * 1024 {
            break;
        }
        out.push_str("<tr>");
        for cell in row.iter().take(256) {
            cells += 1;
            bytes += cell.len().min(8192);
            let _ = write!(out, "<td>{}</td>", esc(&clipped(cell)));
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table></div>");
    if header.len() > 256 || cells >= 100_000 || bytes >= 2 * 1024 * 1024 {
        out.push_str("<p>Table display capped at 256 columns, 100,000 cells or 2 MiB; use Download for the rest.</p>");
    }
    if body.len() > limit {
        out.push_str(&button("more", "Load more rows"));
    }
    out
}

fn tree(value: &Value, path: &str, label: &str, budget: &mut usize, depth: usize) -> String {
    if *budget == 0 || depth >= 64 || path.len() > 8192 {
        return "<span>… node limit; use Raw or Download</span>".into();
    }
    *budget -= 1;
    let copy = format!(
        "<button type=\"button\" data-fv-copy-json=\"{}\" title=\"Copy JSON Pointer\">Copy path</button>",
        esc(&serde_json::to_string(path).unwrap_or_default())
    );
    let label = esc(&clipped(label));
    match value {
        Value::Object(map) => {
            let mut out =
                format!("<details{}><summary>{label} {{ {} keys }} {copy}</summary>", if depth == 0 { " open" } else { "" }, map.len());
            for (key, val) in map.iter().take(*budget) {
                out.push_str(&tree(val, &pointer(path, key), key, budget, depth + 1));
            }
            out.push_str("</details>");
            out
        }
        Value::Array(array) => {
            let mut out =
                format!("<details{}><summary>{label} [ {} items ] {copy}</summary>", if depth == 0 { " open" } else { "" }, array.len());
            for (i, val) in array.iter().enumerate().take(*budget) {
                out.push_str(&tree(val, &pointer(path, &i.to_string()), &i.to_string(), budget, depth + 1));
            }
            out.push_str("</details>");
            out
        }
        _ => format!("<div class=\"fv-node\">{label}: <code>{}</code> {copy}</div>", esc(&clipped(&value.to_string()))),
    }
}

fn json(input: &Input, lines: bool) -> String {
    let limit = if input.limit > 0 { input.limit } else { 1000 };
    let mut budget = limit;
    let mut out = button("source", "Pretty / Raw");
    out.push_str("<div class=\"fv-rendered fv-tree\">");
    if lines {
        let records = json_lines(&input.text).take(limit.saturating_add(1)).collect::<Vec<_>>();
        for (i, value) in records.iter().take(limit) {
            let body = match value {
                Ok(v) => tree(v, "", "$", &mut budget, 0),
                Err(e) => format!("<p role=\"alert\">{}</p>", esc(e)),
            };
            let _ = write!(out, "<details><summary>Line {i}</summary>{body}</details>");
        }
        if records.len() > limit {
            out.push_str(&button("more", "Load more records"));
        }
    } else {
        match serde_json::from_str::<Value>(&input.text) {
            Ok(v) => out.push_str(&tree(&v, "", "$", &mut budget, 0)),
            Err(e) => {
                let _ = write!(out, "<p role=\"alert\">{}</p>", esc(&e.to_string()));
            }
        }
    }
    if budget == 0 {
        out.push_str(&button("more", "Load more nodes"));
    }
    out.push_str("</div><div class=\"fv-source-view\" hidden>");
    out.push_str(&code(&input.text, "json", input.line, limit));
    out.push_str("</div>");
    out
}

fn structured_fence(language: &str) -> bool {
    matches!(language, "csv" | "tsv" | "json" | "jsonl" | "ndjson" | "diff" | "patch" | "log" | "mermaid")
}

pub(crate) fn render(input: &Input) -> String {
    use crate::file_formats::{Renderer, renderer};
    let compact = input.mode == "inline";
    // A pathless, explicitly tagged inline fence may name a grammar (e.g.
    // Markdown), but that must not turn numbered code into a file preview.
    let code_fence = compact
        && input.path.is_empty()
        && input.url.is_empty()
        && input.mime.is_empty()
        && !input.language.is_empty()
        && !structured_fence(&input.language);
    let kind = if code_fence { Renderer::Code } else { renderer(&input.path, &input.mime, &input.language) };
    let name = input.path.rsplit('/').next().unwrap_or(&input.path);
    let limit = if input.limit > 0 {
        input.limit
    } else if compact && !code_fence {
        20
    } else {
        2000
    };
    let body = match kind {
        Renderer::Markdown => {
            if compact {
                let preview = input.text.lines().take(6).map(clipped).collect::<Vec<_>>().join("\n");
                format!(
                    "<div class=\"fv-markdown-card\"><strong>{}</strong><pre>{}</pre></div>",
                    esc(&clipped(input.text.lines().find(|l| l.starts_with('#')).unwrap_or(name).trim_start_matches('#').trim())),
                    esc(&preview)
                )
            } else {
                // File content must not load remote images, even through Markdown.
                let text = prefix(&input.text, 2 * 1024 * 1024).lines().take(limit).map(clipped).collect::<Vec<_>>().join("\n");
                let html = crate::markdown::to_html(&text);
                let more = if text.len() < input.text.trim_end().len() {
                    button("more", "Load more lines (2 MiB display cap)")
                } else {
                    String::new()
                };
                format!(
                    "{}<div class=\"fv-rendered md\">{}{more}</div><div class=\"fv-source-view\" hidden>{}</div>",
                    button("source", "Source / Rendered"),
                    without_images(&html),
                    code(&input.text, "markdown", input.line, limit)
                )
            }
        }
        Renderer::Code | Renderer::Text => {
            let language = match input.language.as_str() {
                "" => extension_language(&input.path),
                "json-code" => "json",
                language => language,
            };
            code(&input.text, language, input.line, limit)
        }
        Renderer::Image => format!(
            "<div class=\"fv-controls\">{}{}{}<span class=\"fv-dimensions\"></span></div><div class=\"fv-image\"><img alt=\"{}\"></div>",
            button("zoom-in", "Zoom in"),
            button("zoom-out", "Zoom out"),
            button("fit", "Fit"),
            esc(name)
        ),
        Renderer::Pdf => format!(
            "<p>PDF · {}</p><div class=\"fv-pdf\">{}</div>",
            esc(name),
            if compact { String::new() } else { "<p>Open PDF if this browser cannot embed it.</p>".into() }
        ),
        Renderer::Csv => table(input, ','),
        Renderer::Tsv => table(input, '\t'),
        Renderer::Json => json(input, false),
        Renderer::JsonLines => json(input, true),
        Renderer::Diff => crate::file_viewers_extra::diff(input),
        Renderer::Log => crate::file_viewers_extra::log(input),
        Renderer::Mermaid => crate::file_viewers_extra::mermaid(input),
        _ => format!("<p>{} · {}</p>", esc(name), esc(if input.mime.is_empty() { "File" } else { &input.mime })),
    };
    let kind = format!("{kind:?}").to_lowercase();
    let cap_note = if input.text.len() > 2 * 1024 * 1024 {
        "<p class=\"fv-cap-note\">Source preview capped at 2 MiB; Download has the complete file.</p>"
    } else {
        ""
    };
    let open = if input.url.is_empty() { String::new() } else { format!("<a href=\"{}\" download>Download</a>", esc(&input.url)) };
    format!(
        "<section class=\"fv\" data-kind=\"{kind}\" data-path=\"{}\" data-url=\"{}\" data-mode=\"{}\" data-language=\"{}\" data-mime=\"{}\" data-line=\"{}\" data-source-truncated=\"{}\"><pre class=\"fv-source\" hidden>{}</pre>{cap_note}{body}<div class=\"fv-controls\">{open}{}</div></section>",
        esc(&input.path),
        esc(&input.url),
        esc(&input.mode),
        esc(&input.language),
        esc(&input.mime),
        input.line,
        input.text.len() > 2 * 1024 * 1024,
        esc(prefix(&input.text, 2 * 1024 * 1024)),
        if input.path.is_empty() {
            String::new()
        } else {
            format!("<button type=\"button\" data-fv-copy=\"{}\">Copy path</button>", esc(&input.path))
        }
    )
}

fn extension_language(path: &str) -> &str {
    match path.rsplit('.').next().unwrap_or("") {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "jsx" => "javascript",
        "ts" | "tsx" => "typescript",
        "sh" => "bash",
        "yml" => "yaml",
        "h" | "cc" | "hpp" => "cpp",
        ext => ext,
    }
}

fn without_images(html: &str) -> String {
    let mut out = String::new();
    let mut rest = html;
    while let Some(i) = rest.find("<img ") {
        out.push_str(&rest[..i]);
        let Some(end) = rest[i..].find('>') else {
            break;
        };
        out.push_str("[image omitted in file preview]");
        rest = &rest[i + end + 1..];
    }
    out.push_str(rest);
    out
}

pub(crate) fn fence(text: &str, language: &str) -> String {
    let language = if language.is_empty() { "text" } else { language };
    let html = render(&Input { text: text.into(), language: language.into(), mode: "inline".into(), ..Default::default() });
    if language == "json" && text.lines().count() > 30 {
        format!("<details class=\"fv-collapsed\"><summary>JSON · Expand</summary>{html}</details>")
    } else {
        html
    }
}

/// The same content contract is available to attachments without an upload API.
#[component]
pub(crate) fn FileContent(path: String, url: String, #[prop(optional)] line: usize, #[prop(optional)] mime: String) -> impl IntoView {
    let binary = matches!(
        crate::file_formats::renderer(&path, &mime, ""),
        crate::file_formats::Renderer::Image | crate::file_formats::Renderer::Pdf | crate::file_formats::Renderer::Other
    );
    let input = Input { path, url: url.clone(), line, mime, mode: "full".into(), ..Default::default() };
    let file = crate::resources::local(move || {
        let (url, mut input) = (url.clone(), input.clone());
        async move {
            if !binary {
                #[cfg(target_arch = "wasm32")]
                {
                    #[wasm_bindgen::prelude::wasm_bindgen]
                    extern "C" {
                        #[wasm_bindgen(catch, js_namespace = c6Viewers, js_name = text)]
                        async fn text(url: &str) -> Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>;
                    }
                    input.text = text(&url)
                        .await
                        .map_err(|e| e.as_string().unwrap_or_else(|| "Download failed".into()))?
                        .as_string()
                        .ok_or_else(|| "Not text".to_owned())?;
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let response = gloo_net::http::Request::get(&url).send().await.map_err(|e| e.to_string())?;
                    if !response.ok() {
                        return Err(format!("Could not load file: HTTP {}", response.status()));
                    }
                    let bytes = response.binary().await.map_err(|e| e.to_string())?;
                    if bytes.len() > 40 * 1024 * 1024 {
                        return Err("File exceeds the 40 MiB viewer limit".into());
                    }
                    input.text = String::from_utf8(bytes).map_err(|_| "Not UTF-8 text; use Download".to_owned())?;
                }
            }
            Ok(render(&input))
        }
    });
    view! { <Suspense fallback=|| view! { <p>"Loading preview…"</p> }>
        {move || file.get().map(|r: Result<String, String>| match r {
            Ok(html) => view! { <div class="fv-host" inner_html=html></div> }.into_any(),
            Err(e) => view! { <crate::util::ErrorBox message=e /> }.into_any(),
        })}
    </Suspense> }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn init() {
    use wasm_bindgen::{JsCast, prelude::*};
    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_namespace = c6Viewers, js_name = init)]
        fn register(render: &js_sys::Function);
    }
    let callback = Closure::<dyn Fn(String) -> String>::new(|json: String| match serde_json::from_str::<Input>(&json) {
        Ok(input) => render(&input),
        Err(e) => format!("<p>{}</p>", esc(&e.to_string())),
    });
    register(callback.as_ref().unchecked_ref());
    callback.forget();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_locations_keep_columns_out_of_filenames() {
        assert_eq!(location("src/main.rs:42:3"), ("src/main.rs", 42, 3));
        assert_eq!(location("/tmp/a.md:1"), ("/tmp/a.md", 1, 0));
        assert_eq!(location("a.rs:0"), ("a.rs:0", 0, 0));
    }

    #[test]
    fn csv_quotes_newlines_empty_fields_and_tsv() {
        assert_eq!(
            delimited("a,b\r\n\"one,two\",\"line\n\"\"two\"\"\"\r\n,,", ',').unwrap(),
            vec![vec!["a", "b"], vec!["one,two", "line\n\"two\""], vec!["", "", ""]]
        );
        assert_eq!(delimited("a\tb\n1\t\"two\tthree\"\n", '\t').unwrap(), vec![vec!["a", "b"], vec!["1", "two\tthree"]]);
        assert!(delimited("\"unterminated", ',').is_err());
        assert!(delimited("\"a\"bad,b", ',').is_err());
    }

    #[test]
    fn json_pointer_escapes_and_jsonl_reports_actual_line_numbers() {
        assert_eq!(pointer("/items/0", "a~/b"), "/items/0/a~0~1b");
        let lines = json_lines("{\"a\":1}\n\nnope\n[2]\n").collect::<Vec<_>>();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].0, 3);
        assert!(lines[0].1.is_ok());
        assert!(lines[1].1.is_err());
        let html = render(&Input { language: "json".into(), text: "{\"a\\r\\u0000\":1}".into(), ..Default::default() });
        assert!(html.contains("data-fv-copy-json=\"&quot;/a\\r\\u0000&quot;\""));
    }

    #[test]
    fn distant_source_locations_render_a_window_not_the_entire_prefix() {
        let text = "line\n".repeat(100_000);
        let html = code(&text, "rust", 90_000, 2000);
        assert_eq!(html.matches("data-line=").count(), 2000);
        assert!(html.contains("fv-selected\" data-line=\"90000\""));
        let html = table(&Input { text: "n\n2\n10\n1x\n".into(), sort: Some(0), ..Default::default() }, ',');
        assert!(html.find("<td>2</td>").unwrap() < html.find("<td>10</td>").unwrap());
        assert!(html.find("<td>10</td>").unwrap() < html.find("<td>1x</td>").unwrap());
    }

    #[test]
    fn generic_fences_keep_their_format_and_limit_on_redraw() {
        let text = "{}\n".repeat(2500);
        for (tag, metadata, grammar) in
            [("", "text", "text"), ("rust", "rust", "rust"), ("json-code", "json-code", "json"), ("pdf", "pdf", "pdf")]
        {
            let initial = fence(&text, tag);
            assert!(initial.contains("data-kind=\"code\""));
            assert!(initial.contains(&format!("data-language=\"{metadata}\"")));
            assert!(initial.contains(&format!("class=\"language-{grammar}\"")));
            assert_eq!(initial.matches("data-line=").count(), 2001); // rows plus the viewer root
            assert!(initial.contains("Load more lines"));

            let expanded =
                render(&Input { language: metadata.into(), text: text.clone(), mode: "inline".into(), limit: 4000, ..Default::default() });
            assert!(expanded.contains("data-kind=\"code\""));
            assert!(expanded.contains(&format!("class=\"language-{grammar}\"")));
            assert_eq!(expanded.matches("data-line=").count(), 2501);
            assert!(!expanded.contains("Load more lines"));
        }
        assert!(fence("{}", "json").contains("data-kind=\"json\""));
    }

    #[test]
    fn svg_is_only_an_image_not_source_markup() {
        let html = render(&Input {
            path: "/tmp/a.svg".into(),
            text: "<svg onload=\"alert(1)\"><script>x</script></svg>".into(),
            ..Default::default()
        });
        assert!(html.contains("<img alt="));
        assert!(!html.contains("<svg"));
        assert!(!html.contains("<script"));
        assert!(!without_images("<p>Hi<img src=\"https://x\" /></p>").contains("<img"));
    }
}
