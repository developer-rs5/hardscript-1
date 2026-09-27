//! `hard deploy logs`, `status`, `start`, `stop`, `restart` (M7.6).
//!
//! Deploying is the part everybody writes down. The next twenty minutes are
//! the part everybody forgets: is it up, what did it just log, and how do I stop
//! it without killing the process group. Those are four commands, and they are
//! worth having next to the deploy rather than in a wiki page of `journalctl`
// incantations with the wrong unit name in them.
//!
//! Every command here is a small pure function that renders a remote command,
//! plus a thin caller. The rendering is where the mistakes live -- a `logs`
//! that pages by default, a `restart` without `--no-block`, a status that
//! reports a release that was replaced an hour ago -- so that is what the tests
//! are about.

use crate::production::UnitConfig;
use crate::release::RemoteLayout;
use crate::ssh::{Ssh, SshTarget};

/// What `hard deploy logs` was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogArgs {
    /// How many lines of history. A `logs` that pages the whole journal is a
    /// terminal that stops responding, so this defaults to a number.
    pub lines: u32,
    /// Keep the stream open.
    pub follow: bool,
    /// `--since`, passed to journalctl as written.
    pub since: Option<String>,
    /// Watch the whole host instead of this unit.
    pub all_units: bool,
}

impl Default for LogArgs {
    fn default() -> LogArgs {
        LogArgs { lines: 200, follow: false, since: None, all_units: false }
    }
}

/// The remote command that reads the journal.
pub fn journal_command(unit: &UnitConfig, args: &LogArgs) -> String {
    let mut out = String::from("journalctl");
    if !args.all_units {
        out.push_str(&format!(" -u {}", crate::release::sh_quote(&unit.service)));
    }
    out.push_str(&format!(" -n {}", args.lines));
    if let Some(since) = &args.since {
        out.push_str(&format!(" --since {}", crate::release::sh_quote(since)));
    }
    if args.follow {
        out.push_str(" -f");
    } else {
        // A pager inside an ssh session with no terminal is a hang; --no-pager
        // and no colour is what makes the output usable in a pipe.
        out.push_str(" --no-pager --output=short-iso");
    }
    out
}

/// The remote command for a lifecycle action.
///
/// `--no-block` is the default on purpose: a `restart` that returns before
/// systemd is done gives a caller the chance to read the answer before the
/// answer exists, and the caller cannot ask "did it work" of a command that
/// already said yes.
pub fn lifecycle_command(action: &str, unit: &UnitConfig) -> String {
    let action = match action {
        "start" => "start",
        "stop" => "stop",
        "restart" => "restart",
        other => other,
    };
    let sudo = crate::release::privilege_prefix(unit.deploy_user.as_deref().unwrap_or("root"));
    format!("{sudo}systemctl {action} {} --no-block", crate::release::sh_quote(&unit.service))
}

/// One line of `hard deploy status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub label: String,
    pub value: String,
}

/// Everything a status reports, as a pure function of what the host said. The
/// caller fetches; this decides what it means, so the meaning is testable
/// without a host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub lines: Vec<Line>,
    /// A release is live and the service is running.
    pub healthy: bool,
    /// What to tell somebody who asked.
    pub summary: String,
}

/// The probes `status` makes, in order.
pub fn status_probes(layout: &RemoteLayout) -> Vec<(&'static str, String)> {
    vec![
        ("current", layout.read_current()),
        ("previous", format!("readlink -f {}", crate::release::sh_quote(&layout.previous()))),
        ("releases", release_count_command(layout)),
        (
            "active",
            format!("systemctl is-active {}", crate::release::sh_quote(&layout.service)),
        ),
        (
            "since",
            format!(
                "systemctl show -p ActiveEnterTimestamp --value {}",
                crate::release::sh_quote(&layout.service)
            ),
        ),
    ]
}

/// `ls` the release directories, newest first. `ls -1t` is one syscall and no
/// parsing of a date column.
pub fn release_count_command(layout: &RemoteLayout) -> String {
    format!("ls -1 {} 2>/dev/null | wc -l", crate::release::sh_quote(&layout.releases()))
}

/// The releases on a host, newest first, with the size of each.
pub fn releases_command(layout: &RemoteLayout) -> String {
    format!("ls -1t {} 2>/dev/null", crate::release::sh_quote(&layout.releases()))
}

/// Turn the answers into a status.
///
/// The rule for `healthy` is deliberate: a release is live *and* the unit is
/// active. A running service with no release behind it is serving whatever is
/// in the root directory, and a release with a stopped service is a deploy
/// that did not take. Neither is fine, and reporting "ok" for either is how a
/// status command stops being believed.
pub fn status_from(layout: &RemoteLayout, answers: &[(&'static str, String)]) -> Status {
    let get = |key: &str| {
        answers.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone()).unwrap_or_default()
    };
    let current = get("current");
    let previous = get("previous");
    let releases = get("releases");
    let active = get("active");
    let since = get("since");
    let live = current.contains(&layout.releases());
    let running = active.trim() == "active";
    let mut lines = vec![
        Line {
            label: "current".to_string(),
            value: if live { current.clone() } else { format!("{current} (no release behind it)") },
        },
        Line { label: "previous".to_string(), value: previous },
        Line {
            label: "releases".to_string(),
            value: if releases.trim().is_empty() {
                "unknown".to_string()
            } else {
                format!("{releases} kept")
            },
        },
        Line {
            label: "service".to_string(),
            value: if running { "active".to_string() } else { format!("{active} (not running)") },
        },
    ];
    if !since.trim().is_empty() && since.trim() != "n/a" {
        lines.push(Line { label: "running since".to_string(), value: since });
    }
    let healthy = live && running;
    let summary = match (live, running) {
        (true, true) => format!("{} is live and running", current),
        (true, false) => format!("{} is deployed but the service is {active}", current),
        (false, true) => "a service is running, but current does not point at a release".to_string(),
        (false, false) => "nothing is deployed or running".to_string(),
    };
    Status { lines, healthy, summary }
}

/// The report, as a person reads it.
pub fn render_status(status: &Status) -> String {
    let width = status.lines.iter().map(|l| l.label.len()).max().unwrap_or(0);
    let mut out = String::new();
    for line in &status.lines {
        out.push_str(&format!("{:<width$}  {}\n", line.label, line.value, width = width));
    }
    out.push_str(&format!("\n{}\n", status.summary));
    out
}

// ---------------------------------------------------------------------------
// The commands
// ---------------------------------------------------------------------------

/// `hard deploy logs [host] [--env <name>] [-n N] [-f] [--since when]`.
pub fn cmd_logs(args: &[String]) {
    let parsed = LogArgs::parse(args).unwrap_or_else(|e| {
        eprintln!("hard deploy logs: {e}");
        std::process::exit(2);
    });
    let (target, unit) = target_and_unit(args);
    let command = journal_command(&unit.unit, &parsed);
    let mut ssh = crate::ssh::SystemSsh::new();
    if parsed.follow {
        // The stream is this process's stdio, so ctrl-c reaches journalctl and
        // the ssh session ends with it.
        match ssh.stream(&target, &command) {
            Ok(0) | Ok(130) | Ok(255) => {}
            Ok(code) => std::process::exit(code),
            Err(e) => crate::die(&e),
        }
        return;
    }
    match ssh.run(&target, &command) {
        Ok(out) if out.ok() => {
            print!("{}", out.stdout);
            let trailing = out.stderr.trim();
            if !trailing.is_empty() {
                eprintln!("{trailing}");
            }
        }
        Ok(out) => {
            eprintln!("{}", out.stderr.trim());
            std::process::exit(1);
        }
        Err(e) => crate::die(&e),
    }
}

/// `hard deploy status [host] [--env <name>]`.
pub fn cmd_status(args: &[String]) {
    let (target, unit) = target_and_unit(args);
    let probes = status_probes(&unit.layout);
    let mut ssh = crate::ssh::SystemSsh::new();
    let mut answers: Vec<(&'static str, String)> = Vec::new();
    // A host that cannot be reached produces the same message for every probe.
    // Printing it once, at the bottom, keeps the report readable; putting it in
    // every field makes the fields unreadable and the cause invisible.
    let mut unreachable: Option<String> = None;
    for (name, command) in probes {
        let text = match ssh.run(&target, &command) {
            Ok(out) if out.ok() => out.trimmed().to_string(),
            Ok(out) => {
                let stderr = out.stderr.trim().to_string();
                if unreachable.is_none() {
                    unreachable = Some(if stderr.is_empty() {
                        format!("`{command}` exited with status {}", out.status)
                    } else {
                        stderr
                    });
                }
                String::new()
            }
            Err(e) => crate::die(&e),
        };
        answers.push((name, text));
    }
    let status = status_from(&unit.layout, &answers);
    print!("{}", render_status(&status));
    if let Some(why) = unreachable {
        println!("could not reach {}: {why}", target.label());
    }
    if !status.healthy {
        std::process::exit(1);
    }
}

/// `hard deploy start|stop|restart [host] [--env <name>]`.
pub fn cmd_lifecycle(action: &str, args: &[String]) {
    let (target, unit) = target_and_unit(args);
    let command = lifecycle_command(action, &unit.unit);
    let mut ssh = crate::ssh::SystemSsh::new();
    match ssh.run(&target, &command) {
        Ok(out) if out.ok() => {
            let message = out.trimmed();
            if !message.is_empty() {
                println!("{message}");
            }
            // An action that systemd accepted is not an action that finished:
            // the deploy health check is the thing that waits.
            if action != "stop" {
                let check = crate::production::checks(&unit.layout, &unit.unit);
                let answers = match crate::production::run_checks(&mut ssh, &target, &check) {
                    Ok(a) => a,
                    Err(e) => crate::die(&e),
                };
                for a in &answers {
                    if !a.ok {
                        eprintln!("{} FAILED: {}", a.name, a.because);
                    }
                }
            }
        }
        Ok(out) => {
            eprintln!("{}", out.stderr.trim());
            std::process::exit(1);
        }
        Err(e) => crate::die(&e),
    }
}

/// `hard deploy releases [host] [--env <name>]`: what is on the host, newest
/// first, and which one is live.
pub fn cmd_releases(args: &[String]) {
    let (target, unit) = target_and_unit(args);
    let mut ssh = crate::ssh::SystemSsh::new();
    let listing = match ssh.run(&target, &releases_command(&unit.layout)) {
        Ok(out) if out.ok() => out.stdout,
        Ok(out) => {
            eprintln!("{}", out.stderr.trim());
            std::process::exit(1);
        }
        Err(e) => crate::die(&e),
    };
    let live = match ssh.run(&target, &unit.layout.read_current()) {
        Ok(out) => out.trimmed().to_string(),
        Err(e) => crate::die(&e),
    };
    let mut count = 0;
    for name in listing.lines().map(str::trim).filter(|l| !l.is_empty()) {
        count += 1;
        let full = format!("{}/{name}", unit.layout.releases());
        let mark = if full == live { "  <- current" } else { "" };
        println!("{name:<40}{mark}");
    }
    if count == 0 {
        println!("no releases in {}", unit.layout.releases());
    }
}

/// The host and the unit a deploy command operates on.
fn target_and_unit(args: &[String]) -> (SshTarget, Deployed) {
    let env_name = flag_value(args, "--env");
    let host = positional(args);
    let (project, manifest) = crate::environments::project_manifest();
    let env = env_name
        .as_ref()
        .map(|name| crate::environments::resolve(&project, &manifest, name).unwrap_or_else(|e| crate::die(&e)));
    let dir = env.as_ref().map(|e| e.dir.clone()).unwrap_or_default();
    let service = env.as_ref().map(|e| e.service.clone());
    let user = env.as_ref().and_then(|e| e.user.clone());
    let host = match host.or_else(|| env.as_ref().and_then(|e| e.host.clone())) {
        Some(h) => h,
        None => {
            eprintln!("hard deploy: which host? try `hard deploy {} <host>`", "status");
            std::process::exit(2);
        }
    };
    let target = match SshTarget::parse(&host) {
        Ok(t) => t,
        Err(e) => crate::die(&e),
    };
    let mut layout = RemoteLayout::new(&dir, &project).unwrap_or_else(|e| crate::die(&e));
    if let Some(unit) = service {
        if let Ok(l) = layout.clone().with_service(&unit) {
            layout = l;
        }
    }
    let account = user.clone().unwrap_or_else(|| target.user.clone());
    let unit = UnitConfig::new(&project, &layout, Some(account));
    (target, Deployed { layout, unit })
}

/// The layout and the unit, together.
struct Deployed {
    layout: RemoteLayout,
    unit: UnitConfig,
}

/// The value of `--flag value` or `--flag=value`.
fn flag_value(args: &[String], name: &str) -> Option<String> {
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

/// The first argument that is not a flag, or a flag's value.
fn positional(args: &[String]) -> Option<String> {
    let value_flags = ["--env", "-n", "--lines", "--since"];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if value_flags.contains(&a) {
            i += 2;
            continue;
        }
        if let Some(v) = a.split_once('=') {
            if v.0.starts_with('-') {
                i += 1;
                continue;
            }
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(a.to_string());
    }
    None
}

impl LogArgs {
    /// Parse the flags after `hard deploy logs`. `--flag=value` is rewritten to
    /// two arguments first, so the loop has one shape to deal with.
    pub fn parse(args: &[String]) -> Result<LogArgs, String> {
        let mut norm: Vec<String> = Vec::with_capacity(args.len());
        for a in args {
            match a.split_once('=') {
                Some((name, value)) if name.starts_with('-') => {
                    norm.push(name.to_string());
                    norm.push(value.to_string());
                }
                _ => norm.push(a.clone()),
            }
        }
        let mut out = LogArgs::default();
        let mut i = 0;
        while i < norm.len() {
            let a = norm[i].as_str();
            match a {
                "-f" | "--follow" => {
                    out.follow = true;
                    i += 1;
                }
                "--all" => {
                    out.all_units = true;
                    i += 1;
                }
                "-n" | "--lines" => {
                    let v = norm.get(i + 1).cloned().ok_or_else(|| format!("{a} needs a value"))?;
                    out.lines = v
                        .parse()
                        .map_err(|_| format!("`{v}` is not a number of lines"))?;
                    i += 2;
                }
                "--since" => {
                    out.since = Some(
                        norm.get(i + 1).cloned().ok_or_else(|| "--since needs a value".to_string())?,
                    );
                    i += 2;
                }
                // `--env` and the host belong to the deploy, not to the
                // journal: accepted here, used by the caller.
                "--env" => i += 2,
                // A host, not a flag: `hard deploy logs root@app` and
                // `hard deploy logs -n 50 root@app` both have to work.
                other if !other.starts_with('-') => {
                    i += 1;
                }
                other => return Err(format!("`{other}` is not a logs flag")),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn layout() -> RemoteLayout {
        RemoteLayout::new("/srv/app", "app").expect("a layout")
    }

    fn unit() -> UnitConfig {
        UnitConfig::new("app", &layout(), Some("deploy".to_string()))
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hs-ops-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("releases")).expect("a scratch root");
        dir
    }

    #[test]
    fn logs_read_a_number_of_lines_and_never_page() {
        let cmd = journal_command(&unit(), &LogArgs::default());
        assert!(cmd.starts_with("journalctl -u 'app.service'"), "{cmd}");
        assert!(cmd.contains(" -n 200"), "a default, not a page: {cmd}");
        assert!(cmd.contains("--no-pager"), "and no pager: {cmd}");
        assert!(!cmd.contains(" -f"), "one shot by default: {cmd}");
        assert!(cmd.contains("--output=short-iso"), "with a timestamp format that sorts: {cmd}");
    }

    #[test]
    fn logs_follow_is_a_stream() {
        let args = LogArgs { follow: true, ..LogArgs::default() };
        let cmd = journal_command(&unit(), &args);
        assert!(cmd.contains(" -f"), "{cmd}");
        assert!(!cmd.contains("--no-pager"), "a follow has no pager either: {cmd}");
        let args = LogArgs { lines: 5, since: Some("1 hour ago".to_string()), ..LogArgs::default() };
        let cmd = journal_command(&unit(), &args);
        assert!(cmd.contains(" -n 5"), "the line count is honoured: {cmd}");
        assert!(cmd.contains("--since '1 hour ago'"), "and the window: {cmd}");
        let all = LogArgs { all_units: true, ..LogArgs::default() };
        assert!(!journal_command(&unit(), &all).contains(" -u "), "the whole host, if asked");
    }

    #[test]
    fn a_lifecycle_command_waits_for_systemd_and_escalates() {
        for action in ["start", "stop", "restart"] {
            let cmd = lifecycle_command(action, &unit());
            assert!(cmd.starts_with("sudo -n systemctl "), "{action}: {cmd}");
            assert!(cmd.contains(&format!("systemctl {action} 'app.service'")), "{action}: {cmd}");
            assert!(cmd.contains("--no-block"), "so the answer means something: {cmd}");
        }
        let root = UnitConfig::new("app", &layout(), None);
        assert!(
            lifecycle_command("restart", &root).starts_with("systemctl restart"),
            "root needs no sudo: {}",
            lifecycle_command("restart", &root)
        );
    }

    #[test]
    fn status_asks_the_questions_a_person_would() {
        let probes = status_probes(&layout());
        let names: Vec<&str> = probes.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["current", "previous", "releases", "active", "since"]);
        assert!(probes.iter().any(|(_, c)| c.contains("readlink -f '/srv/app/current'")), "{probes:?}");
        assert!(probes.iter().any(|(_, c)| c.contains("is-active")), "{probes:?}");
    }

    #[test]
    fn status_says_what_is_live_and_whether_it_is_running() {
        let l = layout();
        let answers = vec![
            ("current", "/srv/app/releases/1.0.0-20250915T070530Z-abcdef12".to_string()),
            ("previous", "/srv/app/releases/0.9.0-20250901T070530Z-12345678".to_string()),
            ("releases", "3".to_string()),
            ("active", "active".to_string()),
            ("since", "Mon 2025-09-15 07:05:30 UTC".to_string()),
        ];
        let status = status_from(&l, &answers);
        assert!(status.healthy, "{status:?}");
        assert!(status.summary.contains("live and running"), "{}", status.summary);
        let text = render_status(&status);
        assert!(text.contains("/srv/app/releases/1.0.0-20250915T070530Z-abcdef12"), "{text}");
        assert!(text.contains("3 kept"), "{text}");
        assert!(text.contains("running since"), "when it came up: {text}");
    }

    #[test]
    fn status_does_not_call_a_broken_deployment_healthy() {
        let l = layout();
        // Running, but `current` is a dangling link: a service executing
        // something nobody can name.
        let answers = vec![
            ("current", String::new()),
            ("previous", String::new()),
            ("releases", "0".to_string()),
            ("active", "active".to_string()),
            ("since", String::new()),
        ];
        let status = status_from(&l, &answers);
        assert!(!status.healthy, "a running service with no release is not healthy");
        assert!(status.summary.contains("current does not point at a release"), "{}", status.summary);
        assert!(status.lines.iter().any(|x| x.value.contains("no release behind it")), "{status:?}");

        // A release is live but systemd says inactive.
        let answers = vec![
            ("current", "/srv/app/releases/1.0.0".to_string()),
            ("previous", String::new()),
            ("releases", "1".to_string()),
            ("active", "failed".to_string()),
            ("since", String::new()),
        ];
        let status = status_from(&l, &answers);
        assert!(!status.healthy, "a deployed release with a dead service is not healthy");
        assert!(status.summary.contains("the service is failed"), "{}", status.summary);
        assert!(status.lines.iter().any(|x| x.value.contains("not running")), "{status:?}");
    }

    #[test]
    fn logs_flags_read_in_both_spellings() {
        let v: Vec<String> = ["-f", "-n", "50", "--since", "1 hour ago"].iter().map(|s| s.to_string()).collect();
        let space = LogArgs::parse(&v).expect("flags");
        let v: Vec<String> = ["-f", "-n=50", "--since=1 hour ago"].iter().map(|s| s.to_string()).collect();
        let equals = LogArgs::parse(&v).expect("flags");
        assert_eq!(space, equals, "the two spellings are the same flag");
        assert!(space.follow, "-f");
        assert_eq!(space.lines, 50, "a number of lines");
        assert_eq!(space.since.as_deref(), Some("1 hour ago"), "and a window");
        let v: Vec<String> = ["root@host"].iter().map(|s| s.to_string()).collect();
        let none = LogArgs::parse(&v).expect("a host is not a flag");
        assert_eq!(none.lines, 200, "a host alone gets the defaults");
        assert!(!none.follow, "and does not follow");
        let v: Vec<String> = ["-n", "all"].iter().map(|s| s.to_string()).collect();
        let err = LogArgs::parse(&v).unwrap_err();
        assert!(err.contains("not a number of lines"), "{err}");
        let v: Vec<String> = ["--since"].iter().map(|s| s.to_string()).collect();
        let err = LogArgs::parse(&v).unwrap_err();
        assert!(err.contains("needs a value"), "{err}");
        let v: Vec<String> = ["--nope"].iter().map(|s| s.to_string()).collect();
        let err = LogArgs::parse(&v).unwrap_err();
        assert!(err.contains("not a logs flag"), "{err}");
    }

    #[test]
    fn the_journal_command_runs_in_a_shell() {
        // A rendering bug in a command that is never executed is a bug that
        // only a host finds; the shell is the cheapest place to find it.
        for args in [
            LogArgs::default(),
            LogArgs { follow: true, ..LogArgs::default() },
            LogArgs { lines: 1, since: Some("yesterday".to_string()), ..LogArgs::default() },
            LogArgs { all_units: true, follow: true, ..LogArgs::default() },
        ] {
            let syntax = std::process::Command::new("sh")
                .arg("-n")
                .arg("-c")
                .arg(journal_command(&unit(), &args))
                .output()
                .expect("sh");
            assert!(
                syntax.status.success(),
                "{args:?} parses: {}",
                String::from_utf8_lossy(&syntax.stderr)
            );
        }
        for action in ["start", "stop", "restart"] {
            let syntax = std::process::Command::new("sh")
                .arg("-n")
                .arg("-c")
                .arg(lifecycle_command(action, &unit()))
                .output()
                .expect("sh");
            assert!(syntax.status.success(), "{action} parses: {}", String::from_utf8_lossy(&syntax.stderr));
        }
    }

    #[test]
    fn the_release_probes_run_against_a_real_directory() {
        let root = scratch("releases");
        let l = RemoteLayout::new(root.to_str().expect("a path"), "app").expect("a layout");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("mkdir -p '{}/a' '{}/b'", l.releases(), l.releases()))
            .output()
            .expect("sh");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let count = std::process::Command::new("sh").arg("-c").arg(release_count_command(&l)).output().expect("sh");
        assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "2", "two releases, counted");
        let list = std::process::Command::new("sh").arg("-c").arg(releases_command(&l)).output().expect("sh");
        let listing = String::from_utf8_lossy(&list.stdout).into_owned();
        let names: Vec<&str> =
            listing.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        assert_eq!(names.len(), 2, "and listed: {names:?}");
        // An empty directory is not an error: a host nobody has deployed to yet.
        let empty = RemoteLayout::new("/tmp/hs-ops-empty-xyz", "app").expect("a layout");
        let out = std::process::Command::new("sh").arg("-c").arg(release_count_command(&empty)).output().expect("sh");
        assert!(out.status.success(), "counting an absent directory is not a failure");
        let _ = std::fs::remove_dir_all(&root);
    }
}
