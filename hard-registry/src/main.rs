//! The `hard-registry` binary.
//!
//! ```text
//! hard-registry serve    [--addr 127.0.0.1:8484] [--data ./data] [--open]
//! hard-registry migrate  [--data ./data]
//! hard-registry stats    [--data ./data]
//! hard-registry verify   [--data ./data]
//! hard-registry search   <query> [--data ./data]
//! ```
//!
//! Nothing here deploys anything: `serve` binds a plain HTTP listener on the
//! loopback interface unless told otherwise, which is what local development,
//! CI and the QA suite need. TLS, reverse proxies and orchestration are the
//! job of whatever sits in front of a registry.

use hard_registry::{app::Config, App, Router, Server, SigningKey, SqliteStore, Store, DEFAULT_ADDR};
use std::path::PathBuf;
use std::sync::Arc;

const USAGE: &str = "\
hard-registry — the HardScript package registry

USAGE:
  hard-registry serve   [--addr <host:port>] [--data <dir>] [--open] [--max-archive <bytes>]
  hard-registry migrate [--data <dir>]
  hard-registry stats   [--data <dir>]
  hard-registry verify  [--data <dir>]
  hard-registry search  <query> [--tag <t>]... [--limit <n>] [--data <dir>]

OPTIONS:
  --addr <host:port>   listen address (default 127.0.0.1:8484)
  --data <dir>         registry data directory (default ./registry-data)
  --open               allow unauthenticated publishes (local development)
  --no-sign            store publishes without an Ed25519 signature
  --max-archive <n>    reject archives larger than n bytes
  --limit <n>          maximum number of search results
  --help               show this help
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print!("{USAGE}");
        return;
    }
    let cmd = args[0].clone();
    let rest = &args[1..];
    let code = match cmd.as_str() {
        "serve" => serve(rest),
        "migrate" => migrate(rest),
        "stats" => stats(rest),
        "verify" => verify(rest),
        "search" => search_cmd(rest),
        "--help" | "-h" | "help" => {
            print!("{USAGE}");
            0
        }
        other => {
            eprintln!("hard-registry: unknown command '{other}'");
            eprint!("{USAGE}");
            2
        }
    };
    std::process::exit(code);
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            return args.get(i + 1).cloned();
        }
        if let Some(v) = args[i].strip_prefix(&format!("{name}=")) {
            return Some(v.to_string());
        }
        i += 1;
    }
    None
}

fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

struct Options {
    data: PathBuf,
    addr: String,
    open: bool,
    sign: bool,
    max_archive: Option<usize>,
    limit: usize,
    tags: Vec<String>,
    query: Option<String>,
}

fn parse_options(args: &[String]) -> Options {
    Options {
        data: flag_value(args, "--data")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("registry-data")),
        addr: flag_value(args, "--addr").unwrap_or_else(|| DEFAULT_ADDR.to_string()),
        open: has_flag(args, "--open"),
        sign: !has_flag(args, "--no-sign"),
        max_archive: flag_value(args, "--max-archive").and_then(|v| v.parse().ok()),
        limit: flag_value(args, "--limit")
            .and_then(|v| v.parse().ok())
            .unwrap_or(20),
        tags: args
            .iter()
            .enumerate()
            .filter(|(_, a)| a.as_str() == "--tag")
            .filter_map(|(i, _)| args.get(i + 1).cloned())
            .collect(),
        query: args.iter().find(|a| !a.starts_with("--")).cloned(),
    }
}

fn build_app(opts: &Options) -> Result<App, String> {
    let mut config = if opts.open {
        Config {
            require_auth: false,
            open_publish: true,
            ..Config::default()
        }
    } else {
        Config::default()
    };
    config.sign_publishes = opts.sign;
    let store = SqliteStore::open(opts.data.join("registry.db"))
        .map_err(|e| format!("cannot open the registry database: {e}"))?;
    let mut archives = hard_registry::ArchiveStore::new(&opts.data);
    if let Some(n) = opts.max_archive {
        archives = archives.with_max_bytes(n);
    }
    Ok(App::new(
        Arc::new(store),
        archives,
        SigningKey::from_env(),
        config,
    ))
}

fn serve(args: &[String]) -> i32 {
    let opts = parse_options(args);
    let app = match build_app(&opts) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("hard-registry: {e}");
            return 1;
        }
    };
    let stats = app.stats().unwrap_or_default();
    let handler = Router::new(Arc::new(app)).into_handler();
    let server = match Server::bind(&opts.addr, handler) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("hard-registry: cannot bind {}: {e}", opts.addr);
            return 1;
        }
    };
    println!("hardscript-registry listening on {}", server.base_url());
    println!("  data:     {}", opts.data.display());
    println!("  backend:  sqlite");
    println!(
        "  packages: {} ({} versions, {} downloads)",
        stats.packages, stats.versions, stats.downloads
    );
    println!(
        "  auth:     {}",
        if opts.open { "open (publishing without a token)" } else { "required" }
    );
    println!(
        "  signing:  {}",
        if opts.sign { "ed25519" } else { "disabled" }
    );
    if let Err(e) = server.serve_forever() {
        eprintln!("hard-registry: {e}");
        return 1;
    }
    0
}

fn migrate(args: &[String]) -> i32 {
    let opts = parse_options(args);
    let path = opts.data.join("registry.db");
    match SqliteStore::open(&path) {
        Ok(store) => {
            println!("migrated {}", path.display());
            println!("schema v{}", hard_registry::sqlite::SCHEMA_VERSION);
            let s = store.stats().unwrap_or_default();
            println!("packages: {}, versions: {}", s.packages, s.versions);
            0
        }
        Err(e) => {
            eprintln!("hard-registry: {e}");
            1
        }
    }
}

fn stats(args: &[String]) -> i32 {
    let opts = parse_options(args);
    let app = match build_app(&opts) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("hard-registry: {e}");
            return 1;
        }
    };
    let s = app.stats().unwrap_or_default();
    println!("backend:      sqlite");
    println!("data:         {}", opts.data.display());
    println!("packages:     {}", s.packages);
    println!("versions:     {}", s.versions);
    println!("yanked:       {}", s.yanked);
    println!("users:        {}", s.users);
    println!("tokens:       {}", s.tokens);
    println!("signed:       {}", s.signed);
    println!("downloads:    {}", s.downloads);
    println!("archive bytes: {}", s.archive_bytes);
    println!("change seq:   {}", app.seq().unwrap_or(0));
    0
}

fn verify(args: &[String]) -> i32 {
    let opts = parse_options(args);
    let app = match build_app(&opts) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("hard-registry: {e}");
            return 1;
        }
    };
    let outcomes = app.verify_all();
    let bad: Vec<&hard_registry::VerifyOutcome> = outcomes.iter().filter(|o| !o.ok()).collect();
    for o in &bad {
        println!("FAIL {}@{}", o.name, o.version);
        for p in &o.problems {
            println!("  {p}");
        }
    }
    println!("checked {} version(s): {} ok, {} failed", outcomes.len(), outcomes.len() - bad.len(), bad.len());
    if bad.is_empty() {
        0
    } else {
        1
    }
}

fn search_cmd(args: &[String]) -> i32 {
    let opts = parse_options(args);
    let Some(query) = opts.query.clone() else {
        eprintln!("hard-registry search: missing query");
        return 2;
    };
    let app = match build_app(&opts) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("hard-registry: {e}");
            return 1;
        }
    };
    let q = hard_registry::search::Query::parse(&query, &opts.tags, None);
    let all = hard_registry::search::run(app.store.as_ref(), &q);
    if all.is_empty() {
        println!("no packages matching '{query}'");
        return 0;
    }
    for h in all.into_iter().take(opts.limit.max(1)) {
        let version = h.version.clone().unwrap_or_else(|| "-".to_string());
        let desc = h.description.clone().unwrap_or_default();
        let tags = if h.tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", h.tags.join(", "))
        };
        println!(
            "{name} {version}  {desc}{tags}  ({downloads} downloads, score {score})",
            name = h.name,
            downloads = h.downloads,
            score = h.score
        );
    }
    0
}
