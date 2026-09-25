//! Binary serialization for the HardScript AST.
//!
//! The incremental build caches each module's parsed program on disk so a
//! warm rebuild can skip lexing + parsing unchanged files. No third-party
//! serializer is used (the crate deliberately has none): this module encodes
//! the closed set of AST types into a compact, versioned binary format with
//! hard bounds against malicious/corrupt input.

use crate::ast::*;
use crate::token::Span;

const MAX_ITEMS: u64 = 1_000_000;
const MAX_DEPTH: u64 = 512;
const MAX_STR: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

struct Enc {
    out: Vec<u8>,
}

impl Enc {
    fn new() -> Enc {
        Enc { out: Vec::new() }
    }
    fn u8(&mut self, v: u8) {
        self.out.push(v);
    }
    fn uv(&mut self, mut v: i64) {
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            self.out.push(b);
            if v == 0 {
                break;
            }
        }
    }
    fn bool(&mut self, b: bool) {
        self.u8(if b { 1 } else { 0 });
    }
    fn str(&mut self, s: &str) {
        self.uv(s.len() as i64);
        self.out.extend_from_slice(s.as_bytes());
    }
    fn opt_str(&mut self, s: &Option<String>) {
        match s {
            Some(v) => {
                self.bool(true);
                self.str(v);
            }
            None => self.bool(false),
        }
    }
    fn f64(&mut self, f: f64) {
        self.out.extend_from_slice(&f.to_bits().to_le_bytes());
    }
    fn span(&mut self, s: Span) {
        self.uv(s.line as i64);
        self.uv(s.col as i64);
    }
}

fn write_expr(enc: &mut Enc, e: &Expr, depth: u64) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("expr nesting too deep".into());
    }
    match e {
        Expr::Int(v, s) => {
            enc.u8(0);
            enc.uv(*v);
            enc.span(*s);
        }
        Expr::Float(v, s) => {
            enc.u8(1);
            enc.f64(*v);
            enc.span(*s);
        }
        Expr::Str(v, s) => {
            enc.u8(2);
            enc.str(v);
            enc.span(*s);
        }
        Expr::Bool(v, s) => {
            enc.u8(3);
            enc.bool(*v);
            enc.span(*s);
        }
        Expr::List(items, s) => {
            enc.u8(4);
            enc.uv(items.len() as i64);
            for it in items {
                write_expr(enc, it, depth + 1)?;
            }
            enc.span(*s);
        }
        Expr::Obj(kvs, s) => {
            enc.u8(5);
            enc.uv(kvs.len() as i64);
            for (k, v) in kvs {
                enc.str(k);
                write_expr(enc, v, depth + 1)?;
            }
            enc.span(*s);
        }
        Expr::Ident(n, s) => {
            enc.u8(6);
            enc.str(n);
            enc.span(*s);
        }
        Expr::Member(b, f, s) => {
            enc.u8(7);
            write_expr(enc, b, depth + 1)?;
            enc.str(f);
            enc.span(*s);
        }
        Expr::Index(b, i, s) => {
            enc.u8(8);
            write_expr(enc, b, depth + 1)?;
            write_expr(enc, i, depth + 1)?;
            enc.span(*s);
        }
        Expr::Call { callee, args, span } => {
            enc.u8(9);
            write_expr(enc, callee, depth + 1)?;
            enc.uv(args.len() as i64);
            for a in args {
                write_expr(enc, a, depth + 1)?;
            }
            enc.span(*span);
        }
        Expr::Unary(op, x, s) => {
            enc.u8(10);
            enc.u8(unop_tag(op));
            write_expr(enc, x, depth + 1)?;
            enc.span(*s);
        }
        Expr::Binary(op, l, r, s) => {
            enc.u8(11);
            enc.u8(binop_tag(op));
            write_expr(enc, l, depth + 1)?;
            write_expr(enc, r, depth + 1)?;
            enc.span(*s);
        }
        Expr::Range(l, r, s) => {
            enc.u8(12);
            write_expr(enc, l, depth + 1)?;
            write_expr(enc, r, depth + 1)?;
            enc.span(*s);
        }
        Expr::HttpCall { verb, path, body, span } => {
            enc.u8(13);
            enc.str(verb);
            enc.str(path);
            match body {
                Some(b) => {
                    enc.bool(true);
                    write_expr(enc, b, depth + 1)?;
                }
                None => enc.bool(false),
            }
            enc.span(*span);
        }
        Expr::Match(s1, arms, s) => {
            enc.u8(14);
            write_expr(enc, s1, depth + 1)?;
            enc.uv(arms.len() as i64);
            for arm in arms {
                write_expr_arm(enc, arm, depth + 1)?;
            }
            enc.span(*s);
        }
    }
    Ok(())
}

fn write_expr_arm(enc: &mut Enc, arm: &MatchArm, depth: u64) -> Result<(), String> {
    match &arm.value {
        Some(v) => {
            enc.bool(true);
            write_expr(enc, v, depth)?;
        }
        None => enc.bool(false),
    }
    write_expr(enc, &arm.body, depth)
}

fn write_stmt_items(enc: &mut Enc, body: &[Stmt]) -> Result<(), String> {
    enc.uv(body.len() as i64);
    for st in body {
        write_stmt(enc, st, 0)?;
    }
    Ok(())
}

fn write_stmt(enc: &mut Enc, st: &Stmt, depth: u64) -> Result<(), String> {
    if depth > MAX_DEPTH {
        return Err("statement nesting too deep".into());
    }
    match st {
        Stmt::Bring(m, s) => {
            enc.u8(0);
            enc.u8(module_tag(m));
            enc.span(*s);
        }
        Stmt::Import { path, span } => {
            enc.u8(1);
            enc.str(path);
            enc.span(*span);
        }
        Stmt::App(p, s) => {
            enc.u8(2);
            enc.uv(*p);
            enc.span(*s);
        }
        Stmt::Model(m) => {
            enc.u8(3);
            write_model(enc, m)?;
        }
        Stmt::Route(r) => {
            enc.u8(4);
            write_route(enc, r)?;
        }
        Stmt::Socket(s) => {
            enc.u8(5);
            write_socket(enc, s)?;
        }
        Stmt::Func(f) => {
            enc.u8(6);
            write_func(enc, f)?;
        }
        Stmt::Middleware { name, body, span } => {
            enc.u8(7);
            enc.str(name);
            write_stmt_items(enc, body)?;
            enc.span(*span);
        }
        Stmt::Test(t) => {
            enc.u8(8);
            write_test(enc, t)?;
        }
        Stmt::Var(v) => {
            enc.u8(9);
            write_vardef(enc, v)?;
        }
        Stmt::Const(v) => {
            enc.u8(10);
            write_vardef(enc, v)?;
        }
        Stmt::If { cond, then_body, else_body, span } => {
            enc.u8(11);
            write_expr(enc, cond, 0)?;
            write_stmt_items(enc, then_body)?;
            write_stmt_items(enc, else_body)?;
            enc.span(*span);
        }
        Stmt::Loop { var, iter, body, span } => {
            enc.u8(12);
            enc.str(var);
            write_expr(enc, iter, 0)?;
            write_stmt_items(enc, body)?;
            enc.span(*span);
        }
        Stmt::Return(e, s) => {
            enc.u8(13);
            write_expr(enc, e, 0)?;
            enc.span(*s);
        }
        Stmt::Race(es, s) => {
            enc.u8(14);
            enc.uv(es.len() as i64);
            for e in es {
                write_expr(enc, e, 0)?;
            }
            enc.span(*s);
        }
        Stmt::Expect { lhs, op, rhs, span } => {
            enc.u8(15);
            write_expr(enc, lhs, 0)?;
            enc.u8(binop_tag(op));
            write_expr(enc, rhs, 0)?;
            enc.span(*span);
        }
        Stmt::ExprStmt(e) => {
            enc.u8(16);
            write_expr(enc, e, 0)?;
        }
    }
    Ok(())
}

fn write_model(enc: &mut Enc, m: &ModelDef) -> Result<(), String> {
    enc.str(&m.name);
    enc.str(&m.table);
    enc.uv(m.fields.len() as i64);
    for f in &m.fields {
        enc.str(&f.name);
        enc.str(&f.ty);
        enc.uv(f.args.len() as i64);
        for a in &f.args {
            match a {
                FieldArg::Positional(e) => {
                    enc.u8(0);
                    write_expr(enc, e, 1)?;
                }
                FieldArg::Constraint(k, v) => {
                    enc.u8(1);
                    enc.str(k);
                    write_expr(enc, v, 1)?;
                }
            }
        }
        enc.uv(f.attrs.len() as i64);
        for a in &f.attrs {
            enc.str(&a.name);
            match &a.arg {
                Some(e) => {
                    enc.bool(true);
                    write_expr(enc, e, 1)?;
                }
                None => enc.bool(false),
            }
        }
        enc.span(f.span);
    }
    enc.bool(m.brace);
    enc.bool(m.strict);
    enc.span(m.span);
    Ok(())
}

fn write_route(enc: &mut Enc, r: &RouteDef) -> Result<(), String> {
    enc.str(&r.method);
    enc.str(&r.path);
    write_params(enc, &r.params);
    write_stmt_items(enc, &r.body)?;
    enc.span(r.span);
    Ok(())
}

fn write_params(enc: &mut Enc, params: &[FunParam]) {
    enc.uv(params.len() as i64);
    for p in params {
        enc.opt_str(&p.ty);
        enc.str(&p.name);
        enc.bool(p.is_body);
        enc.span(p.span);
    }
}

fn write_socket(enc: &mut Enc, s: &SocketDef) -> Result<(), String> {
    enc.str(&s.path);
    write_opt_block(enc, s.connect.as_deref())?;
    write_opt_block(enc, s.message.as_deref())?;
    write_opt_block(enc, s.disconnect.as_deref())?;
    enc.span(s.span);
    Ok(())
}

fn write_opt_block(enc: &mut Enc, block: Option<&[Stmt]>) -> Result<(), String> {
    match block {
        Some(b) => {
            enc.bool(true);
            write_stmt_items(enc, b)?;
        }
        None => enc.bool(false),
    }
    Ok(())
}

fn write_func(enc: &mut Enc, f: &FunDef) -> Result<(), String> {
    enc.str(&f.name);
    enc.opt_str(&f.ret);
    write_params(enc, &f.params);
    write_stmt_items(enc, &f.body)?;
    enc.span(f.span);
    Ok(())
}

fn write_test(enc: &mut Enc, t: &TestDef) -> Result<(), String> {
    enc.str(&t.name);
    write_stmt_items(enc, &t.body)?;
    enc.span(t.span);
    Ok(())
}

fn write_vardef(enc: &mut Enc, v: &VarDef) -> Result<(), String> {
    enc.str(&v.name);
    enc.opt_str(&v.ty);
    write_expr(enc, &v.value, 0)?;
    enc.span(v.span);
    Ok(())
}

/// Tags are `Module::SET` indices, so the encoding and the decoder cannot
/// drift apart when a module is added. The AST cache magic has to be bumped
/// whenever `Module::SET` changes.
fn module_tag(m: &Module) -> u8 {
    Module::SET
        .iter()
        .position(|x| *x == *m)
        .expect("every Module variant is in Module::SET") as u8
}

fn binop_tag(op: &BinOp) -> u8 {
    match op {
        BinOp::Add => 0,
        BinOp::Sub => 1,
        BinOp::Mul => 2,
        BinOp::Div => 3,
        BinOp::Mod => 4,
        BinOp::Eq => 5,
        BinOp::Ne => 6,
        BinOp::Lt => 7,
        BinOp::Le => 8,
        BinOp::Gt => 9,
        BinOp::Ge => 10,
        BinOp::And => 11,
        BinOp::Or => 12,
    }
}

fn unop_tag(op: &UnOp) -> u8 {
    match op {
        UnOp::Neg => 0,
        UnOp::Not => 1,
    }
}

/// Serialize the top-level items of a module (its `Vec<Stmt>`) to bytes.
pub fn serialize_stmts(stmts: &[Stmt]) -> Result<Vec<u8>, String> {
    let mut enc = Enc::new();
    enc.out.extend_from_slice(b"HS2STMT");
    write_stmt_items(&mut enc, stmts)?;
    Ok(enc.out)
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

struct Dec<'a> {
    bytes: &'a [u8],
    pos: usize,
    items: u64,
}

impl<'a> Dec<'a> {
    fn new(bytes: &'a [u8]) -> Dec<'a> {
        Dec {
            bytes,
            pos: 0,
            items: 0,
        }
    }
    fn byte(&mut self) -> Result<u8, String> {
        let b = *self.bytes.get(self.pos).ok_or("truncated")?;
        self.pos += 1;
        Ok(b)
    }
    fn var(&mut self) -> Result<i64, String> {
        let mut result: i64 = 0;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            result |= ((b & 0x7f) as i64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 64 {
                return Err("varint overflow".into());
            }
        }
        Ok(result)
    }
    fn bool(&mut self) -> Result<bool, String> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("bad bool".into()),
        }
    }
    fn u8(&mut self) -> Result<u8, String> {
        self.byte()
    }
    fn str(&mut self) -> Result<String, String> {
        let len = self.var()?;
        if len < 0 || len as usize > MAX_STR {
            return Err("string too long".into());
        }
        let len = len as usize;
        let end = self.pos + len;
        let slice = self.bytes.get(self.pos..end).ok_or("truncated string")?;
        self.pos = end;
        String::from_utf8(slice.to_vec()).map_err(|_| "bad utf8".into())
    }
    fn opt_str(&mut self) -> Result<Option<String>, String> {
        if self.bool()? {
            Ok(Some(self.str()?))
        } else {
            Ok(None)
        }
    }
    fn f64(&mut self) -> Result<f64, String> {
        let mut buf = [0u8; 8];
        for slot in buf.iter_mut() {
            *slot = self.byte()?;
        }
        Ok(f64::from_bits(u64::from_le_bytes(buf)))
    }
    fn span(&mut self) -> Result<Span, String> {
        let line = self.var()?;
        let col = self.var()?;
        Ok(Span::new(line.max(0) as usize, col.max(0) as usize))
    }
    fn bump(&mut self) -> Result<(), String> {
        self.items += 1;
        if self.items > MAX_ITEMS {
            return Err("too many items".into());
        }
        Ok(())
    }
}

fn read_expr(dec: &mut Dec, depth: u64) -> Result<Expr, String> {
    if depth > MAX_DEPTH {
        return Err("expr nesting too deep".into());
    }
    let tag = dec.byte()?;
    Ok(match tag {
        0 => Expr::Int(dec.var()?, dec.span()?),
        1 => Expr::Float(dec.f64()?, dec.span()?),
        2 => Expr::Str(dec.str()?, dec.span()?),
        3 => Expr::Bool(dec.bool()?, dec.span()?),
        4 => {
            let n = decode_count(dec)?;
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(read_expr(dec, depth + 1)?);
            }
            let s = dec.span()?;
            Expr::List(items, s)
        }
        5 => {
            let n = decode_count(dec)?;
            let mut kvs = Vec::with_capacity(n);
            for _ in 0..n {
                let k = dec.str()?;
                let v = read_expr(dec, depth + 1)?;
                kvs.push((k, v));
            }
            let s = dec.span()?;
            Expr::Obj(kvs, s)
        }
        6 => {
            let n = dec.str()?;
            let s = dec.span()?;
            Expr::Ident(n, s)
        }
        7 => {
            let b = read_expr(dec, depth + 1)?;
            let f = dec.str()?;
            let s = dec.span()?;
            Expr::Member(Box::new(b), f, s)
        }
        8 => {
            let b = read_expr(dec, depth + 1)?;
            let i = read_expr(dec, depth + 1)?;
            let s = dec.span()?;
            Expr::Index(Box::new(b), Box::new(i), s)
        }
        9 => {
            let callee = read_expr(dec, depth + 1)?;
            let n = decode_count(dec)?;
            let mut args = Vec::with_capacity(n);
            for _ in 0..n {
                args.push(read_expr(dec, depth + 1)?);
            }
            let span = dec.span()?;
            Expr::Call {
                callee: Box::new(callee),
                args,
                span,
            }
        }
        10 => {
            let op = unop_from_tag(dec.byte()?)?;
            let x = read_expr(dec, depth + 1)?;
            let s = dec.span()?;
            Expr::Unary(op, Box::new(x), s)
        }
        11 => {
            let op = binop_from_tag(dec.byte()?)?;
            let l = read_expr(dec, depth + 1)?;
            let r = read_expr(dec, depth + 1)?;
            let s = dec.span()?;
            Expr::Binary(op, Box::new(l), Box::new(r), s)
        }
        12 => {
            let l = read_expr(dec, depth + 1)?;
            let r = read_expr(dec, depth + 1)?;
            let s = dec.span()?;
            Expr::Range(Box::new(l), Box::new(r), s)
        }
        13 => {
            let verb = dec.str()?;
            let path = dec.str()?;
            let body = if dec.bool()? {
                Some(Box::new(read_expr(dec, depth + 1)?))
            } else {
                None
            };
            let span = dec.span()?;
            Expr::HttpCall { verb, path, body, span }
        }
        14 => {
            let subj = read_expr(dec, depth + 1)?;
            let n = decode_count(dec)?;
            let mut arms = Vec::with_capacity(n);
            for _ in 0..n {
                arms.push(read_arm(dec, depth + 1)?);
            }
            let s = dec.span()?;
            Expr::Match(Box::new(subj), arms, s)
        }
        other => return Err(format!("unknown expr tag {other}")),
    })
}

fn read_arm(dec: &mut Dec, depth: u64) -> Result<MatchArm, String> {
    let value = if dec.bool()? {
        Some(read_expr(dec, depth)?)
    } else {
        None
    };
    let body = read_expr(dec, depth)?;
    Ok(MatchArm { value, body })
}

fn read_ms(dec: &mut Dec, depth: u64) -> Result<Vec<Stmt>, String> {
    let n = decode_count(dec)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        dec.bump()?;
        out.push(read_stmt(dec, depth)?);
    }
    Ok(out)
}

fn read_stmt(dec: &mut Dec, depth: u64) -> Result<Stmt, String> {
    if depth > MAX_DEPTH {
        return Err("statement nesting too deep".into());
    }
    let tag = dec.byte()?;
    Ok(match tag {
        0 => {
            let m = module_from_tag(dec.byte()?)?;
            let s = dec.span()?;
            Stmt::Bring(m, s)
        }
        1 => {
            let path = dec.str()?;
            let span = dec.span()?;
            Stmt::Import { path, span }
        }
        2 => {
            let p = dec.var()?;
            let s = dec.span()?;
            Stmt::App(p, s)
        }
        3 => Stmt::Model(read_model(dec)?),
        4 => Stmt::Route(read_route(dec)?),
        5 => Stmt::Socket(read_socket(dec)?),
        6 => Stmt::Func(read_func(dec)?),
        7 => {
            let name = dec.str()?;
            let body = read_ms(dec, depth + 1)?;
            let span = dec.span()?;
            Stmt::Middleware { name, body, span }
        }
        8 => Stmt::Test(read_test(dec)?),
        9 => Stmt::Var(read_vardef(dec)?),
        10 => Stmt::Const(read_vardef(dec)?),
        11 => {
            let cond = read_expr(dec, 0)?;
            let then_body = read_ms(dec, depth + 1)?;
            let else_body = read_ms(dec, depth + 1)?;
            let span = dec.span()?;
            Stmt::If { cond, then_body, else_body, span }
        }
        12 => {
            let var = dec.str()?;
            let iter = read_expr(dec, 0)?;
            let body = read_ms(dec, depth + 1)?;
            let span = dec.span()?;
            Stmt::Loop { var, iter, body, span }
        }
        13 => {
            let e = read_expr(dec, 0)?;
            let s = dec.span()?;
            Stmt::Return(e, s)
        }
        14 => {
            let n = decode_count(dec)?;
            let mut es = Vec::with_capacity(n);
            for _ in 0..n {
                es.push(read_expr(dec, 0)?);
            }
            let s = dec.span()?;
            Stmt::Race(es, s)
        }
        15 => {
            let lhs = read_expr(dec, 0)?;
            let op = binop_from_tag(dec.byte()?)?;
            let rhs = read_expr(dec, 0)?;
            let span = dec.span()?;
            Stmt::Expect { lhs, op, rhs, span }
        }
        16 => {
            let e = read_expr(dec, 0)?;
            Stmt::ExprStmt(e)
        }
        other => return Err(format!("unknown stmt tag {other}")),
    })
}

fn decode_count(dec: &mut Dec) -> Result<usize, String> {
    let n = dec.var()?;
    if n < 0 || n as u64 > MAX_ITEMS {
        return Err("count out of range".into());
    }
    Ok(n as usize)
}

fn read_model(dec: &mut Dec) -> Result<ModelDef, String> {
    let name = dec.str()?;
    let table = dec.str()?;
    let n = decode_count(dec)?;
    let mut fields = Vec::with_capacity(n);
    for _ in 0..n {
        let fname = dec.str()?;
        let ty = dec.str()?;
        let an = decode_count(dec)?;
        let mut args = Vec::with_capacity(an);
        for _ in 0..an {
            match dec.u8()? {
                0 => args.push(FieldArg::Positional(read_expr(dec, 1)?)),
                1 => {
                    let k = dec.str()?;
                    args.push(FieldArg::Constraint(k, Box::new(read_expr(dec, 1)?)));
                }
                other => return Err(format!("unknown field-arg tag {other}")),
            }
        }
        let cn = decode_count(dec)?;
        let mut attrs = Vec::with_capacity(cn);
        for _ in 0..cn {
            let aname = dec.str()?;
            let arg = if dec.bool()? { Some(read_expr(dec, 1)?) } else { None };
            attrs.push(FieldAttr { name: aname, arg });
        }
        let span = dec.span()?;
        fields.push(FieldDef { name: fname, ty, args, attrs, span });
    }
    let brace = dec.bool()?;
    let strict = dec.bool()?;
    let span = dec.span()?;
    Ok(ModelDef { name, table, fields, brace, strict, span })
}

fn read_params(dec: &mut Dec) -> Result<Vec<FunParam>, String> {
    let n = decode_count(dec)?;
    let mut params = Vec::with_capacity(n);
    for _ in 0..n {
        let ty = dec.opt_str()?;
        let name = dec.str()?;
        let is_body = dec.bool()?;
        let span = dec.span()?;
        params.push(FunParam { ty, name, is_body, span });
    }
    Ok(params)
}

fn read_route(dec: &mut Dec) -> Result<RouteDef, String> {
    let method = dec.str()?;
    let path = dec.str()?;
    let params = read_params(dec)?;
    let body = read_ms(dec, 1)?;
    let span = dec.span()?;
    Ok(RouteDef { method, path, params, body, span })
}

fn read_socket(dec: &mut Dec) -> Result<SocketDef, String> {
    let path = dec.str()?;
    let connect = read_opt_block(dec)?;
    let message = read_opt_block(dec)?;
    let disconnect = read_opt_block(dec)?;
    let span = dec.span()?;
    Ok(SocketDef { path, connect, message, disconnect, span })
}

fn read_opt_block(dec: &mut Dec) -> Result<Option<Vec<Stmt>>, String> {
    if dec.bool()? {
        Ok(Some(read_ms(dec, 1)?))
    } else {
        Ok(None)
    }
}

fn read_func(dec: &mut Dec) -> Result<FunDef, String> {
    let name = dec.str()?;
    let ret = dec.opt_str()?;
    let params = read_params(dec)?;
    let body = read_ms(dec, 1)?;
    let span = dec.span()?;
    Ok(FunDef { name, ret, params, body, span })
}

fn read_test(dec: &mut Dec) -> Result<TestDef, String> {
    let name = dec.str()?;
    let body = read_ms(dec, 1)?;
    let span = dec.span()?;
    Ok(TestDef { name, body, span })
}

fn read_vardef(dec: &mut Dec) -> Result<VarDef, String> {
    let name = dec.str()?;
    let ty = dec.opt_str()?;
    let value = read_expr(dec, 0)?;
    let span = dec.span()?;
    Ok(VarDef { name, ty, value, span })
}

fn module_from_tag(tag: u8) -> Result<Module, String> {
    Module::SET
        .get(tag as usize)
        .ok_or_else(|| format!("unknown module tag {tag}"))
        .map(|m| m.clone())
}

fn binop_from_tag(tag: u8) -> Result<BinOp, String> {
    Ok(match tag {
        0 => BinOp::Add,
        1 => BinOp::Sub,
        2 => BinOp::Mul,
        3 => BinOp::Div,
        4 => BinOp::Mod,
        5 => BinOp::Eq,
        6 => BinOp::Ne,
        7 => BinOp::Lt,
        8 => BinOp::Le,
        9 => BinOp::Gt,
        10 => BinOp::Ge,
        11 => BinOp::And,
        12 => BinOp::Or,
        other => return Err(format!("unknown binop tag {other}")),
    })
}

fn unop_from_tag(tag: u8) -> Result<UnOp, String> {
    Ok(match tag {
        0 => UnOp::Neg,
        1 => UnOp::Not,
        other => return Err(format!("unknown unop tag {other}")),
    })
}

/// Deserialize module items previously written by [`serialize_stmts`].
pub fn deserialize_stmts(bytes: &[u8]) -> Result<Vec<Stmt>, String> {
    if !bytes.starts_with(b"HS2STMT") {
        return Err("bad magic".into());
    }
    let mut dec = Dec::new(&bytes[7..]);
    let stmts = read_ms(&mut dec, 0)?;
    // Strict: the whole buffer must be consumed (catches truncation/corruption).
    if dec.pos != bytes[7..].len() {
        return Err("trailing bytes".into());
    }
    Ok(stmts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fmt::format;
    use crate::parser::Parser;
    use crate::token::Token;
    use crate::Lexer;

    fn parse_ok(src: &str) -> Vec<Stmt> {
        let toks: Vec<Token> = Lexer::new(src).tokenize().unwrap();
        let prog = Parser::new(toks).parse_program().unwrap();
        prog.stmts
    }

    #[test]
    fn round_trip_preserves_all_constructs() {
        let src = r#"
bring http
bring "./utils"
app @3031

model User = users [
    id => Int #id,
    name => Str,
]

items ::= [1, 2.5, 3]

before auth :: {
    req <- req
}

socket "/s" {
    connect :: {
        websocket.join("room")
    }
    message(d) :: {
        websocket.broadcast_room("room", "echo:" + d)
    }
    disconnect :: {
    }
}

calc greet(Str name) => Str {
    out <- "hello, " + name
    ?(out + "" == "go") {
        <- out
    } :{
        <- "short"
    }
}

GET "/users" :: (id = Str, body = body) {
    vals <- [1, 2, 3]
    n <- id
    expect n != ""
    <- { a: 1, name: n, vals: vals }
}

test "hello" {
    created <- POST "/users" { { name: "grace" } }
    expect created.body.status == 201
}
"#;
        let before = parse_ok(src);
        let bytes = serialize_stmts(&before).unwrap();
        let after = deserialize_stmts(&bytes).unwrap();
        // The canonical formatter output must be identical, proving the AST
        // round-trips exactly.
        assert_eq!(format_program(&before), format_program(&after));
    }

    #[test]
    fn import_stmt_survives() {
        let before = parse_ok("bring \"./utils\"\n");
        let after = deserialize_stmts(&serialize_stmts(&before).unwrap()).unwrap();
        match &after[0] {
            Stmt::Import { path, .. } => assert_eq!(path, "./utils"),
            other => panic!("expected Import, got {other:?}"),
        }
    }

    fn format_program(stmts: &[Stmt]) -> String {
        let prog = Program {
            stmts: stmts.to_vec(),
            path: String::new(),
        };
        format(&prog)
    }

    #[test]
    fn corrupt_input_errors_cleanly() {
        assert!(deserialize_stmts(b"garbage").is_err());
        let mut bytes = serialize_stmts(&parse_ok("GET \"/\" :: { <- 1 }")).unwrap();
        // Flip a byte mid-stream to simulate disk corruption.
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xff;
        // Must not panic; must return Err (or a well-formed-but-failed parse).
        let _ = deserialize_stmts(&bytes);
    }

    #[test]
    fn framework_model_metadata_round_trips() {
        // The v0.6 model form added type arguments, field attributes, the
        // brace/legacy discriminator and the model-level `@strict` flag. The
        // AST cache is keyed by a source hash, so a field that fails to
        // survive a round trip shows up as a stale-schema build rather than an
        // error; assert the whole shape instead.
        let src = "model User = people @strict {\n    id : Uuid!\n    email : Email!\n    age : Int(min=18, max=120)\n    role : Enum(\"admin\", \"user\")\n    bio : Str(max=280, nullable) @index\n}\n";
        let before = parse_ok(src);
        let after = deserialize_stmts(&serialize_stmts(&before).unwrap()).unwrap();
        assert_eq!(before.len(), after.len(), "statement count");

        let pick = |p: &[Stmt]| match &p[0] {
            Stmt::Model(m) => format!("{m:?}"),
            other => panic!("expected a model, got {other:?}"),
        };
        assert_eq!(pick(&before), pick(&after), "model must survive the cache");
        let m = match &after[0] {
            Stmt::Model(m) => m,
            _ => unreachable!(),
        };
        assert!(m.brace && m.strict, "flags: {:?}", (m.brace, m.strict));
        assert_eq!(m.table, "people");
        assert_eq!(m.fields[2].args.len(), 2, "min/max args");
        assert_eq!(m.fields[3].args.len(), 2, "enum members");
        assert!(m.fields[4].attrs.iter().any(|a| a.name == "index"), "@index");
    }

    #[test]
    fn every_module_survives_the_cache() {
        // `bring auth` and the rest of the v0.6 catalog were encoded but the
        // decoder still stopped at `time`, so a program that brought a
        // framework module could never be read back from the build cache.
        let mut src = String::new();
        for name in Module::SET_NAMES {
            src.push_str(&format!("bring {name}\n"));
        }
        let before = parse_ok(&src);
        assert_eq!(before.len(), Module::SET.len(), "one bring per module");
        let after = deserialize_stmts(&serialize_stmts(&before).unwrap()).unwrap();
        assert_eq!(format!("{before:?}"), format!("{after:?}"), "modules must round-trip");
        for name in Module::SET_NAMES {
            assert!(
                deserialize_stmts(&serialize_stmts(&parse_ok(&format!("bring {name}\n"))).unwrap())
                    .is_ok(),
                "bring {name} must decode"
            );
        }
    }

    #[test]
    fn many_statements_round_trip() {
        let mut src = String::new();
        for i in 0..500 {
            src.push_str(&format!("add{i} ::= {i}\n"));
        }
        let before = parse_ok(&src);
        let after = deserialize_stmts(&serialize_stmts(&before).unwrap()).unwrap();
        assert_eq!(before.len(), after.len());
    }
}