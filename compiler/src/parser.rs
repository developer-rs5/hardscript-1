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
    pub middlewares: Vec<(String, Vec<Stmt>)>,
    pub tests: Vec<TestDef>,
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
            middlewares: Vec::new(),
            tests: Vec::new(),
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
        if errs.is_empty() {
            Ok(Program { stmts, path: String::new() })
        } else {
            Err(errs)
        }
    }

    fn recover(&mut self) {
        while !matches!(self.peek(), Tok::Eof | Tok::Sym(Sym::RBrace)) {
            self.advance();
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

    fn parse_block_stmt(&mut self) -> Result<Stmt, Vec<Diag>> {
        let sp = self.span();
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
                    "Valid modules: http, postgres, websocket, crypto, json, fs, jwt, env, runtime, time.",
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
                    "Use `bring http`, `bring \"./utils\"`, or `bring std.crypto`.",
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
                "Valid modules: http, postgres, websocket, crypto, json, fs, jwt, env, runtime, time, or a local import like `bring \"./utils\"`.",
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
            match self.parse_stmt()? {
                Some(s) => stmts.push(s),
                None => {}
            }
        }
        self.expect_sym(Sym::RBrace, "to close a block")?;
        Ok(stmts)
    }

    fn parse_model(&mut self) -> Result<Stmt, Vec<Diag>> {
        let sp = self.span();
        self.advance();
        let (name, nsp) = self.expect_ident("as a model name")?;
        let table = if self.eat_sym(Sym::Assign) {
            let (t, _) = self.expect_ident("as a table name")?;
            t
        } else {
            name.to_lowercase()
        };
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
            let mut attrs = Vec::new();
            while self.eat_sym(Sym::Hash) {
                let (a, _) = self.expect_ident("after `#` in model attributes")?;
                attrs.push(a);
            }
            fields.push(FieldDef { name: fname, ty, attrs, span: fsp });
            if !self.eat_sym(Sym::Comma) {
                break;
            }
        }
        self.expect_sym(Sym::RBracket, "to close a model")?;
        let md = ModelDef {
            name,
            table: table.clone(),
            fields: fields.clone(),
            span: nsp,
        };
        self.models.push(md.clone());
        Ok(Stmt::Model(md))
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
        let r = self.parse_or();
        self.exit_depth();
        r
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