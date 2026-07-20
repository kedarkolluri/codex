//! Canonical `(prompt, opts)` cache key for prefix-replay.
//!
//! The invocation ordinal is the spine of prefix-replay, but the *decision* to
//! serve an ordinal from cache vs. run it live turns on whether the recomputed
//! key matches the journaled one (see `docs/dynamic-workflows-spec.md` §7,
//! "Resume algorithm step 3"). The key must therefore be **byte-stable** across
//! serialization permutations (R7): the same logical `(prompt, opts)` — even
//! with a JSON object's keys emitted in a different order, or a schema
//! serialized with its properties reordered — must hash identically, or a
//! cosmetically-unchanged run would diverge on resume and needlessly re-run.
//!
//! Per §7 the key is:
//!
//! ```text
//! key = blake3(canonical_json({ prompt, model, effort, agentType, isolation, schema }))
//! ```
//!
//! with **sorted object keys** and a **stable JSON-Schema serialization**.
//! `label` and `phase` are deliberately **excluded** so cosmetic re-labeling
//! does not bust cache — they are not even fields of [`KeyInputs`].
//!
//! ### Why we canonicalize by hand
//!
//! `serde_json` is compiled with the `preserve_order` feature enabled somewhere
//! in this workspace (`tui`, `config`), and Cargo unifies features across the
//! whole build — so `serde_json::Map` is an insertion-ordered `IndexMap`, **not**
//! a sorted `BTreeMap`. Relying on `serde_json::to_string` to emit sorted keys
//! would silently depend on that feature-unification state and break the moment
//! it changes. [`write_canonical`] instead emits object keys in explicit sorted
//! order and escapes strings itself, so the canonical bytes are independent of
//! any `serde_json` feature flag.
//!
//! ### Versioning
//!
//! [`KEY_ALGO_VERSION`] is folded into the hash (as a domain-separation prefix)
//! and recorded in `run_meta.key_algo_version`, so a change to this algorithm
//! shifts every computed key and is detectable on resume: a resumed run whose
//! `key_algo_version` differs simply diverges at ordinal 0 and re-runs live,
//! rather than silently serving stale cache hits.

use blake3::Hasher;
use serde_json::Value;
use serde_json::json;

/// Version of the cache-key algorithm. Recorded in `run_meta.key_algo_version`.
///
/// Bumping this deliberately changes the computed key for identical inputs so a
/// resumed run built by an older Codex diverges instead of serving stale cache
/// entries. Any change to [`write_canonical`], the folded field set, or the
/// hash construction MUST bump this.
pub const KEY_ALGO_VERSION: u32 = 1;

/// Version of the run-level execution-environment fingerprint.
///
/// This is intentionally independent from [`KEY_ALGO_VERSION`]: the per-call
/// cache-key field set can remain stable while the host learns about another
/// provider/router input that must invalidate a whole replay prefix.
pub const EXECUTION_FINGERPRINT_VERSION: u32 = 1;

/// Prefix on every hash string this module emits (`"blake3:<hex>"`), matching
/// the §7 journal samples (`"key":"blake3:..."`).
const HASH_PREFIX: &str = "blake3:";

/// The inputs that determine an `agent()` invocation's cache key.
///
/// This is exactly the §7 field set — `prompt` plus the cache-relevant `opts`.
/// `label` and `phase` are intentionally **absent**: they are progress-display
/// cosmetics and must never bust cache, so they are unrepresentable here.
#[derive(Debug, Clone, Copy)]
pub struct KeyInputs<'a> {
    /// The prompt text handed to the agent.
    pub prompt: &'a str,
    /// Requested model slug, if any.
    pub model: Option<&'a str>,
    /// Requested reasoning effort, if any.
    pub effort: Option<&'a str>,
    /// Requested agent template / type, if any.
    pub agent_type: Option<&'a str>,
    /// Requested isolation mode, if any.
    pub isolation: Option<&'a str>,
    /// Structured-output JSON schema, if any. Canonicalized before hashing so
    /// property ordering never affects the key.
    pub schema: Option<&'a Value>,
}

impl KeyInputs<'_> {
    /// Compute the `blake3:<hex>` cache key for these inputs at the current
    /// [`KEY_ALGO_VERSION`].
    pub fn cache_key(&self) -> String {
        self.cache_key_with_version(KEY_ALGO_VERSION)
    }

    /// Version-parameterized core of [`cache_key`](Self::cache_key). Private so
    /// production always hashes at [`KEY_ALGO_VERSION`]; tests exercise a bump
    /// by passing a different version explicitly.
    fn cache_key_with_version(&self, key_algo_version: u32) -> String {
        let canonical = self.canonical_json();
        let mut hasher = Hasher::new();
        // Domain-separate by version so a bump shifts every key. The version is
        // a fixed 4-byte little-endian prefix, unambiguous against the JSON
        // bytes that follow.
        hasher.update(&key_algo_version.to_le_bytes());
        hasher.update(canonical.as_bytes());
        format!("{HASH_PREFIX}{}", hasher.finalize().to_hex())
    }

    /// The canonical JSON string hashed by [`cache_key`](Self::cache_key):
    /// `{prompt, model, effort, agentType, isolation, schema}` with recursively
    /// sorted object keys. `None` options serialize as explicit `null` so the
    /// canonical shape is fixed regardless of which fields are present.
    fn canonical_json(&self) -> String {
        let obj = json!({
            "prompt": self.prompt,
            "model": self.model,
            "effort": self.effort,
            "agentType": self.agent_type,
            "isolation": self.isolation,
            "schema": self.schema,
        });
        let mut out = String::new();
        write_canonical(&obj, &mut out);
        out
    }
}

/// Hash of the raw prompt text, as `"blake3:<hex>"`.
///
/// Recorded on each `agent_call` line's `prompt_hash`. This is a plain content
/// hash of the prompt bytes — it is intentionally *not* version-prefixed (it is
/// a content fingerprint, not a cache key).
pub fn prompt_hash(prompt: &str) -> String {
    format!("{HASH_PREFIX}{}", blake3::hash(prompt.as_bytes()).to_hex())
}

/// Hash of a structured-output schema, as `"blake3:<hex>"`.
///
/// Recorded on each `agent_call` line's `opts.schema_hash`. Uses the same
/// canonicalization as the cache key so property ordering does not change the
/// hash; like [`prompt_hash`], it is a content fingerprint and is not
/// version-prefixed.
pub fn schema_hash(schema: &Value) -> String {
    canonical_value_hash(schema)
}

/// Hash of an arbitrary JSON value after recursively sorting every object key.
///
/// This is suitable for structural fingerprints such as workflow arguments,
/// where equivalent objects must hash identically regardless of insertion or
/// serialization order. Array order remains significant.
pub fn canonical_value_hash(value: &Value) -> String {
    let canonical = canonical_value_json(value);
    format!(
        "{HASH_PREFIX}{}",
        blake3::hash(canonical.as_bytes()).to_hex()
    )
}

/// Canonical JSON bytes for a structured value.
///
/// Kept crate-private so durable workflow artifacts can persist the exact byte
/// representation used by [`canonical_value_hash`] without expanding this
/// crate's public API.
pub(crate) fn canonical_value_json(value: &Value) -> String {
    let mut canonical = String::new();
    write_canonical(value, &mut canonical);
    canonical
}

/// Hash a host-selected execution environment for run-level replay safety.
///
/// Callers should pass a JSON object containing only non-secret, execution-
/// relevant facts (for example the effective model/provider identity, reasoning
/// effort, service tier, and role configuration). The canonical encoding makes
/// map insertion order irrelevant. A dedicated domain and version prevent this
/// fingerprint from being confused with workflow arguments or schemas.
pub fn execution_fingerprint(value: &Value) -> String {
    let mut canonical = String::new();
    write_canonical(value, &mut canonical);

    let mut hasher = Hasher::new();
    hasher.update(b"codex-workflow-execution-fingerprint\0");
    hasher.update(&EXECUTION_FINGERPRINT_VERSION.to_le_bytes());
    hasher.update(canonical.as_bytes());
    format!("{HASH_PREFIX}{}", hasher.finalize().to_hex())
}

/// Append a canonical JSON encoding of `value` to `out`.
///
/// Canonical means: object keys emitted in sorted (Unicode scalar / UTF-8 byte)
/// order, arrays in element order, strings escaped exactly as `serde_json`
/// would, numbers via `serde_json::Number`'s stable `Display`. The output does
/// not depend on any `serde_json` feature flag (notably `preserve_order`).
fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        // `serde_json::Number`'s `Display` is a stable canonical rendering.
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_json_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // Sort keys so serialization order (e.g. `preserve_order`'s
            // insertion order) never leaks into the hash.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(key, out);
                out.push(':');
                // Present because we iterate the map's own keys.
                if let Some(child) = map.get(key) {
                    write_canonical(child, out);
                }
            }
            out.push('}');
        }
    }
}

/// Append `s` as a JSON string literal (quoted, escaped) to `out`.
///
/// Matches `serde_json`'s default escaping: `"` and `\` are escaped, the short
/// escapes `\b \f \n \r \t` are used, other control characters below `0x20`
/// become `\u00XX`, and everything else (including non-ASCII and `/`) is emitted
/// verbatim. Hand-rolled to avoid a fallible `serde_json::to_string` call on the
/// canonical path (and its `unwrap`/`expect`).
fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Base inputs used by the discrimination tests; helpers tweak one field.
    fn base() -> KeyInputs<'static> {
        KeyInputs {
            prompt: "review the diff",
            model: Some("gpt-5"),
            effort: Some("high"),
            agent_type: Some("reviewer"),
            isolation: None,
            schema: None,
        }
    }

    #[test]
    fn key_algo_version_is_exported() {
        // Compile-time proof the constant exists and is a u32; recorded in
        // run_meta.key_algo_version.
        let _: u32 = KEY_ALGO_VERSION;
        assert_eq!(KEY_ALGO_VERSION, 1);
    }

    #[test]
    fn execution_fingerprint_is_canonical_versioned_and_domain_separated() {
        let a = json!({
            "provider": { "id": "headroom", "headerNames": ["x-router"] },
            "model": "gpt-5",
        });
        let mut provider = serde_json::Map::new();
        provider.insert("headerNames".to_string(), json!(["x-router"]));
        provider.insert("id".to_string(), json!("headroom"));
        let mut reordered = serde_json::Map::new();
        reordered.insert("model".to_string(), json!("gpt-5"));
        reordered.insert("provider".to_string(), Value::Object(provider));

        assert_eq!(
            execution_fingerprint(&a),
            execution_fingerprint(&Value::Object(reordered))
        );
        assert_ne!(execution_fingerprint(&a), canonical_value_hash(&a));
        assert_eq!(EXECUTION_FINGERPRINT_VERSION, 1);
    }

    #[test]
    fn same_inputs_produce_the_same_key() {
        let a = base();
        let b = base();
        assert_eq!(a.cache_key(), b.cache_key());
        // Stable across repeated calls (no hidden state / wall-clock).
        assert_eq!(a.cache_key(), a.cache_key());
        assert!(a.cache_key().starts_with("blake3:"));
    }

    #[test]
    fn write_canonical_emits_sorted_keys_at_every_depth() {
        // Build the map with keys inserted in NON-sorted order. Whether
        // serde_json's `Map` is an insertion-ordered `IndexMap` (with the
        // `preserve_order` feature, active in a full-workspace build) or a
        // sorted `BTreeMap` (default, e.g. this crate built in isolation),
        // `write_canonical` must emit keys in sorted order — this is the
        // property the cache key relies on and it must not depend on that
        // feature-unification state.
        let mut inner = serde_json::Map::new();
        inner.insert("b".to_string(), json!(2));
        inner.insert("a".to_string(), json!(1));
        let mut outer = serde_json::Map::new();
        outer.insert("z".to_string(), Value::Object(inner));
        outer.insert("m".to_string(), json!([json!({"y": 1, "x": 2})]));
        outer.insert("a".to_string(), json!(true));
        let value = Value::Object(outer);

        let mut out = String::new();
        write_canonical(&value, &mut out);
        assert_eq!(
            out,
            // Top-level a<m<z; nested a<b; array element x<y; array order kept.
            r#"{"a":true,"m":[{"x":2,"y":1}],"z":{"a":1,"b":2}}"#,
        );
    }

    #[test]
    fn object_key_order_does_not_change_the_key() {
        // Two schemas that are identical except for JSON object key order. With
        // serde_json's `preserve_order` feature active workspace-wide, these
        // parse into differently-ordered maps; without it they parse into the
        // same sorted map. Either way, canonicalization erases key order and the
        // resulting keys are identical.
        let schema_a: Value =
            serde_json::from_str(r#"{"type":"object","properties":{"a":{},"b":{}}}"#).unwrap();
        let schema_b: Value =
            serde_json::from_str(r#"{"properties":{"b":{},"a":{}},"type":"object"}"#).unwrap();

        let mut ca = String::new();
        let mut cb = String::new();
        write_canonical(&schema_a, &mut ca);
        write_canonical(&schema_b, &mut cb);
        assert_eq!(ca, cb, "canonical form is key-order invariant");

        let ia = KeyInputs {
            schema: Some(&schema_a),
            ..base()
        };
        let ib = KeyInputs {
            schema: Some(&schema_b),
            ..base()
        };
        assert_eq!(ia.cache_key(), ib.cache_key());
        assert_eq!(schema_hash(&schema_a), schema_hash(&schema_b));
    }

    #[test]
    fn nested_schema_serialization_order_does_not_change_the_key() {
        // Deeper nesting + arrays: order differs at every object level; arrays
        // keep their (semantically-significant) element order.
        let schema_a: Value = serde_json::from_str(
            r#"{"type":"object","required":["x","y"],"properties":{"x":{"type":"string","enum":["p","q"]},"y":{"type":"number"}}}"#,
        )
        .unwrap();
        let schema_b: Value = serde_json::from_str(
            r#"{"properties":{"y":{"type":"number"},"x":{"enum":["p","q"],"type":"string"}},"required":["x","y"],"type":"object"}"#,
        )
        .unwrap();
        let ia = KeyInputs {
            schema: Some(&schema_a),
            ..base()
        };
        let ib = KeyInputs {
            schema: Some(&schema_b),
            ..base()
        };
        assert_eq!(ia.cache_key(), ib.cache_key());
    }

    #[test]
    fn label_and_phase_are_not_part_of_the_key() {
        // `label`/`phase` are structurally excluded — they aren't fields of
        // KeyInputs. Cosmetic re-labeling therefore cannot change the key: two
        // invocations that would carry different labels/phases in the journal
        // but share identical (prompt, opts) hash identically.
        let a = base();
        let b = base();
        assert_eq!(
            a.cache_key(),
            b.cache_key(),
            "identical (prompt, opts) key identically regardless of any label/phase",
        );
    }

    #[test]
    fn different_prompt_changes_the_key() {
        let a = base();
        let b = KeyInputs {
            prompt: "review the OTHER diff",
            ..base()
        };
        assert_ne!(a.cache_key(), b.cache_key());
    }

    #[test]
    fn different_model_changes_the_key() {
        let a = base();
        let b = KeyInputs {
            model: Some("gpt-4"),
            ..base()
        };
        assert_ne!(a.cache_key(), b.cache_key());
        // Present vs. absent also differs.
        let none = KeyInputs {
            model: None,
            ..base()
        };
        assert_ne!(a.cache_key(), none.cache_key());
    }

    #[test]
    fn different_effort_changes_the_key() {
        let a = base();
        let b = KeyInputs {
            effort: Some("low"),
            ..base()
        };
        assert_ne!(a.cache_key(), b.cache_key());
    }

    #[test]
    fn different_agent_type_changes_the_key() {
        let a = base();
        let b = KeyInputs {
            agent_type: Some("planner"),
            ..base()
        };
        assert_ne!(a.cache_key(), b.cache_key());
    }

    #[test]
    fn different_isolation_changes_the_key() {
        let a = base();
        let b = KeyInputs {
            isolation: Some("worktree"),
            ..base()
        };
        assert_ne!(a.cache_key(), b.cache_key());
    }

    #[test]
    fn different_schema_changes_the_key() {
        let schema_a: Value = serde_json::from_str(r#"{"type":"object"}"#).unwrap();
        let schema_b: Value = serde_json::from_str(r#"{"type":"array"}"#).unwrap();
        let a = KeyInputs {
            schema: Some(&schema_a),
            ..base()
        };
        let b = KeyInputs {
            schema: Some(&schema_b),
            ..base()
        };
        assert_ne!(a.cache_key(), b.cache_key());
        // Present vs. absent schema differs.
        assert_ne!(a.cache_key(), base().cache_key());
    }

    #[test]
    fn bumping_key_algo_version_changes_the_key_for_the_same_input() {
        let inputs = base();
        let v1 = inputs.cache_key_with_version(1);
        let v2 = inputs.cache_key_with_version(2);
        let v3 = inputs.cache_key_with_version(3);
        assert_ne!(v1, v2, "a version bump must shift the key");
        assert_ne!(v2, v3);
        assert_ne!(v1, v3);
        // The public key is the current-version key.
        assert_eq!(
            inputs.cache_key(),
            inputs.cache_key_with_version(KEY_ALGO_VERSION)
        );
    }

    #[test]
    fn prompt_hash_is_deterministic_and_discriminating() {
        assert_eq!(prompt_hash("hello"), prompt_hash("hello"));
        assert_ne!(prompt_hash("hello"), prompt_hash("world"));
        assert!(prompt_hash("hello").starts_with("blake3:"));
    }

    #[test]
    fn schema_hash_is_deterministic_and_key_order_invariant() {
        let a: Value = serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap();
        let c: Value = serde_json::from_str(r#"{"a":1,"b":3}"#).unwrap();
        assert_eq!(schema_hash(&a), schema_hash(&b), "key-order invariant");
        assert_ne!(schema_hash(&a), schema_hash(&c), "value change is detected");
        assert!(schema_hash(&a).starts_with("blake3:"));
    }

    #[test]
    fn canonical_value_hash_is_nested_key_order_invariant_but_array_order_sensitive() {
        let a: Value =
            serde_json::from_str(r#"{"outer":{"a":1,"b":2},"items":[{"x":3,"y":4},5]}"#).unwrap();
        let reordered_objects: Value =
            serde_json::from_str(r#"{"items":[{"y":4,"x":3},5],"outer":{"b":2,"a":1}}"#).unwrap();
        let reordered_array: Value =
            serde_json::from_str(r#"{"outer":{"a":1,"b":2},"items":[5,{"x":3,"y":4}]}"#).unwrap();

        assert_eq!(
            canonical_value_hash(&a),
            canonical_value_hash(&reordered_objects)
        );
        assert_ne!(
            canonical_value_hash(&a),
            canonical_value_hash(&reordered_array)
        );
    }

    #[test]
    fn string_escaping_matches_serde_json() {
        // The hand-rolled escaper must agree with serde_json for the canonical
        // bytes to be a faithful canonicalization.
        for s in [
            "plain",
            "with \"quotes\"",
            "back\\slash",
            "tab\tnewline\nreturn\r",
            "control\u{0008}\u{000C}\u{0001}",
            "unicode: café — 日本語 😀",
            "slash/not-escaped",
        ] {
            let mut mine = String::new();
            write_json_string(s, &mut mine);
            let theirs = serde_json::to_string(&Value::String(s.to_string())).unwrap();
            assert_eq!(mine, theirs, "escaping mismatch for {s:?}");
        }
    }

    #[test]
    fn canonical_json_sorts_top_level_keys() {
        // Documents the exact canonical bytes for a fixed input so a silent
        // change to the algorithm is caught (and would require a version bump).
        let inputs = KeyInputs {
            prompt: "p",
            model: Some("m"),
            effort: Some("e"),
            agent_type: Some("t"),
            isolation: Some("i"),
            schema: None,
        };
        assert_eq!(
            inputs.canonical_json(),
            r#"{"agentType":"t","effort":"e","isolation":"i","model":"m","prompt":"p","schema":null}"#,
        );
    }
}
