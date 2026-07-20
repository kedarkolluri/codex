use super::MultiAgentV2Config;

pub(super) const PROMPT_FIELD_MAX_BYTES: usize = 4_000;
pub(super) const PROMPT_TOTAL_MAX_BYTES: usize = 8_000;

pub(super) fn validate(config: &MultiAgentV2Config) -> std::io::Result<()> {
    let prompt_fields = [
        ("usage_hint_text", config.usage_hint_text.as_deref()),
        (
            "root_agent_usage_hint_text",
            config.root_agent_usage_hint_text.as_deref(),
        ),
        (
            "subagent_usage_hint_text",
            config.subagent_usage_hint_text.as_deref(),
        ),
        (
            "multi_agent_mode_hint_text",
            config.multi_agent_mode_hint_text.as_deref(),
        ),
    ];
    let mut total_bytes = 0usize;
    for (field, value) in prompt_fields {
        let Some(value) = value else {
            continue;
        };
        let bytes = value.len();
        if bytes > PROMPT_FIELD_MAX_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "features.multi_agent_v2.{field} exceeds the {PROMPT_FIELD_MAX_BYTES}-byte limit (got {bytes} bytes)"
                ),
            ));
        }
        total_bytes = total_bytes.saturating_add(bytes);
    }
    if total_bytes > PROMPT_TOTAL_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "features.multi_agent_v2 prompt payload exceeds the {PROMPT_TOTAL_MAX_BYTES}-byte combined limit (got {total_bytes} bytes)"
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "multi_agent_v2_bounds_tests.rs"]
mod tests;
