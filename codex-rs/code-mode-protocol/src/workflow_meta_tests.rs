use super::ParsedWorkflowMeta;
use super::WORKFLOW_META_MAX_BYTES;
use super::parse_workflow_meta;
use crate::WORKFLOW_AGENT_OPTION_MAX_BYTES;
use crate::WORKFLOW_DESCRIPTION_MAX_BYTES;
use crate::WORKFLOW_NAME_MAX_BYTES;
use crate::WORKFLOW_PHASE_TITLE_MAX_BYTES;
use crate::WORKFLOW_PHASES_MAX_ITEMS;
use pretty_assertions::assert_eq;

#[test]
fn parses_string_phases_in_declaration_order() {
    assert_eq!(
        parse_workflow_meta(
            "export const meta = {name:'triage', description:'Triage bugs', phases:['scan','fix']}"
        ),
        Ok(ParsedWorkflowMeta {
            name: "triage".to_string(),
            description: "Triage bugs".to_string(),
            phases: vec!["scan".to_string(), "fix".to_string()],
        })
    );
    assert_eq!(
        parse_workflow_meta("export const meta = { name: 'n', description: 'd' }")
            .unwrap()
            .phases,
        Vec::<String>::new()
    );
}

#[test]
fn parses_claude_phase_objects_and_preserves_mixed_order() {
    let source = r#"export const meta = {
        name: 'triage',
        description: 'Triage bugs',
        phases: [
            { detail: 'fan out scanners', title: 'Scan' },
            'Fix',
            { 'title': 'Prove', model: 'claude-sonnet', },
            { title: 'Report' },
        ],
    }
    throw new Error('the body is not metadata');"#;
    assert_eq!(
        parse_workflow_meta(source),
        Ok(ParsedWorkflowMeta {
            name: "triage".to_string(),
            description: "Triage bugs".to_string(),
            phases: vec![
                "Scan".to_string(),
                "Fix".to_string(),
                "Prove".to_string(),
                "Report".to_string(),
            ],
        })
    );
}

#[test]
fn supports_comments_quoted_keys_escapes_and_trailing_commas() {
    let source = r#"﻿// leading
        export /* keyword */ const meta = {
            'name': 'a\x2d\u0042\u{1F600}\uD83D\uDE00',
            "description": "braces } and /* comments */ stay text",
            phases: ["line\nfeed",],
        }
    "#;
    assert_eq!(
        parse_workflow_meta(source),
        Ok(ParsedWorkflowMeta {
            name: "a-B😀😀".to_string(),
            description: "braces } and /* comments */ stay text".to_string(),
            phases: vec!["line\nfeed".to_string()],
        })
    );
}

#[test]
fn stops_before_side_effecting_or_huge_body() {
    let body = "x".repeat(WORKFLOW_META_MAX_BYTES * 2);
    let source = format!(
        "export const meta = {{ name: 'safe', description: 'd', phases: ['p'] }};\nthrow new Error('boom');{body}"
    );
    assert_eq!(parse_workflow_meta(&source).unwrap().name, "safe");

    let asi_source = "export const meta = { name: 'safe', description: 'd' }
        inventory(); globalThis.compromised = true; while (true) {}";
    assert_eq!(parse_workflow_meta(asi_source).unwrap().name, "safe");
}

#[test]
fn rejects_malformed_declarations_and_missing_required_fields() {
    let cases = [
        ("const meta = {}", "export"),
        ("export let meta = {}", "const"),
        ("export const metadata = {}", "meta"),
        ("export const meta: WorkflowMeta = {}", "expected `=`"),
        (
            "export const meta = { description: 'd' }",
            "`meta.name` is required",
        ),
        (
            "export const meta = { name: 'n' }",
            "`meta.description` is required",
        ),
    ];
    for (source, expected) in cases {
        let error = parse_workflow_meta(source).unwrap_err();
        assert!(error.contains(expected), "{source:?}: {error}");
    }
}

#[test]
fn rejects_computed_or_unsupported_metadata() {
    let cases = [
        ("export const meta = buildMeta();", "static object"),
        ("export const meta = META;", "static object"),
        (
            "export const meta = { name: 'n', description: DESCRIPTION };",
            "quoted string",
        ),
        (
            "export const meta = { name: `n`, description: 'd' };",
            "template strings",
        ),
        (
            "export const meta = { ...base, name: 'n', description: 'd' };",
            "spread",
        ),
        (
            "export const meta = { [key]: 'n', description: 'd' };",
            "computed",
        ),
        (
            "export const meta = { name, description: 'd' };",
            "require `:`",
        ),
        (
            "export const meta = { name: 'n', description: 'd', extra: null };",
            "unsupported workflow metadata",
        ),
        (
            "export const meta = { name: 'n', name: 'again', description: 'd' };",
            "duplicate `meta.name`",
        ),
    ];
    for (source, expected) in cases {
        let error = parse_workflow_meta(source).unwrap_err();
        assert!(error.contains(expected), "{source:?}: {error}");
    }
}

#[test]
fn rejects_invalid_phase_shapes() {
    let cases = [
        ("phases: value", "array"),
        ("phases: [1]", "quoted string"),
        ("phases: ['valid', 1]", "quoted string"),
        ("phases: [, 'p']", "quoted string"),
        ("phases: [buildPhase()]", "quoted string"),
        ("phases: [`template`]", "template strings"),
        ("phases: [{}]", "title` is required"),
        ("phases: [{ detail: 'd' }]", "title` is required"),
        ("phases: [{ title: '  ' }]", "must not be empty"),
        ("phases: [{ title: value }]", "quoted string"),
        ("phases: [{ title: 'p', detail: value }]", "quoted string"),
        (
            "phases: [{ title: 'p', model: chooseModel() }]",
            "quoted string",
        ),
        (
            "phases: [{ title: 'a', 'title': 'b' }]",
            "duplicate `meta.phases[].title`",
        ),
        (
            "phases: [{ title: 'p', detail: 'a', detail: 'b' }]",
            "duplicate `meta.phases[].detail`",
        ),
        (
            "phases: [{ title: 'p', model: 'a', model: 'b' }]",
            "duplicate `meta.phases[].model`",
        ),
        ("phases: [{ title: 'p', subtitle: 's' }]", "supports only"),
        ("phases: [{ ...phase }]", "spread"),
        ("phases: [{ [key]: 'p' }]", "computed"),
        ("phases: [{ title }]", "require `:`"),
        ("phases: [{ title() {} }]", "require `:`"),
        (r#"phases: [{ title: `template` }]"#, "template strings"),
    ];
    for (phases, expected) in cases {
        let source = format!("export const meta = {{ name: 'n', description: 'd', {phases} }};");
        let error = parse_workflow_meta(&source).unwrap_err();
        assert!(error.contains(expected), "{source:?}: {error}");
    }
}

#[test]
fn rejects_expression_continuations_after_the_manifest() {
    for continuation in [
        "&& buildMeta()",
        ".valueOf()",
        "[0]",
        "(value)",
        "+ 1",
        "- 1",
        "!= other",
        "in value",
        "in\u{a0}value",
        "instanceof Object",
        "instanceof\u{feff}ObjectType",
        "`tagged`",
    ] {
        for separator in [" ", "\n"] {
            let source = format!(
                "export const meta = {{ name: 'n', description: 'd' }}{separator}{continuation};"
            );
            let error = parse_workflow_meta(&source).unwrap_err();
            assert!(error.contains("end its statement"), "{source:?}: {error}");
        }
    }
}

#[test]
fn accepts_asi_before_prefix_body_statements() {
    let expected = ParsedWorkflowMeta {
        name: "n".to_string(),
        description: "d".to_string(),
        phases: Vec::new(),
    };
    for body in [
        "!foo()",
        "~value",
        "++value",
        "--value",
        ".1",
        "inπ()",
        r"in\u0066oo()",
        "instanceofπ()",
        r"instanceof\u0054hing()",
    ] {
        let source = format!("export const meta = {{ name: 'n', description: 'd' }}\n{body};");
        assert_eq!(
            parse_workflow_meta(&source),
            Ok(expected.clone()),
            "{source:?}"
        );
    }
}

#[test]
fn rejects_malformed_strings_comments_and_escapes() {
    let cases = [
        ("/* never closed", "unterminated block comment"),
        (
            "export const meta = { name: 'never closed",
            "unterminated string",
        ),
        (
            "export const meta = { name: 'raw\nline', description: 'd' };",
            "raw line terminators",
        ),
        (
            r#"export const meta = { name: '\xGG', description: 'd' };"#,
            "hexadecimal",
        ),
        (
            r#"export const meta = { name: '\uD83D', description: 'd' };"#,
            "high surrogate",
        ),
        (
            r#"export const meta = { name: '\uDE00', description: 'd' };"#,
            "low surrogate",
        ),
        (
            r#"export const meta = { name: '\u{D800}', description: 'd' };"#,
            "surrogate",
        ),
    ];
    for (source, expected) in cases {
        let error = parse_workflow_meta(source).unwrap_err();
        assert!(error.contains(expected), "{source:?}: {error}");
    }
}

#[test]
fn enforces_metadata_field_and_collection_bounds() {
    let exact_name = "n".repeat(WORKFLOW_NAME_MAX_BYTES);
    assert!(parse_workflow_meta(&source(&exact_name, "d", "[]")).is_ok());
    let long_name = format!("{exact_name}n");
    assert!(parse_workflow_meta(&source(&long_name, "d", "[]")).is_err());

    let exact_description = "d".repeat(WORKFLOW_DESCRIPTION_MAX_BYTES);
    assert!(parse_workflow_meta(&source("n", &exact_description, "[]")).is_ok());
    let long_description = format!("{exact_description}d");
    assert!(parse_workflow_meta(&source("n", &long_description, "[]")).is_err());

    let exact_title = "p".repeat(WORKFLOW_PHASE_TITLE_MAX_BYTES);
    let phases = format!("['{exact_title}']");
    assert!(parse_workflow_meta(&source("n", "d", &phases)).is_ok());
    let phases = format!("['{exact_title}p']");
    assert!(parse_workflow_meta(&source("n", "d", &phases)).is_err());
    assert!(parse_workflow_meta(&source("n", "d", "['  ']")).is_err());

    let phases = format!("[{{ title: '{exact_title}' }}]");
    assert!(parse_workflow_meta(&source("n", "d", &phases)).is_ok());
    let phases = format!("[{{ title: '{exact_title}p' }}]");
    assert!(parse_workflow_meta(&source("n", "d", &phases)).is_err());

    let exact_detail = "d".repeat(WORKFLOW_DESCRIPTION_MAX_BYTES);
    let phases = format!("[{{ title: 'p', detail: '{exact_detail}' }}]");
    assert!(parse_workflow_meta(&source("n", "d", &phases)).is_ok());
    let phases = format!("[{{ title: 'p', detail: '{exact_detail}d' }}]");
    assert!(parse_workflow_meta(&source("n", "d", &phases)).is_err());

    let exact_model = "m".repeat(WORKFLOW_AGENT_OPTION_MAX_BYTES);
    let phases = format!("[{{ title: 'p', model: '{exact_model}' }}]");
    assert!(parse_workflow_meta(&source("n", "d", &phases)).is_ok());
    let phases = format!("[{{ title: 'p', model: '{exact_model}m' }}]");
    assert!(parse_workflow_meta(&source("n", "d", &phases)).is_err());

    let exact_count = format!(
        "[{},{{ title: 'last' }}]",
        vec!["'p'"; WORKFLOW_PHASES_MAX_ITEMS - 1].join(",")
    );
    assert_eq!(
        parse_workflow_meta(&source("n", "d", &exact_count))
            .unwrap()
            .phases
            .len(),
        WORKFLOW_PHASES_MAX_ITEMS
    );
    let over_count = format!(
        "[{},{{ title: 'last' }},{{ title: 'over' }}]",
        vec!["'p'"; WORKFLOW_PHASES_MAX_ITEMS - 1].join(",")
    );
    assert!(parse_workflow_meta(&source("n", "d", &over_count)).is_err());
}

#[test]
fn bounds_manifest_scanning_and_diagnostics() {
    let huge_comment = format!("/*{}", "x".repeat(WORKFLOW_META_MAX_BYTES));
    let error = parse_workflow_meta(&huge_comment).unwrap_err();
    assert!(error.contains("scan limit"), "{error}");
    assert!(error.len() < 256, "unbounded error: {} bytes", error.len());

    let huge_identifier = "x".repeat(WORKFLOW_META_MAX_BYTES / 2);
    let source = format!("export const {huge_identifier} = {{}};");
    let error = parse_workflow_meta(&source).unwrap_err();
    assert!(error.len() < 256, "unbounded error: {} bytes", error.len());
    assert!(!error.contains(&"x".repeat(512)));
}

fn source(name: &str, description: &str, phases: &str) -> String {
    format!(
        "export const meta = {{ name: '{name}', description: '{description}', phases: {phases} }};"
    )
}
