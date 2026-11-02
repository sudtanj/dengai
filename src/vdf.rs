//! Minimal Valve KeyValues readers: text (PICS app info) and binary (PICS package info, receipts).

/// A KeyValues node.
#[derive(Debug, Clone, PartialEq)]
pub enum Kv {
    Str(String),
    Int(i64),
    Map(Vec<(String, Kv)>),
}

impl Kv {
    /// Child by key (case-insensitive, like Steam).
    pub fn get(&self, key: &str) -> Option<&Kv> {
        self.children().iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
    }

    /// Descendant by key path.
    pub fn at(&self, path: &[&str]) -> Option<&Kv> {
        path.iter().try_fold(self, |kv, key| kv.get(key))
    }

    pub fn children(&self) -> &[(String, Kv)] {
        match self {
            Kv::Map(m) => m,
            _ => &[],
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Kv::Int(n) => u64::try_from(*n).ok(),
            Kv::Str(s) => s.trim().parse().ok(),
            Kv::Map(_) => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Kv::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Number at a path, 0 when missing or not a number.
    pub fn u64_at(&self, path: &[&str]) -> u64 {
        self.at(path).and_then(Kv::as_u64).unwrap_or(0)
    }

    pub fn str_at(&self, path: &[&str]) -> Option<&str> {
        self.at(path).and_then(Kv::as_str)
    }

    /// Boolean at a path: a non-zero number or "true".
    pub fn bool_at(&self, path: &[&str]) -> bool {
        match self.at(path) {
            Some(Kv::Str(s)) => s.eq_ignore_ascii_case("true") || s.trim().parse::<i64>().is_ok_and(|n| n != 0),
            Some(Kv::Int(n)) => *n != 0,
            _ => false,
        }
    }
}

/// Parses text KeyValues. The result is a map of the top-level pairs.
pub fn parse_text(s: &str) -> Option<Kv> {
    let mut p = Text { b: s.as_bytes(), i: 0 };
    p.map(false).map(Kv::Map)
}

enum Tok {
    Open,
    Close,
    Str(String),
}

struct Text<'a> {
    b: &'a [u8],
    i: usize,
}

impl Text<'_> {
    fn next(&mut self) -> Option<Tok> {
        loop {
            while self.b.get(self.i).is_some_and(|c| c.is_ascii_whitespace() || *c == 0) {
                self.i += 1;
            }
            if self.b[self.i..].starts_with(b"//") {
                while self.b.get(self.i).is_some_and(|c| *c != b'\n') {
                    self.i += 1;
                }
                continue;
            }
            break;
        }
        let c = *self.b.get(self.i)?;
        self.i += 1;
        match c {
            b'{' => Some(Tok::Open),
            b'}' => Some(Tok::Close),
            b'"' => {
                let mut out = Vec::new();
                loop {
                    let c = *self.b.get(self.i)?;
                    self.i += 1;
                    match c {
                        b'"' => return Some(Tok::Str(String::from_utf8_lossy(&out).into_owned())),
                        b'\\' => {
                            let e = *self.b.get(self.i)?;
                            self.i += 1;
                            out.push(match e {
                                b'n' => b'\n',
                                b't' => b'\t',
                                other => other,
                            });
                        }
                        _ => out.push(c),
                    }
                }
            }
            _ => {
                let start = self.i - 1;
                while self.b.get(self.i).is_some_and(|c| !c.is_ascii_whitespace() && !b"{}\"".contains(c)) {
                    self.i += 1;
                }
                Some(Tok::Str(String::from_utf8_lossy(&self.b[start..self.i]).into_owned()))
            }
        }
    }

    fn map(&mut self, nested: bool) -> Option<Vec<(String, Kv)>> {
        let mut out = Vec::new();
        loop {
            match self.next() {
                None => return (!nested).then_some(out),
                Some(Tok::Close) => return nested.then_some(out),
                Some(Tok::Open) => return None,
                Some(Tok::Str(key)) => {
                    let value = match self.next()? {
                        Tok::Str(s) => Kv::Str(s),
                        Tok::Open => Kv::Map(self.map(true)?),
                        Tok::Close => return None,
                    };
                    out.push((key, value));
                }
            }
        }
    }
}

/// Parses binary KeyValues. The result is a map of the top-level pairs.
pub fn parse_binary(b: &[u8]) -> Option<Kv> {
    let mut i = 0;
    bin_map(b, &mut i, true).map(Kv::Map)
}

fn take<'a>(b: &'a [u8], i: &mut usize, n: usize) -> Option<&'a [u8]> {
    let s = b.get(*i..*i + n)?;
    *i += n;
    Some(s)
}

fn cstr(b: &[u8], i: &mut usize) -> Option<String> {
    let len = b.get(*i..)?.iter().position(|c| *c == 0)?;
    let s = String::from_utf8_lossy(&b[*i..*i + len]).into_owned();
    *i += len + 1;
    Some(s)
}

fn bin_map(b: &[u8], i: &mut usize, top: bool) -> Option<Vec<(String, Kv)>> {
    let mut out = Vec::new();
    loop {
        let Some(&t) = b.get(*i) else { return top.then_some(out) };
        *i += 1;
        if t == 8 || t == 11 {
            return Some(out);
        }
        let name = cstr(b, i)?;
        let value = match t {
            0 => Kv::Map(bin_map(b, i, false)?),
            1 => Kv::Str(cstr(b, i)?),
            2 | 4 | 6 => Kv::Int(i32::from_le_bytes(take(b, i, 4)?.try_into().ok()?) as i64),
            3 => Kv::Str(f32::from_le_bytes(take(b, i, 4)?.try_into().ok()?).to_string()),
            5 => {
                let mut units = Vec::new();
                loop {
                    let u = u16::from_le_bytes(take(b, i, 2)?.try_into().ok()?);
                    if u == 0 {
                        break;
                    }
                    units.push(u);
                }
                Kv::Str(String::from_utf16_lossy(&units))
            }
            7 | 10 => Kv::Int(i64::from_le_bytes(take(b, i, 8)?.try_into().ok()?)),
            _ => return None,
        };
        out.push((name, value));
    }
}
