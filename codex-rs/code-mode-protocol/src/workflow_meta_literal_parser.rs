use super::LiteralValue;
use super::parser::Parser;
use super::truncate_for_error;

impl Parser<'_> {
    /// Parse a single- or double-quoted string literal, decoding escapes.
    pub(super) fn parse_string(&mut self) -> Result<String, String> {
        let quote = self
            .bump()
            .ok_or_else(|| "unexpected end of input while parsing a string literal".to_string())?;
        let mut out = String::new();
        loop {
            self.guard()?;
            match self.bump() {
                None => {
                    return Err("unterminated string literal in `meta`".to_string());
                }
                Some(ch) if ch == quote => break,
                Some('\\') => {
                    self.parse_escape(&mut out)?;
                }
                Some('\n') => {
                    return Err(
                        "unterminated string literal in `meta` (newline before closing quote)"
                            .to_string(),
                    );
                }
                Some(ch) => out.push(ch),
            }
        }
        Ok(out)
    }

    /// Decode one escape sequence (the leading backslash is already consumed).
    fn parse_escape(&mut self, out: &mut String) -> Result<(), String> {
        match self.bump() {
            None => Err("unterminated escape sequence in `meta` string literal".to_string()),
            Some('n') => {
                out.push('\n');
                Ok(())
            }
            Some('t') => {
                out.push('\t');
                Ok(())
            }
            Some('r') => {
                out.push('\r');
                Ok(())
            }
            Some('b') => {
                out.push('\u{8}');
                Ok(())
            }
            Some('f') => {
                out.push('\u{c}');
                Ok(())
            }
            Some('v') => {
                out.push('\u{b}');
                Ok(())
            }
            Some('0') => {
                out.push('\0');
                Ok(())
            }
            Some('\n') => {
                // Line continuation: emit nothing.
                Ok(())
            }
            Some('x') => {
                let hi = self.bump();
                let lo = self.bump();
                match (hi, lo) {
                    (Some(hi), Some(lo)) => {
                        let code = u32::from_str_radix(&format!("{hi}{lo}"), 16).map_err(|_| {
                            "invalid `\\xHH` escape in `meta` string literal".to_string()
                        })?;
                        push_code_point(out, code)
                    }
                    _ => Err("invalid `\\xHH` escape in `meta` string literal".to_string()),
                }
            }
            Some('u') => self.parse_unicode_escape(out),
            Some(other) => {
                // `\\`, `\'`, `\"`, `\``, `\/` and any other char map to itself.
                out.push(other);
                Ok(())
            }
        }
    }

    /// Decode a `\uXXXX` or `\u{...}` escape.
    ///
    /// A `\uXXXX` escape that encodes a high surrogate is combined with an
    /// immediately following `\uXXXX` low surrogate into the single astral
    /// code point they jointly denote (e.g. `😀` -> U+1F600). Lone
    /// surrogates — high or low — cannot be represented as a Rust `char` and
    /// are rejected.
    fn parse_unicode_escape(&mut self, out: &mut String) -> Result<(), String> {
        if self.peek() == Some('{') {
            self.bump();
            let mut hex = String::new();
            while let Some(ch) = self.peek() {
                self.guard()?;
                if ch == '}' {
                    break;
                }
                hex.push(ch);
                self.bump();
            }
            if self.peek() != Some('}') {
                return Err("unterminated `\\u{...}` escape in `meta` string literal".to_string());
            }
            self.bump();
            let code = u32::from_str_radix(&hex, 16)
                .map_err(|_| "invalid `\\u{...}` escape in `meta` string literal".to_string())?;
            push_code_point(out, code)
        } else {
            let code = self.read_four_hex()?;
            if is_high_surrogate(code) {
                // Look for a paired `\uXXXX` (four-hex form) low surrogate.
                if self.peek() == Some('\\')
                    && self.peek_at(1) == Some('u')
                    && self.peek_at(2) != Some('{')
                {
                    self.bump(); // `\`
                    self.bump(); // `u`
                    let low = self.read_four_hex()?;
                    if is_low_surrogate(low) {
                        let combined = 0x1_0000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                        return push_code_point(out, combined);
                    }
                    return Err(
                        "invalid surrogate pair in `meta` string literal: `\\uXXXX` high surrogate not followed by a low surrogate"
                            .to_string(),
                    );
                }
                Err(
                    "lone high surrogate `\\uXXXX` escape in `meta` string literal is not a valid character"
                        .to_string(),
                )
            } else if is_low_surrogate(code) {
                Err(
                    "lone low surrogate `\\uXXXX` escape in `meta` string literal is not a valid character"
                        .to_string(),
                )
            } else {
                push_code_point(out, code)
            }
        }
    }

    /// Read exactly four hex digits and return their numeric value.
    fn read_four_hex(&mut self) -> Result<u32, String> {
        let start = self.pos;
        for _ in 0..4 {
            match self.bump() {
                Some(ch) if ch.is_ascii_hexdigit() => {}
                _ => {
                    return Err("invalid `\\uXXXX` escape in `meta` string literal".to_string());
                }
            }
        }
        u32::from_str_radix(&self.source[start..self.pos], 16)
            .map_err(|_| "invalid `\\uXXXX` escape in `meta` string literal".to_string())
    }

    /// Parse a plain decimal/float number literal.
    pub(super) fn parse_number(&mut self) -> Result<LiteralValue, String> {
        let start = self.pos;
        if matches!(self.peek(), Some('-') | Some('+')) {
            self.bump();
        }
        let mut saw_digit = false;
        while let Some(ch) = self.peek() {
            self.guard()?;
            if ch.is_ascii_digit() {
                saw_digit = true;
                self.bump();
            } else if ch == '.' || ch == 'e' || ch == 'E' || ch == '+' || ch == '-' || ch == '_' {
                self.bump();
            } else {
                break;
            }
        }
        if !saw_digit {
            return Err("invalid numeric literal in `meta`".to_string());
        }
        let raw: String = self.source[start..self.pos]
            .chars()
            .filter(|ch| *ch != '_')
            .collect();
        raw.parse::<f64>().map(LiteralValue::Number).map_err(|_| {
            format!(
                "invalid numeric literal `{}` in `meta`",
                truncate_for_error(&raw)
            )
        })
    }
}

fn push_code_point(out: &mut String, code: u32) -> Result<(), String> {
    match char::from_u32(code) {
        Some(ch) => {
            out.push(ch);
            Ok(())
        }
        None => Err("invalid unicode code point in `meta` string literal".to_string()),
    }
}

fn is_high_surrogate(code: u32) -> bool {
    (0xD800..=0xDBFF).contains(&code)
}

fn is_low_surrogate(code: u32) -> bool {
    (0xDC00..=0xDFFF).contains(&code)
}
