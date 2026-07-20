//! Hard bounds for user-authored role context exposed to spawned agents.

use crate::config::Config;
use anyhow::bail;
use toml::Value as TomlValue;

pub(super) const MAX_ROLE_CONTEXT_LANE_BYTES: usize = 4_000;
pub(super) const MAX_ROLE_CONTEXT_TOTAL_BYTES: usize = 8_000;
pub(super) const MAX_ROLE_CATALOG_BYTES: usize = 4_000;
pub(super) const MAX_ROLE_CATALOG_ENTRIES: usize = 32;
pub(super) const MAX_ROLE_CATALOG_ENTRY_BYTES: usize = 2_000;
/// Custom inherited instruction lanes are emitted as individual model items, so a byte ceiling
/// below 10K is also a conservative tokenizer-independent token ceiling.
pub(super) const MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES: usize = 8 * 1024;
/// Audited product base instructions commonly exceed 8 KiB. Keep a separate byte backstop so even
/// an accidentally stale audit entry cannot make this lane unbounded.
pub(super) const MAX_WORKFLOW_CHILD_MODEL_BASE_BYTES: usize = 32 * 1024;
/// Workflow children receive a conservative fraction of the root session's 32 KiB default. The
/// rendered project contribution, including provenance labels and its wrapper, must remain well
/// below the repository-wide 10K-token per-item ceiling even under the byte fallback estimator.
pub(super) const MAX_ROLE_PROJECT_DOC_BYTES: usize = 8 * 1024;

const ROLE_CATALOG_OMISSION_MARKER: &str = "... [additional roles omitted]";
const ROLE_CATALOG_ENTRY_OMISSION_MARKER: &str = "\n... [entry truncated]";

pub(super) fn validate_effective_role_context(
    role_layer: &TomlValue,
    config: &Config,
) -> anyhow::Result<()> {
    if role_layer.get("project_doc_max_bytes").is_some()
        && config.project_doc_max_bytes > MAX_ROLE_PROJECT_DOC_BYTES
    {
        bail!(
            "agent role project_doc_max_bytes exceeds the {MAX_ROLE_PROJECT_DOC_BYTES}-byte limit (got {} bytes)",
            config.project_doc_max_bytes
        );
    }

    let lanes = [
        (
            "base instructions",
            role_layer.get("instructions").is_some()
                || role_layer.get("model_instructions_file").is_some(),
            config.base_instructions.as_deref(),
        ),
        (
            "developer instructions",
            role_layer.get("developer_instructions").is_some(),
            config.developer_instructions.as_deref(),
        ),
        (
            "compact prompt",
            role_layer.get("compact_prompt").is_some()
                || role_layer.get("experimental_compact_prompt_file").is_some(),
            config.compact_prompt.as_deref(),
        ),
    ];

    let mut combined_bytes = 0;
    for (label, overridden, content) in lanes {
        if !overridden {
            continue;
        }
        let bytes = content.map(str::len).unwrap_or_default();
        if bytes > MAX_ROLE_CONTEXT_LANE_BYTES {
            bail!(
                "agent role {label} exceeds the {MAX_ROLE_CONTEXT_LANE_BYTES}-byte UTF-8 limit (got {bytes} bytes)"
            );
        }
        combined_bytes += bytes;
    }

    if combined_bytes > MAX_ROLE_CONTEXT_TOTAL_BYTES {
        bail!(
            "agent role instruction lanes exceed the {MAX_ROLE_CONTEXT_TOTAL_BYTES}-byte combined UTF-8 limit (got {combined_bytes} bytes)"
        );
    }
    Ok(())
}

/// Bound every model-visible instruction lane inherited by a workflow child.
///
/// Role-authored overrides have tighter byte limits in [`validate_effective_role_context`], but
/// the parent session's base/developer instructions and compact prompt may be much larger. Reject
/// any effective lane above the repository-wide per-item token ceiling before the child is
/// created. Project-derived instructions are clamped because their configured limit controls a
/// later bounded read rather than content already accepted into the parent session.
pub(crate) fn bound_workflow_child_context(config: &mut Config) -> anyhow::Result<()> {
    if let Some(base_instructions) = config.base_instructions.as_deref() {
        ensure_base_instructions_bounded(base_instructions)?;
    }
    for (label, content) in [
        (
            "developer instructions",
            config.developer_instructions.as_deref(),
        ),
        ("compact prompt", config.compact_prompt.as_deref()),
    ] {
        if let Some(content) = content {
            ensure_custom_lane_bytes(label, content)?;
        }
    }
    config.project_doc_max_bytes = config.project_doc_max_bytes.min(MAX_ROLE_PROJECT_DOC_BYTES);
    crate::context::clamp_workflow_child_tool_output_limit(config);
    Ok(())
}

fn ensure_base_instructions_bounded(content: &str) -> anyhow::Result<()> {
    let bytes = content.len();
    if bytes <= MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES {
        return Ok(());
    }
    if bytes <= MAX_WORKFLOW_CHILD_MODEL_BASE_BYTES
        && codex_models_manager::model_info::is_audited_model_instruction(content)
    {
        return Ok(());
    }
    bail!(
        "workflow child base instructions exceed the {MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES}-byte UTF-8 limit and do not match audited product context (got {bytes} bytes)"
    )
}

fn ensure_custom_lane_bytes(label: &str, content: &str) -> anyhow::Result<()> {
    let bytes = content.len();
    if bytes > MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES {
        bail!(
            "workflow child {label} exceeds the {MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES}-byte UTF-8 limit (got {bytes} bytes)"
        );
    }
    Ok(())
}

pub(super) fn bound_role_catalog(
    header: &str,
    role_entries: impl IntoIterator<Item = String>,
) -> String {
    let mut entries = Vec::new();
    let mut catalog_bytes = header.len();
    let mut role_entries = role_entries.into_iter();
    let mut omitted = false;

    while entries.len() < MAX_ROLE_CATALOG_ENTRIES {
        let Some(entry) = role_entries.next() else {
            break;
        };
        let entry = truncate_catalog_entry(entry);
        if catalog_bytes + 1 + entry.len() > MAX_ROLE_CATALOG_BYTES {
            omitted = true;
            break;
        }
        catalog_bytes += 1 + entry.len();
        entries.push(entry);
    }
    if !omitted && role_entries.next().is_some() {
        omitted = true;
    }

    if omitted {
        while catalog_bytes + 1 + ROLE_CATALOG_OMISSION_MARKER.len() > MAX_ROLE_CATALOG_BYTES {
            let Some(entry) = entries.pop() else {
                break;
            };
            catalog_bytes -= 1 + entry.len();
        }
    }

    let mut catalog = String::with_capacity(MAX_ROLE_CATALOG_BYTES.min(catalog_bytes));
    catalog.push_str(header);
    for entry in entries {
        catalog.push('\n');
        catalog.push_str(&entry);
    }
    if omitted {
        catalog.push('\n');
        catalog.push_str(ROLE_CATALOG_OMISSION_MARKER);
    }
    catalog
}

fn truncate_catalog_entry(mut entry: String) -> String {
    if entry.len() <= MAX_ROLE_CATALOG_ENTRY_BYTES {
        return entry;
    }

    let mut retained_bytes =
        MAX_ROLE_CATALOG_ENTRY_BYTES - ROLE_CATALOG_ENTRY_OMISSION_MARKER.len();
    while !entry.is_char_boundary(retained_bytes) {
        retained_bytes -= 1;
    }
    entry.truncate(retained_bytes);
    entry.push_str(ROLE_CATALOG_ENTRY_OMISSION_MARKER);
    entry
}

#[cfg(test)]
#[path = "role_context_bounds_tests.rs"]
mod tests;
