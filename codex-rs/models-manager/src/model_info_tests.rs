use super::*;
use crate::ModelsManagerConfig;
use codex_protocol::config_types::Personality;
use codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT;
use codex_protocol::openai_models::ApprovalMessages;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;

fn config_with_personality(personality: Option<Personality>) -> ModelsManagerConfig {
    ModelsManagerConfig {
        personality_enabled: true,
        personality,
        ..Default::default()
    }
}

fn product_instruction_variants() -> Vec<(String, String)> {
    let mut variants = Vec::new();
    let bundled_models = crate::bundled_models_response()
        .expect("bundled models should parse")
        .models;
    for model in bundled_models {
        let model_label = model.slug.clone();
        push_model_instruction_variants(&mut variants, &model_label, model);
    }
    for slug in [
        "unknown-model",
        "gpt-5.2-codex",
        "exp-codex-personality",
    ] {
        push_model_instruction_variants(&mut variants, slug, model_info_from_slug(slug));
    }
    variants.push((
        "protocol default base instructions".to_string(),
        BASE_INSTRUCTIONS_DEFAULT.to_string(),
    ));
    variants
}

fn push_model_instruction_variants(
    variants: &mut Vec<(String, String)>,
    model_label: &str,
    model: ModelInfo,
) {
    let personality_disabled =
        with_config_overrides(model.clone(), &ModelsManagerConfig::default());
    variants.push((
        format!("{model_label}/personality-disabled"),
        personality_disabled.get_model_instructions(/*personality*/ None),
    ));
    for personality in [
        /*personality*/ None,
        Some(Personality::None),
        Some(Personality::Friendly),
        Some(Personality::Pragmatic),
    ] {
        let config = config_with_personality(personality);
        let effective_model = with_config_overrides(model.clone(), &config);
        variants.push((
            format!("{model_label}/{personality:?}"),
            effective_model.get_model_instructions(personality),
        ));
    }
}

#[test]
fn audited_model_instruction_manifest_matches_every_product_variant() {
    let tokenizer = tiktoken_rs::o200k_base().expect("construct o200k tokenizer");
    let mut inventory = BTreeMap::<String, (String, Vec<String>, usize)>::new();

    for (label, instructions) in product_instruction_variants() {
        let sha256 = instruction_sha256(&instructions);
        let token_count = tokenizer.count_ordinary(&instructions);
        assert!(
            is_audited_model_instruction(&instructions),
            "product instruction variant {label} ({sha256}) is not audited"
        );
        assert!(
            token_count <= AUDITED_MODEL_INSTRUCTION_MAX_TOKENS,
            "product instruction variant {label} has {token_count} o200k tokens"
        );
        match inventory.entry(sha256) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((instructions, vec![label], token_count));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                assert_eq!(entry.get().0, instructions);
                assert_eq!(entry.get().2, token_count);
                entry.get_mut().1.push(label);
            }
        }
    }

    assert_eq!(inventory.len(), AUDITED_MODEL_INSTRUCTIONS.len());
    for audited in AUDITED_MODEL_INSTRUCTIONS {
        let (instructions, _labels, token_count) = inventory
            .get(audited.sha256)
            .unwrap_or_else(|| panic!("stale audited instruction digest {}", audited.sha256));
        assert_eq!(
            (instructions.len(), *token_count),
            (audited.utf8_bytes, audited.o200k_tokens)
        );
    }
}

#[test]
fn high_token_unicode_does_not_match_the_product_audit() {
    let tokenizer = tiktoken_rs::o200k_base().expect("construct o200k tokenizer");
    let instructions = "\u{10ffff}".repeat(8_192);

    assert_eq!(
        (instructions.len(), tokenizer.count_ordinary(&instructions)),
        (32 * 1024, 32 * 1024)
    );
    assert!(!is_audited_model_instruction(&instructions));
}

#[test]
fn base_instruction_override_preserves_catalog_approval_messages() {
    let mut model = model_info_from_slug("unknown-model");
    let approvals = ApprovalMessages {
        on_request: Some("user approvals".to_string()),
        on_request_auto_review: Some("auto approvals".to_string()),
    };
    model.model_messages = Some(ModelMessages {
        instructions_template: Some("template".to_string()),
        instructions_variables: Some(ModelInstructionsVariables {
            personality_default: Some("default".to_string()),
            personality_friendly: Some("friendly".to_string()),
            personality_pragmatic: Some("pragmatic".to_string()),
        }),
        approvals: Some(approvals.clone()),
    });
    let config = ModelsManagerConfig {
        base_instructions: Some("override".to_string()),
        ..Default::default()
    };

    let updated = with_config_overrides(model, &config);

    assert_eq!(
        updated.model_messages,
        Some(ModelMessages {
            instructions_template: None,
            instructions_variables: None,
            approvals: Some(approvals),
        })
    );
}

#[test]
fn disabled_personality_preserves_catalog_approval_messages() {
    let mut model = model_info_from_slug("unknown-model");
    let approvals = ApprovalMessages {
        on_request: Some("user approvals".to_string()),
        on_request_auto_review: None,
    };
    model.model_messages = Some(ModelMessages {
        instructions_template: Some("template".to_string()),
        instructions_variables: None,
        approvals: Some(approvals.clone()),
    });
    let config = ModelsManagerConfig {
        personality_enabled: false,
        ..Default::default()
    };

    let updated = with_config_overrides(model, &config);

    assert_eq!(
        updated.model_messages,
        Some(ModelMessages {
            instructions_template: None,
            instructions_variables: None,
            approvals: Some(approvals),
        })
    );
}

#[test]
fn personality_none_strips_catalog_instruction_sources_through_the_next_h1() {
    let cases = [
        (
            "Intro\n\n# Personality\n\nRemove me\n\n## Writing Style\n\nRemove me too\n\n# Safety\n\nKeep me",
            "Intro\n\n# Safety\n\nKeep me",
        ),
        ("Intro\n\n# Personality\n\nRemove me", "Intro\n\n"),
        (
            "Intro\n\n## Personality\n\nKeep me",
            "Intro\n\n## Personality\n\nKeep me",
        ),
        (
            "Intro\n\n# Personality \n\nKeep me",
            "Intro\n\n# Personality \n\nKeep me",
        ),
        (
            "Intro\r\n\r\n# Personality\r\n\r\nRemove me\r\n\r\n## Writing Style\r\n\r\nRemove me too\r\n\r\n# General\r\n\r\nKeep me",
            "Intro\r\n\r\n# General\r\n\r\nKeep me",
        ),
    ];
    let config = config_with_personality(Some(Personality::None));

    for (instructions, expected) in cases {
        let mut model = model_info_from_slug("unknown-model");
        model.base_instructions = instructions.to_string();
        model.model_messages = Some(ModelMessages {
            instructions_template: Some(instructions.to_string()),
            instructions_variables: None,
            approvals: None,
        });

        let updated = with_config_overrides(model, &config);
        let instructions_template = updated
            .model_messages
            .as_ref()
            .and_then(|messages| messages.instructions_template.as_deref());

        assert_eq!(
            (updated.base_instructions.as_str(), instructions_template),
            (expected, Some(expected))
        );
    }
}

#[test]
fn baked_personality_section_is_preserved_without_enabled_explicit_none() {
    let instructions = "Intro\n# Personality\nKeep me\n# General\nKeep me too";
    let configs = [
        config_with_personality(/*personality*/ None),
        config_with_personality(Some(Personality::Friendly)),
        config_with_personality(Some(Personality::Pragmatic)),
        ModelsManagerConfig {
            personality: Some(Personality::None),
            ..Default::default()
        },
    ];

    for config in configs {
        let mut model = model_info_from_slug("unknown-model");
        model.base_instructions = instructions.to_string();

        assert_eq!(
            with_config_overrides(model, &config).base_instructions,
            instructions
        );
    }
}

#[test]
fn model_context_window_override_clamps_to_max_context_window() {
    let mut model = model_info_from_slug("unknown-model");
    model.context_window = Some(273_000);
    model.max_context_window = Some(400_000);
    let config = ModelsManagerConfig {
        model_context_window: Some(500_000),
        ..Default::default()
    };

    let updated = with_config_overrides(model.clone(), &config);
    let mut expected = model;
    expected.context_window = Some(400_000);

    assert_eq!(updated, expected);
}

#[test]
fn model_context_window_uses_model_value_without_override() {
    let mut model = model_info_from_slug("unknown-model");
    model.context_window = Some(273_000);
    model.max_context_window = Some(400_000);
    let config = ModelsManagerConfig::default();

    let updated = with_config_overrides(model.clone(), &config);

    assert_eq!(updated, model);
}
