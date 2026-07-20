use super::*;
use crate::ModelsManagerConfig;
use crate::model_info::LOCAL_PERSONALITY_MODEL_SLUGS;
use crate::model_info::model_info_from_slug;
use crate::model_info::with_config_overrides;
use codex_protocol::config_types::Personality;
use codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT;
use codex_protocol::openai_models::ModelInfo;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
use strum::IntoEnumIterator;

fn config_with_personality(personality: Option<Personality>) -> ModelsManagerConfig {
    ModelsManagerConfig {
        personality_enabled: true,
        personality,
        ..Default::default()
    }
}

fn product_instruction_variants() -> Vec<(String, String)> {
    let mut variants = Vec::new();
    for model in crate::bundled_models_response()
        .expect("bundled models should parse")
        .models
    {
        let model_label = model.slug.clone();
        push_model_instruction_variants(&mut variants, &model_label, model);
    }
    for slug in
        std::iter::once("unknown-model").chain(LOCAL_PERSONALITY_MODEL_SLUGS.iter().copied())
    {
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
    for personality in [None].into_iter().chain(Personality::iter().map(Some)) {
        let config = config_with_personality(personality);
        let effective_model = with_config_overrides(model.clone(), &config);
        variants.push((
            format!("{model_label}/{personality:?}"),
            effective_model.get_model_instructions(personality),
        ));
    }
}

#[test]
fn audited_manifest_matches_every_product_instruction_variant() {
    let tokenizer = tiktoken_rs::o200k_base().expect("construct o200k tokenizer");
    let variants = product_instruction_variants();
    let mut inventory = BTreeMap::<String, (String, usize, Vec<String>)>::new();

    for (label, instructions) in &variants {
        let sha256 = instruction_sha256(instructions);
        let token_count = tokenizer.count_ordinary(instructions);
        match inventory.entry(sha256) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((instructions.clone(), token_count, vec![label.clone()]));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                assert_eq!(entry.get().0, *instructions);
                assert_eq!(entry.get().1, token_count);
                entry.get_mut().2.push(label.clone());
            }
        }
    }

    let actual = inventory
        .iter()
        .map(|(sha256, (instructions, token_count, _labels))| {
            (sha256.clone(), instructions.len(), *token_count)
        })
        .collect::<Vec<_>>();
    let expected = AUDITED_MODEL_INSTRUCTIONS
        .iter()
        .map(|entry| {
            (
                entry.sha256.to_string(),
                entry.utf8_bytes,
                entry.o200k_tokens,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);

    for (label, instructions) in variants {
        let token_count = tokenizer.count_ordinary(&instructions);
        assert!(
            instructions.len() <= AUDITED_MODEL_INSTRUCTION_MAX_BYTES,
            "product instruction variant {label} has {} UTF-8 bytes",
            instructions.len()
        );
        assert!(
            token_count <= AUDITED_MODEL_INSTRUCTION_MAX_TOKENS,
            "product instruction variant {label} has {token_count} o200k tokens"
        );
        assert!(
            is_audited_model_instruction(&instructions),
            "product instruction variant {label} is not audited"
        );
    }
}

#[test]
fn audit_requires_exact_instruction_bytes() {
    assert!(is_audited_model_instruction(BASE_INSTRUCTIONS_DEFAULT));

    let modified = BASE_INSTRUCTIONS_DEFAULT.replacen('Y', "X", /*count*/ 1);
    assert_eq!(
        (modified.len(), modified == BASE_INSTRUCTIONS_DEFAULT),
        (BASE_INSTRUCTIONS_DEFAULT.len(), false)
    );
    assert!(!is_audited_model_instruction(&modified));
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
