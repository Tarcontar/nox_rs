//! JSON as OTIO reads and writes it: standard JSON plus the non-finite
//! numbers `NaN`, `Inf`/`Infinity` and `-Inf`/`-Infinity`. Integers beyond
//! 64 bits become floats, as in OTIO's RapidJSON-based reader.
//!
//! The writer produces OTIO's layout: four-space indentation, keys in
//! insertion order, floats with a decimal point.

use nox_project::{Object, Value};
use std::fmt::Write;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("line {line}, column {column}: {message}")]
pub struct ParseError {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

/// Nesting limit, so malicious files can't overflow the stack.
const MAX_DEPTH: usize = 512;

pub fn parse(text: &str) -> Result<Value, ParseError> {
    let mut parser = Parser {
        text: text.as_bytes(),
        pos: 0,
        depth: 0,
    };
    // Tolerate a UTF-8 byte order mark.
    if text.starts_with('\u{feff}') {
        parser.pos = 3;
    }
    let value = parser.value()?;
    parser.skip_whitespace();
    if parser.pos < parser.text.len() {
        return Err(parser.error("trailing characters after the document"));
    }
    Ok(value)
}

struct Parser<'a> {
    text: &'a [u8],
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn error(&self, message: impl Into<String>) -> ParseError {
        let before = &self.text[..self.pos.min(self.text.len())];
        let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
        let line_start = before
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
        ParseError {
            line,
            column: self.pos - line_start + 1,
            message: message.into(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, literal: &str) -> bool {
        if self.text[self.pos..].starts_with(literal.as_bytes()) {
            self.pos += literal.len();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), ParseError> {
        self.skip_whitespace();
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error(format!("expected '{}'", byte as char)))
        }
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'{') => self.nested(Self::object),
            Some(b'[') => self.nested(Self::array),
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b't') if self.eat("true") => Ok(Value::Bool(true)),
            Some(b'f') if self.eat("false") => Ok(Value::Bool(false)),
            Some(b'n') if self.eat("null") => Ok(Value::Null),
            Some(b'N') if self.eat("NaN") => Ok(Value::Float(f64::NAN)),
            Some(b'I') if self.eat("Infinity") || self.eat("Inf") => {
                Ok(Value::Float(f64::INFINITY))
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end of document")),
        }
    }

    fn nested(
        &mut self,
        parse: fn(&mut Self) -> Result<Value, ParseError>,
    ) -> Result<Value, ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        let value = parse(self);
        self.depth -= 1;
        value
    }

    fn object(&mut self) -> Result<Value, ParseError> {
        self.pos += 1; // {
        let mut object = Object::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(object));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a key"));
            }
            let key = self.string()?;
            self.expect(b':')?;
            let value = self.value()?;
            object.insert(key, value);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(object));
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self) -> Result<Value, ParseError> {
        self.pos += 1; // [
        let mut array = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(array));
        }
        loop {
            array.push(self.value()?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(array));
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.pos += 1; // "
        let mut bytes = Vec::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.error("unterminated string"));
            };
            self.pos += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let Some(escape) = self.peek() else {
                        return Err(self.error("unterminated string"));
                    };
                    self.pos += 1;
                    let c = match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode_escape()?,
                        _ => return Err(self.error("invalid escape")),
                    };
                    bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                }
                0..0x20 => return Err(self.error("control character in string")),
                _ => bytes.push(byte),
            }
        }
        String::from_utf8(bytes).map_err(|_| self.error("invalid UTF-8"))
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let digits = self
            .text
            .get(self.pos..self.pos + 4)
            .and_then(|d| std::str::from_utf8(d).ok())
            .and_then(|d| u32::from_str_radix(d, 16).ok())
            .ok_or_else(|| self.error("invalid \\u escape"))?;
        self.pos += 4;
        Ok(digits)
    }

    fn unicode_escape(&mut self) -> Result<char, ParseError> {
        let high = self.hex4()?;
        let code = if (0xd800..0xdc00).contains(&high) {
            if !self.eat("\\u") {
                return Err(self.error("unpaired surrogate"));
            }
            let low = self.hex4()?;
            if !(0xdc00..0xe000).contains(&low) {
                return Err(self.error("unpaired surrogate"));
            }
            0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00)
        } else {
            high
        };
        char::from_u32(code).ok_or_else(|| self.error("invalid code point"))
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        if self.eat("-") && (self.eat("Infinity") || self.eat("Inf")) {
            return Ok(Value::Float(f64::NEG_INFINITY));
        }
        let mut is_float = false;
        while let Some(b) = self.peek() {
            match b {
                b'0'..=b'9' => {}
                b'.' | b'e' | b'E' | b'+' | b'-' => is_float = true,
                _ => break,
            }
            self.pos += 1;
        }
        let text = std::str::from_utf8(&self.text[start..self.pos]).expect("ASCII");
        if !is_float {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Value::Int(i));
            }
            if let Ok(u) = text.parse::<u64>() {
                return Ok(Value::UInt(u));
            }
        }
        text.parse::<f64>()
            .map(Value::Float)
            .map_err(|_| self.error(format!("invalid number '{text}'")))
    }
}

/// Serializes `value` with OTIO's layout.
pub fn to_string(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0);
    out.push('\n');
    out
}

fn write_value(out: &mut String, value: &Value, indent: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => write!(out, "{i}").expect("write to string"),
        Value::UInt(u) => write!(out, "{u}").expect("write to string"),
        Value::Float(f) => write_float(out, *f),
        Value::String(s) => write_string(out, s),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                out.push_str(if i == 0 { "\n" } else { ",\n" });
                push_indent(out, indent + 1);
                write_value(out, item, indent + 1);
            }
            out.push('\n');
            push_indent(out, indent);
            out.push(']');
        }
        Value::Object(object) if object.is_empty() => out.push_str("{}"),
        Value::Object(object) => {
            out.push('{');
            for (i, (key, item)) in object.iter().enumerate() {
                out.push_str(if i == 0 { "\n" } else { ",\n" });
                push_indent(out, indent + 1);
                write_string(out, key);
                out.push_str(": ");
                write_value(out, item, indent + 1);
            }
            out.push('\n');
            push_indent(out, indent);
            out.push('}');
        }
    }
}

fn push_indent(out: &mut String, level: usize) {
    out.extend(std::iter::repeat_n("    ", level));
}

/// Shortest representation that reads back exactly, always marked as a
/// float (`24.0`, `1e300`), and OTIO's spelling of non-finite values.
fn write_float(out: &mut String, f: f64) {
    if f.is_nan() {
        out.push_str("NaN");
    } else if f.is_infinite() {
        out.push_str(if f > 0.0 { "Infinity" } else { "-Infinity" });
    } else {
        write!(out, "{f:?}").expect("write to string");
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32).expect("write to string"),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_otio_extensions() {
        let value =
            parse(r#"{"a": NaN, "b": Inf, "c": -Infinity, "d": Infinity, "e": -Inf}"#).unwrap();
        let o = value.as_object().unwrap();
        assert!(o.get("a").unwrap().as_f64().unwrap().is_nan());
        assert_eq!(o.get("b"), Some(&Value::Float(f64::INFINITY)));
        assert_eq!(o.get("c"), Some(&Value::Float(f64::NEG_INFINITY)));
        assert_eq!(o.get("d"), Some(&Value::Float(f64::INFINITY)));
        assert_eq!(o.get("e"), Some(&Value::Float(f64::NEG_INFINITY)));
    }

    #[test]
    fn number_kinds() {
        let value = parse("[1, -2, 18446744073709551615, 170141183460469231731687303715884105728, 1.5, 2e3, 24.0]").unwrap();
        assert_eq!(
            value.as_array().unwrap(),
            [
                Value::Int(1),
                Value::Int(-2),
                Value::UInt(u64::MAX),
                Value::Float(1.7014118346046923e38),
                Value::Float(1.5),
                Value::Float(2000.0),
                Value::Float(24.0),
            ]
        );
    }

    #[test]
    fn strings_and_escapes() {
        let value = parse(r#""a\"b\\c\/\nä🎬""#).unwrap();
        assert_eq!(value.as_str(), Some("a\"b\\c/\nä🎬"));
        assert!(parse(r#""\ud83c""#).is_err());
        assert!(parse("\"a\u{1}\"").is_err());
    }

    #[test]
    fn errors_have_positions() {
        let err = parse("{\n  \"a\": [1,\n  }").unwrap_err();
        assert_eq!((err.line, err.column), (3, 3));
        assert!(parse("[1] x").is_err());
        assert!(parse("").is_err());
        assert!(parse(&"[".repeat(10_000)).is_err());
    }

    #[test]
    fn round_trip_layout() {
        let text = r#"{
    "OTIO_SCHEMA": "Gap.1",
    "metadata": {},
    "list": [
        1,
        2.0,
        "x\ty"
    ],
    "nan": NaN,
    "big": 1e300,
    "neg": -Infinity,
    "none": null,
    "yes": true,
    "empty": []
}
"#;
        assert_eq!(to_string(&parse(text).unwrap()), text);
    }
}
