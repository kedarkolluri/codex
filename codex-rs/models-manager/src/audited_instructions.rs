//! Exact-content admission for product-provided model instructions.

use sha2::Digest;
use sha2::Sha256;

/// Maximum audited `o200k_base` token count for one product-provided instruction string.
const AUDITED_MODEL_INSTRUCTION_MAX_TOKENS: usize = 8 * 1024;
/// Maximum UTF-8 size for one product-provided instruction string.
const AUDITED_MODEL_INSTRUCTION_MAX_BYTES: usize = 32 * 1024;

/// One exact product instruction string reviewed under the GPT-5 tokenizer.
///
/// This list is intentionally independent of model slugs and catalog provenance. Remote model
/// metadata and config overrides can reuse familiar slugs, so only the exact reviewed bytes may
/// bypass a lower tokenizer-independent byte ceiling at the eventual workflow-child boundary.
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
        sha256: "5fd1b00d8447e9ed2bfd64e31ba17f73d90a3573657fc7311cbf50278aec7a73",
        utf8_bytes: 15_685,
        o200k_tokens: 3_169,
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
        sha256: "cbefa6b0bede0e332d957fca70ccacf9f12f4c0ecdf81b819e5cbe1a3b16e265",
        utf8_bytes: 17_766,
        o200k_tokens: 3_552,
    },
    AuditedModelInstruction {
        sha256: "e58c21f9377e946e2e10f886fcbf6f030e1c6fd9067241c637a56e9e998d3c31",
        utf8_bytes: 19_749,
        o200k_tokens: 4_087,
    },
];

/// Returns whether `instructions` exactly matches product context audited below the 10K-token
/// per-item ceiling.
pub fn is_audited_model_instruction(instructions: &str) -> bool {
    if instructions.len() > AUDITED_MODEL_INSTRUCTION_MAX_BYTES {
        return false;
    }
    if !AUDITED_MODEL_INSTRUCTIONS
        .iter()
        .any(|entry| entry.utf8_bytes == instructions.len())
    {
        return false;
    }

    let sha256 = instruction_sha256(instructions);
    AUDITED_MODEL_INSTRUCTIONS.iter().any(|entry| {
        entry.utf8_bytes == instructions.len()
            && entry.utf8_bytes <= AUDITED_MODEL_INSTRUCTION_MAX_BYTES
            && entry.o200k_tokens <= AUDITED_MODEL_INSTRUCTION_MAX_TOKENS
            && entry.sha256 == sha256
    })
}

fn instruction_sha256(instructions: &str) -> String {
    let digest = Sha256::digest(instructions.as_bytes());
    format!("{digest:x}")
}

#[cfg(test)]
#[path = "audited_instructions_tests.rs"]
mod tests;
