//! High-Level Intermediate Representation (HIR) — ms3.1.
//!
//! `hir` separates the *syntax tree* produced by the parser ([`crate::ast`])
//! from a *semantic tree* the optimizer pipeline can consume:
//!
//! * every node carries a **stable, deterministic `Id`** (`u32`) and every
//!   variable a separate stable [`VarId`], both assigned in a fixed traversal
//!   order so identical sources lower to identical trees;
//! * source [`Span`]s are preserved on every node;
//! * identifiers are **resolved**: each `Ident` expression records the
//!   [`VarId`] of the binding it reads (or `None` for module/function names),
//!   because the same name may denote different variables in nested scopes.
//!
//! The pretty printer ([`HirProgram::render`]) mirrors the lowered tree and is
//! the basis for the snapshot tests in `tests/regression/NNN-hir-*.hard`.

use crate::ast;
use crate::token::Span;

/// Stable id for items, functions, blocks and expressions. One flat counter
/// per compile keeps ids unique and deterministic; the distinct id spaces
/// (fn/block/expr) interleave in source order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub u32);

/// Stable id for a variable binding (param, loop variable, `<-`/`::=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VarId(pub u32);

/// HIR operator sets. Mirrors the AST operators but lives in the semantic
/// tree so the optimizer never touches syntax-shaped nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Program shape counters, used by the optimizer and the compile reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HirStats {
    pub items: usize,
    pub functions: usize,
    pub blocks: usize,
    pub vars: usize,
    pub exprs: usize,
}

/// A lowered program: one deterministic HIR per source file.
#[derive(Debug, Clone)]
pub struct HirProgram {
    pub items: Vec<HirItem>,
    pub stats: HirStats,
}

#[derive(Debug, Clone)]
pub enum HirItem {
    Bring { module: String, span: Span },
    App { port: i64, span: Span },
    Model(HirModel),
    Var { var: HirVar, value: HirExpr, is_const: bool, span: Span },
    Fn(HirFn),
}

#[derive(Debug, Clone)]
pub struct HirModel {
    pub name: String,
    pub table: String,
    pub fields: Vec<HirField>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct HirField {
    pub name: String,
    pub ty: String,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketEvt {
    Connect,
    Message,
    Disconnect,
}

/// Why an [`HirFn`] exists. Functions, routes, middleware, socket events and
/// tests all lower to functions; the kind keeps the source differentiation.
#[derive(Debug, Clone)]
pub enum HirFnKind {
    Func,
    Route { method: String, path: String },
    Middleware { name: String },
    Socket { path: String, event: SocketEvt },
    Test { name: String },
}

#[derive(Debug, Clone)]
pub struct HirFn {
    pub id: Id,
    pub kind: HirFnKind,
    pub name: String,
    pub ret: Option<String>,
    pub params: Vec<HirParam>,
    pub body: HirBlock,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct HirParam {
    pub id: VarId,
    pub name: String,
    pub ty: Option<String>,
    pub is_body: bool,
    pub span: Span,
}

/// A brace-delimited statement sequence. Every block gets a stable [`Id`]
/// its optimizer passes can reference (e.g. for dead-block removal).
#[derive(Debug, Clone)]
pub struct HirBlock {
    pub id: Id,
    pub span: Span,
    pub stmts: Vec<HirStmt>,
}

/// A variable (binding) with its stable [`VarId`] and declaring span.
#[derive(Debug, Clone)]
pub struct HirVar {
    pub id: VarId,
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum HirStmt {
    Var { var: HirVar, value: HirExpr, span: Span },
    Const { var: HirVar, value: HirExpr, span: Span },
    If {
        cond: HirExpr,
        then_b: HirBlock,
        else_b: Option<HirBlock>,
        span: Span,
    },
    Loop { var: HirVar, iter: HirExpr, body: HirBlock, span: Span },
    Return { value: HirExpr, span: Span },
    Race { tasks: Vec<HirExpr>, span: Span },
    Expect { lhs: HirExpr, op: BinOp, rhs: HirExpr, span: Span },
    Expr { expr: HirExpr },
}

#[derive(Debug, Clone)]
pub struct HirExpr {
    pub id: Id,
    pub span: Span,
    pub kind: HirExprKind,
}

#[derive(Debug, Clone)]
pub enum HirExprKind {
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    List(Vec<HirExpr>),
    Obj(Vec<(String, HirExpr)>),
    Ident { name: String, var: Option<VarId> },
    Member { base: Box<HirExpr>, name: String },
    Index { base: Box<HirExpr>, index: Box<HirExpr> },
    Call { callee: Box<HirExpr>, args: Vec<HirExpr> },
    Unary { op: UnOp, operand: Box<HirExpr> },
    Binary { op: BinOp, lhs: Box<HirExpr>, rhs: Box<HirExpr> },
    Range { lo: Box<HirExpr>, hi: Box<HirExpr> },
    HttpCall { verb: String, path: String, body: Option<Box<HirExpr>> },
    Match { scrutinee: Box<HirExpr>, arms: Vec<HirMatchArm> },
    Transaction { body: HirBlock },
}

#[derive(Debug, Clone)]
pub struct HirMatchArm {
    pub value: Option<HirExpr>, // None => wildcard `*`
    pub body: HirExpr,
}

pub fn op_from_unary(op: &ast::UnOp) -> UnOp {
    match op {
        ast::UnOp::Neg => UnOp::Neg,
        ast::UnOp::Not => UnOp::Not,
    }
}

pub fn op_from_binary(op: ast::BinOp) -> BinOp {
    match op {
        ast::BinOp::Add => BinOp::Add,
        ast::BinOp::Sub => BinOp::Sub,
        ast::BinOp::Mul => BinOp::Mul,
        ast::BinOp::Div => BinOp::Div,
        ast::BinOp::Mod => BinOp::Mod,
        ast::BinOp::Eq => BinOp::Eq,
        ast::BinOp::Ne => BinOp::Ne,
        ast::BinOp::Lt => BinOp::Lt,
        ast::BinOp::Le => BinOp::Le,
        ast::BinOp::Gt => BinOp::Gt,
        ast::BinOp::Ge => BinOp::Ge,
        ast::BinOp::And => BinOp::And,
        ast::BinOp::Or => BinOp::Or,
    }
}

pub fn op_to_binary(op: BinOp) -> ast::BinOp {
    match op {
        BinOp::Add => ast::BinOp::Add,
        BinOp::Sub => ast::BinOp::Sub,
        BinOp::Mul => ast::BinOp::Mul,
        BinOp::Div => ast::BinOp::Div,
        BinOp::Mod => ast::BinOp::Mod,
        BinOp::Eq => ast::BinOp::Eq,
        BinOp::Ne => ast::BinOp::Ne,
        BinOp::Lt => ast::BinOp::Lt,
        BinOp::Le => ast::BinOp::Le,
        BinOp::Gt => ast::BinOp::Gt,
        BinOp::Ge => ast::BinOp::Ge,
        BinOp::And => ast::BinOp::And,
        BinOp::Or => ast::BinOp::Or,
    }
}

// ---------------------------------------------------------------------------
// Lowering (ast -> hir)
// ---------------------------------------------------------------------------

struct Lowerer {
    next_id: u32,
    next_var: u32,
    scopes: Vec<Vec<(String, VarId)>>,
}

impl Lowerer {
    fn new() -> Lowerer {
        Lowerer {
            next_id: 0,
            next_var: 0,
            scopes: Vec::new(),
        }
    }

    fn id(&mut self) -> Id {
        let i = Id(self.next_id);
        self.next_id += 1;
        i
    }

    fn var_id(&mut self) -> VarId {
        let v = VarId(self.next_var);
        self.next_var += 1;
        v
    }

    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn declare(&mut self, name: &str) -> VarId {
        let id = self.var_id();
        if let Some(frame) = self.scopes.last_mut() {
            frame.push((name.to_string(), id));
        }
        id
    }

    fn resolve(&self, name: &str) -> Option<VarId> {
        for frame in self.scopes.iter().rev() {
            for (n, id) in frame.iter().rev() {
                if n == name {
                    return Some(*id);
                }
            }
        }
        None
    }
}

/// Lower a parsed [`ast::Program`] into an [`HirProgram`].
///
/// Determinism contract: ids are handed out in a fixed order — global
/// bindings first (in source order), then each item in order; within an item
/// the id counter walks params, body block, then statements/expressions
/// depth-first in source order. Two lowers of the same source produce the
/// exact same tree (enforced by unit tests and the snapshot fixtures).
pub fn lower(prog: &ast::Program) -> HirProgram {
    let mut lw = Lowerer::new();

    // Pass 1: pre-scan top-level Var/Const bindings so later items (routes,
    // functions) can resolve globals declared below them in the file.
    let mut globals: Vec<(String, VarId, Span)> = Vec::new();
    for st in &prog.stmts {
        if let ast::Stmt::Var(v) | ast::Stmt::Const(v) = st {
            let id = lw.var_id();
            globals.push((v.name.clone(), id, v.span));
        }
    }
    lw.push_scope();
    for (name, id, _) in &globals {
        if let Some(frame) = lw.scopes.last_mut() {
            frame.push((name.clone(), *id));
        }
    }

    let mut items: Vec<HirItem> = Vec::new();
    let mut stats = HirStats::default();
    let mut gi = 0usize; // consumes globals in source order

    for st in &prog.stmts {
        match st {
            ast::Stmt::Bring(m, span) => {
                items.push(HirItem::Bring {
                    module: module_name(m),
                    span: *span,
                });
                stats.items += 1;
            }
            // Local imports are graph/cache-level; they contribute no items.
            ast::Stmt::Import { path, span } => {
                items.push(HirItem::Bring {
                    module: path.clone(),
                    span: *span,
                });
                stats.items += 1;
            }
            ast::Stmt::App(port, span) => {
                items.push(HirItem::App { port: *port, span: *span });
                stats.items += 1;
            }
            ast::Stmt::Model(m) => {
                let fields = m
                    .fields
                    .iter()
                    .map(|f| HirField {
                        name: f.name.clone(),
                        ty: f.ty.clone(),
                        span: f.span,
                    })
                    .collect::<Vec<_>>();
                items.push(HirItem::Model(HirModel {
                    name: m.name.clone(),
                    table: m.table.clone(),
                    fields,
                    span: m.span,
                }));
                stats.items += 1;
            }
            ast::Stmt::Var(v) | ast::Stmt::Const(v) => {
                let (name, id, span) = &globals[gi];
                gi += 1;
                let var = HirVar {
                    id: *id,
                    name: name.clone(),
                    span: *span,
                };
                let value = lw.expr(&v.value);
                stats.vars += 1;
                stats.exprs += 1;
                items.push(HirItem::Var {
                    var,
                    value,
                    is_const: matches!(st, ast::Stmt::Const(_)),
                    span: v.span,
                });
            }
            ast::Stmt::Route(r) => {
                let f = lw.route_fn(r);
                stats.functions += 1;
                stats.blocks += 1;
                items.push(HirItem::Fn(f));
                stats.items += 1;
            }
            ast::Stmt::Socket(s) => {
                for (event, body_opt) in [
                    (SocketEvt::Connect, &s.connect),
                    (SocketEvt::Message, &s.message),
                    (SocketEvt::Disconnect, &s.disconnect),
                ] {
                    if let Some(body) = body_opt {
                        let f = lw.socket_fn(&s.path, event, body);
                        stats.functions += 1;
                        stats.blocks += 1;
                        items.push(HirItem::Fn(f));
                        stats.items += 1;
                    }
                }
            }
            ast::Stmt::Func(f) => {
                let f = lw.func_fn(f);
                stats.functions += 1;
                stats.blocks += 1;
                items.push(HirItem::Fn(f));
                stats.items += 1;
            }
            ast::Stmt::Middleware { name, body, span } => {
                let f = lw.middleware_fn(name, body, *span);
                stats.functions += 1;
                stats.blocks += 1;
                items.push(HirItem::Fn(f));
                stats.items += 1;
            }
            ast::Stmt::Test(t) => {
                let f = lw.test_fn(t);
                stats.functions += 1;
                stats.blocks += 1;
                items.push(HirItem::Fn(f));
                stats.items += 1;
            }
            // The guard is a codegen-time decision: HIR keeps routes as they
            // are and the emitter consults the declaration when it walks them.
            ast::Stmt::Protect(_) => {
                stats.items += 1;
            }
            ast::Stmt::If { .. } | ast::Stmt::Loop { .. } | ast::Stmt::Return(..)
            | ast::Stmt::Race(..) | ast::Stmt::Expect { .. } | ast::Stmt::ExprStmt(..) => {
                // Statement at module scope is not legal today; skip.
            }
        }
    }

    lw.pop_scope();

    // Recompute expression/block/var totals precisely. expr/var/block counts
    // are authoritative from the lowered tree so optimizer passes can report
    // faithful deltas.
    let mut ec = 0usize;
    let mut bc = 0usize;
    let mut vc = 0usize;
    for it in &items {
        if let HirItem::Fn(f) = it {
            bc += 1;
            vc += f.params.len();
            walk_block(&f.body, &mut ec, &mut bc, &mut vc);
        } else if let HirItem::Var { value, .. } = it {
            vc += 1;
            walk_expr_count(value, &mut ec, &mut bc, &mut vc);
        }
    }
    stats.exprs = ec;
    stats.blocks = bc;
    stats.vars = vc;

    HirProgram { items, stats }
}

fn walk_block(b: &HirBlock, ec: &mut usize, bc: &mut usize, vc: &mut usize) {
    for s in &b.stmts {
        match s {
            HirStmt::Var { value, .. } | HirStmt::Const { value, .. } => {
                *vc += 1;
                walk_expr_count(value, ec, bc, vc);
            }
            HirStmt::If { cond, then_b, else_b, .. } => {
                walk_expr_count(cond, ec, bc, vc);
                *bc += 1;
                walk_block(then_b, ec, bc, vc);
                if let Some(e) = else_b {
                    *bc += 1;
                    walk_block(e, ec, bc, vc);
                }
            }
            HirStmt::Loop { iter, body, .. } => {
                *vc += 1;
                walk_expr_count(iter, ec, bc, vc);
                *bc += 1;
                walk_block(body, ec, bc, vc);
            }
            HirStmt::Return { value, .. } => walk_expr_count(value, ec, bc, vc),
            HirStmt::Race { tasks, .. } => {
                for t in tasks {
                    walk_expr_count(t, ec, bc, vc);
                }
            }
            HirStmt::Expect { lhs, rhs, .. } => {
                walk_expr_count(lhs, ec, bc, vc);
                walk_expr_count(rhs, ec, bc, vc);
            }
            HirStmt::Expr { expr } => walk_expr_count(expr, ec, bc, vc),
        }
    }
}

fn walk_expr_count(e: &HirExpr, ec: &mut usize, bc: &mut usize, vc: &mut usize) {
    *ec += 1;
    use HirExprKind::*;
    match &e.kind {
        Int(_) | Float(_) | Str(_) | Bool(_) => {}
        List(items) => {
            for i in items {
                walk_expr_count(i, ec, bc, vc);
            }
        }
        Obj(kvs) => {
            for (_, v) in kvs {
                walk_expr_count(v, ec, bc, vc);
            }
        }
        Ident { .. } => {}
        Member { base, .. } => walk_expr_count(base, ec, bc, vc),
        Index { base, index } => {
            walk_expr_count(base, ec, bc, vc);
            walk_expr_count(index, ec, bc, vc);
        }
        Call { callee, args } => {
            walk_expr_count(callee, ec, bc, vc);
            for a in args {
                walk_expr_count(a, ec, bc, vc);
            }
        }
        Unary { operand, .. } => walk_expr_count(operand, ec, bc, vc),
        Binary { lhs, rhs, .. } => {
            walk_expr_count(lhs, ec, bc, vc);
            walk_expr_count(rhs, ec, bc, vc);
        }
        Range { lo, hi } => {
            walk_expr_count(lo, ec, bc, vc);
            walk_expr_count(hi, ec, bc, vc);
        }
        HttpCall { body, .. } => {
            if let Some(b) = body {
                walk_expr_count(b, ec, bc, vc);
            }
        }
        Match { scrutinee, arms } => {
            walk_expr_count(scrutinee, ec, bc, vc);
            for a in arms {
                if let Some(v) = &a.value {
                    walk_expr_count(v, ec, bc, vc);
                }
                walk_expr_count(&a.body, ec, bc, vc);
            }
        }
        Transaction { body } => {
            *bc += 1;
            walk_block(body, ec, bc, vc);
        }
    }
}

fn module_name(m: &ast::Module) -> String {
    m.as_str().into()
}

impl Lowerer {
    fn params(&mut self, params: &[ast::FunParam]) -> Vec<HirParam> {
        let mut out = Vec::with_capacity(params.len());
        self.push_scope();
        for p in params {
            let id = self.declare(&p.name);
            out.push(HirParam {
                id,
                name: p.name.clone(),
                ty: p.ty.clone(),
                is_body: p.is_body,
                span: p.span,
            });
        }
        out
    }

    fn func_fn(&mut self, f: &ast::FunDef) -> HirFn {
        let id = self.id();
        let params = self.params(&f.params);
        let body = self.block(&f.body, f.span);
        let hir = HirFn {
            id,
            kind: HirFnKind::Func,
            name: f.name.clone(),
            ret: f.ret.clone(),
            params,
            body,
            span: f.span,
        };
        self.pop_scope();
        hir
    }

    fn route_fn(&mut self, r: &ast::RouteDef) -> HirFn {
        let id = self.id();
        let params = self.params(&r.params);
        let body = self.block(&r.body, r.span);
        let hir = HirFn {
            id,
            kind: HirFnKind::Route {
                method: r.method.clone(),
                path: r.path.clone(),
            },
            name: format!("{} {}", r.method, r.path),
            ret: None,
            params,
            body,
            span: r.span,
        };
        self.pop_scope();
        hir
    }

    fn middleware_fn(&mut self, name: &str, body: &[ast::Stmt], span: Span) -> HirFn {
        let id = self.id();
        self.push_scope();
        let body = self.block(body, span);
        let hir = HirFn {
            id,
            kind: HirFnKind::Middleware { name: name.to_string() },
            name: name.to_string(),
            ret: None,
            params: Vec::new(),
            body,
            span,
        };
        self.pop_scope();
        hir
    }

    fn socket_fn(&mut self, path: &str, event: SocketEvt, body: &[ast::Stmt]) -> HirFn {
        let id = self.id();
        self.push_scope();
        let span = body.first().map(|s| s.span()).unwrap_or(Span::new(0, 0));
        let body = self.block(body, span);
        let ev = match event {
            SocketEvt::Connect => "connect",
            SocketEvt::Message => "message",
            SocketEvt::Disconnect => "disconnect",
        };
        let hir = HirFn {
            id,
            kind: HirFnKind::Socket {
                path: path.to_string(),
                event,
            },
            name: format!("{path} {ev}"),
            ret: None,
            params: Vec::new(),
            body,
            span,
        };
        self.pop_scope();
        hir
    }

    fn test_fn(&mut self, t: &ast::TestDef) -> HirFn {
        let id = self.id();
        self.push_scope();
        let body = self.block(&t.body, t.span);
        let hir = HirFn {
            id,
            kind: HirFnKind::Test { name: t.name.clone() },
            name: t.name.clone(),
            ret: None,
            params: Vec::new(),
            body,
            span: t.span,
        };
        self.pop_scope();
        hir
    }

    fn block(&mut self, stmts: &[ast::Stmt], span: Span) -> HirBlock {
        let id = self.id();
        self.push_scope();
        let mut out = Vec::with_capacity(stmts.len());
        for s in stmts {
            out.push(self.stmt(s));
        }
        let b = HirBlock { id, span, stmts: out };
        self.pop_scope();
        b
    }

    fn stmt(&mut self, s: &ast::Stmt) -> HirStmt {
        match s {
            ast::Stmt::Var(v) => {
                let var = self.bind(&v.name, v.span);
                HirStmt::Var {
                    var,
                    value: self.expr(&v.value),
                    span: v.span,
                }
            }
            ast::Stmt::Const(v) => {
                let var = self.bind(&v.name, v.span);
                HirStmt::Const {
                    var,
                    value: self.expr(&v.value),
                    span: v.span,
                }
            }
            ast::Stmt::If { cond, then_body, else_body, span } => {
                let cond = self.expr(cond);
                let then_b = self.block(then_body, *span);
                let else_b = if else_body.is_empty() {
                    None
                } else {
                    Some(self.block(else_body, *span))
                };
                HirStmt::If {
                    cond,
                    then_b,
                    else_b,
                    span: *span,
                }
            }
            ast::Stmt::Loop { var, iter, body, span } => {
                let var = self.bind(var, *span);
                let iter = self.expr(iter);
                let body = self.block(body, *span);
                HirStmt::Loop {
                    var,
                    iter,
                    body,
                    span: *span,
                }
            }
            ast::Stmt::Return(e, span) => HirStmt::Return {
                value: self.expr(e),
                span: *span,
            },
            ast::Stmt::Race(es, span) => HirStmt::Race {
                tasks: es.iter().map(|e| self.expr(e)).collect(),
                span: *span,
            },
            ast::Stmt::Expect { lhs, op, rhs, span } => HirStmt::Expect {
                lhs: self.expr(lhs),
                op: op_from_binary(*op),
                rhs: self.expr(rhs),
                span: *span,
            },
            ast::Stmt::ExprStmt(e) => HirStmt::Expr { expr: self.expr(e) },
            // Items are handled by the program walker; guards keep exhaustiveness.
            ast::Stmt::Bring(..) | ast::Stmt::Import { .. } | ast::Stmt::App(..)
            | ast::Stmt::Model(..) | ast::Stmt::Route(..) | ast::Stmt::Socket(..)
            | ast::Stmt::Func(..) | ast::Stmt::Middleware { .. } | ast::Stmt::Test(..)
            | ast::Stmt::Protect(..) => {
                unreachable!("item statement reached stmt lowering")
            }
        }
    }

    /// Declare a binding and return its [`HirVar`].
    fn bind(&mut self, name: &str, span: Span) -> HirVar {
        let id = self.declare(name);
        HirVar {
            id,
            name: name.to_string(),
            span,
        }
    }

    fn expr(&mut self, e: &ast::Expr) -> HirExpr {
        let id = self.id();
        let span = e.span();
        let kind = match e {
            ast::Expr::Int(v, _) => HirExprKind::Int(*v),
            ast::Expr::Float(v, _) => HirExprKind::Float(*v),
            ast::Expr::Str(s, _) => HirExprKind::Str(s.clone()),
            ast::Expr::Bool(b, _) => HirExprKind::Bool(*b),
            ast::Expr::List(items, _) => {
                HirExprKind::List(items.iter().map(|it| self.expr(it)).collect())
            }
            ast::Expr::Obj(kvs, _) => HirExprKind::Obj(
                kvs.iter()
                    .map(|(k, v)| (k.clone(), self.expr(v)))
                    .collect(),
            ),
            ast::Expr::Ident(name, _) => HirExprKind::Ident {
                name: name.clone(),
                var: self.resolve(name),
            },
            ast::Expr::Member(base, name, _) => HirExprKind::Member {
                base: Box::new(self.expr(base)),
                name: name.clone(),
            },
            ast::Expr::Index(base, index, _) => HirExprKind::Index {
                base: Box::new(self.expr(base)),
                index: Box::new(self.expr(index)),
            },
            ast::Expr::Call { callee, args, .. } => HirExprKind::Call {
                callee: Box::new(self.expr(callee)),
                args: args.iter().map(|a| self.expr(a)).collect(),
            },
            ast::Expr::Unary(op, operand, _) => HirExprKind::Unary {
                op: op_from_unary(op),
                operand: Box::new(self.expr(operand)),
            },
            ast::Expr::Binary(op, l, r, _) => HirExprKind::Binary {
                op: op_from_binary(*op),
                lhs: Box::new(self.expr(l)),
                rhs: Box::new(self.expr(r)),
            },
            ast::Expr::Range(lo, hi, _) => HirExprKind::Range {
                lo: Box::new(self.expr(lo)),
                hi: Box::new(self.expr(hi)),
            },
            ast::Expr::HttpCall { verb, path, body, .. } => HirExprKind::HttpCall {
                verb: verb.clone(),
                path: path.clone(),
                body: body.as_ref().map(|b| Box::new(self.expr(b))),
            },
            ast::Expr::Match(scrut, arms, _) => HirExprKind::Match {
                scrutinee: Box::new(self.expr(scrut)),
                arms: arms
                    .iter()
                    .map(|a| HirMatchArm {
                        value: a.value.as_ref().map(|v| self.expr(v)),
                        body: self.expr(&a.body),
                    })
                    .collect(),
            },
            ast::Expr::Transaction { body, span } => HirExprKind::Transaction {
                body: self.block(body, *span),
            },
        };
        HirExpr { id, span, kind }
    }
}

// ---------------------------------------------------------------------------
// Pretty printer
// ---------------------------------------------------------------------------

struct Printer {
    out: String,
}

impl Printer {
    fn new() -> Printer {
        Printer { out: String::new() }
    }

    fn line(&mut self, indent: usize, text: &str) {
        for _ in 0..indent {
            self.out.push_str("  ");
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn sp(&self, span: Span) -> String {
        format!("@{}:{}", span.line, span.col)
    }
}

impl HirProgram {
    /// Render the HIR as a deterministic, human-readable tree. This is the
    /// snapshot format for the `hir` regression fixtures.
    pub fn render(&self) -> String {
        let mut p = Printer::new();
        p.line(0, "program");
        let _ = self;
        for it in &self.items {
            match it {
                HirItem::Bring { module, span } => {
                    p.line(1, &format!("bring {module} {}", p.sp(*span)));
                }
                HirItem::App { port, span } => {
                    p.line(1, &format!("app @{port} {}", p.sp(*span)));
                }
                HirItem::Model(m) => {
                    p.line(1, &format!("model {:?} {}", m.name, p.sp(m.span)));
                    for f in &m.fields {
                        p.line(
                            2,
                            &format!("field {:?} {} {}", f.name, f.ty, p.sp(f.span)),
                        );
                    }
                }
                HirItem::Var { var, value, is_const, span } => {
                    let kw = if *is_const { "const" } else { "var" };
                    p.line(
                        1,
                        &format!(                    
                            "{kw} v#{} {:?} = {}",                        
                            var.id.0,
                            var.name,
                            p.sp(*span),
                        ),
                    );
                    print_expr(&mut p, 2, value);
                }
                HirItem::Fn(f) => {
                    print_fn(&mut p, f);
                }
            }
        }
        p.out
    }
}

fn print_fn(p: &mut Printer, f: &HirFn) {
    let (head, name) = match &f.kind {
        HirFnKind::Func => ("fn", &f.name),
        HirFnKind::Route { method, path } => {
            ("route", &format!("{method} {path}"))
        }
        HirFnKind::Middleware { name } => ("middleware", name),
        HirFnKind::Socket { path, event } => {
            let ev = match event {
                SocketEvt::Connect => "connect",
                SocketEvt::Message => "message",
                SocketEvt::Disconnect => "disconnect",
            };
            ("socket", &format!("{path} {ev}"))
        }
        HirFnKind::Test { name } => ("test", name),
    };
    let ret = f
        .ret
        .as_ref()
        .map(|r| format!(" -> {r}"))
        .unwrap_or_default();
    p.line(
        1,
        &format!(
            "{head} #{} {:?}{} {}",
            f.id.0,
            name,
            ret,
            p.sp(f.span)
        ),
    );
    for pa in &f.params {
        let ty = pa
            .ty
            .as_ref()
            .map(|t| format!(" :{t}"))
            .unwrap_or_default();
        let body = if pa.is_body { " [body]" } else { "" };
        p.line(
            2,
            &format!(
                "param v#{} {:?}{}{} {}",
                pa.id.0,
                pa.name,
                ty,
                body,
                p.sp(pa.span)
            ),
        );
    }
    print_block(p, 2, &f.body);
}

fn print_block(p: &mut Printer, indent: usize, b: &HirBlock) {
    p.line(indent, &format!("block #{} {}", b.id.0, p.sp(b.span)));
    for s in &b.stmts {
        print_stmt(p, indent + 1, s);
    }
}

fn print_stmt(p: &mut Printer, indent: usize, s: &HirStmt) {
    match s {
        HirStmt::Var { var, value, span } => {
            p.line(
                indent,
                &format!("var v#{} {:?} = {}", var.id.0, var.name, p.sp(*span)),
            );
            print_expr(p, indent + 1, value);
        }
        HirStmt::Const { var, value, span } => {
            p.line(
                indent,
                &format!("const v#{} {:?} = {}", var.id.0, var.name, p.sp(*span)),
            );
            print_expr(p, indent + 1, value);
        }
        HirStmt::If { cond, then_b, else_b, span } => {
            p.line(indent, &format!("if {}", p.sp(*span)));
            p.line(indent + 1, "cond");
            print_expr(p, indent + 2, cond);
            print_block(p, indent + 1, then_b);
            if let Some(e) = else_b {
                print_block(p, indent + 1, e);
            }
        }
        HirStmt::Loop { var, iter, body, span } => {
            p.line(
                indent,
                &format!(
                    "loop v#{} {:?} over {}",
                    var.id.0,
                    var.name,
                    p.sp(*span)
                ),
            );
            print_expr(p, indent + 1, iter);
            print_block(p, indent + 1, body);
        }
        HirStmt::Return { value, span } => {
            p.line(indent, &format!("return {}", p.sp(*span)));
            print_expr(p, indent + 1, value);
        }
        HirStmt::Race { tasks, span } => {
            p.line(indent, &format!("race {}", p.sp(*span)));
            for t in tasks {
                print_expr(p, indent + 1, t);
            }
        }
        HirStmt::Expect { lhs, op, rhs, span } => {
            p.line(indent, &format!("expect {} {}", p.sp(*span), op.symbol()));
            print_expr(p, indent + 1, lhs);
            print_expr(p, indent + 1, rhs);
        }
        HirStmt::Expr { expr } => {
            p.line(indent, &format!("expr {}", p.sp(expr.span)));
            print_expr(p, indent + 1, expr);
        }
    }
}

fn print_expr(p: &mut Printer, indent: usize, e: &HirExpr) {
    use HirExprKind::*;
    let tag = |p: &Printer, head: String| {
        // head already includes pan; build the line
        format!("{} {}", head, p.sp(e.span))
    };
    match &e.kind {
        Int(v) => {
            p.line(indent, &tag(p, format!("int #{} {v}", e.id.0)));
        }
        Float(v) => {
            p.line(indent, &tag(p, format!("float #{} {v}", e.id.0)));
        }
        Str(s) => {
            p.line(indent, &tag(p, format!("str #{} {:?}", e.id.0, s)));
        }
        Bool(b) => {
            p.line(indent, &tag(p, format!("bool #{} {b}", e.id.0)));
        }
        List(items) => {
            p.line(indent, &tag(p, format!("list #{}", e.id.0)));
            for it in items {
                print_expr(p, indent + 1, it);
            }
        }
        Obj(kvs) => {
            p.line(indent, &tag(p, format!("obj #{}", e.id.0)));
            for (k, v) in kvs {
                p.line(indent + 1, &format!("key {k:?}"));
                print_expr(p, indent + 2, v);
            }
        }
        Ident { name, var } => {
            let v = match var {
                Some(id) => format!("v#{}", id.0),
                None => "?".to_string(),
            };
            p.line(indent, &tag(p, format!("ident #{} {:?} {v}", e.id.0, name)));
        }
        Member { base, name } => {
            p.line(indent, &tag(p, format!("member #{} .{:?}", e.id.0, name)));
            print_expr(p, indent + 1, base);
        }
        Index { base, index } => {
            p.line(indent, &tag(p, format!("index #{}", e.id.0)));
            print_expr(p, indent + 1, base);
            print_expr(p, indent + 1, index);
        }
        Call { callee, args } => {
            p.line(indent, &tag(p, format!("call #{}", e.id.0)));
            print_expr(p, indent + 1, callee);
            for a in args {
                print_expr(p, indent + 1, a);
            }
        }
        Unary { op, operand } => {
            let sym = match op {
                UnOp::Neg => "-",
                UnOp::Not => "!",
            };
            p.line(indent, &tag(p, format!("unary #{} {sym}", e.id.0)));
            print_expr(p, indent + 1, operand);
        }
        Binary { op, lhs, rhs } => {
            p.line(
                indent,
                &tag(p, format!("binary #{} {}", e.id.0, op.symbol())),
            );
            print_expr(p, indent + 1, lhs);
            print_expr(p, indent + 1, rhs);
        }
        Range { lo, hi } => {
            p.line(indent, &tag(p, format!("range #{}", e.id.0)));
            print_expr(p, indent + 1, lo);
            print_expr(p, indent + 1, hi);
        }
        HttpCall { verb, path, body } => {
            let b = if body.is_some() { " [body]" } else { "" };
            p.line(
                indent,
                &tag(p, format!("http #{} {verb} {path}{b}", e.id.0)),
            );
            if let Some(bo) = body {
                print_expr(p, indent + 1, bo);
            }
        }
        Match { scrutinee, arms } => {
            p.line(indent, &tag(p, format!("match #{}", e.id.0)));
            print_expr(p, indent + 1, scrutinee);
            for a in arms {
                match &a.value {
                    Some(v) => {
                        p.line(indent + 1, "arm:");
                        print_expr(p, indent + 2, v);
                        print_expr(p, indent + 2, &a.body);
                    }
                    None => {
                        p.line(indent + 1, "arm *");
                        print_expr(p, indent + 2, &a.body);
                    }
                }
            }
        }
        Transaction { body } => {
            p.line(indent, &tag(p, format!("transaction #{}", e.id.0)));
            print_block(p, indent + 1, body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend;

    fn hir(src: &str) -> HirProgram {
        let prog = frontend(src, "test.hard").expect("parse");
        lower(&prog)
    }

    const SAMPLE: &str = r#"
bring http
app @3000

users ::= [1, 2, 3]

calc double(Int x) => Int {
    y <- x * 2
    <- y
}

GET "/hello/:name" :: (name = Str) {
    n <- name
    ?(n == "world") {
        <- { hello: n }
    }
    <- { hello: name }
}
"#;

    #[test]
    fn lowering_is_deterministic() {
        let a = hir(SAMPLE);
        let b = hir(SAMPLE);
        assert_eq!(a.items.len(), b.items.len());
        assert_eq!(a.render(), b.render());
    }

    #[test]
    fn ids_are_distinct_per_kind() {
        let h = hir(SAMPLE);
        let fns: Vec<&HirFn> = h
            .items
            .iter()
            .filter_map(|i| match i {
                HirItem::Fn(f) => Some(f),
                _ => None,
            })
            .collect();
        assert_eq!(fns.len(), 2, "double + route");
        assert_ne!(fns[0].id, fns[1].id, "function ids unique");
        assert_ne!(fns[0].body.id, fns[1].body.id, "block ids unique");
    }

    #[test]
    fn global_declared_after_route_resolves() {
        // `users` is defined after nothing; ensure a *later* global resolves
        // even when referenced from a route body defined above it.
        let src = r#"
bring http
app @3000
GET "/" :: {
    n <- users
    <- { n: n }
}
users ::= 7
"#;
        let h = hir(src);
        let f = h
            .items
            .iter()
            .find_map(|i| match i {
                HirItem::Fn(f) => Some(f),
                _ => None,
            })
            .unwrap();
        // find the `users` ident expression
        fn has_users(e: &HirExpr, out: &mut Vec<Option<VarId>>) {
            if let HirExprKind::Ident { name, var } = &e.kind {
                if name == "users" {
                    out.push(*var);
                }
            }
            use HirExprKind::*;
            match &e.kind {
                Binary { lhs, rhs, .. } => {
                    has_users(lhs, out);
                    has_users(rhs, out);
                }
                _ => {}
            }
        }
        let mut found = Vec::new();
        for s in &f.body.stmts {
            if let HirStmt::Var { value, .. } = s {
                has_users(value, &mut found);
            }
        }
        assert_eq!(found.len(), 1, "one users ident");
        assert!(found[0].is_some(), "global resolves to a VarId");
    }

    #[test]
    fn spans_are_preserved() {
        let h = hir("app @3000");
        let span = match &h.items[0] {
            HirItem::App { span, .. } => *span,
            other => panic!("unexpected item {other:?}"),
        };
        assert!(span.line >= 1);
        assert!(span.col >= 1);
    }

    #[test]
    fn stats_are_authoritative() {
        let h = hir(SAMPLE);
        assert_eq!(h.stats.functions, 2);
        assert!(h.stats.exprs > 0);
        assert!(h.stats.blocks >= 2);
    }
}