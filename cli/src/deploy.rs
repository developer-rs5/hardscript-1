//! Deployment commands (M7.2 onwards): the `hard deploy` group.
//!
//! Everything here answers the same question -- "what does this service need
//! in order to run somewhere else?" -- and answers it from the project rather
//! than from a file somebody has to keep in sync. The generators are pure
//! functions over a config so they can be tested without a Docker daemon, an
//! SSH server, or a network; the commands are the thin layer that reads the
//! project, calls a generator, and writes the answer to disk.

use crate::dockerfile::{self, DockerConfig};
use crate::{die, find_target, write, Manifest};
use std::path::{Path, PathBuf};

/// `hard deploy <subcommand>`.
pub fn cmd_deploy(args: &[String]) {
    let sub = args.first().map(String::as_str).unwrap_or("help");
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    match sub {
        "compose" => cmd_compose(rest),
        "help" | "-h" | "--help" => help(),
        other => {
            eprintln!("hard deploy: unknown subcommand '{other}'");
            eprintln!("run `hard deploy help` for the list.");
            std::process::exit(2);
        }
    }
}

fn help() {
    println!(
        "hard deploy -- put a service somewhere else\n\
         \n\
         \x20 hard deploy compose [file]        Write docker-compose.yml for this project\n\
         \x20 hard deploy help                 Show this help"
    );
}

/// The Docker config for the project the target belongs to: the manifest name
/// and port when there is a manifest, the port from the source when there is
/// not, and a health check only when the program actually has one.
pub fn project_docker_config(target: &Path) -> DockerConfig {
    let cpp_name = target
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "main".to_string());
    let mut cfg = DockerConfig {
        app_name: std::env::current_dir()
            .ok()
            .and_then(|d| d.file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "app".to_string()),
        cpp_name,
        ..DockerConfig::default()
    };
    if let Ok(Some(man)) = Manifest::load(Path::new("hard.toml")) {
        if !man.name.is_empty() {
            cfg.app_name = man.name.clone();
        }
        if let Some(port) = man.server.port {
            cfg.port = port;
        }
    }
    if let Ok(src) = std::fs::read_to_string(target) {
        if let Ok(prog) = hs_compiler::frontend(&src, target.to_string_lossy().into_owned()) {
            if !program_has_healthz(&prog) {
                cfg.health_path = None;
            }
        }
    }
    // Overrides for the images, because a pinned base is a decision an
    // operator makes and a mirror is a fact of a network.
    for (var, slot) in [("HS_ALPINE", 0usize), ("HS_RUNTIME_IMAGE", 1)] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                if slot == 0 {
                    cfg.builder_image = v.clone();
                    cfg.runtime_image = v;
                } else {
                    cfg.runtime_image = v;
                }
            }
        }
    }
    if let Ok(v) = std::env::var("HS_DOCKER_HEALTH_PATH") {
        if !v.is_empty() {
            cfg.health_path = Some(v);
        }
    }
    cfg
}

/// Whether a program answers `GET /healthz`. Only the metrics module installs
/// that route, and a health check that requests it from a program without it
/// would report a perfectly healthy server as unhealthy.
pub fn program_has_healthz(prog: &hs_compiler::ast::Program) -> bool {
    use hs_compiler::ast::*;
    for st in &prog.stmts {
        if let Stmt::Route(r) = st {
            if r.method == "GET" && r.path == "/healthz" {
                return true;
            }
        }
    }
    false
}

/// `hard deploy compose`: the compose file for this project, including a
/// database service when the manifest asks for one.
fn cmd_compose(args: &[String]) {
    let (target, _) = find_target(args);
    let out = match first_flag_value(args, "--out") {
        Some(v) => PathBuf::from(v),
        None => PathBuf::from("docker-compose.yml"),
    };
    let print_only = args.iter().any(|a| a == "--print");
    let cfg = project_docker_config(&target);

    let mut compose = crate::compose::Compose {
        project_name: dockerfile::sanitize_name(&cfg.app_name),
        ..Default::default()
    };
    let mut app = crate::compose::app_service(&cfg);
    let mut volumes = Vec::new();
    let networks = vec!["app".to_string()];

    // A PostgreSQL manifest gets a database next to the app. SQLite is a file
    // in the working directory, so it needs a volume rather than a container.
    let dialect = Manifest::load(Path::new("hard.toml"))
        .ok()
        .flatten()
        .and_then(|m| m.database.dialect);
    if let Some(d) = dialect.as_deref() {
        if d.eq_ignore_ascii_case("postgres") {
            let db = crate::compose::postgres_service(&cfg, &cfg.app_name, &cfg.app_name);
            app.depends_on.push("db".to_string());
            volumes.push("db-data".to_string());
            compose.services.push(db);
        } else {
            let name = dockerfile::sanitize_name(&cfg.app_name);
            app.volumes.retain(|v| !v.ends_with("/migrations:ro"));
            app.volumes.push(format!("./{name}.db:/srv/{name}/{name}.db"));
        }
    }
    compose.services.insert(0, app);
    compose.volumes = volumes;
    compose.networks = networks;

    let text = compose.render();
    if print_only {
        print!("{text}");
        return;
    }
    write(&out, &text);
    println!("wrote {}", out.display());
    println!("up:   docker compose -f {} up -d --build", out.display());
    println!("down: docker compose -f {} down", out.display());
    let _ = die;
}

/// `--flag value` or `--flag=value`, first one wins, value removed. The
/// codebase's `take_flag` is in the migrate module and takes the last value;
/// this one is here so the deploy commands do not have to reach into it.
fn first_flag_value(args: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            return args.get(i + 1).cloned();
        }
        if let Some(v) = args[i].strip_prefix(&prefix) {
            return Some(v.to_string());
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flag_reads_both_spellings() {
        let v: Vec<String> = ["--out", "a.yml"].iter().map(|s| s.to_string()).collect();
        assert_eq!(first_flag_value(&v, "--out").as_deref(), Some("a.yml"), "space form");
        let v: Vec<String> = ["--out=b.yml"].iter().map(|s| s.to_string()).collect();
        assert_eq!(first_flag_value(&v, "--out").as_deref(), Some("b.yml"), "equals form");
        let v: Vec<String> = ["main.hard"].iter().map(|s| s.to_string()).collect();
        assert_eq!(first_flag_value(&v, "--out"), None, "absent");
    }

    #[test]
    fn a_healthz_route_is_recognised() {
        let with = hs_compiler::frontend(
            "GET \"/healthz\" :: {\n    <- { ok: true }\n}\n",
            "t.hard".to_string(),
        )
        .unwrap();
        assert!(program_has_healthz(&with), "an explicit healthz route counts");
        let without =
            hs_compiler::frontend("GET \"/\" :: {\n    <- 1\n}\n", "t.hard".to_string()).unwrap();
        assert!(!program_has_healthz(&without), "a program without one does not");
        let metrics = hs_compiler::frontend(
            "GET \"/x\" :: {\n    metrics.incr(\"a\")\n    <- 1\n}\n",
            "t.hard".to_string(),
        )
        .unwrap();
        // The runtime installs it, but it is not a route in the source: the
        // health check is asked for by the user, or it is not.
        assert!(!program_has_healthz(&metrics), "a metrics program has no source route for it");
    }
}
