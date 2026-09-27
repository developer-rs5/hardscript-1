//! Registry search, client side.
//!
//! The server ranks; this module decides *what to ask for* and *how to show
//! it*. Three things happen here that the raw API does not do:
//!
//! - **Query shaping.** A bare term is sent as `?q=`. `--tag` becomes
//!   repeated `tag=` parameters (the server requires *all* of them).
//!   `--owner` is applied client-side, because the public search index is not
//!   indexed by owner.
//! - **Offline search.** When the registry cannot be reached and a cached
//!   index exists, the same ranking runs over the cache, so `hard search` on a
//!   train still answers.
//! - **Rendering.** `hard search` prints a table: name, latest version,
//!   downloads, tags, description — with the score available under `--json`.
//!
//! The ranking itself is a port of the server's, so a cached result and a
//! live result are ordered the same way.

use crate::registry::{HttpResponse, Method, Registry};
use std::collections::BTreeMap;

/// One search hit as the client sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchHit {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub license: Option<String>,
    pub downloads: u64,
    pub tags: Vec<String>,
    pub keywords: Vec<String>,
    pub owner: Option<String>,
    pub score: i64,
}

impl SearchHit {
    /// `name@version`, the form `hard add` wants.
    pub fn spec(&self) -> String {
        match &self.version {
            Some(v) => format!("{}@{}", self.name, v),
            None => self.name.clone(),
        }
    }

    /// One line, aligned, without colour or escape codes.
    pub fn render(&self, width: usize) -> String {
        let name = pad(&self.name, width);
        let version = self.version.clone().unwrap_or_else(|| "-".to_string());
        let tags = if self.tags.is_empty() {
            String::new()
        } else {
            format!("[{}]", self.tags.join(", "))
        };
        let downloads = format!("{} dl", self.downloads);
        let mut line = format!("{name}  {version:<10}  {downloads:>8}  {tags}");
        if let Some(d) = &self.description {
            if !d.is_empty() {
                line.push_str("  ");
                line.push_str(d);
            }
        }
        line
    }
}

fn pad(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(width - len))
    }
}

/// What to search for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchQuery {
    /// The free-text terms.
    pub text: String,
    /// Every tag must be present.
    pub tags: Vec<String>,
    /// Name prefix filter (a hard filter, server-side).
    pub prefix: Option<String>,
    /// Only return these owners (client-side).
    pub owners: Vec<String>,
    /// Results per page.
    pub limit: usize,
    /// Skip this many results.
    pub offset: usize,
    /// Only packages with at least this many downloads.
    pub min_downloads: u64,
    /// The registry to ask.
    pub registry: Option<String>,
}

impl SearchQuery {
    pub fn new(text: &str) -> SearchQuery {
        SearchQuery {
            text: text.trim().to_string(),
            limit: 20,
            ..SearchQuery::default()
        }
    }

    pub fn with_tag(mut self, tag: &str) -> SearchQuery {
        let t = tag.trim().to_ascii_lowercase();
        if !t.is_empty() {
            self.tags.push(t);
        }
        self
    }

    pub fn with_tags(mut self, tags: &[String]) -> SearchQuery {
        for t in tags {
            self = self.with_tag(t);
        }
        self
    }

    pub fn with_prefix(mut self, prefix: &str) -> SearchQuery {
        let p = prefix.trim().to_ascii_lowercase();
        if !p.is_empty() {
            self.prefix = Some(p);
        }
        self
    }

    pub fn with_owner(mut self, owner: &str) -> SearchQuery {
        let o = owner.trim().to_ascii_lowercase();
        if !o.is_empty() {
            self.owners.push(o);
        }
        self
    }

    pub fn with_limit(mut self, limit: usize) -> SearchQuery {
        self.limit = limit.clamp(1, 200);
        self
    }

    /// Is there anything to search for at all?
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
            && self.tags.is_empty()
            && self.prefix.is_none()
            && self.owners.is_empty()
    }

    /// The query string this maps to, for tests and for `--explain`.
    pub fn to_query_string(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.text.is_empty() {
            parts.push(format!("q={}", encode(&self.text)));
        }
        for t in &self.tags {
            parts.push(format!("tag={}", encode(t)));
        }
        if let Some(p) = &self.prefix {
            parts.push(format!("prefix={}", encode(p)));
        }
        if self.limit > 0 {
            parts.push(format!("limit={}", self.limit));
        }
        if self.offset > 0 {
            parts.push(format!("offset={}", self.offset));
        }
        parts.join("&")
    }

    /// The path to request.
    pub fn path(&self) -> String {
        format!("/search?{}", self.to_query_string())
    }
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Where a result set came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Asked the registry.
    Registry,
    /// Answered from the cached index, because the registry was unreachable.
    Cache,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Registry => "registry",
            Origin::Cache => "cache",
        }
    }
}

/// The result of a search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchResults {
    pub query: String,
    pub origin: Option<Origin>,
    pub hits: Vec<SearchHit>,
    /// What the registry reported as the total match count.
    pub total: usize,
    /// Set when the registry could not be reached and no cache could answer.
    pub error: Option<String>,
    /// True when the registry was unreachable but the cache answered.
    pub degraded: bool,
}

impl SearchResults {
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty() && self.error.is_none()
    }

    /// Names only, in rank order.
    pub fn names(&self) -> Vec<String> {
        self.hits.iter().map(|h| h.name.clone()).collect()
    }

    /// `name@version` specs, ready for `hard add`.
    pub fn specs(&self) -> Vec<String> {
        self.hits.iter().map(|h| h.spec()).collect()
    }

    /// A human summary line, printed under the results.
    pub fn summary(&self) -> String {
        let origin = self
            .origin
            .map(|o| o.as_str())
            .unwrap_or("nowhere");
        let mut s = format!("{} result(s) from {} for {}", self.hits.len(), origin, self.describe());
        if self.total > self.hits.len() {
            s.push_str(&format!(" ({} matching)", self.total));
        }
        if self.degraded {
            s.push_str(if self.hits.is_empty() {
                " [the registry could not be reached: nothing in the local cache]"
            } else {
                " [registry unreachable: answered from the cache]"
            });
        }
        s
    }

    /// What the user asked for, as a phrase: the text, or the filters.
    pub fn describe(&self) -> String {
        if self.query.is_empty() {
            String::from("the filters")
        } else {
            format!("{:?}", self.query)
        }
    }

    /// The width of the widest name, for aligned output.
    pub fn name_width(&self) -> usize {
        self.hits
            .iter()
            .map(|h| h.name.chars().count())
            .max()
            .unwrap_or(8)
            .max(8)
    }
}

/// Ask the registry. Any transport error becomes `error` with no hits.
pub fn search(registry: &Registry, query: &SearchQuery) -> SearchResults {
    let mut out = SearchResults {
        query: query.text.clone(),
        ..SearchResults::default()
    };
    if query.is_empty() {
        out.error = Some("search needs a query, a tag, a prefix or an owner".to_string());
        return out;
    }
    match registry.request(Method::Get, &query.path(), Vec::new(), &[], None) {
        Ok(resp) => {
            if !resp.is_success() {
                out.error = Some(Registry::error_message(&resp));
                return out;
            }
            out.origin = Some(Origin::Registry);
            apply(&mut out, query, &resp);
        }
        Err(e) => {
            out.error = Some(e.message);
        }
    }
    out
}

/// Merge a response body into a result set.
fn apply(out: &mut SearchResults, query: &SearchQuery, resp: &HttpResponse) {
    let j = match resp.json() {
        Some(j) => j,
        None => {
            out.error = Some("the registry returned a body that is not JSON".to_string());
            return;
        }
    };
    out.total = j.get("total").and_then(|v| v.as_num()).unwrap_or(0) as usize;
    if let Some(hs_compiler::json::Json::Arr(items)) = j.get("results") {
        for it in items {
            let hit = SearchHit {
                name: it.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                version: it.get("version").and_then(|v| v.as_str()).map(String::from),
                description: it.get("description").and_then(|v| v.as_str()).map(String::from),
                license: it.get("license").and_then(|v| v.as_str()).map(String::from),
                downloads: it.get("downloads").and_then(|v| v.as_num()).unwrap_or(0).max(0) as u64,
                tags: strings(it.get("tags")),
                keywords: strings(it.get("keywords")),
                owner: it.get("owner").and_then(|v| v.as_str()).map(String::from),
                score: it.get("score").and_then(|v| v.as_num()).unwrap_or(0),
            };
            if !keep(&hit, query) {
                continue;
            }
            out.hits.push(hit);
        }
    }
    // The server paginates; truncating again is a no-op unless it ignored the
    // limit, in which case the client is still right.
    if out.hits.len() > query.limit {
        out.hits.truncate(query.limit);
    }
}

fn strings(v: Option<&hs_compiler::json::Json>) -> Vec<String> {
    match v {
        Some(hs_compiler::json::Json::Arr(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

/// The client-side filters the server does not apply.
fn keep(hit: &SearchHit, query: &SearchQuery) -> bool {
    if hit.downloads < query.min_downloads {
        return false;
    }
    if !query.owners.is_empty() {
        let owner = hit.owner.clone().unwrap_or_default().to_ascii_lowercase();
        if !query.owners.iter().any(|o| *o == owner) {
            return false;
        }
    }
    true
}

/// Search the cached metadata when the registry cannot be reached.
///
/// The cache keeps one JSON document per package (`index/<name>.json`), so
/// an offline search is a directory walk plus the same ranking the server
/// uses. If nothing is cached the result carries the original error.
pub fn search_cache(cache: &crate::cache::Cache, query: &SearchQuery) -> SearchResults {
    let mut out = SearchResults {
        query: query.text.clone(),
        ..SearchResults::default()
    };
    if query.is_empty() {
        out.error = Some("search needs a query, a tag, a prefix or an owner".to_string());
        return out;
    }
    let index = cache.root.join("index");
    let Ok(rd) = std::fs::read_dir(&index) else {
        out.error = Some("the cache has no package index to search".to_string());
        return out;
    };
    let mut candidates: Vec<SearchHit> = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e != "json").unwrap_or(true) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(j) = hs_compiler::json::parse(&text) else {
            continue;
        };
        let mut hit = hit_from_metadata(&j, &path);
        hit.score = score(&hit, query);
        if hit.score > 0 && keep(&hit, query) {
            candidates.push(hit);
        }
    }
    candidates.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.downloads.cmp(&a.downloads))
            .then_with(|| a.name.cmp(&b.name))
    });
    out.total = candidates.len();
    out.hits = candidates;
    out.origin = Some(Origin::Cache);
    if out.hits.len() > query.limit {
        out.hits.truncate(query.limit);
    }
    out
}

/// Build a hit from a cached metadata document.
fn hit_from_metadata(j: &hs_compiler::json::Json, path: &std::path::Path) -> SearchHit {
    let name = j
        .get("name")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string()
        });
    let latest = latest_version(j);
    SearchHit {
        name,
        version: latest,
        description: j.get("description").and_then(|v| v.as_str()).map(String::from),
        license: j.get("license").and_then(|v| v.as_str()).map(String::from),
        downloads: j.get("downloads").and_then(|v| v.as_num()).unwrap_or(0).max(0) as u64,
        tags: strings(j.get("tags")),
        keywords: strings(j.get("keywords")),
        owner: j.get("owner").and_then(|v| v.as_str()).map(String::from),
        score: 0,
    }
}

/// The newest version string in a metadata document.
pub fn latest_version(j: &hs_compiler::json::Json) -> Option<String> {
    let mut best: Option<crate::semver::Version> = None;
    if let Some(hs_compiler::json::Json::Arr(versions)) = j.get("versions") {
        for v in versions {
            let Some(text) = v.get("version").and_then(|x| x.as_str()) else {
                continue;
            };
            if v.get("yanked") == Some(&hs_compiler::json::Json::Bool(true)) {
                continue;
            }
            if let Ok(parsed) = crate::semver::Version::parse(text) {
                best = Some(match best {
                    Some(b) if b >= parsed => b,
                    _ => parsed,
                });
            }
        }
    }
    if best.is_none() {
        best = j
            .get("latest")
            .and_then(|x| x.as_str())
            .and_then(|s| crate::semver::Version::parse(s).ok());
    }
    best.map(|v| v.to_string())
}

/// The same scoring the registry uses, so a cached result ranks identically.
///
/// Bands: exact name 1000, prefix 500, substring 250, subsequence 120,
/// tag 60, description 30, fuzzy (edit distance <= 2) 15.
pub fn score(hit: &SearchHit, query: &SearchQuery) -> i64 {
    if let Some(p) = &query.prefix {
        if !hit.name.to_ascii_lowercase().starts_with(p.as_str()) {
            return 0;
        }
    }
    for t in &query.tags {
        if !hit.tags.iter().any(|x| x == t) {
            return 0;
        }
    }
    let terms: Vec<String> = query
        .text
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    if terms.is_empty() {
        return if query.tags.is_empty() && query.prefix.is_none() {
            0
        } else {
            1
        };
    }
    let name = hit.name.to_ascii_lowercase();
    let desc = hit.description.clone().unwrap_or_default().to_ascii_lowercase();
    let mut total = 0i64;
    for term in &terms {
        let mut best = 0i64;
        if name == *term {
            best = best.max(1000);
        }
        if name.starts_with(term.as_str()) {
            best = best.max(500);
        }
        if name.contains(term.as_str()) {
            best = best.max(250);
        }
        if subsequence(term, &name) {
            best = best.max(120);
        }
        if hit.tags.iter().any(|x| x.to_ascii_lowercase() == *term) {
            best = best.max(60);
        }
        if desc.contains(term.as_str()) {
            best = best.max(30);
        }
        if best == 0 && edit_distance(term, &name) <= 2 {
            best = 15;
        }
        if best == 0 {
            return 0;
        }
        total += best;
    }
    total
}

fn subsequence(needle: &str, haystack: &str) -> bool {
    let mut it = haystack.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

/// Levenshtein distance, bounded to 3 (anything larger is a miss anyway).
pub fn edit_distance(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
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

/// Search the registry, falling back to the cache whenever the registry
/// cannot answer.
///
/// This covers both failure modes a user hits: the network is down, and
/// `--offline`, where the registry is disabled before it is even contacted.
/// Either way the answer comes back marked as degraded instead of as an
/// error, because a cached index is better than nothing.
pub fn search_with_fallback(
    registry: &Registry,
    cache: &crate::cache::Cache,
    query: &SearchQuery,
) -> SearchResults {
    let live = search(registry, query);
    if live.error.is_none() {
        return live;
    }
    let cached = search_cache(cache, query);
    if cached.error.is_some() {
        // Nothing to answer with: the caller deserves the real error.
        return live;
    }
    // The cache was searched, so an empty answer is an answer: "not in the
    // cache", not "the registry is down".
    SearchResults {
        degraded: true,
        ..cached
    }
}

/// Render results as a plain table, one hit per line, with a summary.
pub fn render_table(results: &SearchResults) -> String {
    if results.hits.is_empty() {
        return "no packages found\n".to_string();
    }
    let width = results.name_width();
    let mut out = String::new();
    for h in &results.hits {
        out.push_str(&h.render(width));
        out.push('\n');
    }
    out.push_str(&results.summary());
    out.push('\n');
    out
}

/// A `name -> latest version` map, handy for shell loops.
pub fn versions_of(results: &SearchResults) -> BTreeMap<String, String> {
    results
        .hits
        .iter()
        .filter_map(|h| h.version.clone().map(|v| (h.name.clone(), v)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Registry, RegistryConfig};

    fn hit(name: &str, desc: &str, tags: &[&str], downloads: u64) -> SearchHit {
        SearchHit {
            name: name.to_string(),
            version: Some("1.0.0".to_string()),
            description: Some(desc.to_string()),
            license: None,
            downloads,
            tags: tags.iter().map(|t| t.to_string()).collect(),
            keywords: Vec::new(),
            owner: None,
            score: 0,
        }
    }

    fn json_resp(text: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: text.as_bytes().to_vec(),
            headers: Vec::new(),
        }
    }

    #[test]
    fn a_query_becomes_a_path() {
        let q = SearchQuery::new("json web").with_tag("web").with_prefix("hard");
        let path = q.path();
        assert!(path.starts_with("/search?"), "{path}");
        assert!(path.contains("q=json%20web"), "{path}");
        assert!(path.contains("tag=web"), "{path}");
        assert!(path.contains("prefix=hard"), "{path}");
        assert!(path.contains("limit=20"), "{path}");
    }

    #[test]
    fn repeated_tags_are_all_sent() {
        let q = SearchQuery::new("x").with_tags(&["web".to_string(), "fast".to_string()]);
        assert_eq!(q.to_query_string().matches("tag=").count(), 2);
    }

    #[test]
    fn an_empty_query_is_refused() {
        let r = search(
            &Registry::new(RegistryConfig::local("http://127.0.0.1:1")),
            &SearchQuery::new("  "),
        );
        assert!(r.error.unwrap().contains("needs a query"));
        assert!(r.hits.is_empty());
    }

    #[test]
    fn a_dead_registry_reports_an_error() {
        let r = search(
            &Registry::new(RegistryConfig::local("http://127.0.0.1:1")),
            &SearchQuery::new("jwt"),
        );
        assert!(r.error.is_some());
        assert_eq!(r.origin, None);
    }

    #[test]
    fn a_response_is_parsed_into_hits() {
        let resp = json_resp(
            r#"{"query":"jwt","total":2,"count":2,"results":[
                {"name":"jwt","version":"1.2.0","description":"tokens","license":"MIT","downloads":7,"tags":["auth"],"score":1000},
                {"name":"jwt-tools","version":"0.1.0","description":null,"license":null,"downloads":1,"tags":[],"score":500}]}"#,
        );
        let mut out = SearchResults::default();
        apply(&mut out, &SearchQuery::new("jwt"), &resp);
        assert_eq!(out.total, 2);
        assert_eq!(out.hits.len(), 2);
        assert_eq!(out.names(), vec!["jwt", "jwt-tools"]);
        assert_eq!(out.hits[0].downloads, 7);
        assert_eq!(out.hits[0].tags, vec!["auth"]);
        assert_eq!(out.hits[0].spec(), "jwt@1.2.0");
        assert_eq!(out.hits[1].spec(), "jwt-tools@0.1.0");
    }

    #[test]
    fn a_non_json_body_is_an_error_not_a_panic() {
        let resp = json_resp("<html>gateway timeout</html>");
        let mut out = SearchResults::default();
        apply(&mut out, &SearchQuery::new("jwt"), &resp);
        assert!(out.error.unwrap().contains("not JSON"));
    }

    #[test]
    fn the_client_never_returns_more_than_the_limit() {
        let resp = json_resp(
            r#"{"total":4,"results":[
                {"name":"a","version":"1.0.0","score":1},
                {"name":"b","version":"1.0.0","score":2},
                {"name":"c","version":"1.0.0","score":3},
                {"name":"d","version":"1.0.0","score":4}]}"#,
        );
        let mut out = SearchResults::default();
        let mut q = SearchQuery::new("x");
        q.limit = 2;
        apply(&mut out, &q, &resp);
        assert_eq!(out.hits.len(), 2, "a server that ignores limit is still bounded");
        assert_eq!(out.total, 4, "the full match count survives");
    }

    #[test]
    fn the_server_owns_the_offset() {
        // The client asks for `offset` and the server does the skipping, so
        // applying it again here would drop rows twice.
        let resp = json_resp(
            r#"{"total":4,"results":[{"name":"c","version":"1.0.0","score":3}]}"#,
        );
        let mut out = SearchResults::default();
        let mut q = SearchQuery::new("x");
        q.offset = 2;
        apply(&mut out, &q, &resp);
        assert_eq!(out.names(), vec!["c"]);
        assert_eq!(out.total, 4);
    }

    #[test]
    fn owner_and_download_filters_apply_client_side() {
        let resp = json_resp(
            r#"{"total":3,"results":[
                {"name":"a","owner":"ada","downloads":10,"score":1},
                {"name":"b","owner":"bo","downloads":5,"score":2},
                {"name":"c","owner":"ada","downloads":1,"score":3}]}"#,
        );
        let mut q = SearchQuery::new("x");
        q.owners = vec!["ada".to_string()];
        let mut out = SearchResults::default();
        apply(&mut out, &q, &resp);
        assert_eq!(out.names(), vec!["a", "c"]);

        let mut q2 = SearchQuery::new("x");
        q2.min_downloads = 5;
        let mut out2 = SearchResults::default();
        apply(&mut out2, &q2, &resp);
        assert_eq!(out2.names(), vec!["a", "b"]);
    }

    #[test]
    fn scoring_matches_the_registry_bands() {
        let q = SearchQuery::new("jwt");
        assert_eq!(score(&hit("jwt", "", &[], 0), &q), 1000);
        assert_eq!(score(&hit("jwt-tools", "", &[], 0), &q), 500);
        assert_eq!(score(&hit("my-jwt-kit", "", &[], 0), &q), 250);
        // a subsequence that is not a substring scores 120
        assert_eq!(score(&hit("jsonwebtoken", "nothing", &[], 0), &SearchQuery::new("jnt")), 120);
        assert_eq!(score(&hit("other", "", &["jwt"], 0), &q), 60);
        assert_eq!(score(&hit("other", "for jwt users", &[], 0), &q), 30);
        assert_eq!(score(&hit("jw", "", &[], 0), &q), 15);
        assert_eq!(score(&hit("nothing", "", &[], 0), &q), 0);
    }

    #[test]
    fn every_term_must_match() {
        let q = SearchQuery::new("json web");
        // "json" is a prefix (500) and "web" is a substring (250)
        assert_eq!(score(&hit("jsonwebtoken", "", &[], 0), &q), 500 + 250);
        assert_eq!(score(&hit("json", "", &[], 0), &q), 0, "a term with no match disqualifies");
    }

    #[test]
    fn tags_and_prefix_are_hard_filters_in_the_ranking_too() {
        // with no free-text terms a filter-only query still ranks
        let q = SearchQuery::new("").with_tag("web");
        assert_eq!(score(&hit("thing", "", &["web"], 0), &q), 1);
        assert_eq!(score(&hit("thing", "", &["cli"], 0), &q), 0);
        let q2 = SearchQuery::new("").with_prefix("hard");
        assert_eq!(score(&hit("hardscript", "", &[], 0), &q2), 1);
        assert_eq!(score(&hit("my-hard", "", &[], 0), &q2), 0);
        // a text term still has to match
        assert_eq!(score(&hit("thing", "", &["web"], 0), &SearchQuery::new("other").with_tag("web")), 0);
    }

    #[test]
    fn rendering_is_aligned_and_informative() {
        let mut h = hit("jwt", "JSON web tokens", &["auth"], 42);
        h.version = Some("1.2.0".to_string());
        let line = h.render(8);
        assert!(line.starts_with("jwt"), "{line}");
        assert!(line.contains("  1.2.0"), "{line}");
        assert_eq!(line, "jwt       1.2.0          42 dl  [auth]  JSON web tokens");
        assert!(line.contains("42 dl"), "{line}");
        assert!(line.contains("[auth]"), "{line}");
        assert!(line.contains("JSON web tokens"), "{line}");
    }

    #[test]
    fn a_hit_without_a_version_renders_a_dash() {
        let mut h = hit("mystery", "", &[], 0);
        h.version = None;
        assert!(h.render(8).contains("-"));
        assert_eq!(h.spec(), "mystery");
    }

    #[test]
    fn the_name_column_is_padded_to_the_requested_width() {
        assert_eq!(pad("jwt", 6), "jwt   ");
        assert_eq!(pad("jwt", 2), "jwt", "a long name is never truncated");
        // width counts characters, not bytes
        assert_eq!(pad("ünïcödé", 9), "ünïcödé  ");
    }

    #[test]
    fn summaries_mention_origin_and_totals() {
        let r = SearchResults {
            query: "jwt".to_string(),
            origin: Some(Origin::Registry),
            total: 40,
            hits: vec![hit("jwt", "", &[], 0)],
            error: None,
            degraded: false,
        };
        let s = r.summary();
        assert!(s.contains("from registry"), "{s}");
        assert!(s.contains(r#"for "jwt""#), "{s}");
        assert!(s.contains("40 matching"), "{s}");

        let cached = SearchResults {
            origin: Some(Origin::Cache),
            degraded: true,
            ..r
        };
        assert!(cached.summary().contains("answered from the cache"), "{}", cached.summary());
    }

    #[test]
    fn name_width_covers_the_longest_name() {
        let r = SearchResults {
            hits: vec![hit("a", "", &[], 0), hit("long-package-name", "", &[], 0)],
            ..SearchResults::default()
        };
        assert_eq!(r.name_width(), 17);
        let empty = SearchResults::default();
        assert_eq!(empty.name_width(), 8);
    }

    #[test]
    fn latest_version_ignores_yanked() {
        let j = hs_compiler::json::parse(
            r#"{"latest":"2.0.0","versions":[{"version":"1.0.0"},{"version":"2.0.0","yanked":true},{"version":"1.5.0"}]}"#,
        )
        .unwrap();
        assert_eq!(latest_version(&j).as_deref(), Some("1.5.0"));
        let all_yanked =
            hs_compiler::json::parse(r#"{"versions":[{"version":"1.0.0","yanked":true}]}"#).unwrap();
        assert_eq!(latest_version(&all_yanked), None);
        let empty = hs_compiler::json::parse("{}").unwrap();
        assert_eq!(latest_version(&empty), None);
    }

    #[test]
    fn edit_distance_is_bounded_and_correct() {
        assert_eq!(edit_distance("jwt", "jwt"), 0);
        assert_eq!(edit_distance("jwtx", "jwt"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abcdef", "uvwxyz"), 6);
    }

    #[test]
    fn subsequence_matching() {
        assert!(subsequence("jnt", "jsonwebtoken"));
        assert!(!subsequence("jws", "jsonwebtoken"));
    }

    #[test]
    fn an_unreachable_registry_is_answered_from_the_cache() {
        let dir = std::env::temp_dir().join(format!("hard-search-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = crate::cache::Cache::at(dir.clone());
        let index = hs_compiler::json::Json::obj(vec![
            ("name", hs_compiler::json::Json::str("jwt")),
            ("description", hs_compiler::json::Json::str("tokens")),
            ("license", hs_compiler::json::Json::str("MIT")),
            ("downloads", hs_compiler::json::Json::num(9)),
            ("tags", hs_compiler::json::Json::arr(vec![hs_compiler::json::Json::str("auth")])),
            ("owner", hs_compiler::json::Json::str("ada")),
            ("latest", hs_compiler::json::Json::str("1.4.0")),
            ("versions", hs_compiler::json::Json::arr(vec![hs_compiler::json::Json::obj(vec![("version", hs_compiler::json::Json::str("1.4.0"))])])),
        ]);
        cache.write_index("jwt", &index.to_string()).unwrap();

        let registry = Registry::new(RegistryConfig::local("http://127.0.0.1:1"));
        let r = search_with_fallback(&registry, &cache, &SearchQuery::new("jwt"));
        assert!(r.error.is_none(), "{:?}", r.error);
        assert!(r.degraded);
        assert_eq!(r.origin, Some(Origin::Cache));
        assert_eq!(r.names(), vec!["jwt"]);
        assert_eq!(r.hits[0].spec(), "jwt@1.4.0");
        assert_eq!(r.hits[0].downloads, 9);
        assert!(r.summary().contains("answered from the cache"), "{}", r.summary());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cache_miss_keeps_the_original_error() {
        let dir = std::env::temp_dir().join(format!("hard-search-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = crate::cache::Cache::at(dir.clone());
        let registry = Registry::new(RegistryConfig::local("http://127.0.0.1:1"));
        let r = search_with_fallback(&registry, &cache, &SearchQuery::new("jwt"));
        assert!(r.error.is_some());
        assert!(r.hits.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cache_ranks_with_the_same_bands() {
        let dir = std::env::temp_dir().join(format!("hard-search-rank-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = crate::cache::Cache::at(dir.clone());
        for (name, tags) in [("jwt", "web"), ("jsonwebtoken", "web"), ("zzz", "cli")] {
            let j = hs_compiler::json::Json::obj(vec![
                ("name", hs_compiler::json::Json::str(name)),
                ("description", hs_compiler::json::Json::str("d")),
                ("tags", hs_compiler::json::Json::arr(vec![hs_compiler::json::Json::str(tags)])),
                ("latest", hs_compiler::json::Json::str("1.0.0")),
            ]);
            cache.write_index(name, &j.to_string()).unwrap();
        }
        let r = search_cache(&cache, &SearchQuery::new("jwt"));
        assert_eq!(r.names(), vec!["jwt", "jsonwebtoken"], "exact name first");
        assert_eq!(r.hits[0].score, 1000);
        // "jwt" is not a substring of "jsonwebtoken", but it is a subsequence
        assert_eq!(r.hits[1].score, 120);
        let tagged = search_cache(&cache, &SearchQuery::new("").with_tag("cli"));
        assert_eq!(tagged.names(), vec!["zzz"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_table_lists_every_hit() {
        let r = SearchResults {
            query: "jwt".to_string(),
            origin: Some(Origin::Cache),
            total: 1,
            hits: vec![hit("jwt", "tokens", &[], 3)],
            error: None,
            degraded: false,
        };
        let t = render_table(&r);
        assert!(t.contains("jwt"), "{t}");
        assert!(t.contains("3 dl"), "{t}");
        assert!(t.contains("1 result(s) from cache"), "{t}");
        assert_eq!(render_table(&SearchResults::default()), "no packages found\n");
    }

    #[test]
    fn versions_of_builds_a_spec_map() {
        let r = SearchResults {
            hits: vec![hit("a", "", &[], 0), hit("b", "", &[], 0)],
            ..SearchResults::default()
        };
        let m = versions_of(&r);
        assert_eq!(m.get("a").map(String::as_str), Some("1.0.0"));
        assert_eq!(m.len(), 2);
    }
}
