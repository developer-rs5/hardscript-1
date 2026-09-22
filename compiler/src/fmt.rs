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
            Stmt::App(p, _) => self.line(d, &format!("app @{p}")),
            Stmt::Model(m) => {
                self.line(d, &format!("model {} [", m.name));
                for f in &m.fields {
                    let attr: Vec<String> = f.attrs.iter().map(|a| format!(" #{a}")).collect();
                    self.line(d + 1, &format!("{} => {} {}", f.name, f.ty, attr.concat()));
                }
                self.line(d, "]");
            }
            Stmt::Route(r) => {
                self.line(d, &format!("{} {:?} :: {{", r.method, r.path));
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
                self.line(d, &format!("calc fn {}({}){} {{", f.name, params.join(", "), ret));
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
        match m {
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
        }
    }
}