use crate::token::Span;

#[derive(Debug, Clone)]
pub struct Program {
    pub stmts: Vec<Stmt>,
    pub path: String,
}

#[derive(Debug, Clone)]
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
    ];

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
            _ => {
                let _ = span;
                None
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct FieldDef {
    pub name: String,
    pub ty: String,
    pub attrs: Vec<String>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ModelDef {
    pub name: String,
    pub table: String,
    pub fields: Vec<FieldDef>,
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
            Test(t) => t.span,
            Var(v) | Const(v) => v.span,
            If { span: s, .. } | Loop { span: s, .. } | Return(_, s) | Race(_, s)
            | Expect { span: s, .. } => *s,
            ExprStmt(e) => e.span(),
        }
    }
}