//! Static parser for the leading `export const meta = { name, description, phases }`
//! manifest literal of a workflow script.
//!
//! Discovery (`core-workflows`) parses the `meta` manifest from potentially
//! untrusted files. That path must NEVER execute the workflow body, so this
//! parser is a dependency-light, hand-rolled scan over a restricted JavaScript
//! object-literal grammar. It mirrors [`crate::parse_exec_source`] in spirit:
//! pure string/AST-lite inspection, no V8 isolate, no `eval`.
//!
//! Only static literals are accepted for `meta` (string / number / boolean /
//! null / array / nested object literals). Any computed expression — variable
//! references, function calls, template strings, spreads, or computed keys —
//! is rejected with an actionable error. Everything after the `meta` object
//! literal (i.e. the workflow body) is never inspected.
//!
//! Because the input is untrusted, the scan is hardened against adversarial
//! inputs:
//! - it never allocates proportional to the whole file (the cursor iterates a
//!   `&str` in place and stops at the end of the `meta` statement);
//! - recursion into nested objects/arrays is bounded by [`MAX_DEPTH`] so deeply
//!   nested literals cannot overflow the stack; and
//! - the scanned manifest region is capped at [`MAX_MANIFEST_BYTES`] so a
//!   pathological manifest (e.g. a multi-megabyte string or comment) cannot
//!   force unbounded work.

/// Maximum nesting depth allowed inside the `meta` object literal. A chain of
/// containers deeper than this is rejected rather than recursed into, which
/// keeps the hand-rolled recursive-descent scan from overflowing the stack on
/// adversarial input (e.g. tens of thousands of nested arrays).
const MAX_DEPTH: usize = 32;

/// Hard cap on the number of source bytes the scan is allowed to consume while
/// reading the `meta` statement. The manifest region is tiny in practice; this
/// bound only exists to reject adversarial inputs (a huge string literal, a
/// giant comment, or a never-terminated construct) with a clear error instead
/// of scanning an arbitrarily large file.
const MAX_MANIFEST_BYTES: usize = 256 * 1024;

/// Parsed representation of a workflow's static `meta` manifest.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ParsedWorkflowMeta {
    /// Workflow name (required string literal).
    pub name: String,
    /// Human-readable description (required string literal).
    pub description: String,
    /// Declared phase titles, in declaration order (optional; empty when absent).
    pub phases: Vec<String>,
}

/// Restricted literal value produced while scanning the `meta` object.
#[derive(Clone, Debug, PartialEq)]
enum LiteralValue {
    String(String),
    Number(f64),
    Bool(bool),
    Null,
    Array(Vec<LiteralValue>),
    Object(Vec<(String, LiteralValue)>),
}

/// Parse the leading `export const meta = { ... }` object literal from a
/// workflow script WITHOUT executing (or even reading past) the body.
///
/// Returns [`ParsedWorkflowMeta`] on success, or an actionable error string
/// describing why the manifest is not a valid static literal.
pub fn parse_workflow_meta(source: &str) -> Result<ParsedWorkflowMeta, String> {
    let mut parser = Parser::new(source);
    parser.skip_trivia()?;

    // `export const meta`
    parser.expect_ident("export")?;
    parser.skip_trivia()?;
    parser.expect_ident("const")?;
    parser.skip_trivia()?;
    parser.expect_ident("meta")?;
    parser.skip_trivia()?;

    // Optional TypeScript type annotation: `meta: WorkflowMeta = { ... }`.
    if parser.peek() == Some(':') {
        parser.bump();
        // Skip the annotation up to the assignment `=`. Type annotations never
        // contain a top-level `=`, so this is safe for the manifest form.
        while let Some(ch) = parser.peek() {
            parser.guard()?;
            if ch == '=' {
                break;
            }
            parser.bump();
        }
        parser.skip_trivia()?;
    }

    if parser.peek() != Some('=') {
        return Err("expected `export const meta = { ... }`: missing `=` after `meta`".to_string());
    }
    parser.bump();

    let value = parser.parse_value(0)?;
    let LiteralValue::Object(entries) = value else {
        return Err(
            "`meta` must be a static object literal, e.g. `{ name: 'x', description: 'y', phases: [] }`"
                .to_string(),
        );
    };

    // The manifest must be a standalone statement. After the closing brace the
    // only legal continuations are whitespace/comments then an optional `;`,
    // then the end of the statement (EOF or a newline before the next
    // statement). Anything else — a trailing operator or expression such as
    // `{ ... } && buildMeta()` — must be rejected, otherwise a computed value
    // could masquerade as a static literal.
    parser.expect_statement_end()?;

    build_meta(entries)
}

/// Convert the parsed `meta` object entries into a validated [`ParsedWorkflowMeta`].
fn build_meta(entries: Vec<(String, LiteralValue)>) -> Result<ParsedWorkflowMeta, String> {
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut phases: Option<Vec<String>> = None;

    for (key, value) in entries {
        match key.as_str() {
            "name" => {
                let LiteralValue::String(text) = value else {
                    return Err("`meta.name` must be a string literal".to_string());
                };
                name = Some(text);
            }
            "description" => {
                let LiteralValue::String(text) = value else {
                    return Err("`meta.description` must be a string literal".to_string());
                };
                description = Some(text);
            }
            "phases" => {
                let LiteralValue::Array(items) = value else {
                    return Err("`meta.phases` must be an array of string literals".to_string());
                };
                let mut collected = Vec::with_capacity(items.len());
                for item in items {
                    let LiteralValue::String(text) = item else {
                        return Err("`meta.phases` must contain only string literals".to_string());
                    };
                    collected.push(text);
                }
                phases = Some(collected);
            }
            // Unknown literal keys are tolerated (still static), but ignored.
            _ => {}
        }
    }

    let name =
        name.ok_or_else(|| "`meta.name` is required and must be a string literal".to_string())?;
    let description = description
        .ok_or_else(|| "`meta.description` is required and must be a string literal".to_string())?;

    Ok(ParsedWorkflowMeta {
        name,
        description,
        phases: phases.unwrap_or_default(),
    })
}

/// Hand-rolled cursor over the source string.
///
/// The cursor holds a borrow of the source and a byte offset; it never
/// materializes the whole file into an owned buffer, so its memory footprint
/// stays proportional to the (bounded) manifest region it actually scans, not
/// to the size of the workflow body that follows.
struct Parser<'a> {
    source: &'a str,
    /// Current byte offset into `source`. Always kept on a UTF-8 char boundary
    /// because the cursor only ever advances by whole [`char`] widths.
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Self { source, pos: 0 }
    }

    /// Reject inputs that force the scan past the hard manifest-size cap.
    ///
    /// Called at the top of every unbounded scanning loop so that a
    /// never-terminated construct (huge string, giant comment, runaway
    /// identifier/number) fails fast with a clear error instead of consuming an
    /// arbitrarily large file.
    fn guard(&self) -> Result<(), String> {
        if self.pos > MAX_MANIFEST_BYTES {
            return Err(format!(
                "`meta` manifest is too large; the leading `export const meta = {{ ... }}` statement must be within {MAX_MANIFEST_BYTES} bytes"
            ));
        }
        Ok(())
    }

    fn peek(&self) -> Option<char> {
        self.source[self.pos..].chars().next()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.source[self.pos..].chars().nth(offset)
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.source[self.pos..].chars().next()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    /// Skip whitespace and `//` / `/* */` comments.
    ///
    /// Returns whether a line terminator was crossed while skipping, which the
    /// caller uses to apply JavaScript's automatic-semicolon-insertion rule
    /// when validating the end of the `meta` statement.
    fn skip_trivia(&mut self) -> Result<bool, String> {
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
    fn expect_ident(&mut self, expected: &str) -> Result<(), String> {
        match self.parse_ident()? {
            Some(ident) if ident == expected => Ok(()),
            Some(ident) => Err(format!(
                "expected `export const meta = {{ ... }}`: found `{ident}` where `{expected}` was expected"
            )),
            None => Err(format!(
                "expected `export const meta = {{ ... }}`: missing `{expected}`"
            )),
        }
    }

    /// After the top-level `meta` object literal, require the statement to end:
    /// whitespace/comments, an optional `;`, then EOF or a newline-separated
    /// next statement. Any trailing operator or expression is rejected.
    fn expect_statement_end(&mut self) -> Result<(), String> {
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
    fn parse_value(&mut self, depth: usize) -> Result<LiteralValue, String> {
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
                            "`meta` must contain only static literals; found non-literal `{other}` (variable reference, function call, or expression)"
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
                        "expected `:` after key `{key}` in `meta`; shorthand properties are not allowed"
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

    /// Parse a single- or double-quoted string literal, decoding escapes.
    fn parse_string(&mut self) -> Result<String, String> {
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
    fn parse_number(&mut self) -> Result<LiteralValue, String> {
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
        raw.parse::<f64>()
            .map(LiteralValue::Number)
            .map_err(|_| format!("invalid numeric literal `{raw}` in `meta`"))
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

fn is_ident_start(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    ch == '_' || ch == '$' || ch.is_ascii_alphanumeric()
}

fn is_line_terminator(ch: char) -> bool {
    matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn is_high_surrogate(code: u32) -> bool {
    (0xD800..=0xDBFF).contains(&code)
}

fn is_low_surrogate(code: u32) -> bool {
    (0xDC00..=0xDFFF).contains(&code)
}

#[cfg(test)]
mod tests {
    use super::MAX_DEPTH;
    use super::MAX_MANIFEST_BYTES;
    use super::ParsedWorkflowMeta;
    use super::parse_workflow_meta;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_valid_meta_with_phases_in_declared_order() {
        let source = "export const meta = {name:'x', description:'y', phases:['a','b']}\n\
                      export default async function () { await agent('hi'); }";
        assert_eq!(
            parse_workflow_meta(source).unwrap(),
            ParsedWorkflowMeta {
                name: "x".to_string(),
                description: "y".to_string(),
                phases: vec!["a".to_string(), "b".to_string()],
            }
        );
    }

    #[test]
    fn preserves_phase_declaration_order_not_sorted() {
        let source = "export const meta = { name: 'wf', description: 'd', phases: ['zeta', 'alpha', 'middle'] };";
        assert_eq!(
            parse_workflow_meta(source).unwrap().phases,
            vec![
                "zeta".to_string(),
                "alpha".to_string(),
                "middle".to_string()
            ]
        );
    }

    #[test]
    fn phases_default_to_empty_when_absent() {
        let source = "export const meta = { name: 'wf', description: 'no phases here' };";
        assert_eq!(
            parse_workflow_meta(source).unwrap(),
            ParsedWorkflowMeta {
                name: "wf".to_string(),
                description: "no phases here".to_string(),
                phases: vec![],
            }
        );
    }

    #[test]
    fn accepts_double_quotes_trailing_comma_and_comments() {
        let source = "// leading comment\n\
                      /* block */ export const meta = {\n\
                      \x20 name: \"triage\",\n\
                      \x20 description: \"Triage bugs\", // inline\n\
                      \x20 phases: [\"scan\", \"fix\",],\n\
                      };\n\
                      const x = 1;";
        assert_eq!(
            parse_workflow_meta(source).unwrap(),
            ParsedWorkflowMeta {
                name: "triage".to_string(),
                description: "Triage bugs".to_string(),
                phases: vec!["scan".to_string(), "fix".to_string()],
            }
        );
    }

    #[test]
    fn accepts_quoted_keys_and_typescript_annotation() {
        let source =
            "export const meta: WorkflowMeta = { 'name': 'n', \"description\": 'd', phases: [] };";
        assert_eq!(
            parse_workflow_meta(source).unwrap(),
            ParsedWorkflowMeta {
                name: "n".to_string(),
                description: "d".to_string(),
                phases: vec![],
            }
        );
    }

    #[test]
    fn decodes_string_escapes() {
        let source = r#"export const meta = { name: 'a\nb\tA', description: "he said \"hi\"" };"#;
        let meta = parse_workflow_meta(source).unwrap();
        assert_eq!(meta.name, "a\nb\tA");
        assert_eq!(meta.description, "he said \"hi\"");
    }

    #[test]
    fn ignores_unknown_literal_keys() {
        let source = "export const meta = { name: 'n', description: 'd', model: 'gpt', extra: 3 };";
        assert_eq!(
            parse_workflow_meta(source).unwrap(),
            ParsedWorkflowMeta {
                name: "n".to_string(),
                description: "d".to_string(),
                phases: vec![],
            }
        );
    }

    /// The grammar is deliberately broader than `{name, description, phases}`
    /// so that forward-compatible manifest fields (structured phase objects,
    /// `whenToUse`, etc.) still parse as static literals and are ignored rather
    /// than rejected.
    #[test]
    fn accepts_forward_compatible_nested_literal_fields() {
        let source = "export const meta = {\n\
                      \x20 name: 'n',\n\
                      \x20 description: 'd',\n\
                      \x20 whenToUse: 'when things break',\n\
                      \x20 phases: ['scan'],\n\
                      \x20 phaseDetails: [{ title: 'scan', tools: ['grep', 'read'], retries: 2 }],\n\
                      };";
        assert_eq!(
            parse_workflow_meta(source).unwrap(),
            ParsedWorkflowMeta {
                name: "n".to_string(),
                description: "d".to_string(),
                phases: vec!["scan".to_string()],
            }
        );
    }

    // --- Rejection cases -----------------------------------------------------

    #[test]
    fn rejects_missing_name() {
        let err = parse_workflow_meta("export const meta = { description: 'd' };").unwrap_err();
        assert!(err.contains("name"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_missing_description() {
        let err = parse_workflow_meta("export const meta = { name: 'n' };").unwrap_err();
        assert!(err.contains("description"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_function_call_meta() {
        let err = parse_workflow_meta("export const meta = buildMeta();").unwrap_err();
        assert!(err.contains("literal"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_variable_reference_value() {
        let err = parse_workflow_meta("export const meta = { name: NAME, description: 'd' };")
            .unwrap_err();
        assert!(err.contains("literal"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_template_string_value() {
        let err = parse_workflow_meta("export const meta = { name: `hi ${x}`, description: 'd' };")
            .unwrap_err();
        assert!(err.contains("template string"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_spread_in_object() {
        let err =
            parse_workflow_meta("export const meta = { ...base, name: 'n', description: 'd' };")
                .unwrap_err();
        assert!(err.contains("spread"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_computed_key() {
        let err = parse_workflow_meta("export const meta = { [dynamic]: 'n', description: 'd' };")
            .unwrap_err();
        assert!(err.contains("computed"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_shorthand_property() {
        let err =
            parse_workflow_meta("export const meta = { name, description: 'd' };").unwrap_err();
        assert!(err.contains("shorthand"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_non_string_name() {
        let err =
            parse_workflow_meta("export const meta = { name: 42, description: 'd' };").unwrap_err();
        assert!(err.contains("name"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_non_string_phase_entry() {
        let err = parse_workflow_meta(
            "export const meta = { name: 'n', description: 'd', phases: ['ok', 3] };",
        )
        .unwrap_err();
        assert!(err.contains("phases"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_function_call_inside_phase() {
        let err = parse_workflow_meta(
            "export const meta = { name: 'n', description: 'd', phases: [makePhase()] };",
        )
        .unwrap_err();
        assert!(err.contains("literal"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_missing_export_const_meta() {
        let err = parse_workflow_meta("const meta = { name: 'n', description: 'd' };").unwrap_err();
        assert!(err.contains("export"), "unexpected error: {err}");
    }

    /// The parser must be a pure static scan: a body with side-effecting or
    /// throwing code still parses `meta` successfully because the body is never
    /// evaluated (or even read past the manifest literal).
    #[test]
    fn parses_meta_without_evaluating_side_effecting_body() {
        let source = "export const meta = { name: 'safe', description: 'd', phases: ['p'] };\n\
                      throw new Error('boom');\n\
                      globalThis.__pwned = (function () { while (true) {} })();\n\
                      process.exit(1);";
        assert_eq!(
            parse_workflow_meta(source).unwrap(),
            ParsedWorkflowMeta {
                name: "safe".to_string(),
                description: "d".to_string(),
                phases: vec!["p".to_string()],
            }
        );
    }

    // --- Adversarial / hardening cases --------------------------------------

    /// Finding #1: deeply nested containers must be rejected by the depth cap
    /// rather than recursing until the stack overflows.
    #[test]
    fn rejects_nesting_deeper_than_the_limit() {
        // A chain of nested arrays far deeper than `MAX_DEPTH`, tucked under an
        // ignored key. This must fail fast, not stack-overflow.
        let depth = 50_000;
        let nested = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let source =
            format!("export const meta = {{ name: 'n', description: 'd', extra: {nested} }};");
        let err = parse_workflow_meta(&source).unwrap_err();
        assert!(err.contains("nested too deeply"), "unexpected error: {err}");
    }

    /// A modest amount of nesting (well under the cap) is still accepted, so
    /// legitimate structured manifest fields keep working.
    #[test]
    fn accepts_nesting_within_the_limit() {
        let depth = MAX_DEPTH - 2;
        let nested = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let source =
            format!("export const meta = {{ name: 'n', description: 'd', extra: {nested} }};");
        let meta = parse_workflow_meta(&source).unwrap();
        assert_eq!(meta.name, "n");
    }

    /// Finding #1/#4: a huge workflow body after a small manifest must not be
    /// scanned — the parser stops at the end of the `meta` statement, so this
    /// completes with bounded work despite the multi-megabyte body.
    #[test]
    fn does_not_scan_huge_body_after_manifest() {
        let body = "x".repeat(8 * 1024 * 1024);
        let source = format!(
            "export const meta = {{ name: 'n', description: 'd', phases: ['p'] }};\n{body}"
        );
        let meta = parse_workflow_meta(&source).unwrap();
        assert_eq!(meta.name, "n");
        assert_eq!(meta.phases, vec!["p".to_string()]);
    }

    /// Finding #1: a manifest region larger than the hard cap is rejected with
    /// a clear error instead of being scanned in full.
    #[test]
    fn rejects_manifest_region_larger_than_the_cap() {
        // A single string literal whose contents exceed the scan cap. The
        // closing quote lies past `MAX_MANIFEST_BYTES`, so the scan must abort.
        let filler = "a".repeat(MAX_MANIFEST_BYTES + 1024);
        let source = format!("export const meta = {{ name: 'n', description: '{filler}' }};");
        let err = parse_workflow_meta(&source).unwrap_err();
        assert!(err.contains("too large"), "unexpected error: {err}");
    }

    /// Finding #2: a value that continues past the object literal into a larger
    /// expression (`{ ... } && buildMeta()`) must be rejected — otherwise a
    /// computed value masquerades as a static literal.
    #[test]
    fn rejects_trailing_operator_after_object_literal() {
        let err = parse_workflow_meta(
            "export const meta = { name: 'n', description: 'd' } && buildMeta();",
        )
        .unwrap_err();
        assert!(err.contains("trailing token"), "unexpected error: {err}");
    }

    /// Finding #2: other same-line trailing continuations are likewise rejected.
    #[test]
    fn rejects_trailing_member_access_after_object_literal() {
        let err =
            parse_workflow_meta("export const meta = { name: 'n', description: 'd' }.valueOf();")
                .unwrap_err();
        assert!(err.contains("trailing token"), "unexpected error: {err}");
    }

    /// Re-review finding: ASI never fires before a continuation token, so an
    /// operator on the NEXT line still computes `meta` and must be rejected.
    #[test]
    fn rejects_newline_operator_continuation() {
        let err = parse_workflow_meta(
            "export const meta = { name: 'n', description: 'd' }\n&& buildMeta();",
        )
        .unwrap_err();
        assert!(err.contains("trailing token"), "unexpected error: {err}");
    }

    /// Re-review finding: member access across a newline likewise continues the
    /// expression (`{ ... }\n.valueOf()`).
    #[test]
    fn rejects_newline_member_access_continuation() {
        for continuation in [".valueOf()", "[0]", "(x)", "+ 1", "instanceof Foo", "`t`"] {
            let source =
                format!("export const meta = {{ name: 'n', description: 'd' }}\n{continuation};");
            let err = parse_workflow_meta(&source).unwrap_err();
            assert!(
                err.contains("trailing token"),
                "`{continuation}` was not rejected: {err}"
            );
        }
    }

    /// Re-review finding: a `//` comment ends at CR (and U+2028/U+2029), so a
    /// continuation hidden behind a CR-terminated comment is still rejected.
    #[test]
    fn rejects_continuation_after_cr_terminated_comment() {
        let err = parse_workflow_meta(
            "export const meta = { name: 'n', description: 'd' } //x\r&& buildMeta();",
        )
        .unwrap_err();
        assert!(err.contains("trailing token"), "unexpected error: {err}");
    }

    /// A genuine next statement after a newline (no `;` on the meta line) is
    /// still accepted — ASI applies when the next token cannot continue the
    /// expression.
    #[test]
    fn accepts_newline_separated_next_statement() {
        let meta = parse_workflow_meta(
            "export const meta = { name: 'n', description: 'd' }\nconst REPO = 'x';\nphase(REPO);",
        )
        .unwrap();
        assert_eq!(meta.name, "n");
    }

    /// An `in` prefix on an ordinary identifier (`inventory`) does not count as
    /// the relational `in` operator.
    #[test]
    fn accepts_identifier_with_in_prefix_after_newline() {
        let meta = parse_workflow_meta(
            "export const meta = { name: 'n', description: 'd' }\ninventory();",
        )
        .unwrap();
        assert_eq!(meta.name, "n");
    }

    /// Finding #3: bare `undefined` is a global reference, not a literal, and
    /// must be rejected rather than silently treated as `null`.
    #[test]
    fn rejects_undefined_value() {
        let err = parse_workflow_meta(
            "export const meta = { name: 'n', description: 'd', extra: undefined };",
        )
        .unwrap_err();
        assert!(err.contains("undefined"), "unexpected error: {err}");
        assert!(err.contains("literal"), "unexpected error: {err}");
    }

    /// `null` remains an accepted literal keyword.
    #[test]
    fn accepts_null_literal_value() {
        let meta = parse_workflow_meta(
            "export const meta = { name: 'n', description: 'd', extra: null };",
        )
        .unwrap();
        assert_eq!(meta.name, "n");
    }

    /// Finding #5: a lone high surrogate escape cannot form a valid character
    /// and must be rejected.
    #[test]
    fn rejects_lone_high_surrogate_escape() {
        let err =
            parse_workflow_meta(r#"export const meta = { name: '\uD83D', description: 'd' };"#)
                .unwrap_err();
        assert!(err.contains("surrogate"), "unexpected error: {err}");
    }

    /// Finding #5: a lone low surrogate escape is likewise rejected.
    #[test]
    fn rejects_lone_low_surrogate_escape() {
        let err =
            parse_workflow_meta(r#"export const meta = { name: '\uDE00', description: 'd' };"#)
                .unwrap_err();
        assert!(err.contains("surrogate"), "unexpected error: {err}");
    }

    /// Finding #5: a valid surrogate PAIR is combined into the astral code
    /// point it denotes (`😀` -> U+1F600, 😀).
    #[test]
    fn combines_valid_surrogate_pair_escape() {
        // `\uD83D\uDE00` is the UTF-16 surrogate-pair encoding of U+1F600 (😀).
        let meta = parse_workflow_meta(
            r#"export const meta = { name: '\uD83D\uDE00', description: 'd' };"#,
        )
        .unwrap();
        assert_eq!(meta.name, "\u{1F600}");
    }

    /// A `\u{...}` code-point escape for an astral char still works.
    #[test]
    fn decodes_braced_astral_code_point_escape() {
        let meta =
            parse_workflow_meta(r#"export const meta = { name: '\u{1F600}', description: 'd' };"#)
                .unwrap();
        assert_eq!(meta.name, "\u{1F600}");
    }
}
