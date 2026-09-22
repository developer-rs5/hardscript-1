use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub line: usize,
    pub col: usize,
}

impl Span {
    pub fn new(line: usize, col: usize) -> Span {
        Span { line, col }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),
    Int(i64),
    Float(f64),
    Str(String),
    Kw(Kw),
    Sym(Sym),
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kw {
    Bring,
    App,
    Model,
    Run,
    Get,
    Post,
    Put,
    Delete,
    Patch,
    Socket,
    Connect,
    Message,
    Disconnect,
    Before,
    Loop,
    Pick,
    Async,
    Wait,
    Race,
    Test,
    Expect,
    Calc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sym {
    At,          // @
    LBracket,    // [
    RBracket,    // ]
    LBrace,      // {
    RBrace,      // }
    LParen,      // (
    RParen,      // )
    Comma,       // ,
    Colon,       // :
    DColon,      // ::
    DColonEq,    // ::=
    Arrow,       // <-
    FatArrow,    // =>
    Dot,         // .
    Hash,        // #
    Q,           // ?
    Star,        // *
    Plus,        // +
    Minus,       // -
    Slash,       // /
    Percent,     // %
    Bang,        // !
    Lt,          // <
    Le,          // <=
    Gt,          // >
    Ge,          // >=
    EqEq,        // ==
    NotEq,       // !=
    AndAnd,      // &&
    OrOr,        // ||
    Assign,      // =
    Ellipsis,    // ...
}

impl Sym {
    pub fn as_str(&self) -> &'static str {
        use Sym::*;
        match self {
            At => "@",
            LBracket => "[",
            RBracket => "]",
            LBrace => "{",
            RBrace => "}",
            LParen => "(",
            RParen => ")",
            Comma => ",",
            Colon => ":",
            DColon => "::",
            DColonEq => "::=",
            Arrow => "<-",
            FatArrow => "=>",
            Dot => ".",
            Hash => "#",
            Q => "?",
            Star => "*",
            Plus => "+",
            Minus => "-",
            Slash => "/",
            Percent => "%",
            Bang => "!",
            Lt => "<",
            Le => "<=",
            Gt => ">",
            Ge => ">=",
            EqEq => "==",
            NotEq => "!=",
            AndAnd => "&&",
            OrOr => "||",
            Assign => "=",
            Ellipsis => "...",
        }
    }
}

impl fmt::Display for Sym {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`", self.as_str())
    }
}

impl fmt::Display for Kw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}`", format!("{:?}", self).to_lowercase())
    }
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use Tok::*;
        match self {
            Ident(s) => write!(f, "`{s}`"),
            Int(n) => write!(f, "the number `{n}`"),
            Float(x) => write!(f, "the number `{x}`"),
            Str(s) => write!(f, "the string {:?}", s),
            Kw(k) => write!(f, "{k}"),
            Sym(s) => write!(f, "{s}"),
            Eof => write!(f, "end of file"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

impl Token {
    pub fn new(tok: Tok, span: Span) -> Token {
        Token { tok, span }
    }
}