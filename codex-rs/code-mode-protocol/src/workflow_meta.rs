//! Bounded static parser for a workflow's leading `export const meta` manifest.
//!
//! Workflow discovery reads untrusted files. This parser therefore accepts a deliberately small
//! JavaScript literal grammar, never evaluates code, and stops as soon as the manifest statement
//! ends.

#[path = "workflow_phase_meta.rs"]
mod phase_meta;

use crate::workflow_bounds::WORKFLOW_PHASES_MAX_ITEMS;
use crate::workflow_bounds::ensure_parsed_workflow_meta;

/// Maximum source bytes inspected while parsing a workflow manifest.
pub const WORKFLOW_META_MAX_BYTES: usize = 256 * 1024;

/// Statically parsed workflow metadata used by discovery and execution setup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedWorkflowMeta {
    /// Stable saved-workflow name.
    pub name: String,
    /// Human-readable picker and approval description.
    pub description: String,
    /// Declared phase titles in source order.
    pub phases: Vec<String>,
}

/// Parse the leading workflow manifest without evaluating or scanning the workflow body.
pub fn parse_workflow_meta(source: &str) -> Result<ParsedWorkflowMeta, String> {
    let mut parser = Parser { source, pos: 0 };
    parser.skip_trivia()?;
    parser.expect_identifier("export")?;
    parser.skip_trivia()?;
    parser.expect_identifier("const")?;
    parser.skip_trivia()?;
    parser.expect_identifier("meta")?;
    parser.skip_trivia()?;
    parser.expect_char('=', "expected `=` after `export const meta`")?;
    parser.skip_trivia()?;

    let meta = parser.parse_meta_object()?;
    parser.expect_statement_end()?;
    ensure_parsed_workflow_meta(&meta)?;
    Ok(meta)
}

struct Parser<'a> {
    source: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn parse_meta_object(&mut self) -> Result<ParsedWorkflowMeta, String> {
        self.expect_char(
            '{',
            "`meta` must be a static object literal beginning with `{`",
        )?;
        let mut name = None;
        let mut description = None;
        let mut phases = None;

        loop {
            self.skip_trivia()?;
            if self.consume_char('}')? {
                break;
            }
            let key = self.parse_property_key()?;
            self.skip_trivia()?;
            if self.peek() == Some('(') {
                return Err("methods are not allowed in the `meta` object".to_string());
            }
            self.expect_char(
                ':',
                "workflow metadata properties require `:` and a literal value",
            )?;
            self.skip_trivia()?;

            match key.as_str() {
                "name" => {
                    if name.is_some() {
                        return Err("duplicate `meta.name` property".to_string());
                    }
                    name = Some(self.parse_string_field("`meta.name`")?);
                }
                "description" => {
                    if description.is_some() {
                        return Err("duplicate `meta.description` property".to_string());
                    }
                    description = Some(self.parse_string_field("`meta.description`")?);
                }
                "phases" => {
                    if phases.is_some() {
                        return Err("duplicate `meta.phases` property".to_string());
                    }
                    phases = Some(self.parse_phases()?);
                }
                _ => {
                    return Err(
                        "unsupported workflow metadata property; v1 supports only `name`, `description`, and `phases`"
                            .to_string(),
                    );
                }
            }
            self.expect_entry_end('}')?;
        }

        Ok(ParsedWorkflowMeta {
            name: name.ok_or_else(|| "`meta.name` is required".to_string())?,
            description: description.ok_or_else(|| "`meta.description` is required".to_string())?,
            phases: phases.unwrap_or_default(),
        })
    }

    fn parse_phases(&mut self) -> Result<Vec<String>, String> {
        self.expect_char('[', "`meta.phases` must be an array of phase literals")?;
        let mut phases = Vec::new();
        loop {
            self.skip_trivia()?;
            if self.consume_char(']')? {
                break;
            }
            if phases.len() == WORKFLOW_PHASES_MAX_ITEMS {
                return Err(format!(
                    "`meta.phases` has more than {WORKFLOW_PHASES_MAX_ITEMS} entries"
                ));
            }
            let title = if self.peek() == Some('{') {
                self.parse_phase_object()?
            } else {
                self.parse_string_field("`meta.phases` entry")?
            };
            phases.push(title);
            self.expect_entry_end(']')?;
        }
        Ok(phases)
    }

    fn parse_string_field(&mut self, field: &str) -> Result<String, String> {
        match self.peek() {
            Some('\'') | Some('"') => self.parse_string(),
            Some('`') => Err(format!(
                "{field} must be a quoted string literal; template strings are not allowed"
            )),
            _ => Err(format!("{field} must be a quoted string literal")),
        }
    }

    fn parse_property_key(&mut self) -> Result<String, String> {
        match self.peek() {
            Some('\'') | Some('"') => self.parse_string(),
            Some('[') => {
                Err("computed properties are not allowed in workflow metadata".to_string())
            }
            Some('.') if self.remaining().starts_with("...") => {
                Err("spread properties are not allowed in workflow metadata".to_string())
            }
            _ => self.parse_identifier()?.map(str::to_string).ok_or_else(|| {
                "workflow metadata keys must be identifiers or quoted strings".to_string()
            }),
        }
    }

    fn parse_string(&mut self) -> Result<String, String> {
        let quote = self
            .bump()?
            .ok_or_else(|| "expected a quoted string literal".to_string())?;
        let mut output = String::new();
        loop {
            let Some(ch) = self.bump()? else {
                return Err("unterminated string literal in workflow metadata".to_string());
            };
            if ch == quote {
                return Ok(output);
            }
            if is_line_terminator(ch) {
                return Err("raw line terminators are not allowed in string literals".to_string());
            }
            if ch == '\\' {
                self.parse_escape(&mut output)?;
            } else {
                output.push(ch);
            }
        }
    }

    fn parse_escape(&mut self, output: &mut String) -> Result<(), String> {
        let Some(escaped) = self.bump()? else {
            return Err("unterminated escape in workflow metadata string".to_string());
        };
        match escaped {
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            'b' => output.push('\u{8}'),
            'f' => output.push('\u{c}'),
            'v' => output.push('\u{b}'),
            '0' if !self.peek().is_some_and(|ch| ch.is_ascii_digit()) => output.push('\0'),
            '0'..='9' => {
                return Err("legacy octal escapes are not allowed in workflow metadata".to_string());
            }
            '\n' | '\u{2028}' | '\u{2029}' => {}
            '\r' => {
                self.consume_char('\n')?;
            }
            'x' => {
                let code = self.read_hex(/*digits*/ 2)?;
                let Some(decoded) = char::from_u32(code) else {
                    return Err("invalid hexadecimal escape in workflow metadata".to_string());
                };
                output.push(decoded);
            }
            'u' => self.parse_unicode_escape(output)?,
            other => output.push(other),
        }
        Ok(())
    }

    fn parse_unicode_escape(&mut self, output: &mut String) -> Result<(), String> {
        let code = if self.consume_char('{')? {
            let mut value = 0_u32;
            let mut digits = 0;
            loop {
                let Some(ch) = self.peek() else {
                    return Err("unterminated braced Unicode escape".to_string());
                };
                if ch == '}' {
                    self.bump()?;
                    break;
                }
                let digit = ch
                    .to_digit(16)
                    .ok_or_else(|| "invalid braced Unicode escape".to_string())?;
                if digits == 6 {
                    return Err("braced Unicode escape contains too many digits".to_string());
                }
                self.bump()?;
                value = value * 16 + digit;
                digits += 1;
            }
            if digits == 0 {
                return Err("braced Unicode escape must contain a hex value".to_string());
            }
            if (0xD800..=0xDFFF).contains(&value) {
                return Err("surrogates are not valid braced Unicode escapes".to_string());
            }
            value
        } else {
            self.read_hex(/*digits*/ 4)?
        };

        let code = if (0xD800..=0xDBFF).contains(&code) {
            if !self.consume_ascii("\\u")? {
                return Err("lone high surrogate in workflow metadata string".to_string());
            }
            let low = self.read_hex(/*digits*/ 4)?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(
                    "invalid Unicode surrogate pair in workflow metadata string".to_string()
                );
            }
            0x1_0000 + ((code - 0xD800) << 10) + (low - 0xDC00)
        } else if (0xDC00..=0xDFFF).contains(&code) {
            return Err("lone low surrogate in workflow metadata string".to_string());
        } else {
            code
        };
        output.push(
            char::from_u32(code)
                .ok_or_else(|| "invalid Unicode code point in workflow metadata".to_string())?,
        );
        Ok(())
    }

    fn read_hex(&mut self, digits: usize) -> Result<u32, String> {
        let mut value = 0_u32;
        for _ in 0..digits {
            let digit = self
                .bump()?
                .and_then(|ch| ch.to_digit(16))
                .ok_or_else(|| "invalid hexadecimal escape in workflow metadata".to_string())?;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    fn expect_entry_end(&mut self, close: char) -> Result<(), String> {
        self.skip_trivia()?;
        if self.consume_char(',')? {
            return Ok(());
        }
        if self.peek() == Some(close) {
            return Ok(());
        }
        Err(format!("expected `,` or `{close}` in workflow metadata"))
    }

    fn expect_statement_end(&mut self) -> Result<(), String> {
        let crossed_line = self.skip_trivia()?;
        if self.consume_char(';')? {
            return Ok(());
        }
        match self.peek() {
            None => Ok(()),
            Some(next) if crossed_line && !self.starts_expression_continuation(next) => Ok(()),
            Some(_) => Err(
                "the `meta` object must end its statement before the workflow body; add `;` or a non-continuing line break"
                    .to_string(),
            ),
        }
    }

    fn starts_expression_continuation(&self, next: char) -> bool {
        let remaining = self.remaining();
        let operator = match next {
            '!' => remaining.starts_with("!="),
            '+' => !remaining.starts_with("++"),
            '-' => !remaining.starts_with("--"),
            '.' => !remaining
                .chars()
                .nth(1)
                .is_some_and(|ch| ch.is_ascii_digit()),
            '&' | '|' | '(' | '[' | '*' | '/' | '%' | '<' | '>' | '=' | '?' | ',' | '^' | '`' => {
                true
            }
            _ => false,
        };
        operator
            || ["in", "instanceof"].into_iter().any(|keyword| {
                self.remaining().strip_prefix(keyword).is_some_and(|rest| {
                    !rest.chars().next().is_some_and(|ch| {
                        is_identifier_continue(ch)
                            || ch == '\\'
                            || (!ch.is_ascii() && !ch.is_whitespace() && ch != '\u{feff}')
                    })
                })
            })
    }

    fn skip_trivia(&mut self) -> Result<bool, String> {
        let mut crossed_line = false;
        loop {
            self.guard()?;
            match self.peek() {
                Some(ch) if ch.is_whitespace() || ch == '\u{feff}' => {
                    crossed_line |= is_line_terminator(ch);
                    self.bump()?;
                }
                Some('/') if self.remaining().starts_with("//") => {
                    self.consume_ascii("//")?;
                    while let Some(ch) = self.peek() {
                        self.bump()?;
                        if is_line_terminator(ch) {
                            crossed_line = true;
                            break;
                        }
                    }
                }
                Some('/') if self.remaining().starts_with("/*") => {
                    self.consume_ascii("/*")?;
                    loop {
                        if self.consume_ascii("*/")? {
                            break;
                        }
                        let Some(ch) = self.bump()? else {
                            return Err(
                                "unterminated block comment before workflow metadata".to_string()
                            );
                        };
                        crossed_line |= is_line_terminator(ch);
                    }
                }
                _ => return Ok(crossed_line),
            }
        }
    }

    fn expect_identifier(&mut self, expected: &str) -> Result<(), String> {
        if self.parse_identifier()? == Some(expected) {
            Ok(())
        } else {
            Err(format!(
                "workflow scripts must begin with `export const meta`; expected `{expected}` at byte {}",
                self.pos
            ))
        }
    }

    fn parse_identifier(&mut self) -> Result<Option<&'a str>, String> {
        let start = self.pos;
        if !self.peek().is_some_and(is_identifier_start) {
            return Ok(None);
        }
        self.bump()?;
        while self.peek().is_some_and(is_identifier_continue) {
            self.bump()?;
        }
        Ok(Some(&self.source[start..self.pos]))
    }

    fn expect_char(&mut self, expected: char, message: &str) -> Result<(), String> {
        if self.consume_char(expected)? {
            Ok(())
        } else {
            Err(message.to_string())
        }
    }

    fn consume_char(&mut self, expected: char) -> Result<bool, String> {
        if self.peek() == Some(expected) {
            self.bump()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn consume_ascii(&mut self, expected: &str) -> Result<bool, String> {
        if !self.remaining().starts_with(expected) {
            return Ok(false);
        }
        self.pos += expected.len();
        self.guard()?;
        Ok(true)
    }

    fn bump(&mut self) -> Result<Option<char>, String> {
        self.guard()?;
        let Some(ch) = self.peek() else {
            return Ok(None);
        };
        self.pos += ch.len_utf8();
        self.guard()?;
        Ok(Some(ch))
    }

    fn peek(&self) -> Option<char> {
        self.remaining().chars().next()
    }

    fn remaining(&self) -> &'a str {
        &self.source[self.pos..]
    }

    fn guard(&self) -> Result<(), String> {
        if self.pos > WORKFLOW_META_MAX_BYTES {
            Err(format!(
                "workflow metadata exceeds the {WORKFLOW_META_MAX_BYTES}-byte scan limit"
            ))
        } else {
            Ok(())
        }
    }
}

fn is_identifier_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
}

fn is_identifier_continue(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()
}

fn is_line_terminator(ch: char) -> bool {
    matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

#[cfg(test)]
#[path = "workflow_meta_tests.rs"]
mod tests;
