//! Generated Dockerfiles for `hard build --docker` (M7.1).
//!
//! The build does not need `hard` inside the image. `.hard/<name>.cpp` plus
//! the unpacked runtime headers are a complete translation unit, so the builder
//! stage is a C++ compiler and nothing else -- no Rust toolchain, no registry
//! access, no network beyond the base images. That is what keeps the image
//! small and the build reproducible.
//!
//! Two details that are easy to get wrong and expensive to discover later:
//!
//! * `-march=native` is right for a laptop and fatal for an image. A binary
//!   built on a Zen 4 core and run on an older VPS dies with SIGILL before
//!   `main`, so the container build always asks for a portable baseline and
//!   says so in a comment.
//! * The runtime stage links the C++ runtime statically (`-static-libstdc++`,
//!   `-static-libgcc`) so it is plain Alpine: no package installs at run time,
//!   nothing to patch, and the image is measured in megabytes rather than in
//!   CVEs.

use std::fmt::Write as _;

/// What the generator needs to know. Everything has a default, because
/// `hard build --docker` has to work in a directory with nothing but a
/// `.hard` build.
#[derive(Clone, Debug)]
pub struct DockerConfig {
    /// Manifest name, used for image tags, the workdir and the user.
    pub app_name: String,
    /// Name of the generated translation unit (`.hard/<name>.cpp`).
    pub cpp_name: String,
    /// Name the server binary has inside the image.
    pub binary_name: String,
    /// Base images, overridable so a pinned digest can be substituted.
    pub builder_image: String,
    pub runtime_image: String,
    pub port: u16,
    /// Path the health check requests, or None for a TCP check.
    pub health_path: Option<String>,
    /// Seconds between health checks.
    pub health_interval: u32,
    /// Extra `ENV KEY=value` lines (deployment configuration).
    pub env: Vec<(String, String)>,
    /// Directories copied into the image, as `COPY <dir> /srv/app/<dir>`.
    pub extra_dirs: Vec<String>,
    /// Arguments appended to the entrypoint.
    pub args: Vec<String>,
}

impl Default for DockerConfig {
    fn default() -> Self {
        DockerConfig {
            app_name: "app".to_string(),
            cpp_name: "main".to_string(),
            binary_name: "server".to_string(),
            builder_image: "alpine:3.20".to_string(),
            runtime_image: "alpine:3.20".to_string(),
            port: 3000,
            health_path: Some("/healthz".to_string()),
            health_interval: 30,
            env: Vec::new(),
            extra_dirs: Vec::new(),
            args: Vec::new(),
        }
    }
}

/// A sanitised image tag and user name: Docker refuses most punctuation, and a
/// manifest name is allowed to contain it.
pub fn sanitize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "app".to_string()
    } else {
        trimmed
    }
}

impl DockerConfig {
    /// The tag `docker build -t` should use.
    pub fn image_tag(&self) -> String {
        format!("hardscript/{}:latest", sanitize_name(&self.app_name))
    }

    /// The workdir inside both stages.
    pub fn workdir(&self) -> String {
        format!("/srv/{}", sanitize_name(&self.app_name))
    }

    /// The unprivileged account the server runs as. A fixed uid keeps a
    /// bind-mounted volume writable without chowning it first.
    pub fn user(&self) -> String {
        "app".to_string()
    }

    pub fn uid(&self) -> u32 {
        10001
    }

    /// Flags for the in-container compile: optimised, portable, and with the
    /// C++ runtime linked in so the runtime stage needs no packages.
    pub fn build_flags(&self) -> Vec<String> {
        vec![
            "-std=c++17".to_string(),
            "-O2".to_string(),
            "-DNDEBUG".to_string(),
            "-flto".to_string(),
            "-fvisibility=hidden".to_string(),
            // Portable on purpose: `-march=native` produces a binary that dies
            // with SIGILL on any older CPU, which is every server that is not
            // the machine the image was built on.
            "-march=x86-64-v2".to_string(),
            "-mtune=generic".to_string(),
            "-static-libstdc++".to_string(),
            "-static-libgcc".to_string(),
            "-pthread".to_string(),
        ]
    }
}

/// The health check line for a config: an HTTP probe when the program has a
/// health path, a TCP probe when it does not.
pub fn healthcheck(cfg: &DockerConfig) -> String {
    let interval = if cfg.health_interval == 0 { 30 } else { cfg.health_interval };
    let probe = match &cfg.health_path {
        Some(path) => {
            let path = if path.starts_with('/') { path.clone() } else { format!("/{path}") };
            // Busybox wget, which Alpine already has: a health check that needs
            // a package installed is a health check that fails at build time.
            format!(
                "wget --quiet --tries=1 --spider http://127.0.0.1:{}{path} || exit 1",
                cfg.port
            )
        }
        None => format!("nc -z 127.0.0.1 {} || exit 1", cfg.port),
    };
    format!("HEALTHCHECK --interval={interval}s --timeout=3s --start-period=5s --retries=3 \\\n    CMD {probe}")
}

/// The whole Dockerfile. Deterministic: the same config always produces the
/// same bytes, so a regenerated file shows a real diff and nothing else.
pub fn render_dockerfile(cfg: &DockerConfig) -> String {
    let workdir = cfg.workdir();
    let name = sanitize_name(&cfg.app_name);
    let cpp = format!("{}.cpp", cfg.cpp_name);
    let mut s = String::new();

    let _ = writeln!(
        s,
        "# Generated by `hard build --docker` for {name} -- do not edit by hand.\n\
         #\n\
         # Build:  docker build -t {tag} .\n\
         # Run:    docker run --rm -p {port}:{port} {tag}\n\
         #\n\
         # The builder stage needs no HardScript toolchain: .hard/{cpp} and the\n\
         # runtime headers next to it are a complete translation unit.",
        tag = cfg.image_tag(),
        port = cfg.port
    );
    let _ = writeln!(s, "ARG BUILDER_IMAGE={}", cfg.builder_image);
    let _ = writeln!(s, "ARG RUNTIME_IMAGE={}\n", cfg.runtime_image);

    // ---- build stage -----------------------------------------------------
    let _ = writeln!(s, "FROM ${{BUILDER_IMAGE}} AS build");
    let _ = writeln!(s, "RUN apk add --no-cache g++ musl-dev");
    let _ = writeln!(s, "WORKDIR /src");
    // Only the two things a compile needs. The binary in .hard is not copied:
    // it is a different build of the same source, and copying it invites
    // shipping the host's artifact.
    let _ = writeln!(s, "COPY .hard/{cpp} /src/{cpp}");
    let _ = writeln!(s, "COPY .hard/hs_runtime*.hpp /src/");
    let flags = cfg.build_flags().join(" ");
    let _ = writeln!(
        s,
        "# -march=x86-64-v2 rather than native: an image built on one machine has to\n\
         # run on any other, and SIGILL before main() is a bad first impression.\n\
         RUN mkdir -p /out \\\n\
         && g++ {flags} -I /src /src/{cpp} -o /out/server -ldl\n"
    );

    // ---- runtime stage ---------------------------------------------------
    let _ = writeln!(s, "FROM ${{RUNTIME_IMAGE}}");
    let _ = writeln!(s, "LABEL org.hardscript.app=\"{name}\"");
    // Busybox's adduser takes no --uid, and addgroup is what owns the id: a
    // fixed uid is what makes a bind-mounted volume work without chowning it
    // first, so the two-step form is the one that does what it says.
    let _ = writeln!(
        s,
        "RUN addgroup -S -g {uid} {user} \\\n    && adduser -S -u {uid} -G {user} -h /srv {user}\n",
        uid = cfg.uid(),
        user = cfg.user()
    );
    let _ = writeln!(s, "WORKDIR {workdir}");
    for (k, v) in &cfg.env {
        let _ = writeln!(s, "ENV {k}={v}");
    }
    let _ = writeln!(
        s,
        "COPY --from=build /out/server {workdir}/{}\n",
        cfg.binary_name
    );
    for dir in &cfg.extra_dirs {
        let _ = writeln!(s, "COPY {dir} {workdir}/{dir}");
    }
    let _ = writeln!(s, "EXPOSE {}", cfg.port);
    let _ = writeln!(s, "USER {}", cfg.user());
    let _ = writeln!(s, "{}", healthcheck(cfg));
    let _ = writeln!(s, "ENTRYPOINT [\"{}\"]", format!("{workdir}/{}", cfg.binary_name));
    if cfg.args.is_empty() {
        let _ = writeln!(s, "# No arguments: the server reads its port from the source.");
    } else {
        let quoted: Vec<String> = cfg.args.iter().map(|a| format!("\"{a}\"")).collect();
        let _ = writeln!(s, "CMD [{}]", quoted.join(", "));
    }
    s
}

/// The build context is everything in the directory unless something says
/// otherwise, and a Rust project has gigabytes of `target/` in it. This is
/// written next to the Dockerfile for that reason.
///
/// `.hard/` is excluded except for what the builder stage needs: the generated
/// translation unit and the runtime headers. Excluding the whole directory
/// would break the build, and including all of it would ship the host's
/// binary and compile cache into the context.
pub fn render_dockerignore(cfg: &DockerConfig) -> String {
    let cpp = format!("{}.cpp", cfg.cpp_name);
    let mut s = String::new();
    let _ = writeln!(s, "# Generated by `hard build --docker` -- do not edit by hand.");
    let _ = writeln!(s, "target/");
    let _ = writeln!(s, ".git/");
    let _ = writeln!(s, "node_modules/");
    let _ = writeln!(s, "*.db");
    let _ = writeln!(s, "*.db-journal");
    let _ = writeln!(s, "*.so");
    let _ = writeln!(s, "*.o");
    let _ = writeln!(s, "*.log");
    // Keep only the two things the builder stage copies.
    let _ = writeln!(s, ".hard/*");
    let _ = writeln!(s, "!.hard/{cpp}");
    let _ = writeln!(s, "!.hard/hs_runtime*.hpp");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> DockerConfig {
        DockerConfig { app_name: "My App!".into(), port: 8080, ..DockerConfig::default() }
    }

    #[test]
    fn a_name_with_punctuation_still_makes_a_usable_tag() {
        assert_eq!(sanitize_name("My App!"), "my-app", "spaces and punctuation collapse");
        assert_eq!(sanitize_name("api-gateway_2"), "api-gateway_2", "valid names are kept");
        assert_eq!(sanitize_name("///"), "app", "a name that sanitizes away becomes app");
        assert_eq!(sanitize_name("-lead-"), "lead", "leading and trailing dashes are trimmed");
        let tag = DockerConfig { app_name: "My App!".into(), ..DockerConfig::default() }.image_tag();
        assert_eq!(tag, "hardscript/my-app:latest", "and the tag is namespaced");
    }

    #[test]
    fn the_dockerfile_has_two_stages() {
        let d = render_dockerfile(&cfg());
        assert!(d.contains("AS build"), "a build stage");
        assert!(d.contains("FROM ${RUNTIME_IMAGE}"), "and a runtime stage after it");
        assert_eq!(d.matches("FROM ").count(), 2, "exactly two FROM lines");
        assert!(d.find("AS build") < d.find("FROM ${RUNTIME_IMAGE}"), "build comes first");
    }

    #[test]
    fn the_builder_compiles_the_generated_translation_unit() {
        let d = render_dockerfile(&cfg());
        assert!(d.contains("COPY .hard/main.cpp /src/main.cpp"), "the generated C++ is copied in");
        assert!(d.contains("COPY .hard/hs_runtime*.hpp /src/"), "with the runtime headers");
        assert!(d.contains("g++ -std=c++17"), "compiled as C++17");
        assert!(d.contains("-o /out/server"), "to one known output path");
        assert!(d.contains("mkdir -p /out"), "whose directory is created first");
    }

    #[test]
    fn the_compile_asks_for_a_portable_cpu() {
        let d = render_dockerfile(&cfg());
        assert!(!d.contains("-march=native"), "native tuning would SIGILL elsewhere");
        assert!(d.contains("-march=x86-64-v2"), "a portable baseline instead");
        assert!(d.contains("-static-libstdc++"), "the C++ runtime is linked in");
    }

    #[test]
    fn the_runtime_stage_runs_as_nobody() {
        let d = render_dockerfile(&cfg());
        assert!(d.contains("addgroup -S -g 10001 app"), "a fixed gid");
        assert!(d.contains("adduser -S -u 10001"), "and a fixed uid, which busybox needs separately");
        assert!(d.contains("USER app"), "and the server runs as it");
        // Only the runtime stage matters here: the builder is allowed to
        // install a compiler, the image that ships is not.
        let runtime = &d[d.rfind("FROM ${RUNTIME_IMAGE}").unwrap()..];
        assert!(!runtime.contains("apk add"), "no packages in the runtime stage");
        assert!(runtime.contains("USER app"), "and the account is created there");
    }

    #[test]
    fn the_health_check_uses_what_alpine_has() {
        let d = render_dockerfile(&cfg());
        assert!(d.contains("HEALTHCHECK --interval=30s"), "an interval");
        assert!(d.contains("wget"), "busybox wget, already present");
        assert!(d.contains("http://127.0.0.1:8080/healthz"), "against the configured port and path");
        let tcp = DockerConfig { health_path: None, port: 9000, ..DockerConfig::default() };
        assert!(healthcheck(&tcp).contains("nc -z 127.0.0.1 9000"), "a TCP probe when there is no path");
    }

    #[test]
    fn a_health_path_without_a_slash_still_produces_a_url() {
        let c = DockerConfig { health_path: Some("healthz".into()), port: 3000, ..DockerConfig::default() };
        assert!(healthcheck(&c).contains("http://127.0.0.1:3000/healthz"), "the slash is added");
    }

    #[test]
    fn the_environment_and_extra_directories_reach_the_image() {
        let c = DockerConfig {
            env: vec![("DATABASE_URL".into(), "postgres://db/app".into())],
            extra_dirs: vec!["migrations".into(), "static".into()],
            ..DockerConfig::default()
        };
        let d = render_dockerfile(&c);
        assert!(d.contains("ENV DATABASE_URL=postgres://db/app"), "an ENV line");
        assert!(d.contains("COPY migrations /srv/app/migrations"), "migrations are copied");
        assert!(d.contains("COPY static /srv/app/static"), "and static files");
    }

    #[test]
    fn arguments_become_a_cmd_array() {
        let c = DockerConfig { args: vec!["--port".into(), "9000".into()], ..DockerConfig::default() };
        let d = render_dockerfile(&c);
        assert!(d.contains("CMD [\"--port\", \"9000\"]"), "an exec-form CMD, so no shell quoting");
        let none = render_dockerfile(&DockerConfig::default());
        assert!(!none.contains("CMD ["), "no empty CMD when there are no arguments");
    }

    #[test]
    fn the_build_context_carries_only_what_the_builder_needs() {
        let d = render_dockerignore(&cfg());
        assert!(d.contains("target/"), "the Rust build directory is excluded");
        assert!(d.contains(".git/"), "and the repository metadata");
        assert!(d.contains(".hard/*"), "the build directory is excluded by default");
        assert!(d.contains("!.hard/main.cpp"), "except the generated translation unit");
        assert!(d.contains("!.hard/hs_runtime*.hpp"), "and the runtime headers");
        assert_eq!(d.lines().count(), 12, "a short, explicit list, not a wildcard sweep");
    }

    #[test]
    fn the_context_keeps_the_cpp_the_builder_actually_uses() {
        let c = DockerConfig { cpp_name: "server".into(), ..DockerConfig::default() };
        let d = render_dockerignore(&c);
        assert!(d.contains("!.hard/server.cpp"), "the name follows the source file");
        assert!(!d.contains("main.cpp"), "not a hard-coded main.cpp");
    }

    #[test]
    fn generation_is_deterministic() {
        let a = render_dockerfile(&cfg());
        let b = render_dockerfile(&cfg());
        assert_eq!(a, b, "the same config produces the same bytes");
    }
}
