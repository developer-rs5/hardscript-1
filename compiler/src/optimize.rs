//! HIR optimizer pipeline — ms3.2.
//!
//! Nine small passes over [`crate::hir`]. Each pass is independent, side-effect
//! free and deterministic: no pass allocates variable or expression ids (with
//! the single documented exception of expression inlining, which relabels the
//! copied subtree past the current maximum so the id invariant is preserved).
//!
//! The pipeline is pure — it rewrites the tree in place and returns what was
//! eliminated. Codegen does not consume the HIR yet, so running the optimizer
//! never changes emitted C++; `hard opt` drives it for inspection and the
//! snapshot tests pin its behaviour.
//!
//! Passes:
//!   1. `fold_constants`      fold literal binary/unary expressions
//!   2. `dead_code`           drop pure unused statements and dead bindings
//!   3. `propagate_constants`  substitute const-bound literals at read sites
//!   4. `propagate_copies`    forward plain `x <- y` copies through reads
//!   5. `inline_small_functions` inline nullary pure functions
//!   6. `tail_returns`        drop `return` that ends a block (tail position)
//!   7. `remove_unreachable`  strip statements after `return` and dead branches
//!   8. `simplify_booleans`   double negation, `&& true`, `x == x` rewrites
//!   9. `simplify_loops`      remove empty-body and empty-iterate loops

use std::collections::HashMap;

use crate::hir::{
    BinOp, HirBlock, HirExpr, HirExprKind, HirFn, HirFnKind, HirItem, HirProgram, HirStmt, Id,
    UnOp, VarId,
};

/// Per-pass elimination counts plus deterministic debug lines, aggregated over
/// the (bounded) number of fixpoint iterations the pipeline runs.
#[derive(Debug, Clone, Default)]
pub struct OptimizerStats {
    pub fold_constants: usize,
    pub dead_code: usize,
    pub propagate_constants: usize,
    pub propagate_copies: usize,
    pub inline_small_functions: usize,
    pub tail_returns: usize,
    pub remove_unreachable: usize,
    pub simplify_booleans: usize,
    pub simplify_loops: usize,
    /// One line per elimination: `pass: <snippet>`. Deterministic order.
    pub log: Vec<String>,
}

impl OptimizerStats {
    pub fn total(&self) -> usize {
        self.fold_constants
            + self.dead_code
            + self.propagate_constants
            + self.propagate_copies
            + self.inline_small_functions
            + self.tail_returns
            + self.remove_unreachable
            + self.simplify_booleans
            + self.simplify_loops
    }

    /// Multi-line summary for `hard opt` and the compile reports.
    pub fn summary(&self) -> String {
        let rows = [
            ("fold_constants", self.fold_constants),
            ("dead_code", self.dead_code),
            ("propagate_constants", self.propagate_constants),
            ("propagate_copies", self.propagate_copies),
            ("inline_small_functions", self.inline_small_functions),
            ("tail_returns", self.tail_returns),
            ("remove_unreachable", self.remove_unreachable),
            ("simplify_booleans", self.simplify_booleans),
            ("simplify_loops", self.simplify_loops),
        ];
        let mut out = String::new();
        for (name, count) in rows {
            out.push_str(&format!("  {name:<24} {count}\n"));
        }
        out.push_str(&format!("  {:<24} {}\n", "total", self.total()));
        if !self.log.is_empty() {
            out.push_str("  log:\n");
            for line in &self.log {
                out.push_str(&format!("    {line}\n"));
            }
        }
        out
    }
}

fn snippet(e: &HirExpr) -> String {
    use HirExprKind::*;
    match &e.kind {
        Int(v) => v.to_string(),
        Float(v) => v.to_string(),
        Str(s) => format!("{s:?}"),
        Bool(b) => b.to_string(),
        Ident { name, .. } => name.clone(),
        Unary { op, operand } => match op {
            UnOp::Neg => format!("-({})", snippet(operand)),
            UnOp::Not => format!("!({})", snippet(operand)),
        },
        Binary { op, lhs, rhs } => format!("{} {} {}", snippet(lhs), op.symbol(), snippet(rhs)),
        _ => format!("{:?}", e.kind),
    }
}

// ---------------------------------------------------------------------------
// Expression traversal
// ---------------------------------------------------------------------------

/// Rewrite every expression of a block (and its nested blocks), post-order.
fn each_block_expr(b: &mut HirBlock, f: &mut impl FnMut(&mut HirExpr)) {
    for stmt in &mut b.stmts {
        match stmt {
            HirStmt::Var { value, .. } | HirStmt::Const { value, .. } => map_expr(value, f),
            HirStmt::If {
                cond,
                then_b,
                else_b,
                ..
            } => {
                map_expr(cond, f);
                each_block_expr(then_b, f);
                if let Some(e) = else_b {
                    each_block_expr(e, f);
                }
            }
            HirStmt::Loop { iter, body, .. } => {
                map_expr(iter, f);
                each_block_expr(body, f);
            }
            HirStmt::Return { value, .. } => map_expr(value, f),
            HirStmt::Race { tasks, .. } => {
                for t in tasks {
                    map_expr(t, f);
                }
            }
            HirStmt::Expect { lhs, rhs, .. } => {
                map_expr(lhs, f);
                map_expr(rhs, f);
            }
            HirStmt::Expr { expr } => map_expr(expr, f),
        }
    }
}

/// Post-order rewrite of one expression tree. Reads every kind, applying `f`
/// to each node after its children.
fn map_expr(e: &mut HirExpr, f: &mut impl FnMut(&mut HirExpr)) {
    match &mut e.kind {
        HirExprKind::List(items) => {
            for it in items {
                map_expr(it, f);
            }
        }
        HirExprKind::Obj(kv) => {
            for (_, v) in kv {
                map_expr(v, f);
            }
        }
        HirExprKind::Member { base, .. } => map_expr(base, f),
        HirExprKind::Index { base, index } => {
            map_expr(base, f);
            map_expr(index, f);
        }
        HirExprKind::Call { callee, args } => {
            map_expr(callee, f);
            for a in args {
                map_expr(a, f);
            }
        }
        HirExprKind::Unary { operand, .. } => map_expr(operand, f),
        HirExprKind::Binary { lhs, rhs, .. } => {
            map_expr(lhs, f);
            map_expr(rhs, f);
        }
        HirExprKind::Range { lo, hi } => {
            map_expr(lo, f);
            map_expr(hi, f);
        }
        HirExprKind::HttpCall { body, .. } => {
            if let Some(b) = body {
                map_expr(b, f);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            map_expr(scrutinee, f);
            for arm in arms {
                if let Some(v) = &mut arm.value {
                    map_expr(v, f);
                }
                map_expr(&mut arm.body, f);
            }
        }
        _ => {}
    }
    f(e);
}

// ---------------------------------------------------------------------------
// Pass 1: constant folding
// ---------------------------------------------------------------------------

fn fold_lit_bin(op: BinOp, lhs: &HirExpr, rhs: &HirExpr) -> Option<HirExprKind> {
    use HirExprKind::*;
    if matches!(op, BinOp::Div | BinOp::Mod) {
        if let Int(0) = rhs.kind {
            return None; // keep division by zero for runtime semantics
        }
    }
    // string concatenation
    if op == BinOp::Add {
        if let (Str(a), Str(b)) = (&lhs.kind, &rhs.kind) {
            return Some(Str(format!("{a}{b}")));
        }
    }
    let num = |e: &HirExpr| -> Option<(i64, f64)> {
        match &e.kind {
            Int(n) => Some((*n, *n as f64)),
            Float(f) => Some((0, *f)),
            _ => None,
        }
    };
    let (ai, af) = num(lhs)?;
    let (bi, bf) = num(rhs)?;
    let intp = matches!(lhs.kind, Int(_)) && matches!(rhs.kind, Int(_));
    match op {
        BinOp::Add if intp => Some(Int(ai.wrapping_add(bi))),
        BinOp::Sub if intp => Some(Int(ai.wrapping_sub(bi))),
        BinOp::Mul if intp => Some(Int(ai.wrapping_mul(bi))),
        BinOp::Div if intp => Some(Int(ai.wrapping_div(bi))),
        BinOp::Mod if intp => Some(Int(ai.wrapping_rem(bi))),
        BinOp::Add => Some(Float(af + bf)),
        BinOp::Sub => Some(Float(af - bf)),
        BinOp::Mul => Some(Float(af * bf)),
        BinOp::Div if bf != 0.0 => Some(Float(af / bf)),
        BinOp::Div => None,
        BinOp::Mod => None,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let v = if intp {
                match op {
                    BinOp::Eq => ai == bi,
                    BinOp::Ne => ai != bi,
                    BinOp::Lt => ai < bi,
                    BinOp::Le => ai <= bi,
                    BinOp::Gt => ai > bi,
                    _ => ai >= bi,
                }
            } else {
                match op {
                    BinOp::Eq => af == bf,
                    BinOp::Ne => af != bf,
                    BinOp::Lt => af < bf,
                    BinOp::Le => af <= bf,
                    BinOp::Gt => af > bf,
                    _ => af >= bf,
                }
            };
            Some(Bool(v))
        }
        BinOp::And | BinOp::Or => match (&lhs.kind, &rhs.kind) {
            (Bool(a), Bool(b)) => Some(Bool(match op {
                BinOp::And => *a && *b,
                _ => *a || *b,
            })),
            _ => None,
        },
    }
}

fn fold_lit_un(op: UnOp, operand: &HirExpr) -> Option<HirExprKind> {
    use HirExprKind::*;
    match (op, &operand.kind) {
        (UnOp::Neg, Int(n)) => Some(Int(-n)),
        (UnOp::Neg, Float(f)) => Some(Float(-f)),
        (UnOp::Not, Bool(b)) => Some(Bool(!b)),
        _ => None,
    }
}

fn fold_one(e: &mut HirExpr, s: &mut OptimizerStats) {
    let replace = match &mut e.kind {
        HirExprKind::Binary { op, lhs, rhs } => fold_lit_bin(*op, lhs, rhs),
        HirExprKind::Unary { op, operand } => fold_lit_un(*op, operand),
        _ => None,
    };
    if let Some(k) = replace {
        s.fold_constants += 1;
        s.log.push(format!(
            "fold_constants: {} => {}",
            snippet(e),
            snippet_kind(&k)
        ));
        e.kind = k;
    }
}

fn snippet_kind(k: &HirExprKind) -> String {
    match k {
        HirExprKind::Int(v) => v.to_string(),
        HirExprKind::Float(f) => f.to_string(),
        HirExprKind::Str(x) => format!("{x:?}"),
        HirExprKind::Bool(b) => b.to_string(),
        _ => format!("{:?}", k),
    }
}

fn pass_fold(p: &mut HirProgram, s: &mut OptimizerStats) {
    for item in &mut p.items {
        match item {
            HirItem::Var { value, .. } => map_expr(value, &mut |e| fold_one(e, s)),
            HirItem::Fn(f) => each_block_expr(&mut f.body, &mut |e| fold_one(e, s)),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Use count analysis
// ---------------------------------------------------------------------------

fn count_uses(p: &HirProgram) -> HashMap<VarId, usize> {
    let mut uses = HashMap::new();
    for item in &p.items {
        match item {
            HirItem::Var { value, .. } => collect_expr_uses(value, &mut uses),
            HirItem::Fn(f) => {
                for par in &f.params {
                    uses.entry(par.id).or_insert(0);
                }
                collect_block_uses(&f.body, &mut uses);
            }
            _ => {}
        }
    }
    uses
}

fn collect_expr_uses(e: &HirExpr, uses: &mut HashMap<VarId, usize>) {
    if let HirExprKind::Ident { var: Some(id), .. } = &e.kind {
        *uses.entry(*id).or_insert(0) += 1;
    }
    match &e.kind {
        HirExprKind::List(items) => {
            for it in items {
                collect_expr_uses(it, uses);
            }
        }
        HirExprKind::Obj(kv) => {
            for (_, v) in kv {
                collect_expr_uses(v, uses);
            }
        }
        HirExprKind::Member { base, .. } => collect_expr_uses(base, uses),
        HirExprKind::Index { base, index } => {
            collect_expr_uses(base, uses);
            collect_expr_uses(index, uses);
        }
        HirExprKind::Call { callee, args } => {
            collect_expr_uses(callee, uses);
            for a in args {
                collect_expr_uses(a, uses);
            }
        }
        HirExprKind::Unary { operand, .. } => collect_expr_uses(operand, uses),
        HirExprKind::Binary { lhs, rhs, .. } => {
            collect_expr_uses(lhs, uses);
            collect_expr_uses(rhs, uses);
        }
        HirExprKind::Range { lo, hi } => {
            collect_expr_uses(lo, uses);
            collect_expr_uses(hi, uses);
        }
        HirExprKind::HttpCall { body, .. } => {
            if let Some(b) = body {
                collect_expr_uses(b, uses);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            collect_expr_uses(scrutinee, uses);
            for arm in arms {
                if let Some(v) = &arm.value {
                    collect_expr_uses(v, uses);
                }
                collect_expr_uses(&arm.body, uses);
            }
        }
        // Uses inside a transaction body count: dropping a binding the block
        // reads would change what the database sees.
        HirExprKind::Transaction { body } => {
            collect_block_uses(body, uses);
        }
        _ => {}
    }
}

fn collect_block_uses(b: &HirBlock, uses: &mut HashMap<VarId, usize>) {
    for stmt in &b.stmts {
        match stmt {
            HirStmt::Var { value, .. } | HirStmt::Const { value, .. } => {
                collect_expr_uses(value, uses);
            }
            HirStmt::If {
                cond,
                then_b,
                else_b,
                ..
            } => {
                collect_expr_uses(cond, uses);
                collect_block_uses(then_b, uses);
                if let Some(e) = else_b {
                    collect_block_uses(e, uses);
                }
            }
            HirStmt::Loop { iter, body, .. } => {
                collect_expr_uses(iter, uses);
                collect_block_uses(body, uses);
            }
            HirStmt::Return { value, .. } => collect_expr_uses(value, uses),
            HirStmt::Race { tasks, .. } => {
                for t in tasks {
                    collect_expr_uses(t, uses);
                }
            }
            HirStmt::Expect { lhs, rhs, .. } => {
                collect_expr_uses(lhs, uses);
                collect_expr_uses(rhs, uses);
            }
            HirStmt::Expr { expr } => collect_expr_uses(expr, uses),
        }
    }
}

/// Is the expression guaranteed side-effect free for the optimizer to reason
/// about (literals, identifiers, pure operators, member/index access)?
fn is_pure(e: &HirExpr) -> bool {
    match &e.kind {
        HirExprKind::Int(_)
        | HirExprKind::Float(_)
        | HirExprKind::Str(_)
        | HirExprKind::Bool(_)
        | HirExprKind::Ident { .. } => true,
        HirExprKind::List(items) => items.iter().all(is_pure),
        HirExprKind::Obj(kv) => kv.iter().all(|(_, v)| is_pure(v)),
        HirExprKind::Member { base, .. } => is_pure(base),
        HirExprKind::Index { base, index } => is_pure(base) && is_pure(index),
        HirExprKind::Unary { operand, .. } => is_pure(operand),
        HirExprKind::Binary { lhs, rhs, .. } => is_pure(lhs) && is_pure(rhs),
        HirExprKind::Range { lo, hi } => is_pure(lo) && is_pure(hi),
        HirExprKind::Call { .. } | HirExprKind::HttpCall { .. } | HirExprKind::Match { .. } => {
            false
        }
        // A transaction writes to the database: never pure, never removable.
        HirExprKind::Transaction { .. } => false,
    }
}

// ---------------------------------------------------------------------------
// Pass 2: dead code
// ---------------------------------------------------------------------------

fn dce_one(stmt: HirStmt, uses: &HashMap<VarId, usize>, s: &mut OptimizerStats) -> Vec<HirStmt> {
    match stmt {
        HirStmt::Const { var, value, span } => {
            if uses.get(&var.id).copied().unwrap_or(0) > 0 || !is_pure(&value) {
                vec![HirStmt::Const { var, value, span }]
            } else {
                s.dead_code += 1;
                s.log
                    .push(format!("dead_code: binding v#{} {:?}", var.id.0, var.name));
                Vec::new()
            }
        }
        HirStmt::Var { var, value, span } => {
            if uses.get(&var.id).copied().unwrap_or(0) > 0 || !is_pure(&value) {
                vec![HirStmt::Var { var, value, span }]
            } else {
                s.dead_code += 1;
                s.log
                    .push(format!("dead_code: binding v#{} {:?}", var.id.0, var.name));
                Vec::new()
            }
        }
        HirStmt::Expr { expr } => {
            if is_pure(&expr) {
                s.dead_code += 1;
                s.log.push(format!("dead_code: expr {}", snippet(&expr)));
                Vec::new()
            } else {
                vec![HirStmt::Expr { expr }]
            }
        }
        other => vec![other],
    }
}

fn dce_block(b: &mut HirBlock, uses: &HashMap<VarId, usize>, s: &mut OptimizerStats) {
    // The final statement of a block may be the block's computed value (a tail
    // expression after `tail_returns`), so it is never removed here.
    let last = b.stmts.pop();
    let mut out = Vec::new();
    for stmt in std::mem::take(&mut b.stmts) {
        match stmt {
            HirStmt::If {
                cond,
                then_b,
                else_b,
                span,
            } => {
                let mut tb = then_b;
                dce_block(&mut tb, uses, s);
                let eb = else_b.map(|mut e| {
                    dce_block(&mut e, uses, s);
                    e
                });
                out.push(HirStmt::If {
                    cond,
                    then_b: tb,
                    else_b: eb,
                    span,
                });
            }
            HirStmt::Loop {
                var,
                iter,
                body,
                span,
            } => {
                let mut body = body;
                dce_block(&mut body, uses, s);
                out.push(HirStmt::Loop {
                    var,
                    iter,
                    body,
                    span,
                });
            }
            _ => out.extend(dce_one(stmt, uses, s)),
        }
    }
    if let Some(stmt) = last {
        match stmt {
            HirStmt::Expr { expr } => out.push(HirStmt::Expr { expr }),
            other => out.extend(dce_one(other, uses, s)),
        }
    }
    b.stmts = out;
}

fn pass_dce(p: &mut HirProgram, s: &mut OptimizerStats) {
    let uses = count_uses(p);
    for item in &mut p.items {
        if let HirItem::Fn(f) = item {
            dce_block(&mut f.body, &uses, s);
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 3 + 4: constant and copy propagation
// ---------------------------------------------------------------------------

/// Const bindings whose value is a literal kind the optimizer can materialize.
fn collect_consts(p: &HirProgram) -> HashMap<VarId, HirExprKind> {
    fn scan_block(b: &HirBlock, out: &mut HashMap<VarId, HirExprKind>) {
        for stmt in &b.stmts {
            match stmt {
                HirStmt::Const { var, value, .. } => {
                    if is_literal_kind(&value.kind) {
                        out.insert(var.id, value.kind.clone());
                    }
                }
                HirStmt::If { then_b, else_b, .. } => {
                    scan_block(then_b, out);
                    if let Some(e) = else_b {
                        scan_block(e, out);
                    }
                }
                HirStmt::Loop { body, .. } => scan_block(body, out),
                _ => {}
            }
        }
    }
    let mut map = HashMap::new();
    for item in &p.items {
        if let HirItem::Var {
            var,
            value,
            is_const: true,
            ..
        } = item
        {
            if is_literal_kind(&value.kind) {
                map.insert(var.id, value.kind.clone());
            }
        }
        if let HirItem::Fn(f) = item {
            scan_block(&f.body, &mut map);
        }
    }
    map
}

/// Copy bindings `x <- y` resolved to their target `VarId`, plus the display
/// name of every binding for faithful renames.
fn collect_copies(p: &HirProgram) -> (HashMap<VarId, VarId>, HashMap<VarId, String>) {
    fn scan_block(
        b: &HirBlock,
        copies: &mut HashMap<VarId, VarId>,
        names: &mut HashMap<VarId, String>,
    ) {
        let scan_stmt = |stmt: &HirStmt,
                         copies: &mut HashMap<VarId, VarId>,
                         names: &mut HashMap<VarId, String>| {
            if let HirStmt::Var { var, value, .. } | HirStmt::Const { var, value, .. } = stmt {
                names.insert(var.id, var.name.clone());
                if let HirExprKind::Ident {
                    var: Some(target), ..
                } = &value.kind
                {
                    copies.insert(var.id, *target);
                }
            }
        };
        for stmt in &b.stmts {
            match stmt {
                HirStmt::Var { .. } | HirStmt::Const { .. } => {
                    scan_stmt(stmt, copies, names);
                }
                HirStmt::If { then_b, else_b, .. } => {
                    scan_block(then_b, copies, names);
                    if let Some(e) = else_b {
                        scan_block(e, copies, names);
                    }
                }
                HirStmt::Loop { var, body, .. } => {
                    names.insert(var.id, var.name.clone());
                    scan_block(body, copies, names);
                }
                _ => {}
            }
        }
    }
    let mut copies = HashMap::new();
    let mut names = HashMap::new();
    for item in &p.items {
        if let HirItem::Var { var, value, .. } = item {
            names.insert(var.id, var.name.clone());
            if let HirExprKind::Ident {
                var: Some(target), ..
            } = &value.kind
            {
                copies.insert(var.id, *target);
            }
        }
        if let HirItem::Fn(f) = item {
            for par in &f.params {
                names.insert(par.id, par.name.clone());
            }
            scan_block(&f.body, &mut copies, &mut names);
        }
    }
    (copies, names)
}

fn is_literal_kind(k: &HirExprKind) -> bool {
    matches!(
        k,
        HirExprKind::Int(_) | HirExprKind::Float(_) | HirExprKind::Str(_) | HirExprKind::Bool(_)
    )
}

fn pass_prop_const(p: &mut HirProgram, s: &mut OptimizerStats) -> usize {
    let map = collect_consts(p);
    let mut replaced = 0usize;
    for item in &mut p.items {
        if let HirItem::Fn(f) = item {
            each_block_expr(&mut f.body, &mut |e| {
                if let HirExprKind::Ident { var: Some(id), .. } = &e.kind {
                    if let Some(kind) = map.get(id) {
                        let k = literal_kind(kind);
                        if let Some(k) = k {
                            s.propagate_constants += 1;
                            s.log.push(format!(
                                "propagate_constants: v#{} => {}",
                                id.0,
                                snippet_kind(&k)
                            ));
                            e.kind = k;
                            replaced += 1;
                        }
                    }
                }
            });
        }
    }
    replaced
}

fn literal_kind(k: &HirExprKind) -> Option<HirExprKind> {
    match k {
        HirExprKind::Int(n) => Some(HirExprKind::Int(*n)),
        HirExprKind::Float(f) => Some(HirExprKind::Float(*f)),
        HirExprKind::Str(x) => Some(HirExprKind::Str(x.clone())),
        HirExprKind::Bool(b) => Some(HirExprKind::Bool(*b)),
        _ => None,
    }
}

fn pass_prop_copies(p: &mut HirProgram, s: &mut OptimizerStats) -> usize {
    let (copies, names) = collect_copies(p);
    let mut replaced = 0usize;
    for item in &mut p.items {
        if let HirItem::Fn(f) = item {
            each_block_expr(&mut f.body, &mut |e| {
                if let HirExprKind::Ident { var: Some(id), .. } = &e.kind {
                    if let Some(target) = copies.get(id) {
                        let target = *target;
                        let target_name = names.get(&target).cloned().unwrap_or_default();
                        s.propagate_copies += 1;
                        s.log
                            .push(format!("propagate_copies: v#{} => {:?}", id.0, target_name));
                        e.kind = HirExprKind::Ident {
                            name: target_name,
                            var: Some(target),
                        };
                        replaced += 1;
                    }
                }
            });
        }
    }
    replaced
}

// ---------------------------------------------------------------------------
// Pass 5: inline small (nullary, pure) functions
// ---------------------------------------------------------------------------

fn next_id(p: &HirProgram) -> u32 {
    let mut m = 0u32;
    for item in &p.items {
        match item {
            HirItem::Var { value, .. } => walk_expr_ref(value, &mut |e: &HirExpr| {
                if e.id.0 > m {
                    m = e.id.0;
                }
            }),
            HirItem::Fn(f) => {
                if f.id.0 > m {
                    m = f.id.0;
                }
                max_id_block(&f.body, &mut m);
            }
            _ => {}
        }
    }
    m
}

fn max_id_block(b: &HirBlock, m: &mut u32) {
    if b.id.0 > *m {
        *m = b.id.0;
    }
    for_each_expr_ref(b, &mut |e: &HirExpr| {
        if e.id.0 > *m {
            *m = e.id.0;
        }
    });
}

fn for_each_expr_ref(b: &HirBlock, f: &mut impl FnMut(&HirExpr)) {
    for stmt in &b.stmts {
        match stmt {
            HirStmt::Var { value, .. } | HirStmt::Const { value, .. } => walk_expr_ref(value, f),
            HirStmt::If {
                cond,
                then_b,
                else_b,
                ..
            } => {
                walk_expr_ref(cond, f);
                for_each_expr_ref(then_b, f);
                if let Some(e) = else_b {
                    for_each_expr_ref(e, f);
                }
            }
            HirStmt::Loop { iter, body, .. } => {
                walk_expr_ref(iter, f);
                for_each_expr_ref(body, f);
            }
            HirStmt::Return { value, .. } => walk_expr_ref(value, f),
            HirStmt::Race { tasks, .. } => {
                for t in tasks {
                    walk_expr_ref(t, f);
                }
            }
            HirStmt::Expect { lhs, rhs, .. } => {
                walk_expr_ref(lhs, f);
                walk_expr_ref(rhs, f);
            }
            HirStmt::Expr { expr } => walk_expr_ref(expr, f),
        }
    }
}

fn walk_expr_ref(e: &HirExpr, f: &mut impl FnMut(&HirExpr)) {
    f(e);
    match &e.kind {
        HirExprKind::List(items) => {
            for it in items {
                walk_expr_ref(it, f);
            }
        }
        HirExprKind::Obj(kv) => {
            for (_, v) in kv {
                walk_expr_ref(v, f);
            }
        }
        HirExprKind::Member { base, .. } => walk_expr_ref(base, f),
        HirExprKind::Index { base, index } => {
            walk_expr_ref(base, f);
            walk_expr_ref(index, f);
        }
        HirExprKind::Call { callee, args } => {
            walk_expr_ref(callee, f);
            for a in args {
                walk_expr_ref(a, f);
            }
        }
        HirExprKind::Unary { operand, .. } => walk_expr_ref(operand, f),
        HirExprKind::Binary { lhs, rhs, .. } => {
            walk_expr_ref(lhs, f);
            walk_expr_ref(rhs, f);
        }
        HirExprKind::Range { lo, hi } => {
            walk_expr_ref(lo, f);
            walk_expr_ref(hi, f);
        }
        HirExprKind::HttpCall { body, .. } => {
            if let Some(b) = body {
                walk_expr_ref(b, f);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            walk_expr_ref(scrutinee, f);
            for arm in arms {
                if let Some(v) = &arm.value {
                    walk_expr_ref(v, f);
                }
                walk_expr_ref(&arm.body, f);
            }
        }
        _ => {}
    }
}

/// Relabel an expression subtree with fresh ids past `*next` (inlining only).
fn relabel(e: &mut HirExpr, next: &mut u32) {
    e.id = Id(*next);
    *next += 1;
    match &mut e.kind {
        HirExprKind::List(items) => {
            for it in items {
                relabel(it, next);
            }
        }
        HirExprKind::Obj(kv) => {
            for (_, v) in kv {
                relabel(v, next);
            }
        }
        HirExprKind::Member { base, .. } => relabel(base, next),
        HirExprKind::Index { base, index } => {
            relabel(base, next);
            relabel(index, next);
        }
        HirExprKind::Call { callee, args } => {
            relabel(callee, next);
            for a in args {
                relabel(a, next);
            }
        }
        HirExprKind::Unary { operand, .. } => relabel(operand, next),
        HirExprKind::Binary { lhs, rhs, .. } => {
            relabel(lhs, next);
            relabel(rhs, next);
        }
        HirExprKind::Range { lo, hi } => {
            relabel(lo, next);
            relabel(hi, next);
        }
        HirExprKind::HttpCall { body, .. } => {
            if let Some(b) = body {
                relabel(b, next);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            relabel(scrutinee, next);
            for arm in arms {
                if let Some(v) = &mut arm.value {
                    relabel(v, next);
                }
                relabel(&mut arm.body, next);
            }
        }
        _ => {}
    }
}

fn inlineable(f: &HirFn) -> Option<HirExpr> {
    if !matches!(f.kind, HirFnKind::Func) || !f.params.is_empty() {
        return None;
    }
    if f.body.stmts.len() != 1 {
        return None;
    }
    match &f.body.stmts[0] {
        HirStmt::Return { value, .. } if is_pure(value) => Some(value.clone()),
        _ => None,
    }
}

/// Inline calls of nullary pure helper functions inside `e`, in place. Freshly
/// copied subtrees are relabeled past the current id maximum so id uniqueness
/// is preserved. Recursive: inlined bodies are themselves inlined.
fn inline_in_expr(
    e: &mut HirExpr,
    fns: &[(String, HirExpr)],
    next: &mut u32,
    s: &mut OptimizerStats,
) {
    match &mut e.kind {
        HirExprKind::List(items) => {
            for it in items {
                inline_in_expr(it, fns, next, s);
            }
        }
        HirExprKind::Obj(kv) => {
            for (_, v) in kv {
                inline_in_expr(v, fns, next, s);
            }
        }
        HirExprKind::Member { base, .. } => inline_in_expr(base, fns, next, s),
        HirExprKind::Index { base, index } => {
            inline_in_expr(base, fns, next, s);
            inline_in_expr(index, fns, next, s);
        }
        HirExprKind::Call { callee, args } => {
            inline_in_expr(callee, fns, next, s);
            for a in args {
                inline_in_expr(a, fns, next, s);
            }
        }
        HirExprKind::Unary { operand, .. } => inline_in_expr(operand, fns, next, s),
        HirExprKind::Binary { lhs, rhs, .. } => {
            inline_in_expr(lhs, fns, next, s);
            inline_in_expr(rhs, fns, next, s);
        }
        HirExprKind::Range { lo, hi } => {
            inline_in_expr(lo, fns, next, s);
            inline_in_expr(hi, fns, next, s);
        }
        HirExprKind::HttpCall { body, .. } => {
            if let Some(b) = body {
                inline_in_expr(b, fns, next, s);
            }
        }
        HirExprKind::Match { scrutinee, arms } => {
            inline_in_expr(scrutinee, fns, next, s);
            for arm in arms {
                if let Some(v) = &mut arm.value {
                    inline_in_expr(v, fns, next, s);
                }
                inline_in_expr(&mut arm.body, fns, next, s);
            }
        }
        _ => {}
    }
    // self: if this is a call to an inlineable helper, splice its body in.
    let replacement = match &e.kind {
        HirExprKind::Call { callee, args } => {
            if args.is_empty() {
                if let HirExprKind::Ident { name, var: None } = &callee.kind {
                    if let Some((_, body)) = fns.iter().find(|(n, _)| n == name) {
                        let mut copy = body.clone();
                        relabel(&mut copy, next);
                        s.inline_small_functions += 1;
                        s.log.push(format!(
                            "inline_small_functions: {} ({} nodes)",
                            name,
                            count_nodes(&copy)
                        ));
                        Some(copy.kind)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        }
        _ => None,
    };
    if let Some(k) = replacement {
        e.kind = k;
    }
}

fn count_nodes(e: &HirExpr) -> usize {
    let mut n = 0;
    let mut stack = vec![e];
    while let Some(node) = stack.pop() {
        n += 1;
        match &node.kind {
            HirExprKind::List(items) => stack.extend(items.iter()),
            HirExprKind::Obj(kv) => stack.extend(kv.iter().map(|(_, v)| v)),
            HirExprKind::Member { base, .. } => stack.push(base),
            HirExprKind::Index { base, index } => {
                stack.push(index);
                stack.push(base);
            }
            HirExprKind::Call { callee, args } => {
                stack.extend(args.iter());
                stack.push(callee);
            }
            HirExprKind::Unary { operand, .. } => stack.push(operand),
            HirExprKind::Binary { lhs, rhs, .. } => {
                stack.push(rhs);
                stack.push(lhs);
            }
            HirExprKind::Range { lo, hi } => {
                stack.push(hi);
                stack.push(lo);
            }
            HirExprKind::HttpCall { body, .. } => {
                if let Some(b) = body {
                    stack.push(b);
                }
            }
            HirExprKind::Match { scrutinee, arms } => {
                for arm in arms {
                    if let Some(v) = &arm.value {
                        stack.push(v);
                    }
                    stack.push(&arm.body);
                }
                stack.push(scrutinee);
            }
            _ => {}
        }
    }
    n
}

fn pass_inline(p: &mut HirProgram, s: &mut OptimizerStats) {
    let fns: Vec<(String, HirExpr)> = p
        .items
        .iter()
        .filter_map(|item| match item {
            HirItem::Fn(f) => inlineable(f).map(|body| (f.name.clone(), body)),
            _ => None,
        })
        .collect();
    if fns.is_empty() {
        return;
    }
    let mut next = next_id(p) + 1;
    for item in &mut p.items {
        if let HirItem::Fn(f) = item {
            rewrite_block_inline(&mut f.body, &fns, &mut next, s);
        }
    }
}

fn rewrite_block_inline(
    b: &mut HirBlock,
    fns: &[(String, HirExpr)],
    next: &mut u32,
    s: &mut OptimizerStats,
) {
    for stmt in &mut b.stmts {
        match stmt {
            HirStmt::Var { value, .. }
            | HirStmt::Const { value, .. }
            | HirStmt::Return { value, .. }
            | HirStmt::Expr { expr: value } => inline_in_expr(value, fns, next, s),
            HirStmt::If {
                cond,
                then_b,
                else_b,
                ..
            } => {
                inline_in_expr(cond, fns, next, s);
                rewrite_block_inline(then_b, fns, next, s);
                if let Some(e) = else_b {
                    rewrite_block_inline(e, fns, next, s);
                }
            }
            HirStmt::Loop { iter, body, .. } => {
                inline_in_expr(iter, fns, next, s);
                rewrite_block_inline(body, fns, next, s);
            }
            HirStmt::Race { tasks, .. } => {
                for t in tasks {
                    inline_in_expr(t, fns, next, s);
                }
            }
            HirStmt::Expect { lhs, rhs, .. } => {
                inline_in_expr(lhs, fns, next, s);
                inline_in_expr(rhs, fns, next, s);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 6: tail returns
// ---------------------------------------------------------------------------

fn pass_tail(p: &mut HirProgram, s: &mut OptimizerStats) {
    for item in &mut p.items {
        if let HirItem::Fn(f) = item {
            tail_block(&mut f.body, s);
        }
    }
}

fn tail_block(b: &mut HirBlock, s: &mut OptimizerStats) {
    for stmt in &mut b.stmts {
        if let HirStmt::If { then_b, else_b, .. } = stmt {
            tail_block(then_b, s);
            if let Some(e) = else_b {
                tail_block(e, s);
            }
        }
        if let HirStmt::Loop { body, .. } = stmt {
            tail_block(body, s);
        }
    }
    if matches!(b.stmts.last(), Some(HirStmt::Return { .. })) {
        if let Some(HirStmt::Return { value, .. }) = b.stmts.pop() {
            s.tail_returns += 1;
            s.log.push(format!("tail_returns: {:?}", snippet(&value)));
            b.stmts.push(HirStmt::Expr { expr: value });
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 7: unreachable code and dead branches
// ---------------------------------------------------------------------------

fn pass_unreachable(p: &mut HirProgram, s: &mut OptimizerStats) {
    for item in &mut p.items {
        if let HirItem::Fn(f) = item {
            unreachable_block(&mut f.body, s);
        }
    }
}

fn ends_in_return(b: &HirBlock) -> bool {
    matches!(b.stmts.last(), Some(HirStmt::Return { .. }))
}

fn stmt_kind(st: &HirStmt) -> &'static str {
    match st {
        HirStmt::Var { .. } => "var",
        HirStmt::Const { .. } => "const",
        HirStmt::If { .. } => "if",
        HirStmt::Loop { .. } => "loop",
        HirStmt::Return { .. } => "return",
        HirStmt::Race { .. } => "race",
        HirStmt::Expect { .. } => "expect",
        HirStmt::Expr { .. } => "expr",
    }
}

fn unreachable_block(b: &mut HirBlock, s: &mut OptimizerStats) {
    let mut keep = Vec::new();
    for stmt in std::mem::take(&mut b.stmts) {
        match stmt {
            HirStmt::If {
                cond,
                then_b,
                else_b,
                span,
            } => {
                let mut tb = then_b;
                unreachable_block(&mut tb, s);
                let mut eb = else_b.map(|mut e| {
                    unreachable_block(&mut e, s);
                    e
                });
                match cond.kind {
                    HirExprKind::Bool(true) => {
                        s.remove_unreachable += 1;
                        s.log
                            .push("remove_unreachable: ?(true) collapses to then".into());
                        keep.extend(tb.stmts);
                    }
                    HirExprKind::Bool(false) => {
                        s.remove_unreachable += 1;
                        s.log
                            .push("remove_unreachable: ?(false) collapses to else".into());
                        if let Some(e) = eb.take() {
                            keep.extend(e.stmts);
                        }
                    }
                    _ => keep.push(HirStmt::If {
                        cond,
                        then_b: tb,
                        else_b: eb,
                        span,
                    }),
                }
            }
            HirStmt::Loop {
                var,
                iter,
                body,
                span,
            } => {
                let mut body = body;
                unreachable_block(&mut body, s);
                keep.push(HirStmt::Loop {
                    var,
                    iter,
                    body,
                    span,
                });
            }
            other => keep.push(other),
        }
    }
    let mut out = Vec::new();
    let mut terminated = false;
    for stmt in keep {
        if terminated {
            s.remove_unreachable += 1;
            s.log.push(format!(
                "remove_unreachable: dead stmt after terminator ({})",
                stmt_kind(&stmt)
            ));
            continue;
        }
        if matches!(stmt, HirStmt::Return { .. }) || matches!(stmt, HirStmt::Race { .. }) {
            terminated = true;
        }
        if let HirStmt::If { then_b, else_b, .. } = &stmt {
            if ends_in_return(then_b) && else_b.as_ref().map_or(true, ends_in_return) {
                terminated = true;
            }
        }
        out.push(stmt);
    }
    b.stmts = out;
}

// ---------------------------------------------------------------------------
// Pass 8: boolean simplification
// ---------------------------------------------------------------------------

fn boolean_one(e: &mut HirExpr, s: &mut OptimizerStats) {
    match &mut e.kind {
        HirExprKind::Unary {
            op: UnOp::Not,
            operand,
        } => {
            if let HirExprKind::Unary {
                op: UnOp::Not,
                operand: inner,
            } = &mut operand.kind
            {
                let k = std::mem::replace(&mut inner.kind, HirExprKind::Bool(false));
                let _ = std::mem::replace(&mut e.kind, k);
                s.simplify_booleans += 1;
                s.log.push("simplify_booleans: !!x => x".into());
            }
        }
        HirExprKind::Binary { op, lhs, rhs } => {
            let o = *op;
            let l = literal_bool(lhs);
            let r = literal_bool(rhs);
            match (o, &l, &r) {
                (BinOp::And, Some(true), _) => {
                    let k = std::mem::replace(&mut rhs.kind, HirExprKind::Bool(false));
                    let _ = std::mem::replace(&mut e.kind, k);
                    s.simplify_booleans += 1;
                    s.log.push("simplify_booleans: true && x => x".into());
                }
                (BinOp::And, Some(false), _) => {
                    let _ = std::mem::replace(&mut e.kind, HirExprKind::Bool(false));
                    s.simplify_booleans += 1;
                    s.log.push("simplify_booleans: false && x => false".into());
                }
                (BinOp::Or, Some(false), _) => {
                    let k = std::mem::replace(&mut rhs.kind, HirExprKind::Bool(false));
                    let _ = std::mem::replace(&mut e.kind, k);
                    s.simplify_booleans += 1;
                    s.log.push("simplify_booleans: false || x => x".into());
                }
                (BinOp::Or, Some(true), _) => {
                    let _ = std::mem::replace(&mut e.kind, HirExprKind::Bool(true));
                    s.simplify_booleans += 1;
                    s.log.push("simplify_booleans: true || x => true".into());
                }
                (BinOp::Eq, _, _) | (BinOp::Ne, _, _) if same_ident(lhs, rhs) => {
                    let v = o == BinOp::Eq;
                    let _ = std::mem::replace(&mut e.kind, HirExprKind::Bool(v));
                    s.simplify_booleans += 1;
                    s.log
                        .push(format!("simplify_booleans: x {} x => {}", o.symbol(), v));
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn literal_bool(e: &HirExpr) -> Option<bool> {
    if let HirExprKind::Bool(b) = e.kind {
        Some(b)
    } else {
        None
    }
}

fn same_ident(a: &HirExpr, b: &HirExpr) -> bool {
    match (&a.kind, &b.kind) {
        (HirExprKind::Ident { var: Some(x), .. }, HirExprKind::Ident { var: Some(y), .. }) => {
            x == y
        }
        _ => false,
    }
}

fn pass_booleans(p: &mut HirProgram, s: &mut OptimizerStats) {
    for item in &mut p.items {
        if let HirItem::Fn(f) = item {
            each_block_expr(&mut f.body, &mut |e| boolean_one(e, s));
        }
    }
}

// ---------------------------------------------------------------------------
// Pass 9: loop simplification
// ---------------------------------------------------------------------------

fn pass_loops(p: &mut HirProgram, s: &mut OptimizerStats) {
    for item in &mut p.items {
        if let HirItem::Fn(f) = item {
            loop_block(&mut f.body, s);
        }
    }
}

fn loop_block(b: &mut HirBlock, s: &mut OptimizerStats) {
    let mut out = Vec::new();
    for stmt in std::mem::take(&mut b.stmts) {
        match stmt {
            HirStmt::If {
                cond,
                mut then_b,
                else_b,
                span,
            } => {
                loop_block(&mut then_b, s);
                let eb = else_b.map(|mut e| {
                    loop_block(&mut e, s);
                    e
                });
                out.push(HirStmt::If {
                    cond,
                    then_b,
                    else_b: eb,
                    span,
                });
            }
            HirStmt::Loop {
                var,
                iter,
                body,
                span,
            } => {
                let mut body = body;
                loop_block(&mut body, s);
                if body.stmts.is_empty()
                    || matches!(&iter.kind, HirExprKind::List(items) if items.is_empty())
                {
                    s.simplify_loops += 1;
                    s.log
                        .push(format!("simplify_loops: empty loop v#{}", var.id.0));
                } else {
                    out.push(HirStmt::Loop {
                        var,
                        iter,
                        body,
                        span,
                    });
                }
            }
            other => out.push(other),
        }
    }
    b.stmts = out;
}

// ---------------------------------------------------------------------------
// Public driver
// ---------------------------------------------------------------------------

/// Run the optimizer pipeline to a bounded fixpoint. Returns per-pass stats.
pub fn run(p: &mut HirProgram) -> OptimizerStats {
    let mut s = OptimizerStats::default();
    for _ in 0..4 {
        let before = s.total();
        pass_fold(p, &mut s);
        pass_dce(p, &mut s);
        pass_prop_const(p, &mut s);
        pass_prop_copies(p, &mut s);
        pass_inline(p, &mut s);
        pass_tail(p, &mut s);
        pass_unreachable(p, &mut s);
        pass_booleans(p, &mut s);
        pass_loops(p, &mut s);
        if s.total() == before {
            break;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hir;

    fn lower(src: &str) -> HirProgram {
        let prog = crate::frontend(src, "test.hard").expect("parse");
        hir::lower(&prog)
    }

    fn opt(src: &str) -> (HirProgram, OptimizerStats) {
        let mut p = lower(src);
        let s = run(&mut p);
        (p, s)
    }

    fn has_line(p: &HirProgram, needle: &str) -> bool {
        p.render().lines().any(|l| l.contains(needle))
    }

    #[test]
    fn fold_constants_folds_arithmetic() {
        let src = r#"
app @3000
GET "/" :: {
    <- 3 * 4 + 2
}
"#;
        let (p, _) = opt(src);
        let out = p.render();
        assert!(out.contains("14"), "expected 14, got:\n{out}");
        assert!(!out.contains("binary"), "binary should fold:\n{out}");
    }

    #[test]
    fn fold_constants_keeps_div_by_zero() {
        let src = r#"
app @3000
GET "/" :: {
    <- 7 / 0
}
"#;
        let (p, _) = opt(src);
        assert!(has_line(&p, "binary"), "div by zero must stay a binary");
    }

    #[test]
    fn fold_constants_folds_string_concat() {
        let src = r#"
app @3000
GET "/" :: {
    s <- "fo" + "o"
    <- s
}
"#;
        let (p, _) = opt(src);
        assert!(has_line(&p, "\"foo\""), "{:?}", p.render());
    }

    #[test]
    fn dead_code_removes_unused_binding() {
        let src = r#"
app @3000
GET "/" :: {
    dead <- 1 + 2
    <- 5
}
"#;
        let (p, _) = opt(src);
        assert!(!has_line(&p, "\"dead\""), "dead binding should be removed");
    }

    #[test]
    fn dead_code_keeps_used_binding_and_tail() {
        let src = r#"
app @3000
GET "/" :: {
    live <- 1 + 2
    <- live
}
"#;
        let (p, _) = opt(src);
        assert!(has_line(&p, "\"live\""), "live binding stays");
        assert!(!has_line(&p, "return"), "tail return drops");
    }

    #[test]
    fn propagate_constants_flows_into_binary() {
        let src = r#"
app @3000
GET "/" :: {
    base ::= 40
    <- base + 2
}
"#;
        let (p, _) = opt(src);
        assert!(
            has_line(&p, "42") || has_line(&p, "40 + 2"),
            "constant should reach the sum:\n{}",
            p.render()
        );
    }

    #[test]
    fn propagate_copies_collapses_chains() {
        let src = r#"
app @3000
GET "/" :: {
    x <- 7
    y <- x
    <- y
}
"#;
        let (p, _) = opt(src);
        assert!(
            !has_line(&p, "\"y\""),
            "copies should collapse:\n{}",
            p.render()
        );
    }

    #[test]
    fn inline_small_function_removes_call_site() {
        let src = r#"
app @3000
calc five() => Num {
    <- 5
}
GET "/" :: {
    <- five()
}
"#;
        let (p, _) = opt(src);
        let out = p.render();
        assert!(out.contains("5"), "inlined value present:\n{out}");
        assert!(
            p.render().lines().filter(|l| l.contains("call")).count() == 0,
            "call site removed:\n{out}"
        );
    }

    #[test]
    fn tail_returns_drop_final_return() {
        let src = r#"
app @3000
GET "/" :: {
    a <- 1
    <- a
}
"#;
        let (p, _) = opt(src);
        assert!(
            !has_line(&p, "return"),
            "tail return should drop:\n{}",
            p.render()
        );
    }

    #[test]
    fn unreachable_removes_dead_branch() {
        let src = r#"
app @3000
GET "/" :: {
    ?(true) {
        <- 1
    } :{
        <- 2
    }
}
"#;
        let (p, _) = opt(src);
        let out = p.render();
        assert!(
            !out.lines().any(|l| l.ends_with(" 2")),
            "else branch of ?(true) should go:\n{out}"
        );
        assert!(
            !has_line(&p, "if"),
            "collapsed if should be spliced:\n{out}"
        );
    }

    #[test]
    fn simplify_booleans_collapses_double_negation() {
        let src = r#"
app @3000
GET "/" :: {
    a <- 3
    <- a == 3 && !!(a > 2)
}
"#;
        let (p, _) = opt(src);
        assert!(
            !has_line(&p, "!!"),
            "double negation collapses:\n{}",
            p.render()
        );
    }

    #[test]
    fn simplify_loops_removes_empty() {
        let src = r#"
app @3000
GET "/" :: {
    loop v => [1] {
    }
    <- 1
}
"#;
        let (p, _) = opt(src);
        assert!(!has_line(&p, "loop"), "empty loop removed:\n{}", p.render());
    }

    #[test]
    fn optimizer_is_deterministic() {
        let src = r#"
bring http
app @3000
items ::= [1, 2, 3]
calc double(Int x) => Int {
    y <- x * 2
    <- y
}
GET "/echo/:tag" :: (tag = Str, body = body) {
    n <- tag
    ?(n == "go") {
        <- { tag: n }
    }
    <- { tag: tag, n: double(42), items: items }
}
"#;
        let (a, _) = opt(src);
        let (b, _) = opt(src);
        assert_eq!(a.render(), b.render());
    }

    #[test]
    fn stats_total_matches_log() {
        let src = r#"
app @3000
GET "/" :: {
    dead <- 2 * 8
    live <- 3 + 4
    ?(true) {
        <- live
    }
    <- 1
}
"#;
        let (_, s) = opt(src);
        assert_eq!(
            s.total(),
            s.log.len(),
            "each elimination logs once:\n{}",
            s.summary()
        );
    }
}
