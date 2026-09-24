//! Static checks (lightweight but real): duplicate declarations, calls to
//! undefined functions, and named references that are undeclared in scope.
//! Everything is reported as [`Diag`]s with friendly suggestions.

use crate::ast::*;
use crate::catalog as cat;
use crate::error::{Diag, ErrorKind};
use crate::token::Span;
use std::collections::{BTreeMap, HashMap};

const MODULES: [&str; 10] = [
    "crypto", "fs", "env", "json", "jwt", "time", "runtime", "websocket", "postgres", "http",
];

/// Run the static checks over a program. Equivalent to `check_with(prog, &[])`
/// — no per-statement file provenance, so "declared here" related spans cannot
/// point into source files.
pub fn check(prog: &Program) -> Vec<Diag> {
    check_with(prog, &[])
}

/// Run the static checks with per-statement source files (Diagnostics V2).
///
/// `files` maps `prog.stmts[i]` to its module path (project-root-relative, as
/// written in the build manifest); the grapher drives this so related spans
/// render on the real declaring file (e.g. `models/user.hard:5:5`).
pub fn check_with(prog: &Program, files: &[Option<String>]) -> Vec<Diag> {
    let mut t = Checker {
        diags: Vec::new(),
        funcs: HashMap::new(),
        globals: HashMap::new(),
        files: files.to_vec(),
        models: BTreeMap::new(),
        model_files: BTreeMap::new(),
    };
    t.collect(prog);
    t.scan(prog);
    t.diags
}

fn declare_unique(
    map: &mut HashMap<String, (Span, Option<String>)>,
    diags: &mut Vec<Diag>,
    name: &str,
    span: Span,
    file: Option<String>,
) {
    if let Some((prev, prev_file)) = map.get(name) {
        diags.push(
            Diag::new(
                ErrorKind::Type,
                format!("duplicate declaration of `{name}`"),
                span,
                "Rename one of the declarations.",
            )
            .with_code(cat::DUPLICATE_DECL)
            .with_related(
                prev_file.clone().unwrap_or_default(),
                *prev,
                "first declared here",
            ),
        );
    } else {
        map.insert(name.to_string(), (span, file));
    }
}

struct Checker {
    diags: Vec<Diag>,
    funcs: HashMap<String, (Span, Option<String>)>,
    globals: HashMap<String, Span>,
    files: Vec<Option<String>>,
    models: BTreeMap<String, ModelDef>,
    model_files: BTreeMap<String, Option<String>>,
}

impl Checker {
    /// Look up a declared model field by its exact name. Returns the model
    /// name, the field span (for the related frame), and the model's file.
    fn find_model_field(&self, name: &str) -> Option<(&str, Span, Option<String>)> {
        for (model, m) in self.models.iter() {
            for f in &m.fields {
                if f.name == name {
                    return Some((model, f.span, self.model_files.get(model).cloned().flatten()));
                }
            }
        }
        None
    }

    fn err_code(&mut self, code: u16, msg: String, span: Span, suggestion: String) {
        self.diags.push(Diag::new(ErrorKind::Type, msg, span, suggestion).with_code(code));
    }

    fn collect(&mut self, prog: &Program) {
        let funcs: &mut HashMap<String, (Span, Option<String>)> = &mut self.funcs;
        let diags: &mut Vec<Diag> = &mut self.diags;
        for (i, st) in prog.stmts.iter().enumerate() {
            let file = self.files.get(i).cloned().flatten();
            match st {
                Stmt::Func(f) => declare_unique(funcs, diags, &f.name, f.span, file),
                Stmt::Test(t) => declare_unique(funcs, diags, &format!("test {}", t.name), t.span, file),
                Stmt::Middleware { name, span, .. } => {
                    declare_unique(funcs, diags, &format!("middleware {name}"), *span, file)
                }
                Stmt::Route(r) => {
                    declare_unique(funcs, diags, &format!("{} {}", r.method, r.path), r.span, file);
                }
                Stmt::Model(m) => {
                    self.models.insert(m.name.clone(), m.clone());
                    self.model_files.insert(m.name.clone(), file.clone());
                    declare_unique(funcs, diags, &format!("model {}", m.name), m.span, file);
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
                    scope.insert("req".into(), Span::new(0, 0));
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
                    // Globals are shared across the file (SPEC §9), so an
                    // initializer may reference any other top-level name.
                    let mut scope = self.base_scope(true);
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
        for (name, (sp, _)) in self.funcs.iter() {
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
                    let mut d = Diag::new(
                        ErrorKind::Type,
                        format!("`{name}` is not defined"),
                        *sp,
                        "Declare the variable before use, or reference a model field.",
                    )
                    .with_expected("a defined name or module function".to_string())
                    .with_received(format!("`{name}`"))
                    .with_code(cat::UNDEFINED_VARIABLE);
                    // Cross-file related span: if `name` is a declared model
                    // field, point at its definition in the declaring module
                    // (e.g. `models/user.hard:5:5`). BTreeMap iteration keeps
                    // the first match deterministic.
                    if let Some((model, fspan, model_file)) = self.find_model_field(name) {
                        d = d
                            .with_related(
                                model_file.unwrap_or_default(),
                                fspan,
                                format!("field `{name}` declared in model `{model}`"),
                            )
                            .with_help(format!(
                                "Access the field through the model, e.g. `{model}(..).{name}`."
                            ));
                    }
                    self.diags.push(d);
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
                        self.err_code(
                            cat::UNDEFINED_FUNCTION,
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