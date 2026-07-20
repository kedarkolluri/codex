use codex_protocol::config_types::Personality;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelInstructionsVariables;
use codex_protocol::openai_models::ModelMessages;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::TruncationMode;
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::openai_models::WebSearchToolType;
use codex_protocol::openai_models::default_input_modalities;

use crate::config::ModelsManagerConfig;
use codex_utils_output_truncation::approx_bytes_for_tokens;
use sha2::Digest;
use sha2::Sha256;
use tracing::warn;

pub const BASE_INSTRUCTIONS: &str = include_str!("../prompt.md");
const DEFAULT_PERSONALITY_HEADER: &str = "You are Codex, a coding agent based on GPT-5. You and the user share the same workspace and collaborate to achieve the user's goals.";
const LOCAL_FRIENDLY_TEMPLATE: &str =
    "You optimize for team morale and being a supportive teammate as much as code quality.";
const LOCAL_PRAGMATIC_TEMPLATE: &str = "You are a deeply pragmatic, effective software engineer.";
const PERSONALITY_PLACEHOLDER: &str = "{{ personality }}";
const PERSONALITY_SECTION_HEADER: &str = "# Personality";

/// Maximum audited `o200k_base` token count for a product-provided model instruction string.
const AUDITED_MODEL_INSTRUCTION_MAX_TOKENS: usize = 8 * 1024;

/// One exact product instruction string that has been reviewed under the GPT-5 tokenizer.
///
/// The digest list is intentionally independent of `models.json`: changing remote or bundled model
/// metadata must not silently extend the set of long strings admitted into workflow-child context.
/// The sibling audit test regenerates every product-renderable instruction variant and verifies the
/// digest, UTF-8 byte count, and exact `o200k_base` token count together.
struct AuditedModelInstruction {
    sha256: &'static str,
    utf8_bytes: usize,
    o200k_tokens: usize,
}

const AUDITED_MODEL_INSTRUCTIONS: &[AuditedModelInstruction] = &[
    AuditedModelInstruction {
        sha256: "11fcba1a54577605ea5d69c7cb796e7defe21deb9bd90621661d0310fc939674",
        utf8_bytes: 21_474,
        o200k_tokens: 4_429,
    },
    AuditedModelInstruction {
        sha256: "3251ff0c83b78c3a280cbf1b49e463163f448ce921f2795900d8ebb003bb7331",
        utf8_bytes: 21_095,
        o200k_tokens: 4_405,
    },
    AuditedModelInstruction {
        sha256: "3a0d695b341d477d203d38d136898bf7a6c62a4e7cfe841a9e0c6adeac038b4d",
        utf8_bytes: 13_550,
        o200k_tokens: 2_752,
    },
    AuditedModelInstruction {
        sha256: "478e8a11b180adb2659f21aba51744711f79f665039bb0bc4a13d3c051fcb76c",
        utf8_bytes: 14_764,
        o200k_tokens: 2_991,
    },
    AuditedModelInstruction {
        sha256: "4cf5dd6317a9920b3f0398f6fa7ca49310b57961f6dd076eb2141acd4f963843",
        utf8_bytes: 21_039,
        o200k_tokens: 4_395,
    },
    AuditedModelInstruction {
        sha256: "53e1246a930b9dd78154bda2733a824dba50f9f63dab5defccb9f07441b42a08",
        utf8_bytes: 13_957,
        o200k_tokens: 2_824,
    },
    AuditedModelInstruction {
        sha256: "78a2fc84e1bffa421d865c1a2ade4185d3d33ef38e6a15157f0ff1a89b7d52ec",
        utf8_bytes: 16_306,
        o200k_tokens: 3_252,
    },
    AuditedModelInstruction {
        sha256: "9109777dc7f3bc9ee9a0d187982b13538c53e0572de2959300f7226e9c59855e",
        utf8_bytes: 11_131,
        o200k_tokens: 2_300,
    },
    AuditedModelInstruction {
        sha256: "93c0d2d5e30c5d4284950a1ace4c3176c45e68d71f6652905458533a4c8c5930",
        utf8_bytes: 15_330,
        o200k_tokens: 3_104,
    },
    AuditedModelInstruction {
        sha256: "9721f7a86edc261996e628fe14fade8d66ec60e6cc727274a8da6a03e15464de",
        utf8_bytes: 12_911,
        o200k_tokens: 2_652,
    },
    AuditedModelInstruction {
        sha256: "a2e1143d471279aeb422045486fce7cb502fc05bc105ce31790c39a70ab97491",
        utf8_bytes: 12_984,
        o200k_tokens: 2_639,
    },
    AuditedModelInstruction {
        sha256: "ac8ae107a0d72fe3476b430afb161ea4e67da2e446d778aefc44828160559807",
        utf8_bytes: 20_903,
        o200k_tokens: 4_365,
    },
    AuditedModelInstruction {
        sha256: "c2a980bc28af132eb89e0b4c68ae884043faae83a1afd3fd4889f7e8a1ada7b0",
        utf8_bytes: 21_347,
        o200k_tokens: 4_376,
    },
    AuditedModelInstruction {
        sha256: "c9b2fa097ac69cae82c3d2ae12271083890a96521c55ad8dc14cae5168ad3f39",
        utf8_bytes: 21_672,
        o200k_tokens: 4_570,
    },
    AuditedModelInstruction {
        sha256: "cb4369284f6f3f9511b287c1fe89c9d443c969fa17893f370555eb08d9d4f78d",
        utf8_bytes: 21_124,
        o200k_tokens: 4_411,
    },
    AuditedModelInstruction {
        sha256: "e58c21f9377e946e2e10f886fcbf6f030e1c6fd9067241c637a56e9e998d3c31",
        utf8_bytes: 19_749,
        o200k_tokens: 4_087,
    },
    AuditedModelInstruction {
        sha256: "e9778714d505f3dd04d44db4394024c5fab5bf6554fc9faa3cdf9cf776b63bb9",
        utf8_bytes: 16_327,
        o200k_tokens: 3_257,
    },
    AuditedModelInstruction {
        sha256: "fb34dc8e8987c21a819633fd1112231a31028155d63801ec26b2b2e55f806e6c",
        utf8_bytes: 14_225,
        o200k_tokens: 2_869,
    },
];

/// Returns whether `instructions` exactly matches product context audited below the 10K-token
/// per-item ceiling.
///
/// This is deliberately a content allowlist, not a model-slug or metadata-provenance check. Remote
/// model metadata and config overrides can use familiar model names, but long instruction strings
/// are admitted only after their exact bytes have been reviewed and pinned here.
pub fn is_audited_model_instruction(instructions: &str) -> bool {
    if !AUDITED_MODEL_INSTRUCTIONS
        .iter()
        .any(|entry| entry.utf8_bytes == instructions.len())
    {
        return false;
    }

    let sha256 = instruction_sha256(instructions);
    AUDITED_MODEL_INSTRUCTIONS
        .iter()
        .any(|entry| {
            entry.utf8_bytes == instructions.len()
                && entry.o200k_tokens <= AUDITED_MODEL_INSTRUCTION_MAX_TOKENS
                && entry.sha256 == sha256.as_str()
        })
}

fn instruction_sha256(instructions: &str) -> String {
    format!("{:x}", Sha256::digest(instructions.as_bytes()))
}

pub fn with_config_overrides(mut model: ModelInfo, config: &ModelsManagerConfig) -> ModelInfo {
    if let Some(context_window) = config.model_context_window {
        model.context_window = Some(
            model
                .max_context_window
                .map_or(context_window, |max_context_window| {
                    context_window.min(max_context_window)
                }),
        );
    }
    if let Some(auto_compact_token_limit) = config.model_auto_compact_token_limit {
        model.auto_compact_token_limit = Some(auto_compact_token_limit);
    }
    if let Some(token_limit) = config.tool_output_token_limit {
        model.truncation_policy = match model.truncation_policy.mode {
            TruncationMode::Bytes => {
                let byte_limit =
                    i64::try_from(approx_bytes_for_tokens(token_limit)).unwrap_or(i64::MAX);
                TruncationPolicyConfig::bytes(byte_limit)
            }
            TruncationMode::Tokens => {
                let limit = i64::try_from(token_limit).unwrap_or(i64::MAX);
                TruncationPolicyConfig::tokens(limit)
            }
        };
    }

    if let Some(base_instructions) = &config.base_instructions {
        model.base_instructions = base_instructions.clone();
        clear_instruction_messages(&mut model);
    } else {
        if config.personality_enabled && config.personality == Some(Personality::None) {
            model.base_instructions = strip_personality_section(model.base_instructions);
            if let Some(instructions_template) = model
                .model_messages
                .as_mut()
                .and_then(|messages| messages.instructions_template.as_mut())
            {
                *instructions_template =
                    strip_personality_section(std::mem::take(instructions_template));
            }
        }
        if !config.personality_enabled {
            clear_instruction_messages(&mut model);
        }
    }

    model
}

fn strip_personality_section(mut instructions: String) -> String {
    let mut section_start = None;
    let mut section_end = None;
    let mut offset = 0;

    for line_with_ending in instructions.split_inclusive('\n') {
        let line = match line_with_ending.strip_suffix('\n') {
            Some(line) => line.strip_suffix('\r').unwrap_or(line),
            None => line_with_ending,
        };
        if section_start.is_some() {
            if is_h1_heading(line) {
                section_end = Some(offset);
                break;
            }
        } else if line == PERSONALITY_SECTION_HEADER {
            section_start = Some(offset);
        }
        offset += line_with_ending.len();
    }

    if let Some(section_start) = section_start {
        let section_end = section_end.unwrap_or(instructions.len());
        instructions.replace_range(section_start..section_end, "");
    }

    instructions
}

fn is_h1_heading(line: &str) -> bool {
    let Some(rest) = line.strip_prefix('#') else {
        return false;
    };
    rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')
}

fn clear_instruction_messages(model: &mut ModelInfo) {
    if let Some(model_messages) = &mut model.model_messages {
        model_messages.instructions_template = None;
        model_messages.instructions_variables = None;
        if model_messages.approvals.is_none() {
            model.model_messages = None;
        }
    }
}

/// Build a minimal fallback model descriptor for missing/unknown slugs.
pub fn model_info_from_slug(slug: &str) -> ModelInfo {
    warn!("Unknown model {slug} is used. This will use fallback model metadata.");
    ModelInfo {
        slug: slug.to_string(),
        display_name: slug.to_string(),
        description: None,
        default_reasoning_level: None,
        supported_reasoning_levels: Vec::new(),
        shell_type: ConfigShellToolType::Default,
        visibility: ModelVisibility::None,
        supported_in_api: true,
        priority: 99,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        default_service_tier: None,
        availability_nux: None,
        upgrade: None,
        base_instructions: BASE_INSTRUCTIONS.to_string(),
        model_messages: local_personality_messages_for_slug(slug),
        include_skills_usage_instructions: false,
        supports_reasoning_summary_parameter: true,
        default_reasoning_summary: ReasoningSummary::Auto,
        support_verbosity: false,
        default_verbosity: None,
        apply_patch_tool_type: None,
        web_search_tool_type: WebSearchToolType::Text,
        truncation_policy: TruncationPolicyConfig::bytes(/*limit*/ 10_000),
        supports_parallel_tool_calls: false,
        supports_image_detail_original: false,
        context_window: Some(272_000),
        max_context_window: Some(272_000),
        auto_compact_token_limit: None,
        comp_hash: None,
        effective_context_window_percent: 95,
        experimental_supported_tools: Vec::new(),
        input_modalities: default_input_modalities(),
        used_fallback_model_metadata: true, // this is the fallback model metadata
        supports_search_tool: false,
        use_responses_lite: false,
        auto_review_model_override: None,
        tool_mode: None,
        multi_agent_version: None,
    }
}

fn local_personality_messages_for_slug(slug: &str) -> Option<ModelMessages> {
    match slug {
        "gpt-5.2-codex" | "exp-codex-personality" => Some(ModelMessages {
            instructions_template: Some(format!(
                "{DEFAULT_PERSONALITY_HEADER}\n\n{PERSONALITY_PLACEHOLDER}\n\n{BASE_INSTRUCTIONS}"
            )),
            instructions_variables: Some(ModelInstructionsVariables {
                personality_default: Some(String::new()),
                personality_friendly: Some(LOCAL_FRIENDLY_TEMPLATE.to_string()),
                personality_pragmatic: Some(LOCAL_PRAGMATIC_TEMPLATE.to_string()),
            }),
            approvals: None,
        }),
        _ => None,
    }
}

#[cfg(test)]
#[path = "model_info_tests.rs"]
mod tests;
