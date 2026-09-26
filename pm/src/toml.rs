//! A small, focused TOML parser for `hard.toml`.
//!
//! Supports the subset HardScript manifests need — bare/dotted/quoted keys,
//! `[table]` headers, basic and literal strings, integers, floats, booleans,
//! comments, classic arrays and inline tables — and reports helpful
//! line/column diagnostics (with a snippet) instead of raw parse errors.
//!
//! It is deliberately dependency-free so the package manager crate keeps the
//! same minimal footprint as the compiler.

use std::collections::BTreeMap;

/// A TOML value. Only the scalar/collection kinds a manifest uses.
#[derive(Clone, Debug, PartialEq)]
pub enum TomlValue {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Array(Vec<TomlValue>),
    Table(BTreeMap<String, TomlValue>),
    /// A date/time literal, kept as its original text.
    DateTime(String),
}

impl TomlValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            TomlValue::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            TomlValue::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            TomlValue::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&[TomlValue]> {
        match self {
            TomlValue::Array(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_table(&self) -> Option<&BTreeMap<String, TomlValue>> {
        match self {
            TomlValue::Table(t) => Some(t),
            _ => None,
        }
    }
    pub fn as_table_mut(&mut self) -> Option<&mut BTreeMap<String, TomlValue>> {
        match self {
            TomlValue::Table(t) => Some(t),
            _ => None,
        }
    }
}

/// A diagnostic with a source location and an optional suggestion.
#[derive(Clone, Debug)]
pub struct TomlDiag {
    pub line: usize,
    pub col: usize,
    pub message: String,
    pub suggestion: Option<String>,
}

/// A parsed TOML document. Key order does not matter; iteration uses the
/// deterministic [`BTreeMap`] order.
#[derive(Clone, Debug, Default)]
pub struct TomlDoc {
    pub root: BTreeMap<String, TomlValue>,
}

impl TomlDoc {
    pub fn get(&self, key: &str) -> Option<&TomlValue> {
        self.root.get(key)
    }
    pub fn table(&self, key: &str) -> Option<&BTreeMap<String, TomlValue>> {
        self.get(key).and_then(TomlValue::as_table)
    }
}

/// Parse a TOML document. On error, returns every diagnostic found (the
/// parser recovers after each line so a bad file reports all its problems).
pub fn parse(src: &str) -> Result<TomlDoc, Vec<TomlDiag>> {
    let mut parser = Parser {
        src,
        pos: 0,
        line: 1,
        col: 1,
        root: BTreeMap::new(),
        cur_table: Vec::new(),
        diags: Vec::new(),
    };
    parser.parse_document();
    if parser.diags.is_empty() {
        Ok(TomlDoc {
            root: parser.root,
        })
    } else {
        Err(parser.diags)
    }
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    line: usize,
    col: usize,
    root: BTreeMap<String, TomlValue>,
    cur_table: Vec<String>,
    diags: Vec<TomlDiag>,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn err(&mut self, message: impl Into<String>, suggestion: Option<&str>) {
        self.diags.push(TomlDiag {
            line: self.line,
            col: self.col,
            message: message.into(),
            suggestion: suggestion.map(String::from),
        });
    }

    fn skip_ws_comments(&mut self) {
        loop {
            while matches!(self.peek(), Some(' ') | Some('\t') | Some('\r') | Some('\n')) {
                self.bump();
            }
            if self.peek() == Some('#') {
                while let Some(c) = self.peek() {
                    if c == '\n' {
                        break;
                    }
                    self.bump();
                }
            } else {
                break;
            }
        }
    }

    fn parse_document(&mut self) {
        loop {
            self.skip_ws_comments();
            if self.peek().is_none() {
                break;
            }
            // Headers may follow headers: an empty `[server]` section before
            // `[database]` is valid TOML, and treating the second header as a
            // key would report a key error on a line that has no key.
            while self.try_parse_table_header() {
                self.skip_ws_comments();
                if self.peek().is_none() {
                    break;
                }
            }
            if self.peek().is_none() {
                break;
            }
            self.parse_key_value();
        }
    }

    /// Parse `[a.b]` / `[[a.b]]` headers. Returns true when a header was
    /// consumed (even if its key was invalid).
    fn try_parse_table_header(&mut self) -> bool {
        if self.peek() != Some('[') {
            return false;
        }
        self.bump();
        let mut is_array = false;
        if self.peek() == Some('[') {
            self.bump();
            is_array = true;
        }
        let mut key = String::new();
        let start_line = self.line;
        let start_col = self.col;
        let mut segs = Vec::new();
        loop {
            self.skip_ws_comments();
            match self.peek() {
                Some(']') => break,
                Some('.') => {
                    self.bump();
                    if key.is_empty() {
                        self.err(
                            "expected a table name before '.' in the table header",
                            Some("write [package] instead of [.package]"),
                        );
                    } else {
                        segs.push(key.clone());
                        key.clear();
                    }
                }
                Some(c) => {
                    self.bump();
                    if c == ' ' || c == '\t' {
                        self.err(
                            "spaces are not allowed inside a table header",
                            Some("remove the space, e.g. [dependencies]"),
                        );
                    } else if c == '\n' {
                        self.err(
                            "unterminated table header",
                            Some("close the header with ']'"),
                        );
                    } else {
                        key.push(c);
                    }
                }
                None => {
                    self.err("unterminated table header", Some("close the header with ']'"));
                    self.cur_table = segs;
                    return true;
                }
            }
        }
        self.bump();
        if !key.is_empty() {
            segs.push(key);
        }
        if segs.is_empty() {
            self.err("empty table header", Some("write a name, e.g. [compiler]"));
        }
        self.cur_table = segs;
        if is_array {
            self.cur_table.push("__arr__".to_string());
        }
        let _ = (start_line, start_col);
        true
    }

    fn parse_key_value(&mut self) {
        let key = self.parse_key();
        self.skip_inline_ws();
        if !self.eat('=') {
            self.err("expected '=' after key", Some("write key = value"));
            self.skip_to_newline();
            return;
        }
        self.skip_inline_ws();
        let value = self.parse_value();
        if let Some(value) = value {
            self.assign(&key, value);
        }
        self.skip_to_newline();
    }

    fn parse_key(&mut self) -> Vec<String> {
        let mut segs = Vec::new();
        loop {
            self.skip_inline_ws();
            let mut seg = String::new();
            match self.peek() {
                Some('"') => {
                    if let Some(s) = self.parse_basic_string() {
                        seg = s;
                    } else {
                        break;
                    }
                }
                Some('\'') => {
                    if let Some(s) = self.parse_literal_string() {
                        seg = s;
                    } else {
                        seg.clear();
                    }
                }
                Some(c) if is_bare_key_char(c) => {
                    while let Some(c) = self.peek() {
                        if is_bare_key_char(c) {
                            seg.push(c);
                            self.bump();
                        } else {
                            break;
                        }
                    }
                }
                _ => {
                    self.err("expected a key", Some("write name = \"value\""));
                    return segs;
                }
            }
            if seg.is_empty() && !matches!(self.peek(), Some('=')) {
                self.err("empty key", Some("keys must not be empty"));
            }
            segs.push(seg);
            self.skip_inline_ws();
            if self.peek() == Some('.') {
                self.bump();
                continue;
            }
            break;
        }
        segs
    }

    fn skip_inline_ws(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t') | Some('\r')) {
            self.bump();
        }
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn skip_to_newline(&mut self) {
        while let Some(c) = self.peek() {
            if c == '\n' {
                return;
            }
            self.bump();
        }
    }

    fn assign(&mut self, key: &[String], value: TomlValue) {
        let mut table = &mut self.root;
        for seg in self.cur_table.iter() {
            table = table
                .entry(seg.clone())
                .or_insert_with(|| TomlValue::Table(BTreeMap::new()))
                .as_table_mut()
                .unwrap();
        }
        if key.is_empty() {
            return;
        }
        if key.len() == 1 {
            // allow dotted top-level key a.b = v to nest
            let segments = split_dotted(&key[0]);
            insert_nested(table, &segments, value);
            return;
        }
        insert_nested(table, key, value);
    }

    fn parse_value(&mut self) -> Option<TomlValue> {
        self.skip_inline_ws();
        match self.peek() {
            Some('"') => self.parse_basic_string().map(TomlValue::Str),
            Some('\'') => self.parse_literal_string().map(TomlValue::Str),
            Some('[') => self.parse_array().map(TomlValue::Array),
            Some('{') => self.parse_inline_table().map(TomlValue::Table),
            Some('t') | Some('f') => self.parse_bool(),
            Some('+') | Some('-') | Some('0'..='9') => self.parse_number(),
            _ => {
                self.err("expected a value", Some("use a string, number or array"));
                None
            }
        }
    }

    fn parse_basic_string(&mut self) -> Option<String> {
        self.bump(); // "
        let mut out = String::new();
        loop {
            match self.peek() {
                Some('"') => {
                    self.bump();
                    return Some(out);
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
                        Some('b') => out.push('\u{0008}'),
                        Some('f') => out.push('\u{000c}'),
                        Some('/') => out.push('/'),
                        Some('u') => {
                            let mut hex = String::new();
                            for _ in 0..4 {
                                match self.bump() {
                                    Some(c) if c.is_ascii_hexdigit() => hex.push(c),
                                    _ => {
                                        self.err("invalid \\u escape", Some("use 4 hex digits"));
                                        break;
                                    }
                                }
                            }
                            if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                                out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                            }
                        }
                        Some('U') => {
                            let mut hex = String::new();
                            for _ in 0..8 {
                                match self.bump() {
                                    Some(c) if c.is_ascii_hexdigit() => hex.push(c),
                                    _ => {
                                        self.err("invalid \\U escape", Some("use 8 hex digits"));
                                        break;
                                    }
                                }
                            }
                            if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                                out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                            }
                        }
                        Some('\n') => {
                            // multiline basic strings end with escaped newline
                        }
                        _ => self.err("unknown escape in string", Some("use \\n \\t \\\" \\\\")),
                    }
                }
                Some(c) => {
                    self.bump();
                    out.push(c);
                }
                None => {
                    self.err("unterminated string", Some("close the string with \""));
                    return None;
                }
            }
        }
    }

    fn parse_literal_string(&mut self) -> Option<String> {
        self.bump(); // '
        let mut out = String::new();
        loop {
            match self.peek() {
                Some('\'') => {
                    self.bump();
                    return Some(out);
                }
                Some(c) => {
                    self.bump();
                    out.push(c);
                }
                None => {
                    self.err("unterminated literal string", Some("close the string with '"));
                    return None;
                }
            }
        }
    }

    fn parse_bool(&mut self) -> Option<TomlValue> {
        let mut word = String::new();
        for _ in 0..5 {
            match self.peek() {
                Some(c) if c.is_ascii_alphabetic() => {
                    word.push(c);
                    self.bump();
                }
                _ => break,
            }
        }
        match word.as_str() {
            "true" => Some(TomlValue::Bool(true)),
            "false" => Some(TomlValue::Bool(false)),
            _ => {
                self.err(
                    format!("expected a boolean or string, found '{word}'"),
                    Some("write true or false"),
                );
                None
            }
        }
    }

    fn parse_number(&mut self) -> Option<TomlValue> {
        let start = self.pos;
        let mut is_float = false;
        if matches!(self.peek(), Some('+') | Some('-')) {
            self.bump();
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.bump();
            } else if c == '.' {
                is_float = true;
                self.bump();
            } else if c == '_' {
                self.bump();
            } else {
                break;
            }
        }
        if matches!(self.peek(), Some('e') | Some('E')) {
            is_float = true;
            self.bump();
            if matches!(self.peek(), Some('+') | Some('-')) {
                self.bump();
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.bump();
            }
        }
        let text = &self.src[start..self.pos];
        // date/time literals: 2027-01-01 or 12:30:00
        if self.peek() == Some('T') || self.peek() == Some(':') || (self.peek() == Some('-') && !is_float_chars(text)) {
            while let Some(c) = self.peek() {
                if c == '\n' || c == '#' || c == ',' || c == ']' || c == '}' {
                    break;
                }
                self.bump();
            }
            return Some(TomlValue::DateTime(self.src[start..self.pos].trim().to_string()));
        }
        if is_float {
            text.replace('_', "").parse::<f64>().ok().map(TomlValue::Float)
        } else {
            text.replace('_', "")
                .parse::<i64>()
                .map(TomlValue::Int)
                .ok()
        }
    }

    fn parse_array(&mut self) -> Option<Vec<TomlValue>> {
        self.bump(); // [
        let mut items = Vec::new();
        loop {
            self.skip_ws_comments();
            if self.peek() == Some(']') {
                self.bump();
                return Some(items);
            }
            if self.peek().is_none() {
                self.err("unterminated array", Some("close the array with ]"));
                return None;
            }
            match self.parse_value() {
                Some(v) => items.push(v),
                None => break,
            }
            self.skip_ws_comments();
            if self.peek() == Some(',') {
                self.bump();
            } else if self.peek() != Some(']') {
                self.err("expected ',' or ']' in array", Some("add a comma between items"));
                self.skip_to_newline();
                return None;
            }
        }
        None
    }

    fn parse_inline_table(&mut self) -> Option<BTreeMap<String, TomlValue>> {
        self.bump(); // {
        let mut table = BTreeMap::new();
        loop {
            self.skip_ws_comments();
            if self.peek() == Some('}') {
                self.bump();
                return Some(table);
            }
            if self.peek().is_none() {
                self.err("unterminated inline table", Some("close it with }"));
                return None;
            }
            let key = self.parse_key();
            self.skip_ws_comments();
            if !self.eat('=') {
                self.err("expected '=' in inline table", Some("write key = value"));
                self.skip_to_newline();
                return None;
            }
            self.skip_ws_comments();
            if let Some(v) = self.parse_value() {
                if key.len() == 1 {
                    for seg in split_dotted(&key[0]) {
                        table.insert(seg, v.clone());
                    }
                } else {
                    let mut t = BTreeMap::new();
                    insert_nested(&mut t, &key, v);
                    for (k, vv) in t {
                        table.insert(k, vv);
                    }
                }
            }
            self.skip_ws_comments();
            if self.peek() == Some(',') {
                self.bump();
            } else if self.peek() != Some('}') {
                self.err("expected ',' or '}' in inline table", Some("add a comma"));
                self.skip_to_newline();
                return None;
            }
        }
    }
}

/// A dotted a.b = v assignment nests sub-tables.
fn split_dotted(key: &str) -> Vec<String> {
    if key.contains('.') {
        key.split('.').map(|s| s.to_string()).collect()
    } else {
        vec![key.to_string()]
    }
}

fn insert_nested(table: &mut BTreeMap<String, TomlValue>, segments: &[String], value: TomlValue) {
    match segments.split_first() {
        None => {}
        Some((head, rest)) => {
            if rest.is_empty() {
                table.insert(head.clone(), value);
            } else {
                let child = table
                    .entry(head.clone())
                    .or_insert_with(|| TomlValue::Table(BTreeMap::new()));
                if let TomlValue::Table(t) = child {
                    insert_nested(t, rest, value);
                }
            }
        }
    }
}

fn is_bare_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn is_float_chars(text: &str) -> bool {
    text.contains('.')
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_table_headers_nest() {
        let doc = parse("[package.lib]\nversion = \"2.4.0\"\n").unwrap();
        let pkg = doc.table("package").expect("package table");
        let lib = pkg.get("lib").and_then(TomlValue::as_table).expect("lib table");
        assert_eq!(lib.get("version").and_then(TomlValue::as_str), Some("2.4.0"));
    }

    #[test]
    fn three_level_headers_nest() {
        let doc = parse("[a.b.c]\nx = 1\n").unwrap();
        let b = doc.table("a").unwrap().get("b").and_then(TomlValue::as_table).unwrap();
        let c = b.get("c").and_then(TomlValue::as_table).unwrap();
        assert_eq!(c.get("x").and_then(TomlValue::as_int), Some(1));
    }

    #[test]
    fn sibling_tables_do_not_leak_into_each_other() {
        let doc = parse("[package.lib]\nversion = \"1.0.0\"\n\n[package.mini]\nversion = \"0.3.1\"\n").unwrap();
        let pkg = doc.table("package").unwrap();
        let lib = pkg.get("lib").and_then(TomlValue::as_table).unwrap();
        assert_eq!(lib.get("version").and_then(TomlValue::as_str), Some("1.0.0"));
        assert!(lib.get("mini").is_none());
        let mini = pkg.get("mini").and_then(TomlValue::as_table).unwrap();
        assert_eq!(mini.get("version").and_then(TomlValue::as_str), Some("0.3.1"));
    }

    #[test]
    fn top_level_dotted_key_nests() {
        let doc = parse("a.b.c = 1\n").unwrap();
        let b = doc.table("a").unwrap().get("b").and_then(TomlValue::as_table).unwrap();
        assert_eq!(b.get("c").and_then(TomlValue::as_int), Some(1));
    }

    #[test]
    fn strings_numbers_arrays_bools() {
        let doc = parse("s = \"hi\"\nn = 42\na = [1, \"two\", 3]\nt = true\nfals = false\n")
            .unwrap();
        assert_eq!(doc.get("s").and_then(TomlValue::as_str), Some("hi"));
        assert_eq!(doc.get("n").and_then(TomlValue::as_int), Some(42));
        assert!(doc.get("t").and_then(TomlValue::as_bool) == Some(true));
        assert!(doc.get("fals").and_then(TomlValue::as_bool) == Some(false));
        let arr = doc.get("a").and_then(TomlValue::as_array).unwrap();
        assert_eq!(arr.len(), 3);
    }

    #[test]
    fn recoverable_syntax_error_reports_line_and_col() {
        let errs = parse("a = 1\n\n[broken\n").unwrap_err();
        assert!(!errs.is_empty());
        assert!(
            errs.iter().any(|d| d.message.contains("unterminated table header")),
            "diags: {:?}",
            errs
        );
    }
}
