//! Escaped HTML for diff, log, and Mermaid previews.

use std::fmt::Write;

use crate::file_formats::{AnsiSpan, AnsiStyle, DiffLine, DiffLineKind, LogSeverity, ansi_spans, log_severity, parse_unified_diff};
use crate::file_viewers::{Input, clipped, esc};

fn limit(input: &Input, compact: usize) -> usize {
    if input.limit > 0 {
        input.limit
    } else if input.mode == "inline" {
        compact
    } else {
        2000
    }
}

fn more(out: &mut String, label: &str) {
    let _ = write!(out, "<button type=\"button\" class=\"fv-more\" data-fv-action=\"more\">{label}</button>");
}

pub(crate) fn diff(input: &Input) -> String {
    let compact = input.mode == "inline";
    let more_label = if compact { "Expand" } else { "Load more lines" };
    let mut remaining = limit(input, 40);
    let mut out = String::new();
    let Some(files) = parse_unified_diff(&input.text) else {
        out.push_str("<div class=\"fv-code fv-diff-raw\" tabindex=\"0\" aria-label=\"Raw diff\"><pre><code>");
        let mut lines = input.text.split_inclusive('\n');
        for line in lines.by_ref().take(remaining) {
            out.push_str(&esc(&clipped(line)));
        }
        out.push_str("</code></pre></div>");
        if lines.next().is_some() {
            more(&mut out, more_label);
        }
        return out;
    };

    if !compact && files.iter().any(|file| !file.hunks.is_empty()) {
        out.push_str("<div class=\"fv-controls\"><button type=\"button\" data-fv-action=\"split\" aria-pressed=\"false\">Unified / Split</button></div>");
    }
    let mut truncated = false;
    let mut files = files.into_iter().peekable();
    while let Some(file) = files.next() {
        if remaining == 0 {
            truncated = true;
            break;
        }
        out.push_str("<details class=\"fv-diff-file\" open><summary>");
        match (file.old_path, file.new_path) {
            (Some(old), Some(new)) => {
                let _ = write!(out, "<code>{}</code> → <code>{}</code>", esc(old), esc(new));
            }
            _ => {
                let label = file.metadata.iter().find_map(|line| line.strip_prefix("diff --git ")).unwrap_or("Unified diff");
                let _ = write!(out, "<code>{}</code>", esc(label));
            }
        }
        out.push_str("</summary>");

        let metadata_len = file.metadata.len().min(remaining);
        if metadata_len > 0 {
            let _ = write!(out, "<pre class=\"fv-diff-metadata\">{}</pre>", esc(&file.metadata[..metadata_len].join("\n")));
            remaining -= metadata_len;
        }
        truncated |= metadata_len < file.metadata.len();

        if remaining > 0 && !file.hunks.is_empty() {
            out.push_str("<div class=\"fv-code fv-diff\" tabindex=\"0\" aria-label=\"Patch lines\">");
            let mut hunks = file.hunks.iter().peekable();
            while let Some(hunk) = hunks.next() {
                let shown = hunk.lines.len().min(remaining);
                let _ = write!(out, "<div class=\"fv-diff-hunk\"><code>{}</code></div>", esc(hunk.header));
                let lines = &hunk.lines[..shown];
                out.push_str("<div class=\"fv-diff-unified\">");
                for line in lines {
                    unified_line(&mut out, line);
                }
                out.push_str("</div>");
                if !compact {
                    out.push_str("<div class=\"fv-diff-split\" hidden><div class=\"fv-diff-pair fv-diff-columns\"><span>Old</span><span>New</span></div>");
                    split_lines(&mut out, lines);
                    out.push_str("</div>");
                }
                // Empty hunks still consume one slot for the header.
                remaining -= shown.max(1);
                truncated |= shown < hunk.lines.len();
                if remaining == 0 {
                    truncated |= hunks.peek().is_some();
                    break;
                }
            }
            out.push_str("</div>");
        } else {
            truncated |= !file.hunks.is_empty();
        }
        out.push_str("</details>");
        if remaining == 0 {
            truncated |= files.peek().is_some();
            break;
        }
    }
    if truncated {
        more(&mut out, more_label);
    }
    out
}

fn diff_class(kind: DiffLineKind) -> &'static str {
    match kind {
        DiffLineKind::Context => "fv-diff-context",
        DiffLineKind::Removed => "fv-diff-removed",
        DiffLineKind::Added => "fv-diff-added",
        DiffLineKind::NoNewline => "fv-diff-note",
    }
}

fn line_number(out: &mut String, n: Option<u64>) {
    out.push_str("<span class=\"fv-diff-number\">");
    if let Some(n) = n {
        let _ = write!(out, "{n}");
    }
    out.push_str("</span>");
}

fn diff_text(out: &mut String, line: &DiffLine<'_>) {
    let prefix = match line.kind {
        DiffLineKind::Context => " ",
        DiffLineKind::Removed => "-",
        DiffLineKind::Added => "+",
        DiffLineKind::NoNewline => "",
    };
    let _ = write!(out, "<code><span class=\"fv-diff-marker\">{prefix}</span>");
    if line.words.is_empty() {
        out.push_str(&esc(&clipped(line.text)));
    } else {
        let mut bytes = 0usize;
        for word in &line.words {
            bytes += word.text.len();
            if bytes > 8192 {
                out.push_str("… [display capped]");
                break;
            }
            if word.changed {
                let _ = write!(out, "<mark>{}</mark>", esc(word.text));
            } else {
                out.push_str(&esc(word.text));
            }
        }
    }
    out.push_str("</code>");
}

fn unified_line(out: &mut String, line: &DiffLine<'_>) {
    let _ = write!(out, "<div class=\"fv-line fv-diff-line {}\">", diff_class(line.kind));
    line_number(out, line.old_number);
    line_number(out, line.new_number);
    diff_text(out, line);
    out.push_str("</div>");
}

/// Pair runs of removed/added lines by position. Context goes on both sides;
/// a `NoNewline` note stays attached to the preceding line on its own side.
fn split_lines(out: &mut String, mut lines: &[DiffLine<'_>]) {
    use DiffLineKind::*;
    while let Some((line, tail)) = lines.split_first() {
        if line.kind == Context {
            let note = tail.first().filter(|line| line.kind == NoNewline);
            split_pair(out, Some((line, note)), Some((line, note)));
            lines = &tail[usize::from(note.is_some())..];
        } else {
            let end = lines.iter().position(|line| line.kind == Context).unwrap_or(lines.len());
            let (run, tail) = lines.split_at(end);
            let side = |kind| {
                run.iter()
                    .enumerate()
                    .filter(move |(_, line)| line.kind == kind)
                    .map(move |(i, line)| (line, run.get(i + 1).filter(|next| next.kind == NoNewline)))
            };
            let mut old = side(Removed);
            let mut new = side(Added);
            loop {
                let (old, new) = (old.next(), new.next());
                if old.is_none() && new.is_none() {
                    break;
                }
                split_pair(out, old, new);
            }
            lines = tail;
        }
    }
}

type SplitCell<'a, 's> = Option<(&'a DiffLine<'s>, Option<&'a DiffLine<'s>>)>;

fn split_pair(out: &mut String, old: SplitCell<'_, '_>, new: SplitCell<'_, '_>) {
    out.push_str("<div class=\"fv-diff-pair\">");
    for (side, cell) in [("old", old), ("new", new)] {
        let _ = write!(out, "<div class=\"fv-diff-side fv-diff-{side}\">");
        if let Some((line, note)) = cell {
            let _ = write!(out, "<div class=\"fv-line fv-diff-line {}\">", diff_class(line.kind));
            line_number(out, if side == "old" { line.old_number } else { line.new_number });
            diff_text(out, line);
            out.push_str("</div>");
            if let Some(note) = note {
                out.push_str("<div class=\"fv-line fv-diff-note\">");
                line_number(out, None);
                diff_text(out, note);
                out.push_str("</div>");
            }
        }
        out.push_str("</div>");
    }
    out.push_str("</div>");
}

pub(crate) fn log(input: &Input) -> String {
    let total = input.text.split_inclusive('\n').count();
    let limit = limit(input, 20);
    let first = if input.line > 0 { input.line.saturating_sub(limit / 2 + 1) } else { total.saturating_sub(limit) };
    let last = first.saturating_add(limit).min(total);
    let mut rows = String::new();
    let (mut errors, mut warnings, mut visible_errors) = (0, 0, 0);
    let mut error_lines = Vec::new();
    let mut spans = ansi_spans(&input.text).into_iter().peekable();
    let base = input.text.as_ptr().addr();
    let mut start = 0;
    let mut pieces = Vec::new();
    let mut plain = String::new();

    for (i, raw) in input.text.split_inclusive('\n').enumerate() {
        pieces.clear();
        let end = start + raw.strip_suffix('\n').unwrap_or(raw).len();
        // Keep the source's physical line numbers even when an OSC/DCS payload
        // spans newlines. Span slices borrow the same source; their addresses
        // identify the overlap without recovering any discarded payload bytes.
        while let Some(&span) = spans.peek() {
            let lo = span.text.as_ptr().addr() - base;
            let hi = lo + span.text.len();
            if lo >= end {
                break;
            }
            let (a, b) = (lo.max(start), hi.min(end));
            if a < b {
                pieces.push(AnsiSpan { text: &input.text[a..b], style: span.style });
            }
            if hi > end {
                break;
            }
            spans.next();
        }
        start += raw.len();

        let severity = match pieces.as_slice() {
            [] => None,
            [piece] => log_severity(piece.text),
            _ => {
                plain.clear();
                for piece in &pieces {
                    plain.push_str(piece.text);
                }
                log_severity(&plain)
            }
        };
        let (class, error) = match severity {
            Some(LogSeverity::Error) => {
                errors += 1;
                error_lines.push((i + 1).to_string());
                visible_errors += usize::from(i >= first && i < last);
                (" fv-log-error", " data-fv-error tabindex=\"-1\"")
            }
            Some(LogSeverity::Warning) => {
                warnings += 1;
                (" fv-log-warning", "")
            }
            _ => ("", ""),
        };
        if i < first || i >= last {
            continue;
        }
        if rows.len() > 2 * 1024 * 1024 {
            continue;
        }

        let line = i + 1;
        let selected = if line == input.line { " fv-selected" } else { "" };
        let _ = write!(
            rows,
            "<div class=\"fv-line fv-log-line{class}{selected}\" data-line=\"{line}\"{error}><span class=\"fv-line-number\">{line}</span><code>"
        );
        for piece in &pieces {
            if rows.len() > 2 * 1024 * 1024 {
                break;
            }
            log_span(&mut rows, *piece);
        }
        rows.push_str("</code></div>");
    }

    let mut out = format!("<div class=\"fv-controls fv-log-controls\" data-fv-errors=\"{}\">", error_lines.join(","));
    let error_word = if errors == 1 { "error" } else { "errors" };
    let warning_word = if warnings == 1 { "warning" } else { "warnings" };
    let _ = write!(out, "<span class=\"fv-log-counts\">{errors} {error_word} · {warnings} {warning_word}");
    if errors != visible_errors {
        let word = if visible_errors == 1 { "error" } else { "errors" };
        let _ = write!(out, " · {visible_errors} {word} shown");
    }
    out.push_str("</span>");
    let disabled = if errors == 0 { " disabled" } else { "" };
    for (action, label) in [("first-error", "First error"), ("prev-error", "Previous error"), ("next-error", "Next error")] {
        let _ = write!(out, "<button type=\"button\" data-fv-action=\"{action}\"{disabled}>{label}</button>");
    }
    if first > 0 {
        more(&mut out, "Load earlier");
    }
    out.push_str("</div><div class=\"fv-code fv-log\" tabindex=\"0\" aria-label=\"Log lines\">");
    out.push_str(&rows);
    out.push_str("</div>");
    out
}

fn log_span(out: &mut String, span: AnsiSpan<'_>) {
    if span.style == AnsiStyle::default() {
        out.push_str(&esc(&clipped(span.text)));
        return;
    }
    // Only fixed declarations and validated numerical RGB components enter
    // the style attribute; untrusted log text only enters the escaped body.
    out.push_str("<span style=\"");
    let AnsiStyle { foreground, background, bold, dim, italic, underline, inverse, strikethrough } = span.style;
    let (fg, bg) = if inverse { (background, foreground) } else { (foreground, background) };
    if let Some(fg) = fg {
        let (r, g, b) = fg.rgb();
        let _ = write!(out, "color:rgb({r},{g},{b});");
    } else if inverse {
        out.push_str("color:Canvas;");
    }
    if let Some(bg) = bg {
        let (r, g, b) = bg.rgb();
        let _ = write!(out, "background-color:rgb({r},{g},{b});");
    } else if inverse {
        out.push_str("background-color:CanvasText;");
    }
    for (enabled, declaration) in [
        (bold, "font-weight:bold;"),
        (dim, "opacity:.7;"),
        (italic, "font-style:italic;"),
        (underline && strikethrough, "text-decoration:underline line-through;"),
        (underline && !strikethrough, "text-decoration:underline;"),
        (!underline && strikethrough, "text-decoration:line-through;"),
    ] {
        if enabled {
            out.push_str(declaration);
        }
    }
    let _ = write!(out, "\">{}</span>", esc(&clipped(span.text)));
}

pub(crate) fn mermaid(input: &Input) -> String {
    format!(
        "<div class=\"fv-controls\"><button type=\"button\" data-fv-action=\"source\" aria-pressed=\"false\">Source / Rendered</button></div>\
         <div class=\"fv-rendered fv-mermaid\"><p role=\"status\">Diagram renders when visible.</p></div>\
         <div class=\"fv-source-view\" hidden><pre><code>{}</code></pre></div>",
        esc(&clipped(&input.text))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_has_escaped_headers_and_separate_paired_columns() {
        let html = diff(&Input {
            text: concat!(
                "diff --git a/<file> b/<file>\nindex <old>..<new>\n--- a/<file>\n+++ b/<file>\n",
                "@@ -6,3 +9,4 @@ <hunk>\n-old <x>\n-old b\n+new <y>\n+new b\n same\n+extra\n"
            )
            .into(),
            ..Default::default()
        });
        assert!(html.contains("<details class=\"fv-diff-file\" open><summary>"));
        assert!(html.contains("<code>a/&lt;file&gt;</code> → <code>b/&lt;file&gt;</code>"));
        assert!(html.contains("index &lt;old&gt;..&lt;new&gt;") && html.contains("@@ &lt;hunk&gt;"));
        assert!(html.contains("data-fv-action=\"split\"") && html.contains("class=\"fv-diff-split\" hidden"));
        assert!(html.contains("<mark>&lt;x&gt;</mark>") && html.contains("<mark>&lt;y&gt;</mark>"));
        let split = html.split_once("class=\"fv-diff-split\" hidden").unwrap().1;
        let pairs: Vec<_> = split.split("<div class=\"fv-diff-pair\">").skip(1).collect();
        assert_eq!(pairs.len(), 4);
        let (old, new) = pairs[0].split_once("fv-diff-side fv-diff-new").unwrap();
        assert!(old.contains(">6</span>") && old.contains("&lt;x&gt;") && !old.contains("&lt;y&gt;"));
        assert!(new.contains(">9</span>") && new.contains("&lt;y&gt;") && !new.contains("&lt;x&gt;"));
        assert_eq!(pairs[2].matches("</span>same</code>").count(), 2);
        assert!(pairs[3].contains("<div class=\"fv-diff-side fv-diff-old\"></div>"));
        assert!(!html.contains("<x>") && !html.contains("<hunk>"));
    }

    #[test]
    fn diff_caps_inline_lines_globally_and_bounds_escaped_fallback() {
        let text = format!("@@ -1,41 +1,41 @@\n{}", (1..=41).map(|n| format!(" line-{n:02}\n")).collect::<String>());
        let mut input = Input { text, mode: "inline".into(), ..Default::default() };
        let html = diff(&input);
        assert_eq!(html.matches("fv-diff-line").count(), 40);
        assert!(html.contains("line-40") && !html.contains("line-41"));
        assert!(html.contains("data-fv-action=\"more\">Expand</button>"));
        assert!(!html.contains("fv-diff-split"));
        input.limit = 41;
        let html = diff(&input);
        assert!(html.contains("line-41") && !html.contains("data-fv-action=\"more\""));

        input.text = "<script>alert(1)</script> &\nsecond\n".into();
        input.limit = 1;
        let html = diff(&input);
        assert!(html.contains("fv-diff-raw") && html.contains("&lt;script&gt;alert(1)&lt;/script&gt; &amp;"));
        assert!(!html.contains("<script>") && !html.contains("second"));
        assert!(html.contains("data-fv-action=\"more\""));
    }

    #[test]
    fn logs_keep_styles_across_lines_without_rendering_escape_payloads() {
        let html = log(&Input {
            text: concat!(
                "\x1b[38;2;1;2;3mER\x1b[1mROR: <script>\n",
                "continued & raw\x1b[0m\r\n",
                "\x1b]8;;https://secret.test/\nhidden\x07WARNING: visible\n",
                "INFO 0 errors\n"
            )
            .into(),
            line: 2,
            ..Default::default()
        });
        assert!(html.contains("1 error · 1 warning"));
        assert!(html.contains("fv-log-error\" data-line=\"1\" data-fv-error tabindex=\"-1\""));
        assert!(html.contains("fv-log-line fv-selected\" data-line=\"2\""));
        assert!(html.contains("color:rgb(1,2,3);font-weight:bold;\">continued &amp; raw</span>"));
        assert!(html.contains("data-line=\"3\"><span class=\"fv-line-number\">3</span><code></code>"));
        assert!(html.contains("fv-log-warning\" data-line=\"4\""));
        assert!(html.contains("&lt;script&gt;") && !html.contains("<script>"));
        assert!(!html.contains("secret.test") && !html.contains("hidden") && !html.contains('\x1b'));
        for action in ["first-error", "prev-error", "next-error"] {
            assert!(html.contains(&format!("data-fv-action=\"{action}\">")));
        }
    }

    #[test]
    fn logs_load_tail_with_actual_line_numbers_and_report_earlier_errors() {
        let mut input = Input {
            text: format!("ERROR: earlier\n{}", (2..=25).map(|n| format!("line {n}\n")).collect::<String>()),
            mode: "inline".into(),
            ..Default::default()
        };
        let html = log(&input);
        assert_eq!(html.matches(" data-line=").count(), 20);
        assert!(html.contains("data-line=\"6\"") && html.contains("data-line=\"25\""));
        assert!(!html.contains("data-line=\"5\""));
        assert!(html.contains("1 error · 0 warnings · 0 errors shown"));
        assert!(html.contains("data-fv-action=\"first-error\">"));
        assert!(html.contains("data-fv-errors=\"1\""));
        assert!(html.contains("data-fv-action=\"more\">Load earlier</button>"));
        input.limit = 25;
        let html = log(&input);
        assert!(html.contains("data-line=\"1\" data-fv-error") && !html.contains("Load earlier"));
        input.limit = 20;
        input.line = 1;
        let html = log(&input);
        assert!(html.contains("data-line=\"1\" data-fv-error"));
        assert!(!html.contains("data-line=\"25\""));
        input.mode = "full".into();
        input.line = 0;
        input.limit = 0;
        input.text = "line\n".repeat(2002);
        let html = log(&input);
        assert_eq!(html.matches(" data-line=").count(), 2000);
        assert!(html.contains("data-line=\"3\"") && html.contains("data-line=\"2002\""));
    }

    #[test]
    fn mermaid_exposes_only_escaped_source_and_a_placeholder() {
        let html = mermaid(&Input { text: "graph LR\nA[\"<svg onload='bad()'>&</svg>\"]".into(), ..Default::default() });
        assert!(html.contains("data-fv-action=\"source\" aria-pressed=\"false\""));
        assert!(html.contains("class=\"fv-rendered fv-mermaid\""));
        assert!(html.contains("class=\"fv-source-view\" hidden"));
        assert!(html.contains("graph LR\nA[&quot;&lt;svg onload=&#39;bad()&#39;&gt;&amp;&lt;/svg&gt;&quot;]"));
        assert!(!html.contains("<svg") && !html.contains("<script") && !html.contains(" class=\"fv-source\""));
    }
}
