/// Keeps the head and tail of `s` within `max` chars, eliding the middle.
pub fn truncate_middle(s: &str, max: usize) -> (String, bool) {
    let count = s.chars().count();
    if count <= max {
        return (s.to_owned(), false);
    }
    let half = max / 2;
    let head: String = s.chars().take(half).collect();
    let tail: String = s.chars().skip(count - half).collect();
    (format!("{head}\n\n… [{} characters omitted] …\n\n{tail}", count - 2 * half), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_short_and_elides_long() {
        assert_eq!(truncate_middle("abc", 10), ("abc".into(), false));
        let (t, cut) = truncate_middle("0123456789", 4);
        assert!(cut);
        assert!(t.starts_with("01") && t.ends_with("89") && t.contains("6 characters omitted"));
    }
}
