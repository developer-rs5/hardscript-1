use crate::ast::*;
use crate::catalog as cat;
use crate::error::{Diag, ErrorKind};
use crate::lexer::Lexer;
use crate::token::{Kw, Span, Sym, Tok, Token};

pub struct Parser {
    toks: Vec<Token>,
    pos: usize,
    depth: usize,
    fatal: bool,
    pub imports: Vec<Module>,
    /// Local module imports (`bring "./utils"`) resolved against the project
    /// root at module-graph time. Stored as written (no `.hard` extension).
    pub import_paths: Vec<String>,
    pub app_port: Option<i64>,
    pub models: Vec<ModelDef>,
    pub routes: Vec<RouteDef>,
    pub sockets: Vec<SocketDef>,
    pub funcs: Vec<FunDef>,
    /// Declared background jobs (`job Name(..)`), in order: name to payload
    /// parameters. `worker` blocks desugar against this table, so a job must
    /// be declared above the worker that handles it.
    pub jobs: Vec<(String, Vec<JobParam>)>,
    pub middlewares: Vec<(String, Vec<Stmt>)>,
    pub tests: Vec<TestDef>,
    /// Non-fatal parse errors recovered inside statement regions (blocks,
    /// route/func/middleware/test bodies). Collected here so one bad statement
    /// no longer swallows the rest of its enclosing region or the whole file;
    /// surfaced at program level alongside the region's own error.
    na_errs: Vec<Diag>,
}

/// Maximum recursion depth for `parse_expr`, `parse_unary` and `parse_block`,
/// chosen below the stack limit where deep nesting would abort the compiler
/// process with a stack overflow. Dev builds are unoptimized with much larger
/// frames (measured overflow at ~450 nested parens), so the limit is
/// deliberately conservative; the formatter/codegen also recurse over the AST.
const MAX_DEPTH: usize = 256;

/// Maximum number of operators/members folded into one expression by the
/// iterative (loop-built) parse paths (`parse_or` … `parse_postfix`). These
/// build left-deep ASTs that (a) overflow the stack when later traversed
/// (type check, codegen, recursive drop) and (b) make `g++ -O2` degrade
/// super-linearly on the emitted C++ (a 2047-member chain took >25 s to
/// compile). Unbounded chains are rejected with a diagnostic.
const MAX_CHAIN: usize = 256;

impl Parser {
    pub fn new(toks: Vec<Token>) -> Parser {
        Parser {
            toks,
            pos: 0,
            depth: 0,
            fatal: false,
            imports: Vec::new(),
            import_paths: Vec::new(),
            app_port: None,
            models: Vec::new(),
            routes: Vec::new(),
            sockets: Vec::new(),
            funcs: Vec::new(),
            jobs: Vec::new(),
            middlewares: Vec::new(),
            tests: Vec::new(),
            na_errs: Vec::new(),
        }
    }

    fn enter_depth(&mut self, what: &str) -> Result<(), Vec<Diag>> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth = 0;
            self.fatal = true;
            return Err(vec![Diag::new(
                ErrorKind::Parse,
                format!("{what} nesting too deep (limit {MAX_DEPTH})"),
                self.span(),
                format!("Simplify the nesting below {MAX_DEPTH} levels."),
            )
            .with_code(cat::NESTING_TOO_DEEP)]);
        }
        Ok(())
    }

    fn exit_depth(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn chain_ok(&mut self, count: usize) -> Result<(), Vec<Diag>> {
        if count > MAX_CHAIN {
            self.fatal = true;
            self.depth = 0;
            return Err(vec![Diag::new(
                ErrorKind::Parse,
                format!("expression too long (over {MAX_CHAIN} operands)"),
                self.span(),
                "Break the expression into smaller pieces or use variables.",
            )
            .with_code(cat::EXPRESSION_TOO_LONG)]);
        }
        Ok(())
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)].tok
    }

    fn peek_at(&self, off: usize) -> &Tok {
        &self.toks[(self.pos + off).min(self.toks.len() - 1)].tok
    }

    fn span(&self) -> Span {
        self.toks[self.pos.min(self.toks.len() - 1)].span
    }

    fn advance(&mut self) -> Token {
        let t = self.toks[self.pos.min(self.toks.len() - 1)].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn eat_sym(&mut self, s: Sym) -> bool {
        if *self.peek() == Tok::Sym(s) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, k: Kw) -> bool {
        if *self.peek() == Tok::Kw(k) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_sym(&mut self, s: Sym, ctx: &str) -> Result<(Span, Span), Vec<Diag>> {
        let sp = self.span();
        if self.eat_sym(s) {
            Ok((sp, self.prev_span()))
        } else {
            Err(vec![Diag::new(
                ErrorKind::Parse,
                format!("expected '{}' {ctx}", s.as_str()),
                sp,
                format!("Add the missing '{}'.", s.as_str()),
            )
            .with_code(cat::EXPECTED_TOKEN)])
        }
    }

    fn expect_ident(&mut self, ctx: &str) -> Result<(String, Span), Vec<Diag>> {
        let sp = self.span();
        match self.peek() {
            Tok::Ident(s) => {
                let s = s.clone();
                self.advance();
                Ok((s, sp))
            }
            other => Err(vec![Diag::new(
                ErrorKind::Parse,
                format!("expected an identifier {ctx}"),
                sp,
                "Use a name like `users`, `total`, or `handle_request`.",
            )
            .with_expected("an identifier")
            .with_received(format!("`{other}`"))
            .with_code(cat::EXPECTED_TOKEN)]),
        }
    }

    fn member_name(&mut self) -> Result<(String, Span), Vec<Diag>> {
        let sp = self.span();
        match self.peek() {
            Tok::Ident(_) | Tok::Kw(_) => {
                let tok = self.peek().clone();
                self.advance();
                let name = match tok {
                    Tok::Ident(s) => s,
                    Tok::Kw(k) => format!("{:?}", k).to_lowercase(),
                    _ => unreachable!(),
                };
                Ok((name, sp))
            }
            _ => Err(vec![Diag::new(
                ErrorKind::Parse,
                format!("expected a name after `.`"),
                sp,
                "Use a name like `users`, `total`, or `handle_request`.",
            )
            .with_code(cat::EXPECTED_TOKEN)]),
        }
    }

    fn prev_span(&self) -> Span {
        if self.pos == 0 {
            Span::new(1, 1)
        } else {
            self.toks[self.pos - 1].span
        }
    }

    pub fn parse_program(mut self) -> Result<Program, Vec<Diag>> {
        let mut stmts = Vec::new();
        let mut errs: Vec<Diag> = Vec::new();
        loop {
            match *self.peek() {
                Tok::Eof => break,
                Tok::Sym(Sym::RBrace) => {
                    let sp = self.span();
                    errs.push(Diag::new(
                        ErrorKind::Parse,
                        "unexpected closing `}`",
                        sp,
                        "Remove the extra `}` or close the block where it belongs.",
                    )
                    .with_code(cat::UNEXPECTED_TOKEN));
                    self.advance();
                }
                Tok::Kw(_) | Tok::Ident(_) | Tok::Sym(_) => {
                    match self.parse_stmt() {
                        Ok(Some(stmt)) => stmts.push(stmt),
                        Ok(None) => {}
                        Err(e) => {
                            errs.extend(e);
                            if self.fatal {
                                break;
                            }
                            self.advance();
                            self.recover();
                        }
                    }
                }
                _ => {
                    let sp = self.span();
                    errs.push(Diag::new(
                        ErrorKind::Parse,
                        "unexpected token",
                        sp,
                        "Remove the token or write a complete statement.",
                    )
                    .with_code(cat::UNEXPECTED_TOKEN));
                    self.advance();
                }
            }
        }
        errs.append(&mut self.na_errs);
        if errs.is_empty() {
            Ok(Program { stmts, path: String::new(), models: std::mem::take(&mut self.models) })
        } else {
            Err(errs)
        }
    }

    fn recover(&mut self) {
        if self.fatal {
            return;
        }
        self.sync();
    }

    /// Resync the scan to the next plausible statement boundary so recovery
    /// stays inside the current region instead of abandoning the file. A token
    /// is a plausible start when it introduces a statement: any keyword, an
    /// identifier/declaration, a literal, or an expression-opening symbol.
    /// `}` / EOF are terminators because they close (or end) the region.
    fn sync(&mut self) {
        loop {
            match self.peek() {
                Tok::Eof | Tok::Sym(Sym::RBrace) => return,
                Tok::Kw(_) | Tok::Ident(_) | Tok::Str(_) | Tok::Int(_) | Tok::Float(_)
                | Tok::Sym(Sym::Q) | Tok::Sym(Sym::LParen) | Tok::Sym(Sym::Minus) => return,
                _ => {
                    self.advance();
                }
            }
        }
    }

    fn parse_stmt(&mut self) -> Result<Option<Stmt>, Vec<Diag>> {
        let sp = self.span();
        match self.peek() {
            Tok::Kw(Kw::Bring) => {
                self.advance();
                match self.advance().tok {
                    Tok::Ident(name) => self.parse_bring_ident(name, sp),
                    Tok::Str(path) => self.parse_bring_str(path, sp),
                    other => Err(vec![Diag::new(
                        ErrorKind::Parse,
                        format!("expected a module name or path after `bring`, found {other}"),
                        sp,
                        "Use `bring http`, `bring \"./utils\"`, or `bring std.crypto`.",
                    )
                    .with_code(cat::EXPECTED_TOKEN)]),
                }
            }
            Tok::Kw(Kw::App) => {
                self.advance();
                self.expect_sym(Sym::At, "after `app`")?;
                let port = match self.advance().tok {
                    Tok::Int(n) => n,
                    _ => {
                        return Err(vec![Diag::new(
                            ErrorKind::Parse,
                            "expected a port number after `app @`",
                            self.span(),
                            "Use `app @3000`.",
                        )
                        .with_code(cat::EXPECTED_TOKEN)])
                    }
                };
                self.app_port = Some(port);
                Ok(Some(Stmt::App(port, sp)))
            }
            Tok::Kw(Kw::Model) => self.parse_model().map(Some),
            Tok::Kw(Kw::Get) | Tok::Kw(Kw::Post) | Tok::Kw(Kw::Put) | Tok::Kw(Kw::Delete)
            | Tok::Kw(Kw::Patch) => {
                // Route if followed by `::`; otherwise an in-test HTTP expression.
                if *self.peek_at(2) == Tok::Sym(Sym::DColon) {
                    self.parse_route().map(Some)
                } else {
                    let e = self.parse_expr()?;
                    Ok(Some(Stmt::ExprStmt(e)))
                }
            }
            Tok::Kw(Kw::Socket) => self.parse_socket().map(Some),
            Tok::Kw(Kw::Before) => {
                self.advance();
                let (name, _) = self.expect_ident("as a middleware name")?;
                self.expect_sym(Sym::DColon, "before the middleware body")?;
                let body = self.parse_block()?;
                let name = name.clone();
                self.middlewares.push((name.clone(), body.clone()));
                Ok(Some(Stmt::Middleware { name, body, span: sp }))
            }
            Tok::Kw(Kw::Protect) => {
                self.advance();
                self.parse_protect(sp).map(Some)
            }
            Tok::Kw(Kw::Calc) => {
                self.advance();
                self.parse_func(false).map(Some)
            }
            Tok::Kw(Kw::Async) => {
                self.advance();
                if !self.eat_kw(Kw::Calc) {
                    return Err(vec![Diag::new(
                        ErrorKind::Parse,
                        "expected `calc` after `async`",
                        self.span(),
                        "Write `async calc name(...) => ... { ... }`.",
                    )
                    .with_code(cat::EXPECTED_TOKEN)]);
                }
                self.parse_func(true).map(Some)
            }
            Tok::Kw(Kw::Test) => {
                self.advance();
                let (name, _) = match *self.peek() {
                    Tok::Str(ref s) => {
                        let s = s.clone();
                        self.advance();
                        (s, self.prev_span())
                    }
                    _ => {
                        return Err(vec![Diag::new(
                            ErrorKind::Parse,
                            "expected a test name string after `test`",
                            self.span(),
                            "Write `test \"Name\" { ... }`.",
                        )
                        .with_code(cat::EXPECTED_TOKEN)])
                    }
                };
                let body = self.parse_block()?;
                let td = TestDef { name, body: body.clone(), span: sp };
                self.tests.push(td.clone());
                Ok(Some(Stmt::Test(td)))
            }
            _ => self.parse_block_stmt().map(Some),
        }
    }

    /// A duration after `ttl`: a number and a unit (`10m`, `1h`, `30s`, `7d`).
    /// The lexer sees `10m` as two tokens, which is what makes the units
    /// table here instead of a new literal.
    fn parse_duration(&mut self, ctx: &str) -> Result<i64, Vec<Diag>> {
        let sp = self.span();
        let n = match self.peek() {
            Tok::Int(n) => {
                let n = *n;
                self.advance();
                n
            }
            other => {
                return Err(vec![Diag::new(
                    ErrorKind::Parse,
                    format!("expected a number {ctx}, found {other}"),
                    sp,
                    "Write a duration like `10m`, `1h`, `30s` or `7d`.",
                )
                .with_code(cat::EXPECTED_TOKEN)])
            }
        };
        if n < 0 {
            return Err(vec![Diag::new(
                ErrorKind::Parse,
                format!("a duration {ctx} cannot be negative"),
                sp,
                "Write a duration like `10m`, `1h`, `30s` or `7d`.",
            )
            .with_code(cat::EXPECTED_TOKEN)]);
        }
        let unit = match self.peek() {
            Tok::Ident(u) => {
                let u = u.clone();
                self.advance();
                u.to_lowercase()
            }
            other => {
                return Err(vec![Diag::new(
                    ErrorKind::Parse,
                    format!("expected a duration unit {ctx}, found {other}"),
                    self.span(),
                    "Use `s`, `m`, `h` or `d`: `10m`, `1h`, `30s`, `7d`.",
                )
                .with_code(cat::EXPECTED_TOKEN)])
            }
        };
        let mult: i64 = match unit.as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hour" | "hours" => 3600,
            "d" | "day" | "days" => 86400,
            _ => {
                return Err(vec![Diag::new(
                    ErrorKind::Parse,
                    format!("unknown duration unit `{unit}`"),
                    self.span(),
                    "Use `s`, `m`, `h` or `d`: `10m`, `1h`, `30s`, `7d`.",
                )
                .with_code(cat::EXPECTED_TOKEN)])
            }
        };
        n.checked_mul(mult).ok_or_else(|| {
            vec![Diag::new(
                ErrorKind::Parse,
                "duration too large".to_string(),
                sp,
                "Use a smaller number of seconds, minutes, hours or days.",
            )
            .with_code(cat::EXPECTED_TOKEN)]
        })
    }

    /// `job SendEmail(User user, Int retries)`: declare a background job type.
    /// Parameter types are identifiers (a model or a primitive); arity is what
    /// matters downstream, and the runtime checks it on enqueue.
    fn parse_job(&mut self) -> Result<Stmt, Vec<Diag>> {
        let sp = self.span();
        self.advance(); // `job`
        let (name, _) = self.expect_ident("as a job name")?;
        self.expect_sym(Sym::LParen, "to start the job parameters")?;
        let mut params = Vec::new();
        while !matches!(self.peek(), Tok::Sym(Sym::RParen)) {
            if *self.peek() == Tok::Eof {
                break;
            }
            let psp = self.span();
            let ty = match self.peek().clone() {
                Tok::Ident(t) => {
                    let t = t.clone();
                    self.advance();
                    t
                }
                other => {
                    return Err(vec![Diag::new(
                        ErrorKind::Parse,
                        format!("expected a parameter type, found {other}"),
                        self.span(),
                        "Write parameters as `Type name`, e.g. `job SendEmail(User user)`.",
                    )
                    .with_code(cat::EXPECTED_TOKEN)])
                }
            };
            let (pname, _) = self.expect_ident("as a parameter name")?;
            params.push(JobParam { ty, name: pname, span: psp });
            if !self.eat_sym(Sym::Comma) {
                break;
            }
        }
        self.expect_sym(Sym::RParen, "to close the job parameters")?;
        if self.jobs.iter().any(|(n, _)| n == &name) {
            return Err(vec![Diag::new(
                ErrorKind::Parse,
                format!("job `{name}` is declared twice"),
                sp,
                "Declare each job once; enqueue it as often as needed.",
            )
            .with_code(cat::EXPECTED_TOKEN)]);
        }
        self.jobs.push((name.clone(), params.clone()));
        Ok(Stmt::Job(JobDef { name, params, span: sp }))
    }

    /// `queue SendEmail(user, delay = 60)`: enqueue one job. Desugars to an
    /// ordinary `queue.enqueue("SendEmail", [user], {delay: 60})` call, so no
    /// new expression form is needed in either position. Trailing `name =
    /// value` pairs are options (`delay` seconds, `priority`, `max_attempts`);
    /// anything else is a positional payload argument.
    fn parse_enqueue_call(&mut self) -> Result<Expr, Vec<Diag>> {
        let sp = self.span();
        self.advance(); // `queue`
        let (job, _) = self.expect_ident("as a job name")?;
        if !self.jobs.iter().any(|(n, _)| n == &job) {
            return Err(vec![Diag::new(
                ErrorKind::Parse,
                format!("queue target `{job}` is not a declared job"),
                sp,
                format!("Declare it first: `job {job}(...)`."),
            )
            .with_code(cat::EXPECTED_TOKEN)]);
        }
        self.expect_sym(Sym::LParen, "to start the job arguments")?;
        let mut args = Vec::new();
        let mut options = Vec::new();
        while !matches!(self.peek(), Tok::Sym(Sym::RParen)) {
            if *self.peek() == Tok::Eof {
                break;
            }
            // An option is `name = expr`; anything else is positional. `=` never
            // appears inside an expression, so the lookahead is exact.
            if let Tok::Ident(opt) = self.peek().clone() {
                if matches!(self.peek_at(1), Tok::Sym(Sym::Assign)) {
                    let opt = opt.clone();
                    self.advance();
                    self.advance();
                    if !matches!(opt.as_str(), "delay" | "priority" | "max_attempts") {
                        return Err(vec![Diag::new(
                            ErrorKind::Parse,
                            format!("unknown queue option `{opt}`"),
                            self.span(),
                            "The options are `delay` (seconds), `priority` and `max_attempts`.",
                        )
                        .with_code(cat::EXPECTED_TOKEN)]);
                    }
                    let v = self.parse_expr()?;
                    options.push((opt, v));
                    if !self.eat_sym(Sym::Comma) {
                        break;
                    }
                    continue;
                }
            }
            args.push(self.parse_expr()?);
            if !self.eat_sym(Sym::Comma) {
                break;
            }
        }
        self.expect_sym(Sym::RParen, "to close the job arguments")?;
        let callee = Expr::Member(
            Box::new(Expr::Ident("queue".to_string(), sp)),
            "enqueue".to_string(),
            sp,
        );
        Ok(Expr::Call {
            callee: Box::new(callee),
            args: vec![
                Expr::Str(job, sp),
                Expr::List(args, sp),
                Expr::Obj(options, sp),
            ],
            span: sp,
        })
    }

    /// `worker SendEmail { ... }`: handle a job type. Lowers to an ordinary
    /// function taking the payload, with each declared parameter bound up
    /// front, so the rest of the pipeline never learns a new statement form.
    /// The job must be declared above: its parameter names become the body's
    /// bindings. Codegen registers every `__worker_*` function in `main`.
    /// Workers live at the top level, like the functions they lower to.
    fn parse_worker(&mut self) -> Result<Stmt, Vec<Diag>> {
        let sp = self.span();
        self.advance(); // `worker`
        let (job, _) = self.expect_ident("as a job name")?;
        let Some((_, params)) = self.jobs.iter().find(|(n, _)| n == &job).cloned() else {
            return Err(vec![Diag::new(
                ErrorKind::Parse,
                format!("worker for undeclared job `{job}`"),
                sp,
                format!("Declare it first: `job {job}(...)` above the worker."),
            )
            .with_code(cat::EXPECTED_TOKEN)]);
        };
        let body = self.parse_block()?;
        let fname = format!("__worker_{job}");
        // Bind each payload field before the body runs: `user` reads naturally
        // instead of through a payload object.
        let mut prelude = Vec::with_capacity(params.len());
        for p in &params {
            prelude.push(Stmt::Var(VarDef {
                name: p.name.clone(),
                ty: Some(p.ty.clone()),
                value: Expr::Member(
                    Box::new(Expr::Ident("__payload".to_string(), p.span)),
                    p.name.clone(),
                    p.span,
                ),
                span: p.span,
            }));
        }
        let mut full = prelude;
        full.extend(body);
        let fd = FunDef {
            name: fname,
            ret: None,
            params: vec![FunParam { ty: None, name: "__payload".to_string(), is_body: false, span: sp }],
            body: full,
            span: sp,
        };
        self.funcs.push(fd.clone());
        Ok(Stmt::Func(fd))
    }

    fn parse_block_stmt(&mut self) -> Result<Stmt, Vec<Diag>> {
        let sp = self.span();
        // `job Name(T p, ...)`, `queue Name(args, opt = v)` and
        // `worker Name { ... }` are recognized by shape: the leading keyword
        // plus what follows it. Anything else starting with these words parses
        // as an expression, exactly as before.
        if matches!(self.peek(), Tok::Ident(n) if n == "job") {
            if matches!(self.peek_at(1), Tok::Ident(_)) && matches!(self.peek_at(2), Tok::Sym(Sym::LParen)) {
                return self.parse_job();
            }
        }
        if matches!(self.peek(), Tok::Ident(n) if n == "queue") {
            if matches!(self.peek_at(1), Tok::Ident(_)) && matches!(self.peek_at(2), Tok::Sym(Sym::LParen)) {
                let e = self.parse_enqueue_call()?;
                return Ok(Stmt::ExprStmt(e));
            }
        }
        if matches!(self.peek(), Tok::Ident(n) if n == "worker") {
            if matches!(self.peek_at(1), Tok::Ident(_)) && matches!(self.peek_at(2), Tok::Sym(Sym::LBrace)) {
                return self.parse_worker();
            }
        }
        // `cache users ttl 10m`: declare a cache entry's default TTL. It
        // desugars to an ordinary `cache.declare("users", 600)` call, so the
        // rest of the pipeline never learns a new statement form. The four
        // token lookahead keeps it unambiguous: anything else starting with
        // `cache` parses as an expression, exactly as before.
        if matches!(self.peek(), Tok::Ident(n) if n == "cache") {
            if let Tok::Ident(name) = self.peek_at(1).clone() {
                if matches!(self.peek_at(2), Tok::Ident(t) if t == "ttl") {
                    self.advance();
                    self.advance();
                    self.advance();
                    let secs = self.parse_duration("for a cache TTL")?;
                    let callee = Expr::Member(
                        Box::new(Expr::Ident("cache".to_string(), sp)),
                        "declare".to_string(),
                        sp,
                    );
                    return Ok(Stmt::ExprStmt(Expr::Call {
                        callee: Box::new(callee),
                        args: vec![Expr::Str(name, sp), Expr::Int(secs, sp)],
                        span: sp,
                    }));
                }
            }
        }
        // `?(cond) { } :{ }` or `?(cond) { }`
        if *self.peek() == Tok::Sym(Sym::Q) && *self.peek_at(1) == Tok::Sym(Sym::LParen) {
            self.advance();
            self.advance();
            let cond = self.parse_expr()?;
            self.expect_sym(Sym::RParen, "after condition")?;
            let then_body = self.parse_block()?;
            let mut else_body = Vec::new();
            if *self.peek() == Tok::Sym(Sym::Colon) && *self.peek_at(1) == Tok::Sym(Sym::LBrace) {
                self.advance();
                else_body = self.parse_block()?;
            }
            return Ok(Stmt::If { cond, then_body, else_body, span: sp });
        }
        if self.eat_kw(Kw::Loop) {
            let (var, _) = self.expect_ident("as loop variable")?;
            self.expect_sym(Sym::FatArrow, "between loop variable and iterable")?;
            let iter = self.parse_expr()?;
            let body = self.parse_block()?;
            return Ok(Stmt::Loop { var, iter, body, span: sp });
        }
        if self.eat_kw(Kw::Race) {
            self.expect_sym(Sym::LBracket, "after `race`")?;
            let mut exprs = Vec::new();
            while !matches!(self.peek(), Tok::Sym(Sym::RBracket)) {
                if *self.peek() == Tok::Eof {
                    break;
                }
                let e = self.parse_expr()?;
                exprs.push(e);
                if !self.eat_sym(Sym::Comma) {
                    break;
                }
            }
            self.expect_sym(Sym::RBracket, "to close `race` tasks")?;
            return Ok(Stmt::Race(exprs, sp));
        }
        // `<- expr` return value
        if *self.peek() == Tok::Sym(Sym::Arrow) {
            self.advance();
            let value = self.parse_expr()?;
            return Ok(Stmt::Return(value, sp));
        }
        // `x ::= expr` const decl
        if let Tok::Ident(name) = self.peek().clone() {
            if *self.peek_at(1) == Tok::Sym(Sym::Arrow) {
                // `x <- value` (reassignment-free: treat as assignment when declared already)
                self.advance();
                self.advance();
                let value = self.parse_expr()?;
                return Ok(Stmt::Var(VarDef { name, ty: None, value, span: sp }));
            }
            if *self.peek_at(1) == Tok::Sym(Sym::DColonEq) {
                self.advance();
                self.advance();
                let value = self.parse_expr()?;
                return Ok(Stmt::Const(VarDef { name, ty: None, value, span: sp }));
            }
        }
        if self.eat_kw(Kw::Expect) {
            let lhs = self.parse_postfix()?;
            let op = match self.peek() {
                Tok::Sym(Sym::EqEq) => Some(BinOp::Eq),
                Tok::Sym(Sym::NotEq) => Some(BinOp::Ne),
                Tok::Sym(Sym::Lt) => Some(BinOp::Lt),
                Tok::Sym(Sym::Le) => Some(BinOp::Le),
                Tok::Sym(Sym::Gt) => Some(BinOp::Gt),
                Tok::Sym(Sym::Ge) => Some(BinOp::Ge),
                _ => None,
            };
            if let Some(op) = op {
                self.advance();
                let rhs = self.parse_expr()?;
                return Ok(Stmt::Expect { lhs, op, rhs, span: sp });
            }
            return Ok(Stmt::Expect { lhs, op: BinOp::Eq, rhs: Expr::Bool(true, sp), span: sp });
        }
        let e = self.parse_expr()?;
        Ok(Stmt::ExprStmt(e))
    }

    /// `bring http`, `bring std.crypto` (ident form).
    fn parse_bring_ident(&mut self, name: String, sp: Span) -> Result<Option<Stmt>, Vec<Diag>> {
        // `std.<module>` — the namespaced form of a builtin module reference.
        if name == "std" && matches!(self.peek(), Tok::Sym(Sym::Dot)) {
            self.advance(); // `.`
            let (mod_name, _) = self.expect_ident("after `std.`")?;
            return match Module::from_name(&mod_name, sp) {
                Some(m) => {
                    self.imports.push(m.clone());
                    Ok(Some(Stmt::Bring(m, sp)))
                }
                None => Err(vec![Diag::new(
                    ErrorKind::Module,
                    format!("unknown module 'std.{mod_name}'"),
                    sp,
                    crate::suggest::did_you_mean(
                        &mod_name,
                        &Module::NAMES.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                        format!("Valid modules: {}.", Module::name_list()),
                    ),
                )
                .with_code(cat::UNKNOWN_MODULE)]),
            };
        }
        match Module::from_name(&name, sp) {
            Some(m) => {
                self.imports.push(m.clone());
                Ok(Some(Stmt::Bring(m, sp)))
            }
None => Err(vec![Diag::new(
                    ErrorKind::Module,
                    format!("unknown module '{name}'"),
                    sp,
                    crate::suggest::did_you_mean(
                        &name,
                        &Module::NAMES.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                        "Use `bring http`, `bring \"./utils\"`, or `bring std.crypto`.".to_string(),
                    ),
                )
                .with_code(cat::UNKNOWN_MODULE)]),
        }
    }

    /// `bring "./utils"`, `bring "http"`, `bring "std.crypto"` (string form).
    fn parse_bring_str(&mut self, path: String, sp: Span) -> Result<Option<Stmt>, Vec<Diag>> {
        // A local module path (starts with `./`, `../` or `/`, or contains a
        // directory separator) resolves to a `.hard` file on disk. Anything
        // else is an alternate spelling of a builtin module name.
        let is_path = path.starts_with("./")
            || path.starts_with("../")
            || path.starts_with('/')
            || path.contains('/');
        if is_path {
            self.import_paths.push(path.clone());
            return Ok(Some(Stmt::Import { path, span: sp }));
        }
        let name = path.strip_prefix("std.").unwrap_or(&path);
        match Module::from_name(name, sp) {
            Some(m) => {
                self.imports.push(m.clone());
                Ok(Some(Stmt::Bring(m, sp)))
            }
            None => Err(vec![Diag::new(
                ErrorKind::Module,
                format!("unknown module '{path}'"),
                sp,
                crate::suggest::did_you_mean(
                    name,
                    &Module::NAMES.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                    format!("Valid modules: {}, or a local import like `bring \"./utils\"`.", Module::name_list()),
                ),
            )
            .with_code(cat::UNKNOWN_MODULE)]),
        }
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>, Vec<Diag>> {
        self.enter_depth("block")?;
        let r = self.parse_block_inner();
        self.exit_depth();
        r
    }

    fn parse_block_inner(&mut self) -> Result<Vec<Stmt>, Vec<Diag>> {
        let sp = self.span();
        self.expect_sym(Sym::LBrace, "to start a block")?;
        let mut stmts = Vec::new();
        while !matches!(self.peek(), Tok::Sym(Sym::RBrace)) {
            if *self.peek() == Tok::Eof {
                return Err(vec![Diag::new(
                    ErrorKind::Parse,
                    "unexpected end of file inside block",
                    sp,
                    "Close the block with `}`.",
                )
                .with_code(cat::UNEXPECTED_EOI)]);
            }
            // Region-scoped recovery: a non-fatal statement error is confined
            // to its own statement; we report it, resync to the next statement
            // start and keep the rest of the block (and file) parseable.
            let before = self.pos;
            match self.parse_stmt() {
                Ok(Some(s)) => stmts.push(s),
                Ok(None) => {}
                Err(e) => {
                    if self.fatal {
                        return Err(e);
                    }
                    self.na_errs.extend(e);
                    // Guarantee forward progress even when the resync target
                    // is itself a failing statement start token.
                    if self.pos == before {
                        self.advance();
                    }
                    self.sync();
                }
            }
        }
        self.expect_sym(Sym::RBrace, "to close a block")?;
        Ok(stmts)
    }

    /// `protect jwt(secret = env.get("JWT_SECRET"), except = ["/health"])`
    ///
    /// The scheme names the guard, `secret` is the expression for the signing
    /// key (required), and `except` lists the path prefixes that stay public.
    /// Options are named so a later scheme can add more without changing the
    /// shape of the declaration.
    fn parse_protect(&mut self, sp: Span) -> Result<Stmt, Vec<Diag>> {
        let (scheme, ssp) = self.expect_ident("as a protect scheme, e.g. `jwt`")?;
        if !matches!(scheme.as_str(), "jwt") {
            return Err(vec![Diag::new(
                ErrorKind::Parse,
                format!("unknown protect scheme `{scheme}`"),
                ssp,
                "The only scheme today is `jwt`.",
            )
            .with_code(cat::INVALID_PROTECT)]);
        }
        self.expect_sym(Sym::LParen, "after the protect scheme")?;
        let mut secret: Option<Expr> = None;
        let mut except: Vec<String> = Vec::new();
        while !matches!(self.peek(), Tok::Sym(Sym::RParen)) {
            let (key, ksp) = self.expect_ident("as a protect option name")?;
            match key.as_str() {
                "secret" => {
                    self.expect_sym(Sym::Assign, "after `secret`")?;
                    if secret.is_some() {
                        return Err(vec![Diag::new(
                            ErrorKind::Parse,
                            "`secret` is set twice".to_string(),
                            ksp,
                            "A protect declaration takes one signing key.",
                        )
                        .with_code(cat::INVALID_PROTECT)]);
                    }
                    secret = Some(self.parse_expr()?);
                }
                "except" => {
                    self.expect_sym(Sym::Assign, "after `except`")?;
                    self.expect_sym(Sym::LBracket, "to start the `except` path list")?;
                    while !matches!(self.peek(), Tok::Sym(Sym::RBracket)) {
                        let psp = self.span();
                        let Tok::Str(path) = self.peek().clone() else {
                            return Err(vec![Diag::new(
                                ErrorKind::Parse,
                                "an exempt path must be a string".to_string(),
                                psp,
                                "Write `except = [\"/health\"]`.",
                            )
                            .with_code(cat::INVALID_PROTECT)]);
                        };
                        self.advance();
                        except.push(path);
                        if !self.eat_sym(Sym::Comma) {
                            break;
                        }
                    }
                    self.expect_sym(Sym::RBracket, "to close the `except` path list")?;
                }
                other => {
                    return Err(vec![Diag::new(
                        ErrorKind::Parse,
                        format!("unknown protect option `{other}`"),
                        ksp,
                        "Use `secret = ...` and `except = [...]`.",
                    )
                    .with_code(cat::INVALID_PROTECT)]);
                }
            }
            if !self.eat_sym(Sym::Comma) {
                break;
            }
            // A trailing comma after the last option is a typo, not a hint that
            // another option is coming.
            if matches!(self.peek(), Tok::Sym(Sym::RParen)) {
                return Err(vec![Diag::new(
                    ErrorKind::Parse,
                    "trailing comma in the protect declaration".to_string(),
                    sp,
                    "Drop the comma, or add the option it was meant to introduce.",
                )
                .with_code(cat::INVALID_PROTECT)]);
            }
        }
        self.expect_sym(Sym::RParen, "to close the protect declaration")?;
        let Some(secret) = secret else {
            return Err(vec![Diag::new(
                ErrorKind::Parse,
                "protect needs a signing key".to_string(),
                sp,
                "Write `protect jwt(secret = env.get(\"JWT_SECRET\"))`.",
            )
            .with_code(cat::INVALID_PROTECT)]);
        };
        Ok(Stmt::Protect(ProtectDef { scheme, secret, except, span: sp }))
    }

    /// `model Name = table [ field => Type #attr, ... ]` — the v0.1 blueprint
    /// form — and the v0.6 framework form
    /// `model Name { field : Type(min=0) @unique, ... }`.
    ///
    /// Both forms produce the same [`ModelDef`]; the framework form simply
    /// carries type arguments (constraints) and richer attributes that the
    /// validation engine, ORM and OpenAPI generator consume.
    fn parse_model(&mut self) -> Result<Stmt, Vec<Diag>> {
        let sp = self.span();
        self.advance();
        let (name, nsp) = self.expect_ident("as a model name")?;
        // `model Name = table { ... }` pins the table name; `model Name { ... }`
        // defaults the table to the lowercased model name.
        let mut table: Option<String> = None;
        // Model-level attributes: `model User @strict { ... }`
        let mut strict = false;
        self.parse_model_attrs(nsp, &mut strict)?;
        let braced = if self.eat_sym(Sym::Assign) {
            let (t, _) = self.expect_ident("as a table name")?;
            table = Some(t);
            // `@strict` reads naturally on either side of the table name.
            self.parse_model_attrs(nsp, &mut strict)?;
            *self.peek() == Tok::Sym(Sym::LBrace)
        } else {
            *self.peek() == Tok::Sym(Sym::LBrace)
        };
        if braced {
            let fields = self.parse_model_fields(Sym::LBrace, Sym::RBrace, "a model body")?;
            let table = table.unwrap_or_else(|| name.to_lowercase());
            return Ok(Stmt::Model(self.finish_model(name, table, fields, true, strict, nsp)));
        }
        let table = table.unwrap_or_else(|| name.to_lowercase());
        self.expect_sym(Sym::LBracket, "to start a model")?;
        let mut fields = Vec::new();
        loop {
            match self.peek() {
                Tok::Sym(Sym::RBracket) => break,
                Tok::Eof => {
                    return Err(vec![Diag::new(
                        ErrorKind::Parse,
                        "unexpected end of file inside model",
                        sp,
                        "Close the model with `]`.",
                    )
                    .with_code(cat::UNEXPECTED_EOI)])
                }
                _ => {}
            }
            let (fname, fsp) = self.expect_ident("as a field name")?;
            self.expect_sym(Sym::FatArrow, "after field name in model")?;
            let (ty, _) = self.expect_ident("as a field type")?;
            let mut attrs = self.parse_presence_markers();
            while self.eat_sym(Sym::Hash) {
                let (a, _) = self.expect_ident("after `#` in model attributes")?;
                attrs.push(FieldAttr::plain(a));
            }
            fields.push(FieldDef { name: fname, ty, args: Vec::new(), attrs, span: fsp });
            if !self.eat_sym(Sym::Comma) {
                break;
            }
        }
        self.expect_sym(Sym::RBracket, "to close a model")?;
        Ok(Stmt::Model(self.finish_model(name, table, fields, false, false, nsp)))
    }

    /// Model-level attributes: `@strict` rejects unknown keys at validation
    /// time, `@open` is accepted and ignored (a later milestone consumes it).
    fn parse_model_attrs(&mut self, nsp: Span, strict: &mut bool) -> Result<(), Vec<Diag>> {
        while self.eat_sym(Sym::At) {
            let (a, _) = self.expect_ident("after `@` in model attributes")?;
            match a.as_str() {
                "strict" => *strict = true,
                "open" => {}
                other => {
                    return Err(vec![Diag::new(
                        ErrorKind::Parse,
                        format!("unknown model attribute `{other}`"),
                        nsp,
                        "Use `@strict` to reject unknown fields.",
                    )
                    .with_code(cat::UNEXPECTED_TOKEN)])
                }
            }
        }
        Ok(())
    }

    fn finish_model(
        &mut self,
        name: String,
        table: String,
        fields: Vec<FieldDef>,
        brace: bool,
        strict: bool,
        span: Span,
    ) -> ModelDef {
        let md = ModelDef { name, table, fields, brace, strict, span };
        self.models.push(md.clone());
        md
    }

    /// `field : Type(a, b=c) @attr(arg) ,` repeated until `close`.
    /// Optional presence markers on a model field type: `Str!` demands the key
    /// and `Str?` states that it may be absent. Both are sugar for the
    /// `required` / `nullable` constraints, and neither is needed to express
    /// the default (validate when present).
    fn parse_presence_markers(&mut self) -> Vec<FieldAttr> {
        let mut attrs = Vec::new();
        while self.eat_sym(Sym::Bang) {
            attrs.push(FieldAttr::plain("required"));
        }
        if self.eat_sym(Sym::Q) {
            attrs.push(FieldAttr::plain("nullable"));
        }
        attrs
    }

    fn parse_model_fields(
        &mut self,
        open: Sym,
        close: Sym,
        what: &str,
    ) -> Result<Vec<FieldDef>, Vec<Diag>> {
        let sp = self.span();
        self.expect_sym(open, &format!("to start {what}"))?;
        let mut fields = Vec::new();
        while !matches!(self.peek(), Tok::Sym(c) if *c == close) {
            if matches!(self.peek(), Tok::Eof) {
                return Err(vec![Diag::new(
                    ErrorKind::Parse,
                    format!("unexpected end of file inside {what}"),
                    sp,
                    format!("Close the block with `{}`.", close.as_str()),
                )
                .with_code(cat::UNEXPECTED_EOI)]);
            }
            // A field starts with `name :`. Newlines are not statement
            // terminators in HardScript, so the closing delimiter alone cannot
            // end the last field: stop as soon as the next tokens cannot begin
            // one. That is what lets fields be written one per line.
            if !matches!(self.peek(), Tok::Ident(_))
                || !matches!(self.peek_at(1), Tok::Sym(Sym::Colon))
            {
                break;
            }
            let (fname, fsp) = self.expect_ident("as a field name")?;
            self.expect_sym(Sym::Colon, "after field name")?;
            let (ty, _) = self.expect_ident("as a field type")?;
            // `Int!(min=18)` and `Int(min=18)!` are both written in the wild,
            // so markers and constraint args may interleave.
            let mut args = self.parse_type_args()?;
            let mut attrs = self.parse_presence_markers();
            let more = self.parse_type_args()?;
            if !more.is_empty() {
                args.extend(more);
                attrs.extend(self.parse_presence_markers());
            }
            while self.eat_sym(Sym::At) {
                let (a, _) = self.expect_ident("after `@` in field attributes")?;
                let arg = if matches!(self.peek(), Tok::Sym(Sym::LParen)) {
                    self.advance();
                    let e = self.parse_expr()?;
                    self.expect_sym(Sym::RParen, "to close an attribute argument")?;
                    Some(e)
                } else {
                    None
                };
                attrs.push(FieldAttr { name: a, arg });
            }
            fields.push(FieldDef { name: fname, ty, args, attrs, span: fsp });
            self.eat_sym(Sym::Comma);
        }
        self.expect_sym(close, &format!("to close {what}"))?;
        Ok(fields)
    }

    /// `(Int, "a", "b")` and `(min=18, length=3..30, max=120)` type arguments.
    fn parse_type_args(&mut self) -> Result<Vec<FieldArg>, Vec<Diag>> {
        if !matches!(self.peek(), Tok::Sym(Sym::LParen)) {
            return Ok(Vec::new());
        }
        self.advance();
        let mut args = Vec::new();
        while !matches!(self.peek(), Tok::Sym(Sym::RParen)) {
            if matches!(self.peek(), Tok::Eof) {
                break;
            }
            // `name = expr` (also `name == expr` for symmetry with runtime rules)
            if let Tok::Ident(k) = self.peek().clone() {
                if matches!(self.peek_at(1), Tok::Sym(Sym::Assign) | Tok::Sym(Sym::EqEq)) {
                    self.advance();
                    self.advance();
                    let v = self.parse_expr()?;
                    args.push(FieldArg::Constraint(k, Box::new(v)));
                    if !self.eat_sym(Sym::Comma) {
                        break;
                    }
                    continue;
                }
            }
            let v = self.parse_expr()?;
            args.push(FieldArg::Positional(v));
            if !self.eat_sym(Sym::Comma) {
                break;
            }
        }
        self.expect_sym(Sym::RParen, "to close field constraints")?;
        Ok(args)
    }

    fn parse_route(&mut self) -> Result<Stmt, Vec<Diag>> {
        let sp = self.span();
        let method = match *self.peek() {
            Tok::Kw(Kw::Get) => "GET",
            Tok::Kw(Kw::Post) => "POST",
            Tok::Kw(Kw::Put) => "PUT",
            Tok::Kw(Kw::Delete) => "DELETE",
            Tok::Kw(Kw::Patch) => "PATCH",
            _ => unreachable!(),
        }
        .to_string();
        self.advance();
        let path = match self.advance().tok {
            Tok::Str(s) => s,
            _ => {
                return Err(vec![Diag::new(
                    ErrorKind::Parse,
                    format!("expected a path string after {method}"),
                    self.span(),
                    format!("Use {method} \"/users\" :: {{ ... }}."),
                )
                .with_code(cat::EXPECTED_TOKEN)])
            }
        };
        let params = if self.eat_sym(Sym::DColon) {
            if *self.peek() == Tok::Sym(Sym::LParen) {
                self.parse_route_params()?
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        let body = self.parse_block()?;
        let rd = RouteDef { method, path, params, body: body.clone(), span: sp };
        self.routes.push(rd.clone());
        Ok(Stmt::Route(rd))
    }

    fn parse_route_params(&mut self) -> Result<Vec<FunParam>, Vec<Diag>> {
        let mut out = Vec::new();
        self.expect_sym(Sym::LParen, "to start route params")?;
        while !matches!(self.peek(), Tok::Sym(Sym::RParen)) {
            if *self.peek() == Tok::Eof {
                break;
            }
            let sp = self.span();
            let (name, _) = self.expect_ident("as a route parameter")?;
            let mut is_body = false;
            let mut ty = None;
            if self.eat_sym(Sym::Assign) {
                if self.eat_kw(Kw::Calc) {
                    // "body = Calc" placeholder form not used; skip
                }
                let (t, _) = self.expect_ident("as a type")?;
                if t == "body" {
                    // legacy form
                    is_body = true;
                } else {
                    ty = Some(t);
                }
            }
            // `(body = User)` => type User, but named `body`
            if name == "body" && ty.is_some() {
                // represent as body param with model type
                out.push(FunParam { ty: ty.clone(), name: "body".into(), is_body: true, span: sp });
            } else {
                out.push(FunParam { ty, name, is_body, span: sp });
            }
            if !self.eat_sym(Sym::Comma) {
                break;
            }
        }
        self.expect_sym(Sym::RParen, "to close route params")?;
        Ok(out)
    }

    fn parse_socket(&mut self) -> Result<Stmt, Vec<Diag>> {
        let sp = self.span();
        self.advance();
        let path = match self.advance().tok {
            Tok::Str(s) => s,
            _ => {
                return Err(vec![Diag::new(
                    ErrorKind::Parse,
                    "expected a path string after `socket`",
                    self.span(),
                    "Use `socket \"/chat\" { ... }`.",
                )
                .with_code(cat::EXPECTED_TOKEN)])
            }
        };
        self.expect_sym(Sym::LBrace, "to start a socket block")?;
        let mut connect = None;
        let mut message = None;
        let mut disconnect = None;
        while let Tok::Kw(k) = self.peek().clone() {
            match k {
                Kw::Connect | Kw::Message | Kw::Disconnect => {
                    self.advance();
                    let name = match k {
                        Kw::Connect => "connect",
                        Kw::Message => "message",
                        Kw::Disconnect => "disconnect",
                        _ => unreachable!(),
                    };
                    let mut args = Vec::new();
                    if *self.peek() == Tok::Sym(Sym::LParen) {
                        self.advance();
                        while !matches!(self.peek(), Tok::Sym(Sym::RParen)) {
                            let (n, _) = self.expect_ident("as an event argument")?;
                            args.push(n);
                            if !self.eat_sym(Sym::Comma) {
                                break;
                            }
                        }
                        self.expect_sym(Sym::RParen, "")?;
                    }
                    self.expect_sym(Sym::DColon, "before event body")?;
                    let body = self.parse_block()?;
                    match name {
                        "connect" => connect = Some(body),
                        "message" => message = Some(body),
                        _ => disconnect = Some(body),
                    }
                    let _ = args;
                }
                Kw::Socket => {
                    return Err(vec![Diag::new(
                        ErrorKind::Parse,
                        "nested socket blocks are not allowed",
                        self.span(),
                        "Define each socket at the top level.",
                    )
                    .with_code(cat::UNEXPECTED_TOKEN)])
                }
                _ => {
                    return Err(vec![Diag::new(
                        ErrorKind::Parse,
                        "unexpected keyword inside socket block",
                        self.span(),
                        "Only connect / message / disconnect blocks are allowed here.",
                    )
                    .with_code(cat::UNEXPECTED_TOKEN)])
                }
            }
        }
        self.expect_sym(Sym::RBrace, "to close a socket block")?;
        let sd = SocketDef { path, connect, message, disconnect, span: sp };
        self.sockets.push(sd.clone());
        Ok(Stmt::Socket(sd))
    }

    fn parse_func(&mut self, _async: bool) -> Result<Stmt, Vec<Diag>> {
        let (name, sp) = self.expect_ident("as a function name")?;
        let params = self.parse_func_params()?;
        let ret = if self.eat_sym(Sym::FatArrow) {
            let (t, _) = self.expect_ident("as a return type")?;
            Some(t)
        } else {
            None
        };
        let body = self.parse_block()?;
        let fd = FunDef {
            name: name.clone(),
            ret,
            params: params.clone(),
            body: body.clone(),
            span: sp,
        };
        self.funcs.push(fd.clone());
        Ok(Stmt::Func(fd))
    }

    fn parse_func_params(&mut self) -> Result<Vec<FunParam>, Vec<Diag>> {
        let mut out = Vec::new();
        self.expect_sym(Sym::LParen, "to start parameters")?;
        while !matches!(self.peek(), Tok::Sym(Sym::RParen)) {
            if *self.peek() == Tok::Eof {
                break;
            }
            let sp = self.span();
            let ty = match self.peek().clone() {
                Tok::Ident(t) => {
                    let is_ty = !matches!(self.peek_at(1), Tok::Sym(Sym::Assign));
                    if is_ty {
                        let t = t.clone();
                        self.advance();
                        Some(t)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            let (name, _) = self.expect_ident("as a parameter name")?;
            let mut default = None;
            if self.eat_sym(Sym::Assign) {
                default = Some(self.parse_expr()?);
            }
            let p = FunParam { ty, name, is_body: false, span: sp };
            if let Some(d) = default {
                // store default as a synthetic const in body? keep simple: we record it via param.
                let _ = d;
            }
            out.push(p);
            if !self.eat_sym(Sym::Comma) {
                break;
            }
        }
        self.expect_sym(Sym::RParen, "to close parameters")?;
        Ok(out)
    }

    // ---------- expressions ----------

    fn parse_expr(&mut self) -> Result<Expr, Vec<Diag>> {
        self.enter_depth("expression")?;
        let r = self.parse_range();
        self.exit_depth();
        r
    }

    /// `lo ... hi` / `lo .. hi`. Binds looser than every operator, so
    /// `1...n - 1` means `1...(n - 1)`.
    fn parse_range(&mut self) -> Result<Expr, Vec<Diag>> {
        let sp = self.span();
        let lo = self.parse_or()?;
        if *self.peek() == Tok::Sym(Sym::Ellipsis) {
            self.advance();
            let hi = self.parse_or()?;
            return Ok(Expr::Range(Box::new(lo), Box::new(hi), sp));
        }
        Ok(lo)
    }

    fn parse_or(&mut self) -> Result<Expr, Vec<Diag>> {
        let mut lhs = self.parse_and()?;
        let mut k = 0usize;
        while *self.peek() == Tok::Sym(Sym::OrOr) {
            k += 1;
            self.chain_ok(k)?;
            let sp = self.span();
            self.advance();
            let rhs = self.parse_and()?;
            lhs = Expr::Binary(BinOp::Or, Box::new(lhs), Box::new(rhs), sp);
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<Expr, Vec<Diag>> {
        let mut lhs = self.parse_cmp()?;
        let mut k = 0usize;
        while *self.peek() == Tok::Sym(Sym::AndAnd) {
            k += 1;
            self.chain_ok(k)?;
            let sp = self.span();
            self.advance();
            let rhs = self.parse_cmp()?;
            lhs = Expr::Binary(BinOp::And, Box::new(lhs), Box::new(rhs), sp);
        }
        Ok(lhs)
    }

    fn parse_cmp(&mut self) -> Result<Expr, Vec<Diag>> {
        let mut lhs = self.parse_add()?;
        let mut k = 0usize;
        loop {
            let op = match self.peek() {
                Tok::Sym(Sym::EqEq) => BinOp::Eq,
                Tok::Sym(Sym::NotEq) => BinOp::Ne,
                Tok::Sym(Sym::Lt) => BinOp::Lt,
                Tok::Sym(Sym::Le) => BinOp::Le,
                Tok::Sym(Sym::Gt) => BinOp::Gt,
                Tok::Sym(Sym::Ge) => BinOp::Ge,
                _ => break,
            };
            k += 1;
            self.chain_ok(k)?;
            let sp = self.span();
            self.advance();
            let rhs = self.parse_add()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs), sp);
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> Result<Expr, Vec<Diag>> {
        let mut lhs = self.parse_mul()?;
        let mut k = 0usize;
        loop {
            let op = match self.peek() {
                Tok::Sym(Sym::Plus) => BinOp::Add,
                Tok::Sym(Sym::Minus) => BinOp::Sub,
                _ => break,
            };
            k += 1;
            self.chain_ok(k)?;
            let sp = self.span();
            self.advance();
            let rhs = self.parse_mul()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs), sp);
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<Expr, Vec<Diag>> {
        let mut lhs = self.parse_unary()?;
        let mut k = 0usize;
        loop {
            let op = match self.peek() {
                Tok::Sym(Sym::Star) => BinOp::Mul,
                Tok::Sym(Sym::Slash) => BinOp::Div,
                Tok::Sym(Sym::Percent) => BinOp::Mod,
                _ => break,
            };
            k += 1;
            self.chain_ok(k)?;
            let sp = self.span();
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs), sp);
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<Expr, Vec<Diag>> {
        self.enter_depth("expression")?;
        let r = self.parse_unary_inner();
        self.exit_depth();
        r
    }

    fn parse_unary_inner(&mut self) -> Result<Expr, Vec<Diag>> {
        if *self.peek() == Tok::Sym(Sym::Minus) {
            let sp = self.span();
            self.advance();
            let e = self.parse_unary()?;
            return Ok(Expr::Unary(UnOp::Neg, Box::new(e), sp));
        }
        if *self.peek() == Tok::Sym(Sym::Bang) {
            let sp = self.span();
            self.advance();
            let e = self.parse_unary()?;
            return Ok(Expr::Unary(UnOp::Not, Box::new(e), sp));
        }
        if *self.peek() == Tok::Kw(Kw::Wait) {
            let _sp = self.span();
            self.advance();
            // wait expr — evaluates to expr result (synchronous semantics)
            let e = self.parse_unary()?;
            return Ok(e);
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<Expr, Vec<Diag>> {
        // `queue Name(args, opt = v)` in expression position desugars to an
        // ordinary call, exactly like the statement form. Checked before the
        // atom so `queue` itself is consumed by the dedicated parser; anything
        // else starting with `queue` parses as before.
        if matches!(self.peek(), Tok::Ident(n) if n == "queue")
            && matches!(self.peek_at(1), Tok::Ident(_))
            && matches!(self.peek_at(2), Tok::Sym(Sym::LParen))
        {
            return self.parse_enqueue_call();
        }
        let mut e = self.parse_atom()?;
        let mut k = 0usize;
        loop {
            match *self.peek() {
                Tok::Sym(Sym::Dot) => {
                    k += 1;
                    self.chain_ok(k)?;
                    self.advance();
                    let (name, sp) = self.member_name()?;
                    e = Expr::Member(Box::new(e), name, sp);
                }
                Tok::Sym(Sym::LParen) => {
                    // `db.transaction` takes a block, never parentheses: catch
                    // it here so the mistake points at the paren, not at an
                    // undefined `db` later.
                    if let Expr::Member(base, name, _) = &e {
                        if name == "transaction" {
                            if let Expr::Ident(root, _) = base.as_ref() {
                                if root == "db" {
                                    return Err(vec![Diag::new(
                                        ErrorKind::Parse,
                                        "db.transaction takes a block, not parentheses",
                                        self.span(),
                                        "Write `db.transaction { ... }`.",
                                    )
                                    .with_code(cat::EXPECTED_TOKEN)]);
                                }
                            }
                        }
                    }
                    k += 1;
                    self.chain_ok(k)?;
                    let sp = self.span();
                    self.advance();
                    let mut args = Vec::new();
                    while !matches!(self.peek(), Tok::Sym(Sym::RParen)) {
                        if *self.peek() == Tok::Eof {
                            break;
                        }
                        args.push(self.parse_expr()?);
                        if !self.eat_sym(Sym::Comma) {
                            break;
                        }
                    }
                    self.expect_sym(Sym::RParen, "to close arguments")?;
                    e = Expr::Call { callee: Box::new(e), args, span: sp };
                }
                Tok::Sym(Sym::LBracket) => {
                    k += 1;
                    self.chain_ok(k)?;
                    let sp = self.span();
                    self.advance();
                    let idx = self.parse_expr()?;
                    self.expect_sym(Sym::RBracket, "to close index")?;
                    e = Expr::Index(Box::new(e), Box::new(idx), sp);
                }
                Tok::Sym(Sym::LBrace) => {
                    // `db.transaction { ... }` is the only call that takes a
                    // block. A brace after anything else is not a call, so it
                    // breaks out exactly as before.
                    let is_tx = match &e {
                        Expr::Member(base, name, _) if name == "transaction" => {
                            matches!(base.as_ref(), Expr::Ident(root, _) if root == "db")
                        }
                        _ => false,
                    };
                    if !is_tx {
                        break;
                    }
                    let sp = e.span();
                    let body = self.parse_block()?;
                    e = Expr::Transaction { body, span: sp };
                    // A transaction is complete: it takes no further chaining.
                    break;
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_atom(&mut self) -> Result<Expr, Vec<Diag>> {
        let sp = self.span();
        match self.peek().clone() {
            Tok::Int(n) => {
                self.advance();
                Ok(Expr::Int(n, sp))
            }
            Tok::Float(f) => {
                self.advance();
                Ok(Expr::Float(f, sp))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(Expr::Str(s, sp))
            }
            Tok::Ident(s) => {
                if s == "true" || s == "false" {
                    self.advance();
                    Ok(Expr::Bool(s == "true", sp))
                } else {
                    self.advance();
                    Ok(Expr::Ident(s, sp))
                }
            }
            Tok::Sym(Sym::LBracket) => {
                self.advance();
                let mut items = Vec::new();
                while !matches!(self.peek(), Tok::Sym(Sym::RBracket)) {
                    if *self.peek() == Tok::Eof {
                        break;
                    }
                    items.push(self.parse_expr()?);
                    if !self.eat_sym(Sym::Comma) {
                        break;
                    }
                }
                self.expect_sym(Sym::RBracket, "to close list")?;
                Ok(Expr::List(items, sp))
            }
            Tok::Sym(Sym::LBrace) => {
                self.advance();
                let mut kv = Vec::new();
                while !matches!(self.peek(), Tok::Sym(Sym::RBrace)) {
                    if *self.peek() == Tok::Eof {
                        break;
                    }
                    let key = match self.peek().clone() {
                        Tok::Ident(s) => {
                            self.advance();
                            s
                        }
                        Tok::Str(s) => {
                            self.advance();
                            s
                        }
                        _ => {
                            return Err(vec![Diag::new(
                                ErrorKind::Parse,
                                "expected a key in object literal",
                                self.span(),
                                "Write `{ name : \"value\" }`.",
                            )
                            .with_code(cat::EXPECTED_TOKEN)])
                        }
                    };
                    self.expect_sym(Sym::Colon, "after object key")?;
                    let val = self.parse_expr()?;
                    kv.push((key, val));
                    if !self.eat_sym(Sym::Comma) {
                        break;
                    }
                }
                self.expect_sym(Sym::RBrace, "to close object")?;
                Ok(Expr::Obj(kv, sp))
            }
            Tok::Sym(Sym::LParen) => {
                // could be pick/match handled elsewhere; here just group or http-call body (handled in postfix)
                self.advance();
                let e = self.parse_expr()?;
                self.expect_sym(Sym::RParen, "to close parentheses")?;
                Ok(e)
            }
            Tok::Kw(Kw::Pick) => {
                self.advance();
                let scrut = self.parse_expr()?;
                self.expect_sym(Sym::LBrace, "to start `pick` arms")?;
                let mut arms = Vec::new();
                while !matches!(self.peek(), Tok::Sym(Sym::RBrace)) {
                    if *self.peek() == Tok::Eof {
                        break;
                    }
                    let val = match *self.peek() {
                        Tok::Sym(Sym::Star) => {
                            self.advance();
                            None
                        }
                        _ => Some(self.parse_expr()?),
                    };
                    self.expect_sym(Sym::FatArrow, "after `pick` arm value")?;
                    let body = self.parse_expr()?;
                    arms.push(MatchArm { value: val, body });
                    if !self.eat_sym(Sym::Comma) {
                        break;
                    }
                }
                self.expect_sym(Sym::RBrace, "to close `pick` arms")?;
                Ok(Expr::Match(Box::new(scrut), arms, sp))
            }
            Tok::Kw(Kw::Get) | Tok::Kw(Kw::Post) | Tok::Kw(Kw::Put) | Tok::Kw(Kw::Delete)
            | Tok::Kw(Kw::Patch) => {
                let verb = match *self.peek() {
                    Tok::Kw(Kw::Get) => "GET",
                    Tok::Kw(Kw::Post) => "POST",
                    Tok::Kw(Kw::Put) => "PUT",
                    Tok::Kw(Kw::Delete) => "DELETE",
                    Tok::Kw(Kw::Patch) => "PATCH",
                    _ => unreachable!(),
                }
                .to_string();
                self.advance();
                let path = match self.advance().tok {
                    Tok::Str(s) => s,
                    _ => {
                        return Err(vec![Diag::new(
                            ErrorKind::Parse,
                            "expected HTTP path string",
                            self.span(),
                            "",
                        )
                        .with_code(cat::EXPECTED_TOKEN)])
                    }
                };
                let body = if *self.peek() == Tok::Sym(Sym::LBrace) {
                    self.advance();
                    if *self.peek() == Tok::Sym(Sym::RBrace) {
                        self.advance();
                        None
                    } else {
                        let e = self.parse_expr()?;
                        self.expect_sym(Sym::RBrace, "to close HTTP body")?;
                        Some(Box::new(e))
                    }
                } else {
                    None
                };
                Ok(Expr::HttpCall { verb, path, body, span: sp })
            }
            _ => Err(vec![Diag::new(
                ErrorKind::Parse,
                "expected a value",
                sp,
                "Write a literal, variable, or expression.",
            )
            .with_code(cat::EXPECTED_TOKEN)]),
        }
    }
}

/// The imports found by [`scan_imports`].
#[derive(Debug, Default, Clone)]
pub struct ScanImports {
    /// Builtin runtime modules (`http`, `crypto`, …).
    pub builtins: Vec<Module>,
    /// Local module paths as written (`./utils`), before `.hard` resolution.
    pub paths: Vec<String>,
}

/// A lightweight, body-agnostic scanner that extracts `bring` statements from
/// a source file without running the full parser.
///
/// Used by the module-graph builder: resolving the dependency graph only needs
/// the imports, so this stays tolerant (it never fails on a malformed file —
/// the real parse error is raised later, when the module is actually compiled)
/// and avoids walking function bodies. Deterministic: reports imports in
/// source order.
pub fn scan_imports(src: &str) -> ScanImports {
    let Ok(toks) = Lexer::new(src).tokenize() else {
        return ScanImports::default();
    };
    let toks: Vec<Token> = toks;
    let mut out = ScanImports::default();
    let mut i = 0;
    while i < toks.len() {
        if toks[i].tok == Tok::Kw(Kw::Bring) {
            i += 1;
            if i >= toks.len() {
                break;
            }
            match &toks[i].tok {
                Tok::Str(s) => {
                    let path = s.clone();
                    i += 1;
                    let is_path = path.starts_with("./")
                        || path.starts_with("../")
                        || path.starts_with('/')
                        || path.contains('/');
                    if is_path {
                        out.paths.push(path);
                    } else {
                        let name = path.strip_prefix("std.").unwrap_or(&path).to_string();
                        if let Some(m) = Module::from_name(&name, Span::new(0, 0)) {
                            out.builtins.push(m);
                        }
                    }
                    continue;
                }
                Tok::Ident(name) => {
                    i += 1;
                    if name == "std"
                        && i < toks.len()
                        && toks[i].tok == Tok::Sym(Sym::Dot)
                        && i + 1 < toks.len()
                        && matches!(toks[i + 1].tok, Tok::Ident(_))
                    {
                        i += 1; // `.`
                        if let Tok::Ident(mname) = &toks[i].tok {
                            let mname = mname.clone();
                            i += 1;
                            if let Some(m) = Module::from_name(&mname, Span::new(0, 0)) {
                                out.builtins.push(m);
                            }
                        }
                        continue;
                    }
                    if let Some(m) = Module::from_name(name, Span::new(0, 0)) {
                        out.builtins.push(m);
                    }
                    continue;
                }
                _ => {
                    i += 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_errs(src: &str) -> Vec<Diag> {
        match crate::frontend(src, "t.hard") {
            Ok(_) => Vec::new(),
            Err(d) => d,
        }
    }

    fn parse_one(src: &str) -> Stmt {
        let toks: Vec<Token> = crate::Lexer::new(src).tokenize().unwrap();
        crate::Parser::new(toks).parse_program().unwrap().stmts.remove(0)
    }

    #[test]
    fn protect_parses_scheme_secret_and_exempts() {
        let st = parse_one("protect jwt(secret = env.get(\"S\"), except = [\"/health\", \"/metrics\"])\n");
        let d = match st {
            Stmt::Protect(d) => d,
            other => panic!("expected Protect, got {other:?}"),
        };
        assert_eq!(d.scheme, "jwt");
        assert_eq!(d.except, vec!["/health".to_string(), "/metrics".to_string()]);
        assert_eq!(d.secret.render(), "env.get(\"S\")");
    }

    #[test]
    fn protect_except_is_optional() {
        let d = match parse_one("protect jwt(secret = k)\n") {
            Stmt::Protect(d) => d,
            other => panic!("expected Protect, got {other:?}"),
        };
        assert!(d.except.is_empty(), "no exempt list is allowed");
    }

    #[test]
    fn protect_requires_a_secret() {
        // A guard with no secret would either fail every request or, worse,
        // fall back to an empty key; it has to be a parse error.
        let errs = parse_errs("protect jwt()\n");
        assert!(!errs.is_empty(), "empty protect must not parse");
        let errs = parse_errs("protect jwt(except = [\"/x\"])\n");
        assert!(!errs.is_empty(), "protect without secret must not parse");
    }

    #[test]
    fn protect_rejects_a_trailing_comma() {
        // `secret = x,` is a typo, not a one-element list; the parser must say
        // so rather than quietly accepting it.
        let errs = parse_errs("protect jwt(secret = k,)\n");
        assert!(!errs.is_empty(), "trailing comma must not parse");
    }

    #[test]
    fn protect_is_a_keyword_not_an_identifier() {
        // `protect` is a reserved word now, so a variable cannot shadow it.
        let errs = parse_errs("protect <- 1\n");
        assert!(!errs.is_empty(), "protect is reserved");
    }

    #[test]
    fn junk_between_statements_reports_all_regions() {
        // Two junk statements separated by a valid route: region-scoped
        // recovery must resync at the `GET` and report a diagnostic for
        // each junk line instead of abandoning the file after the first.
        let src = "app @3000\n@@@\nGET \"/a\" :: { <- 1 }\n@@@\n";
        let errs = parse_errs(src);
        assert_eq!(errs.len(), 2, "expected two region errors, got {errs:?}");
        assert!(errs[0].message.contains("expected"), "got {}", errs[0].message);
        assert!(errs[1].message.contains("expected"), "got {}", errs[1].message);
    }

    #[test]
    fn bad_statement_inside_block_keeps_region_alive() {
        // An invalid statement inside a route body must not swallow the
        // following statements in the same block: the three junk `?` tokens
        // each become their own isolated error, `<- 2` keeps parsing, and the
        // trailing model region stays intact (no `unexpected end of file`).
        let src = "model User = users [\n  id => Int,\n]\n\
                   GET \"/a\" :: {\n  <- 1\n  ?\n  ?\n  ?\n  <- 2\n}\n\
                   model Account = accounts [\n  id => Int,\n]\n";
        let errs = parse_errs(src);
        assert_eq!(errs.len(), 3, "expected three region errors, got {errs:?}");
        assert!(
            !errs.iter().any(|d| d.message.contains("end of file")),
            "region was swallowed: {errs:?}"
        );
    }

    #[test]
    fn brace_model_fields_need_no_commas() {
        // Newlines are not statement terminators, so the closing `}` alone
        // cannot end the last field: the field loop has to stop on the first
        // token pair that cannot begin a field.
        let src = "model R {\n    email : Email\n    age : Int(min=18)\n}\n";
        let prog = crate::frontend(src, "t.hard").expect("brace model must parse");
        let m = prog
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::Model(m) => Some(m),
                _ => None,
            })
            .expect("model statement");
        assert!(m.brace, "brace form");
        assert_eq!(m.table, "r", "table defaults to the lowercased name");
        assert_eq!(m.fields.len(), 2, "got {:?}", m.fields);
        assert_eq!(m.fields[0].name, "email");
        assert_eq!(m.fields[0].ty, "Email");
        assert_eq!(m.fields[1].name, "age");
        assert_eq!(m.fields[1].ty, "Int");
        assert_eq!(m.fields[1].args.len(), 1, "constraint arg captured");
    }

    #[test]
    fn brace_model_accepts_commas_and_a_trailing_field() {
        let src = "model R {\n    a : Int,\n    b : Str,\n}\n";
        let prog = crate::frontend(src, "t.hard").expect("brace model with commas");
        let m = prog
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::Model(m) => Some(m),
                _ => None,
            })
            .expect("model statement");
        assert_eq!(m.fields.len(), 2, "got {:?}", m.fields);
    }

    #[test]
    fn model_table_override_and_strict_attribute() {
        let src = "model R = people @strict {\n    a : Int\n}\n";
        let prog = crate::frontend(src, "t.hard").expect("table override parses");
        let m = prog
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::Model(m) => Some(m),
                _ => None,
            })
            .expect("model statement");
        assert_eq!(m.table, "people", "explicit table name");
        assert!(m.strict, "@strict marks unknown keys as errors");
    }

    #[test]
    fn presence_markers_become_required_and_nullable() {
        // `Str!` demands the key, `Str?` says it may be absent, and the
        // constraint args may sit on either side of the marker.
        let src = "model R {\n    a : Str!\n    b : Int?(min=1)\n    c : Str(max=9)!\n    d : Str\n}\n";
        let prog = crate::frontend(src, "t.hard").expect("presence markers parse");
        let m = prog
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::Model(m) => Some(m),
                _ => None,
            })
            .expect("model statement");
        let names: Vec<&str> = m.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "d"], "got {names:?}");
        let has = |f: &FieldDef, want: &str| f.attrs.iter().any(|a| a.name == want);
        assert!(has(&m.fields[0], "required"), "a is required");
        assert!(has(&m.fields[1], "nullable"), "b is nullable");
        assert_eq!(m.fields[1].args.len(), 1, "b keeps its constraint");
        assert!(has(&m.fields[2], "required"), "c is required");
        assert_eq!(m.fields[2].args.len(), 1, "c keeps its constraint");
        assert!(m.fields[3].attrs.is_empty(), "d is unmarked");
    }

    #[test]
    fn range_bounds_accept_two_or_three_dots() {
        // `..` used to lex as two `.` tokens, so `Int(range=0..100)` failed
        // to parse; both spellings must produce the same inclusive range.
        for src in [
            "app @3000\nGET \"/a\" :: {\n    loop i => [0..3] {\n        <- i\n    }\n}\n",
            "app @3000\nGET \"/a\" :: {\n    loop i => [0...3] {\n        <- i\n    }\n}\n",
        ] {
            assert!(parse_errs(src).is_empty(), "range must parse: {src}");
        }
    }

    #[test]
    fn legacy_bracket_model_still_parses() {
        // v0.5 shipped `model X = table [ f => T #attr ]`; the brace form is
        // additive, so the old spelling has to keep working.
        let src = "model User = users [\n    id => Int #id,\n    name => Str,\n]\n";
        let prog = crate::frontend(src, "t.hard").expect("legacy model parses");
        let m = prog
            .stmts
            .iter()
            .find_map(|s| match s {
                Stmt::Model(m) => Some(m),
                _ => None,
            })
            .expect("model statement");
        assert!(!m.brace, "bracket form");
        assert_eq!(m.table, "users");
        assert_eq!(m.fields.len(), 2, "got {:?}", m.fields);
        assert!(m.fields[0].attrs.iter().any(|a| a.name == "id"), "#id attribute");
    }

    #[test]
    fn fatal_limit_still_aborts() {
        // Run on a large-stack thread: triggering the depth guard recurses
        // ~256 frames which can exceed the default test-thread stack.
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let src = format!("app @3000\n{}", "(".repeat(260));
                let errs = parse_errs(&src);
                assert_eq!(errs.len(), 1);
                assert!(
                    errs[0].message.contains("nesting too deep"),
                    "got {}",
                    errs[0].message
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    fn cache_decl(src: &str) -> (String, i64) {
        match parse_one(src) {
            Stmt::ExprStmt(Expr::Call { callee, args, .. }) => {
                let (base, name) = match callee.as_ref() {
                    Expr::Member(b, n, _) => (b.as_ref(), n.clone()),
                    other => panic!("expected a cache.declare call, got {other:?}"),
                };
                assert!(matches!(base, Expr::Ident(n, _) if n == "cache"), "root is cache");
                assert_eq!(name, "declare");
                assert_eq!(args.len(), 2);
                let key = match &args[0] {
                    Expr::Str(s, _) => s.clone(),
                    other => panic!("expected a name string, got {other:?}"),
                };
                let secs = match &args[1] {
                    Expr::Int(n, _) => *n,
                    other => panic!("expected seconds, got {other:?}"),
                };
                (key, secs)
            }
            other => panic!("expected a desugared declare call, got {other:?}"),
        }
    }

    #[test]
    fn cache_declaration_desugars_with_units() {
        assert_eq!(cache_decl("cache users ttl 10m\n"), ("users".to_string(), 600));
        assert_eq!(cache_decl("cache users ttl 30s\n"), ("users".to_string(), 30));
        assert_eq!(cache_decl("cache users ttl 2h\n"), ("users".to_string(), 7200));
        assert_eq!(cache_decl("cache users ttl 7d\n"), ("users".to_string(), 604800));
        assert_eq!(cache_decl("cache users ttl 1 minute\n"), ("users".to_string(), 60));
        assert_eq!(cache_decl("cache users ttl 3 HOURS\n"), ("users".to_string(), 10800));
    }

    #[test]
    fn cache_declaration_rejects_bad_durations() {
        assert!(!parse_errs("cache users ttl 10x\n").is_empty(), "unknown unit");
        assert!(!parse_errs("cache users ttl m\n").is_empty(), "missing number");
        assert!(!parse_errs("cache users ttl\n").is_empty(), "missing duration");
        assert!(!parse_errs("cache users ttl 10\n").is_empty(), "missing unit");
    }

    #[test]
    fn cache_without_ttl_is_not_a_declaration() {
        // `cache` stays an ordinary identifier: a variable read followed by
        // another statement must not become a declaration.
        let toks: Vec<Token> = crate::Lexer::new("cache\nusers\n").tokenize().unwrap();
        let prog = crate::Parser::new(toks).parse_program().unwrap();
        assert_eq!(prog.stmts.len(), 2, "two expression statements, not a declaration");
    }

    fn job_of(src: &str) -> JobDef {
        match parse_one(src) {
            Stmt::Job(j) => j,
            other => panic!("expected a job declaration, got {other:?}"),
        }
    }

    #[test]
    fn job_declares_params_in_order() {
        let j = job_of("job SendEmail(User user, Int retries)\n");
        assert_eq!(j.name, "SendEmail");
        assert_eq!(j.params.len(), 2);
        assert_eq!(j.params[0].ty, "User");
        assert_eq!(j.params[0].name, "user");
        assert_eq!(j.params[1].ty, "Int");
    }

    #[test]
    fn job_needs_parens_and_unique_names() {
        // Without parentheses there is no declaration: two meaningless but
        // legal expression statements, the same boundary `cache` keeps.
        let toks: Vec<Token> = crate::Lexer::new("job SendEmail\n").tokenize().unwrap();
        let prog = crate::Parser::new(toks).parse_program().unwrap();
        assert_eq!(prog.stmts.len(), 2);
        assert!(!parse_errs("job SendEmail(User user)\njob SendEmail(User user)\n").is_empty(),
            "declared twice");
    }

    #[test]
    fn queue_desugars_to_enqueue_call() {
        // Declared first, so the name resolves.
        let src = "job SendEmail(User user)\nGET \"/\" :: {\n    id <- queue SendEmail(u, delay = 60)\n    <- id\n}\n";
        let prog = crate::frontend(src, "t.hard").expect("parses");
        let route = prog.stmts.iter().find_map(|s| match s {
            Stmt::Route(r) => Some(r),
            _ => None,
        });
        let route = route.expect("a route");
        let call = match &route.body[0] {
            Stmt::Var(v) => &v.value,
            other => panic!("expected the enqueue binding, got {other:?}"),
        };
        let (callee, args) = match call {
            Expr::Call { callee, args, .. } => (callee.as_ref(), args),
            other => panic!("expected a call, got {other:?}"),
        };
        let (base, method) = match callee {
            Expr::Member(b, m, _) => (b.as_ref(), m.clone()),
            other => panic!("expected queue.enqueue, got {other:?}"),
        };
        assert!(matches!(base, Expr::Ident(n, _) if n == "queue"));
        assert_eq!(method, "enqueue");
        assert_eq!(args.len(), 3);
        assert!(matches!(&args[0], Expr::Str(s, _) if s == "SendEmail"));
        // Options ride along as an object: {delay: 60}.
        match &args[2] {
            Expr::Obj(kvs, _) => {
                assert_eq!(kvs.len(), 1);
                assert_eq!(kvs[0].0, "delay");
                assert!(matches!(&kvs[0].1, Expr::Int(60, _)));
            }
            other => panic!("expected options object, got {other:?}"),
        }
    }

    #[test]
    fn queue_rejects_unknown_jobs_and_options() {
        assert!(!parse_errs("GET \"/\" :: {\n id <- queue Nope(1)\n}\n").is_empty(), "undeclared job");
        let errs = parse_errs("job A(Int x)\nGET \"/\" :: {\n id <- queue A(1, bogus = 2)\n}\n");
        assert!(!errs.is_empty(), "unknown option");
        assert!(errs[0].message.contains("bogus"), "got {}", errs[0].message);
    }

    #[test]
    fn worker_lowers_to_a_function_with_payload_binds() {
        let src = "job SendEmail(User user)\nworker SendEmail {\n    sent <- user\n    <- sent\n}\n";
        let prog = crate::frontend(src, "t.hard").expect("parses");
        let func = prog.stmts.iter().find_map(|s| match s {
            Stmt::Func(f) => Some(f),
            _ => None,
        });
        let func = func.expect("a lowered function");
        assert_eq!(func.name, "__worker_SendEmail");
        assert_eq!(func.params.len(), 1);
        assert_eq!(func.params[0].name, "__payload");
        // First statement binds the declared parameter.
        match &func.body[0] {
            Stmt::Var(v) => assert_eq!(v.name, "user"),
            other => panic!("expected the payload bind, got {other:?}"),
        }
    }

    #[test]
    fn worker_needs_its_job_declared_above() {
        let errs = parse_errs("worker Nope {\n    <- 1\n}\n");
        assert!(!errs.is_empty(), "undeclared job");
        assert!(errs[0].message.contains("undeclared job"), "got {}", errs[0].message);
    }
}