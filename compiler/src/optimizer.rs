//! AST optimization passes.
//!
//! v0.1 keeps the optimizer intentionally small and strictly
//! semantics-preserving: it only does work that cannot change observable
//! behavior (removing statements that can never execute and folding
//! constant arithmetic). The pass runner is the extension point for more
//! aggressive passes (inlining, constant propagation, ...) in later versions.
//!
//! Run between parsing and type checking via [`run`].

use crate::ast::*;

/// Run every optimization pass over the program.
///
/// Returns the number of AST nodes that were rewritten or removed.
pub fn run(p: &mut Program) -> usize {
    let mut count = 0;
    for st in p.stmts.iter_mut() {
        count += opt_stmt(st);
    }
    count
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Number of statements removed that could never execute (dead code).
fn truncate_after_terminator(block: &mut Vec<Stmt>) -> usize {
    let mut cut = block.len();
    for (i, s) in block.iter().enumerate() {
        if matches!(s, Stmt::Return(..) | Stmt::Race(..)) {
            cut = i + 1;
            break;
        }
    }
    let removed = block.len() - cut;
    block.truncate(cut);
    removed
}

fn fold_binary(op: BinOp, l: &Expr, r: &Expr) -> Option<Expr> {
    use BinOp::*;
    let sp = l.span();
    let ints = |a: &Expr| if let Expr::Int(v, _) = a { Some(*v) } else { None };
    let flts = |a: &Expr| if let Expr::Float(v, _) = a { Some(*v) } else { None };
    match op {
        Add => {
            if let (Some(a), Some(b)) = (ints(l), ints(r)) {
                Some(Expr::Int(a.wrapping_add(b), sp))
            } else if let Some(a) = flts(l) {
                if let Some(b) = flts(r) {
                    Some(Expr::Float(a + b, sp))
                } else {
                    ints(r).map(|b| Expr::Float(a + b as f64, sp))
                }
            } else if let Some(a) = ints(l) {
                flts(r).map(|b| Expr::Float(a as f64 + b, sp))
            } else {
                None
            }
        }
        Sub => {
            if let (Some(a), Some(b)) = (ints(l), ints(r)) {
                Some(Expr::Int(a.wrapping_sub(b), sp))
            } else if let Some(a) = flts(l) {
                if let Some(b) = flts(r) {
                    Some(Expr::Float(a - b, sp))
                } else {
                    ints(r).map(|b| Expr::Float(a - b as f64, sp))
                }
            } else if let Some(a) = ints(l) {
                flts(r).map(|b| Expr::Float(a as f64 - b, sp))
            } else {
                None
            }
        }
        Mul => {
            if let (Some(a), Some(b)) = (ints(l), ints(r)) {
                Some(Expr::Int(a.wrapping_mul(b), sp))
            } else if let Some(a) = flts(l) {
                if let Some(b) = flts(r) {
                    Some(Expr::Float(a * b, sp))
                } else {
                    ints(r).map(|b| Expr::Float(a * b as f64, sp))
                }
            } else if let Some(a) = ints(l) {
                flts(r).map(|b| Expr::Float(a as f64 * b, sp))
            } else {
                None
            }
        }
        Eq | Ne | Lt | Le | Gt | Ge => {
            let ress = |v: i64, w: i64| -> Option<bool> {
                use BinOp::*;
                Some(match op {
                    Eq => v == w,
                    Ne => v != w,
                    Lt => v < w,
                    Le => v <= w,
                    Gt => v > w,
                    Ge => v >= w,
                    _ => unreachable!(),
                })
            };
            if let (Some(a), Some(b)) = (ints(l), ints(r)) {
                ress(a, b).map(|b| Expr::Bool(b, sp))
            } else {
                None
            }
        }
        Div | Mod | And | Or => None,
    }
}

// ---------------------------------------------------------------------------
// Statement / expression passes
// ---------------------------------------------------------------------------

fn opt_stmt(st: &mut Stmt) -> usize {
    let mut n = 0;
    match st {
        Stmt::Route(r) => {
            n += truncate_after_terminator(&mut r.body);
            n += opt_block(&mut r.body);
        }
        Stmt::Socket(s) => {
            for ev in [&mut s.connect, &mut s.message, &mut s.disconnect] {
                if let Some(b) = ev {
                    n += truncate_after_terminator(b);
                    n += opt_block(b);
                }
            }
        }
        Stmt::Func(f) => {
            n += truncate_after_terminator(&mut f.body);
            n += opt_block(&mut f.body);
        }
        Stmt::Middleware { body, .. } => {
            n += truncate_after_terminator(body);
            n += opt_block(body);
        }
        Stmt::Test(t) => {
            n += truncate_after_terminator(&mut t.body);
            n += opt_block(&mut t.body);
        }
        Stmt::If { then_body, else_body, .. } => {
            n += truncate_after_terminator(then_body) + truncate_after_terminator(else_body);
            n += opt_block(then_body) + opt_block(else_body);
        }
        Stmt::Loop { body, .. } => {
            n += truncate_after_terminator(body);
            n += opt_block(body);
        }
        Stmt::Race(es, _) => {
            for e in es.iter_mut() {
                n += opt_expr(e);
            }
        }
        _ => {}
    }
    if let Some(v) = stmt_value_mut(st) {
        n += opt_expr(v);
    }
    n
}

/// Fold expressions inside every statement of a block.
fn opt_block(block: &mut [Stmt]) -> usize {
    let mut n = 0;
    for s in block.iter_mut() {
        n += opt_stmt(s);
    }
    n
}

/// Returns `&mut Expr` for statements that carry a value expression.
fn stmt_value_mut(st: &mut Stmt) -> Option<&mut Expr> {
    match st {
        Stmt::Var(v) | Stmt::Const(v) => Some(&mut v.value),
        Stmt::Return(e, _) => Some(e),
        _ => None,
    }
}

fn opt_expr(e: &mut Expr) -> usize {
    let mut n = 0;
    match e {
        Expr::List(items, _) => {
            for it in items.iter_mut() {
                n += opt_expr(it);
            }
        }
        Expr::Obj(kvs, _) => {
            for (_, v) in kvs.iter_mut() {
                n += opt_expr(v);
            }
        }
        Expr::Member(b, _, _) | Expr::Unary(_, b, _) => n += opt_expr(b),
        Expr::Index(b, i, _) => n += opt_expr(b) + opt_expr(i),
        Expr::Call { callee, args, .. } => {
            n += opt_expr(callee);
            for a in args.iter_mut() {
                n += opt_expr(a);
            }
        }
        Expr::Binary(op, l, r, _) => {
            n += opt_expr(l) + opt_expr(r);
            if let Some(folded) = fold_binary(*op, l, r) {
                *e = folded;
                n += 1;
            }
        }
        Expr::Match(scrut, arms, _) => {
            n += opt_expr(scrut);
            for a in arms.iter_mut() {
                if let Some(v) = &mut a.value {
                    n += opt_expr(v);
                }
                n += opt_expr(&mut a.body);
            }
        }
        Expr::Range(l, h, _) => n += opt_expr(l) + opt_expr(h),
        Expr::HttpCall { body, .. } => {
            if let Some(b) = body {
                n += opt_expr(b);
            }
        }
        _ => {}
    }
    n
}