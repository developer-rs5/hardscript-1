//! `hard deploy rollback` and `hard deploy prune` (M7.7).
//!
//! A release is only useful if going back to it is cheap. Everything the layout
//! does in `activate` is already reversible: `previous` is the release that was
//! live before the last deploy, and the unit runs `current`. Rolling back is
//! therefore one symlink swap and a restart -- but the part that matters is the
//! failure path. A rollback that swaps in an old release, finds it unhealthy,
//! and stops leaves a service that is down, which is strictly worse than the
//! deploy that prompted the rollback. So a failed health check puts the release
//! that was live back, restarts it, and reports both: what failed, and that the
//! service is back.
//!
//! Pruning is the other half. Releases accumulate; nobody deletes them; and a
//! host that fills its disk is a host whose next deploy fails halfway. Pruning
//! is therefore never implicit: `prune` prints what it would remove and does
//! nothing until it is told `--yes`.

use crate::release::{
    execute, sh_quote, DeployConfig, DeployFailure, DeployPlan, DeployReport, DeployStep,
    HealthCheck, RealSleeper, RemoteLayout, Sleeper, StepKind,
};
use crate::ssh::{Ssh, SshTarget};

/// What a rollback is going to do, decided before anything happens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RollbackPlan {
    /// The release that is live now, and the one to go back to.
    pub from: String,
    pub to: String,
    /// Whether the failed release is kept as `previous`. Rolling back over it
    /// and keeping it means a second rollback returns to where it was.
    pub keep_from: bool,
}

/// Decide what to roll back to.
///
/// Errors are the point of half of this: rolling back to a release that is not
/// on the host, or on a host with nothing to go back to, is a mistake worth
/// stopping for rather than a swap that leaves the service pointing nowhere.
pub fn plan(
    layout: &RemoteLayout,
    current: &str,
    previous: &str,
    requested: Option<&str>,
) -> Result<RollbackPlan, String> {
    let name = |path: &str| path.rsplit('/').next().unwrap_or("").to_string();
    let from = name(current);
    let to = match requested {
        Some(want) => want.to_string(),
        None => name(previous),
    };
    if to.is_empty() {
        return Err(format!(
            "{} has no previous release: this is the first deploy on this host, so there is nothing to go back to",
            layout.root
        ));
    }
    if from == to {
        return Err(format!("{to} is already live"));
    }
    if !current.contains(&layout.releases()) {
        return Err(format!("{current} is not a release; nothing is deployed to roll back from"));
    }
    Ok(RollbackPlan { from, to, keep_from: true })
}

/// The steps: swap, restart, verify. The swap keeps the release being replaced
/// as `previous`, so the rollback is itself reversible: rolling back a second
/// time returns to where the first one started.
pub fn steps(cfg: &DeployConfig, plan: &RollbackPlan) -> Vec<DeployStep> {
    let target = format!("{}/{}", cfg.layout.releases(), plan.to);
    let swap = |to: &str| {
        let cur = sh_quote(&cfg.layout.current());
        let prev = sh_quote(&cfg.layout.previous());
        format!(
            "cur={cur}; \
             if [ -L \"$cur\" ]; then ln -sfn \"$(readlink -f \"$cur\")\" {prev}; fi; \
             ln -sfn {} \"$cur.new\" && mv -T \"$cur.new\" \"$cur\" && readlink -f \"$cur\"",
            sh_quote(to)
        )
    };
    let mut steps = vec![DeployStep {
        kind: StepKind::Activate,
        detail: format!("current -> {}", plan.to),
        command: swap(&target),
        upload: None,
    }];
    steps.push(DeployStep {
        kind: StepKind::Restart,
        detail: cfg.layout.service.clone(),
        command: cfg.restart_command(),
        upload: None,
    });
    let health = HealthCheck::new(cfg);
    steps.push(DeployStep {
        kind: StepKind::Health,
        detail: health.description.clone(),
        command: health.command.clone(),
        upload: None,
    });
    steps
}

/// Put a release back after a rollback that failed: `current` goes back to the
/// release it came from and `previous` becomes the one that failed, which is
/// the state the host was in before anybody touched it.
pub fn restore_command(cfg: &DeployConfig, plan: &RollbackPlan) -> String {
    let to_failed = format!("{}/{}", cfg.layout.releases(), plan.to);
    format!(
        "ln -sfn {} {} && mv -T {}.new {}.new && ln -sfn {} {} && readlink -f {}",
        sh_quote(&format!("{}/{}", cfg.layout.releases(), plan.from)),
        sh_quote(&cfg.layout.current()),
        cfg.layout.current(),
        cfg.layout.current(),
        sh_quote(&to_failed),
        sh_quote(&cfg.layout.previous()),
        sh_quote(&cfg.layout.current())
    )
}

/// Run a rollback. A health check that fails puts the service back on the
/// release it came from, restarts it, and reports what happened to both.
pub fn apply(
    ssh: &mut dyn Ssh,
    sleeper: &mut dyn Sleeper,
    target: &SshTarget,
    cfg: &DeployConfig,
    plan: &RollbackPlan,
) -> Result<DeployReport, DeployFailure> {
    let id = crate::release::ReleaseId::parse(&plan.to)
        .map_err(|e| DeployFailure {
            id: plan.to.clone(),
            kind: StepKind::Activate,
            detail: "roll back".to_string(),
            command: String::new(),
            message: e,
            recoverable: None,
        })?;
    let deploy_plan = DeployPlan { id, steps: steps(cfg, plan) };
    match execute(ssh, sleeper, target, cfg, &deploy_plan) {
        Ok(report) => Ok(report),
        Err(failure) => {
            // Only a release that is actually swapped out can be restored; a
            // failure before the swap left the service where it was.
            if failure.kind != StepKind::Health {
                return Err(failure);
            }
            let _ = ssh.run(target, &restore_command(cfg, plan));
            let _ = ssh.run(target, &cfg.restart_command());
            Err(DeployFailure {
                id: failure.id.clone(),
                kind: failure.kind,
                detail: failure.detail.clone(),
                command: failure.command.clone(),
                message: format!(
                    "{}\nthe rolled-back release did not come up, so {} is live again",
                    failure.message, plan.from
                ),
                recoverable: Some(format!(
                    "the releases are still on disk in {}; read `hard deploy logs` before trying again",
                    cfg.layout.releases()
                )),
            })
        }
    }
}

/// Which releases a prune would remove: everything older than the newest
/// `keep`, never `current`, never `previous`.
///
/// The two exclusions are the whole safety argument. A prune that deleted the
/// release the service is running would take it down, and one that deleted
/// `previous` would take the rollback away; both are one missing comparison
/// away, and neither is visible until somebody needs it.
pub fn prune_targets(
    listing: &str,
    current: &str,
    previous: &str,
    keep: usize,
) -> Vec<String> {
    let live = |path: &str| path.rsplit('/').next().unwrap_or("").to_string();
    let current = live(current);
    let previous = live(previous);
    let mut names: Vec<String> =
        listing.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
    if names.len() <= keep {
        return Vec::new();
    }
    // The listing arrives newest first, so the tail is what is old enough to go.
    let old: Vec<String> = names.split_off(keep);
    old.into_iter().filter(|n| *n != current && *n != previous).collect()
}

/// The `rm` commands for a prune, as a shell script.
pub fn prune_script(layout: &RemoteLayout, targets: &[String]) -> String {
    if targets.is_empty() {
        return String::new();
    }
    let mut out = format!("set -e; cd {}", crate::release::sh_quote(&layout.releases()));
    for name in targets {
        out.push_str(&format!("; rm -rf {}", crate::release::sh_quote(name)));
    }
    out
}

/// `hard deploy rollback [host] [--env <name>] [--to <release>] [--yes]`.
pub fn cmd_rollback(args: &[String]) {
    let (host, env_name, to, confirmed) = rollback_args(args);
    let (project, manifest) = crate::environments::project_manifest();
    let env = env_name
        .as_ref()
        .map(|name| crate::environments::resolve(&project, &manifest, name).unwrap_or_else(|e| crate::die(&e)));
    let dir = env.as_ref().map(|e| e.dir.clone()).unwrap_or_default();
    let layout = RemoteLayout::new(&dir, &project).unwrap_or_else(|e| crate::die(&e));
    let host = match host.or_else(|| env.as_ref().and_then(|e| e.host.clone())) {
        Some(h) => h,
        None => {
            eprintln!("hard deploy rollback: which host? try `hard deploy rollback deploy@app.example.com`");
            std::process::exit(2);
        }
    };
    let target = match SshTarget::parse(&host) {
        Ok(t) => t,
        Err(e) => crate::die(&e),
    };
    // The same config a deploy builds, so the rollback restarts and health-checks
    // exactly the way the deploy did.
    let ssh_args = crate::deploy::SshArgs {
        host: target.label(),
        env: env_name.clone(),
        health: true,
        migrate: false,
        build: false,
        ..crate::deploy::SshArgs::default()
    };
    let (source, _) = crate::find_target(args);
    let cfg = crate::deploy::project_deploy_config(&source, &ssh_args, env.as_ref())
        .unwrap_or_else(|e| crate::die(&e));

    let mut ssh = crate::ssh::SystemSsh::new();
    let current = read(&mut ssh, &target, &layout.read_current())
        .unwrap_or_else(|e| crate::die(&format!("cannot read the host: {e}")));
    let previous = read(&mut ssh, &target, &layout.read_current_previous())
        .unwrap_or_else(|e| crate::die(&format!("cannot read the host: {e}")));
    let plan = plan(&layout, &current, &previous, to.as_deref()).unwrap_or_else(|e| crate::die(&e));

    if !confirmed {
        println!("would roll {} back to {} on {}", plan.from, plan.to, target.label());
        println!("  {} is live now", current);
        println!("  {} would go back to {}", layout.current(), plan.to);
        println!("run it with --yes, or `hard deploy rollback --to <release> --yes`");
        return;
    }

    let mut sleeper = RealSleeper;
    match apply(&mut ssh, &mut sleeper, &target, &cfg, &plan) {
        Ok(report) => {
            println!("rolled back {} to {}", plan.from, plan.to);
            println!("  current {}", report.current);
        }
        Err(e) => {
            eprintln!("hard deploy rollback: {e}");
            std::process::exit(1);
        }
    }
}

/// `hard deploy prune [host] [--env <name>] [--keep N] [--yes]`.
pub fn cmd_prune(args: &[String]) {
    let (host, env_name, keep, confirmed) = prune_args(args);
    let (project, manifest) = crate::environments::project_manifest();
    let env = env_name
        .as_ref()
        .map(|name| crate::environments::resolve(&project, &manifest, name).unwrap_or_else(|e| crate::die(&e)));
    let dir = env.as_ref().map(|e| e.dir.clone()).unwrap_or_default();
    let layout = RemoteLayout::new(&dir, &project).unwrap_or_else(|e| crate::die(&e));
    let host = match host.or_else(|| env.as_ref().and_then(|e| e.host.clone())) {
        Some(h) => h,
        None => {
            eprintln!("hard deploy prune: which host? try `hard deploy prune deploy@app.example.com`");
            std::process::exit(2);
        }
    };
    let target = match SshTarget::parse(&host) {
        Ok(t) => t,
        Err(e) => crate::die(&e),
    };
    let mut ssh = crate::ssh::SystemSsh::new();
    let current = read(&mut ssh, &target, &layout.read_current())
        .unwrap_or_else(|e| crate::die(&format!("cannot read the host: {e}")));
    let previous = read(&mut ssh, &target, &layout.read_current_previous())
        .unwrap_or_else(|e| crate::die(&format!("cannot read the host: {e}")));
    let listing = read(&mut ssh, &target, &crate::operations::releases_command(&layout))
        .unwrap_or_else(|e| crate::die(&format!("cannot read the host: {e}")));
    let targets = prune_targets(&listing, &current, &previous, keep);
    if targets.is_empty() {
        println!("nothing to prune: {} releases kept", keep);
        return;
    }
    if !confirmed {
        println!("would remove {} of {} releases in {}:", targets.len(), listing.lines().filter(|l| !l.trim().is_empty()).count(), layout.releases());
        for name in &targets {
            println!("  {name}");
        }
        println!("keeping current ({}) and previous", basename(&current));
        println!("run it with --yes");
        return;
    }
    let script = prune_script(&layout, &targets);
    match ssh.run(&target, &script) {
        Ok(out) if out.ok() => {
            println!("removed {} releases, {} kept", targets.len(), keep);
            for name in &targets {
                println!("  {name}");
            }
        }
        Ok(out) => {
            eprintln!("{}", out.stderr.trim());
            std::process::exit(1);
        }
        Err(e) => crate::die(&e),
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or("")
}

/// Read one value from the host.
///
/// An empty answer and a host that cannot be reached are different facts, and
/// only one of them means "there is nothing here". A rollback that cannot tell
/// them apart tells the operator to look for a first deploy on a host it never
/// reached.
fn read(ssh: &mut dyn Ssh, target: &SshTarget, command: &str) -> Result<String, String> {
    match ssh.run(target, command) {
        Ok(out) if out.ok() => Ok(out.trimmed().to_string()),
        Ok(out) => {
            let stderr = out.stderr.trim();
            Err(if stderr.is_empty() {
                format!("`{command}` on {} exited with status {}", target.label(), out.status)
            } else {
                stderr.to_string()
            })
        }
        Err(e) => Err(e),
    }
}

/// `--env <name>`, `--to <release>`, `--yes` and a host.
fn rollback_args(args: &[String]) -> (Option<String>, Option<String>, Option<String>, bool) {
    let mut host = None;
    let mut env = None;
    let mut to = None;
    let mut yes = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let value = |i: &mut usize| args.get(*i + 1).cloned();
        match a {
            "--env" => {
                env = value(&mut i);
                i += 2;
            }
            "--to" => {
                to = value(&mut i);
                i += 2;
            }
            "--yes" | "-y" => {
                yes = true;
                i += 1;
            }
            other if other.starts_with('-') => {
                eprintln!("hard deploy rollback: `{other}` is not a rollback flag");
                std::process::exit(2);
            }
            other => {
                if host.is_some() {
                    eprintln!("hard deploy rollback: `{other}` is a second host");
                    std::process::exit(2);
                }
                host = Some(other.to_string());
                i += 1;
            }
        }
    }
    (host, env, to, yes)
}

/// `--env <name>`, `--keep N`, `--yes` and a host.
fn prune_args(args: &[String]) -> (Option<String>, Option<String>, usize, bool) {
    let mut host = None;
    let mut env = None;
    let mut keep = 5usize;
    let mut yes = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--env" => {
                env = args.get(i + 1).cloned();
                if env.is_none() {
                    eprintln!("hard deploy prune: --env needs a value");
                    std::process::exit(2);
                }
                i += 2;
            }
            "--keep" => {
                let v = match args.get(i + 1).and_then(|v| v.parse::<usize>().ok()) {
                    Some(v) => v,
                    None => {
                        eprintln!("hard deploy prune: --keep needs a number of releases");
                        std::process::exit(2);
                    }
                };
                keep = v;
                i += 2;
            }
            "--yes" | "-y" => {
                yes = true;
                i += 1;
            }
            other if other.starts_with('-') => {
                eprintln!("hard deploy prune: `{other}` is not a prune flag");
                std::process::exit(2);
            }
            other => {
                host = Some(other.to_string());
                i += 1;
            }
        }
    }
    (host, env, keep, yes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::MockSsh;
    use std::path::Path;

    fn layout() -> RemoteLayout {
        RemoteLayout::new("/srv/app", "app").expect("a layout")
    }

    fn config() -> DeployConfig {
        DeployConfig { layout: layout(), user: "deploy".to_string(), ..DeployConfig::default() }
    }

    #[test]
    fn a_rollback_goes_to_the_previous_release_by_default() {
        let plan = plan(
            &layout(),
            "/srv/app/releases/1.0.0-aaaaaaaa",
            "/srv/app/releases/0.9.0-bbbbbbbb",
            None,
        )
        .expect("a plan");
        assert_eq!(plan.from, "1.0.0-aaaaaaaa", "where it is");
        assert_eq!(plan.to, "0.9.0-bbbbbbbb", "and where it is going");
        assert!(plan.keep_from, "and the release it replaced is kept");
    }

    #[test]
    fn a_rollback_can_name_a_release() {
        let plan = plan(
            &layout(),
            "/srv/app/releases/1.0.0-aaaaaaaa",
            "/srv/app/releases/0.9.0-bbbbbbbb",
            Some("0.8.0-cccccccc"),
        )
        .expect("a plan");
        assert_eq!(plan.to, "0.8.0-cccccccc", "the release asked for");
    }

    #[test]
    fn a_rollback_stops_when_there_is_nothing_to_go_back_to() {
        let err = plan(&layout(), "/srv/app/releases/1.0.0", "", None).unwrap_err();
        assert!(err.contains("first deploy"), "{err}");
        let err =
            plan(&layout(), "/srv/app/releases/1.0.0", "/srv/app/releases/0.9.0", Some("1.0.0"))
                .unwrap_err();
        assert!(err.contains("already live"), "{err}");
        let err = plan(&layout(), "", "/srv/app/releases/0.9.0", None).unwrap_err();
        assert!(err.contains("nothing is deployed"), "{err}");
    }

    #[test]
    fn a_rollback_swaps_restarts_and_checks() {
        let plan = plan(
            &layout(),
            "/srv/app/releases/1.0.0-aaaaaaaa",
            "/srv/app/releases/0.9.0-bbbbbbbb",
            None,
        )
        .expect("a plan");
        let steps = steps(&config(), &plan);
        let kinds: Vec<StepKind> = steps.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, vec![StepKind::Activate, StepKind::Restart, StepKind::Health]);
        let activate = &steps[0].command;
        assert!(activate.contains("'/srv/app/releases/0.9.0-bbbbbbbb'"), "to the old one: {activate}");
        assert!(activate.contains("mv -T"), "and atomically: {activate}");
        // The release being replaced becomes `previous`, so the rollback is
        // reversible: rolling back again returns to where it was.
        assert!(activate.contains("'/srv/app/previous'"), "{activate}");
        assert!(steps[1].command.starts_with("sudo -n systemctl restart"), "{}", steps[1].command);
    }

    #[test]
    fn a_healthy_rollback_reports_the_release_it_landed_on() {
        let plan = plan(
            &layout(),
            "/srv/app/releases/1.0.0-aaaaaaaa",
            "/srv/app/releases/0.9.0-bbbbbbbb",
            None,
        )
        .expect("a plan");
        let mut ssh = MockSsh::new()
            .ok_everything()
            .answer("readlink -f \"$cur\"", "/srv/app/releases/0.9.0-bbbbbbbb");
        let target = SshTarget::parse("root@h").expect("a target");
        let mut sleeper = crate::release::NoSleep::default();
        let cfg = config();
        let report = apply(&mut ssh, &mut sleeper, &target, &cfg, &plan).expect("it worked");
        assert_eq!(report.current, "/srv/app/releases/0.9.0-bbbbbbbb", "and says where current is");
    }

    #[test]
    fn a_rollback_that_does_not_come_up_puts_the_service_back() {
        let plan = plan(
            &layout(),
            "/srv/app/releases/1.0.0-aaaaaaaa",
            "/srv/app/releases/0.9.0-bbbbbbbb",
            None,
        )
        .expect("a plan");
        let mut ssh = MockSsh::new()
            .ok_everything()
            .answer("readlink -f \"$cur\"", "/srv/app/releases/0.9.0-bbbbbbbb")
            .fail("http://", 7, "connection refused");
        let target = SshTarget::parse("root@h").expect("a target");
        let mut sleeper = crate::release::NoSleep::default();
        let cfg = config();
        let err = apply(&mut ssh, &mut sleeper, &target, &cfg, &plan).unwrap_err();
        assert_eq!(err.kind, StepKind::Health, "the health check is what failed");
        let log = ssh.command_log();
        assert!(
            log.matches("systemctl restart").count() >= 2,
            "the old release is started again: {log}"
        );
        assert!(log.contains("/srv/app/releases/1.0.0-aaaaaaaa"), "and put back: {log}");
        assert!(err.message.contains("1.0.0-aaaaaaaa is live again"), "and says so: {err}");
        assert!(err.recoverable.is_some(), "with a next step");
    }

    #[test]
    fn a_prune_keeps_the_newest_and_never_the_two_that_matter() {
        let listing = "1.0.0-c\n1.0.0-b\n1.0.0-a\n0.9.0-z\n0.8.0-y\n0.7.0-x\n";
        let targets =
            prune_targets(listing, "/srv/app/releases/1.0.0-c", "/srv/app/releases/1.0.0-b", 2);
        assert_eq!(
            targets,
            vec!["1.0.0-a", "0.9.0-z", "0.8.0-y", "0.7.0-x"],
            "everything older than the newest two"
        );

        // A release that is neither newest nor the current one is still removed.
        let targets =
            prune_targets(listing, "/srv/app/releases/1.0.0-c", "/srv/app/releases/0.8.0-y", 3);
        assert!(!targets.contains(&"0.8.0-y".to_string()), "previous is never removed: {targets:?}");
        assert!(!targets.contains(&"1.0.0-c".to_string()), "current is never removed: {targets:?}");
        assert!(targets.contains(&"0.9.0-z".to_string()), "but the rest is: {targets:?}");

        // With fewer releases than the keep count there is nothing to do.
        assert!(
            prune_targets("1.0.0-c\n1.0.0-b\n", "/srv/app/releases/1.0.0-c", "", 5).is_empty(),
            "a host with three releases is not pruned by --keep 5"
        );
        assert!(prune_targets("", "", "", 5).is_empty(), "and an empty host is not an error");
    }

    #[test]
    fn the_prune_script_deletes_exactly_what_was_listed() {
        let script = prune_script(&layout(), &["0.9.0-z".to_string(), "0.8.0-y".to_string()]);
        assert!(script.starts_with("set -e; cd '/srv/app/releases'"), "{script}");
        assert!(script.contains("rm -rf '0.9.0-z'"), "{script}");
        assert!(script.contains("rm -rf '0.8.0-y'"), "{script}");
        assert!(!script.contains("rm -rf '1.0.0-c'"), "and nothing else: {script}");
        assert_eq!(prune_script(&layout(), &[]), "", "nothing to delete is no script");
    }

    #[test]
    fn the_prune_script_runs_in_a_shell_and_leaves_the_two_live_ones() {
        let root = std::env::temp_dir().join(format!("hs-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let l = RemoteLayout::new(root.to_str().expect("a path"), "app").expect("a layout");
        for name in ["1.0.0-c", "1.0.0-b", "0.9.0-z", "0.8.0-y"] {
            std::fs::create_dir_all(format!("{}/{name}", l.releases())).expect("a release directory");
        }
        // The listing arrives newest first, the way `ls -1t` returns it.
        let mut names: Vec<String> = std::fs::read_dir(l.releases())
            .expect("the releases")
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect();
        names.sort();
        names.reverse();
        let listing = names.join("\n");
        let targets = prune_targets(&format!("{listing}\n"), &format!("{}/1.0.0-c", l.releases()), &format!("{}/1.0.0-b", l.releases()), 2);
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(prune_script(&l, &targets))
            .output()
            .expect("sh");
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(Path::new(&format!("{}/1.0.0-c", l.releases())).exists(), "current survived");
        assert!(Path::new(&format!("{}/1.0.0-b", l.releases())).exists(), "previous survived");
        assert!(!Path::new(&format!("{}/0.9.0-z", l.releases())).exists(), "the old ones did not");
        let _ = std::fs::remove_dir_all(&root);
    }
}
