//! Incremental build cache.
//!
//! Layout under the project root:
//!
//! ```text
//! .hard/cache/env.json               environment stamp (compiler, runtime, flags)
//! .hard/cache/entries/<key>/meta.json  per-module metadata
//! .hard/cache/entries/<key>/items.bin  serialized AST for one module
//! ```
//!
//! Each module's parsed AST is stored under a key derived from its relative
//! path, its source hash and the environment stamp, so a cache entry is only
//! ever reused when the module content and the toolchain both match. Reads are
//! corruption-tolerant: on any parse/length/format error an entry is treated
//! as a miss rather than a build failure.

use crate::ast::Stmt;
use crate::astser;
use crate::json::{self, Json};
use crate::sha256::hex;
use std::io;
use std::path::{Path, PathBuf};

/// Bumped whenever the on-disk layout changes; included in every key so old
/// caches invalidate themselves.
const CACHE_VER: &str = "v1";

pub fn cache_root(project_root: &Path) -> PathBuf {
    project_root.join(".hard").join("cache")
}

/// Everything that must match for a cached artifact to be reused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvStamp {
    pub compiler: String,
    pub runtime: String,
    pub platform: String,
    pub flags: String,
}

impl EnvStamp {
    pub fn new(
        compiler: impl Into<String>,
        runtime: impl Into<String>,
        platform: impl Into<String>,
        flags: impl Into<String>,
    ) -> EnvStamp {
        EnvStamp {
            compiler: compiler.into(),
            runtime: runtime.into(),
            platform: platform.into(),
            flags: flags.into(),
        }
    }

    /// Deterministic fingerprint used in cache keys and meta files.
    pub fn fingerprint(&self) -> String {
        hex(
            format!(
                "env-stamp|{CACHE_VER}|{}|{}|{}|{}",
                self.compiler, self.runtime, self.platform, self.flags
            )
            .as_bytes(),
        )
    }

    pub fn to_json(&self) -> Json {
        Json::obj(vec![
            ("compiler", Json::str(self.compiler.clone())),
            ("runtime", Json::str(self.runtime.clone())),
            ("platform", Json::str(self.platform.clone())),
            ("flags", Json::str(self.flags.clone())),
        ])
    }

    pub fn from_json(j: &Json) -> Option<EnvStamp> {
        Some(EnvStamp {
            compiler: j.get("compiler")?.as_str()?.to_string(),
            runtime: j.get("runtime")?.as_str()?.to_string(),
            platform: j.get("platform")?.as_str()?.to_string(),
            flags: j.get("flags")?.as_str()?.to_string(),
        })
    }

    /// Whole-program ("merged") hash over the topological module list.
    /// `modules` must already be in dependency order; each entry is
    /// `(rel_path, source_sha)`.
    pub fn merged_hash(&self, modules: &[(String, String)]) -> String {
        let mut acc = format!("merged|{CACHE_VER}|{}", self.fingerprint());
        for (rel, sha) in modules {
            acc.push('\n');
            acc.push_str(rel);
            acc.push('|');
            acc.push_str(sha);
        }
        hex(acc.as_bytes())
    }
}

/// A cached module parse: the AST plus the metadata needed for diagnostics
/// and the build manifest.
#[derive(Debug, Clone)]
pub struct EntryData {
    pub rel: String,
    pub source_sha: String,
    pub env_fp: String,
    pub stmts: Vec<Stmt>,
    pub serialized_bytes: u64,
    pub created_ms: u64,
}

impl EntryData {
    pub fn meta_json(&self) -> Json {
        Json::obj(vec![
            ("rel", Json::str(self.rel.clone())),
            ("source_sha", Json::str(self.source_sha.clone())),
            ("env", Json::str(self.env_fp.clone())),
            ("stmt_count", Json::num(self.stmts.len() as i64)),
            ("serialized_bytes", Json::num(self.serialized_bytes as i64)),
            ("created_ms", Json::num(self.created_ms as i64)),
        ])
    }
}

/// On-disk cache of per-module parse results.
#[derive(Debug, Clone)]
pub struct EntryStore {
    root: PathBuf,
}

impl EntryStore {
    pub fn new(project_root: &Path) -> EntryStore {
        EntryStore {
            root: cache_root(project_root),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Parse-tier key for one module.
    pub fn parse_key(rel: &str, source_sha: &str, env_fp: &str) -> String {
        hex(format!("parse|{CACHE_VER}|{rel}|{source_sha}|{env_fp}").as_bytes())
    }

    /// Stored environment fingerprint, if the store has been initialized.
    pub fn read_env(&self) -> Option<String> {
        let bytes = std::fs::read(self.root.join("env.json")).ok()?;
        let j: Json = json::parse(&String::from_utf8_lossy(&bytes))?;
        let fp = j.get("fingerprint")?.as_str()?.to_string();
        if fp.len() != 64 {
            return None;
        }
        Some(fp)
    }

    /// Write (or refresh) the environment stamp. Callers pass the current
    /// stamp; the fingerprint stored is the stamp's own.
    pub fn write_env(&self, env: &EnvStamp) -> io::Result<()> {
        let j = Json::obj(vec![
            ("fingerprint", Json::str(env.fingerprint())),
            ("stamp", env.to_json()),
        ]);
        std::fs::create_dir_all(&self.root)?;
        atomic_write(&self.root.join("env.json"), j.to_string().as_bytes())
    }

    pub fn entry_dir(&self, key: &str) -> PathBuf {
        self.root.join("entries").join(key)
    }

    /// Cache hit. Returns `None` for a miss and for any corrupt/mismatched
    /// entry (a corrupt cache is never a build failure).
    pub fn get(&self, key: &str, source_sha: &str, env_fp: &str) -> Option<EntryData> {
        let dir = self.entry_dir(key);
        let meta_bytes = std::fs::read(dir.join("meta.json")).ok()?;
        let j = json::parse(&String::from_utf8_lossy(&meta_bytes))?;
        if j.get("source_sha")?.as_str()? != source_sha {
            return None;
        }
        if j.get("env")?.as_str()? != env_fp {
            return None;
        }
        let items_bytes = std::fs::read(dir.join("items.bin")).ok()?;
        let stmts = astser::deserialize_stmts(&items_bytes).map_err(|_| ()).ok()?;
        Some(EntryData {
            rel: j.get("rel")?.as_str()?.to_string(),
            source_sha: source_sha.to_string(),
            env_fp: env_fp.to_string(),
            stmts,
            serialized_bytes: items_bytes.len() as u64,
            created_ms: j.get("created_ms").and_then(|v| v.as_num()).unwrap_or(0) as u64,
        })
    }

    /// Cache write. `items.bin` is written atomically first; `meta.json` is
    /// written last as the commit marker, so a partially-written entry reads
    /// as a miss.
    pub fn put(&self, key: &str, data: &EntryData) -> io::Result<()> {
        let dir = self.entry_dir(key);
        std::fs::create_dir_all(&dir)?;
        let bytes = astser::serialize_stmts(&data.stmts).map_err(io::Error::other)?;
        atomic_write(&dir.join("items.bin"), &bytes)?;
        atomic_write(&dir.join("meta.json"), data.meta_json().to_string().as_bytes())
    }

    /// Number of valid entries (dirs with a readable meta.json).
    pub fn count(&self) -> usize {
        let entries = match std::fs::read_dir(self.root.join("entries")) {
            Ok(e) => e,
            Err(_) => return 0,
        };
        entries
            .flatten()
            .filter(|de| {
                let dir = de.path();
                dir.is_dir() && std::fs::read(dir.join("meta.json")).is_ok()
            })
            .count()
    }

    /// Total bytes used by the on-disk cache.
    pub fn size_bytes(&self) -> u64 {
        fn walk(p: &Path, acc: &mut u64) {
            if let Ok(rd) = std::fs::read_dir(p) {
                for de in rd.flatten() {
                    let path = de.path();
                    if path.is_file() {
                        if let Ok(md) = std::fs::metadata(&path) {
                            *acc += md.len();
                        }
                    } else {
                        walk(&path, acc);
                    }
                }
            }
        }
        let mut total = 0;
        walk(&self.root, &mut total);
        total
    }

    /// Remove a single entry by key (used by tests and corruption repair).
    pub fn drop_entry(&self, key: &str) -> io::Result<()> {
        std::fs::remove_dir_all(self.entry_dir(key))
    }
}

/// Write `bytes` to `path` via a temp file + rename so a crash mid-write
/// never leaves a partial file at the final path.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, Stmt};
    use crate::token::Span;
    use std::time::SystemTime;

    fn env() -> EnvStamp {
        EnvStamp::new("0.1.0", "rt-sha", "x86_64-linux", "release -O3")
    }

    fn ast() -> Vec<Stmt> {
        vec![Stmt::ExprStmt(Expr::Int(7, Span::new(1, 1)))]
    }

    fn temp_root(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hardcache_{}_{}_{}",
            std::process::id(),
            tag,
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn fingerprint_deterministic_and_sensitive() {
        let a = env().fingerprint();
        assert_eq!(a.len(), 64);
        assert_eq!(env().fingerprint(), a);
        let mut other = env();
        other.runtime = "rt-sha2".to_string();
        assert_ne!(other.fingerprint(), a);
        let mut other = env();
        other.flags = "dev -O0".to_string();
        assert_ne!(other.fingerprint(), a);
    }

    #[test]
    fn stamp_json_round_trip() {
        let a = env();
        let j = a.to_json();
        let b = EnvStamp::from_json(&j).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert!(EnvStamp::from_json(&Json::obj(vec![])).is_none());
    }

    #[test]
    fn merged_hash_order_sensitive() {
        let a = env();
        let m1 = vec![("a.hard".to_string(), "1".to_string()), ("b.hard".to_string(), "2".to_string())];
        let m2 = vec![("b.hard".to_string(), "2".to_string()), ("a.hard".to_string(), "1".to_string())];
        assert_ne!(a.merged_hash(&m1), a.merged_hash(&m2));
        assert_eq!(a.merged_hash(&m1), a.merged_hash(&m1));
    }

    #[test]
    fn parse_key_deterministic() {
        let e = env();
        let fp = e.fingerprint();
        let k1 = EntryStore::parse_key("main.hard", "abc", &fp);
        let k2 = EntryStore::parse_key("main.hard", "abc", &fp);
        assert_eq!(k1, k2);
        assert_ne!(k1, EntryStore::parse_key("main.hard", "abd", &fp));
        assert_ne!(k1, EntryStore::parse_key("util.hard", "abc", &fp));
    }

    #[test]
    fn put_get_round_trip() {
        let root = temp_root("roundtrip");
        let store = EntryStore::new(&root);
        let e = env();
        let data = EntryData {
            rel: "main.hard".to_string(),
            source_sha: "srcsha".to_string(),
            env_fp: e.fingerprint(),
            stmts: ast(),
            serialized_bytes: 0,
            created_ms: 42,
        };
        let key = EntryStore::parse_key("main.hard", "srcsha", &e.fingerprint());
        store.put(&key, &data).unwrap();
        got(&store, &e, &key, "srcsha", data.stmts.len());
        assert_eq!(store.count(), 1);
        assert!(store.size_bytes() > 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn got(store: &EntryStore, e: &EnvStamp, key: &str, sha: &str, n: usize) {
        let hit = store.get(key, sha, &e.fingerprint()).unwrap();
        assert_eq!(hit.stmts.len(), n);
        assert_eq!(hit.rel, "main.hard");
        assert_eq!(hit.created_ms, 42);
    }

    #[test]
    fn stale_source_is_a_miss() {
        let root = temp_root("stale");
        let store = EntryStore::new(&root);
        let e = env();
        let data = EntryData {
            rel: "main.hard".to_string(),
            source_sha: "s1".to_string(),
            env_fp: e.fingerprint(),
            stmts: ast(),
            serialized_bytes: 0,
            created_ms: 1,
        };
        let key = EntryStore::parse_key("main.hard", "s1", &e.fingerprint());
        store.put(&key, &data).unwrap();
        assert!(store.get(&key, "s2", &e.fingerprint()).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wrong_env_is_a_miss() {
        let root = temp_root("env");
        let store = EntryStore::new(&root);
        let e = env();
        let data = EntryData {
            rel: "main.hard".to_string(),
            source_sha: "s1".to_string(),
            env_fp: e.fingerprint(),
            stmts: ast(),
            serialized_bytes: 0,
            created_ms: 1,
        };
        let key = EntryStore::parse_key("main.hard", "s1", &e.fingerprint());
        store.put(&key, &data).unwrap();
        let other = EnvStamp::new("9.9.9", "rt-sha", "x86_64-linux", "release -O3");
        assert!(store.get(&key, "s1", &other.fingerprint()).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_items_is_a_miss_not_a_failure() {
        let root = temp_root("corrupt");
        let store = EntryStore::new(&root);
        let e = env();
        let data = EntryData {
            rel: "main.hard".to_string(),
            source_sha: "s1".to_string(),
            env_fp: e.fingerprint(),
            stmts: ast(),
            serialized_bytes: 0,
            created_ms: 1,
        };
        let key = EntryStore::parse_key("main.hard", "s1", &e.fingerprint());
        store.put(&key, &data).unwrap();
        std::fs::write(store.entry_dir(&key).join("items.bin"), b"garbage that is not astser").unwrap();
        assert!(store.get(&key, "s1", &e.fingerprint()).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_meta_is_a_miss() {
        let root = temp_root("nometa");
        let store = EntryStore::new(&root);
        let e = env();
        let key = EntryStore::parse_key("main.hard", "s1", &e.fingerprint());
        std::fs::create_dir_all(store.entry_dir(&key)).unwrap();
        assert!(store.get(&key, "s1", &e.fingerprint()).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn env_write_and_read_round_trip() {
        let root = temp_root("envfile");
        let store = EntryStore::new(&root);
        let e = env();
        store.write_env(&e).unwrap();
        assert_eq!(store.read_env(), Some(e.fingerprint()));
        let _ = std::fs::remove_dir_all(&root);
    }
}