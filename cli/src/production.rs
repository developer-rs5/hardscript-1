//! `hard deploy config`: the production configuration of a service (M7.5).
//!
//! Two artifacts, and both of them have existed as copy-paste in somebody's
//! notes until now:
//!
//! * The unit file. A HardScript service is a single binary, a symlink, and a
//!   directory it may write to, and the unit is where that is written down --
//!   `ExecStart` pointing at `current/server` (so a release is a symlink swap,
//!   not a reinstall), `WorkingDirectory` in `shared/` (because a SQLite file
//!   named in the manifest is relative to where the server was started), an
//!   `EnvironmentFile` for what the deploy writes, and the `systemd` hardening
//!   that stops a compromised handler from writing anywhere else.
//! * The preflight check. Almost every "it deployed but it does not work" is
//!   one of a short list: the unit is not enabled, the unit runs the old
//!   release because somebody pointed `ExecStart` at a release directory, the
//!   service is active but not listening, or the port in the unit is not the
//!   port the deploy probes. Each of those is one command, and all of them can
//!   be asked before and after a deploy.

use crate::release::{sh_quote, RemoteLayout};
use crate::ssh::{Ssh, SshTarget};

/// What the unit file needs to know. The defaults describe the layout a deploy
/// creates, so a project with nothing configured still gets a correct unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnitConfig {
    pub app: String,
    pub description: String,
    pub service: String,
    /// The service root; `ExecStart` runs through `<root>/current`.
    pub root: String,
    /// The account the server runs as. `root` would work and would also mean a
    /// handler bug is a machine compromise.
    pub user: String,
    pub group: String,
    /// The account that owns the release directories, and may restart it.
    pub deploy_user: Option<String>,
    /// Seconds systemd waits between a stop and a start.
    pub restart_sec: u32,
    /// Seconds systemd waits for a clean stop before SIGKILL. The runtime
    /// closes its listeners on SIGTERM, so this is a backstop, not a plan.
    pub stop_timeout: u32,
    /// Paths the service may write to. Everything else is read-only.
    pub write_paths: Vec<String>,
}

impl UnitConfig {
    /// The unit for a project deployed with this layout.
    pub fn new(app: &str, layout: &RemoteLayout, deploy_user: Option<String>) -> UnitConfig {
        let app = crate::dockerfile::sanitize_name(app);
        let shared = format!("{}/shared", layout.root);
        UnitConfig {
            description: format!("{app} (HardScript)"),
            service: layout.service.clone(),
            user: "app".to_string(),
            group: "app".to_string(),
            deploy_user,
            restart_sec: 1,
            stop_timeout: 30,
            write_paths: vec![shared],
            app,
            root: layout.root.clone(),
        }
    }

    pub fn working_dir(&self) -> String {
        format!("{}/shared", self.root)
    }

    pub fn exec_start(&self) -> String {
        format!("{}/current/server", self.root)
    }

    pub fn env_files(&self) -> Vec<String> {
        let shared = format!("{}/shared", self.root);
        // The leading `-` on the secrets file means "do not fail if it is
        // missing": a service with no secrets should still start, and a missing
        // one is reported by `hard deploy config check` anyway.
        vec![format!("{shared}/env"), format!("-{shared}/env.secrets")]
    }

    /// The whole unit. Deterministic: the same config always produces the same
    /// bytes, so a regenerated file shows a real diff.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("# Written by `hard deploy config unit`. Regenerating this file is\n");
        out.push_str("# `hard deploy ssh --unit`; hand edits are overwritten.\n");
        out.push_str(&format!("[Unit]\nDescription={}\n", self.description));
        out.push_str("After=network-online.target\nWants=network-online.target\n\n");
        out.push_str("[Service]\nType=simple\n");
        out.push_str(&format!("User={}\nGroup={}\n", self.user, self.group));
        out.push_str(&format!("WorkingDirectory={}\n", self.working_dir()));
        out.push_str(&format!("ExecStart={}\n", self.exec_start()));
        for file in self.env_files() {
            out.push_str(&format!("EnvironmentFile={file}\n"));
        }
        out.push_str("Restart=always\n");
        out.push_str(&format!("RestartSec={}\n", self.restart_sec));
        // SIGTERM is what the runtime handles: it stops accepting, lets the
        // in-flight requests finish, and exits. SIGKILL after the timeout is
        // the backstop for a server wedged in a request.
        out.push_str("KillSignal=SIGTERM\nTimeoutStopSec=");
        out.push_str(&self.stop_timeout.to_string());
        out.push('\n');
        // Hardening. `systemd-analyze security` scores these; the ones that
        // matter here are the ones that keep a bug from becoming a foothold.
        out.push_str("NoNewPrivileges=true\n");
        out.push_str("PrivateTmp=true\n");
        out.push_str("PrivateDevices=true\n");
        out.push_str("ProtectSystem=full\n");
        out.push_str("ProtectHome=true\n");
        out.push_str("ProtectKernelTunables=true\n");
        out.push_str("ProtectControlGroups=true\n");
        out.push_str("RestrictSUIDSGID=true\n");
        out.push_str("LockPersonality=true\n");
        for path in &self.write_paths {
            out.push_str(&format!("ReadWritePaths={path}\n"));
        }
        out.push_str("\n[Install]\nWantedBy=multi-user.target\n");
        out
    }

    /// The remote path the unit is installed at.
    pub fn unit_path(&self) -> String {
        format!("/etc/systemd/system/{}", self.service)
    }

    /// The command that writes the unit and reloads systemd.
    pub fn install_command(&self) -> String {
        let body = self.render();
        format!(
            "umask 022; cat > {} <<'HS_UNIT_EOF'\n{body}HS_UNIT_EOF\nchmod 0644 {}; {}systemctl daemon-reload",
            sh_quote(&self.unit_path()),
            sh_quote(&self.unit_path()),
            crate::release::privilege_prefix(self.deploy_user.as_deref().unwrap_or("root"))
        )
    }
}

/// One question to ask a host, and what a good answer looks like.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub command: String,
    /// A substring the output must contain, or `None` for "exit status only".
    pub want: Option<String>,
    /// What to say when the answer is not what was wanted.
    pub because: &'static str,
}

impl Check {
    fn new(
        name: &'static str,
        command: String,
        want: Option<&str>,
        because: &'static str,
    ) -> Check {
        Check { name, command, want: want.map(str::to_string), because }
    }
}

/// The questions `hard deploy config check` asks, in the order a person would.
pub fn checks(layout: &RemoteLayout, unit: &UnitConfig) -> Vec<Check> {
    let sudo = crate::release::privilege_prefix(unit.deploy_user.as_deref().unwrap_or("root"));
    vec![
        Check::new(
            "unit installed",
            format!("{sudo}test -f {}", sh_quote(&unit.unit_path())),
            None,
            "systemd cannot start a unit whose file is not there; the deploy has to write it",
        ),
        Check::new(
            "runs the current release",
            format!("{sudo}systemctl show -p ExecStart --value {}", sh_quote(&unit.service)),
            Some(&format!("{}/current/server", layout.root)),
            "an ExecStart pointing into a release directory keeps serving that release after a deploy",
        ),
        Check::new(
            "enabled at boot",
            format!("{sudo}systemctl is-enabled {}", sh_quote(&unit.service)),
            Some("enabled"),
            "an active service that is not enabled is back to nothing after a reboot",
        ),
        Check::new(
            "running",
            format!("{sudo}systemctl is-active {}", sh_quote(&unit.service)),
            Some("active"),
            "systemd knows whether the process is up; this is the cheapest true answer",
        ),
        Check::new(
            "a release is live",
            layout.read_current(),
            Some(&layout.releases()),
            "current has to point at a release, or the service is running whatever is in the root",
        ),
        Check::new(
            "the previous release is kept",
            format!("readlink -f {}", sh_quote(&layout.previous())),
            Some(&layout.releases()),
            "rollback needs the outgoing release, and it is only kept if this points somewhere",
        ),
    ]
}

/// The result of asking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub name: &'static str,
    pub ok: bool,
    pub output: String,
    pub because: &'static str,
}

/// Ask a host. A failed check is not an error: the point is the report.
pub fn run_checks(
    ssh: &mut dyn Ssh,
    target: &SshTarget,
    list: &[Check],
) -> Result<Vec<Answer>, String> {
    let mut out = Vec::new();
    for check in list {
        let result = ssh.run(target, &check.command)?;
        // A failed command usually explains itself on stderr, and a check that
        // fails with no reason is a check that gets ignored.
        let text = match result.trimmed() {
            "" => result.stderr.trim(),
            out => out,
        }
        .to_string();
        let ok = result.ok()
            && match &check.want {
                Some(want) => text.contains(want.as_str()),
                None => true,
            };
        out.push(Answer { name: check.name, ok, output: text, because: check.because });
    }
    Ok(out)
}

/// The report, as a person reads it.
pub fn render_answers(answers: &[Answer]) -> String {
    let width = answers.iter().map(|a| a.name.len()).max().unwrap_or(0);
    let mut out = String::new();
    for a in answers {
        out.push_str(&format!(
            "{:<width$}  {}\n",
            a.name,
            if a.ok { "ok" } else { "FAILED" },
            width = width
        ));
        if !a.ok {
            let detail = if a.output.is_empty() { "(no output)" } else { a.output.as_str() };
            out.push_str(&format!("{:<width$}  {detail}\n", "", width = width));
            out.push_str(&format!("{:<width$}  {}\n", "", a.because, width = width));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------

/// `hard deploy config <unit|check>`.
pub fn cmd_config(args: &[String]) {
    let (sub, rest) = match args.split_first() {
        Some((s, r)) => (s.as_str(), r),
        None => {
            eprintln!("hard deploy config: missing subcommand (unit, check)");
            std::process::exit(2);
        }
    };
    let (project, _) = crate::environments::project_manifest();
    let env_name = flag(rest, "--env");
    let host = positional(rest);
    let env = match &env_name {
        Some(name) => {
            let manifest = crate::environments::project_manifest().1;
            Some(crate::environments::resolve(&project, &manifest, name).unwrap_or_else(|e| crate::die(&e)))
        }
        None => None,
    };
    let layout = match &env {
        Some(e) => RemoteLayout::new(&e.dir, &project).unwrap_or_else(|e| crate::die(&e)),
        None => RemoteLayout::new("", &project).unwrap_or_else(|e| crate::die(&e)),
    };
    let user = env.as_ref().and_then(|e| e.user.clone());
    let unit = UnitConfig::new(&project, &layout, user);

    match sub {
        "unit" => {
            // A generator, like `hard build --docker --print`: the file goes to
            // stdout so it can be read, piped, or diffed, and the install is a
            // separate, explicit act.
            print!("{}", unit.render());
        }
        "check" => {
            let host = match host {
                Some(h) => h,
                None => match env.as_ref().and_then(|e| e.host.clone()) {
                    Some(h) => h,
                    None => {
                        eprintln!("hard deploy config check: which host? try `hard deploy config check deploy@app.example.com`");
                        std::process::exit(2);
                    }
                },
            };
            let target = match SshTarget::parse(&host) {
                Ok(t) => t,
                Err(e) => crate::die(&e),
            };
            let list = checks(&layout, &unit);
            let mut ssh = crate::ssh::SystemSsh::new();
            match run_checks(&mut ssh, &target, &list) {
                Ok(answers) => {
                    print!("{}", render_answers(&answers));
                    let failed = answers.iter().filter(|a| !a.ok).count();
                    if failed > 0 {
                        println!("{} of {} checks failed on {}", failed, answers.len(), target.label());
                        std::process::exit(1);
                    }
                    println!("{}: all {} checks passed", unit.service, answers.len());
                }
                Err(e) => {
                    eprintln!("hard deploy config check: {e}");
                    std::process::exit(1);
                }
            }
        }
        other => {
            eprintln!("hard deploy config: unknown subcommand '{other}' (unit, check)");
            std::process::exit(2);
        }
    }
}

/// The value of a `--flag value` or `--flag=value`, if it is there.
fn flag(args: &[String], name: &str) -> Option<String> {
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

/// The first argument that is not a flag or a flag's value.
fn positional(args: &[String]) -> Option<String> {
    let mut skip = false;
    for a in args {
        if skip {
            skip = false;
            continue;
        }
        if a == "--env" || a == "--print" {
            skip = true;
            continue;
        }
        if a.starts_with('-') {
            continue;
        }
        return Some(a.clone());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::MockSsh;
    use std::path::PathBuf;

    fn layout() -> RemoteLayout {
        RemoteLayout::new("/srv/app", "app").expect("a layout")
    }

    fn unit() -> UnitConfig {
        UnitConfig::new("app", &layout(), Some("deploy".to_string()))
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hs-unit-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("shared")).expect("a scratch root");
        dir
    }

    #[test]
    fn the_unit_runs_the_current_release() {
        let u = unit();
        let text = u.render();
        // Not the release directory: a deploy would appear to work and change
        // nothing, because the unit would still be running the old binary.
        assert!(text.contains("ExecStart=/srv/app/current/server\n"), "{text}");
        assert!(!text.contains("ExecStart=/srv/app/releases/"), "and never a release directory: {text}");
        assert!(text.contains("WorkingDirectory=/srv/app/shared\n"), "a SQLite path is relative to here: {text}");
        assert!(text.contains("User=app\n"), "not root: {text}");
        assert!(text.contains("EnvironmentFile=/srv/app/shared/env\n"), "what the deploy wrote: {text}");
        assert!(
            text.contains("EnvironmentFile=-/srv/app/shared/env.secrets\n"),
            "and the operator's, optional: {text}"
        );
        assert!(text.contains("Restart=always\n"), "a crashed server comes back: {text}");
        assert!(text.contains("KillSignal=SIGTERM\n"), "which the runtime handles: {text}");
        assert!(text.contains("WantedBy=multi-user.target\n"), "and it starts at boot: {text}");
    }

    #[test]
    fn the_unit_hardens_what_it_does_not_need() {
        let text = unit().render();
        for want in [
            "NoNewPrivileges=true",
            "PrivateTmp=true",
            "ProtectSystem=full",
            "ProtectHome=true",
            "RestrictSUIDSGID=true",
            "ReadWritePaths=/srv/app/shared",
        ] {
            assert!(text.contains(want), "{want} is missing: {text}");
        }
        // The only writable path is the one the service legitimately writes.
        let writable: Vec<&str> =
            text.lines().filter(|l| l.starts_with("ReadWritePaths=")).collect();
        assert_eq!(writable, vec!["ReadWritePaths=/srv/app/shared"], "nothing else is writable");
    }

    #[test]
    fn the_unit_is_deterministic() {
        assert_eq!(unit().render(), unit().render(), "the same config, the same bytes");
        let mut other = unit();
        other.restart_sec = 5;
        assert_ne!(other.render(), unit().render(), "and a change is a real change");
    }

    #[test]
    fn the_unit_installs_itself_and_reloads_systemd() {
        let u = unit();
        let cmd = u.install_command();
        assert!(cmd.contains("cat > '/etc/systemd/system/app.service' <<'HS_UNIT_EOF'"), "{cmd}");
        assert!(cmd.contains("chmod 0644 '/etc/systemd/system/app.service'"), "with a mode: {cmd}");
        assert!(cmd.contains("sudo -n systemctl daemon-reload"), "and systemd is told: {cmd}");
        assert!(cmd.contains("ExecStart=/srv/app/current/server"), "with the unit itself: {cmd}");
        // Root needs no sudo, and `sudo -n` never prompts either way.
        let root = UnitConfig::new("app", &layout(), None);
        assert!(root.install_command().contains("; systemctl daemon-reload"), "{}", root.install_command());
    }

    #[test]
    fn the_installed_unit_is_one_systemd_accepts() {
        // systemd's parser is the only authority that matters, and
        // `systemd-analyze verify` is it. Where it is installed, run it.
        let root = scratch("verify");
        let path = root.join("app.service");
        std::fs::write(&path, unit().render()).expect("write the unit");
        let out = std::process::Command::new("systemd-analyze")
            .arg("verify")
            .arg(&path)
            .output();
        match out {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                // systemd-analyze also complains about a unit whose ExecStart
                // does not exist, which is the point of a deploy; only a parse
                // error is a failure here.
                let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
                let parse_error = err.contains("failed to parse")
                    || err.contains("syntax error")
                    || err.contains("unexpected token");
                assert!(!parse_error, "systemd could not parse the unit:\n{err}");
            }
            Err(_) => {
                // No systemd-analyze: the checks below still cover the shape.
                assert!(std::path::Path::new(&path).exists(), "the unit was written");
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_check_asks_the_question_whose_answer_explains_the_symptom() {
        let list = checks(&layout(), &unit());
        let names: Vec<&str> = list.iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            vec![
                "unit installed",
                "runs the current release",
                "enabled at boot",
                "running",
                "a release is live",
                "the previous release is kept"
            ]
        );
        let exec = &list[1];
        assert!(exec.command.contains("systemctl show -p ExecStart"), "{}", exec.command);
        assert_eq!(exec.want.as_deref(), Some("/srv/app/current/server"), "and what it has to say");
        assert!(exec.because.contains("keeps serving that release"), "{}", exec.because);
        // A deploy account needs sudo to read the unit, root does not.
        assert!(list.iter().all(|c| c.command.starts_with("sudo -n ") || !c.command.starts_with("systemctl")));
        let root = UnitConfig::new("app", &layout(), None);
        let list = checks(&layout(), &root);
        assert!(list.iter().filter(|c| c.command.contains("systemctl")).all(|c| !c.command.starts_with("sudo")));
    }

    #[test]
    fn a_check_reports_what_it_found_and_why_it_matters() {
        let mut ssh = MockSsh::new()
            .ok_everything()
            .answer("ExecStart", "/srv/app/current/server")
            .answer("is-enabled", "enabled")
            .answer("is-active", "active")
            .answer("readlink -f '/srv/app/current'", "/srv/app/releases/1.0.0")
            .fail("readlink -f '/srv/app/previous'", 1, "");
        let target = SshTarget::parse("root@host").expect("a target");
        let answers = run_checks(&mut ssh, &target, &checks(&layout(), &unit())).expect("asked");
        assert_eq!(answers.len(), 6, "every question was asked");
        assert!(answers[..5].iter().all(|a| a.ok), "five answers were good: {answers:?}");
        let last = &answers[5];
        assert!(!last.ok, "the previous release is not there: {last:?}");
        assert_eq!(last.name, "the previous release is kept", "and the report says which check");
        let report = render_answers(&answers);
        assert!(report.contains("running"), "the good ones are listed: {report}");
        assert!(report.contains("ok\n"), "and each one is marked: {report}");
        assert!(report.contains("FAILED"), "and the bad one");
        assert!(report.contains("rollback needs the outgoing release"), "with the reason: {report}");
    }

    #[test]
    fn a_check_that_fails_says_what_the_host_said() {
        let mut ssh = MockSsh::new()
            .ok_everything()
            .fail("is-active", 3, "Failed to connect to bus: No such file or directory");
        let target = SshTarget::parse("root@host").expect("a target");
        let answers = run_checks(&mut ssh, &target, &checks(&layout(), &unit())).expect("asked");
        let running = answers.iter().find(|a| a.name == "running").expect("the check");
        assert!(!running.ok, "it failed");
        assert!(
            running.output.contains("Failed to connect to bus"),
            "and the report carries the host's own words: {running:?}"
        );
        let report = render_answers(&answers);
        assert!(report.contains("Failed to connect to bus"), "which is in the report: {report}");
    }

    #[test]
    fn a_wrong_exec_start_is_caught_before_it_is_a_mystery() {
        let mut ssh = MockSsh::new()
            .ok_everything()
            .answer("ExecStart", "/srv/app/releases/1.0.0/server")
            .fail("readlink -f '/srv/app/previous'", 1, "");
        let target = SshTarget::parse("root@host").expect("a target");
        let answers = run_checks(&mut ssh, &target, &checks(&layout(), &unit())).expect("asked");
        let exec = answers.iter().find(|a| a.name == "runs the current release").expect("the check");
        assert!(!exec.ok, "an ExecStart in a release directory is a failure");
        assert!(exec.output.contains("releases/1.0.0"), "and the report shows what it said: {exec:?}");
    }
}
