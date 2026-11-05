//! Display-only handling of Responses citations. Never use this on model
//! history: offsets and the original private-use markers belong to the provider.

use std::borrow::Cow;
use std::ops::Range;

use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct UrlCitation {
    pub url: String,
    #[serde(default)]
    pub title: String,
    /// Character offsets into the original text, not UTF-8 byte offsets.
    pub start_index: usize,
    pub end_index: usize,
}

/// Ignore other annotation kinds and malformed entries independently.
pub fn annotations(json: &str) -> Vec<UrlCitation> {
    serde_json::from_str::<Vec<serde_json::Value>>(json)
        .unwrap_or_default()
        .into_iter()
        .filter(|v| v["type"] == "url_citation")
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect()
}

/// Plain display text (previews, notifications and copy). Code is literal.
pub fn clean(text: &str) -> Cow<'_, str> {
    if !text.contains(['\u{e200}', '\u{e201}', '\u{e202}']) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(display(text, &[], "c6-citation-"))
}

/// Markdown with numbered reference links. The UI wraps these references in
/// superscript; Markdown exports keep ordinary, portable links. Unknown markers
/// disappear, never linking a turn id to a guessed search result.
pub fn markdown(text: &str, citations: &[UrlCitation]) -> String {
    display(text, citations, "c6-citation-")
}

/// Exports contain many messages: reference definitions must not collide
/// between them. `scope` is an event/block identity supplied by the exporter.
pub fn markdown_scoped(text: &str, citations: &[UrlCitation], scope: &str) -> String {
    display(text, citations, &format!("c6-citation-{scope}-"))
}

/// The reserved reference labels emitted by `markdown`.
pub fn reference(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("c6-citation-") else { return false };
    let mut numbers = rest.split('-').map(str::parse::<usize>);
    numbers.next_back().is_some_and(|n| n.is_ok_and(|n| n > 0)) && numbers.all(|n| n.is_ok())
}

fn display(text: &str, citations: &[UrlCitation], prefix: &str) -> String {
    if citations.is_empty() && !text.contains(['\u{e200}', '\u{e201}', '\u{e202}']) {
        return text.to_owned();
    }
    // An existing model-written reference definition must not override a
    // source URL from the provider's annotations.
    let mut prefix = prefix.to_owned();
    while text.contains(&prefix) {
        prefix.push_str("0-");
    }
    let mut code = Vec::new();
    let mut in_code = false;
    for (event, range) in Parser::new(text).into_offset_iter() {
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                in_code = true;
                code.push(range);
            }
            Event::End(TagEnd::CodeBlock) => in_code = false,
            Event::Code(_) => code.push(range),
            _ if in_code => code.push(range),
            _ => {}
        }
    }
    let protected = |range: &Range<usize>| code.iter().any(|c| c.start < range.end && range.start < c.end);
    let mut markers = Vec::new();
    let mut at = 0;
    while let Some(i) = text[at..].find('\u{e200}') {
        let start = at + i;
        let end = text[start..].find('\u{e201}').map_or(text.len(), |i| start + i + '\u{e201}'.len_utf8());
        let range = start..end;
        if !protected(&range) {
            markers.push(range);
        }
        at = end;
    }
    let positions: Vec<usize> = text.char_indices().map(|(i, _)| i).chain([text.len()]).collect();
    let mut replacements: Vec<(Range<usize>, Vec<&UrlCitation>)> = markers.into_iter().map(|r| (r, Vec::new())).collect();
    for c in citations {
        let (Some(&start), Some(&end)) = (positions.get(c.start_index), positions.get(c.end_index)) else { continue };
        if start > end || !web_url(&c.url) || protected(&(start..end.max(start + 1))) {
            continue;
        }
        if let Some((_, sources)) = replacements.iter_mut().find(|(r, _)| !r.is_empty() && start < r.end && end > r.start) {
            sources.push(c);
        } else {
            // Normal Responses citations annotate readable text, rather than
            // a private marker. Keep that text and put the link after it.
            replacements.push((end..end, vec![c]));
        }
    }
    replacements.sort_by_key(|(r, _)| (r.start, r.end));
    let mut out = String::new();
    let mut used: Vec<&UrlCitation> = Vec::new();
    let mut cursor = 0;
    for (range, sources) in replacements {
        if range.start < cursor {
            continue;
        }
        out.push_str(&text[cursor..range.start]);
        let rest = &text[range.end..];
        let tight = rest.trim_start_matches([' ', '\t']);
        let before_punctuation = tight.is_empty() || tight.starts_with(|c: char| ".,;:!?)]}".contains(c));
        if sources.is_empty() && (before_punctuation || rest.starts_with(char::is_whitespace)) {
            out.truncate(out.trim_end_matches([' ', '\t']).len());
        }
        let mut numbered = Vec::new();
        for c in sources {
            let number = match used.iter().position(|u| u.url == c.url) {
                Some(i) => i + 1,
                None => {
                    used.push(c);
                    used.len()
                }
            };
            if !numbered.contains(&number) {
                numbered.push(number);
                out.push_str(&format!("[\\[{number}\\]][{prefix}{number}]"));
            }
        }
        cursor = range.end + if before_punctuation { rest.len() - tight.len() } else { 0 };
    }
    out.push_str(&text[cursor..]);
    // Stray separators (including an incomplete streamed delimiter) must not
    // turn into tofu either. Literal code, including its markers, stays intact.
    if out.contains(['\u{e201}', '\u{e202}']) {
        let mut cleaned = String::new();
        let mut cursor = 0;
        for (event, range) in Parser::new(&out).into_offset_iter() {
            if let Event::Code(_) | Event::Start(Tag::CodeBlock(_)) = event {
                cleaned.push_str(&out[cursor..range.start].replace(['\u{e201}', '\u{e202}'], ""));
                cleaned.push_str(&out[range.clone()]);
                cursor = range.end;
            }
        }
        cleaned.push_str(&out[cursor..].replace(['\u{e201}', '\u{e202}'], ""));
        out = cleaned;
    }
    for (i, c) in used.iter().enumerate() {
        let url: String = c
            .url
            .chars()
            .map(|c| match c {
                '<' => "%3C".into(),
                '>' => "%3E".into(),
                '"' => "%22".into(),
                '\\' => "%5C".into(),
                c if c.is_whitespace() => "%20".into(),
                c => c.to_string(),
            })
            .collect();
        let title = c.title.replace('\\', "\\\\").replace('"', "\\\"").replace(['\n', '\r'], " ");
        out.push_str(&format!("\n\n[{prefix}{}]: <{url}> \"{title}\"", i + 1));
    }
    out
}

fn web_url(url: &str) -> bool {
    let scheme = url.split_once("://").map(|(s, _)| s);
    scheme.is_some_and(|s| s.eq_ignore_ascii_case("https") || s.eq_ignore_ascii_case("http")) && !url.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: &str = "\u{e200}cite\u{e202}turn347046search1\u{e201}";
    const TWO: &str = "\u{e200}cite\u{e202}turn919800search4\u{e202}turn919800search5\u{e201}";

    #[test]
    fn unresolved_single_multiple_end_and_punctuation() {
        assert_eq!(clean(&format!("A {ONE}. B {TWO}!")), "A. B!");
        assert_eq!(clean(&format!("A {ONE} . B {TWO} !")), "A. B!");
        assert_eq!(clean(&format!("Sentence. {ONE}")), "Sentence.");
        assert_eq!(clean(&format!("Sentence. {ONE}  ")), "Sentence.");
        assert_eq!(clean(&format!("A {ONE} B")), "A B");
        assert_eq!(clean(&format!("A{ONE}{TWO}.")), "A.");
        // A real saved ChatGPT reply used view ids rather than search ids.
        assert_eq!(
            clean("Exactly one connection matches. \u{e200}cite\u{e202}turn601432view0\u{e202}turn601432view1\u{e201}"),
            "Exactly one connection matches."
        );
    }

    #[test]
    fn code_is_literal_and_ordinary_text_is_unchanged() {
        for text in [format!("`{ONE}`"), format!("`` `{TWO}` ``"), format!("```text\n{ONE}\n```"), "Plain **Markdown**. 🙂".into()] {
            assert_eq!(clean(&text), text);
            assert_eq!(markdown(&text, &[]), text);
        }
        assert!(matches!(clean("no markers"), Cow::Borrowed(_)));
    }

    fn source(url: &str, start: usize, end: usize) -> UrlCitation {
        UrlCitation { url: url.into(), title: "A source".into(), start_index: start, end_index: end }
    }

    #[test]
    fn links_use_character_offsets_and_number_unique_urls() {
        let text = format!("Été {TWO}. Again {ONE}");
        let start = 4;
        let end = start + TWO.chars().count();
        let sources = [
            source("https://one.test", start, end),
            source("https://two.test", start, end),
            source("https://one.test", end + 8, text.chars().count()),
        ];
        let md = markdown(&text, &sources);
        assert!(md.starts_with("Été [\\[1\\]][c6-citation-1][\\[2\\]][c6-citation-2]. Again [\\[1\\]][c6-citation-1]"), "{md}");
        assert_eq!(md.matches("https://one.test").count(), 1);
        assert!(!md.contains('\u{e200}'));
    }

    #[test]
    fn readable_annotations_keep_text_and_unsafe_or_invalid_sources_are_ignored() {
        let c = source("https://source.test", 0, 4);
        assert!(markdown("Fact.", &[c]).starts_with("Fact[\\[1\\]][c6-citation-1]."));
        assert_eq!(markdown(ONE, &[source("javascript:alert(1)", 0, ONE.chars().count())]), "");
        assert_eq!(markdown(ONE, &[source("https://source.test", 1000, 2000)]), "");
        assert_eq!(markdown(&format!("`{ONE}`"), &[source("https://source.test", 1, ONE.chars().count() + 1)]), format!("`{ONE}`"));
    }

    #[test]
    fn partial_stream_markers_and_stray_delimiters_are_hidden() {
        assert_eq!(clean("Fact. \u{e200}cite\u{e202}turn123"), "Fact.");
        assert_eq!(clean("Fact\u{e202}.\u{e201}"), "Fact.");
    }

    #[test]
    fn annotation_metadata_is_additive_and_unknown_or_malformed_entries_do_not_hide_good_sources() {
        let json = r#"[{"type":"other"},{"type":"url_citation","url":"broken"},{"type":"url_citation","url":"https://source.test","title":"Source","start_index":0,"end_index":4}]"#;
        assert_eq!(
            annotations(json),
            vec![UrlCitation { url: "https://source.test".into(), title: "Source".into(), start_index: 0, end_index: 4 }]
        );
        let old: crate::ContentBlock = serde_json::from_str(r#"{"type":"text","text":"Old reply"}"#).unwrap();
        assert!(matches!(&old, crate::ContentBlock::Text { citations, .. } if citations.is_empty()));
        assert_eq!(serde_json::to_string(&old).unwrap(), r#"{"type":"text","text":"Old reply"}"#);
    }

    #[test]
    fn exported_messages_have_independent_source_definitions() {
        let first = markdown_scoped("First.", &[source("https://first.test", 0, 5)], "10-0");
        let second = markdown_scoped("Second.", &[source("https://second.test", 0, 6)], "20-0");
        let combined = format!("{first}\n\n{second}");
        let urls: Vec<_> = Parser::new(&combined)
            .filter_map(|e| match e {
                Event::Start(Tag::Link { dest_url, .. }) => Some(dest_url.into_string()),
                _ => None,
            })
            .collect();
        assert_eq!(urls, ["https://first.test", "https://second.test"]);
    }

    #[test]
    fn model_written_reference_definitions_cannot_override_citation_urls() {
        let text = format!("{ONE}\n\n[c6-citation-1]: https://wrong.test");
        let md = markdown(&text, &[source("https://source.test", 0, ONE.chars().count())]);
        let urls: Vec<_> = Parser::new(&md)
            .filter_map(|e| match e {
                Event::Start(Tag::Link { dest_url, id, .. }) => {
                    assert!(reference(&id));
                    Some(dest_url.into_string())
                }
                _ => None,
            })
            .collect();
        assert_eq!(urls, ["https://source.test"]);
    }
}
