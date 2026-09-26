use crate::token::Span;

#[derive(Debug, Clone)]
pub struct Program {
    pub stmts: Vec<Stmt>,
    pub path: String,
    /// `model` declarations, lifted out of `stmts` by the parser. The ORM
    /// schema engine, the migration commands and codegen all need the model
    /// *metadata* rather than the statements, and a model is not a runtime
    /// statement, so it travels beside the program instead of inside it.
    pub models: Vec<ModelDef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Module {
    Http,
    Postgres,
    WebSocket,
    Crypto,
    Json,
    Fs,
    Jwt,
    Env,
    Runtime,
    Time,
    // ---- v0.6 backend framework modules ----
    /// Validation engine (M5.1).
    Validation,
    /// Authentication guards: JWT, refresh, cookie, bearer (M5.2).
    Auth,
    /// ORM / model engine over SQLite and PostgreSQL (M5.3).
    Db,
    /// Cache engine with memory and Redis adapters (M5.5).
    Cache,
    /// Background job queue (M5.6).
    Queue,
    /// Cron / interval scheduler (M5.7).
    Schedule,
    /// Sliding-window rate limiter (M5.8).
    RateLimit,
}

impl Module {
    /// All builtin module names, in catalog order. Shared by the parser's
    /// "unknown module" diagnostics (did-you-mean candidates) and the catalog.
    pub const NAMES: &[&str] = &[
        "http",
        "postgres",
        "websocket",
        "crypto",
        "json",
        "fs",
        "jwt",
        "env",
        "runtime",
        "time",
        "validation",
        "auth",
        "db",
        "cache",
        "queue",
        "schedule",
        "ratelimit",
    ];

    /// Human-readable module list used in diagnostics.
    pub fn name_list() -> String {
        Module::NAMES.join(", ")
    }

    /// Canonical lowercase module name. The single source of truth shared by
    /// the module graph, formatter, docs generator, HIR and codegen.
    pub fn as_str(&self) -> &'static str {
        match self {
            Module::Http => "http",
            Module::Postgres => "postgres",
            Module::WebSocket => "websocket",
            Module::Crypto => "crypto",
            Module::Json => "json",
            Module::Fs => "fs",
            Module::Jwt => "jwt",
            Module::Env => "env",
            Module::Runtime => "runtime",
            Module::Time => "time",
            Module::Validation => "validation",
            Module::Auth => "auth",
            Module::Db => "db",
            Module::Cache => "cache",
            Module::Queue => "queue",
            Module::Schedule => "schedule",
            Module::RateLimit => "ratelimit",
        }
    }

    /// Ordered builtin module set used to gate builtin lowering. Shared by
    /// the type checker and the code generator so the two can never drift.
    pub const SET: &[Module] = &[
        Module::Http,
        Module::Postgres,
        Module::WebSocket,
        Module::Crypto,
        Module::Json,
        Module::Fs,
        Module::Jwt,
        Module::Env,
        Module::Runtime,
        Module::Time,
        Module::Validation,
        Module::Auth,
        Module::Db,
        Module::Cache,
        Module::Queue,
        Module::Schedule,
        Module::RateLimit,
    ];

    /// The allowed-module set as strings (lowercased names), for validation
    /// and for the "unknown module" candidate list.
    pub const SET_NAMES: &[&str] = Module::NAMES;

    pub fn from_name(s: &str, span: Span) -> Option<Module> {
        match s {
            "http" => Some(Module::Http),
            "postgres" => Some(Module::Postgres),
            "websocket" => Some(Module::WebSocket),
            "crypto" => Some(Module::Crypto),
            "json" => Some(Module::Json),
            "fs" => Some(Module::Fs),
            "jwt" => Some(Module::Jwt),
            "env" => Some(Module::Env),
            "runtime" => Some(Module::Runtime),
            "time" => Some(Module::Time),
            "validation" => Some(Module::Validation),
            "auth" => Some(Module::Auth),
            "db" => Some(Module::Db),
            "cache" => Some(Module::Cache),
            "queue" => Some(Module::Queue),
            "schedule" => Some(Module::Schedule),
            "ratelimit" => Some(Module::RateLimit),
            _ => {
                let _ = span;
                None
            }
        }
    }
}

/// A positional type argument (`List(Int)`) or a named constraint
/// (`min=18`, `length=3..30`).
#[derive(Debug, Clone)]
pub enum FieldArg {
    Positional(Expr),
    Constraint(String, Box<Expr>),
}

/// A field attribute: legacy `#id`, or framework `@primary` / `@default(now())`.
#[derive(Debug, Clone)]
pub struct FieldAttr {
    pub name: String,
    pub arg: Option<Expr>,
}

impl FieldAttr {
    pub fn plain(name: impl Into<String>) -> FieldAttr {
        FieldAttr { name: name.into(), arg: None }
    }
}

#[derive(Debug, Clone)]
pub struct FieldDef {
    pub name: String,
    pub ty: String,
    /// Type arguments and validation constraints (`Int(min=18,max=120)`).
    pub args: Vec<FieldArg>,
    pub attrs: Vec<FieldAttr>,
    pub span: Span,
}

impl FieldDef {
    /// First constraint bound for `key` (e.g. `min` in `Int(min=18)`).
    pub fn constraint(&self, key: &str) -> Option<&Expr> {
        self.args.iter().find_map(|a| match a {
            FieldArg::Constraint(k, v) if k == key => Some(v.as_ref()),
            _ => None,
        })
    }

    /// Positional type arguments in source order (`List(Int)` -> `[Int]`).
    pub fn positional(&self) -> Vec<&Expr> {
        self.args
            .iter()
            .filter_map(|a| match a {
                FieldArg::Positional(v) => Some(v),
                _ => None,
            })
            .collect()
    }

    pub fn has_attr(&self, name: &str) -> bool {
        self.attrs.iter().any(|a| a.name == name)
    }

    pub fn attr(&self, name: &str) -> Option<&FieldAttr> {
        self.attrs.iter().find(|a| a.name == name)
    }
}

#[derive(Debug, Clone)]
pub struct ModelDef {
    pub name: String,
    pub table: String,
    pub fields: Vec<FieldDef>,
    /// Source spelling: `true` for the v0.6 framework form
    /// (`model User { ... }`), `false` for the v0.1 blueprint form
    /// (`model User = users [ ... ]`). The formatter round-trips the
    /// original spelling so `hard fmt --check` stays stable.
    pub brace: bool,
    /// `model User @strict { ... }` — reject request bodies that carry keys
    /// the model does not declare (typo protection).
    pub strict: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct RouteDef {
    pub method: String,
    pub path: String,
    pub params: Vec<FunParam>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SocketDef {
    pub path: String,
    pub connect: Option<Vec<Stmt>>,
    pub message: Option<Vec<Stmt>>,
    pub disconnect: Option<Vec<Stmt>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FunParam {
    pub ty: Option<String>,
    pub name: String,
    pub is_body: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FunDef {
    pub name: String,
    pub ret: Option<String>,
    pub params: Vec<FunParam>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TestDef {
    pub name: String,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn symbol(&self) -> &'static str {
        use BinOp::*;
        match self {
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            Mod => "%",
            Eq => "==",
            Ne => "!=",
            Lt => "<",
            Le => "<=",
            Gt => ">",
            Ge => ">=",
            And => "&&",
            Or => "||",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub value: Option<Expr>, // None => wildcard `*`
    pub body: Expr,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Int(i64, Span),
    Float(f64, Span),
    Str(String, Span),
    Bool(bool, Span),
    List(Vec<Expr>, Span),
    Obj(Vec<(String, Expr)>, Span),
    Ident(String, Span),
    /// Field access with dotted generic-string accessor used for module/builtin
    /// dispatch is represented as plain Member.
    Member(Box<Expr>, String, Span),
    Index(Box<Expr>, Box<Expr>, Span),
    Call { callee: Box<Expr>, args: Vec<Expr>, span: Span },
    Unary(UnOp, Box<Expr>, Span),
    Binary(BinOp, Box<Expr>, Box<Expr>, Span),
    Range(Box<Expr>, Box<Expr>, Span),
    /// Entity-call used in tests: POST "/users" { body }
    HttpCall { verb: String, path: String, body: Option<Box<Expr>>, span: Span },
    Match(Box<Expr>, Vec<MatchArm>, Span),
}

impl Expr {
    pub fn span(&self) -> Span {
        use Expr::*;
        match self {
            Int(_, s) | Float(_, s) | Str(_, s) | Bool(_, s) | List(_, s) | Obj(_, s)
            | Ident(_, s) | Member(_, _, s) | Index(_, _, s) | Unary(_, _, s)
            | Binary(_, _, _, s) | Range(_, _, s) | Call { span: s, .. } | Match(_, _, s)
            | HttpCall { span: s, .. } => *s,
        }
    }

    /// Source-shaped rendering of an expression, used by the formatter, the
    /// docs generator and `expect` diagnostics.
    pub fn render(&self) -> String {
        match self {
            Expr::Int(n, _) => n.to_string(),
            Expr::Float(f, _) => f.to_string(),
            Expr::Str(s, _) => format!("\"{s}\""),
            Expr::Bool(b, _) => b.to_string(),
            Expr::List(items, _) => {
                let inner = items.iter().map(|i| i.render()).collect::<Vec<_>>().join(", ");
                format!("[{inner}]")
            }
            Expr::Obj(kvs, _) => {
                let inner =
                    kvs.iter().map(|(k, v)| format!("{k}: {}", v.render())).collect::<Vec<_>>().join(", ");
                format!("{{ {inner} }}")
            }
            Expr::Ident(n, _) => n.clone(),
            Expr::Member(b, n, _) => format!("{}.{}", b.render(), n),
            Expr::Index(b, i, _) => format!("{}[{}]", b.render(), i.render()),
            Expr::Call { callee, args, .. } => {
                let inner = args.iter().map(|a| a.render()).collect::<Vec<_>>().join(", ");
                format!("{}({inner})", callee.render())
            }
            Expr::Unary(UnOp::Neg, x, _) => format!("-{}", x.render()),
            Expr::Unary(UnOp::Not, x, _) => format!("!{}", x.render()),
            Expr::Binary(op, l, r, _) => {
                format!("{} {} {}", l.render(), op.symbol(), r.render())
            }
            Expr::Range(l, h, _) => format!("{}..{}", l.render(), h.render()),
            Expr::Match(_, _, _) => "pick(..)".to_string(),
            Expr::HttpCall { verb, path, .. } => format!("{verb} \"{path}\""),
        }
    }
}

/// Shared source-shaped expression renderer (see [`Expr::render`]).
pub fn expr_to_src(e: &Expr) -> String {
    e.render()
}

#[derive(Debug, Clone)]
pub struct RouteCallExpr {
    pub verb: String,
    pub path: String,
    pub body_span: Span,
}

#[derive(Debug, Clone)]
pub struct VarDef {
    pub name: String,
    pub ty: Option<String>,
    pub value: Expr,
    pub span: Span,
}

/// A `protect` declaration. `scheme` is the guard to apply (`jwt` today);
/// `secret` is the expression that yields the signing key; `except` lists path
/// prefixes that stay public.
#[derive(Debug, Clone)]
pub struct ProtectDef {
    pub scheme: String,
    pub secret: Expr,
    pub except: Vec<String>,
    pub span: Span,
}

impl ProtectDef {
    /// True when this declaration leaves `path` public.
    ///
    /// An exempt entry covers its own path and everything under it, but the
    /// match is segment-aware: `"/api"` exempts `"/api"` and `"/api/status"`
    /// while `"/ap"` exempts neither, because prefix-matching a partial
    /// segment would quietly expose `"/api"`. Both the emitter and the docs
    /// generator ask this question, so the rule lives here once.
    pub fn exempts(&self, path: &str) -> bool {
        self.except.iter().any(|p| {
            path == p
                || (path.len() > p.len()
                    && path.starts_with(p.as_str())
                    && (p.ends_with('/') || path.as_bytes()[p.len()] == b'/'))
        })
    }
}

#[derive(Debug, Clone)]
pub enum Stmt {
    /// Builtin runtime module (`bring http`, `bring std.crypto`).
    Bring(Module, Span),
    /// Local source module (`bring "./utils"`, `bring "../shared/handlers"`).
    /// Stored without the `.hard` extension (added at resolution time).
    Import { path: String, span: Span },
    App(i64, Span),
    Model(ModelDef),
    Route(RouteDef),
    Socket(SocketDef),
    Func(FunDef),
    Middleware { name: String, body: Vec<Stmt>, span: Span },
    /// `protect jwt(secret = env.get("JWT_SECRET"), except = ["/health"])`
    Protect(ProtectDef),
    Test(TestDef),
    Var(VarDef),
    Const(VarDef),
    If { cond: Expr, then_body: Vec<Stmt>, else_body: Vec<Stmt>, span: Span },
    Loop { var: String, iter: Expr, body: Vec<Stmt>, span: Span },
    Return(Expr, Span),
    Race(Vec<Expr>, Span),
    Expect { lhs: Expr, op: BinOp, rhs: Expr, span: Span },
    ExprStmt(Expr),
}

impl Stmt {
    pub fn span(&self) -> Span {
        use Stmt::*;
        match self {
            Bring(_, s) | Import { span: s, .. } | App(_, s) => *s,
            Model(m) => m.span,
            Route(r) => r.span,
            Socket(s) => s.span,
            Func(f) => f.span,
            Middleware { span: s, .. } => *s,
            Protect(d) => d.span,
            Test(t) => t.span,
            Var(v) | Const(v) => v.span,
            If { span: s, .. } | Loop { span: s, .. } | Return(_, s) | Race(_, s)
            | Expect { span: s, .. } => *s,
            ExprStmt(e) => e.span(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(paths: &[&str]) -> ProtectDef {
        ProtectDef {
            scheme: "jwt".to_string(),
            secret: Expr::Str("k".to_string(), Span::new(1, 1)),
            except: paths.iter().map(|p| p.to_string()).collect(),
            span: Span::new(1, 1),
        }
    }

    #[test]
    fn exempt_paths_match_on_segment_boundaries() {
        let g = guard(&["/api", "/health"]);
        // Exact matches.
        assert!(g.exempts("/api"), "exact");
        assert!(g.exempts("/health"), "exact second entry");
        // Children are covered, which is what makes a prefix useful.
        assert!(g.exempts("/api/status"), "child path");
        assert!(g.exempts("/api/v1/users"), "deep child path");
        // A partial-segment prefix must NOT match: this is the difference
        // between an exempt list and a substring search, and getting it wrong
        // publishes a route the author meant to guard.
        assert!(!g.exempts("/ap"), "shorter than the prefix");
        assert!(!g.exempts("/apix"), "partial segment");
        assert!(!g.exempts("/healthcheck"), "partial segment on the other entry");
        // Unrelated paths stay guarded.
        assert!(!g.exempts("/"), "root");
        assert!(!g.exempts("/users"), "unrelated");
        assert!(!g.exempts(""), "empty path");
        // A trailing slash in the entry is a directory, not a partial segment.
        let dir = guard(&["/api/"]);
        assert!(dir.exempts("/api/"), "exact with slash");
        assert!(dir.exempts("/api/users"), "child with slash");
        assert!(!dir.exempts("/api"), "parent without the slash");
    }

    #[test]
    fn no_exempt_paths_guards_everything() {
        let g = guard(&[]);
        assert!(!g.exempts("/"), "root");
        assert!(!g.exempts("/health"), "anything");
    }
}
