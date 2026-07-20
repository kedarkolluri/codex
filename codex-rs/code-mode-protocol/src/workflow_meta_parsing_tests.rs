use super::MAX_DEPTH;
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

/// A modest amount of nesting (well under the cap) is still accepted, so
/// legitimate structured manifest fields keep working.
#[test]
fn accepts_nesting_within_the_limit() {
    let depth = MAX_DEPTH - 2;
    let nested = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
    let source = format!("export const meta = {{ name: 'n', description: 'd', extra: {nested} }};");
    let meta = parse_workflow_meta(&source).unwrap();
    assert_eq!(meta.name, "n");
}

/// Finding #1/#4: a huge workflow body after a small manifest must not be
/// scanned — the parser stops at the end of the `meta` statement, so this
/// completes with bounded work despite the multi-megabyte body.
#[test]
fn does_not_scan_huge_body_after_manifest() {
    let body = "x".repeat(8 * 1024 * 1024);
    let source =
        format!("export const meta = {{ name: 'n', description: 'd', phases: ['p'] }};\n{body}");
    let meta = parse_workflow_meta(&source).unwrap();
    assert_eq!(meta.name, "n");
    assert_eq!(meta.phases, vec!["p".to_string()]);
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
    let meta =
        parse_workflow_meta("export const meta = { name: 'n', description: 'd' }\ninventory();")
            .unwrap();
    assert_eq!(meta.name, "n");
}

/// `null` remains an accepted literal keyword.
#[test]
fn accepts_null_literal_value() {
    let meta =
        parse_workflow_meta("export const meta = { name: 'n', description: 'd', extra: null };")
            .unwrap();
    assert_eq!(meta.name, "n");
}

/// Finding #5: a valid surrogate PAIR is combined into the astral code
/// point it denotes (`😀` -> U+1F600, 😀).
#[test]
fn combines_valid_surrogate_pair_escape() {
    // `\uD83D\uDE00` is the UTF-16 surrogate-pair encoding of U+1F600 (😀).
    let meta =
        parse_workflow_meta(r#"export const meta = { name: '\uD83D\uDE00', description: 'd' };"#)
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
