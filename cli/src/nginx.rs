//! `hard deploy nginx` and `hard deploy https`: the reverse proxy and the
//! certificate (M7.8).
//!
//! A HardScript server listens on a port on 127.0.0.1 and expects something in
//! front of it: a hostname, TLS, and static files served without going through
//! the application. That "something" is an nginx server block, and writing one
//! by hand for every service means every service gets a slightly different one,
//! usually with the WebSocket headers missing and `client_max_body_size` left at
//! nginx's one megabyte.
//!
//! Two details in the generated file are not taste, they are correctness:
//!
//! * `proxy_http_version 1.1` plus the `Upgrade`/`Connection` headers. Without
//!   them nginx answers a WebSocket upgrade with a 200 and closes the socket,
//!   which presents as a connection that opens and then vanishes.
//! * The ACME challenge is served from a fixed webroot on port 80, *before*
//!   the redirect to HTTPS, and the certificate is issued with `--webroot`.
//!   Certbot's `standalone` mode wants port 80 to itself and needs the
//!   service stopped; the webroot mode is what lets a renew happen on a live
//!   server.

use crate::release::{sh_quote, RemoteLayout};
use std::fmt::Write as _;
use crate::ssh::{Ssh, SshTarget};
use std::path::Path;

/// What the generated server block needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NginxConfig {
    /// The site name, which is also the file name.
    pub site: String,
    /// The hostnames this block answers to. The first is the certificate's
    /// subject and the one the redirect uses.
    pub domains: Vec<String>,
    /// Where the application listens.
    pub port: u16,
    /// `shared/static`, served by nginx rather than by the application.
    pub static_dir: String,
    /// `shared/acme`, the webroot a certificate is issued against.
    pub acme_dir: String,
    /// Issue a certificate and redirect port 80 to it.
    pub tls: bool,
    /// Where the certificate and its key are read from.
    pub cert: String,
    pub cert_key: String,
    /// The largest request body the application will accept, in megabytes.
    pub body_limit_mb: u32,
    /// How long nginx waits for the application.
    pub proxy_read_timeout: u32,
    /// The health path, given its own short-timeout location.
    pub health_path: Option<String>,
    /// Hide the version header.
    pub hide_version: bool,
}

impl NginxConfig {
    /// With a certificate: TLS on 443, and port 80 for the challenge and the
    /// redirect.
    pub fn with_tls(mut self) -> NginxConfig {
        self.tls = true;
        self
    }
}

impl NginxConfig {
    /// The block for a service deployed with this layout.
    pub fn new(site: &str, layout: &RemoteLayout, port: u16) -> NginxConfig {
        let site = crate::dockerfile::sanitize_name(site);
        let shared = format!("{}/shared", layout.root);
        NginxConfig {
            cert: format!("{shared}/tls/fullchain.pem"),
            cert_key: format!("{shared}/tls/privkey.pem"),
            acme_dir: format!("{shared}/acme"),
            static_dir: format!("{shared}/static"),
            site,
            domains: Vec::new(),
            port,
            tls: false,
            body_limit_mb: 16,
            proxy_read_timeout: 60,
            health_path: None,
            hide_version: true,
        }
    }

    pub fn with_domains(mut self, domains: &[String]) -> NginxConfig {
        self.domains = domains.to_vec();
        self
    }

    /// The primary hostname: the certificate's subject and the redirect target.
    pub fn primary(&self) -> &str {
        self.domains.first().map(String::as_str).unwrap_or("")
    }

    /// The `server_name` line.
    pub fn server_name(&self) -> String {
        if self.domains.is_empty() {
            "localhost".to_string()
        } else {
            self.domains.join(" ")
        }
    }

    /// The `server_name` for port 80: the same names, plus the certificate's
    /// own name when it is a wildcard or a bare domain that redirects to www.
    pub fn server_name_tls(&self) -> String {
        self.server_name()
    }

    /// The whole file. Deterministic: the same config always produces the same
    /// bytes, so a regenerated file shows a real diff and nothing else.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "# {}.conf -- written by `hard deploy nginx`.", self.site);
        let _ = writeln!(out, "# Regenerating overwrites this file; the unit and the env");
        let _ = writeln!(out, "# files are not affected.");
        out.push('\n');
        out.push_str(&self.upstream());
        out.push('\n');
        if self.tls {
            out.push_str(&self.port_80_block());
            out.push('\n');
            out.push_str(&self.tls_block());
        } else {
            out.push_str(&self.plain_block());
        }
        out
    }

    /// The upstream, so a later `server` block for the same service reuses it.
    fn upstream(&self) -> String {
        format!(
            "upstream {}_app {{\n    server 127.0.0.1:{};\n    keepalive 32;\n}}\n",
            self.site, self.port
        )
    }

    /// Port 80: the ACME challenge, then everything else to HTTPS.
    fn port_80_block(&self) -> String {
        let primary = self.primary();
        let mut out = String::new();
        let _ = writeln!(out, "server {{");
        let _ = writeln!(out, "    listen 80;");
        let _ = writeln!(out, "    listen [::]:80;");
        let _ = writeln!(out, "    server_name {};", self.server_name());
        out.push('\n');
        let _ = writeln!(out, "    # The certificate is issued against this webroot, so it has to");
        let _ = writeln!(out, "    # be reachable over plain HTTP before there is a certificate.");
        let _ = writeln!(
            out,
            "    location ^~ /.well-known/acme-challenge/ {{\n        root {};\n    }}",
            acme_root(self)
        );
        out.push('\n');
        if primary.is_empty() {
            let _ = writeln!(
                out,
                "    location / {{\n        proxy_pass http://{}_app;\n    }}",
                self.site
            );
        } else {
            let _ = writeln!(out, "    location / {{\n        return 301 https://$host$request_uri;\n    }}");
        }
        out.push_str("}\n");
        out
    }

    fn plain_block(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "server {{");
        let _ = writeln!(out, "    listen 80;");
        let _ = writeln!(out, "    listen [::]:80;");
        let _ = writeln!(out, "    server_name {};", self.server_name());
        out.push_str(&self.common_body(4));
        out.push_str("}\n");
        out
    }

    fn tls_block(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "server {{");
        let _ = writeln!(out, "    listen 443 ssl;");
        let _ = writeln!(out, "    listen [::]:443 ssl;");
        let _ = writeln!(out, "    http2 on;");
        let _ = writeln!(out, "    server_name {};", self.server_name_tls());
        out.push('\n');
        let _ = writeln!(out, "    ssl_certificate     {};", self.cert);
        let _ = writeln!(out, "    ssl_certificate_key {};", self.cert_key);
        let _ = writeln!(out, "    ssl_protocols       TLSv1.2 TLSv1.3;");
        let _ = writeln!(out, "    ssl_prefer_server_ciphers off;");
        let _ = writeln!(out, "    ssl_session_cache   shared:{}:10m;", self.site);
        let _ = writeln!(out, "    ssl_session_timeout 1d;");
        out.push('\n');
        out.push_str(&self.common_body(4));
        out.push_str("}\n");
        out
    }

    /// The part both blocks share. Indented for the block that contains it.
    fn common_body(&self, indent: usize) -> String {
        let pad = " ".repeat(indent);
        let mut out = String::new();
        if self.hide_version {
            let _ = writeln!(out, "{pad}server_tokens off;");
        }
        let _ = writeln!(out, "{pad}client_max_body_size {}m;", self.body_limit_mb);
        out.push('\n');

        // Static files are nginx's job: they do not change between releases and
        // do not need the application to be running.
        let _ = writeln!(out, "{pad}location /static/ {{");
        let _ = writeln!(out, "{pad}    alias {}/;", self.static_dir);
        let _ = writeln!(out, "{pad}    access_log off;");
        let _ = writeln!(out, "{pad}    expires 7d;");
        let _ = writeln!(out, "{pad}    add_header Cache-Control \"public\" always;");
        let _ = writeln!(out, "{pad}}}\n");

        // The health endpoint answers before the application is asked, so a load
        // balancer does not send traffic to a service that is still starting.
        if let Some(path) = &self.health_path {
            let _ = writeln!(out, "{pad}location = {path} {{");
            let _ = writeln!(out, "{pad}    access_log off;");
            let _ = writeln!(out, "{pad}    proxy_pass http://{}_app;", self.site);
            let _ = writeln!(out, "{pad}    proxy_connect_timeout 2s;");
            let _ = writeln!(out, "{pad}    proxy_read_timeout 2s;");
            let _ = writeln!(out, "{pad}}}\n");
        }

        let _ = writeln!(out, "{pad}location / {{");
        let _ = writeln!(out, "{pad}    proxy_pass http://{}_app;", self.site);
        out.push('\n');
        for (key, value) in [
            ("proxy_set_header Host", "$host"),
            ("proxy_set_header X-Real-IP", "$remote_addr"),
            ("proxy_set_header X-Forwarded-For", "$proxy_add_x_forwarded_for"),
            ("proxy_set_header X-Forwarded-Proto", "$scheme"),
        ] {
            let _ = writeln!(out, "{pad}    {key:<28} {value};");
        }
        // The two lines that make WebSockets work through a proxy at all.
        let _ = writeln!(out, "{pad}    proxy_http_version 1.1;");
        let _ = writeln!(out, "{pad}    proxy_set_header   Upgrade $http_upgrade;");
        let _ = writeln!(out, "{pad}    proxy_set_header   Connection $connection_upgrade;");
        let _ = writeln!(out, "{pad}    proxy_read_timeout {}s;", self.proxy_read_timeout);
        let _ = writeln!(out, "{pad}    proxy_send_timeout {}s;", self.proxy_read_timeout);
        out.push('\n');
        if self.tls {
            let _ = writeln!(out, "{pad}    add_header Strict-Transport-Security \"max-age=31536000\" always;");
            let _ = writeln!(out, "{pad}    add_header X-Content-Type-Options    \"nosniff\" always;");
            let _ = writeln!(out, "{pad}    add_header X-Frame-Options           \"SAMEORIGIN\" always;");
            let _ = writeln!(out, "{pad}    add_header Referrer-Policy           \"strict-origin-when-cross-origin\" always;");
        }
        let _ = writeln!(out, "{pad}}}");
        out
    }

    /// A full configuration, for `nginx -t`: the block, the connection header
    /// map it uses, and the temporary paths `nginx -t` insists on having.
    ///
    /// `nginx -t` validates a file, not a fragment, so testing a server block
    /// means wrapping it. On a machine that is not root it also needs somewhere
    /// to put its pid, logs and temp files, which is what the prefix is for.
    pub fn test_wrapper(&self, dir: &Path) -> String {
        let dir = dir.display();
        format!(
            "pid {dir}/nginx.pid;\n\
             error_log {dir}/error.log;\n\
             events {{}}\n\
             http {{\n\
             \x20   client_body_temp_path {dir}/body;\n\
             \x20   proxy_temp_path {dir}/proxy;\n\
             \x20   fastcgi_temp_path {dir}/fastcgi;\n\
             \x20   uwsgi_temp_path {dir}/uwsgi;\n\
             \x20   scgi_temp_path {dir}/scgi;\n\
             \x20   access_log {dir}/access.log;\n\
             \x20   map $http_upgrade $connection_upgrade {{ default upgrade; '' close; }}\n{}\n\
             }}\n",
            self.render()
        )
    }

    /// The certbot command that issues (or renews) the certificate against the
    /// webroot the port 80 block serves.
    pub fn certbot_command(&self, email: Option<&str>, sudo_user: Option<&str>) -> String {
        let sudo = crate::release::privilege_prefix(sudo_user.unwrap_or("root"));
        let mut out = format!(
            "{sudo}certbot certonly --webroot -w {}",
            sh_quote(&acme_root(self))
        );
        if let Some(email) = email {
            out.push_str(&format!(" --email {}", sh_quote(email)));
        }
        out.push_str(" --agree-tos --non-interactive");
        for domain in &self.domains {
            out.push_str(&format!(" -d {}", sh_quote(domain)));
        }
        out.push_str(" --keep-until-expiring");
        out
    }

    /// The command that installs the file and reloads nginx -- and tests the
    /// whole configuration first, because a reload with a bad file takes the
    /// sites that were working down as well.
    pub fn install_command(&self, sudo_user: Option<&str>) -> String {
        let sudo = crate::release::privilege_prefix(sudo_user.unwrap_or("root"));
        let body = self.render();
        let available = format!("/etc/nginx/sites-available/{}.conf", self.site);
        let enabled = format!("/etc/nginx/sites-enabled/{}.conf", self.site);
        format!(
            "umask 022; cat > {available} <<'HS_NGINX_EOF'\n{body}HS_NGINX_EOF\n\
             chmod 0644 {available}; mkdir -p /etc/nginx/sites-enabled; \
             ln -sfn {available} {enabled}; \
             {sudo}nginx -t && {sudo}systemctl reload nginx",
            available = sh_quote(&available),
            enabled = sh_quote(&enabled)
        )
    }
}

/// The webroot a certificate is issued against: the directory that ends in
/// `/.well-known/acme-challenge`, so the challenge file lands where a browser
/// will look for it.
fn acme_root(cfg: &NginxConfig) -> String {
    cfg.acme_dir.clone()
}

/// A structural check, for a machine with no nginx to ask. It catches the
/// mistakes a template can make -- an unclosed brace, a directive with no
/// semicolon, a block with no `server_name` -- and nothing else.
pub fn validate(text: &str) -> Vec<String> {
    let mut problems: Vec<String> = Vec::new();
    let mut depth: i32 = 0;
    let mut in_comment = false;
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        let line = if in_comment {
            match line.find("*/") {
                Some(end) => {
                    in_comment = false;
                    line[end + 2..].trim()
                }
                None => continue,
            }
        } else if line.starts_with('#') || line.is_empty() {
            continue
        } else {
            line
        };
        for ch in line.chars() {
            match ch {
                '#' => break,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth < 0 {
                        problems.push(format!("line {}: a `}}` with no block open", n + 1));
                        depth = 0;
                    }
                }
                _ => {}
            }
        }
        if depth > 0 && !line.ends_with('{') && !line.ends_with(';') && !line.ends_with('}') {
            problems.push(format!("line {}: `{}` ends in neither `;` nor `{{`", n + 1, line));
        }
    }
    if depth != 0 {
        problems.push(format!("{depth} block(s) left open"));
    }
    if !text.contains("server_name") {
        problems.push("no server_name: the block would answer for every host".to_string());
    }
    if !text.contains("proxy_pass") {
        problems.push("no proxy_pass: nothing is forwarded to the application".to_string());
    }
    if text.contains('\t') {
        problems.push("a tab: nginx reads it, and every other tool in the file does not".to_string());
    }
    problems
}

// ---------------------------------------------------------------------------
// The commands
// ---------------------------------------------------------------------------

/// `hard deploy nginx [--env <name>] [--domain <name>] [--tls] [--check]
///                   [--install <host>]`
pub fn cmd_nginx(args: &[String]) {
    let (mut cfg, _) = config_from_args(args);
    if args.iter().any(|a| a == "--tls") {
        cfg = cfg.with_tls();
    }
    let text = cfg.render();

    if args.iter().any(|a| a == "--check") {
        let problems = validate(&text);
        for p in &problems {
            eprintln!("  {p}");
        }
        if problems.is_empty() {
            match check_with_nginx(&cfg) {
                Some(Ok(note)) => println!("{}: {note}", cfg.site),
                Some(Err(report)) => {
                    eprintln!("{}: nginx rejected the configuration:\n{report}", cfg.site);
                    std::process::exit(1);
                }
                None => println!("{}: structurally fine; nginx is not installed to ask", cfg.site),
            }
        } else {
            eprintln!("{}: {} problem(s)", cfg.site, problems.len());
            std::process::exit(1);
        }
        return;
    }

    if let Some(host) = flag(args, "--install").or_else(|| positional(args)) {
        let target = match SshTarget::parse(&host) {
            Ok(t) => t,
            Err(e) => crate::die(&e),
        };
        let user = args_user(args);
        let mut ssh = crate::ssh::SystemSsh::new();
        match ssh.run(&target, &cfg.install_command(user.as_deref())) {
            Ok(out) if out.ok() => {
                let last = cfg
                    .install_command(user.as_deref())
                    .lines()
                    .last()
                    .unwrap_or("")
                    .to_string();
                println!("installed {}", cfg.site);
                println!("  {last}");
            }
            Ok(out) => {
                eprintln!("{}", out.stderr.trim());
                std::process::exit(1);
            }
            Err(e) => crate::die(&e),
        }
        return;
    }
    print!("{text}");
}

/// `hard deploy https [--env <name>] [--email <address>] [--run <host>]`
///
/// The certificate is a decision the operator makes with a real domain, so
/// this prints the exact command by default and only runs it when a host is
/// named. Certbot is the only thing here that talks to the outside world, and
/// running it by surprise is how a rate limit gets spent.
pub fn cmd_https(args: &[String]) {
    let (cfg, environment_email) = config_from_args(args);
    if cfg.domains.is_empty() {
        eprintln!("hard deploy https: which hostname? add `domain = \"app.example.com\"` to [env.<name>], or pass --domain");
        std::process::exit(2);
    }
    let email = flag(args, "--email").or(environment_email);
    let user = args_user(args);
    let command = cfg.certbot_command(email.as_deref(), user.as_deref());
    let Some(host) = flag(args, "--run") else {
        println!("{command}");
        println!();
        println!("# on the host that serves {}/", cfg.primary());
        println!("# the certificate is written to {}", cfg.cert);
        return;
    };
    let target = match SshTarget::parse(&host) {
        Ok(t) => t,
        Err(e) => crate::die(&e),
    };
    let mut ssh = crate::ssh::SystemSsh::new();
    match ssh.run(&target, &command) {
        Ok(out) if out.ok() => {
            println!("issued a certificate for {}", cfg.server_name());
            println!("  {}", cfg.cert);
        }
        Ok(out) => {
            eprintln!("{}", out.stderr.trim());
            eprintln!("the certificate was not issued; check that {} serves {} on port 80",
                cfg.primary(), cfg.acme_dir);
            std::process::exit(1);
        }
        Err(e) => crate::die(&e),
    }
}

/// The config a `nginx` or `https` command works from: the environment's
/// layout, port, health path and domain, with the flags on top.
fn config_from_args(args: &[String]) -> (NginxConfig, Option<String>) {
    let (project, manifest) = crate::environments::project_manifest();
    let env = flag(args, "--env")
        .as_ref()
        .map(|name| crate::environments::resolve(&project, &manifest, name).unwrap_or_else(|e| crate::die(&e)));
    let dir = env.as_ref().map(|e| e.dir.clone()).unwrap_or_default();
    let layout = RemoteLayout::new(&dir, &project).unwrap_or_else(|e| crate::die(&e));
    let port = env.as_ref().and_then(|e| e.port).unwrap_or_else(|| {
        // The manifest's port, the same one a deploy probes.
        manifest
            .server
            .port
            .or_else(|| flag(args, "--port").and_then(|p| p.parse().ok()))
            .unwrap_or(3000)
    });
    let mut domains: Vec<String> = env.as_ref().and_then(|e| e.domain.clone()).into_iter().collect();
    for a in args {
        if a == "--domain" {
            continue;
        }
        if let Some(d) = a.strip_prefix("--domain=") {
            domains.push(d.to_string());
        }
    }
    // The site is named after the project, the way the unit is.
    let site = manifest.name.clone();
    let site = if site.is_empty() { project.clone() } else { site };
    let mut cfg = NginxConfig::new(&site, &layout, port).with_domains(&domains);
    if let Some(path) = env.as_ref().and_then(|e| e.health_path.clone()) {
        cfg.health_path = Some(path);
    }
    (cfg, env.as_ref().and_then(|e| e.tls_email.clone()))
}

/// `--user <name>`: the account the site is written as.
fn args_user(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--user" {
            return args.get(i + 1).cloned();
        }
        if let Some(v) = args[i].strip_prefix("--user=") {
            return Some(v.to_string());
        }
        i += 1;
    }
    None
}

/// `--flag value` or `--flag=value`.
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
    let with_value = ["--env", "--domain", "--email", "--install", "--run", "--port", "--user"];
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if with_value.contains(&a) {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        return Some(a.to_string());
    }
    None
}

/// Ask the installed nginx, if there is one. `None` means there is nothing to
/// ask, which is not a failure.
///
/// A TLS block names a certificate that does not exist yet on a machine that
/// has not issued one, and nginx refuses to parse the file over a missing
/// certificate -- which says nothing about the block. So a throwaway one is
/// generated for the check and the report says that is what happened, rather
/// than leaving the operator to work out that the error is not about the
/// template.
fn check_with_nginx(cfg: &NginxConfig) -> Option<Result<String, String>> {
    let nginx = which("nginx")?;
    let dir = std::env::temp_dir().join(format!("hs-nginx-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).ok()?;
    let mut cfg = cfg.clone();
    let mut note = "nginx -t says the configuration is valid".to_string();
    if !Path::new(&cfg.cert).exists() {
        let made = throwaway_certificate(&dir);
        match made {
            Some((cert, key)) => {
                note = format!(
                    "nginx -t says the configuration is valid (with a throwaway certificate; {} is not issued yet)",
                    cfg.cert
                );
                cfg.cert = cert;
                cfg.cert_key = key;
            }
            None => {
                note = format!("structurally fine, but {} does not exist and openssl is not here to stand in for it", cfg.cert);
            }
        }
    }
    let path = dir.join("nginx.conf");
    std::fs::write(&path, cfg.test_wrapper(&dir)).ok()?;
    let out = std::process::Command::new(&nginx)
        .arg("-t")
        .arg("-c")
        .arg(&path)
        .arg("-p")
        .arg(&dir)
        .output()
        .ok()?;
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = std::fs::remove_dir_all(&dir);
    if out.status.success() {
        Some(Ok(note))
    } else {
        Some(Err(report.trim().to_string()))
    }
}

/// A self-signed certificate and key in `dir`, for a test that only needs
/// nginx to have something to read.
fn throwaway_certificate(dir: &Path) -> Option<(String, String)> {
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    let out = std::process::Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout"])
        .arg(&key)
        .arg("-out")
        .arg(&cert)
        .args(["-days", "1", "-subj", "/CN=hardscript-check"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some((cert.display().to_string(), key.display().to_string()))
}

/// The full path of a program on PATH.
pub fn which(program: &str) -> Option<String> {
    let path = std::env::var("PATH").ok()?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
        .map(|p| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn layout() -> RemoteLayout {
        RemoteLayout::new("/srv/app", "app").expect("a layout")
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hs-nginx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn tls() -> NginxConfig {
        NginxConfig::new("app", &layout(), 3000)
            .with_domains(&["app.example.com".to_string(), "www.app.example.com".to_string()])
            .with_tls()
    }

    #[test]
    fn the_block_proxies_to_the_service_on_127_0_0_1() {
        let cfg = NginxConfig::new("app", &layout(), 3000).with_domains(&["app.example.com".into()]);
        let text = cfg.render();
        assert!(text.contains("upstream app_app {\n    server 127.0.0.1:3000;"), "{text}");
        assert!(text.contains("proxy_pass http://app_app;"), "{text}");
        assert!(text.contains("server_name app.example.com;"), "{text}");
        assert!(text.contains("listen 80;"), "and it listens where a browser looks: {text}");
        assert!(!text.contains("443"), "no TLS until it is asked for: {text}");
    }

    #[test]
    fn websockets_survive_the_proxy() {
        let text = NginxConfig::new("app", &layout(), 3000).render();
        // Without these three lines a WebSocket connects and then disappears.
        assert!(text.contains("proxy_http_version 1.1;"), "{text}");
        assert!(text.contains("proxy_set_header   Upgrade $http_upgrade;"), "{text}");
        assert!(text.contains("proxy_set_header   Connection $connection_upgrade;"), "{text}");
    }

    #[test]
    fn a_certificate_gets_a_challenge_path_and_a_redirect() {
        let cfg = tls();
        let text = cfg.render();
        assert!(text.contains("location ^~ /.well-known/acme-challenge/ {"), "{text}");
        assert!(text.contains("root /srv/app/shared/acme;"), "the webroot certbot is told to use: {text}");
        assert!(text.contains("return 301 https://$host$request_uri;"), "and the redirect: {text}");
        // The challenge has to come before the redirect, or the certificate can
        // never be issued for a service that redirects everything.
        let challenge = text.find("acme-challenge").expect("a challenge block");
        let redirect = text.find("return 301").expect("a redirect");
        assert!(challenge < redirect, "the challenge is served first: {text}");
        assert!(text.contains("ssl_certificate     /srv/app/shared/tls/fullchain.pem;"), "{text}");
        assert!(text.contains("ssl_protocols       TLSv1.2 TLSv1.3;"), "{text}");
        assert!(text.contains("Strict-Transport-Security"), "and HSTS: {text}");
    }

    #[test]
    fn a_certificate_is_issued_against_the_webroot_nothing_else() {
        let cfg = tls();
        let cmd = cfg.certbot_command(Some("ops@example.com"), Some("deploy"));
        assert!(cmd.starts_with("sudo -n certbot certonly --webroot"), "{cmd}");
        assert!(cmd.contains("-w '/srv/app/shared/acme'"), "{cmd}");
        assert!(cmd.contains("--email 'ops@example.com'"), "{cmd}");
        assert!(cmd.contains("-d 'app.example.com'"), "{cmd}");
        assert!(cmd.contains("-d 'www.app.example.com'"), "every name, not just the first: {cmd}");
        assert!(cmd.contains("--non-interactive"), "and nothing waits for a keypress: {cmd}");
        // The nginx reload is not the certbot job's.
        assert!(!cmd.contains("reload"), "{cmd}");
    }

    #[test]
    fn installing_tests_the_configuration_before_reloading() {
        let cfg = tls();
        let cmd = cfg.install_command(Some("deploy"));
        assert!(cmd.contains("cat > '/etc/nginx/sites-available/app.conf'"), "{cmd}");
        assert!(cmd.contains("ln -sfn '/etc/nginx/sites-available/app.conf' '/etc/nginx/sites-enabled/app.conf'"), "{cmd}");
        let test = cmd.find("nginx -t").expect("a test");
        let reload = cmd.find("reload nginx").expect("a reload");
        assert!(test < reload, "a reload with a bad file takes the good sites down too: {cmd}");
    }

    #[test]
    fn the_generated_file_passes_nginx() {
        // `nginx -t` is the only authority that matters, so where nginx is
        // installed, the file is tested by nginx.
        let Some(nginx) = crate::nginx::which("nginx") else {
            eprintln!("nginx: not installed, skipping the real check");
            return;
        };
        let dir = scratch("verify");
        // A certificate has to exist for nginx to read it; a throwaway one is
        // enough to prove the block parses.
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let made = std::process::Command::new("openssl")
            .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout"])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .args(["-days", "1", "-subj", "/CN=app.example.com"])
            .output();
        if !made.as_ref().map(|o| o.status.success()).unwrap_or(false) {
            eprintln!("openssl: no throwaway certificate, skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        for cfg in [
            NginxConfig::new("app", &layout(), 3000).with_domains(&["app.example.com".into()]),
            tls(),
        ] {
            let mut cfg = cfg;
            cfg.cert = cert.display().to_string();
            cfg.cert_key = key.display().to_string();
            let path = dir.join("nginx.conf");
            std::fs::write(&path, cfg.test_wrapper(&dir)).expect("the wrapper");
            let out = std::process::Command::new(&nginx)
                .arg("-t")
                .arg("-c")
                .arg(&path)
                .arg("-p")
                .arg(&dir)
                .output()
                .expect("nginx runs");
            let report = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(out.status.success(), "nginx rejected the block:\n{report}\n{}", cfg.render());
            assert!(report.contains("syntax is ok"), "{report}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_structural_check_catches_what_a_template_can_get_wrong() {
        assert!(validate(&NginxConfig::new("app", &layout(), 3000).render()).is_empty(), "the real thing is clean");
        assert!(validate("server {\n  listen 80;\n").iter().any(|p| p.contains("left open")), "an unclosed block");
        assert!(validate("server_name x;\n}").iter().any(|p| p.contains("no block open")), "a stray brace");
        assert!(validate("server {\n listen 80\n}\n").iter().any(|p| p.contains("ends in neither")), "a missing semicolon");
        assert!(validate("upstream a {}\n").iter().any(|p| p.contains("no server_name")), "no server_name");
        assert!(validate("server {\n server_name x;\n\tlisten 80;\n}\n").iter().any(|p| p.contains("tab")), "a tab");
    }

}
