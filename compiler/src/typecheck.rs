//! Static checks (lightweight but real): duplicate declarations, calls to
//! undefined functions, and named references that are undeclared in scope.
//! Everything is reported as [`Diag`]s with friendly suggestions.

use crate::ast::*;
use crate::catalog as cat;
use crate::error::{Diag, ErrorKind};
use crate::token::Span;
use std::collections::{BTreeMap, HashMap};

/// Allowed builtin modules. Derived from the AST catalog so the checker and
/// the code generator can never disagree about the module set.
const MODULES: &[&str] = Module::SET_NAMES;

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
    /// The model's own declarations, kept because the ORM schema is built from
    /// them (a name reference is not enough to build a table).
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

    /// The validated query plan for `e`, if it is an ORM chain rooted at a
    /// declared model.
    /// The steps of a query chain, if `e` is one.
    ///
    /// The test is the chain's root, not whether the chain is well formed: a
    /// query naming a column that does not exist still has to reach codegen so
    /// the programmer gets `HS0219` for the column instead of `HS0104` for an
    /// undefined variable.
    fn orm_chain_steps(&self, e: &Expr, scope: &HashMap<String, Span>) -> Option<Vec<(String, Vec<Expr>)>> {
        let (model, chain, _) = crate::orm::orm_chain(e)?;
        if !self.models.contains_key(&model) && !scope.contains_key(&model) {
            return None;
        }
        Some(chain.into_iter().map(|(name, args, _)| (name, args)).collect())
    }

    /// Type-check the value-carrying positions of a query chain.
    ///
    /// Column names, model names and direction keywords are the schema's
    /// business, so they are left for codegen; everything else is ordinary
    /// code and gets checked here. The positions are read off the chain's
    /// shape rather than off a parsed plan, so an invalid chain still gets its
    /// value expressions checked.
    fn scan_orm_chain(&mut self, chain: &[(String, Vec<Expr>)], scope: &HashMap<String, Span>) {
        for (name, args) in chain {
            match name.as_str() {
                // `where(col == value)`, `where(col, value)` and
                // `where(col, value, op)`.
                "where" => {
                    if args.len() == 1 {
                        match &args[0] {
                            // A column on the left, a value on the right.
                            Expr::Binary(_, l, r, _) if matches!(l.as_ref(), Expr::Ident(..)) => self.expr(r, scope),
                            // Not a column comparison, so every operand is
                            // ordinary code; codegen reports the bad operator.
                            other => self.expr(other, scope),
                        }
                    }
                    for a in args.iter().skip(1) {
                        self.expr(a, scope);
                    }
                }
                // `where_like(col, pattern)`, `where_in(col, list)`.
                "where_like" | "where_in" => {
                    for a in args.iter().skip(1) {
                        self.expr(a, scope);
                    }
                }
                // `find(key)`.
                "find" => {
                    for a in args {
                        self.expr(a, scope);
                    }
                }
                // `limit(n)`, `offset(n)`.
                "limit" | "offset" => {
                    for a in args {
                        self.expr(a, scope);
                    }
                }
                // `order_by(col, dir)` and the terminals carry no values.
                _ => {}
            }
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
                    } else {
                        // No exact model-field match: offer a "did you mean"
                        // candidate computed over everything in scope plus all
                        // declared model fields, when the edit distance says a
                        // typo is plausible.
                        let mut cands: Vec<String> = scope.keys().cloned().collect();
                        cands.extend(self.models.values().flat_map(|m| m.fields.iter().map(|f| f.name.clone())));
                        if let Some(cand) = crate::suggest::closest(name, &cands).filter(|c| c.as_str() != name) {
                            d = d.with_help(format!(
                                "Maybe you meant `{cand}`? If not, declare the variable before use."
                            ));
                        }
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
                // An ORM chain is rooted at a model, and its column names are
                // field references rather than variables. Type-checking it as
                // ordinary code would report `User` and `age` as undefined, so
                // the chain is walked for its *value* positions only — the
                // column side is the schema's business, and codegen checks it.
                if let Some(chain) = self.orm_chain_steps(e, scope) {
                    self.scan_orm_chain(&chain, scope);
                    return;
                }
                if let Expr::Ident(name, csp) = callee.as_ref() {
                    let known = self.funcs.contains_key(&name.clone()) || scope.contains_key(name);
                    let is_boot = name == "_" || name == "expect";
                    if !known && !is_boot {
                        let fallback = format!(
                            "Define `calc {name}(..) => .. {{ .. }}` before calling it, or use a module function like `json.parse(..)`."
                        );
                        let cands = self
                            .funcs
                            .keys()
                            .map(|k| k.strip_prefix("calc ").unwrap_or(k).to_string())
                            .chain(scope.keys().cloned())
                            .collect::<Vec<_>>();
                        let suggestion = crate::suggest::did_you_mean(name, &cands, fallback);
                        self.err_code(
                            cat::UNDEFINED_FUNCTION,
                            format!("call to undefined function `{name}`"),
                            *csp,
                            suggestion,
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