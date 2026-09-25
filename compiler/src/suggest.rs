//! Intelligent suggestion engine (Diagnostics V2).
//!
//! Given an unknown name (variable, function, module) and the set of known
//! names in scope, produce a "did you mean `candidate`?" candidate when the
//! edit distance is small enough that a typo is plausible. The engine is
//! deterministic: ties resolve to the lexicographically smaller candidate so
//! rendered output never depends on hash-map order.
//!
//! Coverage tracking: [`Diag`]s render a `help:` footer from either their
//! `help` or `suggestion` field; [`coverage`] measures how many diagnostics
//! in a batch carry one — the M3.4.3 target is >= 80%.

use crate::error::Diag;

/// Classic two-row Wagner–Fischer edit distance over `char`s.
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0; m + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (cur[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// The single best "did you mean" candidate for `needle`, or `None`.
///
/// A candidate is plausible when its edit distance is small: at most `3`
/// edits, and strictly fewer edits than the needle's length (so a two-char
/// needle is only ever corrected by a one-char typo, and a one-char needle
/// never). The search is deterministic — lowest distance first,
/// lexicographically smallest on ties.
pub fn closest(needle: &str, candidates: &[String]) -> Option<String> {
    let limit = 3.min(needle.chars().count().saturating_sub(1));
    candidates
        .iter()
        .filter(|c| !c.as_str().is_empty() && c.as_str() != needle)
        .filter_map(|c| {
            let d = levenshtein(needle, c);
            if d <= limit && d > 0 {
                Some((d, c.clone()))
            } else {
                None
            }
        })
        .min_by(|x, y| {
            x.0.cmp(&y.0).then_with(|| x.1.cmp(&y.1))
        })
        .map(|(_, c)| c)
}

/// Franzible: read back the fraction of diagnostics that carry a non-empty
/// `help` or `suggestion` footer. Used for the M3.4.3 coverage target.
pub fn coverage(diags: &[Diag]) -> f64 {
    if diags.is_empty() {
        return 1.0;
    }
    let has_text = |s: &Option<String>| s.as_deref().map(|t| !t.is_empty()).unwrap_or(false);
    let with = diags
        .iter()
        .filter(|d| has_text(&d.help) || has_text(&d.suggestion))
        .count();
    with as f64 / diags.len() as f64
}

/// Build a "Did you mean `x`?" phrase for a matched candidate, or fall back
/// to `fallback` when nothing plausible is found.
pub fn did_you_mean(needle: &str, candidates: &[String], fallback: String) -> String {
    match closest(needle, candidates) {
        Some(cand) => format!("Did you mean `{cand}`? {fallback}"),
        None => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_basics() {
        assert_eq!(levenshtein("", ""), 0);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("user_id", "user_id"), 0);
        assert_eq!(levenshtein("user_id", "userID"), 3);
    }

    #[test]
    fn closest_picks_a_typo() {
        let pool: Vec<String> = ["user_id", "email", "created_at"].iter().map(|s| s.to_string()).collect();
        assert_eq!(closest("userID", &pool).as_deref(), Some("user_id"));
        // exact match is not a suggestion
        assert_eq!(closest("user_id", &pool), None);
    }

    #[test]
    fn closest_stays_deterministic_on_ties() {
        let pool: Vec<String> = ["abc", "abd", "abx"].iter().map(|s| s.to_string()).collect();
        assert_eq!(closest("abz", &pool).as_deref(), Some("abc"));
    }

    #[test]
    fn short_needles_are_only_fixed_by_single_typos() {
        let pool: Vec<String> = ["fs", "is", "h", "help"].iter().map(|s| s.to_string()).collect();
        // id is two chars away from fs -> not a plausible typo
        assert_eq!(closest("id", &pool).as_deref(), Some("is"));
        // one-char needle can never be corrected
        assert_eq!(closest("h", &pool), None);
    }

    #[test]
    fn coverage_tracks_help_presence() {
        use crate::error::{Diag, ErrorKind};
        use crate::token::Span;
        let with_help = Diag::new(ErrorKind::Type, "x", Span::new(1, 1), "Fix it.")
            .with_help("do this");
        let bare = Diag { suggestion: None, ..Diag::new(ErrorKind::Type, "x", Span::new(1, 1), "") };
        let empty = Diag::new(ErrorKind::Type, "x", Span::new(1, 1), "");
        assert_eq!(coverage(&[]), 1.0);
        assert_eq!(coverage(&[with_help.clone()]), 1.0);
        assert_eq!(coverage(&[bare.clone()]), 0.0);
        assert_eq!(coverage(&[empty.clone()]), 0.0);
        assert_eq!(coverage(&[bare, with_help]), 0.5);
    }
}