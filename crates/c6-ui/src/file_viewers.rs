//! Shared, escaped read-only renderers for files, mentions and fenced blocks.

use std::fmt::Write;

use leptos::prelude::*;
use serde::Deserialize;
use serde_json::Value;

pub(crate) fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
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
    while let Some(c) = chars.next() {
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
            row.push(std::mem::take(&mut field));
            start = true;
            closed = false;
        } else if matches!(c, '\r' | '\n') {
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

pub(crate) fn json_lines(text: &str) -> Vec<(usize, Result<Value, String>)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| (i + 1, serde_json::from_str(l).map_err(|e| e.to_string())))
        .collect()
}

fn button(action: &str, label: &str) -> String {
    format!("<button type=\"button\" data-fv-action=\"{action}\">{label}</button>")
}

fn code(text: &str, language: &str, line: usize, limit: usize) -> String {
    let language: String = language.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(30).collect();
    let mut out = String::from("<div class=\"fv-code\" tabindex=\"0\">");
    let lines: Vec<_> = text.lines().collect();
    let start = if line > 0 { line.saturating_sub(4).min(lines.len()) } else { 0 };
    // Full viewers include the target even when it falls beyond the initial cap.
    let end = limit.max(line.saturating_add(4)).min(lines.len());
    for (i, l) in lines.iter().enumerate().take(end).skip(if limit <= 20 { start } else { 0 }) {
        let selected = if i + 1 == line { " fv-selected" } else { "" };
        let _ = write!(
            out,
            "<div class=\"fv-line{selected}\" data-line=\"{}\"><span class=\"fv-line-number\">{}</span><code class=\"language-{language}\">{}</code></div>",
            i + 1,
            i + 1,
            esc(l)
        );
    }
    out.push_str("</div>");
    if end < lines.len() {
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
    for (i, cell) in header.iter().enumerate() {
        let _ = write!(out, "<th><button type=\"button\" data-fv-sort=\"{i}\">{} ↕</button></th>", esc(cell));
    }
    out.push_str("</tr></thead><tbody>");
    for row in body.iter().take(limit) {
        out.push_str("<tr>");
        for cell in row.iter() {
            let _ = write!(out, "<td>{}</td>", esc(cell));
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table></div>");
    if body.len() > limit {
        out.push_str(&button("more", "Load more rows"));
    }
    out
}

fn tree(value: &Value, path: &str, label: &str, budget: &mut usize, depth: usize) -> String {
    if *budget == 0 || depth >= 64 {
        return "<span>… node limit; use Raw or Download</span>".into();
    }
    *budget -= 1;
    let copy = format!("<button type=\"button\" data-fv-copy=\"{}\" title=\"Copy JSON Pointer\">Copy path</button>", esc(path));
    let label = esc(label);
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
        _ => format!("<div class=\"fv-node\">{label}: <code>{}</code> {copy}</div>", esc(&value.to_string())),
    }
}

fn json(input: &Input, lines: bool) -> String {
    let limit = if input.limit > 0 { input.limit } else { 1000 };
    let mut budget = limit;
    let mut out = button("source", "Pretty / Raw");
    out.push_str("<div class=\"fv-rendered fv-tree\">");
    if lines {
        let records = json_lines(&input.text);
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
    out.push_str(&code(&input.text, "json", 0, limit));
    out.push_str("</div>");
    out
}

pub(crate) fn render(input: &Input) -> String {
    use crate::file_formats::{Renderer, renderer};
    let kind = renderer(&input.path, &input.mime, &input.language);
    let name = input.path.rsplit('/').next().unwrap_or(&input.path);
    let compact = input.mode == "inline";
    let limit = if input.limit > 0 {
        input.limit
    } else if compact {
        20
    } else {
        2000
    };
    let body = match kind {
        Renderer::Markdown => {
            if compact {
                let preview = input.text.lines().take(6).collect::<Vec<_>>().join("\n");
                format!(
                    "<div class=\"fv-markdown-card\"><strong>{}</strong><pre>{}</pre></div>",
                    esc(input.text.lines().find(|l| l.starts_with('#')).unwrap_or(name).trim_start_matches('#').trim()),
                    esc(&preview)
                )
            } else {
                // File content must not load remote images, even through Markdown.
                let html = crate::markdown::to_html(&input.text);
                format!(
                    "{}<div class=\"fv-rendered md\">{}</div><div class=\"fv-source-view\" hidden>{}</div>",
                    button("source", "Source / Rendered"),
                    without_images(&html),
                    code(&input.text, "markdown", input.line, limit)
                )
            }
        }
        Renderer::Code | Renderer::Text => {
            code(&input.text, if input.language.is_empty() { extension_language(&input.path) } else { &input.language }, input.line, limit)
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
            if compact { String::new() } else { "<iframe title=\"PDF preview\" sandbox=\"allow-same-origin\"></iframe>".into() }
        ),
        Renderer::Csv => table(input, ','),
        Renderer::Tsv => table(input, '\t'),
        Renderer::Json => json(input, false),
        Renderer::JsonLines => json(input, true),
        _ => format!("<p>{} · {}</p>", esc(name), esc(if input.mime.is_empty() { "File" } else { &input.mime })),
    };
    let kind = format!("{kind:?}").to_lowercase();
    let open = if input.url.is_empty() { String::new() } else { format!("<a href=\"{}\" download>Download</a>", esc(&input.url)) };
    format!(
        "<section class=\"fv\" data-kind=\"{kind}\" data-path=\"{}\" data-url=\"{}\" data-mode=\"{}\" data-language=\"{}\" data-line=\"{}\"><pre class=\"fv-source\" hidden>{}</pre>{body}<div class=\"fv-controls\">{open}{}</div></section>",
        esc(&input.path),
        esc(&input.url),
        esc(&input.mode),
        esc(&input.language),
        input.line,
        esc(&input.text),
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
    if language == "json-code" || !matches!(language, "csv" | "tsv" | "json" | "jsonl" | "ndjson" | "diff" | "patch" | "log" | "mermaid") {
        return format!(
            "<section class=\"fv\" data-mode=\"inline\">{}<pre class=\"fv-source\" hidden>{}</pre></section>",
            code(text, if language == "json-code" { "json" } else { language }, 0, 2000),
            esc(text)
        );
    }
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
        let lines = json_lines("{\"a\":1}\n\nnope\n[2]\n");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].0, 3);
        assert!(lines[0].1.is_ok());
        assert!(lines[1].1.is_err());
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
