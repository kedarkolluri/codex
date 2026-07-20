//! Hard bounds for user-authored role context exposed to workflow-managed agents.

use crate::config::Config;
use anyhow::bail;
use toml::Value as TomlValue;

pub(super) const MAX_ROLE_CONTEXT_LANE_BYTES: usize = 4_000;
pub(super) const MAX_ROLE_CONTEXT_TOTAL_BYTES: usize = 8_000;
pub(super) const MAX_ROLE_PROJECT_DOC_BYTES: usize = 8 * 1024;

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

#[cfg(test)]
#[path = "role_context_bounds_tests.rs"]
mod tests;
