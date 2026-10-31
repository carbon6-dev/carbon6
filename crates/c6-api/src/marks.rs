//! Marks (docs/marks.md): a colour, plain or in a pattern, that tells
//! things apart at a glance. A session has one (its tile's bar wears it),
//! and so does each value of a session's aspects: its worker, its work,
//! its project, its realm, its repo, its model. The same value wears the same mark
//! everywhere it shows, each aspect in a shape of its own.
//!
//! A mark is written `<colour>` (plain) or `<colour>/<pattern>`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The colours, chosen to tell apart in light and dark, in the order new
/// things take them. The first twelve were the sessions' only colours.
pub const COLOURS: [&str; 20] = [
    "sky", "emerald", "amber", "rose", "violet", "teal", "orange", "indigo", "lime", "pink", "cyan", "fuchsia", "red", "yellow", "blue",
    "brown", "navy", "olive", "maroon", "slate",
];

/// The patterns a colour can come in besides plain.
pub const PATTERNS: [&str; 4] = ["hstripes", "vstripes", "diagonal", "dots"];

/// The aspects of a session that have marks, in the order a session's
/// header shows them; a key is `<aspect>:<value>` (`worker:<id>`,
/// `work:<id>`, `project:<id>`, `realm:<id>`, `repo:<name>`, `model:<name>`).
pub const ASPECTS: [&str; 6] = ["worker", "work", "project", "realm", "repo", "model"];

/// What a session's header shows unless a realm chose otherwise.
pub const DEFAULT_SHOWN: [&str; 4] = ["worker", "work", "project", "realm"];

/// Every mark, in the order new things take them: plain colours first,
/// then each pattern in every colour.
pub fn all() -> impl Iterator<Item = String> {
    COLOURS.into_iter().map(str::to_owned).chain(PATTERNS.into_iter().flat_map(|p| COLOURS.into_iter().map(move |c| format!("{c}/{p}"))))
}

/// A mark's colour and pattern ("" for plain), if it is one.
pub fn parse(mark: &str) -> Option<(&'static str, &'static str)> {
    let colour = |c: &str| COLOURS.into_iter().find(|x| *x == c);
    match mark.split_once('/') {
        None => Some((colour(mark)?, "")),
        Some((c, p)) => Some((colour(c)?, PATTERNS.into_iter().find(|x| *x == p)?)),
    }
}

pub fn is_mark(mark: &str) -> bool {
    parse(mark).is_some()
}

/// The first mark (in [`all`]'s order) used least often in `taken`.
pub fn pick(taken: &[String]) -> String {
    all().min_by_key(|m| taken.iter().filter(|t| *t == m).count()).unwrap_or_else(|| COLOURS[0].to_owned())
}

/// A plain colour for a value nobody gave a mark, the same every time.
pub fn hashed(value: &str) -> &'static str {
    let h = value.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    COLOURS[h as usize % COLOURS.len()]
}

/// Whether `key` names an aspect's value.
pub fn is_key(key: &str) -> bool {
    key.split_once(':').is_some_and(|(a, v)| ASPECTS.contains(&a) && !v.trim().is_empty() && key.len() <= 300)
}

/// Every mark given, by key (`GET /api/v1/marks`, and the live feed).
pub type Marks = BTreeMap<String, String>;

/// `PUT /api/v1/marks`: gives a value its mark; an empty mark takes it
/// away (back to the one worked out from the value).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SetMark {
    pub key: String,
    #[serde(default)]
    pub mark: String,
}

/// Which aspects a session's header shows (`/api/v1/settings/session_facets`),
/// in [`ASPECTS`]' order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionFacets {
    pub shown: Vec<String>,
}

impl Default for SessionFacets {
    fn default() -> Self {
        SessionFacets { shown: DEFAULT_SHOWN.into_iter().map(str::to_owned).collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_are_colours_alone_or_in_a_pattern() {
        assert_eq!(all().count(), COLOURS.len() * (1 + PATTERNS.len()));
        assert!(all().all(|m| is_mark(&m)));
        assert_eq!(parse("navy/dots"), Some(("navy", "dots")));
        assert_eq!(parse("sky"), Some(("sky", "")));
        for bad in ["", "beige", "sky/", "sky/plaid", "/dots", "Sky"] {
            assert!(!is_mark(bad), "{bad}");
        }
    }

    #[test]
    fn picks_go_through_plain_colours_before_patterns() {
        let mut taken = Vec::new();
        for _ in 0..COLOURS.len() {
            taken.push(pick(&taken));
        }
        assert_eq!(taken, COLOURS.map(str::to_owned));
        assert_eq!(pick(&taken), "sky/hstripes");
        // A freed colour is taken again first.
        taken.retain(|m| m != "teal");
        assert_eq!(pick(&taken), "teal");
    }

    #[test]
    fn keys_name_an_aspect_and_a_value() {
        assert!(is_key("worker:0b0e"));
        assert!(is_key("repo:carbon6"));
        assert!(is_key("project:01a1"));
        assert!(!is_key("worker:"));
        assert!(!is_key("colour:red"));
        assert!(!is_key("worker"));
    }
}
