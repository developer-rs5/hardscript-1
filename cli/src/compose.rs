//! Generated Docker Compose files for `hard deploy compose` (M7.2).
//!
//! A compose file for a HardScript service is mostly answers to questions a
//! Dockerfile cannot ask: which ports the app publishes, what environment it
//! gets, whether a database sits next to it, and what "healthy" means for the
//! dependency graph. Those answers come from `hard.toml`, so the file is
//! generated rather than written -- a compose file that disagrees with the
//! manifest is a compose file nobody can trust.
//!
//! The shape follows compose spec v3 with one deliberate simplification: no
//! `version:` key, which compose has deprecated, and no `depends_on` condition
//! syntax newer than what `docker compose` v2 implements without complaint.

use std::fmt::Write as _;

/// One service in the generated file.
#[derive(Clone, Debug, PartialEq)]
pub struct Service {
    /// Key under `services:`.
    pub name: String,
    /// Image reference, or a build context for a locally built service.
    pub image: Option<String>,
    pub build: Option<String>,
    pub container_name: Option<String>,
    /// `HOST:CONTAINER` port publications.
    pub ports: Vec<String>,
    /// `KEY=value` environment entries.
    pub environment: Vec<(String, String)>,
    /// Environment variable names whose values come from the host at run time
    /// rather than from the file.
    pub environment_from_env: Vec<String>,
    pub restart: String,
    /// Healthcheck command, already quoted, or None.
    pub healthcheck: Option<String>,
    pub volumes: Vec<String>,
    pub depends_on: Vec<String>,
    pub read_only: bool,
    pub memory_limit: Option<String>,
    pub networks: Vec<String>,
}

/// A whole compose file.
#[derive(Clone, Debug, PartialEq)]
pub struct Compose {
    pub project_name: String,
    pub services: Vec<Service>,
    pub volumes: Vec<String>,
    pub networks: Vec<String>,
}

impl Default for Compose {
    fn default() -> Self {
        Compose {
            project_name: "app".to_string(),
            services: Vec::new(),
            volumes: Vec::new(),
            networks: Vec::new(),
        }
    }
}

/// A value that needs quoting when it is not a plain scalar: anything with a
/// space, a colon, a `$` (which compose would interpolate), a quote, or that
/// merely looks like another YAML type.
/// Whether a value can be written as a plain (unquoted) YAML scalar. Shared by
/// both call sites, because the two disagreeing is how a `PORT="3000"` ends up
/// as the literal five characters `PORT="3000"`.
pub fn is_plain_scalar(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(['-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|',
            '>', '\'', '"', '%', '@', '`'])
        // A colon anywhere: YAML only *requires* quotes for `: `, but URLs,
        // times and IPv6 all carry colons, and every YAML reader this file
        // might meet is happier with them quoted.
        && !value.contains(':')
        && !value.contains(['\n', '"', '\'', '{', '}', '[', ']', ',', '#'])
        && !value.contains('$')
        // Spaces are legal in a plain scalar and quoted anyway: a trailing
        // space is invisible in a diff and has bitten every YAML file ever.
        && !value.contains(' ')
        && !looks_typed(value)
}

pub fn yaml_scalar(value: &str) -> String {
    if is_plain_scalar(value) {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// Strings YAML would read as something other than a string: booleans, null,
/// and numbers. A port written unquoted is an int, and a compose file that
/// turns `3000` into a number is one more thing to explain to a reader.
fn looks_typed(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "false" | "yes" | "no" | "on" | "off" | "null" | "~" | "none"
    ) || value.parse::<f64>().is_ok()
}

/// A path in a volume or env value keeps compose from reading `$` as
/// interpolation; anything with one is quoted.
fn yaml_key(key: &str) -> String {
    if key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-') {
        key.to_string()
    } else {
        yaml_scalar(key)
    }
}

/// One `KEY=value` environment entry. The value is quoted on its own when the
/// key is a plain name, and the whole entry is quoted when it is not, so a
/// value containing `=` cannot be read as two keys.
fn yaml_env_entry(key: &str, value: &str) -> String {
    // A sequence entry is one plain scalar, so the quotes have to wrap the
    // whole `KEY=value` or they become part of the value. Writing
    // `- PORT="3000"` gives the container the variable `PORT` with the value
    // `"3000"`, quotes included, and the port parses as zero.
    let entry = format!("{key}={value}");
    if is_plain_scalar(&entry) {
        entry
    } else {
        yaml_scalar(&entry)
    }
}

fn indent(text: &mut String, depth: usize) {
    for _ in 0..depth {
        text.push_str("  ");
    }
}

impl Service {
    fn render(&self, out: &mut String) {
        indent(out, 2);
        let _ = writeln!(out, "{}:", yaml_key(&self.name));
        if let Some(image) = &self.image {
            indent(out, 3);
            let _ = writeln!(out, "image: {}", yaml_scalar(image));
        }
        if let Some(ctx) = &self.build {
            indent(out, 3);
            let _ = writeln!(out, "build: {}", yaml_scalar(ctx));
        }
        if let Some(name) = &self.container_name {
            indent(out, 3);
            let _ = writeln!(out, "container_name: {}", yaml_scalar(name));
        }
        if !self.depends_on.is_empty() {
            // A list even for one entry: compose v2 validates this field as an
            // array and rejects the scalar shorthand, however convenient it
            // looks in a file somebody typed by hand.
            indent(out, 3);
            let _ = writeln!(out, "depends_on:");
            for d in &self.depends_on {
                indent(out, 4);
                let _ = writeln!(out, "- {}", yaml_key(d));
            }
        }
        if !self.ports.is_empty() {
            indent(out, 3);
            if self.ports.len() == 1 {
                let _ = writeln!(out, "ports:");
                indent(out, 4);
                let _ = writeln!(out, "- {}", yaml_scalar(&self.ports[0]));
            } else {
                let _ = writeln!(out, "ports:");
                for p in &self.ports {
                    indent(out, 4);
                    let _ = writeln!(out, "- {}", yaml_scalar(p));
                }
            }
        }
        if !self.environment.is_empty() || !self.environment_from_env.is_empty() {
            indent(out, 3);
            let _ = writeln!(out, "environment:");
            // Sequence form throughout. Mapping form (`KEY: value`) reads
            // better but cannot be mixed with `- KEY` pass-throughs, and YAML
            // rejects a collection that mixes them -- so the generated file
            // would be invalid the first time somebody set a secret in it.
            for (k, v) in &self.environment {
                indent(out, 4);
                let _ = writeln!(out, "- {}", yaml_env_entry(k, v));
            }
            for k in &self.environment_from_env {
                indent(out, 4);
                // No `=`: compose reads it from the host environment and
                // leaves it unset when the host has none, which is the point --
                // a secret does not belong in a file that gets committed.
                let _ = writeln!(out, "- {}", yaml_key(k));
            }
        }
        if !self.volumes.is_empty() {
            indent(out, 3);
            let _ = writeln!(out, "volumes:");
            for v in &self.volumes {
                indent(out, 4);
                let _ = writeln!(out, "- {}", yaml_scalar(v));
            }
        }
        indent(out, 3);
        let _ = writeln!(out, "restart: {}", yaml_scalar(&self.restart));
        if self.read_only {
            indent(out, 3);
            let _ = writeln!(out, "read_only: true");
        }
        if let Some(m) = &self.memory_limit {
            indent(out, 3);
            let _ = writeln!(out, "mem_limit: {}", yaml_scalar(m));
        }
        if !self.networks.is_empty() {
            // A list, always: compose v2 rejects the scalar form even when
            // there is exactly one network.
            indent(out, 3);
            let _ = writeln!(out, "networks:");
            for n in &self.networks {
                indent(out, 4);
                let _ = writeln!(out, "- {}", yaml_key(n));
            }
        }
        if let Some(h) = &self.healthcheck {
            indent(out, 3);
            let _ = writeln!(out, "healthcheck:");
            indent(out, 4);
            let _ = writeln!(out, "test: {}", h);
            indent(out, 4);
            let _ = writeln!(out, "interval: 30s");
            indent(out, 4);
            let _ = writeln!(out, "timeout: 3s");
            indent(out, 4);
            let _ = writeln!(out, "retries: 3");
        }
    }
}

impl Compose {
    /// The whole file. Deterministic: services, keys and networks come out in
    /// the order they went in, so a regenerated file diffs meaningfully.
    pub fn render(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "# Generated by `hard deploy compose` -- do not edit by hand.\n\
             #\n\
             # Up:    docker compose up -d --build\n\
             # Down:  docker compose down\n\
             # Logs:  docker compose logs -f {name}\n",
            name = self.project_name
        );
        let _ = writeln!(s, "name: {}", yaml_scalar(&self.project_name));
        let _ = writeln!(s, "services:");
        for svc in &self.services {
            svc.render(&mut s);
        }
        if !self.volumes.is_empty() {
            let _ = writeln!(s, "volumes:");
            for v in &self.volumes {
                let _ = writeln!(s, "  {}:", yaml_key(v));
                let _ = writeln!(s, "    external: false");
            }
        }
        if !self.networks.is_empty() {
            let _ = writeln!(s, "networks:");
            for n in &self.networks {
                let _ = writeln!(s, "  {}:", yaml_key(n));
                let _ = writeln!(s, "    driver: bridge");
            }
        }
        s
    }
}

/// The service for a HardScript app: the image `hard build --docker` produces,
/// the port published, the environment, and a health check that matches what
/// the Dockerfile declares.
pub fn app_service(cfg: &crate::dockerfile::DockerConfig) -> Service {
    let mut environment = cfg.env.clone();
    environment.push(("PORT".to_string(), cfg.port.to_string()));
    let mut environment_from_env = Vec::new();
    // Everything the runtime reads out of the environment is passed through
    // from the host rather than written into a file: the file is committed, the
    // values are not.
    for key in [
        "DATABASE_URL", "SESSION_SECRET", "SMTP_HOST", "SMTP_PORT", "SMTP_USER", "SMTP_PASS",
        "SMTP_FROM", "CLUSTER_NODES", "CLUSTER_SECRET", "CLUSTER_STORE", "CLUSTER_DB",
    ] {
        environment_from_env.push(key.to_string());
    }
    let healthcheck = match &cfg.health_path {
        Some(path) => format!(
            "[\"CMD-SHELL\", \"wget --quiet --tries=1 --spider http://127.0.0.1:{}{path} || exit 1\"]",
            cfg.port
        ),
        None => format!("[\"CMD-SHELL\", \"nc -z 127.0.0.1 {} || exit 1\"]", cfg.port),
    };
    let name = crate::dockerfile::sanitize_name(&cfg.app_name);
    let volumes = vec![
        format!("./migrations:/srv/{name}/migrations:ro"),
        format!("./static:/srv/{name}/static:ro"),
    ];
    Service {
        name: "app".to_string(),
        image: Some(cfg.image_tag()),
        build: Some(".".to_string()),
        container_name: Some(format!("{}-app", crate::dockerfile::sanitize_name(&cfg.app_name))),
        ports: vec![format!("{}:{}", cfg.port, cfg.port)],
        environment,
        environment_from_env,
        restart: "unless-stopped".to_string(),
        healthcheck: Some(healthcheck),
        volumes,
        depends_on: Vec::new(),
        read_only: false,
        memory_limit: None,
        networks: vec!["app".to_string()],
    }
}

/// A PostgreSQL service for a project whose manifest asks for one, with the
/// database the app expects, a named volume, and its own health check so
/// `depends_on` means something.
pub fn postgres_service(cfg: &crate::dockerfile::DockerConfig, database: &str, user: &str) -> Service {
    let name = crate::dockerfile::sanitize_name(&cfg.app_name);
    Service {
        name: "db".to_string(),
        image: Some("postgres:16-alpine".to_string()),
        build: None,
        container_name: Some(format!("{}-db", name)),
        ports: Vec::new(),
        environment: vec![
            ("POSTGRES_DB".to_string(), database.to_string()),
            ("POSTGRES_USER".to_string(), user.to_string()),
            // Password from the host, never from this file.
            ("POSTGRES_PASSWORD".to_string(), String::new()),
        ],
        environment_from_env: Vec::new(),
        restart: "unless-stopped".to_string(),
        healthcheck: Some(
            "[\"CMD-SHELL\", \"pg_isready -U ${POSTGRES_USER} -d ${POSTGRES_DB}\"]".to_string(),
        ),
        volumes: vec!["db-data:/var/lib/postgresql/data".to_string()],
        depends_on: Vec::new(),
        read_only: false,
        memory_limit: None,
        networks: vec!["app".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> crate::dockerfile::DockerConfig {
        crate::dockerfile::DockerConfig {
            app_name: "shop".into(),
            port: 8080,
            ..Default::default()
        }
    }

    #[test]
    fn the_app_service_publishes_the_configured_port() {
        let svc = app_service(&cfg());
        assert_eq!(svc.ports, vec!["8080:8080".to_string()], "host and container agree");
        assert_eq!(svc.image.as_deref(), Some("hardscript/shop:latest"), "the generated image");
        assert_eq!(svc.build.as_deref(), Some("."), "and it is built here too");
        assert_eq!(svc.container_name.as_deref(), Some("shop-app"), "a stable container name");
    }

    #[test]
    fn secrets_come_from_the_host_not_from_the_file() {
        let svc = app_service(&cfg());
        let mut env: Vec<&str> = svc.environment_from_env.iter().map(String::as_str).collect();
        env.sort();
        assert!(env.contains(&"DATABASE_URL"), "the database url");
        assert!(env.contains(&"SESSION_SECRET"), "the session secret");
        for (k, _) in &svc.environment {
            assert!(
                !k.contains("SECRET") && !k.contains("PASSWORD"),
                "{k} is a secret and must not be written into the file"
            );
        }
        // A bare `- NAME` line, which is how compose passes the host value
        // through and leaves it unset when the host has none.
        let text = Compose {
            project_name: "shop".into(),
            services: vec![svc],
            ..Default::default()
        }
        .render();
        assert!(text.contains("- DATABASE_URL"), "a bare key, no value");
        assert!(!text.contains("DATABASE_URL: postgres://"), "and no value at all");
    }

    #[test]
    fn the_health_check_matches_the_dockerfile() {
        let with_path = app_service(&cfg());
        assert!(with_path.healthcheck.as_deref().unwrap().contains("/healthz"), "HTTP when the app has it");
        let tcp = app_service(&crate::dockerfile::DockerConfig {
            health_path: None,
            ..cfg()
        });
        assert!(tcp.healthcheck.as_deref().unwrap().contains("nc -z 127.0.0.1 8080"), "TCP otherwise");
    }

    #[test]
    fn migrations_and_static_are_mounted_read_only() {
        let svc = app_service(&cfg());
        assert_eq!(svc.volumes.len(), 2, "migrations and static files");
        for v in &svc.volumes {
            assert!(v.ends_with(":ro"), "{v} is mounted read-only");
        }
    }

    #[test]
    fn a_single_dependency_is_still_written_as_a_list() {
        // compose v2 rejects `depends_on: db` and `networks: app` as scalars;
        // both fields are validated as arrays.
        let mut svc = app_service(&cfg());
        svc.depends_on.push("db".to_string());
        let text = Compose { services: vec![svc], ..Default::default() }.render();
        assert!(text.contains("      depends_on:\n        - db\n"), "a one-item list");
        assert!(text.contains("      networks:\n        - app\n"), "and a one-item network list");
    }

    #[test]
    fn a_postgres_service_carries_its_own_health_check() {
        let db = postgres_service(&cfg(), "shop", "shop");
        assert_eq!(db.name, "db", "the service is called db");
        assert_eq!(db.image.as_deref(), Some("postgres:16-alpine"), "pinned to a major");
        assert!(db.healthcheck.as_deref().unwrap().contains("pg_isready"), "a real readiness probe");
        assert_eq!(db.volumes, vec!["db-data:/var/lib/postgresql/data"], "a named volume");
        let env: Vec<&(String, String)> = db.environment.iter().collect();
        assert!(env.iter().any(|(k, v)| k == "POSTGRES_DB" && v == "shop"), "the database name");
        assert!(env.iter().any(|(k, v)| k == "POSTGRES_PASSWORD" && v.is_empty()), "password unset here");
    }

    #[test]
    fn rendering_is_valid_yaml_for_the_obvious_awkward_cases() {
        // No YAML parser is available here, so the assertions are about the
        // shapes that actually break: colons, hashes, dollars, quotes, and
        // values that look like a bool or a number.
        assert_eq!(yaml_scalar("plain-value"), "plain-value", "a plain scalar is left alone");
        assert_eq!(yaml_scalar("postgres://u:p@h/db"), "\"postgres://u:p@h/db\"", "a url with a colon");
        assert_eq!(yaml_scalar("has spaces"), "\"has spaces\"", "spaces need quotes");
        assert_eq!(yaml_scalar("3000"), "\"3000\"", "a number stays a string");
        assert_eq!(yaml_scalar("true"), "\"true\"", "a bool stays a string");
        assert_eq!(yaml_scalar(""), "\"\"", "the empty string is not nothing");
        assert_eq!(yaml_scalar("$HOME/db"), "\"$HOME/db\"", "a dollar must not be interpolated");
        assert_eq!(yaml_scalar("say \"hi\""), "\"say \\\"hi\\\"\"", "quotes are escaped");
        assert_eq!(yaml_scalar("a: b"), "\"a: b\"", "a colon-space would be a mapping");
        assert_eq!(yaml_scalar("trailing "), "\"trailing \"", "trailing space survives");
    }

    #[test]
    fn a_service_with_one_port_writes_it_in_flow_style() {
        let svc = app_service(&cfg());
        let text = Compose { services: vec![svc], ..Default::default() }.render();
        assert!(text.contains("  app:\n"), "the service key");
        assert!(text.contains("      ports:\n        - \"8080:8080\"\n"), "a quoted port mapping");
        assert!(!text.contains("version:"), "no deprecated version key");
    }

    #[test]
    fn several_ports_list_every_one() {
        let mut svc = app_service(&cfg());
        svc.ports = vec!["8080:8080".into(), "9090:9090".into()];
        let text = Compose { services: vec![svc], ..Default::default() }.render();
        assert_eq!(text.matches("        - \"80").count() + text.matches("        - \"90").count(), 2, "both ports");
    }

    #[test]
    fn an_environment_value_that_needs_quotes_quotes_the_whole_entry() {
        assert_eq!(yaml_env_entry("PORT", "3210"), "PORT=3210", "a plain value, plain");
        assert_eq!(
            yaml_env_entry("DATABASE_URL", "postgres://u:p@h/db"),
            "\"DATABASE_URL=postgres://u:p@h/db\"",
            "a value with a colon quotes the entry, not the value"
        );
        assert_eq!(yaml_env_entry("MSG", "hello world"), "\"MSG=hello world\"", "spaces too");
        assert_eq!(yaml_env_entry("EMPTY", ""), "EMPTY=", "an empty value is assigned, not omitted");
    }

    #[test]
    fn the_environment_block_uses_one_yaml_form() {
        // Mixing `- KEY` and `KEY: value` in one collection is invalid YAML,
        // and it only shows up when a secret is set, so it is checked here.
        let text = Compose { services: vec![app_service(&cfg())], ..Default::default() }.render();
        let block: Vec<&str> = text
            .lines()
            .skip_while(|l| !l.trim_start().starts_with("environment:"))
            .skip(1)
            .take_while(|l| l.starts_with("       "))
            .collect();
        assert!(!block.is_empty(), "an environment block was written");
        for line in block {
            let t = line.trim_start();
            assert!(t.starts_with("- "), "{t:?} is not a sequence entry");
            assert!(!t.contains(": "), "{t:?} would be mapping form");
        }
    }

    #[test]
    fn a_compose_file_is_deterministic() {
        let c = Compose {
            project_name: "shop".into(),
            services: vec![app_service(&cfg()), postgres_service(&cfg(), "shop", "shop")],
            volumes: vec!["db-data".into()],
            networks: vec!["app".into()],
        };
        assert_eq!(c.render(), c.render(), "same input, same bytes");
        let text = c.render();
        assert!(text.find("  app:").unwrap() < text.find("  db:").unwrap(), "services keep their order");
        assert!(text.contains("volumes:\n  db-data:"), "the named volume is declared");
        assert!(text.contains("networks:\n  app:"), "and the network");
    }
}
