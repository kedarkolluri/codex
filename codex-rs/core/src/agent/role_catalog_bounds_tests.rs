use super::*;
use crate::agent::role::spawn_tool_spec;
use crate::config::AgentRoleConfig;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

#[test]
fn catalog_preserves_small_entries_byte_for_byte() {
    let catalog = bound_role_catalog([
        "alpha: no description".to_string(),
        "beta: {\nline one\nline two\n}".to_string(),
    ]);

    assert_eq!(
        catalog,
        "Available roles:\nalpha: no description\nbeta: {\nline one\nline two\n}"
    );
}

#[test]
fn catalog_bounds_entry_count_with_an_explicit_omission() {
    let entries = (0..40).map(|index| format!("entry-{index:02}"));
    let catalog = bound_role_catalog(entries);
    let expected = std::iter::once(ROLE_CATALOG_HEADER.to_string())
        .chain((0..MAX_ROLE_CATALOG_ENTRIES).map(|index| format!("entry-{index:02}")))
        .chain(std::iter::once(ROLE_CATALOG_OMISSION_MARKER.to_string()))
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(catalog, expected);
    assert!(catalog.len() <= MAX_ROLE_CATALOG_BYTES);
}

#[test]
fn catalog_bounds_code_mode_rendered_lines() {
    let newline_dense_entry = format!(
        "dense: {{\n{}\n}}",
        (0..MAX_ROLE_CATALOG_RENDERED_LINES)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let catalog = bound_role_catalog([newline_dense_entry]);

    assert_eq!(
        catalog,
        format!("{ROLE_CATALOG_HEADER}\n{ROLE_CATALOG_OMISSION_MARKER}")
    );
    assert!(rendered_line_count(&catalog) <= MAX_ROLE_CATALOG_RENDERED_LINES);
}

#[test]
fn catalog_accepts_exact_total_byte_limit_and_omits_overflow() {
    let first_entry = "a".repeat(1_991);
    let second_entry = "b".repeat(1_991);
    let exact_catalog = bound_role_catalog([first_entry.clone(), second_entry.clone()]);

    assert_eq!(
        exact_catalog,
        format!("{ROLE_CATALOG_HEADER}\n{first_entry}\n{second_entry}")
    );
    assert_eq!(exact_catalog.len(), MAX_ROLE_CATALOG_BYTES);

    let overflow_catalog = bound_role_catalog([first_entry.clone(), format!("{second_entry}x")]);

    assert_eq!(
        overflow_catalog,
        format!("{ROLE_CATALOG_HEADER}\n{first_entry}\n{ROLE_CATALOG_OMISSION_MARKER}")
    );

    let marker_reserved_catalog =
        bound_role_catalog([first_entry.clone(), second_entry, "c".to_string()]);

    assert_eq!(
        marker_reserved_catalog,
        format!("{ROLE_CATALOG_HEADER}\n{first_entry}\n{ROLE_CATALOG_OMISSION_MARKER}")
    );
    assert!(marker_reserved_catalog.len() <= MAX_ROLE_CATALOG_BYTES);
}

#[test]
fn catalog_bounds_total_and_per_entry_bytes_utf8_safely() {
    let exact_entry = "x".repeat(MAX_ROLE_CATALOG_ENTRY_BYTES);
    assert_eq!(truncate_catalog_entry(exact_entry.clone()), exact_entry);

    let overflow_entry = "x".repeat(MAX_ROLE_CATALOG_ENTRY_BYTES + 1);
    let expected_overflow_entry = format!(
        "{}{}",
        "x".repeat(MAX_ROLE_CATALOG_ENTRY_BYTES - ROLE_CATALOG_ENTRY_OMISSION_MARKER.len()),
        ROLE_CATALOG_ENTRY_OMISSION_MARKER
    );
    assert_eq!(
        truncate_catalog_entry(overflow_entry),
        expected_overflow_entry
    );

    let oversized_unicode_entry = format!("unicode: {}", "🦀".repeat(1_000));
    let unicode_catalog = bound_role_catalog([oversized_unicode_entry]);
    let unicode_entry = unicode_catalog
        .strip_prefix(&format!("{ROLE_CATALOG_HEADER}\n"))
        .expect("catalog should retain its header");

    assert!(unicode_entry.len() <= MAX_ROLE_CATALOG_ENTRY_BYTES);
    assert!(unicode_entry.ends_with(ROLE_CATALOG_ENTRY_OMISSION_MARKER));

    let first_entry = format!("entry-0: {}", "x".repeat(1_980));
    let entries = (0..4).map(|index| format!("entry-{index}: {}", "x".repeat(1_980)));
    let catalog = bound_role_catalog(entries);
    let expected = format!("{ROLE_CATALOG_HEADER}\n{first_entry}\n{ROLE_CATALOG_OMISSION_MARKER}");

    assert_eq!(catalog, expected);
    assert!(catalog.len() <= MAX_ROLE_CATALOG_BYTES);
}

#[test]
fn spawn_tool_spec_applies_catalog_bounds() {
    let user_defined_roles = (0..40)
        .map(|index| {
            (
                format!("role-{index:02}"),
                AgentRoleConfig {
                    description: Some(format!("role description {index}")),
                    config_file: None,
                    nickname_candidates: None,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();

    let catalog = spawn_tool_spec::build(&user_defined_roles);

    assert!(catalog.len() <= MAX_ROLE_CATALOG_BYTES);
    assert!(catalog.ends_with(ROLE_CATALOG_OMISSION_MARKER));
}
