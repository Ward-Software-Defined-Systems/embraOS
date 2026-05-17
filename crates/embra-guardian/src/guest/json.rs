// Vendored by embra-guardian — a zero-dependency JSON value type for
// `#![no_std]` + `alloc` Guardian tool guests. NOT compiled as part of
// embra-guardian; `include_str!`'d verbatim into each generated tool's
// `src/json.rs`. `serde` is banned in v1 (compile-time build.rs/proc-macro
// RCE surface), so this is the ergonomic JSON the intelligence uses inside
// `fn run`.

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

static NULL: Json = Json::Null;

impl Json {
    /// Object field access. Returns `&Null` for non-objects / missing keys
    /// so `v.get("a").get("b").as_str()` chains never panic.
    pub fn get(&self, key: &str) -> &Json {
        if let Json::Obj(m) = self {
            for (k, v) in m {
                if k == key {
                    return v;
                }
            }
        }
        &NULL
    }
    /// Array index, `&Null` if out of range / not an array.
    pub fn idx(&self, i: usize) -> &Json {
        if let Json::Arr(a) = self {
            if let Some(v) = a.get(i) {
                return v;
            }
        }
        &NULL
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }
}

// ---- builders ----
pub fn s(v: &str) -> Json {
    Json::Str(v.to_owned())
}
pub fn n(v: f64) -> Json {
    Json::Num(v)
}
pub fn b(v: bool) -> Json {
    Json::Bool(v)
}
pub fn null() -> Json {
    Json::Null
}
pub fn arr(items: Vec<Json>) -> Json {
    Json::Arr(items)
}
pub fn obj(pairs: Vec<(&str, Json)>) -> Json {
    Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect())
}

// ---- parser (recursive descent over bytes) ----
pub fn parse(input: &str) -> Result<Json, String> {
    let mut p = P { b: input.as_bytes(), i: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("trailing data at byte {}", p.i));
    }
    Ok(v)
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while let Some(&c) = self.b.get(self.i) {
            if c == b' ' || c == b'\t' || c == b'\n' || c == b'\r' {
                self.i += 1;
            } else {
                break;
            }
        }
    }
    fn value(&mut self) -> Result<Json, String> {
        match self.b.get(self.i) {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.lit("true", Json::Bool(true)),
            Some(b'f') => self.lit("false", Json::Bool(false)),
            Some(b'n') => self.lit("null", Json::Null),
            Some(&c) if c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(format!("unexpected token at byte {}", self.i)),
        }
    }
    fn lit(&mut self, word: &str, v: Json) -> Result<Json, String> {
        if self.b[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(format!("invalid literal at byte {}", self.i))
        }
    }
    fn number(&mut self) -> Result<Json, String> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        while let Some(&c) = self.b.get(self.i) {
            if c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' || c == b'+' || c == b'-' {
                self.i += 1;
            } else {
                break;
            }
        }
        let raw = core::str::from_utf8(&self.b[start..self.i]).map_err(|_| "bad number")?;
        raw.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("invalid number `{raw}`"))
    }
    fn string(&mut self) -> Result<String, String> {
        self.i += 1; // opening quote
        let mut out = String::new();
        loop {
            let c = *self.b.get(self.i).ok_or("unterminated string")?;
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or("bad escape")?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let cp = self.hex4()?;
                            if (0xD800..=0xDBFF).contains(&cp) {
                                // high surrogate; expect \uDC00..DFFF
                                if self.b.get(self.i) == Some(&b'\\')
                                    && self.b.get(self.i + 1) == Some(&b'u')
                                {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    let c = 0x10000
                                        + ((cp - 0xD800) << 10)
                                        + (lo - 0xDC00);
                                    out.push(
                                        char::from_u32(c).ok_or("bad surrogate pair")?,
                                    );
                                } else {
                                    return Err("lone high surrogate".into());
                                }
                            } else {
                                out.push(char::from_u32(cp).ok_or("bad \\u escape")?);
                            }
                        }
                        _ => return Err("invalid escape".into()),
                    }
                }
                _ => {
                    // collect a UTF-8 byte run up to the next " or \
                    let start = self.i - 1;
                    while let Some(&n) = self.b.get(self.i) {
                        if n == b'"' || n == b'\\' {
                            break;
                        }
                        self.i += 1;
                    }
                    out.push_str(
                        core::str::from_utf8(&self.b[start..self.i])
                            .map_err(|_| "invalid utf-8 in string")?,
                    );
                }
            }
        }
    }
    fn hex4(&mut self) -> Result<u32, String> {
        let s = self.b.get(self.i..self.i + 4).ok_or("short \\u")?;
        self.i += 4;
        let s = core::str::from_utf8(s).map_err(|_| "bad \\u")?;
        u32::from_str_radix(s, 16).map_err(|_| "bad \\u hex".into())
    }
    fn array(&mut self) -> Result<Json, String> {
        self.i += 1;
        let mut out = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Json::Arr(out));
        }
        loop {
            self.ws();
            out.push(self.value()?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(out));
                }
                _ => return Err(format!("expected ',' or ']' at byte {}", self.i)),
            }
        }
    }
    fn object(&mut self) -> Result<Json, String> {
        self.i += 1;
        let mut out: Vec<(String, Json)> = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Json::Obj(out));
        }
        loop {
            self.ws();
            if self.b.get(self.i) != Some(&b'"') {
                return Err(format!("expected string key at byte {}", self.i));
            }
            let k = self.string()?;
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return Err(format!("expected ':' at byte {}", self.i));
            }
            self.i += 1;
            self.ws();
            let v = self.value()?;
            out.push((k, v));
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(out));
                }
                _ => return Err(format!("expected ',' or '}}' at byte {}", self.i)),
            }
        }
    }
}

// ---- serializer ----
pub fn stringify(v: &Json) -> String {
    let mut s = String::new();
    write(&mut s, v);
    s
}

fn write(out: &mut String, v: &Json) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Num(n) => {
            // Rust's f64 `Display` already prints whole numbers without a
            // trailing `.0` (42.0 -> "42"); avoid `fract()`/`abs()` which
            // are std/libm-only and unavailable in `#![no_std]` guests.
            if n.is_finite() {
                out.push_str(&format!("{n}"));
            } else {
                out.push_str("null");
            }
        }
        Json::Str(s) => write_str(out, s),
        Json::Arr(a) => {
            out.push('[');
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(out, e);
            }
            out.push(']');
        }
        Json::Obj(m) => {
            out.push('{');
            for (i, (k, val)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(out, k);
                out.push(':');
                write(out, val);
            }
            out.push('}');
        }
    }
}

fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
