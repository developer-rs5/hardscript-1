//! Compiler escape analysis (foundation).
//!
//! ms2.5 delivers the analysis *only*: every local binding inside a
//! function-like scope (route, socket handler, `func`, middleware, test) is
//! classified into one of three buckets:
//!
//! * [`EscClass::Stack`] — provably does not escape the local scope; a
//!   future pass may safely stack-allocate it.
//! * [`EscClass::Escape`] — may leave the scope (returned, stored into a
//!   container, handed to a call, launched in a race) or is reassigned; it
//!   must stay heap- or frame-owned.
//! * [`EscClass::Immutable`] — a `const` binding that is declared exactly
//!   once and therefore shares its value safely.
//!
//! The analysis is deliberately conservative: a binding only ever moves
//! *down* from `escaping` to `stack` when every single use is a plain local
//! read (arithmetic operand, comparison, match scrutinee, loop range, index
//! base). Any structurally containing position — list/object element, call
//! callee or argument, return value, http call payload, race task — marks the
//! referenced binding escaping. Codegen is untouched; callers consume the
//! [`EscapeReport`] (e.g. `hard build` in `HARD_ESCAPE_REPORT` mode) without
//! changing the emitted C++.

use crate::ast::*;
use std::collections::{HashMap, HashSet};

/// Escape classification for a single local binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscClass {
    Stack,
    Escape,
    Immutable,
}

impl EscClass {
    pub fn label(self) -> &'static str {
        match self {
            EscClass::Stack => "stack",
            EscClass::Escape => "escaping",
            EscClass::Immutable => "immutable",
        }
    }
}

/// One classified binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub scope: String,
    pub var: String,
    pub class: EscClass,
}

/// The full, deterministic report for a program.
///
/// Entries are in binder order within each scope, and scopes appear in
/// program order, so two runs over the same source produce identical lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EscapeReport {
    pub entries: Vec<Entry>,
}

impl EscapeReport {
    /// Render the report one binding per line, `summary:` line last.
    pub fn to_lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .entries
            .iter()
            .map(|e| format!("{}: {} = {}", e.scope, e.var, e.class.label()))
            .collect();
        let c = |k: EscClass| self.entries.iter().filter(|e| e.class == k).count();
        out.push(format!(
            "summary: {} stack, {} escaping, {} immutable",
            c(EscClass::Stack),
            c(EscClass::Escape),
            c(EscClass::Immutable)
        ));
        out
    }
}

/// Classify every local binding in the program.
pub fn analyze(p: &Program) -> EscapeReport {
    let mut rep = EscapeReport::default();

    let top: Vec<Stmt> = p
        .stmts
        .iter()
        .filter(|s| matches!(s, Stmt::Var(_) | Stmt::Const(_)))
        .cloned()
        .collect();
    if !top.is_empty() {
        analyze_block(&top, "top", &mut rep.entries);
    }

    for s in &p.stmts {
        match s {
            Stmt::Route(r) => {
                let label = format!("route {} {}", r.method, r.path);
                analyze_block(&r.body, &label, &mut rep.entries);
            }
            Stmt::Socket(sk) => {
                analyze_block_opt(&sk.connect, &format!("ws {} connect", sk.path), &mut rep.entries);
                analyze_block_opt(&sk.message, &format!("ws {} message", sk.path), &mut rep.entries);
                analyze_block_opt(&sk.disconnect, &format!("ws {} disconnect", sk.path), &mut rep.entries);
            }
            Stmt::Func(f) => {
                let label = format!("func {}", f.name);
                analyze_block(&f.body, &label, &mut rep.entries);
            }
            Stmt::Middleware { name, body, .. } => {
                let label = format!("middleware {}", name);
                analyze_block(body, &label, &mut rep.entries);
            }
            Stmt::Test(t) => {
                let label = format!("test {}", t.name);
                analyze_block(&t.body, &label, &mut rep.entries);
            }
            _ => {}
        }
    }
    rep
}

// ---------------------------------------------------------------------------
// Scope analysis
// ---------------------------------------------------------------------------

/// Mutable state collected while walking a single scope.
#[derive(Default)]
struct Scope {
    names: Vec<String>,
    first_kind: HashMap<String, bool>, // true == `const`
    rebound: HashSet<String>,
    escapes: HashSet<String>,
}

fn analyze_block_opt(body: &Option<Vec<Stmt>>, label: &str, out: &mut Vec<Entry>) {
    if let Some(b) = body {
        analyze_block(b, label, out);
    }
}

fn analyze_block(stmts: &[Stmt], label: &str, out: &mut Vec<Entry>) {
    let mut sc = Scope::default();
    for s in stmts {
        scan_stmt(s, &mut sc);
    }
    for name in &sc.names {
        let class = if sc.escapes.contains(name) {
            EscClass::Escape
        } else if sc.first_kind.get(name) == Some(&true) && !sc.rebound.contains(name) {
            EscClass::Immutable
        } else {
            EscClass::Stack
        };
        out.push(Entry {
            scope: label.to_string(),
            var: name.clone(),
            class,
        });
    }
}

fn declare(sc: &mut Scope, name: &str, is_const: bool) {
    if sc.first_kind.contains_key(name) {
        sc.rebound.insert(name.to_string());
    } else {
        sc.first_kind.insert(name.to_string(), is_const);
        sc.names.push(name.to_string());
    }
}

fn scan_stmt(s: &Stmt, sc: &mut Scope) {
    match s {
        Stmt::Var(v) => {
            declare(sc, &v.name, false);
            scan_expr(&v.value, false, sc);
        }
        Stmt::Const(v) => {
            declare(sc, &v.name, true);
            scan_expr(&v.value, false, sc);
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            scan_expr(cond, false, sc);
            for b in then_body.iter().chain(else_body.iter()) {
                scan_stmt(b, sc);
            }
        }
        Stmt::Loop { var, iter, body, .. } => {
            declare(sc, var, false);
            scan_expr(iter, false, sc);
            for b in body {
                scan_stmt(b, sc);
            }
        }
        Stmt::Return(e, _) => scan_expr(e, true, sc),
        Stmt::Race(arms, _) => {
            for a in arms {
                scan_expr(a, true, sc);
            }
        }
        Stmt::Expect { lhs, op, rhs, span } => {
            let _ = op;
            let _ = span;
            scan_expr(lhs, false, sc);
            scan_expr(rhs, false, sc);
        }
        Stmt::ExprStmt(e) => scan_expr(e, false, sc),
        Stmt::Job(_) => {}
        _ => {}
    }
}

/// Walk an expression recording uses of local bindings.
///
/// `esc` is true when this expression sits in an *escaping* position: its
/// value may outlive the current scope (return value, container element,
/// call callee or argument, race task, http payload...). Operator operands
/// and reads are `false`: they only read a value's current contents.
fn scan_expr(e: &Expr, esc: bool, sc: &mut Scope) {
    match e {
        Expr::Ident(name, _) => {
            if esc {
                sc.escapes.insert(name.clone());
            }
        }
        Expr::Int(..) | Expr::Float(..) | Expr::Str(..) | Expr::Bool(..) => {}
        Expr::List(items, _) => {
            for it in items {
                scan_expr(it, true, sc);
            }
        }
        Expr::Obj(kvs, _) => {
            for (_, v) in kvs {
                scan_expr(v, true, sc);
            }
        }
        Expr::Member(b, _, _) => scan_expr(b, esc, sc),
        Expr::Index(b, i, _) => {
            scan_expr(b, false, sc);
            scan_expr(i, false, sc);
        }
        Expr::Call { callee, args, .. } => {
            scan_expr(callee, true, sc);
            for a in args {
                scan_expr(a, true, sc);
            }
        }
        Expr::Unary(_, x, _) => scan_expr(x, false, sc),
        Expr::Binary(_, l, r, _) => {
            scan_expr(l, false, sc);
            scan_expr(r, false, sc);
        }
        Expr::Range(lo, hi, _) => {
            scan_expr(lo, false, sc);
            scan_expr(hi, false, sc);
        }
        Expr::HttpCall { body, .. } => {
            if let Some(b) = body {
                scan_expr(b, true, sc);
            }
        }
        Expr::Match(scrut, arms, _) => {
            scan_expr(scrut, false, sc);
            for a in arms {
                if let Some(v) = &a.value {
                    scan_expr(v, false, sc);
                }
                scan_expr(&a.body, esc, sc);
            }
        }
        // A transaction body is ordinary code in a scope of its own: names
        // bound inside stay inside, and the block itself evaluates to nothing.
        Expr::Transaction { body, .. } => {
            for s in body {
                scan_stmt(s, sc);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(src: &str) -> EscapeReport {
        let (toks, _) = crate::lex(src);
        let p = crate::Parser::new(toks.unwrap()).parse_program().unwrap();
        analyze(&p)
    }

    fn labels(rep: &EscapeReport) -> Vec<String> {
        rep.entries
            .iter()
            .map(|e| format!("{} {}={}", e.scope, e.var, e.class.label()))
            .collect()
    }

    #[test]
    fn scalar_reads_stay_stack() {
        let rep = report(
            "app @0\n\
             GET \"/\" :: {\n\
             \x20   a <- 1 + 2\n\
             \x20   b ::= 10\n\
             \x20   c <- a * b\n\
             \x20   <- c\n\
             }\n",
        );
        assert_eq!(
            labels(&rep),
            [
                "route GET / a=stack",
                "route GET / b=immutable",
                "route GET / c=escaping"
            ]
        );
    }

    #[test]
    fn containers_mark_escaping() {
        let rep = report(
            "app @0\n\
             GET \"/\" :: {\n\
             \x20   s <- 40\n\
             \x20   items <- [s, 2]\n\
             \x20   tagged <- { \"k\": items }\n\
             \x20   <- tagged\n\
             }\n",
        );
        // s (list element), items (object value), tagged (returned) escape.
        assert_eq!(labels(&rep), [
            "route GET / s=escaping",
            "route GET / items=escaping",
            "route GET / tagged=escaping"
        ]);
    }

    #[test]
    fn race_tasks_mark_escaping() {
        // A local handed to a race task escapes the calling scope even though
        // the codegen closure capture for locals is not wired up yet
        // (ms2.6 follow-up); the analysis still classifies it correctly.
        let rep = report(
            "app @0\n\
             GET \"/\" :: {\n\
             \x20   keep <- [1]\n\
             \x20   tag ::= \"x\"\n\
             \x20   race [ keep, tag ]\n\
             \x20   <- 0\n\
             }\n",
        );
        assert_eq!(labels(&rep), [
            "route GET / keep=escaping",
            "route GET / tag=escaping"
        ]);
    }

    #[test]
    fn returned_binding_escapes() {
        // `c ::= 5` returned: the value leaves the scope, and the analysis is
        // deliberately conservative — a returned binding is escaping, never
        // marked for stack allocation.
        let rep = report(
            "app @0\n\
             GET \"/\" :: {\n\
             \x20   c ::= 5\n\
             \x20   <- c\n\
             }\n",
        );
        assert_eq!(labels(&rep), ["route GET / c=escaping"]);
    }

    #[test]
    fn member_read_keeps_base_local() {
        let rep = report(
            "app @0\n\
             GET \"/\" :: {\n\
             \x20   o ::= { \"a\": 1 }\n\
             \x20   v <- o.a\n\
             \x20   <- v\n\
             }\n",
        );
        // o read through `.a` only -> stack; but it is a const => immutable.
        // v is returned => escaping.
        assert_eq!(labels(&rep), [
            "route GET / o=immutable",
            "route GET / v=escaping"
        ]);
    }
}