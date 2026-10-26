// SPDX-License-Identifier: GPL-2.0-only
//! The arithmetic of the `compute` tool.
//!
//! The language is a small integer expression: `+`, `-`, `*`, `/`, `%`,
//! parentheses and ASCII whitespace. A leading `+` or `-` applies to one
//! operand, so `-3 * 4` is `-12`. Every operation is checked: an overflow, a
//! division by zero and a malformed expression are errors, never a wrapped
//! value or a panic.
//!
//! The parser is recursive descent over the bytes, which is all this language
//! needs: precedence is one function per level, and the deepest level reads a
//! parenthesised expression or a decimal integer.

/// Evaluate one expression over `i64`.
///
/// The text must be a complete expression; trailing input is an error.
pub fn evaluate(text: &str) -> Result<i64, String> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        position: 0,
    };
    parser.skip();
    if parser.peek().is_none() {
        return Err("empty expression".to_string());
    }
    let value = parser.expression()?;
    parser.skip();
    if let Some(byte) = parser.peek() {
        return Err(format!(
            "unexpected '{}' at byte {}",
            byte as char, parser.position
        ));
    }
    Ok(value)
}

/// A cursor over the bytes of one expression.
struct Parser<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl Parser<'_> {
    /// The next byte without consuming it.
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    /// Consume the next byte.
    fn bump(&mut self) -> Result<u8, String> {
        let byte = self
            .peek()
            .ok_or_else(|| "unexpected end of expression".to_string())?;
        self.position += 1;
        Ok(byte)
    }

    /// Skip ASCII whitespace.
    fn skip(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.position += 1;
        }
    }

    /// Sum and difference, the lowest precedence.
    fn expression(&mut self) -> Result<i64, String> {
        let mut value = self.term()?;
        loop {
            self.skip();
            match self.peek() {
                Some(b'+') => {
                    self.position += 1;
                    let right = self.term()?;
                    value = value
                        .checked_add(right)
                        .ok_or_else(|| "addition overflow".to_string())?;
                }
                Some(b'-') => {
                    self.position += 1;
                    let right = self.term()?;
                    value = value
                        .checked_sub(right)
                        .ok_or_else(|| "subtraction overflow".to_string())?;
                }
                _ => return Ok(value),
            }
        }
    }

    /// Product, quotient and remainder, above the sum.
    fn term(&mut self) -> Result<i64, String> {
        let mut value = self.unary()?;
        loop {
            self.skip();
            match self.peek() {
                Some(b'*') => {
                    self.position += 1;
                    let right = self.unary()?;
                    value = value
                        .checked_mul(right)
                        .ok_or_else(|| "multiplication overflow".to_string())?;
                }
                Some(b'/') => {
                    self.position += 1;
                    let right = self.unary()?;
                    if right == 0 {
                        return Err("division by zero".to_string());
                    }
                    value = value
                        .checked_div(right)
                        .ok_or_else(|| "division overflow".to_string())?;
                }
                Some(b'%') => {
                    self.position += 1;
                    let right = self.unary()?;
                    if right == 0 {
                        return Err("division by zero".to_string());
                    }
                    value = value
                        .checked_rem(right)
                        .ok_or_else(|| "remainder overflow".to_string())?;
                }
                _ => return Ok(value),
            }
        }
    }

    /// One signed operand.
    fn unary(&mut self) -> Result<i64, String> {
        self.skip();
        match self.peek() {
            Some(b'+') => {
                self.position += 1;
                self.unary()
            }
            Some(b'-') => {
                self.position += 1;
                let value = self.unary()?;
                value
                    .checked_neg()
                    .ok_or_else(|| "negation overflow".to_string())
            }
            _ => self.primary(),
        }
    }

    /// A parenthesised expression or an integer.
    fn primary(&mut self) -> Result<i64, String> {
        self.skip();
        match self.peek() {
            Some(b'(') => {
                self.position += 1;
                let value = self.expression()?;
                self.skip();
                match self.bump()? {
                    b')' => Ok(value),
                    byte => Err(format!(
                        "expected ')' at byte {}, found '{}'",
                        self.position, byte as char
                    )),
                }
            }
            Some(byte) if byte.is_ascii_digit() => self.number(),
            Some(byte) => Err(format!(
                "unexpected '{}' at byte {}",
                byte as char, self.position
            )),
            None => Err("unexpected end of expression".to_string()),
        }
    }

    /// A decimal integer, checked against the range of `i64`.
    fn number(&mut self) -> Result<i64, String> {
        let start = self.position;
        let mut value: i64 = 0;
        while let Some(byte) = self.peek() {
            if !byte.is_ascii_digit() {
                break;
            }
            self.position += 1;
            let digit = i64::from(byte - b'0');
            value = value
                .checked_mul(10)
                .and_then(|value| value.checked_add(digit))
                .ok_or_else(|| "integer literal overflow".to_string())?;
        }
        if self.position == start {
            return Err("expected an integer".to_string());
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::evaluate;

    #[test]
    fn applies_precedence_and_parentheses() {
        assert_eq!(evaluate("1 + 2 * 3"), Ok(7));
        assert_eq!(evaluate("(1 + 2) * 3"), Ok(9));
        assert_eq!(evaluate("2 * (3 + 4) - 5"), Ok(9));
        assert_eq!(evaluate("  10  /  2  %  3  "), Ok(2));
        assert_eq!(evaluate("7 / 2"), Ok(3));
        assert_eq!(evaluate("-3 * 4"), Ok(-12));
        assert_eq!(evaluate("+5 - -2"), Ok(7));
    }

    #[test]
    fn rejects_a_division_by_zero() {
        assert_eq!(evaluate("1 / 0"), Err("division by zero".to_string()));
        assert_eq!(evaluate("1 % 0"), Err("division by zero".to_string()));
    }

    #[test]
    fn rejects_a_malformed_expression() {
        assert!(evaluate("").is_err());
        assert!(evaluate("1 +").is_err());
        assert!(evaluate("(1 + 2").is_err());
        assert!(evaluate("1 2").is_err());
        assert!(evaluate("a + 1").is_err());
        assert!(evaluate("1 + 2)").is_err());
    }

    #[test]
    fn rejects_an_overflow_instead_of_wrapping() {
        assert!(evaluate("9223372036854775807 + 1").is_err());
        assert!(evaluate("99999999999999999999").is_err());
    }
}
