//! Deployment commands (M7.2 onwards): the `hard deploy` group.
//!
//! Everything here answers the same question -- "what does this service need
//! in order to run somewhere else?" -- and answers it from the project rather
//! than from a file somebody has to keep in sync. The generators are pure
//! functions over a config so they can be tested without a Docker daemon, an
//! SSH server, or a network; the commands are the thin layer that reads the
//! project, calls a generator, and writes the answer to disk.

use crate::dockerfile::{self, DockerConfig};
use crate::environments::{self, Environment};
use crate::release::{self, DeployConfig, DeployPlan, ReleaseId, RemoteLayout};
use crate::ssh::{SshTarget, SystemSsh};
use crate::{die, find_target, write, Manifest};
use std::path::{Path, PathBuf};

/// `hard deploy <subcommand>`.
pub fn cmd_deploy(args: &[String]) {
    let sub = args.first().map(String::as_str).unwrap_or("help");
    let rest = if args.is_empty() { &[][..] } else { &args[1..] };
    match sub {
        "compose" => cmd_compose(rest),
        "ssh" => cmd_ssh(rest),
        "env" => environments::cmd_env(rest),
        "config" => crate::production::cmd_config(rest),
        "logs" => crate::operations::cmd_logs(rest),
        "status" => crate::operations::cmd_status(rest),
        "releases" => crate::operations::cmd_releases(rest),
        "nginx" => crate::nginx::cmd_nginx(rest),
        "https" => crate::nginx::cmd_https(rest),
        "rollback" => crate::rollback::cmd_rollback(rest),
        "prune" => crate::rollback::cmd_prune(rest),
        "start" | "stop" | "restart" => crate::operations::cmd_lifecycle(sub, rest),
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
         \x20 hard deploy ssh <host>            Build, upload, and restart this service\n\
         \x20 hard deploy env <sub>            List, show, render, or check environments\n\
         \x20 hard deploy config <sub>         Write the systemd unit, or check a host\n\
         \x20 hard deploy logs [host]         The journal for this service (-f to follow)\n\
         \x20 hard deploy status [host]        What is live, what is running, what is kept\n\
         \x20 hard deploy start|stop|restart   Run the service\n\
         \x20 hard deploy releases [host]      The releases on the host, newest first\n\
         \x20 hard deploy nginx [flags]       Write the reverse proxy config\n\
         \x20 hard deploy https [flags]        The command that issues the certificate\n\
         \x20 hard deploy rollback [host]      Go back to the previous release\n\
         \x20 hard deploy prune [host]         Remove old releases (--keep N --yes)\n\
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

// ---------------------------------------------------------------------------
// `hard deploy ssh`
// ---------------------------------------------------------------------------

/// Everything `hard deploy ssh` was told, parsed. Separate from the command so
/// the flag handling can be tested without a host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshArgs {
    pub host: String,
    pub dir: String,
    pub service: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub release_id: Option<String>,
    /// Build the server before uploading it.
    pub build: bool,
    /// Run migrations as part of the deploy.
    pub migrate: bool,
    /// Verify the new release answers before calling the deploy done.
    pub health: bool,
    /// The HTTP path to probe, or `None` for a TCP probe.
    pub health_path: Option<String>,
    pub health_port: u16,
    pub health_timeout: u32,
    /// Whether `--health-timeout` was given; without this a default of 30 would
    /// silently beat the number in the environment.
    pub health_timeout_set: bool,
    /// Replaces the restart command, for a host without systemd.
    pub restart: Option<String>,
    /// The named environment from `hard.toml` to deploy as.
    pub env: Option<String>,
    /// Install the generated systemd unit as part of the deploy.
    pub unit: bool,
    /// Print the plan and stop.
    pub print: bool,
}

impl Default for SshArgs {
    fn default() -> SshArgs {
        SshArgs {
            host: String::new(),
            dir: String::new(),
            service: None,
            user: None,
            port: None,
            release_id: None,
            build: true,
            migrate: true,
            health: true,
            health_path: None,
            health_port: 0,
            health_timeout: 30,
            health_timeout_set: false,
            restart: None,
            env: None,
            unit: false,
            print: false,
        }
    }
}

/// The value after `--flag` in a normalised argument list, or the flag's error.
fn value_at(args: &[String], i: usize, name: &str) -> Result<String, String> {
    args.get(i + 1).cloned().ok_or_else(|| format!("{name} needs a value"))
}

/// `--flag=value` is rewritten to two arguments, so the loop below has one
/// shape to deal with instead of two.
fn normalise(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    for a in args {
        match a.split_once('=') {
            Some((name, value)) if name.starts_with("--") => {
                out.push(name.to_string());
                out.push(value.to_string());
            }
            _ => out.push(a.clone()),
        }
    }
    out
}

/// Parse the arguments after `hard deploy ssh`.
pub fn parse_ssh_args(args: &[String]) -> Result<SshArgs, String> {
    let norm = normalise(args);
    let mut out = SshArgs::default();
    let mut host: Option<String> = None;
    let mut tcp_only = false;
    let mut i = 0;
    while i < norm.len() {
        let a = norm[i].as_str();
        match a {
            "--dir" | "--root" => {
                out.dir = value_at(&norm, i, a)?;
                i += 2;
            }
            "--service" => {
                out.service = Some(value_at(&norm, i, a)?);
                i += 2;
            }
            "--user" => {
                out.user = Some(value_at(&norm, i, a)?);
                i += 2;
            }
            "--release-id" => {
                out.release_id = Some(value_at(&norm, i, a)?);
                i += 2;
            }
            "--restart-cmd" => {
                out.restart = Some(value_at(&norm, i, a)?);
                i += 2;
            }
            "--env" => {
                out.env = Some(value_at(&norm, i, a)?);
                i += 2;
            }
            "--health-path" => {
                out.health_path = Some(value_at(&norm, i, a)?);
                i += 2;
            }
            "--health-port" => {
                let v = value_at(&norm, i, a)?;
                out.health_port = v.parse().map_err(|_| format!("`{v}` is not a port number"))?;
                i += 2;
            }
            "--health-timeout" => {
                let v = value_at(&norm, i, a)?;
                out.health_timeout =
                    v.parse().map_err(|_| format!("`{v}` is not a number of seconds"))?;
                out.health_timeout_set = true;
                i += 2;
            }
            "--port" => {
                let v = value_at(&norm, i, a)?;
                out.port = Some(v.parse().map_err(|_| format!("`{v}` is not an SSH port"))?);
                i += 2;
            }
            "--no-build" => {
                out.build = false;
                i += 1;
            }
            "--no-migrate" => {
                out.migrate = false;
                i += 1;
            }
            "--no-health" => {
                out.health = false;
                i += 1;
            }
            "--unit" => {
                out.unit = true;
                i += 1;
            }
            "--tcp" => {
                tcp_only = true;
                i += 1;
            }
            "--print" | "--dry-run" => {
                out.print = true;
                i += 1;
            }
            other if other.starts_with('-') => {
                return Err(format!("`{other}` is not a deploy flag; run `hard deploy help`"))
            }
            other => {
                if host.is_some() {
                    return Err(format!("`{other}` is a second host; a deploy goes to one"));
                }
                host = Some(other.to_string());
                i += 1;
            }
        }
    }
    if tcp_only {
        out.health_path = None;
    }
    // With `--env` the host may come from the environment, so a missing host is
    // only an error once the environment has been consulted.
    out.host = match host {
        Some(h) => h,
        None if out.env.is_none() => {
            return Err("which host? try `hard deploy ssh root@1.2.3.4`".to_string())
        }
        None => String::new(),
    };
    Ok(out)
}

/// The deploy config for a project: the manifest's port and database, the
/// health path the program actually has, the environment's overrides, and the
/// files that travel with the binary.
///
/// The precedence is deliberate and worth stating once: a flag beats the
/// environment, the environment beats the manifest, and the manifest beats the
/// directory name. A flag is a person saying "this time, this host", so it wins
/// over a file that describes every other day.
pub fn project_deploy_config(
    target: &Path,
    args: &SshArgs,
    env: Option<&Environment>,
) -> Result<DeployConfig, String> {
    let docker = project_docker_config(target);
    let manifest = Manifest::load(Path::new("hard.toml")).ok().flatten();
    let app = manifest
        .as_ref()
        .map(|m| m.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| docker.app_name.clone());
    let env = env.cloned();
    let mut layout = RemoteLayout::new(
        if args.dir.is_empty() { env.as_ref().map(|e| e.dir.as_str()).unwrap_or("") } else { &args.dir },
        &app,
    )?;
    // `Environment::resolve` fills in the project's default unit, so this only
    // changes anything when the environment names a different one.
    if let Some(unit) = &args.service {
        layout = layout.with_service(unit)?;
    } else if let Some(unit) = env.as_ref().map(|e| e.service.clone()) {
        layout = layout.with_service(&unit)?;
    }

    let dialect = manifest
        .as_ref()
        .and_then(|m| m.database.dialect.clone())
        .or_else(|| std::env::var("HS_DEPLOY_DIALECT").ok().filter(|d| !d.is_empty()));

    // A health check that requests a route the program does not have reports a
    // healthy server as broken, so the source decides and the flag only
    // overrides it.
    let health_path = if args.health_path.is_some() {
        args.health_path.clone()
    } else if let Some(path) = env.as_ref().and_then(|e| e.health_path.clone()) {
        Some(path)
    } else if docker.health_path.is_some() {
        Some("/healthz".to_string())
    } else {
        None
    };

    let binary = PathBuf::from(format!(".hard/{}", docker.binary_name));
    let migrations = sql_files(Path::new("migrations"));
    let static_files = files_under(Path::new("static"));
    let migrate_helper = match (dialect.is_some(), args.migrate, migrations.is_empty()) {
        (true, true, false) => Some(crate::migrate::build_helper()),
        _ => None,
    };

    // The manifest ships with the release when there is one, so the far side
    // knows what it is running without a checkout.
    let manifest_path = Path::new("hard.toml");
    let manifest_file = if manifest.is_some() && manifest_path.exists() {
        Some(manifest_path.to_path_buf())
    } else {
        None
    };

    let restart_command = args
        .restart
        .clone()
        .or_else(|| env.as_ref().and_then(|e| e.restart_cmd.clone()));
    let timeout = match (args.health_timeout_set, env.as_ref().and_then(|e| e.health_timeout)) {
        (true, _) => args.health_timeout,
        (false, Some(t)) => t,
        (false, None) => args.health_timeout,
    };

    Ok(DeployConfig {
        app: dockerfile::sanitize_name(&app),
        layout,
        manifest: manifest_file,
        user: args
            .user
            .clone()
            .or_else(|| env.as_ref().and_then(|e| e.user.clone()))
            .unwrap_or_else(|| "root".to_string()),
        binary,
        migrations,
        static_files,
        migrate_helper,
        dialect,
        verify: args.health,
        run_migrations: args.migrate,
        health_path,
        health_port: if args.health_port > 0 {
            args.health_port
        } else {
            env.as_ref().and_then(|e| e.port).unwrap_or(docker.port)
        },
        health_timeout: timeout,
        health_interval: 2,
        restart_command,
        // The env file is written by the plan, with the release id in it, so
        // it is filled in once the id is known.
        env_body: None,
        secrets: env.as_ref().map(|e| e.secrets.clone()).unwrap_or_default(),
        // Installed only when asked: a deploy does not overwrite a unit
        // somebody edited by hand.
        unit_body: None,
    })
}

/// The `*.sql` files in a migrations directory, in version order.
fn sql_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "sql").unwrap_or(false))
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

/// Every file under a directory, recursively, sorted. Static assets are
/// uploaded one by one rather than as a directory, so the remote side ends up
/// with the same tree even without `rsync`.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_files(dir, &mut out);
    out.sort();
    out
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// A digest of everything that is about to be uploaded, so the release id says
/// which bytes went out and two deploys of the same build are visibly the same
/// release.
pub fn artifact_digest(cfg: &DeployConfig) -> String {
    let mut files: Vec<&PathBuf> = vec![&cfg.binary];
    files.extend(cfg.migrations.iter());
    files.extend(cfg.static_files.iter());
    files.extend(cfg.manifest.iter());
    let mut acc = String::new();
    for path in files {
        acc.push_str(&path.to_string_lossy());
        acc.push('\n');
        if let Ok(bytes) = std::fs::read(path) {
            acc.push_str(&hs_compiler::sha256::hex(&bytes));
        }
        acc.push('\n');
    }
    hs_compiler::sha256::hex(acc.as_bytes())
}

/// The release id for a deploy: the version, the time, and the digest.
pub fn release_id_for(cfg: &DeployConfig, args: &SshArgs) -> Result<ReleaseId, String> {
    if let Some(id) = &args.release_id {
        return ReleaseId::parse(id);
    }
    let version = Manifest::load(Path::new("hard.toml"))
        .ok()
        .flatten()
        .map(|m| m.version)
        .unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(ReleaseId::new(&version, now, &artifact_digest(cfg)))
}

/// `hard deploy ssh <host>`: build, upload, migrate, activate, restart, verify.
fn cmd_ssh(args: &[String]) {
    let parsed = match parse_ssh_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("hard deploy ssh: {e}");
            std::process::exit(2);
        }
    };
    // The local source file, for the build; the remote target, for the deploy.
    let (source, _) = find_target(args);
    // An environment, when one was asked for, is resolved before the host so
    // that its host can be the host.
    let (project, manifest) = environments::project_manifest();
    let env = match &parsed.env {
        Some(name) => match environments::resolve(&project, &manifest, name) {
            Ok(e) => Some(e),
            Err(e) => die(&e),
        },
        None => None,
    };
    let mut parsed = parsed;
    if parsed.host.is_empty() {
        match env.as_ref().and_then(|e| e.host.clone()) {
            Some(h) => parsed.host = h,
            None => die(&format!(
                "environment `{}` has no host; pass one: `hard deploy ssh --env {} <host>`",
                parsed.env.clone().unwrap_or_default(),
                parsed.env.clone().unwrap_or_default()
            )),
        }
    }
    let mut target = match SshTarget::parse(&parsed.host) {
        Ok(t) => t,
        Err(e) => die(&e),
    };
    if let Some(user) = &parsed.user {
        target.user = user.clone();
    } else if let Some(user) = env.as_ref().and_then(|e| e.user.clone()) {
        target.user = user.clone();
    }
    if let Some(port) = parsed.port {
        target.port = port;
    }
    // The account being deployed to decides whether the restart needs sudo, so
    // it has to reach the config even when it came from `user@host` rather than
    // from `--user`.
    parsed.user = Some(target.user.clone());

    if parsed.build {
        // The build is the same portable `hard build` a developer runs, not the
        // `-march=native` release path: this binary is going to execute on a
        // machine that is not this one, where a CPU-specific instruction is a
        // SIGILL before main.
        let opts = crate::build_options(false, 1);
        match crate::incremental_build(&source, &opts, &hs_compiler::warn::WarningPolicy::default()) {
            Ok(_) => {}
            Err(diags) => crate::report(&diags),
        }
    }

    let mut cfg = match project_deploy_config(&source, &parsed, env.as_ref()) {
        Ok(c) => c,
        Err(e) => die(&e),
    };
    let id = match release_id_for(&cfg, &parsed) {
        Ok(i) => i,
        Err(e) => die(&e),
    };
    // The env file names the release, so it can only be rendered once the id
    // is known.
    if let Some(env) = &env {
        cfg.env_body = Some(environments::render_env_file(env, &id.to_string()));
    }
    if parsed.unit {
        // A deploy that owns the unit writes it; one that does not, leaves the
        // file alone even if it is wrong, because somebody may have edited it.
        let unit = crate::production::UnitConfig::new(&cfg.app, &cfg.layout, Some(cfg.user.clone()));
        cfg.unit_body = Some(unit.install_command());
    }
    let plan = DeployPlan::build(&cfg, &id);

    if parsed.print {
        print!("{}", plan.render());
        return;
    }
    if !cfg.binary.exists() {
        die(&format!(
            "{} does not exist; run `hard build` first, or drop --no-build",
            cfg.binary.display()
        ));
    }

    println!("deploying {} to {}", cfg.app, target.label());
    println!("  release {}", id);
    if let Some(e) = &env {
        println!("  env     {}", e.name);
    }
    println!("  root    {}", cfg.layout.root);
    println!("  {} files, {} service", plan.upload_count(), cfg.layout.service);
    let mut ssh = SystemSsh::new();
    let mut sleeper = release::RealSleeper;
    match release::execute(&mut ssh, &mut sleeper, &target, &cfg, &plan) {
        Ok(report) => {
            println!("  {} files, {} bytes", report.files, report.bytes);
            println!("  current {}", report.current);
            println!("deployed {} to {}", id, target.label());
        }
        Err(e) => {
            eprintln!("hard deploy: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// A scratch directory that removes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir()
                .join(format!("hs-deploy-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("a scratch directory");
            Scratch(dir)
        }

        fn file(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).expect("a parent directory");
            std::fs::write(&path, text).expect("a file");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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

    #[test]
    fn a_deploy_needs_a_host_and_nothing_else() {
        let args = parse_ssh_args(&argv(&["root@1.2.3.4"])).expect("a host is enough");
        assert_eq!(args.host, "root@1.2.3.4", "the host");
        assert!(args.build, "it builds by default");
        assert!(args.migrate, "and migrates");
        assert!(args.health, "and checks the new release answers");
        assert!(!args.print, "but does not deploy unless told to");
        let err = parse_ssh_args(&[]).unwrap_err();
        assert!(err.contains("which host"), "{err}");
        let err = parse_ssh_args(&argv(&["root@h", "root@other"])).unwrap_err();
        assert!(err.contains("second host"), "{err}");
        let err = parse_ssh_args(&argv(&["root@h", "--nonsense"])).unwrap_err();
        assert!(err.contains("not a deploy flag"), "{err}");
    }

    #[test]
    fn every_deploy_flag_reads_in_both_spellings() {
        let space = parse_ssh_args(&argv(&[
            "deploy@host",
            "--dir",
            "/opt/svc",
            "--service",
            "web.service",
            "--user",
            "deploy",
            "--port",
            "2222",
            "--health-path",
            "/live",
            "--health-port",
            "8080",
            "--health-timeout",
            "90",
            "--release-id",
            "v1",
            "--restart-cmd",
            "rc-service web restart",
            "--no-build",
            "--no-migrate",
            "--no-health",
        ]))
        .expect("all of them");
        let equals = parse_ssh_args(&argv(&[
            "deploy@host",
            "--dir=/opt/svc",
            "--service=web.service",
            "--user=deploy",
            "--port=2222",
            "--health-path=/live",
            "--health-port=8080",
            "--health-timeout=90",
            "--release-id=v1",
            "--restart-cmd=rc-service web restart",
            "--no-build",
            "--no-migrate",
            "--no-health",
        ]))
        .expect("all of them");
        assert_eq!(space, equals, "the two spellings are the same flag");
        assert_eq!(space.dir, "/opt/svc", "--dir");
        assert_eq!(space.service.as_deref(), Some("web.service"), "--service");
        assert_eq!(space.user.as_deref(), Some("deploy"), "--user");
        assert_eq!(space.port, Some(2222), "--port");
        assert_eq!(space.health_path.as_deref(), Some("/live"), "--health-path");
        assert_eq!(space.health_port, 8080, "--health-port");
        assert_eq!(space.health_timeout, 90, "--health-timeout");
        assert_eq!(space.release_id.as_deref(), Some("v1"), "--release-id");
        assert_eq!(space.restart.as_deref(), Some("rc-service web restart"), "--restart-cmd");
        assert!(!space.build && !space.migrate && !space.health, "and the negations");
    }

    #[test]
    fn a_flag_with_a_bad_value_says_so_instead_of_guessing() {
        let err = parse_ssh_args(&argv(&["root@h", "--health-port", "http"])).unwrap_err();
        assert!(err.contains("not a port number"), "{err}");
        let err = parse_ssh_args(&argv(&["root@h", "--health-timeout", "soon"])).unwrap_err();
        assert!(err.contains("not a number of seconds"), "{err}");
        let err = parse_ssh_args(&argv(&["root@h", "--port", "70000"])).unwrap_err();
        assert!(err.contains("not an SSH port"), "{err}");
        let err = parse_ssh_args(&argv(&["root@h", "--dir"])).unwrap_err();
        assert!(err.contains("needs a value"), "{err}");
    }

    #[test]
    fn a_tcp_probe_drops_the_http_path() {
        let args = parse_ssh_args(&argv(&["root@h", "--health-path", "/live", "--tcp"])).unwrap();
        assert_eq!(args.health_path, None, "--tcp wins, because a TCP check has no path");
    }

    #[test]
    fn migrations_and_static_files_are_collected_in_order() {
        let s = Scratch::new("files");
        s.file("migrations/002_two.sql", "");
        s.file("migrations/001_one.sql", "");
        s.file("migrations/README.md", "");
        s.file("static/app.css", "");
        s.file("static/img/logo.svg", "");
        let sqls = sql_files(&s.0.join("migrations"));
        let names: Vec<String> =
            sqls.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["001_one.sql", "002_two.sql"], "SQL only, in version order");
        let static_files = files_under(&s.0.join("static"));
        let rel: Vec<String> = static_files
            .iter()
            .map(|p| p.strip_prefix(&s.0).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(rel, vec!["static/app.css", "static/img/logo.svg"], "recursively, sorted");
        assert!(sql_files(&s.0.join("nothing-here")).is_empty(), "a missing directory is not an error");
        assert!(files_under(&s.0.join("nothing-here")).is_empty(), "for either one");
    }

    #[test]
    fn the_digest_covers_the_binary_and_everything_that_travels_with_it() {
        let s = Scratch::new("digest");
        let binary = s.file("server", "binary one");
        let migration = s.file("001.sql", "create table t (id int)");
        let asset = s.file("app.css", "body {}");
        let mut cfg = release::DeployConfig {
            binary,
            migrations: vec![migration.clone()],
            static_files: vec![asset.clone()],
            ..release::DeployConfig::default()
        };
        let first = artifact_digest(&cfg);
        assert_eq!(first.len(), 64, "a sha256 in hex");
        // The same artifacts, the same release: a rebuild that changed nothing
        // is the same release, and the deploy says so.
        assert_eq!(artifact_digest(&cfg), first, "the same files hash the same");
        std::fs::write(&migration, "create table t (id int, name text)").expect("a change");
        let after = artifact_digest(&cfg);
        assert_ne!(after, first, "a changed migration changes the release");
        cfg.static_files.clear();
        assert_ne!(artifact_digest(&cfg), after, "and so does dropping a file");
        let without_manifest = artifact_digest(&cfg);
        cfg.manifest = Some(s.file("hard.toml", "name = \"t\""));
        let with_manifest = artifact_digest(&cfg);
        assert_ne!(with_manifest, without_manifest, "and so does shipping one");
        std::fs::write(cfg.manifest.as_ref().unwrap(), "name = \"u\"").expect("a change");
        assert_ne!(artifact_digest(&cfg), with_manifest, "a changed manifest changes it too");
    }

    #[test]
    fn a_release_id_names_the_version_the_time_and_the_bytes() {
        let s = Scratch::new("releaseid");
        let cfg = release::DeployConfig {
            binary: s.file("server", "binary"),
            ..release::DeployConfig::default()
        };
        let args = SshArgs { host: "root@h".to_string(), ..SshArgs::default() };
        let id = release_id_for(&cfg, &args).expect("an id");
        assert!(id.to_string().ends_with(&artifact_digest(&cfg)[..8]), "{} names the bytes", id);
        // In this project there is no manifest, so no version: the stamp and the
        // digest still make it unique.
        assert!(id.to_string().starts_with("20"), "{}", id);
        let named = SshArgs {
            host: "root@h".to_string(),
            release_id: Some("v1.2.3".to_string()),
            ..SshArgs::default()
        };
        assert_eq!(release_id_for(&cfg, &named).unwrap().to_string(), "v1.2.3", "an operator's id is used as given");
        let bad = SshArgs {
            host: "root@h".to_string(),
            release_id: Some("../../etc/passwd".to_string()),
            ..SshArgs::default()
        };
        assert!(release_id_for(&cfg, &bad).is_err(), "and it is checked");
    }

    #[test]
    fn a_deploy_config_names_the_service_after_the_project() {
        let s = Scratch::new("config");
        let target = s.file("main.hard", "GET \"/\" :: {\n    <- 1\n}\n");
        let args = SshArgs { host: "root@h".to_string(), ..SshArgs::default() };
        let cfg = project_deploy_config(&target, &args, None).expect("a config");
        // With no manifest the name comes from the working directory, so assert
        // the relationship rather than a literal: the root is the name under
        // /srv, and the unit is the name with `.service`.
        let name = cfg.layout.service.trim_end_matches(".service").to_string();
        assert_eq!(cfg.layout.root, format!("/srv/{name}"), "the default root follows the project name");
        assert_eq!(cfg.health_path, None, "no /healthz route, so a TCP probe");
        assert!(cfg.migrations.is_empty(), "no migrations in a scratch directory");
        let args = SshArgs {
            host: "root@h".to_string(),
            dir: "/opt/web".to_string(),
            service: Some("web.service".to_string()),
            user: Some("deploy".to_string()),
            health_path: Some("/live".to_string()),
            health_port: 8080,
            ..SshArgs::default()
        };
        let cfg = project_deploy_config(&target, &args, None).expect("a config");
        assert_eq!(cfg.layout.root, "/opt/web", "--dir");
        assert_eq!(cfg.layout.service, "web.service", "--service");
        assert_eq!(cfg.user, "deploy", "--user, which decides the sudo prefix");
        assert_eq!(cfg.health_path.as_deref(), Some("/live"), "--health-path overrides the source");
        assert_eq!(cfg.health_port, 8080, "--health-port");
        let bad = SshArgs { host: "root@h".to_string(), dir: "/opt/web; rm -rf /".to_string(), ..SshArgs::default() };
        assert!(project_deploy_config(&target, &bad, None).is_err(), "and a root that is a shell problem is refused");
    }

    fn environment() -> Environment {
        Environment {
            name: "production".to_string(),
            host: Some("deploy@app.example.com".to_string()),
            dir: "/srv/api".to_string(),
            service: "api.service".to_string(),
            user: Some("deploy".to_string()),
            port: Some(8080),
            health_path: Some("/live".to_string()),
            health_timeout: Some(90),
            restart_cmd: Some("rc-service api restart".to_string()),
            domain: Some("api.example.com".to_string()),
            tls_email: Some("ops@example.com".to_string()),
            vars: vec![("RUST_LOG".to_string(), "info".to_string())],
            secrets: vec!["DATABASE_URL".to_string()],
        }
    }

    #[test]
    fn an_environment_supplies_what_a_flag_does_not() {
        let s = Scratch::new("envcfg");
        let target = s.file("main.hard", "GET \"/\" :: {\n    <- 1\n}\n");
        let env = environment();
        let args = SshArgs { host: String::new(), env: Some("production".to_string()), ..SshArgs::default() };
        let cfg = project_deploy_config(&target, &args, Some(&env)).expect("a config");
        assert_eq!(cfg.layout.root, "/srv/api", "the environment's root");
        assert_eq!(cfg.layout.service, "api.service", "and its unit");
        assert_eq!(cfg.user, "deploy", "and the account it deploys as");
        assert_eq!(cfg.health_port, 8080, "and the port it probes");
        assert_eq!(cfg.health_path.as_deref(), Some("/live"), "and the path");
        assert_eq!(cfg.health_timeout, 90, "and how long it waits");
        assert_eq!(cfg.restart_command.as_deref(), Some("rc-service api restart"), "and how it restarts");
        assert_eq!(cfg.secrets, vec!["DATABASE_URL".to_string()], "and which secrets the host must have");
        // A flag still wins: a person on the command line outranks a file.
        let override_args = SshArgs {
            dir: "/opt/other".to_string(),
            health_port: 9000,
            health_timeout: 5,
            health_timeout_set: true,
            ..args.clone()
        };
        let cfg = project_deploy_config(&target, &override_args, Some(&env)).expect("a config");
        assert_eq!(cfg.layout.root, "/opt/other", "--dir beats the environment");
        assert_eq!(cfg.health_port, 9000, "--health-port beats it");
        assert_eq!(cfg.health_timeout, 5, "and so does --health-timeout");
    }

    #[test]
    fn a_deploy_with_no_environment_has_nothing_to_configure() {
        let s = Scratch::new("noenv");
        let target = s.file("main.hard", "GET \"/\" :: {\n    <- 1\n}\n");
        let args = SshArgs { host: "root@h".to_string(), ..SshArgs::default() };
        let cfg = project_deploy_config(&target, &args, None).expect("a config");
        assert!(cfg.env_body.is_none(), "no env file to write");
        assert!(cfg.secrets.is_empty(), "and no secrets to check");
        let plan = DeployPlan::build(&cfg, &ReleaseId::parse("v1").unwrap());
        assert!(
            !plan.steps.iter().any(|s| s.kind == release::StepKind::Config),
            "and no config step"
        );
    }

    #[test]
    fn a_deploy_with_an_environment_writes_it_and_checks_its_secrets() {
        let s = Scratch::new("envplan");
        let target = s.file("main.hard", "GET \"/\" :: {\n    <- 1\n}\n");
        let env = environment();
        let args = SshArgs { host: String::new(), env: Some("production".to_string()), ..SshArgs::default() };
        let mut cfg = project_deploy_config(&target, &args, Some(&env)).expect("a config");
        let id = ReleaseId::parse("1.0.0-20250915T070530Z-abcdef12").unwrap();
        cfg.env_body = Some(environments::render_env_file(&env, &id.to_string()));
        let plan = DeployPlan::build(&cfg, &id);
        let config_steps: Vec<&release::DeployStep> =
            plan.steps.iter().filter(|s| s.kind == release::StepKind::Config).collect();
        assert_eq!(config_steps.len(), 2, "the file and the secret check: {}", plan.render());
        let write = &config_steps[0].command;
        assert!(write.contains("/srv/api/shared/env"), "into shared, not into the release: {write}");
        assert!(write.contains("RUST_LOG=\"info\""), "with the variables: {write}");
        assert!(write.contains(&format!("HS_RELEASE={id}")), "and the release: {write}");
        assert!(!write.contains("DATABASE_URL="), "and no secret: {write}");
        let check = &config_steps[1].command;
        assert!(check.contains("/srv/api/shared/env.secrets"), "the operator's file: {check}");
        assert!(check.contains("^DATABASE_URL="), "checked by name: {check}");
        // Configuration happens before the migration, which may read it, and
        // long before the restart.
        let kinds: Vec<release::StepKind> = plan.steps.iter().map(|s| s.kind).collect();
        let first_config = kinds.iter().position(|k| *k == release::StepKind::Config).expect("a config step");
        let activate = kinds.iter().position(|k| *k == release::StepKind::Activate).expect("activation");
        assert!(first_config < activate, "the host is configured before it goes live");
    }

    #[test]
    fn an_environment_can_be_what_names_the_host() {
        let args = parse_ssh_args(&argv(&["--env", "production"])).expect("an environment can carry the host");
        assert_eq!(args.host, "", "no host on the command line");
        assert_eq!(args.env.as_deref(), Some("production"), "and the environment to take it from");
        let err = parse_ssh_args(&argv(&["--no-build"])).unwrap_err();
        assert!(err.contains("which host"), "without an environment it is still required: {err}");
    }
}
