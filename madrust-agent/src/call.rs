// SPDX-License-Identifier: GPL-2.0-only
//! The parser of one tool call.
//!
//! [`madrust_infer::grammar`] constrains the model to the shape
//! `{"tool":"NAME","args":{...}}`, so the answer is already well formed when
//! it reaches this module. The parser still checks every byte: a grammar bug
//! or a prompt that the model answered without the mask must become an error,
//! not a call with silently missing arguments.
//!
//! [`madrust_infer::json::Scanner`] does the string and number lexing. The
//! arguments become a map of string to [`Value`]; the tools read that map and
//! [`Call::args_json`] writes it back for the `AGENT` line.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use madrust_infer::error::{Error, Result};
use madrust_infer::json::Scanner;

/// One argument value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A JSON string.
    String(String),
    /// A JSON number.
    Number(f64),
    /// `true` or `false`.
    Bool(bool),
    /// `null`.
    ///
    /// The grammar of a tool call offers it as an argument value, so the
    /// parser accepts it: a step that ends because the model chose `null`
    /// loses the answer for no reason.
    Null,
}

impl Value {
    /// The text of a string value.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(value) => Some(value),
            _ => None,
        }
    }

    /// Write the value as JSON.
    pub fn write_json(&self, out: &mut String) {
        match self {
            Value::String(value) => write_json_string(out, value),
            Value::Number(value) => {
                if value.is_finite()
                    && value.fract() == 0.0
                    && value.abs() <= 9_007_199_254_740_992.0
                {
                    let _ = write!(out, "{}", *value as i64);
                } else {
                    let _ = write!(out, "{value}");
                }
            }
            Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Value::Null => out.push_str("null"),
        }
    }
}

/// One parsed tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    /// Name of the tool the model chose.
    pub tool: String,
    /// Arguments, keyed by name in stable order.
    pub args: BTreeMap<String, Value>,
}

impl Call {
    /// Parse the bytes the model produced.
    ///
    /// The tool name comes first and the argument object second, the order the
    /// grammar fixes. A missing, duplicated, reordered or trailing member is
    /// an error.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut scanner = Scanner::new(bytes);
        scanner.skip_whitespace();
        scanner.expect(b'{')?;

        let key = member_key(&mut scanner)?;
        if key != "tool" {
            return Err(Error::new(format!(
                "call: expected the tool member, found {key:?}"
            )));
        }
        let tool = scanner.parse_string()?;

        scanner.skip_whitespace();
        scanner.expect(b',')?;
        let key = member_key(&mut scanner)?;
        if key != "args" {
            return Err(Error::new(format!(
                "call: expected the args member, found {key:?}"
            )));
        }

        let args = parse_args(&mut scanner)?;

        scanner.skip_whitespace();
        scanner.expect(b'}')?;
        if !scanner.at_end() {
            return Err(Error::new(format!(
                "call: trailing bytes at byte {}",
                scanner.position()
            )));
        }
        Ok(Self { tool, args })
    }

    /// The text of one string argument.
    pub fn string(&self, key: &str) -> Option<&str> {
        self.args.get(key).and_then(Value::as_str)
    }

    /// Write the argument map as a JSON object.
    pub fn args_json(&self) -> String {
        let mut out = String::from("{");
        for (index, (key, value)) in self.args.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            write_json_string(&mut out, key);
            out.push(':');
            value.write_json(&mut out);
        }
        out.push('}');
        out
    }
}

/// Read `"key":` and return the key.
fn member_key(scanner: &mut Scanner<'_>) -> Result<String> {
    scanner.skip_whitespace();
    let key = scanner.parse_string()?;
    scanner.skip_whitespace();
    scanner.expect(b':')?;
    scanner.skip_whitespace();
    Ok(key)
}

/// Read the argument object after the colon.
fn parse_args(scanner: &mut Scanner<'_>) -> Result<BTreeMap<String, Value>> {
    let mut args = BTreeMap::new();
    scanner.skip_whitespace();
    scanner.expect(b'{')?;
    scanner.skip_whitespace();
    if scanner.peek() == Some(b'}') {
        scanner.expect(b'}')?;
        return Ok(args);
    }
    loop {
        let key = member_key(scanner)?;
        let value = parse_value(scanner)?;
        if args.insert(key.clone(), value).is_some() {
            return Err(Error::new(format!("call: duplicate argument {key:?}")));
        }
        scanner.skip_whitespace();
        match scanner.bump()? {
            b',' => {
                scanner.skip_whitespace();
            }
            b'}' => return Ok(args),
            byte => {
                return Err(Error::new(format!(
                    "call: expected ',' or '}}' at byte {}, found '{}'",
                    scanner.position(),
                    byte as char
                )))
            }
        }
    }
}

/// Read one argument value.
fn parse_value(scanner: &mut Scanner<'_>) -> Result<Value> {
    scanner.skip_whitespace();
    match scanner.peek() {
        Some(b'"') => Ok(Value::String(scanner.parse_string()?)),
        Some(b't') | Some(b'f') => Ok(Value::Bool(scanner.parse_bool()?)),
        Some(b'n') => {
            scanner.skip_whitespace();
            for expected in b"null" {
                if scanner.bump()? != *expected {
                    return Err(Error::new("call: the value is not null"));
                }
            }
            Ok(Value::Null)
        }
        Some(_) => {
            let value = scanner.parse_number()?;
            if !value.is_finite() {
                return Err(Error::new("call: the number is not finite"));
            }
            Ok(Value::Number(value))
        }
        None => Err(Error::new("call: unexpected end of document")),
    }
}

/// Write one escaped JSON string.
fn write_json_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", character as u32);
            }
            character => out.push(character),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::{Call, Value};

    #[test]
    fn parses_the_three_argument_value_kinds() {
        let call = Call::parse(
            br#"{"tool":"read_file","args":{"path":"/vocab.json","limit":12,"ok":true}}"#,
        )
        .expect("the call is valid");
        assert_eq!(call.tool, "read_file");
        assert_eq!(call.string("path"), Some("/vocab.json"));
        assert_eq!(call.args.get("limit"), Some(&Value::Number(12.0)));
        assert_eq!(call.args.get("ok"), Some(&Value::Bool(true)));
    }

    #[test]
    fn parses_an_empty_argument_object() {
        let call = Call::parse(br#"{"tool":"done","args":{}}"#).expect("the call is valid");
        assert_eq!(call.tool, "done");
        assert!(call.args.is_empty());
        assert_eq!(call.args_json(), "{}");
    }

    #[test]
    fn writes_the_arguments_back_with_escapes() {
        let call =
            Call::parse(br#"{"tool":"write_file","args":{"path":"/a","text":"line\n\"quoted\""}}"#)
                .expect("the call is valid");
        assert_eq!(
            call.args_json(),
            r#"{"path":"/a","text":"line\n\"quoted\""}"#
        );
    }

    #[test]
    fn rejects_a_wrong_order_and_trailing_bytes() {
        assert!(Call::parse(br#"{"args":{},"tool":"done"}"#).is_err());
        assert!(Call::parse(br#"{"tool":"done","args":{}} extra"#).is_err());
        assert!(Call::parse(br#"{"tool":"done","args":{"a":1,"a":2}}"#).is_err());
    }

    #[test]
    fn accepts_null_as_a_value() {
        // The grammar of a tool call offers null as an argument value, so the
        // parser takes it: a step that ends because the model chose null
        // loses the answer for no reason.
        let call = Call::parse(br#"{"tool":"read_file","args":{"path":null}}"#)
            .expect("the call is valid");
        assert_eq!(call.args.get("path"), Some(&Value::Null));
        assert_eq!(call.string("path"), None);
        assert_eq!(call.args_json(), r#"{"path":null}"#);
    }

    #[test]
    fn refuses_a_number_that_is_not_finite() {
        assert!(Call::parse(br#"{"tool":"done","args":{"limit":1e999}}"#).is_err());
    }
}
