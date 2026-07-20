use super::MultiAgentV2Config;

pub(super) const USAGE_HINT_TEXT_MAX_BYTES: usize = 1_000;
pub(super) const PROMPT_FIELD_MAX_BYTES: usize = 4_000;
pub(super) const PROMPT_TOTAL_MAX_BYTES: usize = 8_000;

pub(super) fn validate(config: &MultiAgentV2Config) -> std::io::Result<()> {
    let prompt_fields = [
        (
            "usage_hint_text",
            config.usage_hint_text.as_deref(),
            USAGE_HINT_TEXT_MAX_BYTES,
        ),
        (
            "root_agent_usage_hint_text",
            config.root_agent_usage_hint_text.as_deref(),
            PROMPT_FIELD_MAX_BYTES,
        ),
        (
            "subagent_usage_hint_text",
            config.subagent_usage_hint_text.as_deref(),
            PROMPT_FIELD_MAX_BYTES,
        ),
        (
            "multi_agent_mode_hint_text",
            config.multi_agent_mode_hint_text.as_deref(),
            PROMPT_FIELD_MAX_BYTES,
        ),
    ];
    let mut total_bytes = 0usize;
    for (field, value, max_bytes) in prompt_fields {
        let Some(value) = value else {
            continue;
        };
        let bytes = value.len();
        if bytes > max_bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "features.multi_agent_v2.{field} exceeds the {max_bytes}-byte limit (got {bytes} bytes)"
                ),
            ));
        }
        total_bytes += bytes;
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
