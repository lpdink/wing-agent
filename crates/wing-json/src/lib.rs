//! wing-json — single-pass partial JSON parser for LLM streaming.
//!
//! Parses potentially incomplete JSON (unterminated strings, unclosed
//! brackets) in a single pass, returning the best-effort result.
//! Exposed to Python via PyO3 (abi3).

use serde_json::Value;

// ── Parser ─────────────────────────────────────────────────────

/// Maximum nesting depth to prevent stack overflow on adversarial input.
const MAX_DEPTH: usize = 128;

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            bytes: input.as_bytes(),
            pos: 0,
            depth: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let b = self.bytes.get(self.pos).copied();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn skip_whitespace(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Parse any JSON value. Returns None only if input is exhausted
    /// before any meaningful token starts.
    fn parse_value(&mut self) -> Option<Value> {
        self.skip_whitespace();
        match self.peek()? {
            b'"' => Some(Value::String(self.parse_string())),
            b'{' => Some(self.parse_object()),
            b'[' => Some(self.parse_array()),
            b't' => self.parse_literal(b"true", Value::Bool(true)),
            b'f' => self.parse_literal(b"false", Value::Bool(false)),
            b'n' => self.parse_literal(b"null", Value::Null),
            b'-' | b'0'..=b'9' => Some(self.parse_number()),
            _ => None,
        }
    }

    /// Parse a JSON string. Handles:
    /// - standard escapes (\", \\, \/, \b, \f, \n, \r, \t, \uXXXX)
    /// - invalid escapes (kept as literal backslash + char)
    /// - raw control characters (kept as-is in the Rust String)
    /// - unterminated strings at EOF (returns what we have)
    fn parse_string(&mut self) -> String {
        self.pos += 1; // skip opening '"'
        let mut out: Vec<u8> = Vec::new();

        while let Some(b) = self.advance() {
            match b {
                b'"' => break,
                b'\\' => {
                    let Some(esc) = self.advance() else {
                        break; // trailing backslash at EOF
                    };
                    match esc {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0C),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            if let Some(cp) = self.try_parse_hex4() {
                                // Handle UTF-16 surrogate pairs (e.g. \uD83D\uDE00 → 😀)
                                let codepoint = if (0xD800..=0xDBFF).contains(&cp) {
                                    // High surrogate — try to read \uDC00-\uDFFF
                                    if self.bytes.get(self.pos..self.pos + 2) == Some(b"\\u") {
                                        self.pos += 2;
                                        if let Some(low) = self.try_parse_hex4() {
                                            if (0xDC00..=0xDFFF).contains(&low) {
                                                0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00)
                                            } else {
                                                // Not a low surrogate — emit replacement for both
                                                out.extend_from_slice("\u{FFFD}".as_bytes());
                                                low
                                            }
                                        } else {
                                            break; // EOF mid low surrogate
                                        }
                                    } else {
                                        cp // lone high surrogate
                                    }
                                } else {
                                    cp
                                };
                                let mut buf = [0u8; 4];
                                let s = char::from_u32(codepoint)
                                    .unwrap_or('\u{FFFD}')
                                    .encode_utf8(&mut buf);
                                out.extend_from_slice(s.as_bytes());
                            } else {
                                // Incomplete \uXXXX at EOF — stop
                                break;
                            }
                        }
                        _ => {
                            // Invalid escape: keep backslash + char
                            out.push(b'\\');
                            out.push(esc);
                        }
                    }
                }
                _ => out.push(b),
            }
        }

        String::from_utf8_lossy(&out).into_owned()
    }

    /// Try to parse exactly 4 hex digits after \u. Returns None if
    /// fewer than 4 digits remain (EOF mid-escape).
    fn try_parse_hex4(&mut self) -> Option<u32> {
        if self.pos + 4 > self.bytes.len() {
            return None;
        }
        let hex = &self.bytes[self.pos..self.pos + 4];
        let mut val: u32 = 0;
        for &b in hex {
            let d = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return None,
            };
            val = val * 16 + d as u32;
        }
        self.pos += 4;
        Some(val)
    }

    fn parse_object(&mut self) -> Value {
        self.pos += 1; // skip '{'
        self.depth += 1;
        let mut map = serde_json::Map::new();
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Value::Object(map);
        }

        loop {
            self.skip_whitespace();
            match self.peek() {
                None => break, // EOF → return partial
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                Some(b',') => {
                    self.pos += 1;
                    continue;
                }
                Some(b'"') => {}
                _ => break, // malformed → return partial
            }

            let key = self.parse_string();
            self.skip_whitespace();

            // Expect ':'
            match self.peek() {
                Some(b':') => {
                    self.pos += 1;
                }
                _ => {
                    // EOF or malformed after key — insert with null
                    map.insert(key, Value::Null);
                    break;
                }
            }

            // Parse value
            self.skip_whitespace();
            match self.parse_value() {
                Some(val) => {
                    map.insert(key, val);
                }
                None => {
                    map.insert(key, Value::Null);
                    break;
                }
            }
        }

        self.depth -= 1;
        Value::Object(map)
    }

    fn parse_array(&mut self) -> Value {
        self.pos += 1; // skip '['
        self.depth += 1;
        let mut arr: Vec<Value> = Vec::new();
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Value::Array(arr);
        }

        loop {
            self.skip_whitespace();
            match self.peek() {
                None => break, // EOF → return partial
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                Some(b',') => {
                    self.pos += 1;
                    continue;
                }
                _ => {}
            }

            match self.parse_value() {
                Some(val) => arr.push(val),
                None => break,
            }
        }

        self.depth -= 1;
        Value::Array(arr)
    }

    fn parse_number(&mut self) -> Value {
        let start = self.pos;
        // Consume: optional '-', digits, optional '.', digits, optional e/E +/- digits
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }

        let slice = &self.bytes[start..self.pos];
        let s = std::str::from_utf8(slice).unwrap_or("0");
        // Try integer first, then float
        if let Ok(i) = s.parse::<i64>() {
            Value::Number(i.into())
        } else if let Ok(f) = s.parse::<f64>() {
            serde_json::Number::from_f64(f)
                .map(Value::Number)
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        }
    }

    fn parse_literal(&mut self, expected: &[u8], val: Value) -> Option<Value> {
        if self.bytes[self.pos..].starts_with(expected) {
            self.pos += expected.len();
            Some(val)
        } else {
            // Partial literal at EOF (e.g. "tru") — accept it
            let remaining = &self.bytes[self.pos..];
            if expected.starts_with(remaining) {
                self.pos = self.bytes.len();
                Some(val)
            } else {
                None
            }
        }
    }
}

// ── Public API ─────────────────────────────────────────────────

/// Parse potentially incomplete JSON, returning best-effort dict.
/// Never panics. Returns empty object on total failure.
pub fn parse_streaming_json(input: &str) -> Value {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Value::Object(serde_json::Map::new());
    }

    // Fast path: try direct parse first (complete JSON)
    if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
        return val;
    }

    // Slow path: single-pass recursive descent with EOF tolerance
    let mut parser = Parser::new(trimmed);
    parser
        .parse_value()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
}

// ── PyO3 bindings ──────────────────────────────────────────────

#[cfg(feature = "python")]
mod python {
    use super::*;
    use pyo3::prelude::*;
    use pyo3::types::PyDict;

    fn value_to_py(py: Python<'_>, val: &Value) -> PyObject {
        match val {
            Value::Null => py.None(),
            Value::Bool(b) => b.into_pyobject(py).unwrap().to_owned().into_any().unbind(),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    i.into_pyobject(py).unwrap().into_any().unbind()
                } else if let Some(f) = n.as_f64() {
                    f.into_pyobject(py).unwrap().into_any().unbind()
                } else {
                    py.None()
                }
            }
            Value::String(s) => s.into_pyobject(py).unwrap().into_any().unbind(),
            Value::Array(arr) => {
                let list: Vec<PyObject> = arr.iter().map(|v| value_to_py(py, v)).collect();
                list.into_pyobject(py).unwrap().into_any().unbind()
            }
            Value::Object(map) => {
                let dict = PyDict::new(py);
                for (k, v) in map {
                    dict.set_item(k, value_to_py(py, v)).ok();
                }
                dict.into_any().unbind()
            }
        }
    }

    #[pyfunction(name = "parse_streaming_json")]
    fn parse_streaming_json_py(py: Python<'_>, raw: &str) -> PyObject {
        let val = super::parse_streaming_json(raw);
        // Ensure we return a dict (not array/scalar)
        match &val {
            Value::Object(_) => value_to_py(py, &val),
            _ => PyDict::new(py).into_any().unbind(),
        }
    }

    #[pymodule]
    pub fn wing_json(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add_function(wrap_pyfunction!(parse_streaming_json_py, m)?)?;
        Ok(())
    }
}

#[cfg(feature = "python")]
pub use python::wing_json;

// ── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Value {
        parse_streaming_json(raw)
    }

    fn get_str<'a>(v: &'a Value, key: &str) -> &'a str {
        v.get(key).and_then(|v| v.as_str()).unwrap_or("")
    }

    #[test]
    fn complete_json() {
        let v = parse(r#"{"command": "ls -la"}"#);
        assert_eq!(get_str(&v, "command"), "ls -la");
    }

    #[test]
    fn empty_and_whitespace() {
        assert_eq!(parse(""), Value::Object(serde_json::Map::new()));
        assert_eq!(parse("   "), Value::Object(serde_json::Map::new()));
    }

    #[test]
    fn non_dict_returns_array() {
        // Rust API returns raw Value; dict enforcement is in the Python binding.
        let v = parse("[1, 2, 3]");
        assert!(v.is_array());
    }

    #[test]
    fn unterminated_string_value() {
        let v = parse(r#"{"command": "ls -la"#);
        assert_eq!(get_str(&v, "command"), "ls -la");
    }

    #[test]
    fn unterminated_key() {
        let v = parse(r#"{"comm"#);
        assert_eq!(get_str(&v, "comm"), "");
    }

    #[test]
    fn missing_closing_brace() {
        let v = parse(r#"{"path": "/tmp/test.py""#);
        assert_eq!(get_str(&v, "path"), "/tmp/test.py");
    }

    #[test]
    fn trailing_comma() {
        let v = parse(r#"{"a": "1","#);
        assert_eq!(get_str(&v, "a"), "1");
    }

    #[test]
    fn nested_incomplete() {
        let v = parse(r#"{"outer": {"inner": "val"#);
        let outer = v.get("outer").unwrap();
        assert_eq!(get_str(outer, "inner"), "val");
    }

    #[test]
    fn array_incomplete() {
        let v = parse(r#"{"items": ["a", "b"#);
        let items = v.get("items").and_then(|v| v.as_array()).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn raw_control_chars() {
        let v = parse("{\"content\": \"line1\nline2\"}");
        assert_eq!(get_str(&v, "content"), "line1\nline2");
    }

    #[test]
    fn invalid_escape() {
        let v = parse(r#"{"path": "C:\docs\file.txt"}"#);
        assert!(v.is_object());
    }

    #[test]
    fn trailing_backslash() {
        let v = parse(r#"{"cmd": "echo hello\"#);
        assert!(v.is_object());
    }

    #[test]
    fn incomplete_unicode_escape() {
        let v = parse(r#"{"text": "hello\u00"#);
        assert!(v.is_object());
    }

    #[test]
    fn unicode_content() {
        let v = parse(r#"{"content": "你好世界"#);
        assert_eq!(get_str(&v, "content"), "你好世界");
    }

    #[test]
    fn trailing_colon() {
        let v = parse(r#"{"a":"#);
        // Key with no value → null
        assert!(v.get("a").is_some());
    }

    #[test]
    fn streaming_simulation() {
        let chunks = [
            r#"{"com"#,
            r#"{"command": "git"#,
            r#"{"command": "git status"#,
            r#"{"command": "git status"}"#,
        ];
        for chunk in &chunks {
            let v = parse(chunk);
            assert!(v.is_object());
        }
        assert_eq!(get_str(&parse(chunks[3]), "command"), "git status");
    }

    #[test]
    fn write_content_streaming() {
        let v = parse(r#"{"path": "/tmp/hello.py", "content": "def main():\n    print"#);
        assert_eq!(get_str(&v, "path"), "/tmp/hello.py");
        assert!(get_str(&v, "content").contains("def main():"));
    }

    #[test]
    fn numbers() {
        let v = parse(r#"{"int": 42, "float": 3.14, "neg": -1, "exp": 1e5"#);
        assert_eq!(v.get("int").and_then(|v| v.as_i64()), Some(42));
        assert!(v.get("float").and_then(|v| v.as_f64()).is_some());
        assert_eq!(v.get("neg").and_then(|v| v.as_i64()), Some(-1));
    }

    #[test]
    fn literals() {
        let v = parse(r#"{"a": true, "b": false, "c": null"#);
        assert_eq!(v.get("a").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(v.get("b").and_then(|v| v.as_bool()), Some(false));
        assert!(v.get("c").map(|v| v.is_null()).unwrap_or(false));
    }

    #[test]
    fn partial_literal_at_eof() {
        let v = parse(r#"{"a": tru"#);
        assert_eq!(v.get("a").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn surrogate_pair_emoji() {
        // 😀 = U+1F600 = \uD83D\uDE00
        let v = parse(r#"{"emoji": "\uD83D\uDE00"}"#);
        assert_eq!(get_str(&v, "emoji"), "😀");
    }

    #[test]
    fn lone_surrogate_replacement() {
        // Lone high surrogate → U+FFFD
        let v = parse(r#"{"x": "\uD800"}"#);
        assert_eq!(get_str(&v, "x"), "\u{FFFD}");
    }

    #[test]
    fn deep_nesting_no_crash() {
        // 200 levels of nesting — should not stack overflow
        let deep = "[".repeat(200) + &"]".repeat(200);
        let v = parse(&deep);
        assert!(v.is_array());
    }

    #[test]
    fn deep_object_nesting_no_crash() {
        let deep = r#"{"a":"#.repeat(200) + &"}".repeat(200);
        let v = parse(&deep);
        assert!(v.is_object());
    }
}
