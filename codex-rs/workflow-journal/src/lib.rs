//! Serde types for the dynamic-workflows journal (`journal.jsonl`).
//!
//! The journal is the authoritative run→agent link and the source of truth for
//! prefix-replay. See `docs/dynamic-workflows-spec.md` §7 ("Journal format").
//!
//! These are deliberately **standalone** types. We do NOT extend
//! [`codex_protocol::protocol::RolloutItem`] (`protocol.rs:3141`): that enum is
//! conversation-shaped and every rollout consumer would have to learn to handle
//! new variants. The journal is its own append-only file with its own envelope.
//!
//! This crate defines only the wire types plus their serde round-trip tests.
//! The recorder, replay reader, and cache-key hashing live in sibling modules
//! added by later tickets.

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::IgnoredAny;
use serde_json::Value;

/// Zero-sized marker for the `ordinal` field on `phase`/`log` lines, which §7
/// requires to be **always `null`** (only `agent_call` lines carry a numeric
/// ordinal). Modeling it as `Option<u64>` would let callers serialize
/// `ordinal:7`, which §7 forbids; this type makes the null-only invariant
/// unrepresentable otherwise.
///
/// It always serializes as `null` and only deserializes from an explicit
/// `null`, so the wire bytes stay identical to the §7 samples
/// (`{"type":"phase","ordinal":null,...}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NullOrdinal;

impl Serialize for NullOrdinal {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_none()
    }
}

impl<'de> Deserialize<'de> for NullOrdinal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Accept only an explicit JSON `null`; any concrete value is rejected so
        // the §7 "ordinal must be null on phase/log lines" invariant cannot be
        // violated on the wire.
        match Option::<IgnoredAny>::deserialize(deserializer)? {
            None => Ok(NullOrdinal),
            Some(_) => Err(serde::de::Error::custom(
                "ordinal must be null on phase/log journal lines",
            )),
        }
    }
}

/// Marker for the `type` discriminant on the run-meta (line 0) record.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RunMetaTag {
    /// The one and only value: serializes to `"run_meta"`.
    #[default]
    RunMeta,
}

/// Line 0 of `journal.jsonl`: run-level metadata.
///
/// Matches the §7 sample:
/// ```text
/// {"type":"run_meta","run_id":"...","parent_run_id":null,"script_hash":"...","args_hash":"...","name":"triage","budget_total":500000,"key_algo_version":1,"created_at":"..."}
/// ```
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunMeta {
    /// Discriminant; always [`RunMetaTag::RunMeta`].
    #[serde(rename = "type")]
    pub kind: RunMetaTag,
    /// Identifier of this run.
    pub run_id: String,
    /// Parent run id when this run was spawned via `workflow()`; otherwise null.
    pub parent_run_id: Option<String>,
    /// Hash of the executed script (structural-change detector for replay).
    pub script_hash: String,
    /// Hash of the run arguments (structural-change detector for replay).
    pub args_hash: String,
    /// Human-facing workflow name.
    pub name: String,
    /// Total token budget granted to the run.
    pub budget_total: u64,
    /// Version of the cache-key algorithm; lets hash changes across Codex
    /// versions be detected on resume.
    pub key_algo_version: u32,
    /// Host-supplied creation timestamp (never the isolate).
    pub created_at: String,
}

impl WorkflowRunMeta {
    /// Construct a run-meta record with the discriminant pre-filled.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run_id: String,
        parent_run_id: Option<String>,
        script_hash: String,
        args_hash: String,
        name: String,
        budget_total: u64,
        key_algo_version: u32,
        created_at: String,
    ) -> Self {
        Self {
            kind: RunMetaTag::RunMeta,
            run_id,
            parent_run_id,
            script_hash,
            args_hash,
            name,
            budget_total,
            key_algo_version,
            created_at,
        }
    }
}

/// A journal line after line 0. Internally tagged on `type`; every variant
/// shares the `{timestamp, ordinal, type, ...}` envelope described in §7.
///
/// `timestamp` is host-supplied and optional on the wire (the §7 samples omit
/// it), so it is skipped when absent to keep byte-stable round-trips against
/// those samples.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalLine {
    /// A single `agent()` invocation and its recorded outcome.
    ///
    /// Boxed because this variant is much larger than the narration variants.
    AgentCall(Box<AgentCallLine>),
    /// A `phase()` narration marker.
    Phase(PhaseLine),
    /// A `log()` narration line.
    Log(LogLine),
}

impl JournalLine {
    /// Enforce the status-dependent replay/linkage invariants that §7's serde
    /// shapes are too permissive to encode in the types alone.
    ///
    /// Only `agent_call` lines carry invariants today; `phase`/`log` lines
    /// always pass. See [`AgentCallLine::validate`] for the rules.
    ///
    /// The recorder (a later ticket) MUST call this before appending an
    /// `agent_call` line — a `completed` entry that fails validation would
    /// silently break budget re-add and journal-only transcript grouping on
    /// resume.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            JournalLine::AgentCall(call) => call.validate(),
            JournalLine::Phase(_) | JournalLine::Log(_) => Ok(()),
        }
    }
}

/// Completion status of an `agent_call`. Serializes to `completed | error`;
/// the third state — "in flight / unknown" — is represented by `null`
/// (`Option::None`) on the [`AgentCallLine::status`] field.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// The agent finished and produced a return value.
    Completed,
    /// The agent errored.
    Error,
}

/// The `opts` sub-object recorded on an `agent_call`.
///
/// Note the intentionally mixed casing to match the §7 sample: `agentType` is
/// camelCase while the surrounding fields are snake_case.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentCallOpts {
    /// Model slug requested for the agent.
    pub model: Option<String>,
    /// Reasoning effort requested for the agent.
    pub effort: Option<String>,
    /// Agent template / type.
    #[serde(rename = "agentType")]
    pub agent_type: Option<String>,
    /// Isolation mode (e.g. `"worktree"`); null for the default.
    pub isolation: Option<String>,
    /// Hash of the structured-output schema, if any.
    pub schema_hash: Option<String>,
}

/// The `agent_call` journal line.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AgentCallLine {
    /// Host-supplied timestamp; omitted on the wire when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Invocation ordinal — the spine of prefix-replay.
    pub ordinal: u64,
    /// The `(prompt, opts)` cache key (e.g. `"blake3:..."`).
    pub key: String,
    /// Hash of the prompt text.
    pub prompt_hash: String,
    /// Recorded options for this call.
    pub opts: AgentCallOpts,
    /// Progress-attribution phase, if any.
    pub phase: Option<String>,
    /// Progress-attribution label, if any.
    pub label: Option<String>,
    /// Thread id of the spawned child agent.
    pub child_thread_id: Option<String>,
    /// Absolute path of the child's rollout session file.
    pub rollout_path: Option<String>,
    /// Completion status; `null` while unknown/in-flight.
    pub status: Option<AgentStatus>,
    /// Return value: losslessly round-trips a string, an object, or `null`.
    #[serde(rename = "return")]
    pub ret: Value,
    /// Tokens spent by the agent.
    pub tokens_spent: Option<u64>,
    /// Order in which concurrent agents completed (recorded defensively).
    pub completion_seq: Option<u64>,
}

impl AgentCallLine {
    /// Enforce the status-dependent invariants that §7's nullable serde shapes
    /// cannot express in the types.
    ///
    /// A `status:"completed"` line is a replay/linkage anchor and MUST carry:
    /// - `tokens_spent` — the resume path re-adds it to the budget counter so
    ///   `spent()`/`remaining()` land at the identical ordinal (§7 resume step
    ///   3); a null here would silently under-count the budget on resume.
    /// - `child_thread_id` **and** `rollout_path` — the authoritative
    ///   journal-only run→agent link (§7 "authoritative run→agent link"); a
    ///   null here would break transcript grouping from the journal alone.
    ///
    /// `error` and in-flight (`null`) lines are exempt — an errored or
    /// unfinished call legitimately lacks a return value, token count, and
    /// child linkage.
    ///
    /// The recorder (a later ticket) MUST call this before appending.
    pub fn validate(&self) -> Result<(), String> {
        if self.status == Some(AgentStatus::Completed) {
            if self.tokens_spent.is_none() {
                return Err(format!(
                    "agent_call ordinal {}: status=completed requires tokens_spent",
                    self.ordinal
                ));
            }
            if self.child_thread_id.is_none() {
                return Err(format!(
                    "agent_call ordinal {}: status=completed requires child_thread_id",
                    self.ordinal
                ));
            }
            if self.rollout_path.is_none() {
                return Err(format!(
                    "agent_call ordinal {}: status=completed requires rollout_path",
                    self.ordinal
                ));
            }
        }
        Ok(())
    }
}

/// The `phase` journal line. Its `ordinal` is always null.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PhaseLine {
    /// Host-supplied timestamp; omitted on the wire when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Always `null` for phase lines (present in the envelope for symmetry).
    pub ordinal: NullOrdinal,
    /// The phase title.
    pub title: String,
}

/// The `log` journal line. Its `ordinal` is always null.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// Host-supplied timestamp; omitted on the wire when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Always `null` for log lines (present in the envelope for symmetry).
    pub ordinal: NullOrdinal,
    /// The narrator message.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    /// Assert that a value survives serialize→deserialize→serialize byte-stably.
    fn assert_byte_stable<T>(value: &T)
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let s1 = serde_json::to_string(value).expect("serialize");
        let back: T = serde_json::from_str(&s1).expect("deserialize");
        let s2 = serde_json::to_string(&back).expect("re-serialize");
        assert_eq!(s1, s2, "byte-stable round-trip");
    }

    fn sample_agent_call() -> AgentCallLine {
        AgentCallLine {
            timestamp: None,
            ordinal: 0,
            key: "blake3:abc".to_string(),
            prompt_hash: "ph".to_string(),
            opts: AgentCallOpts {
                model: Some("gpt".to_string()),
                effort: Some("high".to_string()),
                agent_type: Some("reviewer".to_string()),
                isolation: None,
                schema_hash: Some("sh".to_string()),
            },
            phase: Some("analyze".to_string()),
            label: Some("file-a".to_string()),
            child_thread_id: Some("th_1".to_string()),
            rollout_path: Some("/home/u/.codex/sessions/rollout.jsonl".to_string()),
            status: Some(AgentStatus::Completed),
            ret: json!({"ok": true}),
            tokens_spent: Some(8123),
            completion_seq: Some(2),
        }
    }

    #[test]
    fn run_meta_round_trips_byte_stably() {
        let meta = WorkflowRunMeta::new(
            "run_1".to_string(),
            None,
            "sh".to_string(),
            "ah".to_string(),
            "triage".to_string(),
            500_000,
            1,
            "2026-07-16T00:00:00Z".to_string(),
        );
        assert_byte_stable(&meta);

        let with_parent = WorkflowRunMeta::new(
            "run_2".to_string(),
            Some("run_1".to_string()),
            "sh".to_string(),
            "ah".to_string(),
            "sub".to_string(),
            10,
            3,
            "2026-07-16T00:00:01Z".to_string(),
        );
        assert_byte_stable(&with_parent);
    }

    #[test]
    fn agent_call_round_trips_byte_stably() {
        assert_byte_stable(&JournalLine::AgentCall(Box::new(sample_agent_call())));

        // With a host timestamp present and null-ish optional fields.
        let mut c = sample_agent_call();
        c.timestamp = Some("2026-07-16T00:00:02Z".to_string());
        c.status = None;
        c.phase = None;
        c.label = None;
        c.tokens_spent = None;
        c.completion_seq = None;
        assert_byte_stable(&JournalLine::AgentCall(Box::new(c)));
    }

    #[test]
    fn phase_and_log_round_trip_byte_stably() {
        assert_byte_stable(&JournalLine::Phase(PhaseLine {
            timestamp: None,
            ordinal: NullOrdinal,
            title: "analyze".to_string(),
        }));
        assert_byte_stable(&JournalLine::Log(LogLine {
            timestamp: Some("2026-07-16T00:00:03Z".to_string()),
            ordinal: NullOrdinal,
            message: "narrator line".to_string(),
        }));
    }

    #[test]
    fn return_field_losslessly_round_trips_string_object_and_null() {
        for value in [
            json!("a plain string"),
            json!({"nested": {"deep": [1, 2, 3]}, "flag": true}),
            Value::Null,
        ] {
            let mut c = sample_agent_call();
            c.ret = value.clone();
            let line = JournalLine::AgentCall(Box::new(c));
            let s = serde_json::to_string(&line).expect("serialize");
            let back: JournalLine = serde_json::from_str(&s).expect("deserialize");
            let JournalLine::AgentCall(back_call) = back else {
                panic!("expected agent_call");
            };
            assert_eq!(back_call.ret, value, "return round-trips losslessly");
            assert_byte_stable(&line);
        }
    }

    #[test]
    fn matches_spec_section_7_run_meta_sample() {
        let sample = r#"{"type":"run_meta","run_id":"r","parent_run_id":null,"script_hash":"s","args_hash":"a","name":"triage","budget_total":500000,"key_algo_version":1,"created_at":"2026-07-16T00:00:00Z"}"#;
        let parsed: WorkflowRunMeta = serde_json::from_str(sample).expect("parse");
        let reserialized = serde_json::to_string(&parsed).expect("serialize");
        assert_eq!(reserialized, sample, "field names/casing/order match §7");
    }

    #[test]
    fn matches_spec_section_7_journal_line_samples() {
        let samples = [
            r#"{"type":"agent_call","ordinal":0,"key":"blake3:x","prompt_hash":"ph","opts":{"model":"gpt","effort":"high","agentType":"reviewer","isolation":null,"schema_hash":"sh"},"phase":"analyze","label":"file-a","child_thread_id":"th_1","rollout_path":"/p/rollout.jsonl","status":"completed","return":{"k":"v"},"tokens_spent":8123,"completion_seq":2}"#,
            r#"{"type":"phase","ordinal":null,"title":"analyze"}"#,
            r#"{"type":"log","ordinal":null,"message":"narrator line"}"#,
        ];
        for sample in samples {
            let parsed: JournalLine = serde_json::from_str(sample).expect("parse");
            let reserialized = serde_json::to_string(&parsed).expect("serialize");
            assert_eq!(reserialized, sample, "field names/casing/order match §7");
        }
    }

    #[test]
    fn null_ordinal_serializes_as_json_null() {
        let line = JournalLine::Phase(PhaseLine {
            timestamp: None,
            ordinal: NullOrdinal,
            title: "analyze".to_string(),
        });
        let s = serde_json::to_string(&line).expect("serialize");
        assert_eq!(s, r#"{"type":"phase","ordinal":null,"title":"analyze"}"#);
    }

    #[test]
    fn null_ordinal_rejects_non_null_values() {
        // A phase line that (illegally) carries a numeric ordinal must fail to
        // deserialize — the §7 invariant is unrepresentable, not merely unused.
        let bad = r#"{"type":"phase","ordinal":7,"title":"analyze"}"#;
        let err = serde_json::from_str::<JournalLine>(bad)
            .expect_err("numeric ordinal on a phase line must be rejected");
        assert!(
            err.to_string().contains("ordinal must be null"),
            "unexpected error: {err}"
        );

        let bad_log = r#"{"type":"log","ordinal":0,"message":"x"}"#;
        serde_json::from_str::<JournalLine>(bad_log)
            .expect_err("numeric ordinal on a log line must be rejected");
    }

    #[test]
    fn null_ordinal_absent_is_read_as_null_and_re_emitted_as_null() {
        // serde's internally-tagged (`#[serde(tag = "type")]`) enum routes a
        // missing field through a none-yielding deserializer, so an absent
        // `ordinal` is accepted as `NullOrdinal` rather than erroring. That is
        // fine: the invariant §7 cares about is that a *non*-null ordinal is
        // impossible (covered above) and that we always *emit* `ordinal:null`.
        let missing = r#"{"type":"phase","title":"analyze"}"#;
        let parsed: JournalLine = serde_json::from_str(missing).expect("parse");
        let reserialized = serde_json::to_string(&parsed).expect("serialize");
        assert_eq!(
            reserialized, r#"{"type":"phase","ordinal":null,"title":"analyze"}"#,
            "output always carries an explicit ordinal:null matching §7"
        );
    }

    #[test]
    fn validate_accepts_a_well_formed_completed_call() {
        let line = JournalLine::AgentCall(Box::new(sample_agent_call()));
        assert!(line.validate().is_ok());
        // Phase/log lines have no invariants and always pass.
        assert!(
            JournalLine::Phase(PhaseLine {
                timestamp: None,
                ordinal: NullOrdinal,
                title: "p".to_string(),
            })
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn validate_rejects_completed_call_missing_replay_or_linkage_fields() {
        // completed + tokens_spent:null → budget re-add on resume would break.
        let mut missing_tokens = sample_agent_call();
        missing_tokens.tokens_spent = None;
        let err = missing_tokens
            .validate()
            .expect_err("must reject null tokens");
        assert!(err.contains("tokens_spent"), "unexpected error: {err}");

        // completed + missing child_thread_id → journal-only grouping breaks.
        let mut missing_child = sample_agent_call();
        missing_child.child_thread_id = None;
        let err = missing_child
            .validate()
            .expect_err("must reject missing child_thread_id");
        assert!(err.contains("child_thread_id"), "unexpected error: {err}");

        // completed + missing rollout_path → journal-only grouping breaks.
        let mut missing_rollout = sample_agent_call();
        missing_rollout.rollout_path = None;
        let err = missing_rollout
            .validate()
            .expect_err("must reject missing rollout_path");
        assert!(err.contains("rollout_path"), "unexpected error: {err}");

        // The same defect surfaces through the JournalLine wrapper.
        let via_wrapper = JournalLine::AgentCall(Box::new(missing_tokens));
        assert!(via_wrapper.validate().is_err());
    }

    #[test]
    fn validate_exempts_error_and_in_flight_calls() {
        // An errored call legitimately lacks tokens/linkage/return.
        let mut errored = sample_agent_call();
        errored.status = Some(AgentStatus::Error);
        errored.tokens_spent = None;
        errored.child_thread_id = None;
        errored.rollout_path = None;
        errored.ret = Value::Null;
        assert!(errored.validate().is_ok(), "error lines are exempt");

        // An in-flight (status:null) call is likewise exempt.
        let mut in_flight = sample_agent_call();
        in_flight.status = None;
        in_flight.tokens_spent = None;
        in_flight.child_thread_id = None;
        in_flight.rollout_path = None;
        assert!(in_flight.validate().is_ok(), "in-flight lines are exempt");
    }
}
