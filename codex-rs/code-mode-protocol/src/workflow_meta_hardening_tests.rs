use super::MAX_MANIFEST_BYTES;
use super::parse_workflow_meta;

/// Finding #1: deeply nested containers must be rejected by the depth cap
/// rather than recursing until the stack overflows.
#[test]
fn rejects_nesting_deeper_than_the_limit() {
    // A chain of nested arrays far deeper than `MAX_DEPTH`, tucked under an
    // ignored key. This must fail fast, not stack-overflow.
    let depth = 50_000;
    let nested = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    let source = format!("export const meta = {{ name: 'n', description: 'd', extra: {nested} }};");
    let err = parse_workflow_meta(&source).unwrap_err();
    assert!(err.contains("nested too deeply"), "unexpected error: {err}");
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
    let err =
        parse_workflow_meta("export const meta = { name: 'n', description: 'd' } && buildMeta();")
            .unwrap_err();
    assert!(err.contains("trailing token"), "unexpected error: {err}");
}

/// Finding #2: other same-line trailing continuations are likewise rejected.
#[test]
fn rejects_trailing_member_access_after_object_literal() {
    let err = parse_workflow_meta("export const meta = { name: 'n', description: 'd' }.valueOf();")
        .unwrap_err();
    assert!(err.contains("trailing token"), "unexpected error: {err}");
}

/// Re-review finding: ASI never fires before a continuation token, so an
/// operator on the NEXT line still computes `meta` and must be rejected.
#[test]
fn rejects_newline_operator_continuation() {
    let err =
        parse_workflow_meta("export const meta = { name: 'n', description: 'd' }\n&& buildMeta();")
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

/// Finding #5: a lone high surrogate escape cannot form a valid character
/// and must be rejected.
#[test]
fn rejects_lone_high_surrogate_escape() {
    let err = parse_workflow_meta(r#"export const meta = { name: '\uD83D', description: 'd' };"#)
        .unwrap_err();
    assert!(err.contains("surrogate"), "unexpected error: {err}");
}

/// Finding #5: a lone low surrogate escape is likewise rejected.
#[test]
fn rejects_lone_low_surrogate_escape() {
    let err = parse_workflow_meta(r#"export const meta = { name: '\uDE00', description: 'd' };"#)
        .unwrap_err();
    assert!(err.contains("surrogate"), "unexpected error: {err}");
}

/// Re-review finding: an offending identifier echoed into an error message
/// must be capped, not reproduced verbatim. A ~256 KiB identifier would
/// otherwise become a ~60K-token error string surfaced to the model.
#[test]
fn caps_offending_non_literal_identifier_in_error_message() {
    let huge = "a".repeat(200_000);
    let source = format!("export const meta = {{ name: 'n', description: 'd', extra: {huge} }};");
    let err = parse_workflow_meta(&source).unwrap_err();
    assert!(err.contains('…'), "expected an ellipsis marker: {err}");
    assert!(err.contains("literal"), "unexpected error: {err}");
    assert!(
        err.len() < 512,
        "error must stay bounded, got {} bytes",
        err.len()
    );
    assert!(
        !err.contains(&"a".repeat(super::MAX_ERROR_TOKEN_CHARS + 1)),
        "offending identifier must not be echoed verbatim: {err}"
    );
}

/// The wrong keyword after `export const` is likewise capped in the error.
#[test]
fn caps_offending_keyword_in_error_message() {
    let huge = "x".repeat(200_000);
    let source = format!("export const {huge} = 1;");
    let err = parse_workflow_meta(&source).unwrap_err();
    assert!(err.contains('…'), "expected an ellipsis marker: {err}");
    assert!(
        err.len() < 512,
        "error must stay bounded, got {} bytes",
        err.len()
    );
}

/// A shorthand property whose key is pathologically long is capped in the
/// error rather than echoed in full.
#[test]
fn caps_offending_shorthand_key_in_error_message() {
    let huge = "k".repeat(200_000);
    let source = format!("export const meta = {{ {huge} }};");
    let err = parse_workflow_meta(&source).unwrap_err();
    assert!(err.contains("shorthand"), "unexpected error: {err}");
    assert!(err.contains('…'), "expected an ellipsis marker: {err}");
    assert!(
        err.len() < 512,
        "error must stay bounded, got {} bytes",
        err.len()
    );
}

/// A short offending token is echoed unchanged (no spurious ellipsis).
#[test]
fn short_offending_token_is_not_truncated() {
    let err =
        parse_workflow_meta("export const meta = { name: NAME, description: 'd' };").unwrap_err();
    assert!(err.contains("NAME"), "short token should be echoed: {err}");
    assert!(
        !err.contains('…'),
        "short token should not be truncated: {err}"
    );
}
