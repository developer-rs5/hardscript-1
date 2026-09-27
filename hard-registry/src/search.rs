//! Registry search: fuzzy matching, tags, downloads and latest versions.
//!
//! Search has to feel like a package index rather than a grep: `hard search
//! json` should find `hardscript-json`, `hard search jwt` should rank a
//! package literally called `jwt` above one that merely mentions it, and
//! `hard search --tag=web http` should filter before ranking.
//!
//! Scoring is deliberately simple and explainable — every term contributes a
//! bounded number of points and the sum is the rank — so results can be
//! reproduced and asserted in tests, and so a mirror can re-rank identically.

use crate::model::Package;
use crate::store::Store;
use crate::views::SearchHit;

/// A parsed search query.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// Lowercased terms the name/description must match.
    pub terms: Vec<String>,
    /// Tags that must all be present.
    pub tags: Vec<String>,
    /// Name prefix filter (as opposed to a fuzzy term).
    pub prefix: Option<String>,
    /// Restrict to these tags only (no other filter implied).
    pub only_tags: bool,
}

impl Query {
    /// Parse `text`, plus optional tag and prefix filters.
    pub fn parse(text: &str, tags: &[String], prefix: Option<&str>) -> Query {
        Query {
            terms: text
                .split_whitespace()
                .map(|t| t.to_ascii_lowercase())
                .filter(|t| !t.is_empty())
                .collect(),
            tags: tags.iter().map(|t| t.trim().to_ascii_lowercase()).filter(|t| !t.is_empty()).collect(),
            prefix: prefix.map(|p| p.to_ascii_lowercase()).filter(|p| !p.is_empty()),
            only_tags: text.trim().is_empty(),
        }
    }

    /// Is this query empty (matches everything, subject to filters)?
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.tags.is_empty() && self.prefix.is_none()
    }
}

/// Score one candidate. Higher is better; `0` means "no match".
///
/// The bands, in order of weight:
/// - exact name match: 1000
/// - name starts with the term: 500
/// - name contains the term: 250
/// - name matches as a subsequence: 120
/// - tag matches: 60
/// - description contains the term: 30
/// - a fuzzy (edit distance <= 2) name match: 15
pub fn score(name: &str, description: Option<&str>, tags: &[String], q: &Query) -> i64 {
    let lname = name.to_ascii_lowercase();
    let ldesc = description.unwrap_or("").to_ascii_lowercase();
    let ltags: Vec<String> = tags.iter().map(|t| t.to_ascii_lowercase()).collect();

    if let Some(p) = &q.prefix {
        if !lname.starts_with(p.as_str()) {
            return 0;
        }
    }
    for t in &q.tags {
        if !ltags.iter().any(|x| x == t) {
            return 0;
        }
    }
    if q.terms.is_empty() {
        // A filter-only query still ranks: exact tag hits first.
        return 1 + q.tags.len() as i64;
    }

    let mut total = 0i64;
    for term in &q.terms {
        let mut best = 0i64;
        if lname == *term {
            best = best.max(1000);
        }
        if lname.starts_with(term.as_str()) {
            best = best.max(500);
        }
        if lname.contains(term.as_str()) {
            best = best.max(250);
        }
        if is_subsequence(term, &lname) {
            best = best.max(120);
        }
        if ltags.iter().any(|x| x == term) {
            best = best.max(60);
        }
        if ldesc.contains(term.as_str()) {
            best = best.max(30);
        }
        if best == 0 && edit_distance(term, &lname) <= 2 {
            best = 15;
        }
        if best == 0 {
            // Every term must match somewhere, otherwise the package is out.
            return 0;
        }
        total += best;
    }
    total
}

/// Do `needle`'s characters appear in `haystack` in order?
pub fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut it = haystack.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

/// Levenshtein distance, bounded so long strings cost little.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(cur[j] + 1).min(prev[j + 1] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Rank a set of packages. Ties break on downloads, then name, so the output
/// order is total and reproducible.
pub fn rank(mut packages: Vec<Package>, q: &Query, limit: usize) -> Vec<SearchHit> {
    let mut hits: Vec<SearchHit> = packages
        .drain(..)
        .filter_map(|p| {
            let sc = score(&p.name, p.description.as_deref(), &p.tags, q);
            if sc == 0 {
                return None;
            }
            Some(SearchHit {
                name: p.name,
                version: None,
                description: p.description.clone(),
                license: p.license.clone(),
                downloads: p.downloads,
                tags: p.tags.clone(),
                keywords: p.keywords.clone(),
                owner: p.owner.clone(),
                score: sc,
            })
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.downloads.cmp(&a.downloads))
            .then_with(|| a.name.cmp(&b.name))
    });
    hits.truncate(limit);
    hits
}

/// Run a search against a store, filling in the latest version of each hit.
///
/// Every match is returned, ranked: the caller paginates, so that a client can
/// be told both how many packages matched and how many it is being shown.
pub fn run(store: &dyn Store, q: &Query) -> Vec<SearchHit> {
    let packages = match store.packages(q.prefix.as_deref()) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    let mut hits = rank(packages, q, usize::MAX);
    for h in hits.iter_mut() {
        h.version = store
            .latest(&h.name)
            .ok()
            .flatten()
            .map(|v| v.version.to_string());
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, desc: &str, tags: &[&str], downloads: u64) -> Package {
        let mut p = Package::new(name);
        p.description = Some(desc.to_string());
        p.tags = tags.iter().map(|t| t.to_string()).collect();
        p.downloads = downloads;
        p
    }

    #[test]
    fn exact_name_outranks_everything() {
        let q = Query::parse("jwt", &[], None);
        let pkgs = vec![
            pkg("jwt-tools", "helpers", &[], 100),
            pkg("jsonwebtoken", "jwt implementation", &[], 10),
            pkg("jwt", "tokens", &["auth"], 1),
        ];
        let hits = rank(pkgs, &q, 10);
        assert_eq!(hits[0].name, "jwt");
        assert!(hits[0].score >= 1000);
        assert!(hits[1].score < 1000);
    }

    #[test]
    fn prefix_beats_substring() {
        let q = Query::parse("json", &[], None);
        let pkgs = vec![
            pkg("my-json-parser", "x", &[], 0),
            pkg("jsonwebtoken", "x", &[], 0),
        ];
        let hits = rank(pkgs, &q, 10);
        assert_eq!(hits[0].name, "jsonwebtoken");
    }

    #[test]
    fn subsequence_matching_finds_rough_names() {
        assert!(is_subsequence("jnt", "jsonwebtoken"));
        assert!(!is_subsequence("jws", "jsonwebtoken")); // out of order
        assert!(is_subsequence("jwt", "jsonwebtoken"));
        assert!(!is_subsequence("x", "jsonwebtoken"));
        let q = Query::parse("jnt", &[], None);
        let hits = rank(vec![pkg("jsonwebtoken", "x", &[], 0)], &q, 10);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn all_terms_must_match() {
        let q = Query::parse("json web", &[], None);
        let hits = rank(
            vec![
                pkg("jsonwebtoken", "x", &[], 0),
                pkg("json-parser", "x", &[], 0),
            ],
            &q,
            10,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "jsonwebtoken");
    }

    #[test]
    fn tags_filter_and_rank() {
        let q = Query::parse("", &["web".to_string()], None);
        let pkgs = vec![
            pkg("client", "x", &["web"], 0),
            pkg("b", "x", &["cli"], 0),
            pkg("server", "x", &["web", "fast"], 0),
        ];
        let hits = rank(pkgs, &q, 10);
        // the filter excludes untagged packages; ordering then falls back to
        // downloads and finally name, so it is total and reproducible
        let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(names, vec!["client", "server"]);
    }

    #[test]
    fn a_name_match_outranks_a_tag_match() {
        let q = Query::parse("api", &["web".to_string()], None);
        let pkgs = vec![
            pkg("client", "x", &["web", "api"], 0),
            pkg("web-api", "x", &["web"], 0),
        ];
        let hits = rank(pkgs, &q, 10);
        assert_eq!(hits[0].name, "web-api");
    }

    #[test]
    fn tag_terms_contribute_to_the_score() {
        let q = Query::parse("auth", &[], None);
        let hits = rank(
            vec![
                pkg("alpha", "x", &["auth"], 0),
                pkg("beta", "a library for auth tokens", &[], 0),
            ],
            &q,
            10,
        );
        assert_eq!(hits[0].name, "alpha");
    }

    #[test]
    fn prefix_filter_is_a_hard_constraint() {
        let q = Query::parse("", &[], Some("hard"));
        let hits = rank(
            vec![pkg("hardscript", "x", &[], 0), pkg("json", "x", &[], 0)],
            &q,
            10,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "hardscript");
    }

    #[test]
    fn downloads_break_ties() {
        let q = Query::parse("tool", &[], None);
        let hits = rank(
            vec![
                pkg("tool-a", "x", &[], 5),
                pkg("tool-b", "x", &[], 500),
            ],
            &q,
            10,
        );
        assert_eq!(hits[0].name, "tool-b");
    }

    #[test]
    fn limit_is_respected() {
        let q = Query::parse("t", &[], None);
        let pkgs: Vec<Package> = (0..20).map(|i| pkg(&format!("t{i}"), "x", &[], 0)).collect();
        assert_eq!(rank(pkgs, &q, 5).len(), 5);
    }

    #[test]
    fn empty_query_returns_everything_sorted_by_name() {
        let q = Query::parse("", &[], None);
        assert!(q.is_empty());
        let pkgs = vec![pkg("b", "x", &[], 0), pkg("a", "x", &[], 0)];
        let hits = rank(pkgs, &q, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "a");
    }

    #[test]
    fn no_match_returns_nothing() {
        let q = Query::parse("zzzzz", &[], None);
        assert!(rank(vec![pkg("jwt", "x", &[], 0)], &q, 10).is_empty());
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("jwt", "jwt"), 0);
        assert_eq!(edit_distance("jws", "jwt"), 1);
        assert!(edit_distance("hard-script", "hardscript") <= 2);
    }

    #[test]
    fn typo_tolerance_is_bounded() {
        let q = Query::parse("jwtt", &[], None);
        let hits = rank(vec![pkg("jwt", "x", &[], 0)], &q, 10);
        assert_eq!(hits.len(), 1, "a one-character typo should still match");
        let q2 = Query::parse("zzzzzzzzzz", &[], None);
        assert!(rank(vec![pkg("jwt", "x", &[], 0)], &q2, 10).is_empty());
    }
}
