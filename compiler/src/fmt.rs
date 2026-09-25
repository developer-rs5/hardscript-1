//! Canonical source formatter. Re-emits a program with consistent
//! indentation and spacing from the AST.

use crate::ast::*;

pub fn format(prog: &Program) -> String {
    let mut f = Fmt::default();
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
            Stmt::ExprStmt(e) => self.line(d, &format!("{}", self.expr(e, 0))),
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
            Expr::Call { callee, args, .. } => format!(
                "{}({})",
                self.expr(callee, 0),
                args.iter().map(|a| self.expr(a, 0)).collect::<Vec<_>>().join(", ")
            ),
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
}
