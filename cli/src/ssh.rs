//! The SSH transport `hard deploy` uses, and the mock the tests run against.
//!
//! Everything here is about being *unattended*: a deploy that can stop to ask
//! a question is a deploy that hangs in a terminal nobody is watching, so the
//! options below are fixed and non-interactive by construction. The interface
//! exists so the deploy sequence can be tested without a network: a
//! `MockSsh` records the exact commands a deploy would run, and asserts on
//! them the way it would assert on a wire protocol.

use std::path::Path;
use std::process::{Command, Stdio};

/// What a remote command produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl SshOutput {
    pub fn ok(&self) -> bool {
        self.status == 0
    }

    /// The command's output with trailing whitespace removed, for a value
    /// read back from the host.
    pub fn trimmed(&self) -> &str {
        self.stdout.trim_end_matches(['\n', '\r', ' ', '\t'])
    }
}

/// A host to talk to. `host` is `user@host` or `host`; `port` is the SSH port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    /// The account on the far side.
    pub user: String,
}

impl SshTarget {
    /// Parse `root@1.2.3.4`, `deploy@host:2222`, `1.2.3.4`.
    pub fn parse(spec: &str) -> Result<SshTarget, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("no host given".to_string());
        }
        let (user, rest) = match spec.split_once('@') {
            Some((u, r)) => (u.to_string(), r.to_string()),
            None => (current_user(), spec.to_string()),
        };
        if user.is_empty() {
            return Err(format!("`{spec}` has no user before the @"));
        }
        if rest.is_empty() {
            return Err(format!("`{spec}` has no host after the @"));
        }
        // A port suffix, but not a colon inside an IPv6 literal.
        let (host, port) = if rest.starts_with('[') {
            match rest.split_once("]:") {
                Some((h, p)) => (h.trim_start_matches('[').to_string(), parse_port(p, spec)?),
                None => (rest.trim_matches(['[', ']']).to_string(), 22),
            }
        } else {
            match rest.rsplit_once(':') {
                Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
                    (h.to_string(), parse_port(p, spec)?)
                }
                _ => (rest, 22),
            }
        };
        if host.is_empty() {
            return Err(format!("`{spec}` has no host"));
        }
        Ok(SshTarget { host, port, user })
    }

    /// `user@host`, the form ssh itself takes.
    pub fn destination(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    /// A word for a message: `root@1.2.3.4:2222`.
    pub fn label(&self) -> String {
        if self.port == 22 {
            format!("{}@{}", self.user, self.host)
        } else {
            format!("{}@{}:{}", self.user, self.host, self.port)
        }
    }
}

fn parse_port(text: &str, spec: &str) -> Result<u16, String> {
    match text.parse::<u16>() {
        Ok(p) if p > 0 => Ok(p),
        _ => Err(format!("`{spec}` has an invalid port")),
    }
}

fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "root".to_string())
}

/// One remote command, as the tests see it.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshCall {
    pub kind: &'static str,
    pub host: String,
    pub detail: String,
    pub command: String,
}

/// The interface a deploy is written against.
pub trait Ssh: std::fmt::Debug {
    /// Run a command and collect its output.
    fn run(&mut self, target: &SshTarget, command: &str) -> Result<SshOutput, String>;

    /// Run a command with this process's stdio attached, for `logs --follow`:
    /// a follow is an unbounded stream, and capturing it into memory would
    /// turn a service logging for an hour into an out-of-memory error.
    fn stream(&mut self, target: &SshTarget, command: &str) -> Result<i32, String>;

    /// Copy a local file or directory to a remote path.
    fn upload(
        &mut self,
        target: &SshTarget,
        local: &Path,
        remote: &str,
    ) -> Result<u64, String>;

    /// Every call made, in order. Test-only: a real transport has no use for
    /// the log, and keeping it out of the trait means it is not in the way when
    /// the trait grows a streaming call for `logs --follow`.
    #[cfg(test)]
    fn calls(&self) -> Vec<SshCall> {
        Vec::new()
    }
}

/// Options that make ssh unattended and repeatable. `BatchMode` is the one
/// that matters: without it, a wrong key means a password prompt on a machine
/// with no terminal.
pub const SSH_OPTIONS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "LogLevel=ERROR",
];

fn scp_options() -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < SSH_OPTIONS.len() {
        // `-o Name=Value` and the one that takes a number are both fine for
        // scp; only the log level of the client differs.
        let pair = format!("{} {}", SSH_OPTIONS[i], SSH_OPTIONS[i + 1]);
        if !pair.starts_with("-o LogLevel") {
            out.push(SSH_OPTIONS[i].to_string());
            out.push(SSH_OPTIONS[i + 1].to_string());
        }
        i += 2;
    }
    out
}

/// The real thing: `ssh` and `scp` from the system.
#[derive(Debug, Default)]
pub struct SystemSsh {
    /// Overrides the binaries, for tests and for a box that keeps them
    /// somewhere unusual. Empty means "look them up on PATH".
    pub ssh_bin: String,
    pub scp_bin: String,
}

impl SystemSsh {
    pub fn new() -> SystemSsh {
        SystemSsh::default()
    }

    fn ssh_program(&self) -> &str {
        if self.ssh_bin.is_empty() {
            "ssh"
        } else {
            &self.ssh_bin
        }
    }

    fn scp_program(&self) -> &str {
        if self.scp_bin.is_empty() {
            "scp"
        } else {
            &self.scp_bin
        }
    }
}

impl Ssh for SystemSsh {
    fn run(&mut self, target: &SshTarget, command: &str) -> Result<SshOutput, String> {
        let mut cmd = Command::new(self.ssh_program());
        cmd.args(SSH_OPTIONS);
        if target.port != 22 {
            cmd.arg("-p").arg(target.port.to_string());
        }
        cmd.arg(target.destination()).arg(command);
        let out = cmd
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run {}: {e}", self.ssh_program()))?;
        Ok(SshOutput {
            status: out.status.code().unwrap_or(255),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn stream(&mut self, target: &SshTarget, command: &str) -> Result<i32, String> {
        let mut cmd = Command::new(self.ssh_program());
        cmd.args(SSH_OPTIONS);
        if target.port != 22 {
            cmd.arg("-p").arg(target.port.to_string());
        }
        cmd.arg(target.destination()).arg(command);
        let status = cmd
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|e| format!("cannot run {}: {e}", self.ssh_program()))?;
        Ok(status.code().unwrap_or(255))
    }

    fn upload(
        &mut self,
        target: &SshTarget,
        local: &Path,
        remote: &str,
    ) -> Result<u64, String> {
        let size = std::fs::metadata(local).map(|m| m.len()).unwrap_or(0);
        let mut cmd = Command::new(self.scp_program());
        cmd.args(scp_options());
        if target.port != 22 {
            cmd.arg("-P").arg(target.port.to_string());
        }
        cmd.arg(local).arg(format!("{}:{remote}", target.destination()));
        let out = cmd
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run {}: {e}", self.scp_program()))?;
        if !out.status.success() {
            return Err(format!(
                "upload of {} to {} failed: {}",
                local.display(),
                remote,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(size)
    }
}

/// An in-process `Ssh` for tests: scripted answers and a full record of what
/// was asked. Nothing here touches a network.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct MockSsh {
    /// Answers keyed by a substring of the command. The first match wins, so
    /// a test can script "systemctl restart" separately from the rest.
    pub answers: Vec<(String, SshOutput)>,
    /// Answers that fail a fixed number of times before succeeding, for the
    /// race every real deploy hits: the service is not up the instant systemd
    /// says the restart is done.
    pub flaky: Vec<(String, usize, SshOutput)>,
    /// Used when nothing matches. A command nobody scripted is a bug in the
    /// deploy or in the test, and the mock says so rather than pretending.
    pub default: Option<SshOutput>,
    pub calls: Vec<SshCall>,
    /// Uploads recorded as (local path, remote path, bytes).
    pub uploads: Vec<(String, String, u64)>,
    /// Streaming commands recorded but not run.
    pub streamed: Vec<String>,
}

#[cfg(test)]
impl MockSsh {
    pub fn new() -> MockSsh {
        MockSsh::default()
    }

    /// Answer any command containing `needle` with `stdout` and a 0 status.
    pub fn answer(mut self, needle: &str, stdout: &str) -> MockSsh {
        self.answers.push((
            needle.to_string(),
            SshOutput { status: 0, stdout: stdout.to_string(), stderr: String::new() },
        ));
        self
    }

    /// Answer with a failure status and a message on stderr.
    pub fn fail(mut self, needle: &str, status: i32, stderr: &str) -> MockSsh {
        self.answers.push((
            needle.to_string(),
            SshOutput { status, stdout: String::new(), stderr: stderr.to_string() },
        ));
        self
    }

    /// Succeed for anything not scripted. A deploy runs a dozen commands and a
    /// test only cares about three of them.
    pub fn ok_everything(mut self) -> MockSsh {
        self.default = Some(SshOutput { status: 0, stdout: String::new(), stderr: String::new() });
        self
    }

    /// Refuse `refusals` times, then succeed: what a service does for the
    /// second or two after a restart. Once the refusals are used up the entry
    /// is dropped, so later calls fall through to the scripted or default
    /// answer.
    pub fn refuse_then_ok(mut self, needle: &str, refusals: usize, stderr: &str) -> MockSsh {
        self.flaky.push((
            needle.to_string(),
            refusals,
            SshOutput { status: 1, stdout: String::new(), stderr: stderr.to_string() },
        ));
        self
    }

    /// The commands asked, as a single string for easy substring assertions.
    pub fn command_log(&self) -> String {
        self.calls.iter().map(|c| c.command.clone()).collect::<Vec<_>>().join("\n")
    }

    fn record(&mut self, kind: &'static str, target: &SshTarget, detail: &str, command: &str) {
        self.calls.push(SshCall {
            kind,
            host: target.label(),
            detail: detail.to_string(),
            command: command.to_string(),
        });
    }
}

#[cfg(test)]
impl Ssh for MockSsh {
    fn run(&mut self, target: &SshTarget, command: &str) -> Result<SshOutput, String> {
        self.record("run", target, "", command);
        let mut i = 0;
        while i < self.flaky.len() {
            if command.contains(self.flaky[i].0.as_str()) {
                if self.flaky[i].1 > 0 {
                    self.flaky[i].1 -= 1;
                    return Ok(self.flaky[i].2.clone());
                }
                // The refusals are used up: drop the entry so later calls fall
                // through to the scripted or default answer.
                self.flaky.remove(i);
                break;
            }
            i += 1;
        }
        for (needle, answer) in &self.answers {
            if command.contains(needle.as_str()) {
                return Ok(answer.clone());
            }
        }
        self.default
            .clone()
            .ok_or_else(|| format!("mock: no answer scripted for `{}`", command))
    }

    fn stream(&mut self, target: &SshTarget, command: &str) -> Result<i32, String> {
        self.record("stream", target, "", command);
        self.streamed.push(command.to_string());
        Ok(0)
    }

    fn upload(
        &mut self,
        target: &SshTarget,
        local: &Path,
        remote: &str,
    ) -> Result<u64, String> {
        // scp fails on a file that is not there, and a deploy that quietly
        // "succeeds" without shipping the binary is the worst outcome there
        // is -- so the mock fails the same way.
        let meta = std::fs::metadata(local)
            .map_err(|e| format!("cannot read {}: {e}", local.display()))?;
        let size = meta.len();
        self.record("upload", target, &local.display().to_string(), &format!("-> {remote}"));
        self.uploads.push((local.display().to_string(), remote.to_string(), size));
        Ok(size)
    }

    fn calls(&self) -> Vec<SshCall> {
        self.calls.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(spec: &str) -> SshTarget {
        SshTarget::parse(spec).expect("parses")
    }

    #[test]
    fn a_target_reads_the_usual_spellings() {
        let t = target("root@1.2.3.4");
        assert_eq!(t.user, "root", "the user before the @");
        assert_eq!(t.host, "1.2.3.4", "the host");
        assert_eq!(t.port, 22, "the default port");
        assert_eq!(t.destination(), "root@1.2.3.4", "what ssh takes");
        assert_eq!(t.label(), "root@1.2.3.4", "no port in the label when it is the default");

        let t = target("deploy@example.com:2222");
        assert_eq!(t.user, "deploy", "user");
        assert_eq!(t.host, "example.com", "host without the port");
        assert_eq!(t.port, 2222, "and the port");
        assert_eq!(t.label(), "deploy@example.com:2222", "the label names the port");

        let t = target("1.2.3.4");
        assert_eq!(t.user, current_user(), "no user means this one");
        assert_eq!(t.host, "1.2.3.4", "and the whole string is the host");
    }

    #[test]
    fn an_ipv6_literal_is_not_mistaken_for_a_port() {
        let t = target("root@[2001:db8::1]:2222");
        assert_eq!(t.host, "2001:db8::1", "the address, brackets stripped");
        assert_eq!(t.port, 2222, "with the port");
        let plain = target("root@[2001:db8::1]");
        assert_eq!(plain.host, "2001:db8::1", "and without a port");
        assert_eq!(plain.port, 22, "the default");
    }

    #[test]
    fn a_broken_target_says_what_is_missing() {
        assert!(SshTarget::parse("").unwrap_err().contains("no host"), "empty");
        assert!(SshTarget::parse("   ").is_err(), "whitespace is not a host");
        assert!(SshTarget::parse("root@").unwrap_err().contains("no host after"), "no host");
        assert!(SshTarget::parse("@host").unwrap_err().contains("no user"), "no user");
        assert!(SshTarget::parse("root@h:0").unwrap_err().contains("invalid port"), "port 0");
        assert!(SshTarget::parse("root@h:99999").is_err(), "a port that does not exist");
    }

    #[test]
    fn ssh_is_always_unattended() {
        // The options are the contract: a deploy that prompts is a deploy that
        // hangs, so BatchMode is not optional.
        assert!(SSH_OPTIONS.windows(2).any(|w| w == ["-o", "BatchMode=yes"]), "no password prompt");
        assert!(
            SSH_OPTIONS.windows(2).any(|w| w == ["-o", "ConnectTimeout=10"]),
            "a connect that cannot hang forever"
        );
        let scp = scp_options();
        assert!(scp.windows(2).any(|w| w == ["-o", "BatchMode=yes"]), "scp too");
        assert!(!scp.windows(2).any(|w| w[1].starts_with("LogLevel")), "scp has no LogLevel option");
    }

    #[test]
    fn the_mock_answers_by_substring_and_records_everything() {
        let mut ssh = MockSsh::new().answer("systemctl is-active", "active\n");
        let t = target("root@host");
        let out = ssh.run(&t, "systemctl is-active myapp").expect("answered");
        assert!(out.ok(), "status 0");
        assert_eq!(out.trimmed(), "active", "trailing newline trimmed");
        assert_eq!(ssh.command_log(), "systemctl is-active myapp", "and the call recorded");
        assert_eq!(ssh.calls()[0].kind, "run", "as a run");
        assert_eq!(ssh.calls()[0].host, "root@host", "against the target");
    }

    #[test]
    fn an_unscripted_command_is_an_error_rather_than_a_guess() {
        let mut ssh = MockSsh::new();
        let t = target("root@h");
        let err = ssh.run(&t, "anything at all").unwrap_err();
        assert!(err.contains("no answer scripted"), "{err}");
    }

    #[test]
    fn the_first_matching_answer_wins() {
        let mut ssh = MockSsh::new()
            .answer("is-active", "inactive\n")
            .answer("is-active", "active\n");
        let t = target("root@h");
        assert_eq!(ssh.run(&t, "systemctl is-active app").unwrap().trimmed(), "inactive", "the first");
    }

    #[test]
    fn a_scripted_failure_keeps_its_status_and_message() {
        let mut ssh = MockSsh::new().fail("systemctl restart", 1, "Job for app failed");
        let t = target("root@h");
        let out = ssh.run(&t, "systemctl restart app").unwrap();
        assert_eq!(out.status, 1, "the status is passed through");
        assert!(out.stderr.contains("Job for app failed"), "and so is the message");
        assert!(!out.ok(), "so the caller can see the failure");
    }

    #[test]
    fn uploads_are_recorded_with_their_sizes() {
        let mut ssh = MockSsh::new();
        let t = target("root@h");
        let n = ssh.upload(&t, Path::new("Cargo.toml"), "/srv/app/releases/x/server").unwrap();
        assert!(n > 0, "the byte count");
        assert_eq!(ssh.uploads[0].1, "/srv/app/releases/x/server", "the remote path");
        assert_eq!(ssh.calls()[0].kind, "upload", "recorded as an upload");
    }

    #[test]
    fn a_file_that_is_not_there_is_not_an_upload() {
        let mut ssh = MockSsh::new();
        let t = target("root@h");
        let err = ssh.upload(&t, Path::new("/does/not/exist"), "/srv/app/server").unwrap_err();
        assert!(err.contains("cannot read"), "{err}");
        assert!(ssh.uploads.is_empty(), "and nothing is recorded as sent");
    }

    #[test]
    fn a_service_that_takes_a_moment_to_answer_is_modelled() {
        let mut ssh = MockSsh::new().ok_everything().refuse_then_ok("curl", 2, "connection refused");
        let t = target("root@h");
        for _ in 0..2 {
            let out = ssh.run(&t, "curl http://127.0.0.1:3000/healthz").expect("answered");
            assert!(!out.ok(), "still starting up");
            assert!(out.stderr.contains("connection refused"), "with the reason");
        }
        let out = ssh.run(&t, "curl http://127.0.0.1:3000/healthz").expect("answered");
        assert!(out.ok(), "and then it is up");
        assert!(ssh.flaky.is_empty(), "the script is spent");
    }
}
