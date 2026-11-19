//! Pure, dependency-free format selection and text parsing for file previews.

use crate::word_diff::{WordChange, word_diff_preserving_whitespace};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Renderer {
    Markdown,
    Code,
    Text,
    Image,
    Pdf,
    Csv,
    Tsv,
    Json,
    JsonLines,
    Diff,
    Log,
    Mermaid,
    Other,
}

/// Prefer an explicit fence tag, then a specific MIME type, then the file
/// extension. Generic types (e.g. `text/plain`) do not override the extension.
/// This is a display hint, not a trust or content validation decision.
pub(crate) fn renderer(name: &str, mime: &str, fence: &str) -> Renderer {
    if !fence.trim().is_empty() {
        let tag = fence.split_whitespace().next().unwrap_or_default().trim_start_matches('.').to_ascii_lowercase();
        return match tag.as_str() {
            "markdown" | "md" => Renderer::Markdown,
            "image" => Renderer::Image,
            "pdf" => Renderer::Pdf,
            "text" | "txt" | "plain" | "plaintext" => Renderer::Text,
            other => extension_renderer(other).unwrap_or(Renderer::Code),
        };
    }
    let mime = mime.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    if let Some(renderer) = mime_renderer(&mime) {
        return renderer;
    }
    let file = name.rsplit(['/', '\\']).next().unwrap_or_default().to_ascii_lowercase();
    match file.as_str() {
        "dockerfile" | "containerfile" | "makefile" | "gnumakefile" => Renderer::Code,
        "build.out" | "test.out" | "build-output.txt" | "test-output.txt" | "stdout" | "stderr" => Renderer::Log,
        _ => file
            .rsplit_once('.')
            .and_then(|(_, ext)| extension_renderer(ext))
            .unwrap_or_else(|| if mime.starts_with("text/") { Renderer::Text } else { Renderer::Other }),
    }
}

fn mime_renderer(mime: &str) -> Option<Renderer> {
    use Renderer::*;
    Some(match mime {
        "text/markdown" | "text/x-markdown" => Markdown,
        "application/pdf" => Pdf,
        "text/csv" | "application/csv" => Csv,
        "text/tab-separated-values" | "text/tsv" => Tsv,
        "application/json" | "text/json" => Json,
        "application/x-ndjson" | "application/ndjson" | "application/jsonl" | "application/x-jsonlines" | "text/jsonl" => JsonLines,
        "text/x-diff" | "text/x-patch" | "text/diff" | "application/x-patch" => Diff,
        "text/x-log" | "text/log" | "application/x-log" => Log,
        "text/vnd.mermaid" | "text/x-mermaid" | "application/vnd.mermaid" => Mermaid,
        "application/javascript"
        | "text/javascript"
        | "application/typescript"
        | "text/typescript"
        | "text/html"
        | "application/xhtml+xml"
        | "text/css"
        | "text/xml"
        | "application/xml"
        | "text/yaml"
        | "text/x-yaml"
        | "application/yaml"
        | "application/x-yaml"
        | "application/toml"
        | "application/sql"
        | "application/x-sh"
        | "text/x-shellscript"
        | "text/x-rust"
        | "text/x-python" => Code,
        mime if mime.starts_with("image/") => Image,
        mime if mime.ends_with("+json") => Json,
        mime if mime.ends_with("+xml") => Code,
        _ => return None,
    })
}

fn extension_renderer(ext: &str) -> Option<Renderer> {
    use Renderer::*;
    Some(match ext {
        "md" | "markdown" | "mdown" | "mkd" => Markdown,
        "png" | "apng" | "jpg" | "jpeg" | "jfif" | "gif" | "webp" | "avif" | "bmp" | "ico" | "svg" | "tif" | "tiff" => Image,
        "pdf" => Pdf,
        "csv" => Csv,
        "tsv" => Tsv,
        "json" => Json,
        "jsonl" | "ndjson" | "jsonlines" => JsonLines,
        "diff" | "patch" => Diff,
        "log" => Log,
        "mmd" | "mermaid" => Mermaid,
        "txt" | "text" => Text,
        "rs" | "rust" | "py" | "pyw" | "python" | "js" | "mjs" | "cjs" | "javascript" | "jsx" | "ts" | "mts" | "cts" | "typescript"
        | "tsx" | "sh" | "zsh" | "bash" | "shell" | "fish" | "ps1" | "powershell" | "go" | "golang" | "rb" | "ruby" | "c" | "h" | "cpp"
        | "c++" | "cxx" | "cc" | "hpp" | "hxx" | "cs" | "csharp" | "kt" | "kts" | "kotlin" | "java" | "swift" | "lua" | "php" | "r"
        | "html" | "htm" | "css" | "scss" | "sql" | "graphql" | "gql" | "xml" | "yaml" | "yml" | "toml" | "ini" | "editorconfig"
        | "dockerfile" | "mk" | "makefile" | "jsonc" => Code,
        _ => return None,
    })
}

#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct DiffFile<'a> {
    /// Contents following `--- ` and `+++ `, including any timestamp.
    pub(crate) old_path: Option<&'a str>,
    pub(crate) new_path: Option<&'a str>,
    /// Pre-hunk text, such as `diff --git`, `index`, or rename metadata.
    pub(crate) metadata: Vec<&'a str>,
    pub(crate) hunks: Vec<DiffHunk<'a>>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct DiffHunk<'a> {
    pub(crate) header: &'a str,
    pub(crate) lines: Vec<DiffLine<'a>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DiffLineKind {
    Context,
    Removed,
    Added,
    NoNewline,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct DiffLine<'a> {
    pub(crate) kind: DiffLineKind,
    pub(crate) old_number: Option<u64>,
    pub(crate) new_number: Option<u64>,
    /// The line without its leading diff marker (except `NoNewline`).
    pub(crate) text: &'a str,
    /// Empty when there are no highlights; render `text` as-is in that case.
    pub(crate) words: Vec<DiffWord<'a>>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct DiffWord<'a> {
    pub(crate) text: &'a str,
    pub(crate) changed: bool,
}

/// Parse ordinary two-way unified patches; return `None` to show raw source
/// for unsupported or malformed patches (e.g. combined diffs/truncated hunks).
pub(crate) fn parse_unified_diff(input: &str) -> Option<Vec<DiffFile<'_>>> {
    let mut source = input.lines().peekable();
    let mut files = Vec::new();
    let mut file = DiffFile::default();
    let mut recognized = false;
    let mut git_header = false;
    let mut word_budget = 1024 * 1024;

    while let Some(line) = source.next() {
        if line.starts_with("diff --git ") {
            if git_header || file.old_path.is_some() || !file.hunks.is_empty() {
                files.push(std::mem::take(&mut file));
            }
            recognized = true;
            git_header = true;
            file.metadata.push(line);
        } else if let Some(old) = line.strip_prefix("--- ").filter(|_| source.peek().is_some_and(|s| s.starts_with("+++ "))) {
            if file.old_path.is_some() || !file.hunks.is_empty() {
                files.push(std::mem::take(&mut file));
                git_header = false;
            }
            recognized = true;
            file.old_path = Some(old);
            file.new_path = source.next().and_then(|s| s.strip_prefix("+++ "));
        } else if line.starts_with("@@") {
            let (mut old, mut new) = hunk_ranges(line)?;
            let mut lines = Vec::new();
            while old.remaining > 0 || new.remaining > 0 {
                let line = source.next()?;
                let (kind, old_number, new_number, text) = match line.as_bytes().first() {
                    Some(b' ') => (DiffLineKind::Context, Some(old.take()?), Some(new.take()?), &line[1..]),
                    Some(b'-') => (DiffLineKind::Removed, Some(old.take()?), None, &line[1..]),
                    Some(b'+') => (DiffLineKind::Added, None, Some(new.take()?), &line[1..]),
                    _ => return None,
                };
                lines.push(DiffLine { kind, old_number, new_number, text, words: Vec::new() });
                if source.peek() == Some(&"\\ No newline at end of file") {
                    lines.push(DiffLine {
                        kind: DiffLineKind::NoNewline,
                        old_number: None,
                        new_number: None,
                        text: source.next()?,
                        words: Vec::new(),
                    });
                }
            }
            highlight_replacements(&mut lines, &mut word_budget);
            recognized = true;
            file.hunks.push(DiffHunk { header: line, lines });
        } else {
            // If a completed hunk is followed by more body lines, the header
            // counts were wrong. Other trailers also need the raw fallback so
            // that we don't move them ahead of a hunk when rendering metadata.
            if !file.hunks.is_empty() && !line.is_empty() {
                return None;
            }
            if file.hunks.is_empty() {
                file.metadata.push(line);
            }
        }
    }
    recognized.then(|| {
        files.push(file);
        files
    })
}

struct DiffRange {
    next: u64,
    remaining: u64,
}

impl DiffRange {
    fn parse(input: &str, prefix: char) -> Option<Self> {
        let input = input.strip_prefix(prefix)?;
        let (next, remaining) = input.split_once(',').unwrap_or((input, "1"));
        if ![next, remaining].iter().all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())) {
            return None;
        }
        let (next, remaining): (u64, u64) = (next.parse().ok()?, remaining.parse().ok()?);
        if remaining > 0 && (next == 0 || next.checked_add(remaining - 1).is_none()) {
            return None;
        }
        Some(Self { next, remaining })
    }

    fn take(&mut self) -> Option<u64> {
        self.remaining = self.remaining.checked_sub(1)?;
        let number = self.next;
        if self.remaining > 0 {
            self.next += 1;
        }
        Some(number)
    }
}

fn hunk_ranges(line: &str) -> Option<(DiffRange, DiffRange)> {
    let mut fields = line.strip_prefix("@@ ")?.split_ascii_whitespace();
    let old = DiffRange::parse(fields.next()?, '-')?;
    let new = DiffRange::parse(fields.next()?, '+')?;
    (fields.next()? == "@@").then_some((old, new))
}

/// Only pair balanced adjacent removed/added runs. On insertions/deletions
/// of unequal size, avoid guessing which distant lines are replacements.
fn highlight_replacements(lines: &mut [DiffLine<'_>], budget: &mut usize) {
    use DiffLineKind::*;
    let mut i = 0;
    while i < lines.len() && *budget > 0 {
        if lines[i].kind != Removed {
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && matches!(lines[i].kind, Removed | NoNewline) {
            i += 1;
        }
        let middle = i;
        while i < lines.len() && matches!(lines[i].kind, Added | NoNewline) {
            i += 1;
        }
        let (before, after) = lines[start..i].split_at_mut(middle - start);
        // A `NoNewline` marker takes no part in comparison or pairing.
        if before.iter().filter(|l| l.kind == Removed).count() != after.iter().filter(|l| l.kind == Added).count() {
            continue;
        }
        for (old, new) in before.iter_mut().filter(|l| l.kind == Removed).zip(after.iter_mut().filter(|l| l.kind == Added)) {
            if old.text == new.text {
                continue;
            }
            if let Some(words) = word_diff_preserving_whitespace(old.text, new.text, budget) {
                for word in words {
                    match word {
                        WordChange::Same(text) => {
                            old.words.push(DiffWord { text, changed: false });
                            new.words.push(DiffWord { text, changed: false });
                        }
                        WordChange::Removed(text) => old.words.push(DiffWord { text, changed: true }),
                        WordChange::Added(text) => new.words.push(DiffWord { text, changed: true }),
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnsiColor {
    /// The standard, bright, and 256-color forms share the xterm indices.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl AnsiColor {
    pub(crate) fn rgb(self) -> (u8, u8, u8) {
        const STANDARD: [(u8, u8, u8); 16] = [
            (0, 0, 0),
            (128, 0, 0),
            (0, 128, 0),
            (128, 128, 0),
            (0, 0, 128),
            (128, 0, 128),
            (0, 128, 128),
            (192, 192, 192),
            (128, 128, 128),
            (255, 0, 0),
            (0, 255, 0),
            (255, 255, 0),
            (0, 0, 255),
            (255, 0, 255),
            (0, 255, 255),
            (255, 255, 255),
        ];
        match self {
            Self::Indexed(i @ 0..=15) => STANDARD[usize::from(i)],
            Self::Indexed(i @ 16..=231) => {
                let i = i - 16;
                let level = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
                (level(i / 36), level(i / 6 % 6), level(i % 6))
            }
            Self::Indexed(i) => {
                let gray = 8 + 10 * (i - 232);
                (gray, gray, gray)
            }
            Self::Rgb(r, g, b) => (r, g, b),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AnsiStyle {
    pub(crate) foreground: Option<AnsiColor>,
    pub(crate) background: Option<AnsiColor>,
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    pub(crate) italic: bool,
    pub(crate) underline: bool,
    pub(crate) inverse: bool,
    pub(crate) strikethrough: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AnsiSpan<'a> {
    /// Render as a text node, never as HTML. Escape payloads are not exposed.
    pub(crate) text: &'a str,
    pub(crate) style: AnsiStyle,
}

/// Interpret only SGR appearance; discard cursor operations, OSC (including
/// hyperlinks), other terminal escape strings, and control characters other
/// than tab and newline. The returned text still needs normal HTML escaping.
pub(crate) fn ansi_spans(input: &str) -> Vec<AnsiSpan<'_>> {
    let mut spans = Vec::new();
    let mut style = AnsiStyle::default();
    let mut chars = input.char_indices().peekable();
    let mut start = 0;
    while let Some((i, c)) = chars.next() {
        if !c.is_control() || matches!(c, '\t' | '\n') {
            continue;
        }
        if start < i {
            spans.push(AnsiSpan { text: &input[start..i], style });
        }
        // Convert the ESC-prefixed forms to their C1 equivalents. For other
        // ESC commands, eat intermediate bytes and one final byte if present.
        let mut control = c;
        if c == '\x1b' {
            control = match chars.peek().map(|&(_, c)| c) {
                Some('[') => '\u{9b}',
                Some(']') => '\u{9d}',
                Some('P' | 'X' | '^' | '_') => '\u{90}',
                _ => '\x1b',
            };
            if control != '\x1b' {
                chars.next();
            } else {
                while chars.next_if(|&(_, c)| (' '..='/').contains(&c)).is_some() {}
                chars.next_if(|&(_, c)| ('0'..='~').contains(&c));
            }
        }
        match control {
            '\u{9b}' => {
                let begin = chars.peek().map_or(input.len(), |&(i, _)| i);
                while chars.next_if(|&(_, c)| (' '..='?').contains(&c)).is_some() {}
                if let Some((end, 'm')) = chars.next_if(|&(_, c)| ('@'..='~').contains(&c)) {
                    let params = &input[begin..end];
                    if params.bytes().all(|b| b.is_ascii_digit() || matches!(b, b';' | b':')) {
                        apply_sgr(&mut style, params);
                    }
                }
            }
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => {
                while let Some((_, ch)) = chars.next() {
                    if ch == '\u{9c}'
                        || (control == '\u{9d}' && ch == '\x07')
                        || (ch == '\x1b' && chars.next_if(|&(_, c)| c == '\\').is_some())
                    {
                        break;
                    }
                }
            }
            _ => {}
        }
        start = chars.peek().map_or(input.len(), |&(i, _)| i);
    }
    if start < input.len() {
        spans.push(AnsiSpan { text: &input[start..], style });
    }
    spans
}

fn apply_sgr(style: &mut AnsiStyle, params: &str) {
    use AnsiColor::Indexed;
    let mut fields = params.split(';');
    while let Some(field) = fields.next() {
        if field.contains(':') {
            let mut parts = field.split(':');
            let target = parts.next().unwrap_or_default();
            let form = parts.next();
            let a = parts.next();
            let b = parts.next();
            let c = parts.next();
            let d = parts.next();
            let color = match (form, a, b, c, d, parts.next()) {
                (Some("5"), Some(i), None, None, None, None) => i.parse().ok().map(Indexed),
                (Some("2"), Some(r), Some(g), Some(b), None, None) | (Some("2"), Some("" | "0"), Some(r), Some(g), Some(b), None) => {
                    rgb_color(r, g, b)
                }
                _ => None,
            };
            if let Some(color) = color {
                match target {
                    "38" => style.foreground = Some(color),
                    "48" => style.background = Some(color),
                    _ => {}
                }
            }
            continue;
        }
        match if field.is_empty() { Ok(0) } else { field.parse::<u8>() } {
            Ok(0) => *style = AnsiStyle::default(),
            Ok(1) => style.bold = true,
            Ok(2) => style.dim = true,
            Ok(3) => style.italic = true,
            Ok(4 | 21) => style.underline = true,
            Ok(7) => style.inverse = true,
            Ok(9) => style.strikethrough = true,
            Ok(22) => (style.bold, style.dim) = (false, false),
            Ok(23) => style.italic = false,
            Ok(24) => style.underline = false,
            Ok(27) => style.inverse = false,
            Ok(29) => style.strikethrough = false,
            Ok(n @ 30..=37) => style.foreground = Some(Indexed(n - 30)),
            Ok(n @ 40..=47) => style.background = Some(Indexed(n - 40)),
            Ok(n @ 90..=97) => style.foreground = Some(Indexed(n - 90 + 8)),
            Ok(n @ 100..=107) => style.background = Some(Indexed(n - 100 + 8)),
            Ok(39) => style.foreground = None,
            Ok(49) => style.background = None,
            Ok(target @ (38 | 48 | 58)) => {
                let color = match fields.next() {
                    Some("5") => fields.next().and_then(|s| s.parse().ok()).map(Indexed),
                    Some("2") => {
                        rgb_color(fields.next().unwrap_or_default(), fields.next().unwrap_or_default(), fields.next().unwrap_or_default())
                    }
                    _ => break,
                };
                if let Some(color) = color {
                    match target {
                        38 => style.foreground = Some(color),
                        48 => style.background = Some(color),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

fn rgb_color(r: &str, g: &str, b: &str) -> Option<AnsiColor> {
    Some(AnsiColor::Rgb(r.parse().ok()?, g.parse().ok()?, b.parse().ok()?))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LogSeverity {
    Trace,
    Debug,
    Info,
    Warning,
    Error,
}

/// Look for a level field in the prefix, not for keywords anywhere in the
/// message. Supports common compiler, timestamped, bracketed, and `level=`
/// formats, including ANSI color around or inside a level label.
pub(crate) fn log_severity(line: &str) -> Option<LogSeverity> {
    if line.chars().any(|c| c.is_control() && !matches!(c, '\t' | '\n')) {
        let plain: String = ansi_spans(line).into_iter().map(|span| span.text).collect();
        return plain_log_severity(&plain);
    }
    plain_log_severity(line)
}

fn plain_log_severity(line: &str) -> Option<LogSeverity> {
    let mut rest = line.trim_start();
    let mut timestamp = false;
    for _ in 0..4 {
        if let Some(bracketed) = rest.strip_prefix('[') {
            let (field, tail) = bracketed.split_once(']')?;
            if let Some(level) = severity_label(field.trim()) {
                return Some(level);
            }
            // A non-level bracketed prefix can be a timestamp or logger tag.
            timestamp |= is_date(field) || is_time(field);
            rest = tail.trim_start_matches(|c: char| c.is_ascii_whitespace() || c == '|');
            continue;
        }
        let (field, tail) = rest.split_at(rest.find(char::is_whitespace).unwrap_or(rest.len()));
        if field.is_empty() {
            return None;
        }
        if is_date(field) || is_time(field) || (timestamp && matches!(field, "-" | "|")) {
            timestamp = true;
            rest = tail.trim_start();
            continue;
        }
        if let Some(("level" | "severity", value)) = field.split_once('=') {
            return severity_label(value.trim_matches(['"', '\'']));
        }
        let end = field.find([':', '[', '|']).unwrap_or(field.len());
        let label = &field[..end];
        let suffix = &field[end..];
        let explicit = suffix.starts_with('|')
            || (suffix.starts_with(':') && !suffix.starts_with("::") && !suffix.starts_with("://"))
            || (suffix.starts_with('[') && suffix.ends_with("]:"));
        // A bare uppercase field is conventional. A bare lowercase keyword
        // on its own is too ambiguous ("warning signs", "error rate").
        if explicit || (suffix.is_empty() && (timestamp || tail.is_empty() || label.bytes().all(|b| b.is_ascii_uppercase()))) {
            return severity_label(label);
        }
        return None;
    }
    None
}

fn severity_label(label: &str) -> Option<LogSeverity> {
    // The longest supported label is "critical"; avoid allocating for long
    // tokens such as bracketed URLs or unbroken diagnostic output.
    if label.len() > 8 {
        return None;
    }
    use LogSeverity::*;
    Some(match label.to_ascii_lowercase().as_str() {
        "trace" => Trace,
        "debug" => Debug,
        "info" | "notice" => Info,
        "warn" | "warning" => Warning,
        "err" | "error" | "fatal" | "critical" | "panic" => Error,
        _ => return None,
    })
}

fn is_date(field: &str) -> bool {
    field.as_bytes().get(..10).is_some_and(|date| {
        date.iter().enumerate().all(|(i, b)| if matches!(i, 4 | 7) { *b == b'-' } else { b.is_ascii_digit() })
            && (field.len() == 10 || field.as_bytes().get(10) == Some(&b'T'))
    })
}

fn is_time(field: &str) -> bool {
    field.as_bytes().get(..5).is_some_and(|time| time[2] == b':' && time[..2].iter().chain(&time[3..]).all(u8::is_ascii_digit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_renderers_from_suffix_mime_and_fence() {
        use Renderer::*;
        for (ext, mime, fence, expected) in [
            ("MD", "text/markdown", "markdown", Markdown),
            ("rs", "text/x-rust", "rust", Code),
            ("PNG", "IMAGE/PNG; charset=utf-8", "image", Image),
            ("pdf", "application/pdf", "pdf", Pdf),
            ("csv", "text/csv", "CSV filename=report.csv", Csv),
            ("tsv", "text/tab-separated-values", "tsv", Tsv),
            ("json", "application/problem+json", "json", Json),
            ("jsonl", "application/x-ndjson", "ndjson", JsonLines),
            ("diff", "text/x-patch", "patch", Diff),
            ("log", "text/x-log", "log", Log),
            ("mmd", "text/vnd.mermaid", "mermaid", Mermaid),
            ("txt", "text/plain", "text", Text),
        ] {
            assert_eq!(renderer(&format!("folder.with.dots/file.{ext}"), "text/plain", ""), expected);
            assert_eq!(renderer("", mime, ""), expected);
            assert_eq!(renderer("file.none", "application/octet-stream", fence), expected);
        }
        assert_eq!(renderer("file.log", "application/pdf", "diff"), Diff);
        assert_eq!(renderer("file.log", "application/pdf", ""), Pdf);
        assert_eq!(renderer("file.log", "application/octet-stream", ""), Log);
        assert_eq!(renderer("folder.with.dots/file", "", ""), Other);
        assert_eq!(renderer("C:\\nested\\TEST.OUT", "", ""), Log);
        assert_eq!(renderer("Makefile", "", ""), Code);
        assert_eq!(renderer("", "", "unknown-language"), Code);
    }

    #[test]
    fn parses_multiple_files_hunks_and_exact_line_numbers() {
        use DiffLineKind::*;
        let input = concat!(
            "diff --git a/a b/a\nindex 123..456\n--- a/a\tdate\n+++ b/a\n",
            "@@ -5,3 +9,4 @@ fn name\n same\n-old\n+new\n+extra\n \n",
            "@@ -80 +85 @@\n---- begins with dashes\n++++ begins with pluses\n",
            "diff --git a/b b/b\nnew file mode 100644\n--- /dev/null\n+++ b/b\n@@ -0,0 +1 @@\n+added\n",
            "--- a/c\n+++ /dev/null\n@@ -1 +0,0 @@\n-deleted\n"
        );
        let files = parse_unified_diff(input).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].metadata, ["diff --git a/a b/a", "index 123..456"]);
        assert_eq!(files[0].old_path, Some("a/a\tdate"));
        assert_eq!(files[0].new_path, Some("b/a"));
        let hunk = &files[0].hunks[0];
        assert_eq!(hunk.header, "@@ -5,3 +9,4 @@ fn name");
        assert_eq!(
            hunk.lines.iter().map(|l| (l.kind, l.old_number, l.new_number, l.text)).collect::<Vec<_>>(),
            [
                (Context, Some(5), Some(9), "same"),
                (Removed, Some(6), None, "old"),
                (Added, None, Some(10), "new"),
                (Added, None, Some(11), "extra"),
                (Context, Some(7), Some(12), ""),
            ]
        );
        assert!(hunk.lines.iter().all(|line| line.words.is_empty()));
        assert_eq!(files[0].hunks[1].lines[0].text, "--- begins with dashes");
        assert_eq!(files[0].hunks[1].lines[1].text, "+++ begins with pluses");
        assert_eq!(files[1].old_path, Some("/dev/null"));
        assert_eq!(files[1].hunks[0].lines[0].new_number, Some(1));
        assert_eq!(files[2].new_path, Some("/dev/null"));
        assert_eq!(files[2].hunks[0].lines[0].old_number, Some(1));
    }

    #[test]
    fn preserves_whitespace_and_ignores_no_newline_markers_when_pairing() {
        let files = parse_unified_diff(concat!(
            "@@ -12 +33 @@\r\n-\tlet  café = old;\r\n\\ No newline at end of file\r\n",
            "+\tlet  café = new;\r\n\\ No newline at end of file\r\n"
        ))
        .unwrap();
        let lines = &files[0].hunks[0].lines;
        assert_eq!(lines[1].kind, DiffLineKind::NoNewline);
        assert_eq!(lines[1].old_number, None);
        assert_eq!(lines[1].new_number, None);
        for (line, changed) in [(&lines[0], "old;"), (&lines[2], "new;")] {
            assert_eq!(line.words.iter().map(|w| w.text).collect::<String>(), line.text);
            assert_eq!(line.words.iter().filter(|w| w.changed).map(|w| w.text).collect::<String>(), changed);
        }
    }

    #[test]
    fn falls_back_for_malformed_or_unsupported_input() {
        for raw in [
            "plain text\n+not a patch",
            "@@@ -1 -1 +1 @@@\n++combined",
            "@@ -1,2 +1 @@\n one",
            "@@ -1,0 +1 @@\n unexpected context",
            "@@ -0 +1 @@\n line zero is invalid",
            "@@ -18446744073709551615,2 +1 @@\n signed overflow",
            "@@ -1 +1 @@\n-old\n+new\n+unexpected",
        ] {
            assert!(parse_unified_diff(raw).is_none(), "accepted {raw:?}");
        }
        assert!(parse_unified_diff("diff --git a/bin b/bin\nBinary files a/bin and b/bin differ\n").unwrap()[0].hunks.is_empty());
    }

    #[test]
    fn interprets_sgr_color_styles_and_resets() {
        use AnsiColor::{Indexed, Rgb};
        let spans = ansi_spans(concat!(
            "\x1b[31;44;1;2;3;4;7;9mstandard\x1b[96;103;22;23;24;27;29mbright",
            "\x1b[38;5;202;48;2;4;5;6mextended\x1b[39;49mclear\x1b[38:2::1:2:3;48:5:255mcolon\u{9b}mreset"
        ));
        assert_eq!(spans.iter().map(|s| s.text).collect::<Vec<_>>(), ["standard", "bright", "extended", "clear", "colon", "reset"]);
        let colors = |i: usize| (spans[i].style.foreground, spans[i].style.background);
        assert_eq!(colors(0), (Some(Indexed(1)), Some(Indexed(4))));
        assert_eq!(colors(1), (Some(Indexed(14)), Some(Indexed(11))));
        assert_eq!(colors(2), (Some(Indexed(202)), Some(Rgb(4, 5, 6))));
        assert_eq!(colors(4), (Some(Rgb(1, 2, 3)), Some(Indexed(255))));
        assert_eq!(
            spans[0].style,
            AnsiStyle {
                foreground: Some(Indexed(1)),
                background: Some(Indexed(4)),
                bold: true,
                dim: true,
                italic: true,
                underline: true,
                inverse: true,
                strikethrough: true,
            }
        );
        assert_eq!(spans[1].style, AnsiStyle { foreground: Some(Indexed(14)), background: Some(Indexed(11)), ..Default::default() });
        assert_eq!(spans[3].style, AnsiStyle::default());
        assert_eq!(spans[5].style, AnsiStyle::default());
        assert_eq!(Indexed(1).rgb(), (128, 0, 0));
        assert_eq!(Indexed(202).rgb(), (255, 95, 0));
        assert_eq!(Indexed(255).rgb(), (238, 238, 238));
        assert_eq!(Rgb(4, 5, 6).rgb(), (4, 5, 6));
    }

    #[test]
    fn discards_escape_payloads_and_other_controls() {
        let input = concat!(
            "\x1b]8;;javascript:bad\x07link\x1b]8;;\x1b\\ \x1b[2J\x1b(0plain\x1bPq<script>\x1b\\",
            "\u{9d}52;clipboard\u{9c}\u{9f}hidden\u{9c}\x00\x7f\r\n\t雪 <b>text only</b>\x1b]unterminated"
        );
        let spans = ansi_spans(input);
        assert!(spans.iter().all(|span| span.style == AnsiStyle::default()));
        assert_eq!(spans.iter().map(|span| span.text).collect::<String>(), "link plain\n\t雪 <b>text only</b>");
        assert!(ansi_spans("\x1b[38;5;999mX\x1b[?31mY\x1b[48:2:0:1:2:999mZ\x1b[31").iter().all(|s| s.style == AnsiStyle::default()));
        assert!(ansi_spans("").is_empty());
    }

    #[test]
    fn recognizes_level_prefixes_without_flagging_message_text() {
        use LogSeverity::*;
        for (line, expected) in [
            (" ERROR disk full", Error),
            ("error[E0308]: mismatched types", Error),
            ("Warning: retrying", Warning),
            ("[2026-01-01T01:10:30Z] [warn] retry", Warning),
            ("2026-01-01 01:10:30,123 | error disk full", Error),
            ("\x1b[31mER\x1b[1mROR\x1b[0m: bad", Error),
            ("level=\"fatal\" service=app", Error),
            ("[logger] [DEBUG] found a warning string", Debug),
            ("[TRACE] diagnostics", Trace),
            ("INFO 0 errors and 0 warnings", Info),
        ] {
            assert_eq!(log_severity(line), Some(expected), "{line}");
        }
        for line in [
            "0 errors, no warnings",
            "error_count=0 warning_count=0",
            "error rate is stable",
            "warning signs are in the manual",
            "we found a WARNING in the file",
            "ERRORS were ignored",
            "warningless run",
            "error::Result",
            "https://example.test/ERROR",
            "\x1b]8;;ERROR\x07link\x1b]8;;\x07",
        ] {
            assert_eq!(log_severity(line), None, "{line}");
        }
    }
}
