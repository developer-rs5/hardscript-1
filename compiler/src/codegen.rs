//! C++ code generator.
//!
//! Emits a single `.cpp` file that `#include "hs_runtime.hpp"` and wires the
//! parsed HardScript program into `hs::Server`, `hs::WsHandler`, crypto/fs/...
//! builtins, the test runner and `main()`.

use crate::ast::*;
use crate::error::{Diag, ErrorKind};
use crate::token::Span;

/// Allowed builtin modules for lowering `module.builtin(...)` calls. Derived
/// from the AST catalog (see [`Module::SET`]).
const MODULES: &[&str] = Module::SET_NAMES;

const CPP_KEYWORDS: [&str; 61] = [
    "alignas", "alignof", "and", "and_eq", "asm", "auto", "bitand", "bitor", "bool", "break",
    "case", "catch", "char", "class", "compl", "const", "constexpr", "const_cast", "continue",
    "default", "delete", "do", "double", "dynamic_cast", "else", "enum", "explicit", "export",
    "extern", "false", "float", "for", "friend", "goto", "if", "inline", "int", "long", "mutable",
    "namespace", "new", "noexcept", "not", "not_eq", "nullptr", "operator", "or", "or_eq", "private",
    "protected", "public", "register", "reinterpret_cast", "return", "short", "signed", "sizeof",
    "static", "static_assert", "static_cast", "struct",
];

/// The C++ spelling of a column's storage class, for `hs::OrmKind`.
fn orm_kind_cpp(t: crate::orm::SqlType) -> &'static str {
    use crate::orm::SqlType::*;
    match t {
        Int | BigInt => "hs::OrmKind::Int",
        Float => "hs::OrmKind::Float",
        Bool => "hs::OrmKind::Bool",
        Uuid => "hs::OrmKind::Uuid",
        Time => "hs::OrmKind::Time",
        Json => "hs::OrmKind::Json",
        Text => "hs::OrmKind::Text",
    }
}

/// The C++ expression for a `where` step's bound value.
fn orm_value_cpp(cg: &mut Codegen, method: &str, args: &[Expr]) -> String {
    let nil = "hs::Val::nil()".to_string();
    match method {
        // `find(id)` — the key itself.
        "find" => args.first().map(|a| cg.expr(a)).unwrap_or(nil),
        // `where_like(col, pattern)` and `where_in(col, list)`.
        "where_like" | "where_in" => args.get(1).map(|a| cg.expr(a)).unwrap_or(nil),
        "where" => {
            // `where(col == value)` puts the comparison in a single argument,
            // so the value is the comparison's right-hand side.
            if args.len() == 1 {
                if let Expr::Binary(_, _, r, _) = &args[0] {
                    return cg.expr(r);
                }
                return nil;
            }
            // `where(col, value)` and `where(col, value, op)`.
            args.get(1).map(|a| cg.expr(a)).unwrap_or(nil)
        }
        _ => nil,
    }
}

fn safe_id(name: &str) -> String {
    if CPP_KEYWORDS.contains(&name) {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

fn cstring(s: &str) -> String {
    format!("{:?}", s)
}

/// A desugared `ratelimit.check(..)` call (or one written by hand): the shape
/// the parser produces for `limit ...`, collected for route prologues when it
/// appears at the top level.
fn is_ratelimit_check(e: &Expr) -> bool {
    match e {
        Expr::Call { callee, args, .. } => match callee.as_ref() {
            Expr::Member(base, name, _) => {
                matches!(base.as_ref(), Expr::Ident(root, _) if root == "ratelimit")
                    && name == "check"
                    && (args.len() == 4 || args.len() == 5)
            }
            _ => false,
        },
        _ => false,
    }
}
/// A desugared `queue.enqueue(..)` call (or one written by hand): the shape
/// the parser produces for `queue Name(..)`, collected for `main` when it
/// appears at the top level.
fn is_queue_enqueue_call(e: &Expr) -> bool {
    match e {
        Expr::Call { callee, args, .. } => match callee.as_ref() {
            Expr::Member(base, name, _) => {
                matches!(base.as_ref(), Expr::Ident(root, _) if root == "queue")
                    && name == "enqueue"
                    && args.len() == 3
            }
            _ => false,
        },
        _ => false,
    }
}

/// A top-level `cache x ttl T` declaration, desugared to
/// `cache.declare("x", N)`. Only declarations execute at startup; any other
/// top-level expression statement keeps its old meaning (dropped).
fn is_cache_declare(e: &Expr) -> bool {
    match e {
        Expr::Call { callee, args, .. } => match callee.as_ref() {
            Expr::Member(base, name, _) => {
                matches!(base.as_ref(), Expr::Ident(root, _) if root == "cache")
                    && name == "declare"
                    && args.len() == 2
                    && matches!(&args[0], Expr::Str(..))
                    && matches!(&args[1], Expr::Int(..))
            }
            _ => false,
        },
        _ => false,
    }
}

/// A top-level `email.template("name", "body ...")`, which is a registration
/// like `cache x ttl T`: it has to run at startup or no send can find it.
fn is_email_template(e: &Expr) -> bool {
    match e {
        Expr::Call { callee, args, .. } => match callee.as_ref() {
            Expr::Member(base, name, _) => {
                matches!(base.as_ref(), Expr::Ident(root, _) if root == "email")
                    && name == "template"
                    && args.len() == 2
                    && matches!(&args[0], Expr::Str(..))
                    && matches!(&args[1], Expr::Str(..))
            }
            _ => false,
        },
        _ => false,
    }
}

/// C++ double literal for a rule bound (`18` -> `18.0`).
fn cdbl(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 9.0e15 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

/// Map one `name = value` field constraint onto a runtime rule constructor.
///
/// Unknown constraint names lower to nothing rather than failing the build:
/// the field type still validates, and `hard openapi` records the constraint
/// as documentation.
fn rule_from_expr(key: &str, v: &Expr) -> Option<String> {
    let num = |e: &Expr| -> Option<f64> {
        match e {
            Expr::Int(n, _) => Some(*n as f64),
            Expr::Float(f, _) => Some(*f),
            _ => None,
        }
    };
    let txt = |e: &Expr| -> Option<String> {
        match e {
            Expr::Str(s, _) => Some(s.clone()),
            Expr::Ident(s, _) => Some(s.clone()),
            _ => None,
        }
    };
    match key {
        "min" | "gte" => num(v).map(|n| format!("hs::r_min({})", cdbl(n))),
        "max" | "lte" => num(v).map(|n| format!("hs::r_max({})", cdbl(n))),
        "gt" => num(v).map(|n| format!("hs::r_min({})", cdbl(n + 1.0))),
        "lt" => num(v).map(|n| format!("hs::r_max({})", cdbl(n - 1.0))),
        "between" | "range" => match v {
            Expr::Range(lo, hi, _) => {
                let l = num(lo)?;
                let h = num(hi)?;
                Some(format!("hs::r_minmax({}, {})", cdbl(l), cdbl(h)))
            }
            _ => None,
        },
        "length" | "len" => match v {
            // `length = 3..30`
            Expr::Range(lo, hi, _) => {
                let l = num(lo)?;
                let h = num(hi)?;
                Some(format!("hs::r_length({}, {})", l as i64, h as i64))
            }
            // `length = 8`
            _ => num(v).map(|n| format!("hs::r_length({}, {})", n as i64, i64::MAX)),
        },
        "regex" | "match" | "pattern" => txt(v).map(|s| format!("hs::r_regex({:?})", s)),
        "in" | "oneof" | "enum" | "values" => match v {
            Expr::List(items, _) => {
                let lits = items.iter().filter_map(|i| txt(i)).collect::<Vec<_>>();
                if lits.is_empty() {
                    None
                } else {
                    let parts = lits.iter().map(|s| format!("{:?}", s)).collect::<Vec<_>>();
                    Some(format!("hs::r_enum({{{}}})", parts.join(", ")))
                }
            }
            _ => txt(v).map(|s| format!("hs::r_enum_str({:?})", s)),
        },
        "nullable" | "optional" => Some("hs::r_nullable()".to_string()),
        "required" => Some("hs::r_required()".to_string()),
        _ => None,
    }
}

struct Codegen {
    out: String,
    ind: usize,
    path: String,
    diags: Vec<Diag>,
    /// Declared `model` names, used to decide whether a route body parameter
    /// gets automatic validation (M5.1).
    model_names: std::collections::BTreeSet<String>,
    /// The `protect` declaration, if any: decides which routes get the auth
    /// guard and which stay public (M5.2).
    protect: Option<ProtectDef>,
    /// Which model a local holds, for a record's own `save`/`touch`/`destroy`.
    ///
    /// A record is an ordinary `Val` at runtime, so the only thing tying
    /// `user.save()` to the `user` table is knowing what `user` was read
    /// from. That is a fact about the program text, and codegen is where the
    /// program text is in hand.
    record_types: std::collections::HashMap<String, String>,
    /// The ORM schema, built leniently so a model nobody queries keeps
    /// compiling exactly as it did before the ORM existed. Strictness is
    /// applied per model, when a query actually touches it.
    orm: Option<crate::orm::Schema>,
    /// What is being emitted right now. Request-scoped builtins such as
    /// `http.header` and `auth.require` read the in-flight `Request`, so they
    /// are only meaningful where one exists; without this the mistake would
    /// surface as a raw g++ "req was not declared" error.
    ctx: Ctx,
}

/// Generate the C++ translation unit for `prog`.
pub fn generate(prog: &Program) -> Result<String, Vec<Diag>> {
    let mut cg = Codegen {
        out: String::new(),
        ind: 0,
        path: prog.path.clone(),
        diags: Vec::new(),
        model_names: std::collections::BTreeSet::new(),
        orm: None,
        record_types: std::collections::HashMap::new(),
        protect: None,
        ctx: Ctx::Func,
    };
    cg.run(prog);
    if cg.diags.is_empty() {
        Ok(cg.out)
    } else {
        Err(cg.diags)
    }
}

impl Codegen {
    fn w(&mut self, s: &str) {
        self.out.push_str(s);
    }
    fn wln(&mut self, s: &str) {
        for _ in 0..self.ind {
            self.out.push_str("    ");
        }
        self.out.push_str(s);
        self.out.push('\n');
    }
    fn blank(&mut self) {
        self.out.push('\n');
    }

    /// Render statements to a string instead of the output, for positions
    /// where the emitter needs a value (the transaction-in-value fallback).
    /// Declaration tracking is shared, not swapped: the statements execute in
    /// order either way.
    fn stmt_str(&mut self, body: &[Stmt], ctx: Ctx) -> String {
        let saved = std::mem::take(&mut self.out);
        let saved_ind = self.ind;
        self.ind = 0;
        self.stmts(body, ctx);
        let rendered = std::mem::replace(&mut self.out, saved);
        self.ind = saved_ind;
        rendered
    }
    fn err_with(&mut self, msg: impl Into<String>, span: Span, help: &str) {
        self.diags.push(
            Diag::new(ErrorKind::Type, msg, span, help)
                .with_code(crate::catalog::DUPLICATE_PROTECT),
        );
    }

    fn err(&mut self, msg: impl Into<String>, span: Span) {
        self.diags.push(Diag::new(
            ErrorKind::Codegen,
            msg,
            span,
            "This construct is not supported by the compiler yet.",
        )
        .with_code(crate::catalog::INTERNAL_COMPILER));
    }

    fn run(&mut self, prog: &Program) {
        self.w("// Generated by the HardScript compiler — do not edit.\n");
        self.wln("#include \"hs_runtime.hpp\"");
        self.wln("using namespace hs;");
        self.wln("using std::string;");
        self.blank();

        let mut routes: Vec<&RouteDef> = Vec::new();
        let mut protect: Option<&ProtectDef> = None;
        let mut funcs: Vec<&FunDef> = Vec::new();
        let mut mws: Vec<String> = Vec::new();
        let mut mw_bodies: Vec<&Vec<Stmt>> = Vec::new();
        let mut sockets: Vec<&SocketDef> = Vec::new();
        let mut tests: Vec<&TestDef> = Vec::new();
        let mut models: Vec<&ModelDef> = Vec::new();
        // Top-level `cache x ttl T` declarations run once at startup. They
        // desugar to `cache.declare(..)` calls, which look like any other
        // top-level expression statement — except these must execute, so
        // they are collected for `main` instead of dropped with the rest.
        let mut cache_decls: Vec<String> = Vec::new();
        let mut email_templates: Vec<String> = Vec::new();
        // Top-level `job` declarations, `queue` calls and `__worker_*`
        // functions, likewise collected for `main`: jobs register their
        // types, enqueues seed initial jobs, and workers register handlers.
        let mut job_decls: Vec<(String, Vec<String>)> = Vec::new();
        let mut top_enqueues: Vec<String> = Vec::new();
        let mut worker_regs: Vec<String> = Vec::new();
        // Top-level `limit ...` declarations guard every route: one shared
        // bucket per declaration, checked before anything else runs.
        let mut global_limits: Vec<String> = Vec::new();
        // Top-level `every ...` schedules register in `main` with their
        // lowered functions.
        let mut sched_regs: Vec<String> = Vec::new();

        for st in &prog.stmts {
            match st {
                Stmt::Var(v) | Stmt::Const(v) => {
                    let val = self.expr(&v.value);
                    let n = safe_id(&v.name);
                    let line = format!("hs::Val {n} = {val};");
                    self.wln(&line);
                    if let Some(m) = self.expr_record_type(&v.value) {
                        self.record_types.insert(v.name.clone(), m);
                    }
                }
                Stmt::Func(f) => funcs.push(f),
                Stmt::Route(r) => routes.push(r),
                Stmt::Middleware { name, body, .. } => {
                    mws.push(name.clone());
                    mw_bodies.push(body);
                }
                Stmt::Socket(s) => sockets.push(s),
                Stmt::Test(t) => tests.push(t),
                Stmt::Model(m) => models.push(m),
                Stmt::Protect(d) => match self.check_protect(d) {
                    Some(err) => self.diags.push(err),
                    None if protect.is_some() => self.err_with(
                        "`protect` is already declared".to_string(),
                        d.span,
                        "One protect declaration covers every route; merge the two.",
                    ),
                    None => protect = Some(d),
                },
                Stmt::ExprStmt(e) if is_cache_declare(e) => {
                    cache_decls.push(self.expr(e));
                }
                Stmt::Job(j) => {
                    job_decls.push((j.name.clone(), j.params.iter().map(|p| p.name.clone()).collect()));
                }
                Stmt::Schedule(s) => {
                    sched_regs.push(self.schedule_register_call(s));
                }
                // A desugared top-level `queue Name(..)` seeds an initial
                // job: run it from `main` like the declarations above,
                // instead of dropping it with the other top-level
                // expressions.
                Stmt::ExprStmt(e) if is_queue_enqueue_call(e) => {
                    top_enqueues.push(self.expr(e));
                }
                Stmt::ExprStmt(e) if is_email_template(e) => {
                    email_templates.push(self.expr(e));
                }
                Stmt::ExprStmt(e) if is_ratelimit_check(e) => {
                    // Rendered for route prologues, where a request exists:
                    // temporarily borrow the route context so the
                    // request-gated builtin lowers instead of erroring.
                    let outer = self.ctx;
                    self.ctx = Ctx::Route;
                    let rendered = self.expr(e);
                    self.ctx = outer;
                    global_limits.push(rendered);
                }
                _ => {}
            }
        }

        // Model names drive automatic request-body validation and `protect`
        // decides which routes get the guard, so both have to be known before
        // any route is emitted.
        self.model_names = models.iter().map(|m| m.name.clone()).collect();
        self.protect = protect.cloned();
        if !models.is_empty() {
            // Lenient on purpose: see the field comment. A schema error must
            // only ever be reachable from a query that uses the model.
            let owned: Vec<crate::ast::ModelDef> = models.iter().map(|m| (*m).clone()).collect();
            self.orm = crate::orm::build(&owned, crate::orm::BuildOpts::lenient()).ok();
        }

        for f in &funcs {
            self.emit_func(f);
            // A lowered `worker Name { }` block registers its handler here,
            // by function name: the definition above makes the pointer valid.
            if let Some(job) = f.name.strip_prefix("__worker_") {
                worker_regs.push(job.to_string());
            }
        }
        for (i, m) in models.iter().enumerate() {
            self.emit_schema(m, i);
        }
        if let Some(schema) = self.orm.clone() {
            self.emit_orm_models(&schema);
        }
        for (i, r) in routes.iter().enumerate() {
            self.emit_route(r, i, &global_limits);
        }
        for (i, (name, body)) in mws.iter().zip(mw_bodies.iter()).enumerate() {
            self.emit_middleware(name, body, i);
        }

        let port = prog
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::App(p, _) => Some(*p),
                _ => None,
            })
            .unwrap_or(3000);

        self.emit_socket_registrations(&sockets);
        self.emit_test_registrations(&tests);
        self.emit_main(
            port,
            &routes,
            mws.len(),
            sockets.len(),
            tests.len(),
            models.len(),
            &cache_decls,
            &job_decls,
            &top_enqueues,
            &worker_regs,
            &sched_regs,
            &email_templates,
        );
    }

    /// `every ...`: register the schedule with its lowered function. The
    /// function exists because the parser emitted it alongside this node.
    fn schedule_register_call(&self, s: &crate::ast::SchedDef) -> String {
        let f = format!("fn_{}", safe_id(&s.name));
        let name = format!("hs::Val::text({:?})", s.name);
        match &s.kind {
            crate::ast::SchedKind::Interval { secs } => {
                format!("hs::schedule_register_interval({name}, hs::Val::int_({secs}), {f})")
            }
            crate::ast::SchedKind::Daily { h, m, s: sec, tz } => {
                let tz = tz.as_ref().map(|t| format!("hs::Val::text({t:?})")).unwrap_or_else(|| "hs::Val::nil()".to_string());
                format!("hs::schedule_register_daily({name}, hs::Val::int_({h}), hs::Val::int_({m}), hs::Val::int_({sec}), {tz}, {f})")
            }
            crate::ast::SchedKind::Weekly { weekday, h, m, s: sec, tz } => {
                let tz = tz.as_ref().map(|t| format!("hs::Val::text({t:?})")).unwrap_or_else(|| "hs::Val::nil()".to_string());
                format!("hs::schedule_register_weekly({name}, hs::Val::int_({weekday}), hs::Val::int_({h}), hs::Val::int_({m}), hs::Val::int_({sec}), {tz}, {f})")
            }
            crate::ast::SchedKind::Startup => {
                format!("hs::schedule_register_startup({name}, {f})")
            }
        }
    }

    /// `job Name(..)`: declare the type, its payload parameter names, and the
    /// default attempt budget. Used for top-level declarations collected into
    /// `main` and for declarations inside bodies.
    fn declare_job_call(&self, name: &str, params: &[String]) -> String {
        let list: Vec<String> = params.iter().map(|p| format!("hs::Val::text({p:?})")).collect();
        format!(
            "hs::queue_declare_job(hs::Val::text({name:?}), hs::Val::list({{{}}}), hs::Val::nil())",
            list.join(", ")
        )
    }

    // -----------------------------------------------------------------
    // ORM (M5.3)
    // -----------------------------------------------------------------

    /// The C++ identifier a model's metadata object is registered under.
    fn orm_sym(model: &str) -> String {
        format!("__hs_orm_{}", safe_id(model))
    }

    /// Register every model as an `hs::OrmModel`, so a query names its model
    /// rather than rebuilding the metadata at each call site.
    ///
    /// Emitted at namespace scope and initialized from string literals, which
    /// is constant initialization: it is ready before `main` runs, so a query
    /// in a global initializer would work too.
    fn emit_orm_models(&mut self, schema: &crate::orm::Schema) {
        self.blank();
        for t in schema.canonical_tables() {
            let mut cols = Vec::new();
            let mut kinds = Vec::new();
            // The database fills these in, so a write may leave them out and
            // `touch` knows which one to refresh.
            let mut generated = Vec::new();
            let mut updated = String::new();
            for c in &t.columns {
                cols.push(format!("{:?}", c.name));
                kinds.push(orm_kind_cpp(c.sql_type));
                if c.has_db_default() {
                    generated.push(format!("{:?}", c.name));
                }
                if c.updated_at {
                    updated = c.name.clone();
                }
            }
            let pk = t.primary_key_name().unwrap_or("");
            self.wln(&format!(
                "static const hs::OrmModel {}({:?}, {:?}, {:?}, std::vector<std::string>{{ {} }}, std::vector<hs::OrmKind>{{ {} }}, std::vector<std::string>{{ {} }}, {:?});",
                Self::orm_sym(&t.model),
                t.model,
                t.table,
                pk,
                cols.join(", "),
                kinds.join(", "),
                generated.join(", "),
                updated
            ));
        }
    }

    /// Emit a validated query chain.
    ///
    /// The model a value holds, when it is knowable from its own expression.
    ///
    /// A query's terminal decides it: `User.all()` and `User.find(1)` both
    /// yield `User` rows, `User.count()` yields a number and so yields no
    /// model at all.
    fn expr_record_type(&mut self, e: &Expr) -> Option<String> {
        let (root, chain, _) = crate::orm::orm_chain(e)?;

        // A chain rooted at a record reads a relationship, so what it yields is
        // the *target's* rows, not the record's own model.
        if let Some(model) = self.record_types.get(&root).cloned() {
            let (field, args, _) = chain.first()?.clone();
            if !args.is_empty() {
                return None;
            }
            let target = self
                .orm
                .as_ref()?
                .relations_from(&model)
                .find(|r| r.field == field)?
                .to
                .clone();
            let rest = &chain[1..];
            let terminal = rest.last().map(|(n, _, _)| n.as_str()).unwrap_or("all");
            return match crate::orm::terminal_of(terminal) {
                Some(crate::orm::Terminal::All) | Some(crate::orm::Terminal::First) => Some(target),
                _ => None,
            };
        }

        let model = root;
        if !self.model_names.contains(&model) {
            return None;
        }
        // The terminal is the last step; without one the chain is a
        // collection query. `find` spells no terminal but implies one.
        let terminal = chain.last().map(|(n, _, _)| n.as_str()).unwrap_or("all");
        if terminal == "find" {
            return Some(model);
        }
        match crate::orm::terminal_of(terminal) {
            Some(crate::orm::Terminal::All) | Some(crate::orm::Terminal::First) => Some(model),
            _ => None,
        }
    }

    /// A write against a model, or a record's own write.
    ///
    /// Model-rooted writes (`User.create(..)`) name their model in the source,
    /// so they need nothing but the schema. A record's write (`user.save()`)
    /// names a variable instead, and the model comes from what that variable
    /// was read as.
    fn orm_write(&mut self, call: &Expr) -> Option<String> {
        let schema = self.orm.as_ref()?;

        // `user.save()`
        if let Some((method, args, span)) = crate::orm::record_write(call) {
            let Expr::Call { callee, .. } = call else { return None };
            let Expr::Member(base, _, _) = callee.as_ref() else { return None };
            let Expr::Ident(name, _) = base.as_ref() else { return None };
            // HardScript has no methods of its own, so a bare name with no
            // record type is a mistake rather than some other kind of call:
            // falling through would read it as a field and report that the
            // field is missing, which names the wrong problem.
            let Some(model) = self.record_types.get(name).cloned() else {
                self.diags.push(crate::orm::err(
                    span,
                    crate::catalog::ORM_NOT_A_RECORD,
                    format!("`{name}` is not a record, so it has nothing to write."),
                    "Read one row first, then write it: `u <- User.find(1)`.",
                ));
                return Some("hs::Val::nil()".to_string());
            };
            let plan = match crate::orm::parse_write(&model, &method, &args, true, schema) {
                Ok(p) => p,
                Err(mut d) => {
                    self.diags.append(&mut d);
                    return Some("hs::Val::nil()".to_string());
                }
            };
            let rec = self.expr(base);
            let m = self.model_ref(&plan.model);
            return Some(match plan.op {
                crate::orm::WriteOp::Save => {
                    format!("hs::orm_save({m}, hs::db_need(), {rec})")
                }
                crate::orm::WriteOp::Touch => {
                    format!("hs::orm_touch({m}, hs::db_need(), {rec})")
                }
                _ => format!("hs::orm_delete({m}, hs::db_need(), {rec})"),
            });
        }

        // `User.create(..)`
        let (model, chain, _) = crate::orm::orm_chain(call)?;
        if !self.model_names.contains(&model) {
            return None;
        }
        let (method, args, _) = chain.first()?.clone();
        if !crate::orm::is_orm_write_method(&method) {
            return None;
        }
        if chain.len() > 1 {
            self.diags.push(crate::orm::err(
                chain[1].2,
                crate::catalog::ORM_BAD_QUERY,
                format!("`{method}` runs immediately, so nothing can be chained after it."),
                "End the write there.",
            ));
            return Some("hs::Val::nil()".to_string());
        }
        let plan = match crate::orm::parse_write(&model, &method, &args, false, schema) {
            Ok(p) => p,
            Err(mut d) => {
                self.diags.append(&mut d);
                return Some("hs::Val::nil()".to_string());
            }
        };
        let m = self.model_ref(&plan.model);
        let arg0 = args.first().map(|a| self.expr(a)).unwrap_or_else(|| "hs::Val::object({})".to_string());
        Some(match plan.op {
            crate::orm::WriteOp::Create(_) => format!("hs::orm_create({m}, hs::db_need(), {arg0})"),
            crate::orm::WriteOp::Upsert(_) => format!("hs::orm_upsert({m}, hs::db_need(), {arg0})"),
            crate::orm::WriteOp::DeleteKey => format!("hs::orm_delete_key({m}, hs::db_need(), {arg0})"),
            crate::orm::WriteOp::CreateMany => {
                format!("hs::orm_create_many({m}, hs::db_need(), {arg0})")
            }
            crate::orm::WriteOp::UpdateMany => {
                format!("hs::orm_update_many({m}, hs::db_need(), {arg0})")
            }
            crate::orm::WriteOp::DeleteMany => {
                format!("hs::orm_delete_many({m}, hs::db_need(), {arg0})")
            }
            _ => format!("hs::orm_find_or_create({m}, hs::db_need(), {arg0})"),
        })
    }

    /// The static model object generated for a model.
    fn model_ref(&self, model: &str) -> String {
        format!("&__hs_orm_{}", safe_id(model))
    }

    /// The compiler checks every column name here, against the schema, and
    /// emits the steps as a call chain. No SQL text is generated: the runtime
    /// assembles it, and the only strings that reach a statement are column
    /// names that came from the schema.
    fn orm_query(&mut self, call: &Expr) -> Option<String> {
        let schema = self.orm.clone()?;
        // A chain rooted at a record reads a relationship; one rooted at a
        // model queries the model. The root says which, and the value's
        // declared type is what makes it knowable.
        let base = crate::orm::orm_chain(call)
            .and_then(|(root, _, _)| self.record_types.get(&root).cloned());
        let plan = match crate::orm::parse_query_from(call, base.as_deref(), &schema) {
            Ok(p) => p,
            Err(mut d) => {
                self.diags.append(&mut d);
                return Some("hs::Val::nil()".to_string());
            }
        };
        let table = schema.table(&plan.model)?;

        // A model the ORM cannot map is reported here rather than as a
        // confusing failure at request time.
        if let Some(bad) = table.columns.iter().find(|c| c.unsupported_type.is_some()) {
            self.diags.push(crate::error::Diag::new(
                crate::error::ErrorKind::Type,
                format!(
                    "`{}` has no column type for the field `{} : {}`.",
                    plan.model, bad.name, bad.unsupported_type.as_deref().unwrap_or("")
                ),
                bad.span,
                "Use Int, Float, Bool, String, Time, UUID, or JSON for a column field.",
            )
            .with_code(crate::catalog::ORM_UNKNOWN_TYPE));
            return Some("hs::Val::nil()".to_string());
        }

        // Walk the raw chain a second time, in the same order `parse_query`
        // did, and generate the C++ for each step's value. One step is pushed
        // per builder method, so the two sequences line up exactly.
        let (_, chain, _) = crate::orm::orm_chain(call)?;
        // A relationship is the chain's first step but not a query step, so it
        // is consumed here; the rest line up with the plan one for one.
        let query_steps = if plan.join.is_some() { &chain[1..] } else { &chain[..] };
        // `Model.find_many(ids)` on its own reads through a dedicated call:
        // an `IN` query would return the database's order, while `find_many`
        // promises the keys' order. With anything else chained the builder
        // lowering below applies, in database order.
        if plan.join.is_none()
            && matches!(
                plan.steps.as_slice(),
                [crate::orm::QueryStep::Where { kind: crate::orm::WhereKind::In, .. }]
            )
            && query_steps.iter().map(|(name, _, _)| name.as_str()).collect::<Vec<_>>() == ["find_many"]
        {
            let args = &query_steps[0].1;
            let arg = args.first().map(|a| self.expr(a)).unwrap_or_else(|| "hs::Val::list({})".to_string());
            return Some(format!(
                "hs::orm_find_many(&{}, hs::db_need(), {arg})",
                Self::orm_sym(&plan.model)
            ));
        }
        let mut builder = query_steps.iter().filter(|(name, _, _)| !crate::orm::is_orm_terminal(name));
        let mut emitted: Vec<String> = Vec::new();
        for step in &plan.steps {
            let Some((name, args, _)) = builder.next() else { break };
            match step {
                crate::orm::QueryStep::Where { column, op, kind, .. } => {
                    let col = format!("{:?}", column);
                    let value = orm_value_cpp(self, name, args);
                    let (method, arg) = match kind {
                        crate::orm::WhereKind::Like => ("where_like", value),
                        crate::orm::WhereKind::In => ("where_in", value),
                        crate::orm::WhereKind::Compare => (
                            match op {
                                crate::orm::CompareOp::Eq => "where_eq",
                                crate::orm::CompareOp::Ne => "where_ne",
                                crate::orm::CompareOp::Lt => "where_lt",
                                crate::orm::CompareOp::Le => "where_le",
                                crate::orm::CompareOp::Gt => "where_gt",
                                crate::orm::CompareOp::Ge => "where_ge",
                            },
                            value,
                        ),
                    };
                    emitted.push(format!(".{method}({col}, {arg})"));
                }
                crate::orm::QueryStep::Limit(n) => emitted.push(format!(".limit({n})")),
                crate::orm::QueryStep::Offset(n) => emitted.push(format!(".offset({n})")),
                crate::orm::QueryStep::OrderBy { column, desc } => {
                    emitted.push(format!(".order_by({:?}, {desc})", column))
                }
            }
        }

        let terminal = match plan.terminal {
            crate::orm::Terminal::All => "all",
            crate::orm::Terminal::First => "first",
            crate::orm::Terminal::Count => "count",
            crate::orm::Terminal::Exists => "exists",
        };
        // A relationship's own predicate, named here from the schema and bound
        // to the parent record at run time.
        let rel = match (&plan.join, &plan.parent) {
            (Some(j), Some(p)) => {
                let rec = self.expr(&Expr::Ident(p.clone(), call.span()));
                format!(
                    ".of_record({}, {:?}, {:?}, {:?})",
                    rec, j.parent_col, j.target_col, j.join
                )
            }
            (Some(_), None) => {
                self.diags.push(crate::orm::err(
                    call.span(),
                    crate::catalog::ORM_UNKNOWN_RELATION,
                    "this relationship is read from something the compiler cannot name.",
                    "Bind the record to a variable first, e.g. `u <- User.find(1)`.",
                ));
                return Some("hs::Val::nil()".to_string());
            }
            _ => String::new(),
        };
        Some(format!(
            "hs::OrmQuery(&{}){rel}{}.{}(hs::db_need())",
            Self::orm_sym(&plan.model),
            emitted.join(""),
            terminal
        ))
    }

    // -----------------------------------------------------------------
    // Validation schemas (M5.1)
    // -----------------------------------------------------------------

    /// Lower one `model` declaration into a `hs::register_schema` call.
    /// Every field becomes an `hs::vf` entry carrying its declared type plus
    /// the constraint rules written in the source.
    fn emit_schema(&mut self, m: &ModelDef, idx: usize) {
        let mut entries = Vec::new();
        for f in &m.fields {
            let mut rules = Vec::new();
            let pos = f.positional();
            for a in &f.args {
                if let FieldArg::Constraint(k, v) = a {
                    if let Some(r) = rule_from_expr(k, v) {
                        rules.push(r);
                    }
                }
            }
            // Positional arguments: `Enum("a","b")` becomes an enum rule and
            // `List(Int)` an element-type rule; a bare `nullable` / `required`
            // is a flag rather than a value.
            for v in &pos {
                match v {
                    Expr::Str(s, _) => {
                        if f.ty == "Enum" {
                            rules.push(format!("hs::r_enum_one({:?})", s));
                        } else if s == "nullable" {
                            rules.push("hs::r_nullable()".to_string());
                        } else if s == "required" {
                            rules.push("hs::r_required()".to_string());
                        }
                    }
                    Expr::Ident(s, _) => {
                        if s == "nullable" {
                            rules.push("hs::r_nullable()".to_string());
                        } else if s == "required" {
                            rules.push("hs::r_required()".to_string());
                        } else if f.ty == "List" || f.ty == "Array" {
                            rules.push(format!("hs::r_items({:?})", s));
                        }
                    }
                    _ => {}
                }
            }
            for a in &f.attrs {
                match a.name.as_str() {
                    "nullable" | "optional" => rules.push("hs::r_nullable()".to_string()),
                    "required" => rules.push("hs::r_required()".to_string()),
                    _ => {}
                }
            }
            let strict = if m.strict { "true" } else { "false" };
            let _ = strict;
            let rule_list = if rules.is_empty() {
                String::new()
            } else {
                format!(", {{{}}}", rules.join(", "))
            };
            entries.push(format!(
                "hs::vf({:?}, {:?}{})",
                f.name, f.ty, rule_list
            ));
        }
        let fn_name = format!("register_schema_{idx}");
        self.wln(&format!("static bool schema_registered_{idx} = false;"));
        self.wln(&format!(
            "static void {fn_name}() {{ if (schema_registered_{idx}) return; schema_registered_{idx} = true;"
        ));
        self.ind += 1;
        self.wln(&format!(
            "hs::register_schema({:?}, {{{}}}, {});",
            m.name,
            entries.join(", "),
            m.strict
        ));
        self.ind -= 1;
        self.wln("}");
        self.blank();
    }

    fn emit_func(&mut self, f: &FunDef) {
        let mut args = String::new();
        for (i, p) in f.params.iter().enumerate() {
            if i > 0 {
                args.push_str(", ");
            }
            args.push_str(&format!("const hs::Val& {}", safe_id(&p.name)));
        }
        let line = format!("static hs::Val fn_{}({}) {{", safe_id(&f.name), args);
        self.wln(&line);
        self.ind += 1;
        self.stmts(&f.body, Ctx::Func);
        self.wln("return hs::Val::nil();");
        self.ind -= 1;
        self.wln("}");
        self.blank();
    }

    /// Reject a `protect` we cannot honour at runtime, rather than emitting a
    /// guard that silently does the wrong thing.
    fn check_protect(&mut self, d: &ProtectDef) -> Option<Diag> {
        if d.scheme != "jwt" {
            return Some(Diag::new(
                ErrorKind::Type,
                format!("unknown protect scheme `{}`", d.scheme),
                d.span,
                "The only scheme today is `jwt`.",
            )
            .with_code(crate::catalog::INVALID_PROTECT));
        }
        for p in &d.except {
            if !p.starts_with('/') {
                return Some(Diag::new(
                    ErrorKind::Type,
                    format!("exempt path `{p}` must start with `/`"),
                    d.span,
                    "Write `except = [\"/health\"]`.",
                )
                .with_code(crate::catalog::INVALID_PROTECT));
            }
        }
        None
    }

    fn emit_route(&mut self, r: &RouteDef, idx: usize, global_limits: &[String]) {
        // Locals do not outlive their body, so the record types start empty.
        self.record_types.clear();
        let line = format!("static hs::Response route_{idx}(const hs::Request& req) {{");
        self.wln(&line);
        self.ind += 1;
        self.wln("try {");
        self.ind += 1;
        // Global limits run before everything, including auth: shedding load
        // is cheaper than authenticating it.
        for check in global_limits {
            self.wln(&format!("(void)({check});"));
        }
        // `protect` runs before parameter binding, so a request without a
        // valid token never reaches body validation and never sees the field
        // names in the schema.
        if let Some(g) = self.protect.clone() {
            if !g.exempts(&r.path) {
                let secret = self.expr(&g.secret);
                self.wln("static const std::string __hs_secret = hs::to_text(");
                self.ind += 1;
                self.wln(&secret);
                self.ind -= 1;
                self.wln(").sv;");
                self.wln("hs::auth_guard(req, __hs_secret);");
            }
        }
        for p in &r.params {
            let nm = safe_id(&p.name);
            if p.is_body {
                self.wln(&format!("hs::Val {nm} = req.json();"));
                // M5.1: binding the body to a declared model validates it
                // before the handler body runs; a failure is HTTP 400.
                if let Some(ty) = &p.ty {
                    if self.model_names.contains(ty) {
                        self.wln(&format!(
                            "{{ hs::Response __vresp; if (!hs::validation_gate({:?}, {nm}, __vresp)) return __vresp; }}",
                            ty
                        ));
                    }
                }
            } else {

                self.wln(&format!("hs::Val {nm} = [&]() -> hs::Val {{"));
                self.ind += 1;
                self.wln(&format!(
                    "auto __it = req.params.find({});",
                    cstring(&p.name)
                ));
                self.wln("if (__it != req.params.end()) return hs::Val::text(std::string(__it->second));");
                self.wln(&format!("std::string __q = req.q({});", cstring(&p.name)));
                self.wln("if (!__q.empty()) return hs::Val::text(__q);");
                self.wln("return hs::Val::nil();");
                self.ind -= 1;
                self.wln("}();");
            }
        }
        self.stmts(&r.body, Ctx::Route);
        self.wln("return hs::Response::error(500, \"route reached the end without returning a value\");");
        self.ind -= 1;
        self.wln("} catch (const hs::HttpAbort& a) {");
        self.ind += 1;
        self.wln("return a.r;");
        self.ind -= 1;
        self.wln("} catch (const std::exception& e) {");
        self.ind += 1;
        self.wln("return hs::Response::error(500, std::string(\"route error: \") + e.what());");
        self.ind -= 1;
        self.wln("}");
        self.ind -= 1;
        self.wln("}");
        self.blank();
    }

    fn emit_middleware(&mut self, name: &str, body: &[Stmt], idx: usize) {
        let _ = name;
        self.record_types.clear();
        let line = format!(
            "static hs::Response mw_{idx}(const hs::Request& req, const std::function<hs::Response()>& next) {{"
        );
        self.wln(&line);
        self.ind += 1;
        self.wln("try {");
        self.ind += 1;
        self.stmts(body, Ctx::Middleware);
        self.wln("return next();");
        self.ind -= 1;
        self.wln("} catch (const hs::HttpAbort& a) {");
        self.ind += 1;
        self.wln("return a.r;");
        self.ind -= 1;
        self.wln("} catch (const std::exception& e) {");
        self.ind += 1;
        self.wln("return hs::Response::error(500, std::string(\"middleware error: \") + e.what());");
        self.ind -= 1;
        self.wln("}");
        self.ind -= 1;
        self.wln("}");
        self.blank();
    }

    fn emit_socket_registrations(&mut self, sockets: &[&SocketDef]) {
        for (k, s) in sockets.iter().enumerate() {
            let line = format!("static bool ws_registered_{k} = false;");
            self.wln(&line);
            let line = format!(
                "static void register_ws_{k}() {{ if (ws_registered_{k}) return; ws_registered_{k} = true;"
            );
            self.wln(&line);
            self.ind += 1;
            self.wln("hs::WsHandler __w;");
            let line = format!("__w.path = {};", cstring(&s.path));
            self.wln(&line);
            if let Some(body) = &s.connect {
                self.wln("__w.on_open = [](const hs::Request&) {");
                self.ind += 1;
                self.stmts(body, Ctx::Ws);
                self.ind -= 1;
                self.wln("};");
            }
            if let Some(body) = &s.message {
                self.wln("__w.on_msg = [](const std::string& __payload, bool) {");
                self.ind += 1;
                self.wln("hs::Val d = hs::Val::text(__payload);");
                self.stmts(body, Ctx::Ws);
                self.ind -= 1;
                self.wln("};");
            }
            if let Some(body) = &s.disconnect {
                self.wln("__w.on_close = []() {");
                self.ind += 1;
                self.stmts(body, Ctx::Ws);
                self.ind -= 1;
                self.wln("};");
            }
            self.wln("hs::ws_registry().push_back(std::move(__w));");
            self.ind -= 1;
            self.wln("}");
            self.blank();
        }
    }

    fn emit_test_registrations(&mut self, tests: &[&TestDef]) {
        for (k, t) in tests.iter().enumerate() {
            let line = format!("static bool test_registered_{k} = false;");
            self.wln(&line);
            let line = format!(
                "static void register_test_{k}() {{ if (test_registered_{k}) return; test_registered_{k} = true;"
            );
            self.wln(&line);
            self.ind += 1;
            let name = cstring(&t.name);
            let line = format!("hs::register_test({name}, []() {{");
            self.wln(&line);
            self.ind += 1;
            self.stmts(&t.body, Ctx::Test);
            self.ind -= 1;
            self.wln("});");
            self.ind -= 1;
            self.wln("}");
            self.blank();
        }
    }

    fn emit_main(&mut self, port: i64, routes: &[&RouteDef], nmw: usize, nws: usize, ntests: usize,
                 nmodels: usize, cache_decls: &[String], job_decls: &[(String, Vec<String>)],
                 top_enqueues: &[String], worker_regs: &[String], sched_regs: &[String],
                 email_templates: &[String]) {
        self.wln("int main(int argc, char** argv) {");
        self.ind += 1;
        self.wln("hs::set_args(argc, argv);");
        // Declared caches exist before the first request can ask for them.
        for decl in cache_decls {
            self.wln(&format!("(void)({decl});"));
        }
        // Templates register with the caches: before the first request, and
        // before any job could send one.
        for decl in email_templates {
            self.wln(&format!("(void)({decl});"));
        }
        // Job types register before anything enqueues; workers register their
        // handlers; top-level enqueues seed initial jobs. Registration is
        // idempotent, so `hard test` running the same binary is safe.
        for (name, params) in job_decls {
            let decl = self.declare_job_call(name, params);
            self.wln(&format!("(void)({decl});"));
        }
        for job in worker_regs {
            let f = format!("__worker_{job}");
            self.wln(&format!(
                "hs::queue_register_worker(hs::Val::text({job:?}), hs::Val::int_(4), fn_{});",
                safe_id(&f)
            ));
        }
        for reg in sched_regs {
            self.wln(&format!("(void)({reg});"));
        }
        for enq in top_enqueues {
            self.wln(&format!("(void)({enq});"));
        }
        self.wln("hs::Server app;");
        self.wln("hs::hs_set_server(&app);");
        for i in 0..nmw {
            let line = format!("app.before(mw_{i});");
            self.wln(&line);
        }
        for (i, r) in routes.iter().enumerate() {
            let line = format!(
                "app.handle({}, {}, route_{i});",
                cstring(&r.method),
                cstring(&r.path)
            );
            self.wln(&line);
        }
        let line = format!("app.port = {port};");
        self.wln(&line);
        self.blank();
        for i in 0..nmodels {
            let line = format!("register_schema_{i}();");
            self.wln(&line);
        }
        for i in 0..nws {
            let line = format!("register_ws_{i}();");
            self.wln(&line);
        }
        for i in 0..ntests {
            let line = format!("register_test_{i}();");
            self.wln(&line);
        }
        if ntests > 0 {
            self.wln("if (hs::want_test()) return app.test_main();");
        }
        self.wln("app.listen();");
        self.wln("return 0;");
        self.ind -= 1;
        self.wln("}");
    }

    // ---------- statements ----------

    fn stmts(&mut self, body: &[Stmt], ctx: Ctx) {
        let outer = self.ctx;
        self.ctx = ctx;
        for st in body {
            self.stmt(st, ctx);
        }
        self.ctx = outer;
    }

    fn stmt(&mut self, st: &Stmt, ctx: Ctx) {
        match st {
            Stmt::Bring(..) | Stmt::Import { .. } | Stmt::App(..) | Stmt::Model(..) => {}
            // `protect` is consumed by `run` before any body is emitted.
            Stmt::Protect(..) => {}
            Stmt::Func(_) | Stmt::Route(_) | Stmt::Middleware { .. } | Stmt::Socket(_)
            | Stmt::Test(_) => {}
            Stmt::Var(v) | Stmt::Const(v) => {
                let val = self.expr(&v.value);
                let line = format!("hs::Val {} = {val};", safe_id(&v.name));
                self.wln(&line);
                // A local bound from a query holds that model's records, which
                // is what lets `user.save()` know its table.
                if let Some(m) = self.expr_record_type(&v.value) {
                    self.record_types.insert(v.name.clone(), m);
                }
            }
            Stmt::If { cond, then_body, else_body, .. } => {
                let c = self.expr(cond);
                let line = format!("if ({c} .truthy()) {{");
                self.wln(&line);
                self.ind += 1;
                self.stmts(then_body, ctx);
                self.ind -= 1;
                if else_body.is_empty() {
                    self.wln("}");
                } else {
                    self.wln("} else {");
                    self.ind += 1;
                    self.stmts(else_body, ctx);
                    self.ind -= 1;
                    self.wln("}");
                }
            }
            Stmt::Loop { var, iter, body, .. } => {
                let v = safe_id(var);
                if let Expr::Range(lo, hi, _) = iter {
                    let l = self.expr(lo);
                    let h = self.expr(hi);
                    let line = format!(
                        "for (hs::Val {v} = {l}; {v}.iv <= {h} .iv; {v} = hs::op_add({v}, hs::Val::int_(1))) {{"
                    );
                    self.wln(&line);
                    self.ind += 1;
                    self.stmts(body, ctx);
                    self.ind -= 1;
                    self.wln("}");
                } else {
                    let it = self.expr(iter);
                    self.wln("{ /* loop over iterable */");
                    self.ind += 1;
                    let line = format!("hs::Val __it = {it};");
                    self.wln(&line);
                    self.wln("if (__it.is_arr()) {");
                    self.ind += 1;
                    self.wln("for (size_t __n = 0; __n < __it.arr.size(); __n++) {");
                    self.ind += 1;
                    let line = format!("hs::Val {v} = __it.atc(__n);");
                    self.wln(&line);
                    self.stmts(body, ctx);
                    self.ind -= 1;
                    self.wln("}");
                    self.ind -= 1;
                    self.wln("} else if (__it.is_str()) {");
                    self.ind += 1;
                    self.wln("for (size_t __n = 0; __n < __it.sv.size(); __n++) {");
                    self.ind += 1;
                    let line = format!("hs::Val {v} = hs::Val::text(std::string(1, __it.sv[__n]));");
                    self.wln(&line);
                    self.stmts(body, ctx);
                    self.ind -= 1;
                    self.wln("}");
                    self.ind -= 1;
                    self.wln("} else {");
                    self.ind += 1;
                    let line = format!("hs::Val {v} = __it;");
                    self.wln(&line);
                    self.stmts(body, ctx);
                    self.ind -= 1;
                    self.wln("}");
                    self.ind -= 1;
                    self.wln("}");
                }
            }
            Stmt::Return(expr, _) => match ctx {
                Ctx::Func => {
                    let val = self.expr(expr);
                    let line = format!("return {val};");
                    self.wln(&line);
                }
                Ctx::Route | Ctx::Middleware => {
                    let val = self.expr(expr);
                    let line = format!("return hs::hs_respond({val});");
                    self.wln(&line);
                }
                Ctx::Test | Ctx::Ws => self.wln("return;"),
            },
            Stmt::Race(exprs, _) => {
                let mut tasks = String::from("{ ");
                for (i, e) in exprs.iter().enumerate() {
                    if i > 0 {
                        tasks.push_str(", ");
                    }
                    let v = self.expr(e);
                    tasks.push_str(&format!("[]() -> hs::Val {{ return {v}; }}"));
                }
                tasks.push_str(" }");
                let line = format!("(void)hs::race_val({tasks});");
                self.wln(&line);
            }
            Stmt::Expect { lhs, op, rhs, span } => {
                let cond = {
                    let c = self.binwrap(op, lhs, rhs);
                    format!("{c} .bv")
                };
                let what = format!(
                    "expect {} {} {}",
                    self.src_expr(lhs),
                    op.symbol(),
                    self.src_expr(rhs)
                );
                let where_ = format!("{}:{}", self.path, span.line);
                let line = format!("hs::expect({cond}, {:?}, {:?});", what, where_);
                self.wln(&line);
            }
            Stmt::ExprStmt(Expr::Transaction { body, span }) => {
                // The guard's name comes from the source position, which is
                // unique per transaction and keeps the output deterministic.
                // The guard begins (or savepoints, when nested) here and
                // commits when the scope ends; an exception unwinds through
                // it into a rollback.
                let g = format!("__tx_{}_{}", span.line, span.col);
                self.wln(&format!("{{ hs::TxGuard {g}(hs::db_need());"));
                self.ind += 1;
                self.stmts(body, ctx);
                self.ind -= 1;
                self.wln("}");
            }
            Stmt::ExprStmt(e) => {
                let cpp = self.expr(e);
                if !cpp.is_empty() {
                    let line = format!("(void)({cpp});");
                    self.wln(&line);
                }
            }
            Stmt::Job(j) => {
                let params: Vec<String> = j.params.iter().map(|p| p.name.clone()).collect();
                let decl = self.declare_job_call(&j.name, &params);
                self.wln(&format!("(void)({decl});"));
            }
            Stmt::Schedule(s) => {
                let reg = self.schedule_register_call(s);
                self.wln(&format!("(void)({reg});"));
            }
        }
    }

    fn binwrap(&mut self, op: &BinOp, lhs: &Expr, rhs: &Expr) -> String {
        let l = self.expr(lhs);
        let r = self.expr(rhs);
        let f = match op {
            BinOp::Add => "hs::op_add",
            BinOp::Sub => "hs::op_sub",
            BinOp::Mul => "hs::op_mul",
            BinOp::Div => "hs::op_div",
            BinOp::Mod => "hs::op_mod",
            BinOp::Eq => "hs::op_eq",
            BinOp::Ne => "hs::op_ne",
            BinOp::Lt => "hs::op_lt",
            BinOp::Le => "hs::op_le",
            BinOp::Gt => "hs::op_gt",
            BinOp::Ge => "hs::op_ge",
            BinOp::And => "hs::op_and",
            BinOp::Or => "hs::op_or",
        };
        format!("{f}({l}, {r})")
    }

    // ---------- expressions ----------

    fn expr(&mut self, e: &Expr) -> String {
        match e {
            Expr::Int(n, _) => format!("hs::Val::int_({n})"),
            Expr::Float(f, _) => {
                if *f == (*f).trunc() {
                    format!("hs::Val::flt({f}.0)")
                } else {
                    format!("hs::Val::flt({f})")
                }
            }
            Expr::Str(s, _) => format!("hs::Val::text({:?})", s),
            Expr::Bool(b, _) => format!("hs::Val::boolean({b})"),
            Expr::List(items, _) => {
                let mut inner = String::new();
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        inner.push_str(", ");
                    }
                    inner.push_str(&self.expr(it));
                }
                format!("hs::Val::list(std::vector<hs::Val>{{ {inner} }})")
            }
            Expr::Obj(kvs, _) => {
                let mut inner = String::new();
                for (i, (k, v)) in kvs.iter().enumerate() {
                    if i > 0 {
                        inner.push_str(", ");
                    }
                    let v = self.expr(v);
                    inner.push_str(&format!("{{\"{}\", {v}}}", k));
                }
                format!("hs::Val::object({{ {inner} }})")
            }
            Expr::Ident(name, _) => safe_id(name),
            Expr::Member(base, name, sp) => {
                if let Expr::Ident(module, _) = base.as_ref() {
                    if MODULES.contains(&module.as_str()) {
                        if let Some(r) = self.builtin(module, name, &[], *sp) {
                            return r;
                        }
                    }
                }
                // A `has_one` read bare is the one relationship that needs no
                // terminal, so it is a query rather than a field read.
                if let Some(r) = self.orm_bare_rel(base, name, *sp) {
                    return r;
                }
                let b = self.expr(base);
                format!("hs::get_member({b}, {:?})", name)
            }
            Expr::Index(base, idx, _) => {
                let b = self.expr(base);
                let i = self.expr(idx);
                format!("hs::index_at({b}, {i})")
            }
            Expr::Call { callee, args, span } => self.call(callee, args, *span),
            Expr::Unary(op, inner, _) => {
                let i = self.expr(inner);
                match op {
                    UnOp::Neg => format!("hs::op_neg({i})"),
                    UnOp::Not => format!("hs::Val::boolean(!{i} .truthy())"),
                }
            }
            Expr::Binary(op, l, r, _) => self.binwrap(op, l, r),
            Expr::Range(_, _, sp) => {
                self.err("range used outside a loop", *sp);
                "hs::Val::nil()".to_string()
            }
            Expr::Match(scrut, arms, _) => {
                let s = self.expr(scrut);
                let mut out = String::from("([](const hs::Val& __m) -> hs::Val {");
                for arm in arms {
                    if let Some(val) = &arm.value {
                        let v = self.expr(val);
                        let b = self.expr(&arm.body);
                        out.push_str(&format!(" if (hs::op_eq(__m, {v}).bv) return {b};"));
                    } else {
                        let b = self.expr(&arm.body);
                        out.push_str(&format!(" return {b};"));
                    }
                }
                out.push_str(" return hs::Val::nil(); })(");
                out.push_str(&s);
                out.push(')');
                out
            }
            Expr::HttpCall { verb, path, body, .. } => {
                let b = match body {
                    Some(b) => self.expr(b),
                    None => "hs::Val::object({})".to_string(),
                };
                format!("hs::test_call({:?}, {:?}, {b})", verb, path)
            }
            // Unreachable in valid programs: typecheck rejects a transaction
            // in value position before codegen runs. The fallback keeps the
            // emitter total so an unchecked pipeline still produces valid C++.
            Expr::Transaction { body, .. } => {
                let inner = self.stmt_str(body, self.ctx);
                format!("([&]() -> hs::Val {{ hs::TxGuard __tx(hs::db_need());\n{inner} return hs::Val::nil(); }}())")
            }
        }
    }

    fn call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> String {
        // An ORM query is a chain rooted at a model, so it is recognized from
        // the call expression as a whole rather than from its callee. Both a
        // direct `User.all()` and a chained `User.where(..).limit(..)` arrive
        // here, so the hook sits above the module dispatch.
        if self.orm.is_some() {
            let whole = Expr::Call {
                callee: Box::new(callee.clone()),
                args: args.to_vec(),
                span,
            };
            // Gate on the root being a declared model or a known record, not
            // on the chain's shape: `json.parse(..)` has the same shape as
            // `User.all()` and must not be reported as a query against a
            // missing model. A record root is a relationship read.
            let is_query = crate::orm::chain_root(&whole)
                .map(|root| self.model_names.contains(&root) || self.record_types.contains_key(&root))
                .unwrap_or(false);
            // A write is tried first: `create` and friends are the same
            // shape as a query, and the query path would report them as
            // unknown methods.
            if let Some(r) = self.orm_write(&whole) {
                return r;
            }
            if is_query {
                if let Some(r) = self.orm_query(&whole) {
                    return r;
                }
            }
        }
        match callee {
            Expr::Member(base, name, msp) => {
                // A relationship read without a call is a `has_one`: one row,
                // and reading it should not need `.first()`. A `has_many` has
                // no single-row reading, so it says so rather than guessing.
                if let Some(r) = self.orm_bare_rel(base, name, *msp) {
                    return r;
                }
                // Database control calls: savepoints against the ambient
                // connection. Names are validated here so a bad call fails
                // with a diagnostic, not with a SQL error at runtime.
                if let Expr::Ident(root, _) = base.as_ref() {
                    if root == "db" && (name == "savepoint" || name == "rollback_to") {
                        return self.db_savepoint_call(name, args, span, *msp);
                    }
                }
                if let Expr::Ident(module, _) = base.as_ref() {
                    if MODULES.contains(&module.as_str()) {
                        // The callee's own span reads better in a diagnostic
                        // than the one at the opening paren.
                        if let Some(r) = self.builtin(module, name, args, *msp) {
                            return r;
                        }
                    }
                }
                let b = self.expr(base);
                format!("hs::get_member({b}, {:?})", name)
            }
            Expr::Ident(name, _) => {
                if name == "_" {
                    return "hs::Val::nil()".to_string();
                }
                let args_cpp: Vec<String> = args.iter().map(|a| self.expr(a)).collect();
                // User calc functions are emitted as global `static ... fn_x` in
                // the generated TU, so call them unqualified (`using namespace
                // hs;` is active at that point). `hs::fn_x` would not resolve.
                format!("fn_{}({})", safe_id(name), args_cpp.join(", "))
            }
            _ => {
                self.err("cannot call this expression", span);
                "hs::Val::nil()".to_string()
            }
        }
    }

    /// `db.savepoint("x")` / `db.rollback_to("x")` against the ambient
    /// connection. Typecheck validates every savepoint call before codegen
    /// runs, so a malformed one here means an unchecked pipeline: diagnose
    /// it rather than emitting a call the runtime would refuse.
    fn db_savepoint_call(&mut self, method: &str, args: &[Expr], span: Span, msp: Span) -> String {
        let _ = msp;
        if args.len() != 1 {
            self.diags.push(crate::orm::err(
                span,
                crate::catalog::TX_BAD_SAVEPOINT,
                format!("`db.{method}` takes exactly one savepoint name"),
                format!("Write `db.{method}(\"name\")`."),
            ));
            return "hs::Val::nil()".to_string();
        }
        let ok = match &args[0] {
            Expr::Str(s, _) => {
                let mut chars = s.chars();
                matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
                    && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
            }
            _ => false,
        };
        if !ok {
            self.diags.push(crate::orm::err(
                args[0].span(),
                crate::catalog::TX_BAD_SAVEPOINT,
                format!("`db.{method}` needs its name as a string literal"),
                "Write the name in quotes with letters, digits and underscores.".to_string(),
            ));
            return "hs::Val::nil()".to_string();
        }
        let arg = self.expr(&args[0]);
        format!("hs::db_{method}(hs::db_need(), {arg})")
    }

    /// A relationship read with no call on it, as in `author.latest`.
    fn orm_bare_rel(&mut self, base: &Expr, field: &str, sp: Span) -> Option<String> {
        let Expr::Ident(name, _) = base else { return None };
        let model = self.record_types.get(name)?.clone();
        let rel = self
            .orm
            .as_ref()?
            .relations_from(&model)
            .find(|r| r.field == field)?
            .clone();
        if rel.kind == crate::orm::RelKind::HasMany || rel.kind == crate::orm::RelKind::ManyToMany {
            self.diags.push(crate::orm::err(
                sp,
                crate::catalog::ORM_BAD_QUERY,
                format!("`{model}.{field}` is a list, so it has to be read with a terminal."),
                "Read every row with `.all()`, or one with `.first()`.",
            ));
            return Some("hs::Val::nil()".to_string());
        }
        // The same call a programmer would have written, so one path validates
        // it: `author.latest` is `author.latest.first()`.
        let call = Expr::Call {
            callee: Box::new(Expr::Member(
                Box::new(Expr::Member(Box::new(base.clone()), field.to_string(), sp)),
                "first".to_string(),
                sp,
            )),
            args: Vec::new(),
            span: sp,
        };
        self.orm_query(&call)
    }

    fn arg_at(&mut self, args: &[Expr], i: usize) -> String {
        if i < args.len() {
            self.expr(&args[i])
        } else {
            "hs::Val::nil()".to_string()
        }
    }

    fn ttx(&mut self, args: &[Expr], i: usize) -> String {
        let v = self.arg_at(args, i);
        format!("hs::to_text({v}).sv")
    }

    fn tin(&mut self, args: &[Expr], i: usize) -> String {
        let v = self.arg_at(args, i);
        format!("hs::to_int({v}).iv")
    }

    /// Builtins that read the in-flight `Request`. Using one outside a route or
    /// middleware would generate C++ referring to a `req` that does not exist,
    /// so it is a source-level error instead of a g++ failure.
    fn needs_request(&self, module: &str, name: &str) -> bool {
        matches!((module, name),
            ("http", "header")
                | ("http", "query")
                | ("auth", "bearer")
                | ("auth", "require")
                | ("auth", "optional")
                | ("session", _)
                | ("ratelimit", _))
    }

    fn builtin(&mut self, module: &str, name: &str, args: &[Expr], span: Span) -> Option<String> {
        if self.needs_request(module, name) && !matches!(self.ctx, Ctx::Route | Ctx::Middleware) {
            self.diags.push(
                Diag::new(
                    ErrorKind::Type,
                    format!("{module}.{name}() can only be called in a route"),
                    span,
                    "It reads the request being handled, which only exists there. \
                     Pass the value in, or move the call into a route body.",
                )
                .with_code(crate::catalog::REQUEST_UNAVAILABLE),
            );
            return None;
        }
        match (module, name) {
            // crypto
            (_, _) if module == "crypto" && name == "sha256" => {
                Some(format!("hs::Val::text(hs::sha256_hex({}))", self.ttx(args, 0)))
            }
            (_, _) if module == "crypto" && name == "sha256bin" => {
                Some(format!("hs::Val::text(hs::sha256_bin({}))", self.ttx(args, 0)))
            }
            (_, _) if module == "crypto" && name == "sha1" => {
                Some(format!("hs::Val::text(hs::sha1_hex({}))", self.ttx(args, 0)))
            }
            (_, _) if module == "crypto" && name == "md5" => {
                Some(format!("hs::Val::text(hs::md5_hex({}))", self.ttx(args, 0)))
            }
            (_, _) if module == "crypto" && name == "hmac" => {
                let a = self.ttx(args, 0);
                let b = self.ttx(args, 1);
                Some(format!("hs::Val::text(hs::hmac_sha256_bin({a}, {b}))"))
            }
            (_, _) if module == "crypto" && name == "base64" => {
                Some(format!("hs::Val::text(hs::base64_encode({}))", self.ttx(args, 0)))
            }
            (_, _) if module == "crypto" && name == "base64url" => {
                Some(format!(
                    "hs::Val::text(hs::base64_encode_url({}))",
                    self.ttx(args, 0)
                ))
            }
            (_, _) if module == "crypto" && name == "base64decode" => {
                Some(format!(
                    "hs::Val::text(hs::base64_decode({}))",
                    self.ttx(args, 0)
                ))
            }
            (_, _) if module == "crypto" && name == "uuid" => {
                Some("hs::Val::text(hs::random_uuid())".to_string())
            }
            (_, _) if module == "crypto" && name == "random_hex" => {
                Some(format!("hs::Val::text(hs::random_hex({}))", self.tin(args, 0)))
            }
            (_, _) if module == "crypto" && name == "token" => {
                Some(format!(
                    "hs::Val::text(hs::random_urlsafe({}))",
                    self.tin(args, 0)
                ))
            }
            // json
            (_, _) if module == "json" && name == "parse" => {
                Some(format!("hs::parse_json({})", self.ttx(args, 0)))
            }
            // cache (M6.1): one call per operation against the ambient cache.
            // Names and values are plain expressions; the runtime validates
            // shapes and reports misuse with a runtime error, not SQL.
            (_, _) if module == "cache" && name == "declare" => {
                let a = self.arg_at(args, 0);
                let b = self.arg_at(args, 1);
                Some(format!("hs::cache_declare({a}, {b})"))
            }
            (_, _) if module == "cache" && name == "get" => {
                Some(format!("hs::cache_get({})", self.arg_at(args, 0)))
            }
            (_, _) if module == "cache" && name == "set" => {
                let (a, b, c) = (self.arg_at(args, 0), self.arg_at(args, 1), self.arg_at(args, 2));
                Some(format!("hs::cache_set({a}, {b}, {c})"))
            }
            (_, _) if module == "cache" && name == "delete" => {
                Some(format!("hs::cache_delete({})", self.arg_at(args, 0)))
            }
            (_, _) if module == "cache" && name == "exists" => {
                Some(format!("hs::cache_exists({})", self.arg_at(args, 0)))
            }
            (_, _) if module == "cache" && (name == "increment" || name == "decrement") => {
                let a = self.arg_at(args, 0);
                // A missing step counts by one: `tin` on nil would count by
                // zero, which is a different operation wearing the same name.
                let b = if args.len() > 1 { self.tin(args, 1) } else { "1".to_string() };
                Some(format!("hs::cache_{name}({a}, hs::Val::int_({b}))"))
            }
            (_, _) if module == "cache" && name == "clear" => Some("hs::cache_clear()".to_string()),
            (_, _) if module == "cache" && name == "keys" => Some("hs::cache_keys()".to_string()),
            (_, _) if module == "cache" && name == "ttl" => {
                Some(format!("hs::cache_ttl({})", self.arg_at(args, 0)))
            }
            (_, _) if module == "cache" && name == "expire" => {
                let a = self.arg_at(args, 0);
                let b = self.arg_at(args, 1);
                Some(format!("hs::cache_expire({a}, {b})"))
            }
            // queue (M6.2): `queue Name(..)` desugars to this form, which
            // users may also write directly.
            (_, _) if module == "queue" && name == "enqueue" => {
                let (a, b, c) = (self.arg_at(args, 0), self.arg_at(args, 1), self.arg_at(args, 2));
                Some(format!("hs::queue_enqueue({a}, {b}, {c})"))
            }
            // ratelimit (M6.5): `limit ...` desugars to this form, which
            // users may also write directly. Missing keys become client IP;
            // the check throws the 429 itself.
            (_, _) if module == "ratelimit" && name == "check" => {
                let id = self.ttx(args, 0);
                let algo = self.tin(args, 1);
                let rate = self.tin(args, 2);
                let per = self.tin(args, 3);
                let key = if args.len() > 4 {
                    self.arg_at(args, 4)
                } else {
                    "hs::Val::nil()".to_string()
                };
                Some(format!(
                    "hs::limit_check_or_abort({id}, (int)({algo}), {rate}, {per}, hs::limit_key_or_ip({key}, req.peer_ip))"
                ))
            }
            (_, _) if module == "email" && name == "send" => {
                // The parser has already collected the named options into one
                // object; the runtime validates presence, types and the address
                // itself, so `email.send({..})` behaves the same when written
                // by hand.
                Some(format!("hs::email_send({})", self.arg_at(args, 0)))
            }
            (_, _) if module == "email" && name == "template" => {
                let (a, b) = (self.arg_at(args, 0), self.arg_at(args, 1));
                Some(format!("hs::email_template({a}, {b})"))
            }
            (_, _) if module == "json" && name == "stringify" => {
                Some(format!("hs::Val::text(hs::to_json({}))", self.arg_at(args, 0)))
            }
            (_, _) if module == "json" && name == "keys" => {
                let a = self.arg_at(args, 0);
                Some(format!(
                    "hs::Val::list([](const hs::Val& __o) {{ std::vector<hs::Val> __v; for (auto& __kv : __o.obj) __v.push_back(hs::Val::text(__kv.first)); return __v; }}({a}))"
                ))
            }
            // env
            (_, _) if module == "env" && name == "get" => {
                Some(format!("hs::Val::text(hs::env_get({}))", self.ttx(args, 0)))
            }
            // fs
            (_, _) if module == "fs" && name == "read" => {
                Some(format!("hs::Val::text(hs::fs_read({}))", self.ttx(args, 0)))
            }
            (_, _) if module == "fs" && name == "write" => {
                let a = self.ttx(args, 0);
                let b = self.ttx(args, 1);
                Some(Self::void_expr(format!("hs::fs_write({a}, {b})")))
            }
            (_, _) if module == "fs" && name == "append" => {
                let a = self.ttx(args, 0);
                let b = self.ttx(args, 1);
                Some(Self::void_expr(format!("hs::fs_append({a}, {b})")))
            }
            (_, _) if module == "fs" && name == "exists" => {
                Some(format!("hs::Val::boolean(hs::fs_exists({}))", self.ttx(args, 0)))
            }
            (_, _) if module == "fs" && name == "is_dir" => {
                Some(format!("hs::Val::boolean(hs::fs_is_dir({}))", self.ttx(args, 0)))
            }
            (_, _) if module == "fs" && name == "list" => {
                let a = self.ttx(args, 0);
                Some(format!(
                    "([](const std::vector<std::string>& __v) {{ std::vector<hs::Val> __r; for (auto& __s : __v) __r.push_back(hs::Val::text(__s)); return hs::Val::list(__r); }}(hs::fs_list({a})))"
                ))
            }
            (_, _) if module == "fs" && name == "remove" => {
                Some(Self::void_expr(format!("hs::fs_remove({})", self.ttx(args, 0))))
            }
            (_, _) if module == "fs" && name == "size" => {
                Some(format!("hs::Val::int_(hs::fs_size({}))", self.ttx(args, 0)))
            }
            // jwt
            (_, _) if module == "jwt" && name == "sign" => {
                let a = self.arg_at(args, 0);
                let b = self.ttx(args, 1);
                Some(format!("hs::Val::text(hs::jwt_sign({a}, {b}))"))
            }
            (_, _) if module == "jwt" && name == "verify" => {
                let a = self.ttx(args, 0);
                let b = self.ttx(args, 1);
                Some(format!("hs::Val::boolean(hs::jwt_verify({a}, {b}))"))
            }
            // time
            (_, _) if module == "time" && name == "now" => {
                Some("hs::Val::int_(hs::unix_ms())".to_string())
            }
            (_, _) if module == "time" && name == "iso" => {
                Some("hs::Val::text(hs::time_iso())".to_string())
            }
            (_, _) if module == "time" && name == "sleep" => {
                let a = self.arg_at(args, 0);
                Some(Self::void_expr(format!("hs::sleep_sec({a} .num())")))
            }
            // runtime
            (_, _) if module == "runtime" && name == "args" => {
                Some("hs::args_list()".to_string())
            }
            (_, _) if module == "runtime" && name == "argc" => {
                Some("hs::Val::int_(hs::argc())".to_string())
            }
            (_, _) if module == "runtime" && name == "argv" => {
                Some(format!("hs::Val::text(hs::argv({}))", self.tin(args, 0)))
            }
            (_, _) if module == "runtime" && name == "print" => {
                Some(format!("hs::v_print({})", self.arg_at(args, 0)))
            }
            (_, _) if module == "runtime" && name == "pid" => {
                Some("hs::Val::text(hs::pid())".to_string())
            }
            (_, _) if module == "runtime" && name == "hostname" => {
                Some("hs::Val::text(hs::hostname())".to_string())
            }
            (_, _) if module == "runtime" && name == "platform" => {
                Some("hs::Val::text(hs::platform())".to_string())
            }
            (_, _) if module == "runtime" && name == "cpus" => {
                Some("hs::Val::int_(hs::cpus())".to_string())
            }
            // websocket
            (_, _) if module == "websocket" && name == "broadcast" => {
                Some(Self::void_expr(format!("hs::ws_broadcast({})", self.ttx(args, 0))))
            }
            (_, _) if module == "websocket" && name == "broadcast_room" => {
                let a = self.ttx(args, 0);
                let b = self.ttx(args, 1);
                Some(Self::void_expr(format!("hs::ws_broadcast_room({a}, {b})")))
            }
            (_, _) if module == "websocket" && name == "reply" => {
                Some(Self::void_expr(format!("hs::ws_reply({})", self.ttx(args, 0))))
            }
            (_, _) if module == "websocket" && name == "join" => {
                Some(Self::void_expr(format!(
                    "hs::ws_join(hs::ws_self(), {})",
                    self.ttx(args, 0)
                )))
            }
            (_, _) if module == "websocket" && name == "leave" => {
                Some(Self::void_expr("hs::ws_leave(hs::ws_self())".to_string()))
            }
            (_, _) if module == "websocket" && name == "self" => {
                Some("hs::Val::text(hs::ws_self())".to_string())
            }
            // postgres
            (_, _) if module == "postgres" && name == "connect" => {
                Some(format!("hs::Val::int_(hs::pg_connect({}))", self.ttx(args, 0)))
            }
            (_, _) if module == "postgres" && name == "query" => {
                let fd = self.tin(args, 0);
                let sql = self.ttx(args, 1);
                Some(format!("hs::pg_query({fd}, {sql})"))
            }
            // validation (M5.1)
            (_, _) if module == "validation" && name == "check" => {
                let a = self.ttx(args, 0);
                let b = self.arg_at(args, 1);
                Some(format!("hs::validate({a}, {b}).to_val()"))
            }
            (_, _) if module == "validation" && name == "valid" => {
                let a = self.ttx(args, 0);
                let b = self.arg_at(args, 1);
                Some(format!("hs::Val::boolean(hs::valid({a}, {b}))"))
            }
            (_, _) if module == "validation" && name == "errors" => {
                let a = self.arg_at(args, 0);
                Some(format!(
                    "([&](const hs::Val& __r) {{ const hs::Val* __e = __r.find(\"errors\"); return __e ? *__e : hs::Val::list({{}}); }})({a})"
                ))
            }
            (_, _) if module == "validation" && name == "reject" => {
                let a = self.arg_at(args, 0);
                Some(Self::void_expr(format!("hs::validation_reject({a})")))
            }
            (_, _) if module == "validation" && name == "email" => {
                let a = self.ttx(args, 0);
                Some(format!("hs::Val::boolean(hs::v_nonempty({a}) && hs::v_is_email({a}))"))
            }
            (_, _) if module == "validation" && name == "url" => {
                let a = self.ttx(args, 0);
                Some(format!("hs::Val::boolean(hs::v_nonempty({a}) && hs::v_is_url({a}))"))
            }
            (_, _) if module == "validation" && name == "uuid" => {
                let a = self.ttx(args, 0);
                Some(format!("hs::Val::boolean(hs::v_nonempty({a}) && hs::v_is_uuid({a}))"))
            }
            (_, _) if module == "validation" && name == "ip" => {
                let a = self.ttx(args, 0);
                Some(format!("hs::Val::boolean(hs::v_nonempty({a}) && hs::v_is_ip({a}))"))
            }
            (_, _) if module == "validation" && name == "phone" => {
                let a = self.ttx(args, 0);
                Some(format!("hs::Val::boolean(hs::v_nonempty({a}) && hs::v_is_phone({a}))"))
            }
            (_, _) if module == "validation" && name == "regex" => {
                let a = self.ttx(args, 0);
                let b = self.ttx(args, 1);
                Some(format!("hs::Val::boolean(hs::v_regex_test(hs::v_regex_id({a}), {b}))"))
            }
            (_, _) if module == "validation" && name == "min" => {
                let a = self.arg_at(args, 0);
                let b = self.tin(args, 1);
                Some(format!("hs::Val::boolean(hs::v_min({a}, (double){b}))"))
            }
            (_, _) if module == "validation" && name == "max" => {
                let a = self.arg_at(args, 0);
                let b = self.tin(args, 1);
                Some(format!("hs::Val::boolean(hs::v_max({a}, (double){b}))"))
            }
            (_, _) if module == "validation" && name == "length" => {
                let a = self.arg_at(args, 0);
                let b = self.tin(args, 1);
                let c = self.tin(args, 2);
                Some(format!("hs::Val::boolean(hs::v_length({a}, (double){b}, (double){c}))"))
            }
            (_, _) if module == "validation" && name == "between" => {
                let a = self.arg_at(args, 0);
                let b = self.tin(args, 1);
                let c = self.tin(args, 2);
                Some(format!("hs::Val::boolean(hs::v_between({a}, (double){b}, (double){c}))"))
            }
            (_, _) if module == "validation" && (name == "one_of" || name == "oneof") => {
                let a = self.arg_at(args, 0);
                let b = self.ttx(args, 1);
                Some(format!("hs::Val::boolean(hs::v_one_of({a}, {b}))"))
            }
            (_, _) if module == "validation" && name == "is_null" => {
                let a = self.arg_at(args, 0);
                Some(format!("hs::Val::boolean(hs::v_is_null({a}))"))
            }
            // auth (M5.2)
            //
            // `auth.issue` mints a token; everything else reads the request.
            // `auth.user()` returns the claims the `protect` guard published
            // for the request in flight, so a handler never has to re-verify
            // a token it already proved valid.
            (_, _) if module == "auth" && name == "issue" => {
                let claims = self.arg_at(args, 0);
                let secret = self.ttx(args, 1);
                let ttl = match args.get(2) {
                    Some(_) => format!("(double){}", self.tin(args, 2)),
                    None => "3600.0".to_string(),
                };
                let iss = match args.get(3) {
                    Some(_) => self.ttx(args, 3),
                    None => "std::string()".to_string(),
                };
                Some(format!(
                    "hs::Val::text(hs::auth_issue({claims}, {secret}, {ttl}, {iss}))"
                ))
            }
            (_, _) if module == "auth" && (name == "verify" || name == "check") => {
                let token = self.ttx(args, 0);
                let secret = self.ttx(args, 1);
                Some(format!(
                    "hs::Val::boolean(hs::auth_check({token}, {secret}).ok)"
                ))
            }
            (_, _) if module == "auth" && name == "claims" => {
                // Decode without verifying. Useful for a logout endpoint that
                // wants to read `jti` off a token it is about to discard, and
                // for tests; never use it to make an authorization decision.
                let token = self.ttx(args, 0);
                Some(format!("hs::jwt_claims_unchecked({token})"))
            }
            (_, _) if module == "auth" && name == "reason" => {
                let a = self.arg_at(args, 0);
                let b = self.ttx(args, 1);
                Some(format!("hs::Val::text(hs::auth_check({a}, {b}).reason)"))
            }
            (_, _) if module == "auth" && name == "user" => {
                Some("hs::auth_user()".to_string())
            }
            (_, _) if module == "auth" && name == "bearer" => {
                Some("hs::Val::text(hs::auth_bearer(req))".to_string())
            }
            (_, _) if module == "auth" && name == "require" => {
                let secret = self.ttx(args, 0);
                let claim = match args.get(1) {
                    Some(_) => self.ttx(args, 1),
                    None => "std::string()".to_string(),
                };
                Some(Self::void_expr(format!(
                    "hs::auth_require(req, {secret}, {claim})"
                )))
            }
            (_, _) if module == "auth" && name == "optional" => {
                let secret = self.ttx(args, 0);
                Some(format!("hs::Val::boolean(hs::auth_optional(req, {secret}))"))
            }
            // session (M6.4): cookie sessions against the in-flight request.
            // Every call threads `req` for cookie input; cookies leave on the
            // response through the drain, never through a return value.
            (_, _) if module == "session" && name == "start" => {
                Some(format!("hs::session_start(req, {})", self.arg_at(args, 0)))
            }
            (_, _) if module == "session" && name == "user" => {
                Some("hs::session_user(req)".to_string())
            }
            (_, _) if module == "session" && name == "destroy" => {
                Some("hs::session_destroy(req)".to_string())
            }
            (_, _) if module == "session" && name == "flash" => {
                let a = self.arg_at(args, 0);
                let b = self.arg_at(args, 1);
                Some(format!("hs::session_flash(req, {a}, {b})"))
            }
            (_, _) if module == "session" && name == "csrf" => {
                Some("hs::session_csrf(req)".to_string())
            }
            (_, _) if module == "session" && name == "csrf_valid" => {
                Some(format!("hs::session_csrf_valid(req, {})", self.arg_at(args, 0)))
            }
            (_, _) if module == "auth" && name == "reject" => {
                let status = self.arg_at(args, 0);
                let msg = match args.get(1) {
                    Some(_) => self.ttx(args, 1),
                    None => "std::string()".to_string(),
                };
                Some(Self::void_expr(format!(
                    "hs::auth_reject((int)hs::to_int({status}).iv, {msg})"
                )))
            }
            (_, _) if module == "auth" && name == "hash" => {
                Some(format!("hs::Val::text(hs::auth_hash_password({}))", self.ttx(args, 0)))
            }
            (_, _) if module == "auth" && name == "check_password" => {
                Some(format!(
                    "hs::Val::boolean(hs::auth_verify_password({}, {}))",
                    self.ttx(args, 0),
                    self.ttx(args, 1)
                ))
            }
            // http: request/response access
            (_, _) if module == "http" && name == "header" => {
                Some(format!(
                    "hs::Val::text(req.header({}))",
                    self.ttx(args, 0)
                ))
            }
            (_, _) if module == "http" && name == "query" => {
                Some(format!("hs::Val::text(req.q({}))", self.ttx(args, 0)))
            }
            // http: early response with an arbitrary status
            (_, _) if module == "http" && name == "abort" => {
                let st = self.arg_at(args, 0);
                let body = self.arg_at(args, 1);
                Some(Self::void_expr(format!(
                    "hs::abort_json((int)hs::to_int({st}).iv, \"error\", hs::to_text({body}).sv)"
                )))
            }
            _ => {
                let _ = span;
                self.err(format!("unknown builtin `{module}.{name}`"), span);
                Some("hs::Val::nil()".to_string())
            }
        }
    }

    fn void_expr(body: String) -> String {
        format!("([&]() -> hs::Val {{ {body}; return hs::Val::nil(); }}())")
    }

    // ---------- source-ish rendering for diagnostics / expect ----------

    fn src_expr(&self, e: &Expr) -> String {
        e.render()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Func,
    Route,
    Middleware,
    Ws,
    Test,
}