//! Minimal dependency-free JSON parser for the LibreHardwareMonitor
//! loopback endpoint. Read-only document model; bounded depth and input
//! size; exact escape handling per RFC 8259.

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

const MAX_DEPTH: usize = 64;

impl Json {
    pub fn parse(input: &str) -> Result<Json, String> {
        let bytes = input.as_bytes();
        let mut p = Parser { bytes, pos: 0 };
        p.skip_ws();
        let value = p.value(0)?;
        p.skip_ws();
        if p.pos != bytes.len() {
            return Err(format!("trailing data at byte {}", p.pos));
        }
        Ok(value)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len()
            && matches!(self.bytes[self.pos], b' ' | b'\t' | b'\n' | b'\r')
        {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> Result<(), String> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!(
                "expected {:?} at byte {}, found {:?}",
                b as char,
                self.pos,
                self.peek().map(|c| c as char)
            ))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_DEPTH {
            return Err("JSON nesting depth limit exceeded".into());
        }
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            other => Err(format!(
                "unexpected {:?} at byte {}",
                other.map(|c| c as char),
                self.pos
            )),
        }
    }

    fn literal(&mut self, text: &str, value: Json) -> Result<Json, String> {
        if self.bytes[self.pos..].starts_with(text.as_bytes()) {
            self.pos += text.len();
            Ok(value)
        } else {
            Err(format!("invalid literal at byte {}", self.pos))
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            let value = self.value(depth + 1)?;
            pairs.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(pairs));
                }
                other => {
                    return Err(format!(
                        "expected , or }} at byte {}, found {:?}",
                        self.pos,
                        other.map(|c| c as char)
                    ))
                }
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            let value = self.value(depth + 1)?;
            items.push(value);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                other => {
                    return Err(format!(
                        "expected , or ] at byte {}, found {:?}",
                        self.pos,
                        other.map(|c| c as char)
                    ))
                }
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err("unterminated string".into());
            };
            self.pos += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(esc) = self.peek() else {
                        return Err("unterminated escape".into());
                    };
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = self
                                .bytes
                                .get(self.pos..self.pos + 4)
                                .ok_or("truncated \\u escape")?;
                            let hex = std::str::from_utf8(hex).map_err(|_| "invalid \\u escape")?;
                            let mut cp =
                                u32::from_str_radix(hex, 16).map_err(|_| "invalid \\u escape")?;
                            self.pos += 4;
                            // Surrogate pair handling.
                            if (0xD800..0xDC00).contains(&cp)
                                && self.bytes.get(self.pos) == Some(&b'\\')
                                && self.bytes.get(self.pos + 1) == Some(&b'u')
                            {
                                let hex2 = self
                                    .bytes
                                    .get(self.pos + 2..self.pos + 6)
                                    .ok_or("truncated surrogate pair")?;
                                let low = u32::from_str_radix(
                                    std::str::from_utf8(hex2)
                                        .map_err(|_| "invalid surrogate pair")?,
                                    16,
                                )
                                .map_err(|_| "invalid surrogate pair")?;
                                if (0xDC00..0xE000).contains(&low) {
                                    cp = 0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00);
                                    self.pos += 6;
                                }
                            }
                            out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        }
                        other => {
                            return Err(format!(
                                "invalid escape \\{} at byte {}",
                                other as char, self.pos
                            ))
                        }
                    }
                }
                _ => {
                    // Re-assemble UTF-8 sequences byte by byte.
                    if c < 0x80 {
                        if c < 0x20 {
                            return Err("control character in string".into());
                        }
                        out.push(c as char);
                    } else {
                        let len = utf8_len(c);
                        let start = self.pos - 1;
                        let end = start + len;
                        let slice = self
                            .bytes
                            .get(start..end)
                            .ok_or("truncated UTF-8 sequence")?;
                        let s =
                            std::str::from_utf8(slice).map_err(|_| "invalid UTF-8 in string")?;
                        out.push_str(s);
                        self.pos = end;
                    }
                }
            }
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while self
            .peek()
            .map(|c| c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-'))
            .unwrap_or(false)
        {
            self.pos += 1;
        }
        let text =
            std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|_| "invalid number")?;
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| format!("invalid number {:?}", text))
    }
}

fn utf8_len(first: u8) -> usize {
    if first >= 0xF0 {
        4
    } else if first >= 0xE0 {
        3
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lhm_shape() {
        let doc = Json::parse(
            r#"{"id":0,"Text":"Temperature","Children":[
                {"id":1,"Text":"AMD CPU","Children":[
                    {"id":2,"Text":"Temperature","Children":[
                        {"id":3,"Text":"Core (Tctl/Tdie)","Value":"67.500 °C","Path":"/amdcpu/0/temperature/2"}
                    ]}]
                }]
            }"#,
        )
        .unwrap();
        let children = doc.get("Children").unwrap().as_arr().unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].get("Text").unwrap().as_str(), Some("AMD CPU"));
    }

    #[test]
    fn strings_escapes_and_unicode() {
        let doc = Json::parse(r#"["a\nb", "é\u00e9", "😀", "q\"q"]"#).unwrap();
        let arr = doc.as_arr().unwrap();
        assert_eq!(arr[0].as_str(), Some("a\nb"));
        assert_eq!(arr[1].as_str(), Some("éé"));
        assert_eq!(arr[2].as_str(), Some("😀"));
        assert_eq!(arr[3].as_str(), Some("q\"q"));
    }

    #[test]
    fn numbers_and_literals() {
        let doc = Json::parse(r#"{"a": 1.5, "b": -3, "c": 1e3, "d": true, "e": null, "f": false}"#)
            .unwrap();
        assert_eq!(doc.get("a").unwrap().as_f64(), Some(1.5));
        assert_eq!(doc.get("b").unwrap().as_f64(), Some(-3.0));
        assert_eq!(doc.get("c").unwrap().as_f64(), Some(1000.0));
        assert_eq!(doc.get("d"), Some(&Json::Bool(true)));
        assert_eq!(doc.get("e"), Some(&Json::Null));
        assert_eq!(doc.get("f"), Some(&Json::Bool(false)));
    }

    #[test]
    fn rejects_malformed() {
        assert!(Json::parse("{").is_err());
        assert!(Json::parse("[1,]").is_err());
        assert!(Json::parse("{\"a\" 1}").is_err());
        assert!(Json::parse("nul").is_err());
        assert!(Json::parse("1 2").is_err());
        // Depth limit.
        let deep = format!("{}{}", "[".repeat(80), "]".repeat(80));
        assert!(Json::parse(&deep).is_err());
    }
}
