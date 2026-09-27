//! `hard deploy env`: the named environments a project deploys to (M7.4).
//!
//! Three environments, three hosts, three ports, and a set of variables that
//! differ: that is what a service has the moment it is more than a demo, and
//! leaving it in a deploy command line means the differences live in somebody's
//! shell history. An environment puts them in `hard.toml` instead, where they
//! are reviewable, and the deploy reads them.
//!
//! The one thing deliberately *not* in `hard.toml` is a secret's value. An
//! environment names its secrets -- `secrets = ["DATABASE_URL"]` -- which is
//! enough to tell an operator what a host is missing, and enough for a deploy
//! to fail with a sentence naming the variable, without the value ever being
//! written to a file the tool controls.

use crate::release::sh_quote;
use hs_pm::manifest::{EnvironmentConfig, Manifest};

/// The environment a deploy resolves to: the manifest's table with the
/// project's own defaults filled in, so nothing downstream has to guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    pub name: String,
    pub host: Option<String>,
    pub dir: String,
    pub service: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub health_path: Option<String>,
    pub health_timeout: Option<u32>,
    pub restart_cmd: Option<String>,
    pub vars: Vec<(String, String)>,
    pub secrets: Vec<String>,
}

impl Environment {
    /// Fill in the defaults a project without a manifest would have used.
    pub fn resolve(project: &str, name: &str, cfg: &EnvironmentConfig) -> Environment {
        let app = crate::dockerfile::sanitize_name(project);
        Environment {
            name: name.to_string(),
            host: cfg.host.clone(),
            dir: cfg.dir.clone().unwrap_or_else(|| format!("/srv/{app}")),
            service: cfg.service.clone().unwrap_or_else(|| format!("{app}.service")),
            user: cfg.user.clone(),
            port: cfg.port,
            health_path: cfg.health_path.clone(),
            health_timeout: cfg.health_timeout,
            restart_cmd: cfg.restart_cmd.clone(),
            vars: cfg.vars.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            secrets: cfg.secrets.clone(),
        }
    }
}

/// The environments a manifest declares, in name order.
pub fn names(manifest: &Manifest) -> Vec<String> {
    manifest.environments.keys().cloned().collect()
}

/// One environment, or an error that says which ones exist.
pub fn resolve(project: &str, manifest: &Manifest, name: &str) -> Result<Environment, String> {
    let cfg = manifest
        .environments
        .get(name)
        .ok_or_else(|| match names(manifest).is_empty() {
            true => format!("no environments in hard.toml: add [env.{name}]"),
            false => format!("no environment '{name}'; try one of {}", names(manifest).join(", ")),
        })?;
    Ok(Environment::resolve(project, name, cfg))
}

/// A value as the shell that will read this file has to see it.
///
/// The file is sourced by the migration step and by the unit, so a value is
/// double-quoted with the four characters that still mean something inside
/// double quotes escaped. Without this, a banner containing an apostrophe --
/// `MOTD=it's fine` -- is a file that writes correctly and then fails to be
/// sourced, which is the worst of both.
pub fn quote_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            '`' => out.push_str("\\`"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The file the deploy writes on the host: the variables, and nothing else.
///
/// `HS_ENVIRONMENT` and `HS_RELEASE` are added by the deploy, so a running
/// service can be asked what it is and answer exactly. The header says where
/// the file came from, because the first person to edit it by hand should know
/// a deploy will overwrite it.
pub fn render_env_file(env: &Environment, release: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("# hardscript environment: {}\n", env.name));
    out.push_str("# written by `hard deploy`; edit [env.");
    out.push_str(&env.name);
    out.push_str(".vars] in hard.toml instead, or the next deploy overwrites this\n");
    let secrets = if env.secrets.is_empty() { "none".to_string() } else { env.secrets.join(", ") };
    out.push_str(&format!(
        "# secrets ({secrets}) live in shared/env.secrets, which this file never touches\n"
    ));
    for (k, v) in &env.vars {
        out.push_str(&format!("{k}={}\n", quote_value(v)));
    }
    out.push_str(&format!("HS_ENVIRONMENT={}\n", env.name));
    out.push_str(&format!("HS_RELEASE={release}\n"));
    out
}

/// The names a host has to provide, and whether this process has them. The
/// value is never read, only the presence: a `show` that printed them would end
/// up in a terminal scrollback that nobody can revoke.
pub fn secret_status(env: &Environment) -> Vec<(String, bool)> {
    env.secrets.iter().map(|name| (name.clone(), std::env::var(name).is_ok())).collect()
}

/// A one-line summary for a list.
pub fn describe(env: &Environment) -> String {
    match &env.host {
        Some(h) => format!("{h} (dir {}, {})", env.dir, env.service),
        None => format!("(no host; dir {}, {})", env.dir, env.service),
    }
}

/// Check an environment against what a deploy will do with it. Problems are
/// returned rather than printed, so `hard deploy env check` and the deploy
/// itself can use the same judgement.
pub fn problems(project: &str, env: &Environment) -> Vec<String> {
    let mut out = Vec::new();
    if let Err(e) = crate::release::RemoteLayout::new(&env.dir, project) {
        out.push(format!("dir: {e}"));
    }
    if env.service.is_empty() || env.service.contains('/') {
        out.push(format!("service: `{}` is not a unit name", env.service));
    }
    if let Some(p) = env.port {
        if p < 1024 {
            out.push(format!("port: {p} is privileged; a service on it needs root"));
        }
    }
    for (k, v) in &env.vars {
        if v.contains('\n') {
            out.push(format!("vars.{k}: a value may not span lines"));
        }
        if env.secrets.contains(k) {
            out.push(format!("vars.{k}: also listed as a secret, so the deploy would write it"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------

/// `hard deploy env <list|show|render|check>`.
pub fn cmd_env(args: &[String]) {
    let (sub, rest) = match args.split_first() {
        Some((s, r)) => (s.as_str(), r),
        None => {
            eprintln!("hard deploy env: missing subcommand (list, show, render, check)");
            std::process::exit(2);
        }
    };
    let (project, manifest) = project_manifest();
    match sub {
        "list" => cmd_list(&project, &manifest),
        "show" => cmd_show(&project, &manifest, rest),
        "render" => cmd_render(&project, &manifest, rest),
        "check" => cmd_check(&project, &manifest, rest),
        other => {
            eprintln!("hard deploy env: unknown subcommand '{other}' (list, show, render, check)");
            std::process::exit(2);
        }
    }
}

fn cmd_list(project: &str, manifest: &Manifest) {
    if manifest.environments.is_empty() {
        println!("no environments: add one like this to hard.toml");
        println!();
        println!("[env.production]");
        println!("host = \"deploy@app.example.com\"");
        println!();
        println!("[env.production.vars]");
        println!("RUST_LOG = \"info\"");
        println!();
        println!("secrets = [\"DATABASE_URL\"]");
        return;
    }
    for name in names(manifest) {
        let env = Environment::resolve(project, &name, &manifest.environments[&name]);
        println!("{name:<12} {}", describe(&env));
    }
    println!();
    println!("deploy: hard deploy ssh --env <name>");
}

fn cmd_show(project: &str, manifest: &Manifest, args: &[String]) {
    let name = match args.first() {
        Some(n) => n.as_str(),
        None => {
            eprintln!("hard deploy env show: which environment? try `hard deploy env list`");
            std::process::exit(2);
        }
    };
    let env = resolve(project, manifest, name).unwrap_or_else(|e| crate::die(&e));
    println!("environment {}", env.name);
    println!("  host      {}", env.host.clone().unwrap_or_else(|| "(none)".to_string()));
    println!("  dir       {}", env.dir);
    println!("  service   {}", env.service);
    if let Some(u) = &env.user {
        println!("  user      {u}");
    }
    if let Some(p) = env.port {
        println!("  port      {p}");
    }
    if let Some(h) = &env.health_path {
        println!("  health    {h}");
    }
    if let Some(t) = env.health_timeout {
        println!("  timeout   {t}s");
    }
    if env.vars.is_empty() {
        println!("  vars      (none)");
    } else {
        println!("  vars");
        for (k, v) in &env.vars {
            println!("    {k} = {v}");
        }
    }
    if env.secrets.is_empty() {
        println!("  secrets   (none)");
    } else {
        println!("  secrets   (names only; the values belong in shared/env.secrets on the host)");
        for (name, here) in secret_status(&env) {
            println!(
                "    {name:<24} {}",
                if here { "set in this shell" } else { "not set here" }
            );
        }
    }
}

fn cmd_render(project: &str, manifest: &Manifest, args: &[String]) {
    let name = match args.first() {
        Some(n) => n.as_str(),
        None => {
            eprintln!("hard deploy env render: which environment? try `hard deploy env list`");
            std::process::exit(2);
        }
    };
    let env = resolve(project, manifest, name).unwrap_or_else(|e| crate::die(&e));
    // The release is not known until a deploy is built; the placeholder keeps
    // the shape of the file obvious.
    print!("{}", render_env_file(&env, "<release>"));
}

fn cmd_check(project: &str, manifest: &Manifest, args: &[String]) {
    let name = match args.first() {
        Some(n) => n.as_str(),
        None => {
            eprintln!("hard deploy env check: which environment? try `hard deploy env list`");
            std::process::exit(2);
        }
    };
    let env = resolve(project, manifest, name).unwrap_or_else(|e| crate::die(&e));
    let issues = problems(project, &env);
    if issues.is_empty() {
        println!("{name}: ok");
        if env.host.is_none() {
            println!("  no host: `hard deploy ssh --env {name} <host>` will need one");
        }
        for (s, here) in secret_status(&env) {
            if !here {
                println!("  {s} is not in this shell; set it in shared/env.secrets on the host");
            }
        }
        return;
    }
    eprintln!("{name}: {} problem(s)", issues.len());
    for p in &issues {
        eprintln!("  {p}");
    }
    std::process::exit(1);
}

/// The project name and manifest, from the working directory.
pub fn project_manifest() -> (String, Manifest) {
    let manifest = Manifest::load(std::path::Path::new("hard.toml")).ok().flatten();
    let name = manifest
        .as_ref()
        .map(|m| m.name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|d| d.file_name().map(|s| s.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "app".to_string())
        });
    (name, manifest.unwrap_or_default())
}

/// The remote command that writes the environment file, heredoc and all.
///
/// A quoted delimiter means the shell writes the lines exactly as they are --
/// no expansion, no glob, no `$` in a value turning into a variable. The
/// `umask` is in the command because the file is not secret but the operator
/// should not have to think about what mode a heredoc lands with.
pub fn env_write_command(path: &str, body: &str) -> String {
    format!("umask 022; cat > {} <<'HS_ENV_EOF'\n{body}HS_ENV_EOF\nchmod 0644 {}", sh_quote(path), sh_quote(path))
}

/// The remote command that checks the host has the secrets, by name.
///
/// This runs before the restart, so a missing secret fails a deploy that has
/// not yet swapped a symlink, and says which variable is missing rather than
/// letting the service die on boot with a less helpful message.
pub fn secrets_check_command(path: &str, secrets: &[String]) -> String {
    let file = sh_quote(path);
    // The `;` after every `}` is load-bearing: `{ ...; } grep` is a syntax
    // error, and a deploy that cannot parse its own check cannot report
    // anything useful about it.
    let mut out = format!("f={file}; [ -f \"$f\" ] || {{ echo \"no $f on this host\" >&2; exit 1; }};");
    for name in secrets {
        // The name is an upper-case identifier the manifest already checked, so
        // it needs no quoting -- and quoting it inside a quoted pattern is how
        // you end up grepping for a literal apostrophe.
        out.push_str(&format!(
            "grep -q \"^{name}=\" \"$f\" || {{ echo \"{name} is not set in $f\" >&2; exit 1; }}; "
        ));
    }
    // The last check leaves the exit status of the final grep; the loop ends
    // with a true so an environment with no secrets is a pass, not the exit
    // status of the last statement before it.
    out.push_str(" true");
    out
}

/// `source` lines a command needs to read both env files.
pub fn env_source_lines(shared: &str) -> String {
    let env = sh_quote(&format!("{shared}/env"));
    let secrets = sh_quote(&format!("{shared}/env.secrets"));
    format!("[ -f {secrets} ] && . {secrets}; [ -f {env} ] && . {env}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn manifest_with(envs: &str) -> Manifest {
        let src = format!(
            "schema = 1\nname = \"app\"\nversion = \"1.0.0\"\nedition = \"2027\"\n\n{envs}\n"
        );
        hs_pm::manifest::parse(&src, hs_pm::manifest::ManifestMode::Strict)
            .expect("the fixture parses")
            .manifest
    }

    fn production() -> Manifest {
        manifest_with(
            r#"[env.production]
host = "deploy@app.example.com"
dir = "/srv/api"
port = 8080
health_path = "/live"
health_timeout = 60
secrets = ["DATABASE_URL", "SESSION_KEY"]

[env.production.vars]
RUST_LOG = "info"

[env.staging]
host = "deploy@staging.example.com""#,
        )
    }

    #[test]
    fn an_environment_fills_in_the_defaults_a_project_would_have_used() {
        let m = production();
        let staging = resolve("app", &m, "staging").expect("staging");
        assert_eq!(staging.dir, "/srv/app", "the default root follows the project");
        assert_eq!(staging.service, "app.service", "and so does the unit");
        assert!(staging.vars.is_empty(), "nothing declared, nothing set");
        let prod = resolve("app", &m, "production").expect("production");
        assert_eq!(prod.dir, "/srv/api", "the environment wins");
        assert_eq!(prod.port, Some(8080), "and the port");
        assert_eq!(prod.health_timeout, Some(60), "and the timeout");
        assert_eq!(describe(&prod), "deploy@app.example.com (dir /srv/api, app.service)", "one line");
    }

    #[test]
    fn a_missing_environment_says_which_ones_exist() {
        let m = production();
        let err = resolve("app", &m, "stagin").unwrap_err();
        assert!(err.contains("no environment 'stagin'"), "{err}");
        assert!(err.contains("production, staging"), "and lists them: {err}");
        let err = resolve("app", &Manifest::default(), "production").unwrap_err();
        assert!(err.contains("add [env.production]"), "an empty manifest says what to write: {err}");
    }

    #[test]
    fn the_env_file_has_the_variables_and_nothing_else() {
        let m = production();
        let env = resolve("app", &m, "production").unwrap();
        let text = render_env_file(&env, "1.0.0-20250915T070530Z-abcdef12");
        assert!(text.contains("RUST_LOG=\"info\"\n"), "the declared variable: {text}");
        assert!(text.contains("HS_ENVIRONMENT=production\n"), "and which environment this is: {text}");
        assert!(text.contains("HS_RELEASE=1.0.0-20250915T070530Z-abcdef12\n"), "and which release: {text}");
        for secret in &env.secrets {
            assert!(!text.contains(&format!("{secret}=")), "{secret} is not written: {text}");
        }
        assert!(text.contains("# secrets (DATABASE_URL, SESSION_KEY)"), "but says where they are: {text}");
        // And it is the kind of file a shell can source without a surprise.
        let empty = render_env_file(&resolve("app", &m, "staging").unwrap(), "r1");
        assert!(!empty.contains("RUST_LOG"), "staging does not inherit production's variables");
    }

    #[test]
    fn a_secret_is_reported_by_presence_only() {
        let m = production();
        let env = resolve("app", &m, "production").unwrap();
        std::env::set_var("HS_DEPLOY_TEST_SECRET", "x");
        // The two the fixture names are almost certainly not in this shell.
        let status = secret_status(&env);
        assert_eq!(status.len(), 2, "both secrets are listed");
        assert!(status.iter().all(|(_, here)| !*here), "neither is set here: {status:?}");
        assert!(status.iter().all(|(name, _)| name.chars().all(|c| c.is_ascii_uppercase() || c == '_')), "names only");
    }

    #[test]
    fn a_check_finds_what_a_deploy_would_trip_over() {
        let m = production();
        let env = resolve("app", &m, "production").unwrap();
        assert!(problems("app", &env).is_empty(), "a good environment has no problems");
        let mut bad = env.clone();
        bad.dir = "/srv/api; rm -rf /".to_string();
        bad.service = "api.service/extra".to_string();
        bad.port = Some(80);
        bad.vars.push(("DATABASE_URL".to_string(), "postgresql://u:p@h/d".to_string()));
        let issues = problems("app", &bad);
        assert_eq!(issues.len(), 4, "{issues:?}");
        assert!(issues.iter().any(|i| i.starts_with("dir:")), "{issues:?}");
        assert!(issues.iter().any(|i| i.starts_with("service:")), "{issues:?}");
        assert!(issues.iter().any(|i| i.contains("privileged")), "{issues:?}");
        assert!(issues.iter().any(|i| i.contains("also listed as a secret")), "{issues:?}");
    }

    #[test]
    fn the_env_file_is_written_exactly_as_it_was_rendered() {
        let body = "RUST_LOG=info\nHS_ENVIRONMENT=production\n";
        let cmd = env_write_command("/srv/app/shared/env", body);
        assert!(cmd.contains("<<'HS_ENV_EOF'"), "a quoted delimiter, so nothing is expanded: {cmd}");
        assert!(cmd.contains("\nHS_ENV_EOF\n"), "and the heredoc is closed: {cmd}");
        assert!(cmd.contains("chmod 0644 '/srv/app/shared/env'"), "with a mode: {cmd}");
        // A value with a shell metacharacter must survive the round trip.
        let tricky = format!("MOTD=it's \"quoted\" $HOME\n");
        let cmd = env_write_command("/srv/app/shared/env", &tricky);
        assert!(cmd.contains(&tricky), "the value is written as it is: {cmd}");
    }

    #[test]
    fn a_missing_secret_fails_the_deploy_by_name() {
        let cmd = secrets_check_command("/srv/app/shared/env.secrets", &["DATABASE_URL".into()]);
        assert!(cmd.contains("[ -f \"$f\" ] || { echo \"no $f on this host\""), "when the file is absent: {cmd}");
        assert!(cmd.contains("grep -q \"^DATABASE_URL=\" \"$f\""), "or when the variable is not in it: {cmd}");
        assert!(cmd.contains("DATABASE_URL is not set in"), "naming the variable: {cmd}");
        assert!(cmd.trim_end().ends_with("true"), "and an environment with nothing to check passes: {cmd}");
        let none = secrets_check_command("/srv/app/shared/env.secrets", &[]);
        assert!(!none.contains("grep"), "nothing to check means nothing to grep");
    }

    #[test]
    fn a_value_survives_a_shell_that_reads_the_file() {
        // The cases are the ones a real banner or connection string contains.
        assert_eq!(quote_value("info"), "\"info\"");
        assert_eq!(quote_value("it's fine"), "\"it's fine\"");
        assert_eq!(quote_value("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote_value("$HOME"), "\"\\$HOME\"");
        assert_eq!(quote_value("a\\b"), "\"a\\\\b\"");
        assert_eq!(quote_value("`id`"), "\"\\`id\\`\"");
        assert_eq!(quote_value(""), "\"\"");
        // And the whole file is sourceable, which is the point.
        let mut env = Environment::resolve("app", "p", &EnvironmentConfig::default());
        env.vars = vec![
            ("MOTD".to_string(), "it's fine".to_string()),
            ("GREETING".to_string(), "say \"hi\"".to_string()),
            ("HOME_DIR".to_string(), "$HOME".to_string()),
        ];
        env.secrets = vec!["TOKEN".to_string()];
        let text = render_env_file(&env, "r1");
        let mut sh = String::from("set -a\n");
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            sh.push_str(line);
            sh.push('\n');
        }
        sh.push_str("set +a\nprintf '%s|%s|%s\\n' \"$MOTD\" \"$GREETING\" \"$HOME_DIR\"\n");
        let out = std::process::Command::new("sh").arg("-c").arg(&sh).output().expect("sh runs");
        assert!(out.status.success(), "the file is sourceable: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "it's fine|say \"hi\"|$HOME",
            "and the values come back exactly"
        );
    }

    /// Run a generated command in `sh`, because a command that does not parse
    /// is a deploy that fails for a reason nobody can read.
    fn sh(dir: &Path, script: &str) -> (i32, String, String) {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .output()
            .expect("sh runs");
        (
            out.status.code().unwrap_or(255),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hs-env-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("shared")).expect("a scratch root");
        dir
    }

    #[test]
    fn the_written_env_file_is_one_a_shell_accepts() {
        let root = scratch("write");
        let path = root.join("shared/env");
        let env = Environment {
            name: "production".to_string(),
            host: None,
            dir: root.display().to_string(),
            service: "app.service".to_string(),
            user: None,
            port: None,
            health_path: None,
            health_timeout: None,
            restart_cmd: None,
            vars: vec![
                ("RUST_LOG".to_string(), "info".to_string()),
                ("MOTD".to_string(), "it's fine".to_string()),
                ("QUOTED".to_string(), "say \"hi\"".to_string()),
                ("HOME_DIR".to_string(), "$HOME".to_string()),
            ],
            secrets: vec!["TOKEN".to_string()],
        };
        let file = path.display().to_string();
        let body = render_env_file(&env, "r1");
        let cmd = env_write_command(&file, &body);
        let (rc, _, err) = sh(&root, &cmd);
        assert_eq!(rc, 0, "the write step runs: {err}");
        let written = std::fs::read_to_string(&path).expect("the file it wrote");
        assert!(written.contains("MOTD=\"it's fine\""), "quoted: {written}");
        let (rc, _, err) = sh(&root, &format!("set -a; . '{file}'; set +a; printf '%s|%s|%s\n' \"$MOTD\" \"$QUOTED\" \"$HOME_DIR\""));
        assert_eq!(rc, 0, "and the file a shell can read: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_secret_check_is_a_script_a_shell_can_parse() {
        let root = scratch("secrets");
        let file = root.join("shared/env.secrets");
        let path = file.display().to_string();
        let cmd = secrets_check_command(&path, &["DATABASE_URL".to_string(), "SESSION_KEY".to_string()]);

        let (rc, _, err) = sh(&root, &cmd);
        assert_ne!(rc, 0, "an absent file fails the check");
        assert!(err.contains("env.secrets"), "and says so: {err}");

        std::fs::write(&file, "DATABASE_URL=postgres://u@h/d\n").expect("one of two");
        let (rc, _, err) = sh(&root, &cmd);
        assert_ne!(rc, 0, "a missing variable fails it");
        assert!(err.contains("SESSION_KEY is not set in"), "by name: {err}");

        std::fs::write(&file, "DATABASE_URL=postgres://u@h/d\nSESSION_KEY=abc\n").expect("both");
        let (rc, _, err) = sh(&root, &cmd);
        assert_eq!(rc, 0, "and a host that has them passes: {err}");

        // A variable that is present but empty is present: the operator's
        // decision, and a different failure if the service dislikes it.
        std::fs::write(&file, "DATABASE_URL=\nSESSION_KEY=abc\n").expect("empty");
        let (rc, _, err) = sh(&root, &cmd);
        assert_eq!(rc, 0, "an empty value counts as set: {err}");

        // An environment with no secrets to check is not a failure.
        let none = secrets_check_command(&path, &[]);
        let (rc, _, err) = sh(&root, &none);
        assert_eq!(rc, 0, "nothing to check is a pass: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_migration_reads_both_env_files() {
        let src = env_source_lines("/srv/app/shared");
        assert!(src.contains("'/srv/app/shared/env.secrets'"), "secrets first: {src}");
        assert!(src.contains("'/srv/app/shared/env'"), "and the generated file: {src}");
        // A file that is not there is not an error: a service with no secrets
        // is a normal service.
        assert!(src.starts_with("[ -f "), "every source is guarded: {src}");
    }
}
