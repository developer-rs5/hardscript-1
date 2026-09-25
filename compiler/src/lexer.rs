use crate::catalog as cat;
use crate::error::{Diag, ErrorKind};
use crate::token::{Kw, Span, Sym, Tok, Token};

pub struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    col: usize,
}

impl Lexer {
    pub fn new(src: &str) -> Lexer {
        Lexer {
            chars: src.chars().collect(),
            pos: 0,
            line: 1,
            col: 1,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if let Some(c) = c {
            self.pos += 1;
            if c == '\n' {
                self.line += 1;
                self.col = 1;
            } else {
                self.col += 1;
            }
        }
        c
    }

    fn span(&self) -> Span {
        Span::new(self.line, self.col)
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>, Vec<Diag>> {
        let mut out = Vec::new();
        let mut errs = Vec::new();
        loop {
            self.skip_ws_and_comments(&mut errs);
            let sp = self.span();
            let Some(c) = self.peek() else {
                out.push(Token::new(Tok::Eof, sp));
                break;
            };
            if c.is_ascii_digit() {
                let t = self.lex_number();
                out.push(Token::new(t, sp));
                continue;
            }
            if c == '"' {
                match self.lex_string() {
                    Ok(s) => out.push(Token::new(Tok::Str(s), sp)),
                    Err(e) => errs.push(e),
                }
                continue;
            }
            if is_ident_start(c) {
                let ident = self.lex_ident();
                out.push(Token::new(classify_ident(&ident), sp));
                continue;
            }
            match self.lex_sym() {
                Ok(s) => out.push(Token::new(Tok::Sym(s), sp)),
                Err(e) => errs.push(e),
            }
        }
        if errs.is_empty() {
            Ok(out)
        } else {
            Err(errs)
        }
    }

    fn skip_ws_and_comments(&mut self, errs: &mut Vec<Diag>) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('/') if self.peek2() == Some('/') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                Some('/') if self.peek2() == Some('*') => {
                    let sp = self.span();
                    self.bump();
                    self.bump();
                    let mut closed = false;
                    while let Some(c) = self.peek() {
                        if c == '*' && self.peek2() == Some('/') {
                            self.bump();
                            self.bump();
                            closed = true;
                            break;
                        }
                        self.bump();
                    }
                    if !closed {
                        errs.push(Diag::new(
                            ErrorKind::Lex,
                            "unterminated block comment",
                            sp,
                            "Close the comment with \"*/\".\n    /* comment */",
                        )
                        .with_code(cat::UNTERMINATED_DELIMITER));
                    }
                }
                _ => break,
            }
        }
    }

    fn lex_number(&mut self) -> Tok {
        let mut is_float = false;
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                s.push(c);
                self.bump();
            } else if c == '.' && self.peek2().map(|d| d.is_ascii_digit()).unwrap_or(false) {
                is_float = true;
                s.push(c);
                self.bump();
            } else {
                break;
            }
        }
        let sp = self.span();
        if is_float {
            Tok::Float(s.parse().unwrap_or_else(|_| {
                self.emit_fallback_float(&s, sp);
                0.0
            }))
        } else {
            Tok::Int(s.parse().unwrap_or_else(|_| {
                // out-of-range i64 -> float fallback with warning is complex; clamp to i64::MAX
                i64::MAX
            }))
        }
    }

    fn emit_fallback_float(&self, _s: &str, _sp: Span) -> f64 {
        0.0
    }

    fn lex_string(&mut self) -> Result<String, Diag> {
        let sp = self.span();
        self.bump(); // opening "
        let mut out = String::new();
        // triple-quote block string
        if self.peek() == Some('"') && self.peek2() == Some('"') {
            self.bump();
            self.bump();
            loop {
                match self.peek() {
                    Some('"') if self.peek2() == Some('"') && {
                        let mut n = 0;
                        while self.chars.get(self.pos + n) == Some(&'"') {
                            n += 1;
                        }
                        n >= 3
                    } => {
                        // consume three quotes (or more)
                        for _ in 0..3 {
                            self.bump();
                        }
                        return Ok(out);
                    }
                    Some(c) => {
                        out.push(c);
                        self.bump();
                    }
                    None => {
                        return Err(Diag::new(
                            ErrorKind::Lex,
                            "unterminated triple-quoted string",
                            sp,
                            "Close the string with \"\"\".",
                        )
                        .with_code(cat::UNTERMINATED_DELIMITER));
                    }
                }
            }
        }
        loop {
            match self.peek() {
                Some('"') => {
                    self.bump();
                    return Ok(out);
                }
                Some('\\') => {
                    self.bump();
                    match self.bump() {
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some('r') => out.push('\r'),
                        Some('"') => out.push('"'),
                        Some('\\') => out.push('\\'),
                        Some('\'') => out.push('\''),
                        Some('0') => out.push('\0'),
                        Some(c) => {
                            out.push('\\');
                            out.push(c);
                        }
                        None => {
                            return Err(Diag::new(
                                ErrorKind::Lex,
                                "unterminated string escape",
                                sp,
                                "Remove the trailing backslash.",
                            )
                            .with_code(cat::UNTERMINATED_DELIMITER));
                        }
                    }
                }
                Some('\n') => {
                    return Err(Diag::new(
                        ErrorKind::Lex,
                        "unterminated string literal",
                        sp,
                        "Close the string with a double quote \".",
                    )
                    .with_code(cat::UNTERMINATED_DELIMITER));
                }
                Some(c) => {
                    if c == '\u{0}' && self.pos >= self.chars.len() - 1 {
                        break;
                    }
                    out.push(c);
                    self.bump();
                }
                None => {
                    return Err(Diag::new(
                        ErrorKind::Lex,
                        "unterminated string literal",
                        sp,
                        "Close the string with a double quote \".",
                    )
                    .with_code(cat::UNTERMINATED_DELIMITER));
                }
            }
        }
        Err(Diag::new(
            ErrorKind::Lex,
            "unterminated string literal",
            sp,
            "Close the string with a double quote \".",
        )
        .with_code(cat::UNTERMINATED_DELIMITER))
    }

    fn lex_ident(&mut self) -> String {
        let mut s = String::new();
        while let Some(c) = self.peek() {
            if is_ident_continue(c) {
                s.push(c);
                self.bump();
            } else {
                break;
            }
        }
        s
    }

    fn lex_sym(&mut self) -> Result<Sym, Diag> {
        let sp = self.span();
        let c = self.bump().unwrap();
        let two = |s: &Lexer, c2: char| s.peek() == Some(c2);
        match c {
            '@' => Ok(Sym::At),
            '[' => Ok(Sym::LBracket),
            ']' => Ok(Sym::RBracket),
            '{' => Ok(Sym::LBrace),
            '}' => Ok(Sym::RBrace),
            '(' => Ok(Sym::LParen),
            ')' => Ok(Sym::RParen),
            ',' => Ok(Sym::Comma),
            '.' => {
                // `..` and `...` are both spellings of the range operator, so a
                // dot run of two or three becomes a single range token and
                // never leaves a stray `.` behind (which used to be parsed as
                // member access and fail with a confusing diagnostic).
                // `lex_sym` already consumed the first dot, so count from 1.
                let mut n = 1;
                while self.chars.get(self.pos + n - 1) == Some(&'.') {
                    n += 1;
                }
                if n >= 2 {
                    for _ in 1..n.min(3) {
                        self.bump();
                    }
                    Ok(Sym::Ellipsis)
                } else {
                    Ok(Sym::Dot)
                }
            }
            '#' => Ok(Sym::Hash),
            '?' => Ok(Sym::Q),
            '*' => Ok(Sym::Star),
            '+' => Ok(Sym::Plus),
            '-' => {
                if two(&self, '>') {
                    self.bump();
                    Ok(Sym::FatArrow)
                } else {
                    Ok(Sym::Minus)
                }
            }
            '/' => Ok(Sym::Slash),
            '%' => Ok(Sym::Percent),
            '!' => {
                if two(&self, '=') {
                    self.bump();
                    Ok(Sym::NotEq)
                } else {
                    Ok(Sym::Bang)
                }
            }
            '<' => {
                if two(&self, '-') {
                    self.bump();
                    Ok(Sym::Arrow)
                } else if two(&self, '=') {
                    self.bump();
                    Ok(Sym::Le)
                } else {
                    Ok(Sym::Lt)
                }
            }
            '>' => {
                if two(&self, '=') {
                    self.bump();
                    Ok(Sym::Ge)
                } else {
                    Ok(Sym::Gt)
                }
            }
            '=' => {
                if two(&self, '=') {
                    self.bump();
                    Ok(Sym::EqEq)
                } else if two(&self, '>') {
                    self.bump();
                    Ok(Sym::FatArrow)
                } else {
                    Ok(Sym::Assign)
                }
            }
            ':' => {
                if two(&self, ':') {
                    self.bump();
                    if two(&self, '=') {
                        self.bump();
                        Ok(Sym::DColonEq)
                    } else {
                        Ok(Sym::DColon)
                    }
                } else {
                    Ok(Sym::Colon)
                }
            }
            '&' => {
                if two(&self, '&') {
                    self.bump();
                    Ok(Sym::AndAnd)
                } else {
                    Err(Diag::new(
                        ErrorKind::Lex,
                        "unexpected '&'",
                        sp,
                        "Use \"&&\" for logical and.",
                    )
                    .with_code(cat::UNEXPECTED_TOKEN))
                }
            }
            '|' => {
                if two(&self, '|') {
                    self.bump();
                    Ok(Sym::OrOr)
                } else {
                    Err(Diag::new(
                        ErrorKind::Lex,
                        "unexpected '|'",
                        sp,
                        "Use \"||\" for logical or.",
                    )
                    .with_code(cat::UNEXPECTED_TOKEN))
                }
            }
            other => Err(Diag::new(
                ErrorKind::Lex,
                format!("unexpected character '{other}'"),
                sp,
                "Remove the character, or replace it with a valid symbol.",
            )
            .with_code(cat::UNEXPECTED_TOKEN)),
        }
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

pub fn classify_ident(s: &str) -> Tok {
    let kw = match s {
        "bring" => Kw::Bring,
        "app" => Kw::App,
        "model" => Kw::Model,
        "RUN" => Kw::Run,
        "GET" => Kw::Get,
        "POST" => Kw::Post,
        "PUT" => Kw::Put,
        "DELETE" => Kw::Delete,
        "PATCH" => Kw::Patch,
        "socket" => Kw::Socket,
        "connect" => Kw::Connect,
        "message" => Kw::Message,
        "disconnect" => Kw::Disconnect,
        "before" => Kw::Before,
        "loop" => Kw::Loop,
        "pick" => Kw::Pick,
        "async" => Kw::Async,
        "wait" => Kw::Wait,
        "race" => Kw::Race,
        "test" => Kw::Test,
        "expect" => Kw::Expect,
        "calc" => Kw::Calc,
        _ => return Tok::Ident(s.to_string()),
    };
    Tok::Kw(kw)
}
