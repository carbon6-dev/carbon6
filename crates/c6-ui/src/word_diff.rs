//! Bounded word-level comparisons for decision revisions and unified diffs.

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum WordChange<'a> {
    Same(&'a str),
    Removed(&'a str),
    Added(&'a str),
}

const MAX_CHARS: usize = 4000;

/// Decisions display the words with normalized spaces. Keep their original
/// behavior: whitespace-only revisions use a plain before/after comparison.
pub(crate) fn word_diff<'a>(before: &'a str, after: &'a str) -> Option<Vec<WordChange<'a>>> {
    if too_long(before, after) {
        return None;
    }
    let (a, b): (Vec<_>, Vec<_>) = (before.split_whitespace().collect(), after.split_whitespace().collect());
    if a == b && before != after {
        return None;
    }
    Some(compare(&a, &b))
}

fn too_long(before: &str, after: &str) -> bool {
    [before, after].iter().any(|s| s.chars().take(MAX_CHARS + 1).count() > MAX_CHARS)
}

fn runs(text: &str) -> impl Iterator<Item = &str> {
    let mut whitespace = None;
    let start = text.char_indices().filter_map(move |(i, c)| {
        let kind = c.is_whitespace();
        (whitespace.replace(kind) != Some(kind)).then_some(i)
    });
    let mut start = start.chain(std::iter::once(text.len())).peekable();
    std::iter::from_fn(move || {
        let i = start.next()?;
        Some(&text[i..*start.peek()?])
    })
}

/// Include whitespace runs as tokens, so each side is reconstructed exactly.
/// `budget` counts LCS cells across the caller's entire preview; each pair
/// also has its own cap. `None` means use the original text without highlights.
pub(crate) fn word_diff_preserving_whitespace<'a>(before: &'a str, after: &'a str, budget: &mut usize) -> Option<Vec<WordChange<'a>>> {
    const MAX_CELLS: usize = 64 * 1024;
    if *budget == 0 || too_long(before, after) {
        return None;
    }
    let (a, b): (Vec<_>, Vec<_>) = (runs(before).collect(), runs(after).collect());
    let cells = (a.len() + 1) * (b.len() + 1);
    if cells > MAX_CELLS || cells > *budget {
        return None;
    }
    *budget -= cells;
    Some(compare(&a, &b))
}

/// Bound the quadratic table at the callers before allocating it.
fn compare<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<WordChange<'a>> {
    let cols = b.len() + 1;
    let mut lcs = vec![0_u16; (a.len() + 1) * cols];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i * cols + j] =
                if a[i] == b[j] { lcs[(i + 1) * cols + j + 1] + 1 } else { lcs[(i + 1) * cols + j].max(lcs[i * cols + j + 1]) };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut changes = Vec::new();
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            changes.push(WordChange::Same(a[i]));
            i += 1;
            j += 1;
        } else if i < a.len() && (j == b.len() || lcs[(i + 1) * cols + j] >= lcs[i * cols + j + 1]) {
            changes.push(WordChange::Removed(a[i]));
            i += 1;
        } else {
            changes.push(WordChange::Added(b[j]));
            j += 1;
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::{WordChange::*, *};

    #[test]
    fn reconstructs_both_sides_including_whitespace() {
        let before = "\tkeep  café\n";
        let after = "\tkeep thé\n  ";
        let changes = word_diff_preserving_whitespace(before, after, &mut 1024).unwrap();
        let old: String = changes.iter().filter_map(|c| if let Same(s) | Removed(s) = c { Some(*s) } else { None }).collect();
        let new: String = changes.iter().filter_map(|c| if let Same(s) | Added(s) = c { Some(*s) } else { None }).collect();
        assert_eq!((old.as_str(), new.as_str()), (before, after));
        assert!(changes.contains(&Removed("café")) && changes.contains(&Added("thé")));
        assert_eq!(word_diff_preserving_whitespace(" ", "\t", &mut 4), Some(vec![Removed(" "), Added("\t")]));
        assert_eq!(word_diff_preserving_whitespace("", "", &mut 1), Some(vec![]));
    }

    #[test]
    fn bounds_each_comparison_and_debits_the_shared_budget() {
        let mut budget = 8;
        assert!(word_diff_preserving_whitespace("x", "y", &mut budget).is_some());
        assert!(word_diff_preserving_whitespace("y", "z", &mut budget).is_some());
        assert!(word_diff_preserving_whitespace("z", "a", &mut budget).is_none());
        let mut unlimited = usize::MAX;
        assert!(word_diff_preserving_whitespace(&"é".repeat(4001), "x", &mut unlimited).is_none());
        let many = "x ".repeat(200);
        assert!(word_diff_preserving_whitespace(&many, &many, &mut unlimited).is_none());
    }
}
