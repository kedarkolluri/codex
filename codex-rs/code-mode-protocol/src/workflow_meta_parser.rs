use super::LiteralValue;
use super::MAX_DEPTH;
use super::MAX_MANIFEST_BYTES;
use super::truncate_for_error;

/// Hand-rolled cursor over the source string.
///
/// The cursor holds a borrow of the source and a byte offset; it never
/// materializes the whole file into an owned buffer, so its memory footprint
/// stays proportional to the (bounded) manifest region it actually scans, not
/// to the size of the workflow body that follows.
pub(super) struct Parser<'a> {
    pub(super) source: &'a str,
    /// Current byte offset into `source`. Always kept on a UTF-8 char boundary
    /// because the cursor only ever advances by whole [`char`] widths.
    pub(super) pos: usize,
}

impl<'a> Parser<'a> {
    pub(super) fn new(source: &'a str) -> Self {
        Self { source, pos: 0 }
    }

    /// Reject inputs that force the scan past the hard manifest-size cap.
    ///
    /// Called at the top of every unbounded scanning loop so that a
    /// never-terminated construct (huge string, giant comment, runaway
    /// identifier/number) fails fast with a clear error instead of consuming an
    /// arbitrarily large file.
    pub(super) fn guard(&self) -> Result<(), String> {
        if self.pos > MAX_MANIFEST_BYTES {
            return Err(format!(
                "`meta` manifest is too large; the leading `export const meta = {{ ... }}` statement must be within {MAX_MANIFEST_BYTES} bytes"
            ));
        }
        Ok(())
    }

    pub(super) fn peek(&self) -> Option<char> {
        self.source[self.pos..].chars().next()
    }

    pub(super) fn peek_at(&self, offset: usize) -> Option<char> {
        self.source[self.pos..].chars().nth(offset)
    }

    pub(super) fn bump(&mut self) -> Option<char> {
        let ch = self.source[self.pos..].chars().next()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    /// Skip whitespace and `//` / `/* */` comments.
    ///
    /// Returns whether a line terminator was crossed while skipping, which the
    /// caller uses to apply JavaScript's automatic-semicolon-insertion rule
    /// when validating the end of the `meta` statement.
    pub(super) fn skip_trivia(&mut self) -> Result<bool, String> {
        let mut saw_newline = false;
        loop {
            self.guard()?;
            match self.peek() {
                Some(ch) if ch.is_whitespace() => {
                    if is_line_terminator(ch) {
                        saw_newline = true;
                    }
                    self.bump();
                }
                Some('/') if self.peek_at(1) == Some('/') => {
                    while let Some(ch) = self.peek() {
                        self.guard()?;
                        self.bump();
                        // A `//` comment ends at any JS LineTerminator (LF, CR,
                        // U+2028, U+2029), not just LF — otherwise a CR-only
                        // line break would hide the following tokens inside the
                        // comment.
                        if is_line_terminator(ch) {
                            saw_newline = true;
                            break;
                        }
                    }
                }
                Some('/') if self.peek_at(1) == Some('*') => {
                    self.bump();
                    self.bump();
                    loop {
                        self.guard()?;
                        match self.bump() {
                            Some('*') if self.peek() == Some('/') => {
                                self.bump();
                                break;
                            }
                            Some(ch) => {
                                if is_line_terminator(ch) {
                                    saw_newline = true;
                                }
                            }
                            None => break,
                        }
                    }
                }
                _ => break,
            }
        }
        Ok(saw_newline)
    }

    /// Read a JS identifier (`[A-Za-z_$][A-Za-z0-9_$]*`) if present.
    fn parse_ident(&mut self) -> Result<Option<String>, String> {
        let start = self.pos;
        match self.peek() {
            Some(ch) if is_ident_start(ch) => {
                self.bump();
            }
            _ => return Ok(None),
        }
        while let Some(ch) = self.peek() {
            self.guard()?;
            if is_ident_continue(ch) {
                self.bump();
            } else {
                break;
            }
        }
        Ok(Some(self.source[start..self.pos].to_string()))
    }

    /// Require the given identifier keyword next.
    pub(super) fn expect_ident(&mut self, expected: &str) -> Result<(), String> {
        match self.parse_ident()? {
            Some(ident) if ident == expected => Ok(()),
            Some(ident) => Err(format!(
                "expected `export const meta = {{ ... }}`: found `{}` where `{expected}` was expected",
                truncate_for_error(&ident)
            )),
            None => Err(format!(
                "expected `export const meta = {{ ... }}`: missing `{expected}`"
            )),
        }
    }

    /// After the top-level `meta` object literal, require the statement to end:
    /// whitespace/comments, an optional `;`, then EOF or a newline-separated
    /// next statement. Any trailing operator or expression is rejected.
    pub(super) fn expect_statement_end(&mut self) -> Result<(), String> {
        let saw_newline = self.skip_trivia()?;
        match self.peek() {
            // Explicit terminator or end of input: the statement is complete.
            Some(';') | None => Ok(()),
            // A newline (or a comment spanning one) only ends the statement if
            // the next token cannot continue the expression — JavaScript's
            // automatic semicolon insertion never fires before a continuation
            // token, so `{ ... }\n&& buildMeta()` or `{ ... }\n.valueOf()` is
            // still a computed `meta`, not a static literal.
            Some(ch) if saw_newline && !self.at_expression_continuation(ch) => Ok(()),
            // Anything else continues the `meta` expression (e.g.
            // `{ ... } && buildMeta()`), which would make `meta` a computed
            // value rather than a static literal.
            Some(ch) => Err(format!(
                "unexpected trailing token `{ch}` after the `meta` object literal; \
                 `meta` must be a standalone static literal (`export const meta = {{ ... }};`), \
                 not part of a larger expression"
            )),
        }
    }

    /// Whether the upcoming token can syntactically continue the preceding
    /// `meta` object-literal expression across a line break (which suppresses
    /// automatic semicolon insertion).
    fn at_expression_continuation(&self, next: char) -> bool {
        const CONTINUATION_CHARS: &[char] = &[
            '&', '|', '.', '(', '[', '+', '-', '*', '/', '%', '<', '>', '=', '?', ',', '^', '~',
            ':', '`', '!',
        ];
        if CONTINUATION_CHARS.contains(&next) {
            return true;
        }
        // The relational keyword operators also continue an expression.
        for keyword in ["in", "instanceof"] {
            let rest = &self.source[self.pos..];
            if let Some(after) = rest.strip_prefix(keyword)
                && !after.chars().next().is_some_and(is_ident_continue)
            {
                return true;
            }
        }
        false
    }

    /// Parse a single restricted literal value at the given nesting `depth`.
    pub(super) fn parse_value(&mut self, depth: usize) -> Result<LiteralValue, String> {
        if depth > MAX_DEPTH {
            return Err(format!(
                "`meta` is nested too deeply; nesting must not exceed {MAX_DEPTH} levels"
            ));
        }
        self.skip_trivia()?;
        match self.peek() {
            Some('{') => self.parse_object(depth),
            Some('[') => self.parse_array(depth),
            Some('\'') | Some('"') => Ok(LiteralValue::String(self.parse_string()?)),
            Some('`') => Err(
                "template strings are not allowed in `meta`; use a plain string literal"
                    .to_string(),
            ),
            Some('.') if self.peek_at(1) == Some('.') && self.peek_at(2) == Some('.') => {
                Err("spread (`...`) is not allowed in `meta`".to_string())
            }
            Some(ch) if ch == '-' || ch == '+' || ch.is_ascii_digit() => self.parse_number(),
            Some(_) => {
                // Any bare identifier here is a reference / call, which is not a
                // static literal — except the reserved literal keywords.
                match self.parse_ident()? {
                    Some(ident) => match ident.as_str() {
                        "true" => Ok(LiteralValue::Bool(true)),
                        "false" => Ok(LiteralValue::Bool(false)),
                        "null" => Ok(LiteralValue::Null),
                        // `undefined` is a global binding, not a literal keyword.
                        // Treating it as `null` would silently accept a
                        // non-literal reference, so reject it explicitly.
                        other => Err(format!(
                            "`meta` must contain only static literals; found non-literal `{}` (variable reference, function call, or expression)",
                            truncate_for_error(other)
                        )),
                    },
                    None => Err(
                        "`meta` must contain only static literals; found an unexpected token"
                            .to_string(),
                    ),
                }
            }
            None => Err("unexpected end of input while parsing `meta`".to_string()),
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<LiteralValue, String> {
        // Consume '{'.
        self.bump();
        let mut entries: Vec<(String, LiteralValue)> = Vec::new();
        loop {
            self.guard()?;
            self.skip_trivia()?;
            match self.peek() {
                Some('}') => {
                    self.bump();
                    break;
                }
                Some('.') => {
                    return Err("spread (`...`) is not allowed in `meta`".to_string());
                }
                Some('[') => {
                    return Err("computed object keys are not allowed in `meta`".to_string());
                }
                None => {
                    return Err("unexpected end of input while parsing `meta` object".to_string());
                }
                _ => {}
            }

            // Key: string literal or bare identifier.
            let key = match self.peek() {
                Some('\'') | Some('"') => self.parse_string()?,
                _ => self.parse_ident()?.ok_or_else(|| {
                    "expected an object key (identifier or string literal) in `meta`".to_string()
                })?,
            };

            self.skip_trivia()?;
            match self.peek() {
                Some(':') => {
                    self.bump();
                }
                Some('(') => {
                    return Err(
                        "object methods are not allowed in `meta`; use a plain value".to_string(),
                    );
                }
                _ => {
                    // Shorthand `{ name }` is a variable reference, not a literal.
                    return Err(format!(
                        "expected `:` after key `{}` in `meta`; shorthand properties are not allowed",
                        truncate_for_error(&key)
                    ));
                }
            }

            let value = self.parse_value(depth + 1)?;
            entries.push((key, value));

            self.skip_trivia()?;
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some('}') => {
                    self.bump();
                    break;
                }
                _ => {
                    return Err("expected `,` or `}` in `meta` object literal".to_string());
                }
            }
        }
        Ok(LiteralValue::Object(entries))
    }

    fn parse_array(&mut self, depth: usize) -> Result<LiteralValue, String> {
        // Consume '['.
        self.bump();
        let mut items: Vec<LiteralValue> = Vec::new();
        loop {
            self.guard()?;
            self.skip_trivia()?;
            match self.peek() {
                Some(']') => {
                    self.bump();
                    break;
                }
                Some('.') => {
                    return Err("spread (`...`) is not allowed in `meta`".to_string());
                }
                None => {
                    return Err("unexpected end of input while parsing `meta` array".to_string());
                }
                _ => {}
            }

            let value = self.parse_value(depth + 1)?;
            items.push(value);

            self.skip_trivia()?;
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(']') => {
                    self.bump();
                    break;
                }
                _ => {
                    return Err("expected `,` or `]` in `meta` array literal".to_string());
                }
            }
        }
        Ok(LiteralValue::Array(items))
    }
}

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
}

pub(super) fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()
}

fn is_line_terminator(ch: char) -> bool {
    matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}
