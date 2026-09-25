//! Static-warning analysis (Diagnostics V2 M3.4.4).
//!
//! Emits [`Diag`]s with warning-range catalog codes (HS2001..HS2015). The
//! build stays green by default; the CLI promotes warning codes to hard
//! errors when the user passes `--deny`. Unlike [`crate::typecheck`], every
//! node here is walked for diagnostics, so the walker returns the set of
//! identifiers referenced in a subtree (used for unused-variable detection)
//! as well as the warnings themselves.
//!
//! Emitted categories: W_UNUSED_VARIABLE, W_CONSTANT_CONDITION,
//! W_ALWAYS_TRUE_COMPARISON, W_ALWAYS_FALSE_COMPARISON,
//! W_UNREACHABLE_STATEMENT, W_EMPTY_BLOCK. The remaining catalog warning
//! codes are reserved for future passes and can already be `--deny`ed.

use crate::ast::*;
use crate::catalog as cat;
use crate::error::{Diag, ErrorKind};
use crate::token::Span;
use std::collections::HashSet;

/// Run the warning analysis over a merged program.
///
/// `files` maps `prog.stmts[i]` to the declaring module so warnings carry
/// project-root-relative locations (mirrors [`crate::typecheck::check_with`]).
pub fn analyze(prog: &Program, files: &[Option<String>]) -> Vec<Diag> {
    let mut used: HashSet<String> = HashSet::new();
    // Pass 0: whole-program reference set. Globals declared at top level may
    // be read by functions, routes and middleware anywhere in the merged
    // program, including other modules.
    for st in &prog.stmts {
        refs_in_stmt(st, &mut used);
    }
    let mut w = Warn { diags: Vec::new() };
    for (i, st) in prog.stmts.iter().enumerate() {
        let file = files.get(i).cloned().flatten();
        top_level(st, file, &used, &mut w);
    }
    w.diags
}

struct Warn {
    diags: Vec<Diag>,
}

fn push(w: &mut Warn, code: u16, message: String, span: Span, suggestion: String, file: &Option<String>) {
    let mut d = Diag::new(ErrorKind::Type, message, span, suggestion).with_code(code);
    if let Some(f) = file {
        d = d.with_location(f.clone());
    }
    w.diags.push(d);
}

/// Handle a single top-level statement: file provenance from the build
/// grapher, then scope-aware walking for everything nested.
fn top_level(st: &Stmt, file: Option<String>, whole_program_used: &HashSet<String>, w: &mut Warn) {
    match st {
        // Globals are visible across the whole merged program.
        Stmt::Var(v) | Stmt::Const(v) => {
            refs_in_expr(&v.value, &mut HashSet::new()); // no-op; already counted pass 0
            if !whole_program_used.contains(&v.name) {
                push(
                    w,
                    cat::W_UNUSED_VARIABLE,
                    format!("value `{}` is never used", v.name),
                    v.span,
                    format!("Remove the declaration, or reference `{}` somewhere.", v.name),
                    &file,
                );
            }
        }
        Stmt::Route(r) => walk_block(&r.body, &file, w),
        Stmt::Middleware { body, .. } => walk_block(body, &file, w),
        Stmt::Test(t) => walk_block(&t.body, &file, w),
        Stmt::Func(f) => {
            let mut decls: Vec<(String, Span, Option<String>)> = Vec::new();
            for p in &f.params {
                decls.push((p.name.clone(), p.span, None));
            }
            walk_block(&f.body, &file, w);
            if f.body.is_empty() {
                push(
                    w,
                    cat::W_EMPTY_BLOCK,
                    format!("function `{}` has an empty body", f.name),
                    f.span,
                    "Implement the body or remove the function.".to_string(),
                    &file,
                );
            }
            let _ = decls;
        }
        Stmt::Socket(s) => {
            if let Some(b) = &s.connect {
                walk_block(b, &file, w);
            }
            if let Some(b) = &s.message {
                walk_block(b, &file, w);
            }
            if let Some(b) = &s.disconnect {
                walk_block(b, &file, w);
            }
        }
        _ => {}
    }
}

/// Walk one lexical block: detect unused locals, constant conditions,
/// always-true/false comparisons and unreachable statements, then recurse.
fn walk_block(stmts: &[Stmt], file: &Option<String>, w: &mut Warn) {
    let mut local = Vec::<(String, Span)>::new();
    let mut referenced: HashSet<String> = HashSet::new();
    for st in stmts {
        refs_in_stmt(st, &mut referenced);
    }
    let mut dead = false;
    for st in stmts {
        if dead {
            push(
                w,
                cat::W_UNREACHABLE_STATEMENT,
                "unreachable statement".to_string(),
                st.span(),
                "Remove it, or move it before the unconditional return.".to_string(),
                file,
            );
            // Only report the first one so a long tail does not spam.
            dead = false;
        }
        match st {
            Stmt::Var(v) | Stmt::Const(v) => {
                local.push((v.name.clone(), v.span));
                check_expr(&v.value, file, w);
            }
            Stmt::If { cond, then_body, else_body, .. } => {
                constant_condition(cond, file, w);
                check_expr(cond, file, w);
                walk_block(then_body, file, w);
                walk_block(else_body, file, w);
                if then_body.is_empty() && else_body.is_empty() {
                    push(
                        w,
                        cat::W_EMPTY_BLOCK,
                        "empty conditional block".to_string(),
                        cond.span(),
                        "Add a body or remove the `?` branch.".to_string(),
                        file,
                    );
                }
            }
            Stmt::Loop { var, iter, body, span } => {
                check_expr(iter, file, w);
                let mut body_refs = HashSet::new();
                for s in body {
                    refs_in_stmt(s, &mut body_refs);
                }
                if !body_refs.contains(var) {
                    push(
                        w,
                        cat::W_UNUSED_VARIABLE,
                        format!("loop variable `{var}` is never read"),
                        *span,
                        format!("Use `{var}` in the loop body, or iterate without a binding."),
                        file,
                    );
                }
                walk_block(body, file, w);
            }
            Stmt::Return(e, _) => {
                check_expr(e, file, w);
                dead = true;
            }
            Stmt::Race(es, _) => {
                for e in es {
                    check_expr(e, file, w);
                }
            }
            Stmt::Expect { lhs, rhs, .. } => {
                check_expr(lhs, file, w);
                check_expr(rhs, file, w);
                binary_comparisons(lhs, file, w);
            }
            Stmt::ExprStmt(e) => check_expr(e, file, w),
            _ => {}
        }
    }
    // Unused locals: only when the name is never referenced anywhere in this
    // block (including nested blocks), and not exported.
    for (name, span) in &local {
        if !referenced.contains(name) {
            push(
                w,
                cat::W_UNUSED_VARIABLE,
                format!("local value `{name}` is never used"),
                *span,
                format!("Remove the declaration, or reference `{name}` later in the block."),
                file,
            );
        }
    }
}

/// Detect `?` conditions that fold to a constant and direct literal
/// comparisons that are always true or always false.
fn check_expr(e: &Expr, file: &Option<String>, w: &mut Warn) {
    match e {
        Expr::Binary(_, l, r, _) => {
            binary_comparisons(e, file, w);
            check_expr(l, file, w);
            check_expr(r, file, w);
        }
        Expr::Call { callee, args, .. } => {
            check_expr(callee, file, w);
            for a in args {
                check_expr(a, file, w);
            }
        }
        Expr::Member(base, _, _) => check_expr(base, file, w),
        Expr::Index(base, idx, _) => {
            check_expr(base, file, w);
            check_expr(idx, file, w);
        }
        Expr::Unary(_, x, _) => check_expr(x, file, w),
        Expr::Range(l, r, _) => {
            check_expr(l, file, w);
            check_expr(r, file, w);
        }
        Expr::Match(x, arms, _) => {
            check_expr(x, file, w);
            for a in arms {
                check_expr(&a.body, file, w);
            }
        }
        Expr::HttpCall { body, .. } => {
            if let Some(b) = body {
                check_expr(b, file, w);
            }
        }
        Expr::List(xs, _) => {
            for x in xs {
                check_expr(x, file, w);
            }
        }
        Expr::Obj(pairs, _) => {
            for (_, v) in pairs {
                check_expr(v, file, w);
            }
        }
        _ => {}
    }
}

/// Emit an always-true / always-false warning when both sides of a comparison
/// fold to constants.
fn binary_comparisons(e: &Expr, file: &Option<String>, w: &mut Warn) {
    let Expr::Binary(op, l, r, span) = e else {
        return;
    };
    use BinOp::*;
    if !matches!(op, Eq | Ne | Lt | Le | Gt | Ge) {
        return;
    }
    let (Some(a), Some(b)) = (fold(l), fold(r)) else {
        return;
    };
    let truth = compare(*op, &a, &b);
    let code = if truth {
        cat::W_ALWAYS_TRUE_COMPARISON
    } else {
        cat::W_ALWAYS_FALSE_COMPARISON
    };
    push(
        w,
        code,
        format!(
            "comparison is always {} because both operands are constant",
            if truth { "true" } else { "false" }
        ),
        *span,
        format!(
            "`{} {} {}` evaluates to `{}` at design time; replace with the result.",
            lit_repr(a),
            op.symbol(),
            lit_repr(b),
            if truth { "true" } else { "false" }
        ),
        file,
    );
}

/// Emit a constant-condition warning when a branch condition folds.
fn constant_condition(cond: &Expr, file: &Option<String>, w: &mut Warn) {
    let Some(f) = fold(cond) else {
        return;
    };
    let truth = match f {
        Folded::B(b) => b,
        Folded::I(n) => n != 0,
        Folded::F(x) => x != 0.0,
        _ => return, // strings/objects as conditions are odd; leave them
    };
    push(
        w,
        cat::W_CONSTANT_CONDITION,
        format!("condition is always {}", if truth { "true" } else { "false" }),
        cond.span(),
        "Branch is taken unconditionally; inline the body or drop the `?`.".to_string(),
        file,
    );
}

/// Constant-fold a literal-only expression. `None` when the expression
/// references anything at runtime (identifiers, calls, members).
fn fold(e: &Expr) -> Option<Folded> {
    match e {
        Expr::Int(n, _) => Some(Folded::I(*n)),
        Expr::Float(f, _) => Some(Folded::F(*f)),
        Expr::Bool(b, _) => Some(Folded::B(*b)),
        Expr::Str(s, _) => Some(Folded::S(s.clone())),
        Expr::Unary(un, x, _) => {
            use UnOp::*;
            let x = fold(x)?;
            match (un, x) {
                (Neg, Folded::I(n)) => Some(Folded::I(-n)),
                (Neg, Folded::F(f)) => Some(Folded::F(-f)),
                (Not, Folded::B(b)) => Some(Folded::B(!b)),
                _ => None,
            }
        }
        Expr::Binary(op, l, r, _) => {
            use BinOp::*;
            let (Some(a), Some(b)) = (fold(l), fold(r)) else {
                return None;
            };
            match (op, a, b) {
                (Add, Folded::I(x), Folded::I(y)) => Some(Folded::I(x.wrapping_add(y))),
                (Sub, Folded::I(x), Folded::I(y)) => Some(Folded::I(x.wrapping_sub(y))),
                (Mul, Folded::I(x), Folded::I(y)) => Some(Folded::I(x.wrapping_mul(y))),
                (Div, Folded::I(x), Folded::I(y)) => (y != 0).then(|| Folded::I(x.wrapping_div(y))),
                (Mod, Folded::I(x), Folded::I(y)) => (y != 0).then(|| Folded::I(x.wrapping_rem(y))),
                (Add, Folded::F(x), Folded::F(y)) => Some(Folded::F(x + y)),
                (Add, Folded::S(x), Folded::S(y)) => Some(Folded::S(format!("{x}{y}"))),
                (Eq, x, y) => Some(Folded::B(fold_eq(&x, &y))),
                (Ne, x, y) => Some(Folded::B(!fold_eq(&x, &y))),
                _ => None,
            }
        }
        _ => None,
    }
}

fn fold_eq(a: &Folded, b: &Folded) -> bool {
    match (a, b) {
        (Folded::I(x), Folded::I(y)) => x == y,
        (Folded::I(x), Folded::F(y)) => (*x as f64) == *y,
        (Folded::F(x), Folded::I(y)) => *x == (*y as f64),
        (Folded::F(x), Folded::F(y)) => x == y,
        (Folded::B(x), Folded::B(y)) => x == y,
        (Folded::S(x), Folded::S(y)) => x == y,
        _ => false,
    }
}

#[derive(Debug, Clone)]
enum Folded {
    I(i64),
    F(f64),
    B(bool),
    S(String),
}

fn lit_repr(f: Folded) -> String {
    match f {
        Folded::I(n) => n.to_string(),
        Folded::F(x) => x.to_string(),
        Folded::B(b) => b.to_string(),
        Folded::S(s) => format!("\"{s}\""),
    }
}

/// Evaluate a comparison of two folded values.
fn compare(op: BinOp, a: &Folded, b: &Folded) -> bool {
    use BinOp::*;
    match (op, a, b) {
        (Eq, x, y) => fold_eq(x, y),
        (Ne, x, y) => !fold_eq(x, y),
        (Lt, Folded::I(x), Folded::I(y)) => x < y,
        (Le, Folded::I(x), Folded::I(y)) => x <= y,
        (Gt, Folded::I(x), Folded::I(y)) => x > y,
        (Ge, Folded::I(x), Folded::I(y)) => x >= y,
        (Lt, Folded::I(x), Folded::F(y)) => (*x as f64) < *y,
        (Le, Folded::I(x), Folded::F(y)) => (*x as f64) <= *y,
        (Gt, Folded::I(x), Folded::F(y)) => (*x as f64) > *y,
        (Ge, Folded::I(x), Folded::F(y)) => (*x as f64) >= *y,
        (Lt, Folded::F(x), Folded::I(y)) => *x < (*y as f64),
        (Le, Folded::F(x), Folded::I(y)) => *x <= (*y as f64),
        (Gt, Folded::F(x), Folded::I(y)) => *x > (*y as f64),
        (Ge, Folded::F(x), Folded::I(y)) => *x >= (*y as f64),
        (Lt, Folded::F(x), Folded::F(y)) => x < y,
        (Le, Folded::F(x), Folded::F(y)) => x <= y,
        (Gt, Folded::F(x), Folded::F(y)) => x > y,
        (Ge, Folded::F(x), Folded::F(y)) => x >= y,
        (Lt, Folded::S(x), Folded::S(y)) => x < y,
        (Le, Folded::S(x), Folded::S(y)) => x <= y,
        (Gt, Folded::S(x), Folded::S(y)) => x > y,
        (Ge, Folded::S(x), Folded::S(y)) => x >= y,
        _ => false, // cross-kind relationals stay unclassified
    }
}

/// Collect all identifier names referenced anywhere inside a statement
/// (used for whole-program global usage).
fn refs_in_stmt(st: &Stmt, used: &mut HashSet<String>) {
    match st {
        Stmt::Var(v) | Stmt::Const(v) => refs_in_expr(&v.value, used),
        Stmt::Route(r) => {
            for p in &r.params {
                used.insert(p.name.clone());
            }
            for s in &r.body {
                refs_in_stmt(s, used);
            }
        }
        Stmt::Middleware { body, .. } => {
            for s in body {
                refs_in_stmt(s, used);
            }
        }
        Stmt::Func(f) => {
            for s in &f.body {
                refs_in_stmt(s, used);
            }
        }
        Stmt::Socket(s) => {
            for b in [&s.connect, &s.message, &s.disconnect].into_iter().flatten() {
                for st in b {
                    refs_in_stmt(st, used);
                }
            }
        }
        Stmt::Test(t) => {
            for s in &t.body {
                refs_in_stmt(s, used);
            }
        }
        Stmt::If { cond, then_body, else_body, .. } => {
            refs_in_expr(cond, used);
            for s in then_body {
                refs_in_stmt(s, used);
            }
            for s in else_body {
                refs_in_stmt(s, used);
            }
        }
        Stmt::Loop { var, iter, body, .. } => {
            used.insert(var.clone());
            refs_in_expr(iter, used);
            for s in body {
                refs_in_stmt(s, used);
            }
        }
        Stmt::Return(e, _) => refs_in_expr(e, used),
        Stmt::Race(es, _) => {
            for e in es {
                refs_in_expr(e, used);
            }
        }
        Stmt::Expect { lhs, rhs, .. } => {
            refs_in_expr(lhs, used);
            refs_in_expr(rhs, used);
        }
        Stmt::ExprStmt(e) => refs_in_expr(e, used),
        _ => {}
    }
}

/// Collect every identifier name referenced inside an expression.
fn refs_in_expr(e: &Expr, used: &mut HashSet<String>) {
    match e {
        Expr::Ident(n, _) => {
            used.insert(n.clone());
        }
        Expr::Member(base, _, _) => refs_in_expr(base, used),
        Expr::Index(base, idx, _) => {
            refs_in_expr(base, used);
            refs_in_expr(idx, used);
        }
        Expr::Call { callee, args, .. } => {
            refs_in_expr(callee, used);
            for a in args {
                refs_in_expr(a, used);
            }
        }
        Expr::Unary(_, x, _) => refs_in_expr(x, used),
        Expr::Binary(_, l, r, _) => {
            refs_in_expr(l, used);
            refs_in_expr(r, used);
        }
        Expr::Range(l, r, _) => {
            refs_in_expr(l, used);
            refs_in_expr(r, used);
        }
        Expr::Match(x, arms, _) => {
            refs_in_expr(x, used);
            for a in arms {
                refs_in_expr(&a.body, used);
            }
        }
        Expr::HttpCall { body, .. } => {
            if let Some(b) = body {
                refs_in_expr(b, used);
            }
        }
        Expr::List(xs, _) => {
            for x in xs {
                refs_in_expr(x, used);
            }
        }
        Expr::Obj(pairs, _) => {
            for (_, v) in pairs {
                refs_in_expr(v, used);
            }
        }
        _ => {}
    }
}

/// A selectable/deniable view of the warning catalog (M3.4.4).
///
/// Default policy enables every warning and denies none, so a build always
/// stays green. `--deny <spec>` escalates matching warnings to hard errors
/// (the CL renders them as `error[HSxxxx]` and fails the build); `--warnings
/// <spec>` narrows the set that is shown. `--deny all` escalates everything.
#[derive(Debug, Clone, Default)]
pub struct WarningPolicy {
    /// `None` = all warnings enabled; `Some(set)` = only those codes.
    pub enabled: Option<HashSet<u16>>,
    /// Codes escalated to hard errors.
    pub deny: HashSet<u16>,
}

impl WarningPolicy {
    /// Every warning shown, none denied.
    pub fn all() -> Self {
        Self::default()
    }

    /// Split a warning batch into what should be shown and what must be
    /// promoted to hard errors by `--deny`. The latter is a subset of the
    /// former, already flagged with `with_promote`.
    pub fn filter(&self, warnings: Vec<Diag>) -> (Vec<Diag>, Vec<Diag>) {
        let mut shown = Vec::new();
        let mut promoted = Vec::new();
        for mut d in warnings {
            if !self.is_enabled(d.code) {
                continue;
            }
            if self.deny.contains(&d.code) {
                d.notes.push("promoted to a hard error by --deny".to_string());
                d.promote = true;
                promoted.push(d.clone());
            }
            shown.push(d);
        }
        (shown, promoted)
    }

    fn is_enabled(&self, code: u16) -> bool {
        self.enabled.as_ref().map_or(true, |s| s.contains(&code))
    }
}

/// Resolve one warning selector to its catalog code.
///
/// Accepts, case-insensitively: the catalog name (`unused-variable`),
/// the const-style spelling (`W_UNUSED_VARIABLE`), or the code itself
/// (`2001`, `HS2001`, `hs2001`). Fails on anything unknown so the CLI can
/// surface a usage error instead of silently ignoring a typo.
pub fn resolve_selector(spec: &str) -> Result<u16, String> {
    let s = spec.trim();
    if s.is_empty() {
        return Err("empty warning selector".to_string());
    }
    if let Some(num) = s.strip_prefix("HS").or_else(|| s.strip_prefix("hs")) {
        return parse_warning_number(num);
    }
    if let Ok(n) = s.parse::<u16>() {
        return parse_warning_number(&n.to_string()).or_else(|_| Err(format!("unknown warning `{spec}`")));
    }
    let norm = s.to_lowercase().replace('_', "-");
    let norm = norm.strip_prefix("w-").unwrap_or(&norm).to_string();
    crate::catalog::WARNINGS
        .iter()
        .find(|(n, _)| n == &norm)
        .map(|(_, c)| *c)
        .ok_or_else(|| format!("unknown warning `{spec}`; see `hard errors` for warning codes"))
}

fn parse_warning_number(num: &str) -> Result<u16, String> {
    let n = num
        .parse::<u16>()
        .map_err(|_| format!("invalid warning code `{num}`"))?;
    if !(2000..=2099).contains(&n) || crate::catalog::lookup(n).is_none() {
        return Err(format!("{n} is not a warning code"));
    }
    Ok(n)
}

/// Build a policy from raw CLI values, e.g. `--warnings unused-variable,2002`
/// and `--deny all`. `None` values (flag absent) mean "default".
pub fn parse_policy(
    enabled_spec: Option<&str>,
    deny_spec: Option<&str>,
) -> Result<WarningPolicy, String> {
    let mut policy = WarningPolicy::all();
    if let Some(spec) = enabled_spec {
        let trimmed = spec.trim();
        if trimmed.eq_ignore_ascii_case("all") {
            policy.enabled = None;
        } else if trimmed.eq_ignore_ascii_case("none") {
            policy.enabled = Some(HashSet::new());
        } else {
            let mut set = HashSet::new();
            for part in trimmed.split(',') {
                set.insert(resolve_selector(part)?);
            }
            policy.enabled = Some(set);
        }
    }
    if let Some(spec) = deny_spec {
        let trimmed = spec.trim();
        if trimmed.eq_ignore_ascii_case("all") {
            for (_, code) in crate::catalog::WARNINGS.iter().copied() {
                policy.deny.insert(code);
            }
        } else {
            for part in trimmed.split(',') {
                policy.deny.insert(resolve_selector(part)?);
            }
        }
    }
    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog as cat;

    fn warnings(src: &str) -> Vec<Diag> {
        let (toks, _) = crate::lex(src);
        let toks = toks.unwrap();
        let prog = crate::parse(toks).unwrap();
        analyze(&prog, &[])
    }

    fn codes(src: &str) -> Vec<u16> {
        warnings(src).iter().map(|d| d.code).collect()
    }

    #[test]
    fn unused_local_is_reported() {
        assert_eq!(
            codes("GET \"/\" :: {\n  unused ::= 1\n  <- 0\n}\n"),
            vec![cat::W_UNUSED_VARIABLE]
        );
    }

    #[test]
    fn used_local_is_silent() {
        assert_eq!(
            codes("GET \"/\" :: {\n  used ::= 1\n  <- used\n}\n"),
            Vec::<u16>::new()
        );
    }

    #[test]
    fn unused_loop_var_is_reported() {
        let c = codes("GET \"/\" :: {\n  loop i => [1, 2] { <- 0 }\n}\n");
        assert!(
            c.contains(&cat::W_UNUSED_VARIABLE),
            "missing unused-loop-var warning, got {c:?}"
        );
    }

    #[test]
    fn constant_condition_and_comparisons() {
        let c = codes("GET \"/\" :: {\n  ?(true) { <- 0 }\n  <- (1 == 2)\n}\n");
        assert!(c.contains(&cat::W_CONSTANT_CONDITION));
        assert!(c.contains(&cat::W_ALWAYS_FALSE_COMPARISON));
    }

    #[test]
    fn unreachable_statement_is_reported() {
        assert_eq!(
            codes("GET \"/\" :: {\n  <- 1\n  <- 2\n}\n"),
            vec![cat::W_UNREACHABLE_STATEMENT]
        );
    }

    #[test]
    fn empty_function_body_is_reported() {
        assert_eq!(
            codes("calc nothing() => Int {\n}\n"),
            vec![cat::W_EMPTY_BLOCK]
        );
    }

    #[test]
    fn dynamic_comparisons_are_silent() {
        let c = codes("GET \"/\" :: {\n  a ::= 1\n  <- (a == 2)\n}\n");
        assert!(!c.contains(&cat::W_ALWAYS_FALSE_COMPARISON));
    }

    #[test]
    fn selectors_resolve_case_and_aliases() {
        assert_eq!(resolve_selector("unused-variable").unwrap(), 2001);
        assert_eq!(resolve_selector("W_UNUSED_VARIABLE").unwrap(), 2001);
        assert_eq!(resolve_selector("unused_variable").unwrap(), 2001);
        assert_eq!(resolve_selector("2001").unwrap(), 2001);
        assert_eq!(resolve_selector("HS2001").unwrap(), 2001);
        assert!(resolve_selector("0201").is_err(), "codes outside the warning range must fail");
        assert!(resolve_selector("nonsense").is_err());
        assert!(resolve_selector("2005").is_ok());
    }

    #[test]
    fn deny_promotes_matching_warnings_only() {
        let ws = warnings("GET \"/\" :: {\n  unused ::= 1\n  ?(true) { <- 0 }\n  <- 0\n}\n");
        assert_eq!(ws.len(), 2);
        let policy = parse_policy(None, Some("constant-condition")).unwrap();
        let (shown, promoted) = policy.filter(ws);
        assert_eq!(shown.len(), 2);
        assert_eq!(promoted.len(), 1);
        assert_eq!(promoted[0].code, cat::W_CONSTANT_CONDITION);
        assert!(promoted[0].promote);
        assert!(!shown.iter().find(|d| d.code == cat::W_UNUSED_VARIABLE).unwrap().promote);
    }

    #[test]
    fn enable_narrows_and_none_silences() {
        let ws = warnings("GET \"/\" :: {\n  unused ::= 1\n  ?(true) { <- 0 }\n  <- 0\n}\n");
        let policy = parse_policy(Some("always-false-comparison,2001"), None).unwrap();
        let (shown, promoted) = policy.filter(ws.clone());
        // only 2001 (unused local) is in the enabled set
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].code, cat::W_UNUSED_VARIABLE);
        assert!(promoted.is_empty());
        let silent = parse_policy(Some("none"), Some("all")).unwrap();
        let (shown, promoted) = silent.filter(ws.clone());
        assert!(shown.is_empty());
        assert!(promoted.is_empty());
    }
}