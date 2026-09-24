//! Build manifest (`.hard/build.json`).
//!
//! Records everything needed to (a) decide whether a whole build can be
//! skipped (warm path) and (b) let `hard doctor` report cache behavior. The
//! manifest is written after every build, whether or not native recompilation
//! happened. Content is deterministic except for wall-clock timestamps and
//! measured stage timings.

use crate::cache::EnvStamp;
use crate::json::{self, Json};
use std::io;
use std::path::{Path, PathBuf};

const SCHEMA: &str = "hard-build/v1";

/// Parse-tier outcome for one module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleStatus {
    /// Parsed AST reused from the cache.
    Hit,
    /// Parsed from source this build.
    Miss,
}

impl ModuleStatus {
    fn as_str(&self) -> &'static str {
        match self {
            ModuleStatus::Hit => "hit",
            ModuleStatus::Miss => "miss",
        }
    }
    fn from_str(s: &str) -> Option<ModuleStatus> {
        match s {
            "hit" => Some(ModuleStatus::Hit),
            "miss" => Some(ModuleStatus::Miss),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleEntry {
    pub rel: String,
    pub source_sha: String,
    /// Dependency rel paths, in import order.
    pub deps: Vec<String>,
    pub status: ModuleStatus,
    pub items_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CacheCounts {
    pub hits: usize,
    pub misses: usize,
    pub skipped: usize,
    pub compiled: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Timings {
    pub discover_ms: f64,
    pub parse_ms: f64,
    pub merge_ms: f64,
    pub optimize_ms: f64,
    pub typecheck_ms: f64,
    pub codegen_ms: f64,
    pub native_ms: f64,
    pub total_ms: f64,
}

/// One build's record. Timestamps/timings are informational; all decision
/// inputs are hashes, so the manifest never needs to be normalized.
#[derive(Debug, Clone)]
pub struct BuildManifest {
    pub created_ms: u64,
    pub env_fp: String,
    pub merged_hash: String,
    pub root: String,
    pub flags: Vec<String>,
    pub jobs: usize,
    pub cpp: String,
    pub binary: String,
    pub native_skipped: bool,
    pub cache: CacheCounts,
    pub timings: Timings,
    pub modules: Vec<ModuleEntry>,
}

/// SHA-256 hex of a source file's bytes.
pub fn source_sha_bytes(bytes: &[u8]) -> String {
    crate::sha256::hex(bytes)
}

/// SHA-256 hex of a source file on disk.
pub fn source_sha(path: &Path) -> io::Result<String> {
    Ok(source_sha_bytes(&std::fs::read(path)?))
}

impl BuildManifest {
    pub fn to_json(&self) -> Json {
        Json::obj(vec![
            ("schema", Json::str(SCHEMA)),
            ("created_ms", Json::num(self.created_ms as i64)),
            ("env_fp", Json::str(self.env_fp.clone())),
            ("merged_hash", Json::str(self.merged_hash.clone())),
            ("root", Json::str(self.root.clone())),
            (
                "flags",
                Json::arr(self.flags.iter().cloned().map(Json::str).collect()),
            ),
            ("jobs", Json::num(self.jobs as i64)),
            ("cpp", Json::str(self.cpp.clone())),
            ("binary", Json::str(self.binary.clone())),
            ("native_skipped", Json::Bool(self.native_skipped)),
            (
                "cache",
                Json::obj(vec![
                    ("hits", Json::num(self.cache.hits as i64)),
                    ("misses", Json::num(self.cache.misses as i64)),
                    ("skipped", Json::num(self.cache.skipped as i64)),
                    ("compiled", Json::num(self.cache.compiled as i64)),
                ]),
            ),
            ("timings_ms", self.timings_tree()),
            (
                "modules",
                Json::arr(
                    self.modules
                        .iter()
                        .map(|m| {
                            Json::obj(vec![
                                ("rel", Json::str(m.rel.clone())),
                                ("sha", Json::str(m.source_sha.clone())),
                                (
                                    "deps",
                                    Json::arr(m.deps.iter().cloned().map(Json::str).collect()),
                                ),
                                ("status", Json::str(m.status.as_str())),
                                ("items_bytes", Json::num(m.items_bytes as i64)),
                            ])
                        })
                        .collect(),
                ),
            ),
        ])
    }

    fn timings_tree(&self) -> Json {
        let t = &self.timings;
        Json::obj(vec![
            ("discover", Json::float(t.discover_ms)),
            ("parse", Json::float(t.parse_ms)),
            ("merge", Json::float(t.merge_ms)),
            ("optimize", Json::float(t.optimize_ms)),
            ("typecheck", Json::float(t.typecheck_ms)),
            ("codegen", Json::float(t.codegen_ms)),
            ("native", Json::float(t.native_ms)),
            ("total", Json::float(t.total_ms)),
        ])
    }

    pub fn from_json(j: &Json) -> Option<BuildManifest> {
        if j.get("schema")?.as_str()? != SCHEMA {
            return None;
        }
        let cache = j.get("cache")?;
        let tim = j.get("timings_ms")?;
        let mut modules = Vec::new();
        for m in j.get("modules")?.as_arr()? {
            let mut deps = Vec::new();
            for d in m.get("deps")?.as_arr()? {
                deps.push(d.as_str()?.to_string());
            }
            modules.push(ModuleEntry {
                rel: m.get("rel")?.as_str()?.to_string(),
                source_sha: m.get("sha")?.as_str()?.to_string(),
                deps,
                status: ModuleStatus::from_str(m.get("status")?.as_str()?)?,
                items_bytes: m.get("items_bytes")?.as_num()? as u64,
            });
        }
        Some(BuildManifest {
            created_ms: j.get("created_ms")?.as_num()? as u64,
            env_fp: j.get("env_fp")?.as_str()?.to_string(),
            merged_hash: j.get("merged_hash")?.as_str()?.to_string(),
            root: j.get("root")?.as_str()?.to_string(),
            flags: j
                .get("flags")?
                .as_arr()?
                .iter()
                .map(|v| v.as_str().map(String::from))
                .collect::<Option<Vec<_>>>()?,
            jobs: j.get("jobs")?.as_num()? as usize,
            cpp: j.get("cpp")?.as_str()?.to_string(),
            binary: j.get("binary")?.as_str()?.to_string(),
            native_skipped: j.get("native_skipped")?.as_bool()?,
            cache: CacheCounts {
                hits: cache.get("hits")?.as_num()? as usize,
                misses: cache.get("misses")?.as_num()? as usize,
                skipped: cache.get("skipped")?.as_num()? as usize,
                compiled: cache.get("compiled")?.as_num()? as usize,
            },
            timings: Timings {
                discover_ms: tim.get("discover")?.as_float()?,
                parse_ms: tim.get("parse")?.as_float()?,
                merge_ms: tim.get("merge")?.as_float()?,
                optimize_ms: tim.get("optimize")?.as_float()?,
                typecheck_ms: tim.get("typecheck")?.as_float()?,
                codegen_ms: tim.get("codegen")?.as_float()?,
                native_ms: tim.get("native")?.as_float()?,
                total_ms: tim.get("total")?.as_float()?,
            },
            modules,
        })
    }

    pub fn manifest_path(project_root: &Path) -> PathBuf {
        project_root.join(".hard").join("build.json")
    }

    pub fn save(&self, project_root: &Path) -> io::Result<()> {
        let dir = project_root.join(".hard");
        std::fs::create_dir_all(&dir)?;
        crate::cache::atomic_write_file(
            &dir.join("build.json"),
            self.to_json().to_string().as_bytes(),
        )
    }

    /// Load the previous manifest for warm-build decisions; a missing or
    /// corrupt manifest reads as None (never a failure).
    pub fn load(project_root: &Path) -> Option<BuildManifest> {
        let bytes = std::fs::read(Self::manifest_path(project_root)).ok()?;
        let j = json::parse(&String::from_utf8_lossy(&bytes))?;
        BuildManifest::from_json(&j)
    }

    /// True when a warm build may skip the entire frontend AND native stage:
    /// same environment, same merged program hash, outputs still on disk.
    /// Parallelism (`jobs`) never changes output bytes, so it is not part of
    /// the warm-build decision.
    pub fn is_warm(&self, env: &EnvStamp, merged_hash: &str, cpp: &Path, binary: &Path) -> bool {
        self.env_fp == env.fingerprint()
            && self.merged_hash == merged_hash
            && Path::new(&self.cpp) == cpp
            && Path::new(&self.binary) == binary
            && cpp.exists()
            && binary.exists()
    }
}

/// Read a build-manifest-safe `as_float`/`as_bool`/`as_arr` helpers.
impl Json {
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Json::Float(f) => Some(*f),
            Json::Num(n) => Some(*n as f64),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> BuildManifest {
        BuildManifest {
            created_ms: 1_700_000_000_000,
            env_fp: "e".repeat(64),
            merged_hash: "m".repeat(64),
            root: "main.hard".to_string(),
            flags: vec!["-O2".to_string()],
            jobs: 8,
            cpp: "./.hard/main.cpp".to_string(),
            binary: "./.hard/main".to_string(),
            native_skipped: false,
            cache: CacheCounts { hits: 3, misses: 1, skipped: 0, compiled: 1 },
            timings: Timings {
                discover_ms: 1.0,
                parse_ms: 2.0,
                merge_ms: 0.25,
                optimize_ms: 0.5,
                typecheck_ms: 0.7,
                codegen_ms: 1.5,
                native_ms: 122.0,
                total_ms: 130.0,
            },
            modules: vec![
                ModuleEntry {
                    rel: "main.hard".to_string(),
                    source_sha: "a".repeat(64),
                    deps: vec!["utils.hard".to_string()],
                    status: ModuleStatus::Hit,
                    items_bytes: 512,
                },
                ModuleEntry {
                    rel: "utils.hard".to_string(),
                    source_sha: "b".repeat(64),
                    deps: vec![],
                    status: ModuleStatus::Hit,
                    items_bytes: 256,
                },
            ],
        }
    }

    #[test]
    fn json_round_trip_preserves_everything() {
        let a = sample();
        let b = BuildManifest::from_json(&a.to_json()).unwrap();
        assert_eq!(b.created_ms, a.created_ms);
        assert_eq!(b.env_fp, a.env_fp);
        assert_eq!(b.merged_hash, a.merged_hash);
        assert_eq!(b.root, a.root);
        assert_eq!(b.flags, a.flags);
        assert_eq!(b.jobs, a.jobs);
        assert_eq!(b.cpp, a.cpp);
        assert_eq!(b.binary, a.binary);
        assert_eq!(b.native_skipped, a.native_skipped);
        assert_eq!(b.cache, a.cache);
        assert_eq!(b.modules, a.modules);
        assert_eq!(b.timings.total_ms, a.timings.total_ms);
        assert_eq!(b.timings.parse_ms, a.timings.parse_ms);
    }

    #[test]
    fn rejects_unknown_schema() {
        let j = Json::obj(vec![("schema", Json::str("nope"))]);
        assert!(BuildManifest::from_json(&j).is_none());
        // Missing required fields also reject.
        let bad = Json::obj(vec![("schema", Json::str(SCHEMA))]);
        assert!(BuildManifest::from_json(&bad).is_none());
    }

    #[test]
    fn source_sha_is_deterministic() {
        assert_eq!(source_sha_bytes(b"x"), source_sha_bytes(b"x"));
        assert_ne!(source_sha_bytes(b"x"), source_sha_bytes(b"y"));
        assert_eq!(source_sha_bytes(b"x").len(), 64);
    }

    #[test]
    fn warm_requires_matching_hash_and_outputs() {
        let root = std::env::temp_dir().join(format!("hardmanifest_{}", std::process::id()));
        std::fs::create_dir_all(root.join(".hard")).unwrap();
        let cpp = root.join(".hard/main.cpp");
        let bin = root.join(".hard/main");
        std::fs::write(&cpp, "x").unwrap();
        std::fs::write(&bin, "x").unwrap();
        let env = EnvStamp::new("0.1.0", "rt", "plat", "rel");
        let mut m = sample();
        m.cpp = cpp.to_string_lossy().into_owned();
        m.binary = bin.to_string_lossy().into_owned();
        m.env_fp = env.fingerprint();
        assert!(m.is_warm(&env, &m.merged_hash, &cpp, &bin));
        assert!(!m.is_warm(&env, &"x".repeat(64), &cpp, &bin));
        std::fs::remove_file(&bin).unwrap();
        assert!(!m.is_warm(&env, &m.merged_hash, &cpp, &bin));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn save_and_load_round_trip() {
        let root = std::env::temp_dir().join(format!("hardmanifest_save_{}", std::process::id()));
        let a = sample();
        // manifest path is .hard/build.json inside root
        a.save(&root).unwrap();
        let b = BuildManifest::load(&root).unwrap();
        assert_eq!(b.merged_hash, a.merged_hash);
        assert_eq!(b.modules.len(), a.modules.len());
        assert_eq!(b.cache.hits, a.cache.hits);
        // corrupt file -> None, not a panic
        std::fs::write(BuildManifest::manifest_path(&root), b"{not json").unwrap();
        assert!(BuildManifest::load(&root).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}