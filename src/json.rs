//! Minimal JSON: parse, build, serialize. Keeps object key order and number text as written,
//! so editing someone's settings.json changes only what we touch.
use std::fmt::Write;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn get(&self, k: &str) -> Option<&Json> {
        match self {
            Json::Obj(v) => v.iter().find(|e| e.0 == k).map(|e| &e.1),
            _ => None,
        }
    }
    /// Walks a path of object keys.
    pub fn at(&self, path: &[&str]) -> Option<&Json> {
        path.iter().try_fold(self, |j, k| j.get(k))
    }
    pub fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn arr(&self) -> &[Json] {
        match self {
            Json::Arr(a) => a,
            _ => &[],
        }
    }
    /// Object entry, created (as `Obj`) if missing. Panics if self isn't an object.
    pub fn entry(&mut self, k: &str, default: Json) -> &mut Json {
        let Json::Obj(v) = self else { panic!("not an object") };
        let i = match v.iter().position(|e| e.0 == k) {
            Some(i) => i,
            None => {
                v.push((k.to_string(), default));
                v.len() - 1
            }
        };
        &mut v[i].1
    }
    pub fn remove(&mut self, k: &str) {
        if let Json::Obj(v) = self {
            v.retain(|e| e.0 != k);
        }
    }
    pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
        Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }
    pub fn s(v: &str) -> Json {
        Json::Str(v.to_string())
    }
    pub fn n(v: i64) -> Json {
        Json::Num(v.to_string())
    }

    pub fn parse(s: &str) -> Option<Json> {
        let mut p = P { b: s.as_bytes(), i: 0, depth: 0 };
        let v = p.value()?;
        p.ws();
        (p.i == p.b.len()).then_some(v)
    }

    pub fn to_string(&self) -> String {
        let mut o = String::new();
        self.write(&mut o, None, 0);
        o
    }
    pub fn pretty(&self) -> String {
        let mut o = String::new();
        self.write(&mut o, Some(2), 0);
        o.push('\n');
        o
    }

    fn write(&self, o: &mut String, indent: Option<usize>, lvl: usize) {
        let nl = |o: &mut String, l: usize| {
            if let Some(n) = indent {
                o.push('\n');
                o.extend(std::iter::repeat_n(' ', n * l));
            }
        };
        match self {
            Json::Null => o.push_str("null"),
            Json::Bool(b) => o.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) => o.push_str(n),
            Json::Str(s) => quote(o, s),
            Json::Arr(a) if a.is_empty() => o.push_str("[]"),
            Json::Obj(v) if v.is_empty() => o.push_str("{}"),
            Json::Arr(a) => {
                o.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        o.push(',');
                    }
                    nl(o, lvl + 1);
                    x.write(o, indent, lvl + 1);
                }
                nl(o, lvl);
                o.push(']');
            }
            Json::Obj(v) => {
                o.push('{');
                for (i, (k, x)) in v.iter().enumerate() {
                    if i > 0 {
                        o.push(',');
                    }
                    nl(o, lvl + 1);
                    quote(o, k);
                    o.push(':');
                    if indent.is_some() {
                        o.push(' ');
                    }
                    x.write(o, indent, lvl + 1);
                }
                nl(o, lvl);
                o.push('}');
            }
        }
    }
}

fn quote(o: &mut String, s: &str) {
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn eat(&mut self, c: u8) -> Option<()> {
        self.ws();
        (self.peek()? == c).then(|| self.i += 1)
    }
    fn lit(&mut self, s: &str, v: Json) -> Option<Json> {
        self.b[self.i..].starts_with(s.as_bytes()).then(|| {
            self.i += s.len();
            v
        })
    }
    fn value(&mut self) -> Option<Json> {
        self.ws();
        self.depth += 1;
        if self.depth > 128 {
            return None;
        }
        let v = match self.peek()? {
            b'n' => self.lit("null", Json::Null),
            b't' => self.lit("true", Json::Bool(true)),
            b'f' => self.lit("false", Json::Bool(false)),
            b'"' => self.string().map(Json::Str),
            b'[' => {
                self.i += 1;
                let mut a = vec![];
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                } else {
                    loop {
                        a.push(self.value()?);
                        self.ws();
                        match self.peek()? {
                            b',' => self.i += 1,
                            b']' => {
                                self.i += 1;
                                break;
                            }
                            _ => return None,
                        }
                    }
                }
                Some(Json::Arr(a))
            }
            b'{' => {
                self.i += 1;
                let mut v = vec![];
                self.ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                } else {
                    loop {
                        self.ws();
                        let k = self.string()?;
                        self.eat(b':')?;
                        v.push((k, self.value()?));
                        self.ws();
                        match self.peek()? {
                            b',' => self.i += 1,
                            b'}' => {
                                self.i += 1;
                                break;
                            }
                            _ => return None,
                        }
                    }
                }
                Some(Json::Obj(v))
            }
            b'-' | b'0'..=b'9' => {
                let st = self.i;
                while self.i < self.b.len() && matches!(self.b[self.i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                    self.i += 1;
                }
                let t = std::str::from_utf8(&self.b[st..self.i]).ok()?;
                t.parse::<f64>().ok()?;
                Some(Json::Num(t.to_string()))
            }
            _ => None,
        };
        self.depth -= 1;
        v
    }
    fn hex4(&mut self) -> Option<u32> {
        let h = std::str::from_utf8(self.b.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u32::from_str_radix(h, 16).ok()
    }
    fn string(&mut self) -> Option<String> {
        if self.peek()? != b'"' {
            return None;
        }
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let c = self.peek()?;
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let e = self.peek()?;
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let mut u = self.hex4()?;
                            if (0xd800..0xdc00).contains(&u) {
                                if self.b.get(self.i..self.i + 2)? != b"\\u" {
                                    return None;
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                u = 0x10000 + ((u - 0xd800) << 10) + (lo.checked_sub(0xdc00)? & 0x3ff);
                            }
                            char::from_u32(u)?
                        }
                        _ => return None,
                    };
                    let mut buf = [0; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                c if c < 0x20 => return None,
                c => out.push(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let src = r#"{"a": [1, 2.50, -3e2, true, null], "b": {"c": "x\"y\\né😀"}, "z": {}}"#;
        let j = Json::parse(src).unwrap();
        assert_eq!(j.at(&["b", "c"]).unwrap().str(), Some("x\"y\\né😀"));
        assert_eq!(Json::parse(&j.pretty()).unwrap(), j);
        assert_eq!(Json::parse(&j.to_string()).unwrap(), j);
        assert!(j.to_string().contains("2.50"));
        for bad in ["", "{", "[1,]", "{\"a\" 1}", "\"\u{1}\"", "01x", "nul"] {
            assert!(Json::parse(bad).is_none(), "{bad}");
        }
        assert!(Json::parse(&"[".repeat(1000)).is_none());
    }
}
