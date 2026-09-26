//! Canonical source formatter. Re-emits a program with consistent
//! indentation and spacing from the AST.

use crate::ast::*;

pub fn format(prog: &Program) -> String {
    let mut f = Fmt::default();
    // Job names are needed to decide whether a desugared `queue.enqueue` can be
    // printed back as `queue Name(..)`, so collect them before printing.
    f.jobs = prog
        .stmts
        .iter()
        .filter_map(|s| match s {
            Stmt::Job(j) => Some(j.name.clone()),
            _ => None,
        })
        .collect();
    // `every 1h { .. }` and `worker X { .. }` keep their bodies in paired
    // functions; the formatter prints the body at the declaration and drops the
    // pair, so the bodies are indexed here rather than printed on their own.
    f.funcs = prog
        .stmts
        .iter()
        .filter_map(|s| match s {
            Stmt::Func(fd) => Some((fd.name.clone(), fd.body.clone())),
            _ => None,
        })
        .collect();
    for st in &prog.stmts {
        f.stmt(st, 0);
        f.out.push('\n');
    }
    f.out
}

/// Canonical rendering of one field attribute in the legacy blueprint form.
/// The blueprint grammar takes `#name` only, so an argument is dropped.
fn field_attr(a: &FieldAttr) -> String {
    let _ = &a.arg;
    format!("#{}", a.name)
}

/// Canonical rendering of one field attribute in the v0.6 framework form:
/// `@name` or `@name(arg)`.
fn field_attr_at(a: &FieldAttr) -> String {
    match &a.arg {
        Some(e) => format!("@{}({})", a.name, e.render()),
        None => format!("@{}", a.name),
    }
}

/// A schedule's wall time, quoted because `at` takes a string, with the seconds
/// left off when they are zero: `at "03:00"` is what people write.
fn fmt_time(h: u8, m: u8, s: u8) -> String {
    if s == 0 {
        format!("\"{h:02}:{m:02}\"")
    } else {
        format!("\"{h:02}:{m:02}:{s:02}\"")
    }
}

/// Canonical rendering of a schedule's timezone: omitted for the default.
fn fmt_tz(tz: &Option<String>) -> String {
    match tz {
        Some(t) => format!(" timezone {t:?}"),
        None => String::new(),
    }
}

/// Weekday names for schedule rendering, Monday-first like the parser.
const WEEKDAYS: [&str; 7] =
    ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];

/// The units `format` picks for a duration, largest first: `10m` rather than
/// `600s`, matching how the declaration reads best.
const DURATION_UNITS: [(&str, i64); 4] = [("d", 86400), ("h", 3600), ("m", 60), ("s", 1)];

/// Canonical rendering of a duration in seconds as a single `Nunit` token.
/// Returns None for a value no unit divides, so a hand-written
/// `cache.declare("x", 7)` stays a call instead of becoming a lie.
fn fmt_duration(secs: i64) -> Option<String> {
    if secs <= 0 {
        return None;
    }
    for (unit, size) in DURATION_UNITS {
        if secs % size == 0 {
            return Some(format!("{}{}", secs / size, unit));
        }
    }
    None
}

/// The window name a rate limit's millisecond window prints as, or None when
/// it is not a whole second (only then is `limit` unrepresentable).
fn fmt_window(ms: i64) -> Option<&'static str> {
    if ms <= 0 {
        return None;
    }
    [("day", 86_400_000), ("hour", 3_600_000), ("minute", 60_000), ("second", 1000)]
        .into_iter()
        .find(|(_, size)| ms % size == 0)
        .map(|(name, _)| name)
}

/// Canonical rendering of one framework field: `Type`, `Type(args)` and the
/// trailing `@attr` / `@attr(arg)` list.
fn field_decl(f: &FieldDef) -> String {
    let mut s = f.ty.clone();
    // Presence rides on the type as `!` / `?`, before the constraint list, so
    // `nick : Str?(length=2..8)` does not widen into `nick : Str(length=2..8)?`.
    for a in &f.attrs {
        if a.arg.is_none() {
            match a.name.as_str() {
                "required" => s.push('!'),
                "nullable" | "optional" => s.push('?'),
                _ => {}
            }
        }
    }
    if !f.args.is_empty() {
        let inner = f
            .args
            .iter()
            .map(|a| match a {
                FieldArg::Positional(e) => e.render(),
                FieldArg::Constraint(k, v) => format!("{k}={}", v.render()),
            })
            .collect::<Vec<_>>()
            .join(", ");
        s.push_str(&format!("({inner})"));
    }
    for a in &f.attrs {
        if a.arg.is_none() && matches!(a.name.as_str(), "required" | "nullable" | "optional") {
            continue; // already rendered as a presence marker
        }
        s.push(' ');
        s.push_str(&field_attr_at(a));
    }
    s
}

#[derive(Default)]
struct Fmt {
    out: String,
    jobs: Vec<String>,
    funcs: std::collections::HashMap<String, Vec<Stmt>>,
}

/// Function names the parser invents for a paired body. They are never printed
/// as functions: the declaration owns them.
fn is_paired_body(name: &str) -> bool {
    name.starts_with("__sched_") || name.starts_with("__worker_")
}

/// A worker's body without the `x <- __payload.x` prelude the parser prepends:
/// those bindings come back from the `job` declaration, and printing them would
/// shadow the parameter names with themselves.
fn worker_body(body: &[Stmt]) -> Vec<&Stmt> {
    let mut rest = body;
    while let Some(Stmt::Var(v)) = rest.first() {
        let from_payload = matches!(&v.value, Expr::Member(b, _, _)
            if matches!(b.as_ref(), Expr::Ident(n, _) if n == "__payload"));
        if !from_payload {
            break;
        }
        rest = &rest[1..];
    }
    rest.iter().collect()
}

impl Fmt {
    fn line(&mut self, depth: usize, s: &str) {
        for _ in 0..depth {
            self.out.push_str("    ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn stmt(&mut self, st: &Stmt, d: usize) {
        match st {
            Stmt::Bring(m, _) => self.line(d, &format!("bring {}", self.module(m))),
            Stmt::Import { path, .. } => self.line(d, &format!("bring {path:?}")),
            Stmt::App(p, _) => self.line(d, &format!("app @{p}")),
            Stmt::Model(m) => {
                if m.brace {
                    let head = if m.table == m.name.to_lowercase() {
                        format!("model {}{} {{", m.name, if m.strict { " @strict" } else { "" })
                    } else {
                        format!(
                            "model {} = {}{} {{",
                            m.name,
                            m.table,
                            if m.strict { " @strict" } else { "" }
                        )
                    };
                    self.line(d, &head);
                    for f in &m.fields {
                        self.line(d + 1, &format!("{} : {}", f.name, field_decl(f)));
                    }
                    self.line(d, "}");
                } else {
                    self.line(d, &format!("model {} = {} [", m.name, m.table));
                    for f in &m.fields {
                        let attrs: String = if f.attrs.is_empty() {
                            String::new()
                        } else {
                            format!(
                                " {}",
                                f.attrs.iter().map(field_attr).collect::<Vec<_>>().join(" ")
                            )
                        };
                        self.line(d + 1, &format!("{} => {}{attrs},", f.name, f.ty));
                    }
                    self.line(d, "]");
                }
            }
            Stmt::Route(r) => {
                let params: Vec<String> = r
                    .params
                    .iter()
                    .map(|p| match &p.ty {
                        Some(t) => format!("{} = {t}", p.name),
                        None => p.name.clone(),
                    })
                    .collect();
                let ps = if params.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", params.join(", "))
                };
                self.line(d, &format!("{} {:?} ::{ps} {{", r.method, r.path));
                for s in &r.body {
                    self.stmt(s, d + 1);
                }
                self.line(d, "}");
            }
            Stmt::Socket(s) => {
                self.line(d, &format!("socket {:?} {{", s.path));
                if let Some(b) = &s.connect {
                    self.line(d + 1, "connect :: {");
                    for x in b {
                        self.stmt(x, d + 2);
                    }
                    self.line(d + 1, "}");
                }
                if let Some(b) = &s.message {
                    self.line(d + 1, "message(d) :: {");
                    for x in b {
                        self.stmt(x, d + 2);
                    }
                    self.line(d + 1, "}");
                }
                if let Some(b) = &s.disconnect {
                    self.line(d + 1, "disconnect :: {");
                    for x in b {
                        self.stmt(x, d + 2);
                    }
                    self.line(d + 1, "}");
                }
                self.line(d, "}");
            }
            Stmt::Func(f) => {
                if let Some(job) = f.name.strip_prefix("__worker_") {
                    // `worker Name { .. }`: the job declaration already carries
                    // the parameter names, so the payload bindings the parser
                    // prepended are dropped rather than printed as shadowing
                    // locals.
                    self.line(d, &format!("worker {job} {{"));
                    for s in worker_body(&f.body) {
                        self.stmt(s, d + 1);
                    }
                    self.line(d, "}");
                    return;
                }
                if is_paired_body(&f.name) {
                    return; // printed with its `every ..` declaration
                }
                let params: Vec<String> = f
                    .params
                    .iter()
                    .map(|p| match &p.ty {
                        Some(t) => format!("{t} {}", p.name),
                        None => p.name.clone(),
                    })
                    .collect();
                let ret = f
                    .ret
                    .as_ref()
                    .map(|t| format!(" => {t}"))
                    .unwrap_or_default();
                self.line(d, &format!("calc {}({}){} {{", f.name, params.join(", "), ret));
                for s in &f.body {
                    self.stmt(s, d + 1);
                }
                self.line(d, "}");
            }
            Stmt::Middleware { name, body, .. } => {
                self.line(d, &format!("before {name} :: {{"));
                for s in body {
                    self.stmt(s, d + 1);
                }
                self.line(d, "}");
            }
            Stmt::Job(j) => {
                let params: Vec<String> =
                    j.params.iter().map(|p| format!("{} {}", p.ty, p.name)).collect();
                self.line(d, &format!("job {}({})", j.name, params.join(", ")));
            }
            Stmt::Schedule(s) => {
                // The body lives in the paired `__sched_N` function; print it
                // here, where the user wrote it, instead of as a stray `calc`.
                let head = match &s.kind {
                    SchedKind::Interval { secs } => {
                        format!("every {}", fmt_duration(*secs).unwrap_or_else(|| format!("{secs}s")))
                    }
                    SchedKind::Daily { h, m, s: sec, tz } => {
                        format!("every day at {}{}", fmt_time(*h, *m, *sec), fmt_tz(tz))
                    }
                    SchedKind::Weekly { weekday, h, m, s: sec, tz } => {
                        format!(
                            "every {} at {}{}",
                            WEEKDAYS[*weekday as usize],
                            fmt_time(*h, *m, *sec),
                            fmt_tz(tz)
                        )
                    }
                    SchedKind::Startup => "every startup".to_string(),
                };
                match self.funcs.get(&s.name).cloned() {
                    Some(body) => {
                        self.line(d, &format!("{head} {{"));
                        for st in &body {
                            self.stmt(st, d + 1);
                        }
                        self.line(d, "}");
                    }
                    // A schedule whose body is gone still prints its spec: half
                    // a declaration beats none, and the build will say why.
                    None => self.line(d, &head),
                }
            }
            Stmt::Protect(p) => {
                let mut opts = vec![format!("secret = {}", p.secret.render())];
                if !p.except.is_empty() {
                    let paths = p
                        .except
                        .iter()
                        .map(|e| format!("{e:?}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    opts.push(format!("except = [{paths}]"));
                }
                self.line(d, &format!("protect {}({})", p.scheme, opts.join(", ")));
            }
            Stmt::Test(t) => {
                self.line(d, &format!("test \"{}\" {{", t.name));
                for s in &t.body {
                    self.stmt(s, d + 1);
                }
                self.line(d, "}");
            }
            Stmt::Var(v) => {
                self.line(d, &format!("{} <- {}", v.name, self.expr(&v.value, 0)));
            }
            Stmt::Const(v) => {
                self.line(d, &format!("{} ::= {}", v.name, self.expr(&v.value, 0)));
            }
            Stmt::If { cond, then_body, else_body, .. } => {
                self.line(d, &format!("?({}) {{", self.expr(cond, 0)));
                for s in then_body {
                    self.stmt(s, d + 1);
                }
                if else_body.is_empty() {
                    self.line(d, "}");
                } else {
                    self.line(d, "} :{");
                    for s in else_body {
                        self.stmt(s, d + 1);
                    }
                    self.line(d, "}");
                }
            }
            Stmt::Loop { var, iter, body, .. } => {
                self.line(d, &format!("loop {var} => {} {{", self.expr(iter, 0)));
                for s in body {
                    self.stmt(s, d + 1);
                }
                self.line(d, "}");
            }
            Stmt::Return(e, _) => self.line(d, &format!("<- {}", self.expr(e, 0))),
            Stmt::Race(es, _) => {
                let items: Vec<String> = es.iter().map(|e| self.expr(e, 0)).collect();
                self.line(d, &format!("race [{}]", items.join(", ")));
            }
            Stmt::Expect { lhs, op, rhs, .. } => {
                self.line(
                    d,
                    &format!(
                        "expect {} {} {}",
                        self.expr(lhs, 0),
                        op.symbol(),
                        self.expr(rhs, 0)
                    ),
                );
            }
            Stmt::ExprStmt(Expr::Transaction { body, .. }) => {
                self.line(d, "db.transaction {");
                for s in body {
                    self.stmt(s, d + 1);
                }
                self.line(d, "}");
            }
            Stmt::ExprStmt(e) => self.line(d, &format!("{}", self.expr(e, 0))),
        }
    }

    /// The declarations of M6.1-M6.6 are parsed into ordinary calls, so the
    /// formatter would print their machinery (`cache.declare("users", 600)`)
    /// instead of the language the user wrote. Each form is recognised by its
    /// exact shape and re-printed as its declaration; anything that does not
    /// match exactly keeps the plain call, so no value is ever lost or invented.
    fn declaration_form(&self, callee: &Expr, args: &[Expr]) -> Option<String> {
        let Expr::Member(base, name, _) = callee else { return None };
        let Expr::Ident(module, _) = base.as_ref() else { return None };
        match (module.as_str(), name.as_str()) {
            // `cache users ttl 10m`
            ("cache", "declare") if args.len() == 2 => {
                let Expr::Str(entry, _) = &args[0] else { return None };
                let Expr::Int(secs, _) = &args[1] else { return None };
                Some(format!("cache {entry} ttl {}", fmt_duration(*secs)?))
            }
            // `queue SendEmail(user, delay = 60)`. Only for declared jobs:
            // the declaration form would not re-parse otherwise.
            ("queue", "enqueue")
                if args.len() == 3
                    && matches!(&args[1], Expr::List(_, _))
                    && matches!(&args[2], Expr::Obj(_, _)) =>
            {
                let Expr::Str(job, _) = &args[0] else { return None };
                if !self.jobs.iter().any(|j| j == job) {
                    return None;
                }
                let Expr::List(payload, _) = &args[1] else { return None };
                let Expr::Obj(opts, _) = &args[2] else { return None };
                if !opts.iter().all(|(k, _)| matches!(k.as_str(), "delay" | "priority" | "max_attempts"))
                {
                    return None;
                }
                let mut parts: Vec<String> =
                    payload.iter().map(|a| self.expr(a, 0)).collect();
                for (k, v) in opts {
                    parts.push(format!("{k} = {}", self.expr(v, 0)));
                }
                Some(format!("queue {job}({})", parts.join(", ")))
            }
            // `limit 100 requests / minute [sliding] [key expr]`
            ("ratelimit", "check") if args.len() == 4 || args.len() == 5 => {
                let Expr::Str(id, _) = &args[0] else { return None };
                if !id.starts_with("__limit_") {
                    return None;
                }
                let Expr::Int(algo, _) = &args[1] else { return None };
                let Expr::Int(rate, _) = &args[2] else { return None };
                let Expr::Int(per, _) = &args[3] else { return None };
                let mut out = format!("limit {rate} requests / {}", fmt_window(*per)?);
                if *algo == 1 {
                    out.push_str(" sliding");
                } else if *algo != 0 {
                    return None;
                }
                if let Some(k) = args.get(4) {
                    out.push_str(&format!(" key {}", self.expr(k, 0)));
                }
                Some(out)
            }
            // `email.send(to = "a@b.c", subject = "Hi")`
            ("email", "send") if args.len() == 1 => {
                let Expr::Obj(opts, _) = &args[0] else { return None };
                const EMAIL_OPTS: [&str; 7] =
                    ["to", "from", "subject", "body", "template", "data", "async"];
                if !opts.iter().all(|(k, _)| EMAIL_OPTS.contains(&k.as_str())) {
                    return None;
                }
                let parts: Vec<String> =
                    opts.iter().map(|(k, v)| format!("{k} = {}", self.expr(v, 0))).collect();
                Some(format!("email.send({})", parts.join(", ")))
            }
            _ => None,
        }
    }

    fn expr(&self, e: &Expr, _d: usize) -> String {
        match e {
            Expr::Int(n, _) => n.to_string(),
            Expr::Float(f, _) => f.to_string(),
            Expr::Str(s, _) => format!("{:?}", s),
            Expr::Bool(b, _) => b.to_string(),
            Expr::List(items, _) => format!(
                "[{}]",
                items.iter().map(|i| self.expr(i, 0)).collect::<Vec<_>>().join(", ")
            ),
            Expr::Obj(kvs, _) => format!(
                "{{ {} }}",
                kvs.iter()
                    .map(|(k, v)| format!("{k}: {}", self.expr(v, 0)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Expr::Ident(n, _) => n.clone(),
            Expr::Member(b, n, _) => format!("{}.{}", self.expr(b, 0), n),
            Expr::Index(b, i, _) => format!("{}[{}]", self.expr(b, 0), self.expr(i, 0)),
            Expr::Call { callee, args, .. } => {
                if let Some(decl) = self.declaration_form(callee, args) {
                    return decl;
                }
                format!(
                    "{}({})",
                    self.expr(callee, 0),
                    args.iter().map(|a| self.expr(a, 0)).collect::<Vec<_>>().join(", ")
                )
            }
            Expr::Unary(UnOp::Neg, x, _) => format!("-{}", self.expr(x, 0)),
            Expr::Unary(UnOp::Not, x, _) => format!("!{}", self.expr(x, 0)),
            Expr::Binary(op, l, r, _) => {
                format!("{} {} {}", self.expr(l, 0), op.symbol(), self.expr(r, 0))
            }
            Expr::Range(l, h, _) => format!("{}...{}", self.expr(l, 0), self.expr(h, 0)),
            Expr::Match(s, arms, _) => {
                let mut out = format!("pick {} {{ ", self.expr(s, 0));
                for a in arms {
                    let v = a
                        .value
                        .as_ref()
                        .map(|v| self.expr(v, 0))
                        .unwrap_or_else(|| "*".into());
                    out.push_str(&format!("{v} => {}, ", self.expr(&a.body, 0)));
                }
                out.push('}');
                out
            }
            Expr::HttpCall { verb, path, body, .. } => {
                let b = body
                    .as_ref()
                    .map(|b| format!(" {{ {} }}", self.expr(b, 0)))
                    .unwrap_or_default();
                format!("{verb} {:?}{b}", path)
            }
            // A transaction in value position is rejected by typecheck, but
            // the formatter sees raw parses: emit it so formatting never
            // crashes, and in a shape that re-parses.
            Expr::Transaction { body, .. } => {
                let mut inner = Fmt::default();
                for s in body {
                    inner.stmt(s, 1);
                }
                format!("db.transaction {{\n{}}}", inner.out)
            }
        }
    }

    fn module(&self, m: &Module) -> &str {
        m.as_str()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calc_function_roundtrip_is_idempotent() {
        // Regression for the "calc fn" formatter bug: formatting a program
        // with `calc` functions must emit `calc name(...)`, re-parse, and be
        // byte-identical on the second pass.
        let src = "calc add(Int a, Int b) => Int {\n    <- a + b\n}\n\ncalc greet(Str name) => Str {\n    <- \"hi \" + name\n}\n";
        let prog = crate::frontend(src, "test.hard").expect("parses");
        let once = format(&prog);
        let reparsed = crate::frontend(&once, "test.hard").expect("formatted output parses");
        assert!(
            once.contains("calc add("),
            "formatter must keep `calc`, got:\n{once}"
        );
        assert!(
            !once.contains("calc fn"),
            "formatter must not emit stray `fn`, got:\n{once}"
        );
        assert_eq!(
            once,
            format(&reparsed),
            "formatting must be idempotent:\n{once}"
        );
    }

    #[test]
    fn framework_model_survives_roundtrip() {
        // The v0.6 brace form carries type arguments, field attributes, an
        // explicit table name and a model-level `@strict`, all of which have
        // to come back out of the formatter unchanged.
        let src = "model User = people @strict {\n    id : Uuid!\n    email : Email!\n    age : Int(min=18, max=120)\n    role : Enum(\"admin\", \"user\")\n    tags : List(Str)\n    nick : Str?(length=2..8)\n    bio : Str(max=280, nullable) @index\n}\n";
        let once = format(&crate::frontend(src, "test.hard").unwrap());
        assert!(once.contains("model User = people @strict {"), "got:\n{once}");
        for want in [
            "id : Uuid!",
            "email : Email!",
            "age : Int(min=18, max=120)",
            "role : Enum(\"admin\", \"user\")",
            "tags : List(Str)",
            "nick : Str?(length=2..8)",
            "bio : Str(max=280, nullable) @index",
        ] {
            assert!(once.contains(want), "formatter lost `{want}`, got:\n{once}");
        }
        let twice = format(&crate::frontend(&once, "test.hard").unwrap());
        assert_eq!(once, twice, "formatting must be idempotent:\n{once}");
    }

    #[test]
    fn route_and_handler_survive_roundtrip() {
        let src = "bring http\n\napp @3033\n\nGET \"/\" :: {\n    <- { hello: \"world\" }\n}\n";
        let once = format(&crate::frontend(src, "test.hard").unwrap());
        assert!(once.contains("GET \"/\" :: {"), "got:\n{once}");
        let twice = format(&crate::frontend(&once, "test.hard").unwrap());
        assert_eq!(once, twice);
    }

    /// The M6 declarations are parsed into ordinary calls. Formatting must put
    /// the language back, not the machinery: this is the shape a user reads.
    #[test]
    fn m6_declarations_survive_roundtrip() {
        let src = "cache users ttl 10m\n\njob Send(Text u)\n\nqueue Send(u, delay = 60)\n\nlimit 100 requests / minute\n\nGET \"/api\" :: {\n    limit 10 requests / second sliding key \"api\"\n    email.send(to = \"a@b.co\", subject = \"Hi\", body = \"there\", async = true)\n    <- \"ok\"\n}\n";
        let once = format(&crate::frontend(src, "test.hard").unwrap());
        for want in [
            "cache users ttl 10m",
            "queue Send(u, delay = 60)",
            "limit 100 requests / minute",
            "limit 10 requests / second sliding key \"api\"",
            "email.send(to = \"a@b.co\", subject = \"Hi\", body = \"there\", async = true)",
        ] {
            assert!(once.contains(want), "expected {want:?} in:\n{once}");
        }
        assert!(!once.contains("cache.declare"), "no machinery leaks out:\n{once}");
        assert!(!once.contains("queue.enqueue"), "no machinery leaks out:\n{once}");
        assert!(!once.contains("ratelimit.check"), "no machinery leaks out:\n{once}");
        let twice = format(&crate::frontend(&once, "test.hard").unwrap());
        assert_eq!(once, twice, "formatting must be idempotent:\n{once}");
    }

    #[test]
    fn a_hand_written_call_the_declaration_cannot_express_stays_a_call() {
        // Each of these would lose meaning if reformatted: a window that is not
        // whole seconds, an undeclared job, a duration no unit divides.
        let src = "ratelimit.check(\"manual\", 0, 5, 45000)\n\nqueue.enqueue(\"Ghost\", [1], { delay: 5 })\n\ncache.declare(\"x\", 0)\n";
        let once = format(&crate::frontend(src, "test.hard").unwrap());
        assert!(once.contains("ratelimit.check(\"manual\", 0, 5, 45000)"), "got:\n{once}");
        assert!(once.contains("queue.enqueue(\"Ghost\", [1], { delay: 5 })"), "got:\n{once}");
        assert!(once.contains("cache.declare(\"x\", 0)"), "got:\n{once}");
        let twice = format(&crate::frontend(&once, "test.hard").unwrap());
        assert_eq!(once, twice);
    }

    #[test]
    fn durations_pick_the_readable_unit() {
        assert_eq!(fmt_duration(600), Some("10m".to_string()));
        assert_eq!(fmt_duration(30), Some("30s".to_string()));
        assert_eq!(fmt_duration(7200), Some("2h".to_string()));
        assert_eq!(fmt_duration(604800), Some("7d".to_string()));
        assert_eq!(fmt_duration(0), None, "no unit for zero");
        assert_eq!(fmt_duration(-5), None, "no unit for a negative");
        assert_eq!(fmt_window(60000), Some("minute"));
        assert_eq!(fmt_window(86_400_000), Some("day"));
        assert_eq!(fmt_window(1500), None, "half a second is not a window");
    }

    #[test]
    fn a_transaction_block_survives_roundtrip() {
        let src = "GET \"/\" :: {\n    db.transaction {\n        db.savepoint(\"s\")\n        db.rollback_to(\"s\")\n    }\n    <- { ok: true }\n}\n";
        let once = format(&crate::frontend(src, "test.hard").unwrap());
        assert!(once.contains("db.transaction {"), "got:\n{once}");
        assert!(once.contains("db.savepoint(\"s\")"), "got:\n{once}");
        let twice = format(&crate::frontend(&once, "test.hard").unwrap());
        assert_eq!(once, twice, "formatting must be idempotent:\n{once}");
    }
}
