use super::parse_workflow_meta;

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
    let err =
        parse_workflow_meta("export const meta = { name: NAME, description: 'd' };").unwrap_err();
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
    let err = parse_workflow_meta("export const meta = { ...base, name: 'n', description: 'd' };")
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
    let err = parse_workflow_meta("export const meta = { name, description: 'd' };").unwrap_err();
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
