//! Static checks (lightweight but real): duplicate declarations, calls to
//! undefined functions, and named references that are undeclared in scope.
//! Everything is reported as [`Diag`]s with friendly suggestions.

use crate::ast::*;
use crate::error::{Diag, ErrorKind};
use crate::token::Span;
use std::collections::HashMap;

const MODULES: [&str; 10] = [
    "crypto", "fs", "env", "json", "jwt", "time", "runtime", "websocket", "postgres", "http",
];

pub fn check(prog: &Program) -> Vec<Diag> {
    let mut t = Checker {
        diags: Vec::new(),
        funcs: HashMap::new(),
        globals: HashMap::new(),
    };
    t.collect(prog);
    t.scan(prog);
    t.diags
}

fn declare_unique(
    map: &mut HashMap<String, Span>,
    diags: &mut Vec<Diag>,
    name: &str,
    span: Span,
) {
    if let Some(prev) = map.get(name) {
        diags.push(Diag::new(
            ErrorKind::Type,
            format!("duplicate declaration of `{name}`"),
            span,
            format!(
                "Rename one of them; the first is declared at {}:{}.",
                prev.line, prev.col
            ),
        ));
    } else {
        map.insert(name.to_string(), span);
    }
}

struct Checker {
    diags: Vec<Diag>,
    funcs: HashMap<String, Span>,
    globals: HashMap<String, Span>,
}

impl Checker {
    fn err(&mut self, msg: String, span: Span, suggestion: String) {
        self.diags.push(Diag::new(ErrorKind::Type, msg, span, suggestion));
    }

    fn collect(&mut self, prog: &Program) {
        let funcs: &mut HashMap<String, Span> = &mut self.funcs;
        let diags: &mut Vec<Diag> = &mut self.diags;
        for st in &prog.stmts {
            match st {
                Stmt::Func(f) => declare_unique(funcs, diags, &f.name, f.span),
                Stmt::Test(t) => declare_unique(funcs, diags, &format!("test {}", t.name), t.span),
                Stmt::Middleware { name, span, .. } => {
                    declare_unique(funcs, diags, &format!("middleware {name}"), *span)
                }
                Stmt::Route(r) => {
                    declare_unique(funcs, diags, &format!("{} {}", r.method, r.path), r.span);
                }
                Stmt::Model(m) => {
                    declare_unique(funcs, diags, &format!("model {}", m.name), m.span);
                }
                Stmt::Var(v) | Stmt::Const(v) => {
                    self.globals.entry(v.name.clone()).or_insert(v.span);
                }
                _ => {}
            }
        }
    }

    fn scan(&mut self, prog: &Program) {
        for st in &prog.stmts {
            match st {
                Stmt::Func(f) => {
                    let mut scope = self.base_scope(true);
                    for p in &f.params {
                        scope.insert(p.name.clone(), p.span);
                    }
                    self.scan_block(&f.body, &mut scope);
                }
                Stmt::Route(r) => {
                    let mut scope = self.base_scope(true);
                    scope.insert("req".into(), r.span);
                    for p in &r.params {
                        scope.insert(p.name.clone(), p.span);
                    }
                    self.scan_block(&r.body, &mut scope);
                }
                Stmt::Middleware { body, .. } => {
                    let mut scope = self.base_scope(true);
                    self.scan_block(body, &mut scope);
                }
                Stmt::Socket(s) => {
                    let mut scope = self.base_scope(true);
                    if let Some(b) = &s.connect {
                        self.scan_block(b, &mut scope);
                    }
                    if let Some(b) = &s.message {
                        let mut m = scope.clone();
                        m.insert("d".into(), s.span);
                        self.scan_block(b, &mut m);
                    }
                    if let Some(b) = &s.disconnect {
                        self.scan_block(b, &mut scope);
                    }
                }
                Stmt::Test(t) => {
                    let mut scope = self.base_scope(true);
                    self.scan_block(&t.body, &mut scope);
                }
                Stmt::Var(v) | Stmt::Const(v) => {
                    if let Some(prev) = self.globals.get(&v.name) {
                        let _ = prev;
                    }
                    let mut scope = self.base_scope(false);
                    self.expr(&v.value, &mut scope);
                }
                _ => {}
            }
        }
    }

    fn base_scope(&mut self, with_globals: bool) -> HashMap<String, Span> {
        let mut s: HashMap<String, Span> = HashMap::new();
        for m in MODULES {
            s.insert(m.to_string(), Span::new(0, 0));
        }
        for (name, sp) in self.funcs.iter() {
            s.insert(name.clone(), *sp);
        }
        if with_globals {
            for (name, sp) in self.globals.iter() {
                s.insert(name.clone(), *sp);
            }
        }
        s
    }

    fn scan_block(&mut self, body: &[Stmt], outer: &mut HashMap<String, Span>) {
        let mut scope = outer.clone();
        for st in body {
            self.stmt(st, &mut scope);
        }
    }

    fn stmt(&mut self, st: &Stmt, scope: &mut HashMap<String, Span>) {
        match st {
            Stmt::Var(v) | Stmt::Const(v) => {
                self.expr(&v.value, scope);
                scope.insert(v.name.clone(), v.span);
            }
            Stmt::If { cond, then_body, else_body, .. } => {
                self.expr(cond, scope);
                self.scan_block(then_body, scope);
                self.scan_block(else_body, scope);
            }
            Stmt::Loop { var, iter, body, .. } => {
                self.expr(iter, scope);
                let mut inner = scope.clone();
                inner.insert(var.clone(), iter.span());
                self.scan_block(body, &mut inner);
            }
            Stmt::Return(e, _) => self.expr(e, scope),
            Stmt::Race(es, _) => {
                for e in es {
                    self.expr(e, scope);
                }
            }
            Stmt::Expect { lhs, rhs, .. } => {
                self.expr(lhs, scope);
                self.expr(rhs, scope);
            }
            Stmt::ExprStmt(e) => self.expr(e, scope),
            _ => {}
        }
    }

    fn expr(&mut self, e: &Expr, scope: &HashMap<String, Span>) {
        match e {
            Expr::Ident(name, sp) => {
                if !scope.contains_key(name) && !self.funcs.contains_key(name) {
                    self.diags.push(
                        Diag::new(
                            ErrorKind::Type,
                            format!("`{name}` is not defined"),
                            *sp,
                            format!(
                                "Define `{name} <- ...` (mutable) or `{name} ::= ...` (const) before use."
                            ),
                        )
                        .with_expected(format!("a defined name or module function"))
                        .with_received(format!("`{name}`")),
                    );
                }
            }
            Expr::Member(base, _, _) => self.expr(base, scope),
            Expr::Index(base, idx, _) => {
                self.expr(base, scope);
                self.expr(idx, scope);
            }
            Expr::Call { callee, args, span } => {
                if let Expr::Ident(name, csp) = callee.as_ref() {
                    let known = self.funcs.contains_key(&name.clone()) || scope.contains_key(name);
                    let is_boot = name == "_" || name == "expect";
                    if !known && !is_boot {
                        self.err(
                            format!("call to undefined function `{name}`"),
                            *csp,
                            format!("Define `calc {name}(..) => .. {{ .. }}` before calling it, or use a module function like `json.parse(..)`."),
                        );
                    }
                } else if let Expr::Member(base, field, _) = callee.as_ref() {
                    if let Expr::Ident(module, _) = base.as_ref() {
                        if !MODULES.contains(&module.as_str()) {
                            // field call on a value — acceptable dynamic call
                        }
                    }
                    let _ = field;
                    self.expr(base, scope);
                }
                let _ = span;
                for a in args {
                    self.expr(a, scope);
                }
            }
            Expr::Unary(_, x, _) => self.expr(x, scope),
            Expr::Binary(_, l, r, _) => {
                self.expr(l, scope);
                self.expr(r, scope);
            }
            Expr::Range(l, r, _) => {
                self.expr(l, scope);
                self.expr(r, scope);
            }
            Expr::List(items, _) => {
                for i in items {
                    self.expr(i, scope);
                }
            }
            Expr::Obj(kvs, _) => {
                for (_, v) in kvs {
                    self.expr(v, scope);
                }
            }
            Expr::Match(s, arms, _) => {
                self.expr(s, scope);
                for arm in arms {
                    if let Some(v) = &arm.value {
                        self.expr(v, scope);
                    }
                    self.expr(&arm.body, scope);
                }
            }
            Expr::HttpCall { body, .. } => {
                if let Some(b) = body {
                    self.expr(b, scope);
                }
            }
            _ => {}
        }
    }
}