//! A tiny, dependency-free JSON value + serializer.
//!
//! Enough for the module graph, build cache metadata and the build manifest —
//! the project deliberately avoids a JSON crate. Output is deterministic:
//! object keys are serialized in insertion order.

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(i64),
    Float(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }
    pub fn num(n: i64) -> Json {
        Json::Num(n)
    }
    pub fn float(f: f64) -> Json {
        Json::Float(f)
    }
    pub fn arr(items: Vec<Json>) -> Json {
        Json::Arr(items)
    }
    pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
        Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_num(&self) -> Option<i64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn to_string(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) => out.push_str(&n.to_string()),
            Json::Float(f) => {
                if f.is_finite() {
                    out.push_str(&format!("{f}"));
                } else {
                    out.push_str("0.0");
                }
            }
            Json::Str(s) => write_str(s, out),
            Json::Arr(items) => {
                out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    it.write(out);
                }
                out.push(']');
            }
            Json::Obj(pairs) => {
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_str(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

fn write_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A minimal JSON parser used by cache/doctor diagnostics (round-tripping our
/// own output). Not a general-purpose parser: it only accepts the subset the
/// compiler emits.
pub fn parse(s: &str) -> Option<Json> {
    let mut p = P {
        bytes: s.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.value()?;
    p.skip_ws();
    if p.pos == p.bytes.len() {
        Some(v)
    } else {
        None
    }
}

struct P<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> P<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && (self.bytes[self.pos] as char).is_whitespace() {
            self.pos += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }
    fn value(&mut self) -> Option<Json> {
        match self.peek()? {
            b'{' => self.obj(),
            b'[' => self.arr(),
            b'"' => Some(Json::Str(self.string()?)),
            b't' => {
                self.eat(b"true")?;
                Some(Json::Bool(true))
            }
            b'f' => {
                self.eat(b"false")?;
                Some(Json::Bool(false))
            }
            b'n' => {
                self.eat(b"null")?;
                Some(Json::Null)
            }
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }
    fn eat(&mut self, lit: &[u8]) -> Option<()> {
        if self.bytes.get(self.pos..self.pos + lit.len())? == lit {
            self.pos += lit.len();
            Some(())
        } else {
            None
        }
    }
    fn string(&mut self) -> Option<String> {
        if self.peek()? != b'"' {
            return None;
        }
        self.pos += 1;
        let mut s = String::new();
        loop {
            let c = self.peek()?;
            self.pos += 1;
            match c {
                b'"' => break,
                b'\\' => match self.peek()? {
                    b'"' => {
                        s.push('"');
                        self.pos += 1;
                    }
                    b'\\' => {
                        s.push('\\');
                        self.pos += 1;
                    }
                    b'n' => {
                        s.push('\n');
                        self.pos += 1;
                    }
                    b'r' => {
                        s.push('\r');
                        self.pos += 1;
                    }
                    b't' => {
                        s.push('\t');
                        self.pos += 1;
                    }
                    b'u' => {
                        let hexv: String = self.bytes.get(self.pos..self.pos + 4)?.iter().map(|b| *b as char).collect();
                        self.pos += 4;
                        if let Ok(n) = u32::from_str_radix(&hexv, 16) {
                            if let Some(ch) = char::from_u32(n) {
                                s.push(ch);
                            }
                        }
                    }
                    _ => return None,
                },
                b => s.push(b as char),
            }
        }
        Some(s)
    }
    fn number(&mut self) -> Option<Json> {
        let start = self.pos;
        if self.peek()? == b'-' {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        let mut is_float = false;
        if matches!(self.peek(), Some(b'.')) {
            is_float = true;
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).ok()?;
        if is_float {
            text.parse::<f64>().ok().map(Json::Float)
        } else {
            text.parse::<i64>().ok().map(Json::Num)
        }
    }
    fn arr(&mut self) -> Option<Json> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        while self.peek()? != b']' {
            items.push(self.value()?);
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b']' => break,
                _ => return None,
            }
        }
        self.pos += 1;
        Some(Json::Arr(items))
    }
    fn obj(&mut self) -> Option<Json> {
        self.pos += 1;
        let mut pairs = Vec::new();
        self.skip_ws();
        while self.peek()? != b'}' {
            let k = self.string()?;
            self.skip_ws();
            if self.peek()? != b':' {
                return None;
            }
            self.pos += 1;
            self.skip_ws();
            let v = self.value()?;
            pairs.push((k, v));
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b'}' => break,
                _ => return None,
            }
        }
        self.pos += 1;
        Some(Json::Obj(pairs))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Json};

    #[test]
    fn round_trip_simple() {
        let v = Json::obj(vec![
            ("name", Json::str("util.hard")),
            ("count", Json::num(3)),
            ("ok", Json::Bool(true)),
            ("deps", Json::arr(vec![Json::num(1)])),
        ]);
        let s = v.to_string();
        assert_eq!(parse(&s), Some(v));
    }

    #[test]
    fn escapes_quotes_and_newlines() {
        let v = Json::obj(vec![("a", Json::str("say \"hi\"\nbye"))]);
        let s = v.to_string();
        assert!(s.contains("\\\""));
        assert!(s.contains("\\n"));
        assert_eq!(parse(&s), Some(v));
    }

    #[test]
    fn get_nested() {
        let v = Json::obj(vec![(
            "mods",
            Json::arr(vec![Json::obj(vec![("id", Json::num(7))])]),
        )]);
        let first = match v.get("mods") {
            Some(Json::Arr(items)) => items.first().cloned(),
            _ => None,
        };
        assert_eq!(first.as_ref().and_then(|m| m.get("id")), Some(&Json::num(7)));
    }
}