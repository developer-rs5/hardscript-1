//! What a release is, where it lands on the far side, and in what order
//! (M7.3).
//!
//! The remote layout is the boring one that every other tool converges on: a
//! `releases/` directory with one immutable directory per deploy, a `current`
//! symlink the service points at, and a `shared/` directory for the things that
//! must survive a deploy -- migrations, static files, the SQLite file. Nothing
//! is ever overwritten in place, which is the only reason a rollback is
//! possible at all: the previous release is still a directory on disk, and
//! going back to it is one `ln`.
//!
//! The deploy sequence is a pure function of a [`DeployConfig`] and a
//! [`ReleaseId`], producing a list of [`DeployStep`]s. That is deliberate:
//! `--print` renders the plan for a human to read before anything happens, a
//! test runs it against a `MockSsh` with no network, and the executor has
//! nothing to decide. The clock and the file digest are arguments rather than
//! calls to `SystemTime`, so the release id a test sees is the one it asked
//! for.

use crate::ssh::{Ssh, SshTarget};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Release identity
// ---------------------------------------------------------------------------

/// `20250927T141530Z`: the UTC stamp in a release directory name. Sortable,
/// and unambiguous, which is the whole requirement -- a release name has to
/// tell two deploys apart and read as a date.
pub fn civil_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date, the standard
/// era-based conversion. No dependency, no timezone database, no locale.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A release directory name: `0.3.0-20250927T141530Z-3f9a1c2d`.
///
/// The manifest version says what was deployed, the stamp says when, and the
/// digest says it is the same bytes that were hashed locally. Everything is
/// restricted to `[A-Za-z0-9._-]` because the name lands in a remote path, in
/// a `systemctl` unit, and in a `ln -sfn` argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseId(String);

impl ReleaseId {
    /// Build an id from the three facts that make one unique.
    pub fn new(version: &str, now: u64, digest: &str) -> ReleaseId {
        let version = sanitize_part(version, 32);
        let mut hex: String = digest
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .take(8)
            .collect();
        if hex.is_empty() {
            hex.push_str("00000000");
        }
        let stamp = civil_utc(now);
        ReleaseId(if version.is_empty() {
            format!("{stamp}-{hex}")
        } else {
            format!("{version}-{stamp}-{hex}")
        })
    }

    /// Accept an operator-supplied id (`--release-id`), rejecting anything that
    /// could escape the release directory.
    pub fn parse(text: &str) -> Result<ReleaseId, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("a release id cannot be empty".to_string());
        }
        if text.len() > 64 {
            return Err(format!("`{text}` is too long for a release id (64 characters max)"));
        }
        if text.starts_with('-') || text.starts_with('.') {
            return Err(format!("`{text}` cannot start with '{:?}'", text.chars().next().unwrap()));
        }
        for c in text.chars() {
            if !(c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
                return Err(format!("`{text}` has '{c}', which is not allowed in a release id"));
            }
        }
        Ok(ReleaseId(text.to_string()))
    }

}

impl fmt::Display for ReleaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Reduce arbitrary text (a manifest version, a `--release-id` typo) to the
/// characters a path segment may hold. Empty when nothing survives, which the
/// callers treat as "no version" rather than as a broken name.
///
/// Runs of dots collapse to one: `..` in a release name is a directory
/// traversal, and a version string is the only thing that would ever put it
/// there.
fn sanitize_part(raw: &str, max: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max));
    for c in raw.chars() {
        if out.len() >= max {
            break;
        }
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else if c == '.' {
            if !out.ends_with('.') {
                out.push('.');
            }
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').trim_end_matches('.').to_string()
}

// ---------------------------------------------------------------------------
// Remote layout
// ---------------------------------------------------------------------------

/// Where things live on the host, and the unit that runs the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteLayout {
    /// The service root, e.g. `/srv/app`.
    pub root: String,
    /// The systemd unit name, e.g. `app.service`.
    pub service: String,
    /// The server binary's name inside a release directory.
    pub binary: String,
}

impl RemoteLayout {
    /// `/srv/<app>` and `<app>.service` by default. The root is validated:
    /// it is interpolated into shell commands on someone else's machine, so a
    /// space or a `;` in it has to be an error at the call site rather than a
    /// surprise at run time.
    pub fn new(root: &str, app: &str) -> Result<RemoteLayout, String> {
        let app = crate::dockerfile::sanitize_name(app);
        let root = root.trim();
        let root = if root.is_empty() {
            format!("/srv/{app}")
        } else {
            root.trim_end_matches('/').to_string()
        };
        check_remote_path(&root)?;
        Ok(RemoteLayout { root, service: format!("{app}.service"), binary: "server".to_string() })
    }

    /// Point at a different unit, for a host where the service is not named
    /// after the project.
    pub fn with_service(mut self, unit: &str) -> Result<RemoteLayout, String> {
        let unit = unit.trim();
        if unit.is_empty() {
            return Err("--service needs a unit name".to_string());
        }
        if unit.contains('/') || unit.chars().any(|c| c.is_whitespace()) {
            return Err(format!("`{unit}` is not a unit name"));
        }
        self.service = unit.to_string();
        Ok(self)
    }

    pub fn releases(&self) -> String {
        format!("{}/releases", self.root)
    }

    pub fn release_dir(&self, id: &ReleaseId) -> String {
        format!("{}/{}", self.releases(), id)
    }

    /// The immutable copy of the binary for this release.
    pub fn server(&self, id: &ReleaseId) -> String {
        format!("{}/{}", self.release_dir(id), self.binary)
    }

    /// The migration helper for this release, next to the binary.
    pub fn helper(&self, id: &ReleaseId) -> String {
        format!("{}/migrate-helper", self.release_dir(id))
    }

    /// The manifest, copied into the release directory.
    pub fn manifest(&self, id: &ReleaseId) -> String {
        format!("{}/hard.toml", self.release_dir(id))
    }

    /// The generated environment file. Not in the release directory: the
    /// values are per-environment and every release on this host shares them.
    pub fn env_file(&self) -> String {
        format!("{}/env", self.shared())
    }

    /// The file the operator keeps the secrets in. The deploy reads it and
    /// never writes it.
    pub fn secrets_file(&self) -> String {
        format!("{}/env.secrets", self.shared())
    }

    pub fn current(&self) -> String {
        format!("{}/current", self.root)
    }

    pub fn previous(&self) -> String {
        format!("{}/previous", self.root)
    }

    pub fn shared(&self) -> String {
        format!("{}/shared", self.root)
    }

    pub fn migrations(&self) -> String {
        format!("{}/migrations", self.shared())
    }

    pub fn static_dir(&self) -> String {
        format!("{}/static", self.shared())
    }

    /// The SQLite file lives in `shared/`, which is the point of `shared/`.
    pub fn database(&self, app: &str) -> String {
        format!("{}/{}.db", self.shared(), crate::dockerfile::sanitize_name(app))
    }

    /// The path the unit's `ExecStart` points at. It goes through `current`, so
    /// activating a release is a symlink swap and a restart, not a copy.
    pub fn live_server(&self) -> String {
        format!("{}/{}", self.current(), self.binary)
    }

    /// The directories a release needs before anything can be copied into it.
    pub fn prepare(&self) -> Vec<String> {
        vec![format!(
            "mkdir -p {} {} {}",
            sh_quote(&self.releases()),
            sh_quote(&self.migrations()),
            sh_quote(&self.static_dir())
        )]
    }

    /// Point `current` at a release, keeping the outgoing one at `previous`.
    ///
    /// Two details, both learned the hard way. `ln -sfn` on an existing
    /// symlink *to a directory* follows the link and creates the new one
    /// inside the old release, so the swap goes through a temporary link and
    /// `mv -T` (without `-T`, `mv` moves the link *into* the directory it
    /// points at). And `current` is only repointed after the migration step, so
    /// a failed migration leaves the running release exactly as it was.
    pub fn activate(&self, id: &ReleaseId) -> String {
        let current = sh_quote(&self.current());
        let previous = sh_quote(&self.previous());
        let target = sh_quote(&self.release_dir(id));
        format!(
            "cur={current}; \
             if [ -L \"$cur\" ]; then ln -sfn \"$(readlink -f \"$cur\")\" {previous}; fi; \
             ln -sfn {target} \"$cur.new\" && mv -T \"$cur.new\" \"$cur\" && readlink -f \"$cur\""
        )
    }

    /// What `current` points at, resolved to an absolute path.
    pub fn read_current(&self) -> String {
        format!("readlink -f {}", sh_quote(&self.current()))
    }
}

/// Single-quote a path for `sh`. Everything interpolated into a remote command
/// goes through here, and the layout validator rejects the nastier characters
/// before they get this far.
pub fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A remote path has to survive being pasted into a shell command, so the
/// accepted shape is deliberately small: an absolute path of letters, digits,
/// `.`, `-`, `_` and `/`, with no `..` and no double slash.
fn check_remote_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("`{path}` is not an absolute path"));
    }
    if path.contains("..") {
        return Err(format!("`{path}` may not contain `..`"));
    }
    for c in path.chars() {
        if !(c.is_ascii_alphanumeric() || c == '/' || c == '.' || c == '-' || c == '_') {
            return Err(format!("`{path}` has '{c}', which is not allowed in a remote path"));
        }
    }
    Ok(())
}

/// The privilege prefix for a service command. Root does not need `sudo`, and
/// `sudo -n` never prompts: a deploy that waits for a password on a host with
/// no terminal hangs until the connection drops.
pub fn privilege_prefix(user: &str) -> &'static str {
    if user == "root" {
        ""
    } else {
        "sudo -n "
    }
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// What the deploy needs to know. Built by the CLI from the project and the
/// flags; every field has a default so a directory with only a `.hard` build
/// can be deployed.
#[derive(Clone, Debug)]
pub struct DeployConfig {
    pub app: String,
    pub layout: RemoteLayout,
    /// The account on the far side, which decides whether `sudo` is needed.
    pub user: String,
    /// The locally built server binary.
    pub binary: PathBuf,
    /// The project manifest, shipped with the release so the far side knows
    /// what it is running without a checkout.
    pub manifest: Option<PathBuf>,
    /// Local migration files, copied into `shared/migrations`.
    pub migrations: Vec<PathBuf>,
    /// Local static files, copied into `shared/static`.
    pub static_files: Vec<PathBuf>,
    /// The locally built migration helper, when a database is configured.
    pub migrate_helper: Option<PathBuf>,
    /// `sqlite` or `postgres`, from the manifest.
    pub dialect: Option<String>,
    /// Whether to verify the new release answers before calling it deployed.
    /// Off means the deploy ends at the restart, which is occasionally what an
    /// operator wants and is never the default.
    pub verify: bool,
    /// Whether to run migrations as part of the deploy.
    pub run_migrations: bool,
    /// An HTTP path to probe after the restart; `None` probes the TCP port.
    pub health_path: Option<String>,
    pub health_port: u16,
    /// Total seconds to wait for the new release to answer.
    pub health_timeout: u32,
    /// Seconds between probes.
    pub health_interval: u32,
    /// Overrides the restart command entirely (OpenRC, `supervisorctl`, ...).
    pub restart_command: Option<String>,
    /// The environment file to write on the host, and the secrets it must not
    /// contain. Empty means this deploy has nothing to configure.
    pub env_body: Option<String>,
    pub secrets: Vec<String>,
    /// The systemd unit to install, when the deploy owns it. `None` leaves the
    /// unit alone, which is the default: overwriting a unit somebody edited by
    /// hand is not something a deploy should do unless it was asked.
    pub unit_body: Option<String>,
}

impl Default for DeployConfig {
    fn default() -> DeployConfig {
        DeployConfig {
            app: "app".to_string(),
            layout: RemoteLayout::new("", "app").expect("a default root is valid"),
            user: "root".to_string(),
            binary: PathBuf::from(".hard/server"),
            manifest: None,
            migrations: Vec::new(),
            static_files: Vec::new(),
            migrate_helper: None,
            dialect: None,
            verify: true,
            run_migrations: false,
            health_path: Some("/healthz".to_string()),
            health_port: 3000,
            health_timeout: 30,
            health_interval: 2,
            restart_command: None,
            env_body: None,
            secrets: Vec::new(),
            unit_body: None,
        }
    }
}

impl DeployConfig {
    /// Whether the project has anything to migrate.
    pub fn migrates(&self) -> bool {
        self.run_migrations && self.dialect.is_some() && !self.migrations.is_empty()
    }
}

/// One thing the deploy does, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployStep {
    pub kind: StepKind,
    /// What a human reads: `upload .hard/server`, `activate 0.3.0-...`.
    pub detail: String,
    /// The remote command, empty for an upload.
    pub command: String,
    /// The local file and its destination, `None` for a command.
    pub upload: Option<(PathBuf, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// What is live right now, read before anything is changed.
    Inspect,
    Prepare,
    Upload,
    /// Making an uploaded file runnable, which is its own step because a
    /// binary that arrives without the execute bit fails at the restart with a
    /// message about permissions rather than about the deploy.
    Install,
    Config,
    Migrate,
    Activate,
    Restart,
    Health,
}

impl StepKind {
    pub fn label(&self) -> &'static str {
        match self {
            StepKind::Inspect => "inspect",
            StepKind::Prepare => "prepare",
            StepKind::Upload => "upload",
            StepKind::Install => "install",
            StepKind::Config => "config",
            StepKind::Migrate => "migrate",
            StepKind::Activate => "activate",
            StepKind::Restart => "restart",
            StepKind::Health => "health",
        }
    }
}

/// A whole deploy, decided before anything happens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployPlan {
    pub id: ReleaseId,
    pub steps: Vec<DeployStep>,
}

impl DeployPlan {
    /// The plan for `cfg`, identified as `id`.
    pub fn build(cfg: &DeployConfig, id: &ReleaseId) -> DeployPlan {
        let mut steps = vec![DeployStep {
            kind: StepKind::Inspect,
            // `readlink -f` on a host that has never been deployed fails, and a
            // first deploy is not a failure, so the read cannot report one.
            detail: format!("what is live now (the unit runs {})", cfg.layout.live_server()),
            command: format!("{} 2>/dev/null || echo '(first deploy)'", cfg.layout.read_current()),
            upload: None,
        }];
        for command in cfg.layout.prepare() {
            steps.push(DeployStep {
                kind: StepKind::Prepare,
                detail: "create the release directories".to_string(),
                command,
                upload: None,
            });
        }

        // The binary goes into its own release directory and nowhere else.
        steps.push(DeployStep {
            kind: StepKind::Upload,
            detail: format!("{}", cfg.binary.display()),
            command: String::new(),
            upload: Some((cfg.binary.clone(), cfg.layout.server(id))),
        });
        steps.push(DeployStep {
            kind: StepKind::Install,
            detail: "make it executable".to_string(),
            command: format!("chmod 0755 {}", sh_quote(&cfg.layout.server(id))),
            upload: None,
        });
        if let Some(manifest) = &cfg.manifest {
            steps.push(DeployStep {
                kind: StepKind::Upload,
                detail: "hard.toml".to_string(),
                command: String::new(),
                upload: Some((manifest.clone(), cfg.layout.manifest(id))),
            });
        }

        for file in &cfg.migrations {
            let name = file_name(file);
            steps.push(DeployStep {
                kind: StepKind::Upload,
                detail: format!("{name}"),
                command: String::new(),
                upload: Some((file.clone(), format!("{}/{name}", cfg.layout.migrations()))),
            });
        }
        for file in &cfg.static_files {
            let name = file_name(file);
            steps.push(DeployStep {
                kind: StepKind::Upload,
                detail: format!("{name}"),
                command: String::new(),
                upload: Some((file.clone(), format!("{}/{name}", cfg.layout.static_dir()))),
            });
        }

        // Configuration before migrations: a migration that reads a variable
        // the release has not been given yet fails for the wrong reason.
        if let Some(unit) = &cfg.unit_body {
            steps.push(DeployStep {
                kind: StepKind::Config,
                detail: format!("install {} and reload systemd", cfg.layout.service),
                command: unit.clone(),
                upload: None,
            });
        }
        if let Some(body) = &cfg.env_body {
            steps.push(DeployStep {
                kind: StepKind::Config,
                detail: format!("write {}", cfg.layout.env_file()),
                command: crate::environments::env_write_command(&cfg.layout.env_file(), body),
                upload: None,
            });
        }
        if !cfg.secrets.is_empty() {
            steps.push(DeployStep {
                kind: StepKind::Config,
                detail: format!("the host has {}", cfg.secrets.join(", ")),
                command: crate::environments::secrets_check_command(
                    &cfg.layout.secrets_file(),
                    &cfg.secrets,
                ),
                upload: None,
            });
        }

        if cfg.migrates() {
            if let Some(helper) = &cfg.migrate_helper {
                steps.push(DeployStep {
                    kind: StepKind::Upload,
                    detail: "migration helper".to_string(),
                    command: String::new(),
                    upload: Some((helper.clone(), cfg.layout.helper(id))),
                });
                steps.push(DeployStep {
                    kind: StepKind::Install,
                    detail: "make it executable".to_string(),
                    command: format!("chmod 0755 {}", sh_quote(&cfg.layout.helper(id))),
                    upload: None,
                });
            }
            steps.push(DeployStep {
                kind: StepKind::Migrate,
                detail: format!("run {} migrations", cfg.dialect.clone().unwrap_or_default()),
                command: cfg.migrate_command(id),
                upload: None,
            });
        }

        steps.push(DeployStep {
            kind: StepKind::Activate,
            detail: format!("current -> {id}"),
            command: cfg.layout.activate(id),
            upload: None,
        });

        steps.push(DeployStep {
            kind: StepKind::Restart,
            detail: cfg.layout.service.clone(),
            command: cfg.restart_command(),
            upload: None,
        });

        if cfg.verify {
            let health = HealthCheck::new(cfg);
            steps.push(DeployStep {
                kind: StepKind::Health,
                detail: health.description.clone(),
                command: health.command.clone(),
                upload: None,
            });
        }

        DeployPlan { id: id.clone(), steps }
    }

    /// The plan as a shell script, with the uploads spelled out as `scp`
    /// lines. This is what `--print` shows and what goes into the docs: the
    /// exact steps, in order, with nothing hidden in the executor.
    pub fn render(&self) -> String {
        let mut out = format!("# release {}\n", self.id);
        for step in &self.steps {
            match &step.upload {
                Some((local, remote)) => {
                    out.push_str(&format!("{:<9} {} -> {}\n", step.kind.label(), local.display(), remote));
                }
                None => out.push_str(&format!("{:<9} {}\n", step.kind.label(), step.command)),
            }
        }
        out
    }

    /// The number of bytes the plan will copy, as far as the local files go.
    pub fn upload_count(&self) -> usize {
        self.steps.iter().filter(|s| s.upload.is_some()).count()
    }
}

fn file_name(path: &PathBuf) -> String {
    path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "file".to_string())
}

impl DeployConfig {
    /// The remote command that restarts the service.
    pub fn restart_command(&self) -> String {
        match &self.restart_command {
            Some(cmd) => cmd.clone(),
            None => format!("{}systemctl restart {}", privilege_prefix(&self.user), self.layout.service),
        }
    }

    /// The remote command that applies migrations, with the release's own
    /// helper rather than a `hard` on the far side -- the helper is built from
    /// the same embedded runtime as the server, so the migration code that runs
    /// in production is the code that was tested.
    ///
    /// A PostgreSQL URL is read from the service's env file rather than passed
    /// in by the operator, so the secret never ends up in this process's argv,
    /// in a shell history, or in a log of the deploy.
    pub fn migrate_command(&self, id: &ReleaseId) -> String {
        let dialect = self.dialect.clone().unwrap_or_default();
        let helper = sh_quote(&self.layout.helper(id));
        let mut files: Vec<String> = self
            .migrations
            .iter()
            .map(|f| sh_quote(&format!("{}/{}", self.layout.migrations(), file_name(f))))
            .collect();
        files.sort();
        let files = files.join(" ");
        if dialect.eq_ignore_ascii_case("postgres") {
            let secrets = sh_quote(&self.layout.secrets_file());
            let env_file = sh_quote(&self.layout.env_file());
            let source = crate::environments::env_source_lines(&self.layout.shared());
            format!(
                "{source}; \
                 : \"${{DATABASE_URL:?no DATABASE_URL in {secrets} or {env_file}}}\"; \
                 {helper} up postgres \"$DATABASE_URL\" {files}"
            )
        } else {
            let db = sh_quote(&self.layout.database(&self.app));
            format!("{helper} up sqlite {db} {files}")
        }
    }
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// The probe a deploy makes after a restart, and how patient it is.
///
/// A restart is not a deploy finishing: systemd reports the job as started
/// while the process is still opening its socket, and on a loaded box that gap
/// is seconds. The deploy is only done when the new release answers, so the
/// wait is part of the deploy rather than something the operator is left to
/// babysit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HealthCheck {
    pub command: String,
    pub description: String,
    pub attempts: u32,
    pub interval: Duration,
}

impl HealthCheck {
    pub fn new(cfg: &DeployConfig) -> HealthCheck {
        let interval = if cfg.health_interval == 0 { 2 } else { cfg.health_interval };
        let timeout = if cfg.health_timeout == 0 { 30 } else { cfg.health_timeout };
        let attempts = ((timeout + interval - 1) / interval).max(1);
        let port = cfg.health_port;
        let (probe, description) = match &cfg.health_path {
            Some(path) => {
                let path = if path.starts_with('/') { path.clone() } else { format!("/{path}") };
                (format!("http://127.0.0.1:{port}{path}"), format!("GET {path} on port {port}"))
            }
            None => (format!("127.0.0.1:{port}"), format!("TCP port {port}")),
        };
        HealthCheck {
            command: health_command(&probe, cfg.health_path.is_some()),
            description,
            attempts,
            interval: Duration::from_secs(u64::from(interval)),
        }
    }
}

/// One remote command that probes a URL, or a TCP port, with whatever the host
/// actually has. HardScript servers are deployed onto machines nobody has
/// provisioned, so the probe asks for `curl`, falls back to `wget`, and falls
/// back again to bash's `/dev/tcp` -- in that order, because the first two
/// give a real HTTP status and the last one only proves something is
/// listening.
pub fn health_command(probe: &str, http: bool) -> String {
    if http {
        // curl and wget want a URL; bash's /dev/tcp wants `host:port`, so the
        // path has to come off.
        let authority = probe
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or(probe);
        format!(
            "if command -v curl >/dev/null 2>&1; then curl -fsS -o /dev/null --max-time 5 {probe}; \
             elif command -v wget >/dev/null 2>&1; then wget -q -T 5 -O /dev/null {probe}; \
             else bash -c \"exec 3<>/dev/tcp/{authority}\" 2>/dev/null; fi"
        )
    } else {
        format!("bash -c \"exec 3<>/dev/tcp/{probe}\" 2>/dev/null || nc -z -w 3 {probe}")
    }
}

/// Waiting between health probes, injectable so a test with a server that never
/// comes up finishes in microseconds instead of the production timeout.
pub trait Sleeper: fmt::Debug {
    fn sleep(&mut self, d: Duration);
}

#[derive(Debug, Default)]
pub struct RealSleeper;

impl Sleeper for RealSleeper {
    fn sleep(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
pub struct NoSleep {
    pub slept: Vec<Duration>,
}

#[cfg(test)]
impl Sleeper for NoSleep {
    fn sleep(&mut self, d: Duration) {
        self.slept.push(d);
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// What a finished deploy did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployReport {
    pub id: String,
    pub files: usize,
    pub bytes: u64,
    pub steps: usize,
    /// Probes made after the restart, including the one that answered.
    pub probes: u32,
    /// Where `current` points now, read back from the host.
    pub current: String,
}

/// A deploy that stopped. Every failure carries the step and the remote output,
/// because "deploy failed" with nothing else is the least useful sentence a
/// tool can print.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployFailure {
    pub id: String,
    pub kind: StepKind,
    pub detail: String,
    pub command: String,
    pub message: String,
    /// Set when the release was already activated: the previous release is
    /// still on disk, and the deploy says so instead of leaving the operator
    /// to guess.
    pub recoverable: Option<String>,
}

impl fmt::Display for DeployFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "release {} failed at {}: {}", self.id, self.kind.label(), self.message)?;
        if !self.command.is_empty() {
            write!(f, "\n  command: {}", self.command)?;
        }
        if let Some(hint) = &self.recoverable {
            write!(f, "\n  {hint}")?;
        }
        Ok(())
    }
}

/// Run a plan. Every step is a remote command or an upload, in order, and the
/// first failure stops the deploy -- a release that cannot be uploaded is not
/// worth restarting a service for.
pub fn execute(
    ssh: &mut dyn Ssh,
    sleeper: &mut dyn Sleeper,
    target: &SshTarget,
    cfg: &DeployConfig,
    plan: &DeployPlan,
) -> Result<DeployReport, DeployFailure> {
    let mut report = DeployReport {
        id: plan.id.to_string(),
        files: 0,
        bytes: 0,
        steps: 0,
        probes: 0,
        current: String::new(),
    };
    let mut activated = false;

    for step in &plan.steps {
        if let Some((local, remote)) = &step.upload {
            let n = ssh.upload(target, local, remote).map_err(|e| DeployFailure {
                id: report.id.clone(),
                kind: step.kind,
                detail: step.detail.clone(),
                command: format!("upload {} -> {remote}", local.display()),
                message: e,
                recoverable: rollback_hint(cfg, activated),
            })?;
            report.files += 1;
            report.bytes += n;
        } else if step.kind == StepKind::Health {
            report.probes = poll_health(ssh, sleeper, target, cfg, plan, step)?;
        } else {
            let out = ssh.run(target, &step.command).map_err(|e| DeployFailure {
                id: report.id.clone(),
                kind: step.kind,
                detail: step.detail.clone(),
                command: step.command.clone(),
                message: e,
                recoverable: rollback_hint(cfg, activated),
            })?;
            if !out.ok() {
                let message = remote_message(&out);
                return Err(DeployFailure {
                    id: report.id.clone(),
                    kind: step.kind,
                    detail: step.detail.clone(),
                    command: step.command.clone(),
                    message,
                    recoverable: rollback_hint(cfg, activated),
                });
            }
            // The activation is the step that decides whether a later failure
            // leaves the service on the new release or the old one.
            if step.kind == StepKind::Activate {
                activated = true;
                report.current = out.trimmed().to_string();
            }
        }
        report.steps += 1;
    }
    Ok(report)
}

/// Probe until it answers, or until the attempts run out.
fn poll_health(
    ssh: &mut dyn Ssh,
    sleeper: &mut dyn Sleeper,
    target: &SshTarget,
    cfg: &DeployConfig,
    plan: &DeployPlan,
    step: &DeployStep,
) -> Result<u32, DeployFailure> {
    let check = HealthCheck::new(cfg);
    let mut last = String::from("no answer");
    let total = u64::from(check.attempts) * check.interval.as_secs();
    for attempt in 1..=check.attempts {
        let out = ssh.run(target, &step.command).map_err(|e| DeployFailure {
            id: plan.id.to_string(),
            kind: StepKind::Health,
            detail: step.detail.clone(),
            command: step.command.clone(),
            message: e,
            recoverable: rollback_hint(cfg, true),
        })?;
        if out.ok() {
            return Ok(attempt);
        }
        last = remote_message(&out);
        if attempt < check.attempts {
            sleeper.sleep(check.interval);
        }
    }
    Err(DeployFailure {
        id: plan.id.to_string(),
        kind: StepKind::Health,
        detail: step.detail.clone(),
        command: step.command.clone(),
        message: format!("{last} ({} probes over {total}s)", check.attempts),
        recoverable: rollback_hint(cfg, true),
    })
}

/// What the remote side said, trimmed to the part a human wants.
fn remote_message(out: &crate::ssh::SshOutput) -> String {
    let stderr = out.stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    let stdout = out.stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_string();
    }
    format!("exited with status {}", out.status)
}

/// The sentence that saves an operator from a bad release: the previous one is
/// still there, and here is where.
fn rollback_hint(cfg: &DeployConfig, activated: bool) -> Option<String> {
    if !activated {
        return None;
    }
    Some(format!(
        "the previous release is still at {}; point current back at it to go live again",
        cfg.layout.previous()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::{MockSsh, SshTarget};

    fn layout() -> RemoteLayout {
        RemoteLayout::new("/srv/app", "my app").expect("a valid root")
    }

    fn config() -> DeployConfig {
        // `Cargo.toml` stands in for the built binary: a file that exists, so
        // the upload path is exercised rather than mocked away.
        DeployConfig {
            app: "my-app".to_string(),
            layout: layout(),
            binary: PathBuf::from("Cargo.toml"),
            ..DeployConfig::default()
        }
    }

    fn id() -> ReleaseId {
        ReleaseId::new("0.3.0", 1_757_919_930, "3f9a1c2dbeef")
    }

    #[test]
    fn a_release_id_reads_as_a_version_a_date_and_a_digest() {
        let id = id();
        assert_eq!(id.to_string(), "0.3.0-20250915T070530Z-3f9a1c2d", "{}", id);
        // The stamp is UTC, whatever the machine's zone is.
        assert_eq!(civil_utc(0), "19700101T000000Z", "the epoch");
        assert_eq!(civil_utc(951_782_400), "20000229T000000Z", "a leap day");
        assert_eq!(civil_utc(1_709_164_800), "20240229T000000Z", "another one");
    }

    #[test]
    fn a_release_id_without_a_version_still_says_when() {
        let id = ReleaseId::new("", 1_757_919_930, "abcdef12");
        assert_eq!(id.to_string(), "20250915T070530Z-abcdef12", "stamp and digest only");
        let no_version = ReleaseId::new("  ", 0, "zzzz");
        assert_eq!(no_version.to_string(), "19700101T000000Z-00000000", "no hex means no digest");
    }

    #[test]
    fn a_version_with_punctuation_cannot_break_out_of_a_path() {
        let id = ReleaseId::new("../../etc; rm -rf /", 0, "ab12");
        assert!(!id.to_string().contains('/'), "no separator: {}", id);
        assert!(!id.to_string().contains(';'), "no shell punctuation: {}", id);
        assert!(!id.to_string().contains(".."), "no parent directory: {}", id);
    }

    #[test]
    fn a_supplied_release_id_is_checked_rather_than_trusted() {
        assert!(ReleaseId::parse("20250101T000000Z-abcdef12").is_ok(), "a plain id");
        assert!(ReleaseId::parse("").is_err(), "empty");
        assert!(ReleaseId::parse("../../root").is_err(), "traversal");
        assert!(ReleaseId::parse("a;rm -rf /").is_err(), "a command is not an id");
        assert!(ReleaseId::parse("-leading").is_err(), "no leading dash");
        assert!(ReleaseId::parse(&"x".repeat(65)).is_err(), "and not unbounded");
    }

    #[test]
    fn the_layout_keeps_releases_current_and_shared_apart() {
        let l = layout();
        assert_eq!(l.root, "/srv/app", "the root is absolute");
        assert_eq!(l.service, "my-app.service", "named after the project");
        assert_eq!(l.releases(), "/srv/app/releases", "one directory per deploy");
        assert_eq!(l.server(&id()), "/srv/app/releases/0.3.0-20250915T070530Z-3f9a1c2d/server", "the binary");
        assert_eq!(l.current(), "/srv/app/current", "what the unit points at");
        assert_eq!(l.live_server(), "/srv/app/current/server", "through the symlink");
        assert_eq!(l.migrations(), "/srv/app/shared/migrations", "migrations survive a deploy");
        assert_eq!(l.static_dir(), "/srv/app/shared/static", "and so do static files");
        assert_eq!(l.database("My App"), "/srv/app/shared/my-app.db", "and the SQLite file");
    }

    #[test]
    fn a_root_that_would_be_a_shell_problem_is_rejected() {
        assert!(RemoteLayout::new("srv/app", "a").is_err(), "a relative path");
        assert!(RemoteLayout::new("/srv/app; rm -rf /", "a").is_err(), "a command fragment");
        assert!(RemoteLayout::new("/srv/../etc", "a").is_err(), "traversal");
        assert!(RemoteLayout::new("/srv/my app", "a").is_err(), "a space breaks quoting silently");
        assert!(RemoteLayout::new("", "a").unwrap().root == "/srv/a", "empty means the default");
        assert_eq!(RemoteLayout::new("/srv/app/", "a").unwrap().root, "/srv/app", "no trailing slash");
    }

    #[test]
    fn activating_swaps_the_symlink_instead_of_following_it() {
        let cmd = layout().activate(&id());
        // `ln -sfn` alone creates releases/<old>/releases/<new> when current is
        // a symlink to a directory; the temporary link plus `mv -T` is what
        // makes the swap atomic.
        assert!(cmd.contains("\"$cur.new\""), "a temporary link: {cmd}");
        assert!(cmd.contains("mv -T"), "moved as a file, not into the directory: {cmd}");
        assert!(cmd.contains("ln -sfn '/srv/app/releases/0.3.0-20250915T070530Z-3f9a1c2d'"), "the new target");
        assert!(cmd.contains("readlink -f \"$cur\""), "and read back, so the deploy can report it");
        // The outgoing release is kept before the swap, not after.
        let keep = cmd.find("/srv/app/previous").expect("previous is written");
        let swap = cmd.find("mv -T").expect("then the swap");
        assert!(keep < swap, "previous is recorded first: {cmd}");
    }

    /// The activation is shell, and a shell command that does not parse is a
    /// deploy that fails with a syntax error instead of a release. Run it.
    #[test]
    fn the_activation_runs_in_a_shell_and_moves_the_symlink() {
        let root = std::env::temp_dir().join(format!("hs-activate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let layout = RemoteLayout::new(root.to_str().expect("a path"), "app").expect("a layout");
        let a = ReleaseId::parse("v1").expect("an id");
        let b = ReleaseId::parse("v2").expect("an id");
        for cmd in layout.prepare() {
            let out = std::process::Command::new("sh").arg("-c").arg(&cmd).output().expect("sh");
            assert!(out.status.success(), "prepare: {}", String::from_utf8_lossy(&out.stderr));
        }
        for id in [&a, &b] {
            std::fs::create_dir_all(layout.release_dir(id)).expect("a release directory");
        }
        let run = |id: &ReleaseId| {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(layout.activate(id))
                .output()
                .expect("sh runs");
            (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).trim().to_string(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        };
        let (ok, pointed, err) = run(&a);
        assert!(ok, "the first activation runs: {err}");
        assert_eq!(pointed, layout.release_dir(&a), "and reports where current points");
        let (ok, pointed, err) = run(&b);
        assert!(ok, "the second activation runs: {err}");
        assert_eq!(pointed, layout.release_dir(&b), "current moved");
        let previous = std::fs::read_link(layout.previous()).expect("previous is a symlink");
        assert_eq!(previous.display().to_string(), layout.release_dir(&a), "and previous kept the first release");
        assert!(
            !std::path::Path::new(&layout.release_dir(&a)).join("releases").exists(),
            "no link inside the old release"
        );
        assert!(!root.join("current.new").exists(), "and no temporary link left behind");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_plan_uploads_the_binary_then_activates_then_restarts() {
        let cfg = config();
        let plan = DeployPlan::build(&cfg, &id());
        let kinds: Vec<StepKind> = plan.steps.iter().map(|s| s.kind).collect();
        let prepare = kinds.iter().position(|k| *k == StepKind::Prepare).expect("prepare");
        let upload = kinds.iter().position(|k| *k == StepKind::Upload).expect("upload");
        let activate = kinds.iter().position(|k| *k == StepKind::Activate).expect("activate");
        let restart = kinds.iter().position(|k| *k == StepKind::Restart).expect("restart");
        let health = kinds.iter().position(|k| *k == StepKind::Health).expect("health");
        assert!(prepare < upload && upload < activate, "upload before activation");
        assert!(activate < restart, "activation before the restart");
        assert!(restart < health, "the health check is after the restart");
        let server = &plan.steps[upload].upload.as_ref().expect("an upload").clone();
        assert_eq!(server.0, PathBuf::from("Cargo.toml"), "the local binary");
        assert_eq!(server.1, "/srv/app/releases/0.3.0-20250915T070530Z-3f9a1c2d/server", "and where it goes");
        assert!(plan.steps[upload + 1].command.contains("chmod 0755"), "then it is made runnable");
        assert_eq!(plan.upload_count(), 1, "just the binary in a project with no assets");
    }

    #[test]
    fn a_deploy_runs_the_plan_in_order_and_reports_what_it_did() {
        let mut ssh = MockSsh::new()
            .ok_everything()
            .answer("readlink -f \"$cur\"", "/srv/app/releases/0.3.0-20250915T070530Z-3f9a1c2d");
        let target = SshTarget::parse("root@1.2.3.4").unwrap();
        let cfg = config();
        let plan = DeployPlan::build(&cfg, &id());
        let mut sleeper = NoSleep::default();
        let report = execute(&mut ssh, &mut sleeper, &target, &cfg, &plan).expect("the deploy works");
        assert_eq!(report.id, "0.3.0-20250915T070530Z-3f9a1c2d", "the release it made");
        assert_eq!(report.steps, plan.steps.len(), "every step ran");
        assert_eq!(report.files, 1, "one file uploaded");
        assert_eq!(report.bytes, std::fs::metadata("Cargo.toml").map(|m| m.len()).unwrap_or(0), "the binary's size");
        assert_eq!(report.probes, 1, "the first probe answered");
        assert_eq!(report.current, "/srv/app/releases/0.3.0-20250915T070530Z-3f9a1c2d", "current read back");
        let log = ssh.command_log();
        assert!(log.find("mkdir -p") < log.find("systemctl restart"), "prepare before restart: {log}");
        assert!(log.find("systemctl restart") < log.find("curl"), "restart before probing: {log}");
    }

    #[test]
    fn a_failing_step_stops_the_deploy_and_says_which() {
        let mut ssh = MockSsh::new()
            .ok_everything()
            .fail("systemctl restart", 1, "Job for my-app.service failed");
        let target = SshTarget::parse("root@1.2.3.4").unwrap();
        let cfg = config();
        let plan = DeployPlan::build(&cfg, &id());
        let mut sleeper = NoSleep::default();
        let err = execute(&mut ssh, &mut sleeper, &target, &cfg, &plan).unwrap_err();
        assert_eq!(err.kind, StepKind::Restart, "the failing step");
        assert!(err.message.contains("Job for my-app.service failed"), "and the remote message: {err}");
        assert!(!ssh.command_log().contains("curl"), "no health probe after a failed restart");
        // The release was activated, so the operator is told where the old one is.
        let hint = err.recoverable.expect("a hint after activation");
        assert!(hint.contains("/srv/app/previous"), "{hint}");
    }

    #[test]
    fn a_failed_upload_never_restarts_the_service() {
        let mut ssh = MockSsh::new().ok_everything();
        let target = SshTarget::parse("root@1.2.3.4").unwrap();
        let mut cfg = config();
        cfg.binary = PathBuf::from("/does/not/exist");
        let plan = DeployPlan::build(&cfg, &id());
        let mut sleeper = NoSleep::default();
        let err = execute(&mut ssh, &mut sleeper, &target, &cfg, &plan).unwrap_err();
        assert_eq!(err.kind, StepKind::Upload, "the binary could not be read");
        assert!(err.recoverable.is_none(), "nothing was activated, so there is nothing to undo");
        assert!(!ssh.command_log().contains("systemctl"), "the service was never touched: {}", ssh.command_log());
        assert!(ssh.calls()[0].kind == "run" && ssh.calls()[0].command.contains("readlink"), "but the host was read first");
    }

    #[test]
    fn a_release_that_never_answers_fails_the_deploy_after_waiting() {
        let mut ssh = MockSsh::new().ok_everything().fail("curl", 7, "connection refused");
        let target = SshTarget::parse("root@1.2.3.4").unwrap();
        let mut cfg = config();
        cfg.health_timeout = 6;
        cfg.health_interval = 2;
        let plan = DeployPlan::build(&cfg, &id());
        let mut sleeper = NoSleep::default();
        let err = execute(&mut ssh, &mut sleeper, &target, &cfg, &plan).unwrap_err();
        assert_eq!(err.kind, StepKind::Health, "the health check is where it failed");
        assert!(err.message.contains("connection refused"), "the probe's message survives: {err}");
        assert!(err.message.contains("3 probes over 6s"), "and how patient it was: {err}");
        assert_eq!(sleeper.slept.len(), 2, "two waits between three probes");
        assert!(err.recoverable.is_some(), "the previous release is still there");
    }

    #[test]
    fn a_health_check_that_fails_once_still_passes_the_deploy() {
        let target = SshTarget::parse("root@1.2.3.4").unwrap();
        let mut cfg = config();
        cfg.health_timeout = 10;
        cfg.health_interval = 2;
        let plan = DeployPlan::build(&cfg, &id());
        // The restart races the probe: one refusal, then the service answers.
        let mut ssh = MockSsh::new()
            .ok_everything()
            .refuse_then_ok("curl", 1, "connection refused");
        let mut sleeper = NoSleep::default();
        let report = execute(&mut ssh, &mut sleeper, &target, &cfg, &plan).expect("the retry succeeds");
        assert_eq!(report.probes, 2, "it waited and probed again");
        assert_eq!(sleeper.slept.len(), 1, "once");
    }

    #[test]
    fn the_health_probe_uses_what_the_host_has() {
        let http = health_command("http://127.0.0.1:3000/healthz", true);
        assert!(http.contains("curl -fsS"), "curl first: {http}");
        assert!(http.contains("wget -q"), "wget second");
        assert!(http.contains("/dev/tcp/127.0.0.1:3000"), "and a raw socket last");
        assert!(!http.contains("/dev/tcp/http://"), "which needs host:port, not a URL: {http}");
        let tcp = health_command("127.0.0.1:3000", false);
        assert!(!tcp.contains("curl"), "no HTTP client for a port with no path: {tcp}");
        assert!(tcp.contains("/dev/tcp/127.0.0.1:3000"), "a raw socket");
        assert!(tcp.contains("nc -z"), "and netcat");
    }

    #[test]
    fn a_release_carries_the_manifest_that_describes_it() {
        let mut cfg = config();
        assert!(!DeployPlan::build(&cfg, &id()).steps.iter().any(|s| s
            .upload
            .as_ref()
            .is_some_and(|(_, remote)| remote.ends_with("hard.toml"))), "no manifest, no upload");
        cfg.manifest = Some(PathBuf::from("Cargo.toml"));
        let plan = DeployPlan::build(&cfg, &id());
        let manifest = plan
            .steps
            .iter()
            .find(|s| s.upload.as_ref().is_some_and(|(_, r)| r.ends_with("hard.toml")))
            .expect("the manifest travels with the release");
        assert_eq!(
            manifest.upload.as_ref().unwrap().1,
            "/srv/app/releases/0.3.0-20250915T070530Z-3f9a1c2d/hard.toml",
            "in the release directory, not in shared/: it belongs to this release"
        );
    }

    #[test]
    fn a_deploy_that_does_not_verify_stops_at_the_restart() {
        let mut cfg = config();
        cfg.verify = false;
        let plan = DeployPlan::build(&cfg, &id());
        assert!(!plan.steps.iter().any(|s| s.kind == StepKind::Health), "no probe to wait for");
        assert_eq!(plan.steps.last().map(|s| s.kind), Some(StepKind::Restart), "the restart is the last step");
    }

    #[test]
    fn a_health_check_is_patient_enough_for_a_slow_start() {
        let mut cfg = config();
        cfg.health_path = Some("/healthz".to_string());
        cfg.health_port = 8080;
        cfg.health_timeout = 30;
        cfg.health_interval = 2;
        let h = HealthCheck::new(&cfg);
        assert_eq!(h.attempts, 15, "one probe every two seconds for thirty");
        assert_eq!(h.interval, Duration::from_secs(2), "and not in a tight loop");
        assert!(h.description.contains("8080"), "it names the port it probes: {}", h.description);
        cfg.health_timeout = 0;
        assert!(HealthCheck::new(&cfg).attempts >= 1, "a zero timeout still probes once");
    }

    #[test]
    fn a_project_with_a_database_migrates_before_it_goes_live() {
        let mut cfg = config();
        cfg.dialect = Some("postgres".to_string());
        cfg.migrations = vec![PathBuf::from("migrations/002_add_email.sql")];
        cfg.migrate_helper = Some(PathBuf::from(".hard/migrate-helper"));
        cfg.run_migrations = true;
        let plan = DeployPlan::build(&cfg, &id());
        let migrate = plan
            .steps
            .iter()
            .find(|s| s.kind == StepKind::Migrate)
            .expect("a migration step");
        let command = &migrate.command;
        // The helper is the release's own, built from the same runtime.
        assert!(command.contains("/srv/app/releases/0.3.0-20250915T070530Z-3f9a1c2d/migrate-helper"), "{command}");
        assert!(command.contains("up postgres"), "with the dialect: {command}");
        // The URL comes from the service env file, never from this process's argv.
        assert!(command.contains("'/srv/app/shared/env'"), "the env file: {command}");
        assert!(command.contains("\"$DATABASE_URL\""), "and the URL from it: {command}");
        assert!(command.contains("'/srv/app/shared/migrations/002_add_email.sql'"), "the files: {command}");
        assert!(command.contains("'/srv/app/shared/env.secrets'"), "and the operator's secrets file: {command}");
        let kinds: Vec<StepKind> = plan.steps.iter().map(|s| s.kind).collect();
        assert!(
            kinds.iter().position(|k| *k == StepKind::Migrate) < kinds.iter().position(|k| *k == StepKind::Activate),
            "migrations run against the current release, then the swap happens"
        );
        assert_eq!(plan.upload_count(), 3, "the binary, the helper, and the migration");
    }

    #[test]
    fn a_sqlite_project_migrates_against_a_file_in_shared() {
        let mut cfg = config();
        cfg.dialect = Some("sqlite".to_string());
        cfg.migrations = vec![PathBuf::from("migrations/001_init.sql")];
        cfg.run_migrations = true;
        let plan = DeployPlan::build(&cfg, &id());
        let command = &plan.steps.iter().find(|s| s.kind == StepKind::Migrate).expect("a step").command;
        assert!(command.contains("up sqlite '/srv/app/shared/my-app.db'"), "{command}");
        assert!(!command.contains("DATABASE_URL"), "a file, not a URL: {command}");
    }

    #[test]
    fn no_dialect_means_no_migration_step() {
        let mut cfg = config();
        cfg.migrations = vec![PathBuf::from("migrations/001_init.sql")];
        cfg.run_migrations = true;
        assert!(!cfg.migrates(), "a migrations directory with no database configured is not a migration");
        let plan = DeployPlan::build(&cfg, &id());
        assert!(!plan.steps.iter().any(|s| s.kind == StepKind::Migrate), "and no step for it");
    }

    #[test]
    fn only_root_escalates_and_only_without_prompting() {
        assert_eq!(privilege_prefix("root"), "", "root does not need sudo");
        assert_eq!(privilege_prefix("deploy"), "sudo -n ", "and -n never prompts");
        let mut cfg = config();
        cfg.user = "deploy".to_string();
        assert!(cfg.restart_command().starts_with("sudo -n systemctl restart"), "{}", cfg.restart_command());
        cfg.restart_command = Some("rc-service my-app restart".to_string());
        assert_eq!(cfg.restart_command(), "rc-service my-app restart", "a host without systemd");
    }

    #[test]
    fn the_printed_plan_is_the_plan() {
        let mut cfg = config();
        cfg.migrations = vec![PathBuf::from("migrations/001_init.sql")];
        let plan = DeployPlan::build(&cfg, &id());
        let text = plan.render();
        for step in &plan.steps {
            match &step.upload {
                Some((local, remote)) => assert!(
                    text.contains(&format!("{} -> {remote}", local.display())),
                    "the script mentions {}",
                    local.display()
                ),
                None => assert!(text.contains(&step.command), "the script contains the command"),
            }
        }
        assert!(text.starts_with("# release 0.3.0-"), "and says which release it is");
    }
}
