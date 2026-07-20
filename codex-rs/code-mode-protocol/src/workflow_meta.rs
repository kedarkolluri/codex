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

#[path = "workflow_meta_literal_parser.rs"]
mod literal_parser;
#[path = "workflow_meta_parser.rs"]
mod parser;

use self::parser::Parser;

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

/// Maximum number of characters of an offending identifier/number/key that is
/// echoed back into an error message.
///
/// Error strings from this parser are surfaced to the model as tool results (see
/// `CodeModeWorkflowHandler`). Because the scanned manifest region is only
/// bounded by [`MAX_MANIFEST_BYTES`] (256 KiB), a single adversarial identifier
/// or number could otherwise be reproduced verbatim into a ~60K-token error
/// string that poisons the model context. Any offending token longer than this
/// is truncated with an ellipsis marker.
const MAX_ERROR_TOKEN_CHARS: usize = 80;

/// Cap an offending identifier/number/key echoed into an error message so a
/// pathological input cannot inflate the (model-visible) error string. Longer
/// tokens are truncated to [`MAX_ERROR_TOKEN_CHARS`] characters plus an ellipsis
/// marker.
fn truncate_for_error(token: &str) -> String {
    let mut chars = token.chars();
    let truncated: String = chars.by_ref().take(MAX_ERROR_TOKEN_CHARS).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

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

    let meta = ParsedWorkflowMeta {
        name,
        description,
        phases: phases.unwrap_or_default(),
    };
    crate::workflow_bounds::ensure_parsed_workflow_meta(&meta)?;
    Ok(meta)
}

#[cfg(test)]
#[path = "workflow_meta_parsing_tests.rs"]
mod parsing_tests;

#[cfg(test)]
#[path = "workflow_meta_rejection_tests.rs"]
mod rejection_tests;

#[cfg(test)]
#[path = "workflow_meta_hardening_tests.rs"]
mod hardening_tests;
