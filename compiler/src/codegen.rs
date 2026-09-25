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

        for st in &prog.stmts {
            match st {
                Stmt::Var(v) | Stmt::Const(v) => {
                    let val = self.expr(&v.value);
                    let n = safe_id(&v.name);
                    let line = format!("hs::Val {n} = {val};");
                    self.wln(&line);
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
                _ => {}
            }
        }

        // Model names drive automatic request-body validation and `protect`
        // decides which routes get the guard, so both have to be known before
        // any route is emitted.
        self.model_names = models.iter().map(|m| m.name.clone()).collect();
        self.protect = protect.cloned();

        for f in &funcs {
            self.emit_func(f);
        }
        for (i, m) in models.iter().enumerate() {
            self.emit_schema(m, i);
        }
        for (i, r) in routes.iter().enumerate() {
            self.emit_route(r, i);
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
        );
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

    fn emit_route(&mut self, r: &RouteDef, idx: usize) {
        let line = format!("static hs::Response route_{idx}(const hs::Request& req) {{");
        self.wln(&line);
        self.ind += 1;
        self.wln("try {");
        self.ind += 1;
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
                 nmodels: usize) {
        self.wln("int main(int argc, char** argv) {");
        self.ind += 1;
        self.wln("hs::set_args(argc, argv);");
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
            Stmt::Var(v) => {
                let val = self.expr(&v.value);
                let line = format!("hs::Val {} = {val};", safe_id(&v.name));
                self.wln(&line);
            }
            Stmt::Const(v) => {
                let val = self.expr(&v.value);
                let line = format!("hs::Val {} = {val};", safe_id(&v.name));
                self.wln(&line);
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
            Stmt::ExprStmt(e) => {
                let cpp = self.expr(e);
                if !cpp.is_empty() {
                    let line = format!("(void)({cpp});");
                    self.wln(&line);
                }
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
        }
    }

    fn call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> String {
        match callee {
            Expr::Member(base, name, msp) => {
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
                | ("auth", "optional"))
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