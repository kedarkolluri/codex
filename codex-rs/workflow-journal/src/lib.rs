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
//! This crate defines the wire types plus their serde round-trip tests, and the
//! canonical `(prompt, opts)` cache key ([`mod@key`]). The recorder and replay
//! reader live in sibling modules added by later tickets.

pub mod key;
pub mod lease;
mod private_fs;
pub mod recorder;
mod recovery_cursor;
pub mod replay;

pub use key::EXECUTION_FINGERPRINT_VERSION;
pub use key::KEY_ALGO_VERSION;
pub use key::KeyInputs;
pub use key::canonical_value_hash;
pub use key::execution_fingerprint;
pub use key::prompt_hash;
pub use key::schema_hash;
pub use lease::WorkflowRunLease;
pub use lease::WorkflowRunLeaseAcquire;
pub use recorder::JournalRecorder;
pub use recovery_cursor::WorkflowRecoveryCursor;
pub use recovery_cursor::WorkflowRecoveryCursorGuard;
pub use recovery_cursor::WorkflowRecoveryCursorInvalidContent;
pub use recovery_cursor::WorkflowRecoveryCursorRead;
pub use replay::Divergence;
pub use replay::ReplayEntry;
pub use replay::ReplayJournal;
pub use replay::RunAgentJournal;
pub use replay::RunAgentLink;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::IgnoredAny;
use serde_json::Value;

pub mod storage;

/// Maximum serialized byte length of one workflow agent return value.
///
/// Returns are durable replay inputs and can later reach model-visible workflow output. This cap
/// keeps each such fragment below the repository's 10K-token context limit with safety margin.
pub const WORKFLOW_AGENT_RETURN_MAX_BYTES: usize = 32 * 1024;

/// Maximum serialized byte length of one journal record, excluding its trailing newline.
pub(crate) const WORKFLOW_JOURNAL_RECORD_MAX_BYTES: usize = 128 * 1024;

/// Maximum total byte length of one run journal.
pub(crate) const WORKFLOW_JOURNAL_MAX_BYTES: u64 = 192 * 1024 * 1024;

/// Maximum number of nonblank records a journal reader will scan.
///
/// One maximally admitted run can write 57,001 records: one header; 4,000 agents with six
/// attempts, one binding and at most one cleanup diagnostic per attempt, and one terminal record;
/// plus 4,000 workflow logs and 1,000 phases. The remaining 2,999 records are maintenance
/// headroom, while [`WORKFLOW_JOURNAL_MAX_BYTES`] remains the independent total-byte bound.
pub const WORKFLOW_JOURNAL_MAX_RECORDS: usize = 60_000;

/// Validate a workflow agent return before it is journaled or replayed.
pub fn ensure_workflow_agent_return(value: &Value) -> Result<(), String> {
    let serialized_len = serde_json::to_vec(value)
        .map_err(|error| format!("failed to serialize workflow agent return: {error}"))?
        .len();
    if serialized_len > WORKFLOW_AGENT_RETURN_MAX_BYTES {
        return Err(format!(
            "workflow agent return exceeds the {WORKFLOW_AGENT_RETURN_MAX_BYTES}-byte replay cap"
        ));
    }
    Ok(())
}

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

/// Durable lifecycle status stored in `meta.json` for discovery-index rebuilds.
///
/// The journal's line-zero copy remains the immutable start record; the per-run
/// `meta.json` projection is atomically rewritten once when the run terminates.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    /// The workflow body has started and has not emitted its terminal event yet.
    #[default]
    Running,
    /// The workflow body completed successfully.
    Completed,
    /// The run was explicitly stopped by its owning user session.
    Stopped,
    /// The run reached a controller-requested checkpoint after child cleanup.
    Paused,
    /// The workflow body errored or was interrupted.
    Failed,
}

impl WorkflowRunStatus {
    // Serde's `skip_serializing_if` callback receives the field by reference.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn is_running(&self) -> bool {
        *self == Self::Running
    }
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
    /// Source run whose journal or checkpoint seeded this run; otherwise null.
    ///
    /// This is deliberately distinct from [`Self::parent_run_id`], which only
    /// models runtime `workflow()` nesting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from_run_id: Option<String>,
    /// The one durable successor admitted from this paused checkpoint.
    ///
    /// This field lives on the source run and is claimed while its lease is
    /// held. Once present it is immutable: retries and concurrent callers must
    /// converge on this exact run id instead of minting a fork.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_by_run_id: Option<String>,
    /// Root thread whose session owns mutation authority for this run.
    ///
    /// Missing on legacy runs, which remain readable and recoverable but do not
    /// acquire ownership implicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_thread_id: Option<String>,
    /// Hash of the executed script (structural-change detector for replay).
    pub script_hash: String,
    /// Hash of the run arguments (structural-change detector for replay).
    pub args_hash: String,
    /// Human-facing workflow name.
    pub name: String,
    /// Total token budget granted to the run, or `None` when unmetered.
    ///
    /// Legacy numeric values deserialize as `Some`, including zero. Older
    /// journals cannot distinguish an explicit zero ceiling from a zero that
    /// was used as an unmetered sentinel, so preserving the numeric meaning is
    /// the only backward-compatible interpretation.
    pub budget_total: Option<u64>,
    /// Version of the cache-key algorithm; lets hash changes across Codex
    /// versions be detected on resume.
    pub key_algo_version: u32,
    /// Host-supplied creation timestamp (never the isolate).
    pub created_at: String,
    /// Opaque hash of the non-secret provider/router/model environment that
    /// determines inherited `agent()` execution. Missing on legacy runs; a
    /// current host with a fingerprint treats that as replay divergence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_fingerprint: Option<String>,
    /// Rebuildable lifecycle projection. Missing means `running`, preserving the
    /// original line-zero wire format while terminal `meta.json` writes are explicit.
    #[serde(default, skip_serializing_if = "WorkflowRunStatus::is_running")]
    pub status: WorkflowRunStatus,
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
        budget_total: Option<u64>,
        key_algo_version: u32,
        created_at: String,
    ) -> Self {
        Self {
            kind: RunMetaTag::RunMeta,
            run_id,
            parent_run_id,
            resumed_from_run_id: None,
            resumed_by_run_id: None,
            owner_thread_id: None,
            script_hash,
            args_hash,
            name,
            budget_total,
            key_algo_version,
            created_at,
            execution_fingerprint: None,
            status: WorkflowRunStatus::Running,
        }
    }

    /// Attach the host-computed execution fingerprint persisted for replay
    /// compatibility checks.
    pub fn with_execution_fingerprint(mut self, execution_fingerprint: String) -> Self {
        self.execution_fingerprint = Some(execution_fingerprint);
        self
    }

    /// Attach the root thread whose session owns mutation authority for this run.
    pub fn with_owner_thread_id(mut self, owner_thread_id: String) -> Self {
        self.owner_thread_id = Some(owner_thread_id);
        self
    }

    /// Attach the source run whose journal or checkpoint seeded this fresh run.
    pub fn with_resumed_from_run_id(mut self, resumed_from_run_id: String) -> Self {
        self.resumed_from_run_id = Some(resumed_from_run_id);
        self
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
    /// Durable child-thread binding recorded before the child's first turn starts.
    AgentBound(AgentBoundLine),
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
            JournalLine::AgentBound(bound) => bound.validate(),
            JournalLine::Phase(_) | JournalLine::Log(_) => Ok(()),
        }
    }
}

/// Durable binding between an invocation ordinal and its child transcript.
///
/// This is separate from [`AgentCallLine`] so the binding can be flushed before
/// the child's first turn starts without racing the terminal call record.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AgentBoundLine {
    /// Host-supplied timestamp; omitted on the wire when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// Invocation ordinal shared with the eventual [`AgentCallLine`].
    pub ordinal: u64,
    /// Zero-based attempt generation for this logical invocation.
    ///
    /// Older journals omitted this field and therefore describe the initial attempt.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub attempt: u32,
    /// Registered child thread.
    pub child_thread_id: String,
    /// Absolute host-local path of the child's materialized rollout.
    pub rollout_path: String,
}

impl AgentBoundLine {
    pub fn validate(&self) -> Result<(), String> {
        uuid::Uuid::parse_str(&self.child_thread_id).map_err(|error| {
            format!(
                "agent_bound ordinal {} has invalid child_thread_id: {error}",
                self.ordinal
            )
        })?;
        if !std::path::Path::new(&self.rollout_path).is_absolute() {
            return Err(format!(
                "agent_bound ordinal {} requires an absolute rollout_path",
                self.ordinal
            ));
        }
        Ok(())
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

/// Intentional selected-attempt outcome attached to the one terminal replay anchor.
///
/// This journal-local type keeps persistence independent from `codex-protocol`. A successful
/// retry needs no terminal reason: its nonzero [`AgentCallLine::attempt`] carries the retry
/// generation, while these variants distinguish intentional null settlements from an ordinary
/// agent failure that also returns JavaScript `null`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentControlReason {
    UserSkip,
    RetryLimitReached,
}

/// Aggregate token counters for every attempt of one logical `agent()` invocation.
///
/// Kept as a standalone journal type to avoid coupling durable workflow data to protocol crates.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentTokenUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
}

/// Replay-visible aggregate counters across the initial attempt and every retry.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentCallProgress {
    pub token_usage: AgentTokenUsage,
    pub tool_call_count: u64,
    pub duration_ms: u64,
}

impl AgentCallProgress {
    fn validate(&self, ordinal: u64) -> Result<(), String> {
        let counters = [
            ("input_tokens", self.token_usage.input_tokens),
            ("cached_input_tokens", self.token_usage.cached_input_tokens),
            ("output_tokens", self.token_usage.output_tokens),
            (
                "reasoning_output_tokens",
                self.token_usage.reasoning_output_tokens,
            ),
            ("total_tokens", self.token_usage.total_tokens),
        ];
        if let Some((field, value)) = counters.into_iter().find(|(_, value)| *value < 0) {
            return Err(format!(
                "agent_call ordinal {ordinal}: aggregate {field} must be non-negative, got {value}"
            ));
        }
        Ok(())
    }
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
    /// Zero-based generation that produced this logical call's final outcome.
    ///
    /// Intermediate retries write only [`AgentBoundLine`] records; exactly one terminal call line
    /// remains the replay anchor for the ordinal. Older journals default to the initial attempt.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub attempt: u32,
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
    /// Intentional selected-attempt terminal outcome, when one settled the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_reason: Option<AgentControlReason>,
    /// Return value: losslessly round-trips a string, an object, or `null`.
    #[serde(rename = "return")]
    pub ret: Value,
    /// Tokens spent by the agent.
    pub tokens_spent: Option<u64>,
    /// Aggregate progress for all attempts. Missing on legacy journal records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<AgentCallProgress>,
    /// Order in which concurrent agents completed (recorded defensively).
    pub completion_seq: Option<u64>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_u32(value: &u32) -> bool {
    *value == 0
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
        ensure_workflow_agent_return(&self.ret)?;
        self.validate_structure()
    }

    /// Validate replay and linkage fields independently of the current return-size bound.
    ///
    /// Legacy journals may contain a return that was valid before the bound was
    /// introduced. Link discovery can still use those records, while replay
    /// separately treats that ordinal as the end of the safe cached prefix.
    fn validate_structure(&self) -> Result<(), String> {
        if let Some(progress) = &self.progress {
            progress.validate(self.ordinal)?;
        }
        if self.control_reason.is_some() && self.status != Some(AgentStatus::Completed) {
            return Err(format!(
                "agent_call ordinal {}: control_reason requires status=completed",
                self.ordinal
            ));
        }
        if self.control_reason.is_some() && !self.ret.is_null() {
            return Err(format!(
                "agent_call ordinal {}: control_reason requires return=null",
                self.ordinal
            ));
        }
        if self.control_reason.is_some() && self.progress.is_none() {
            return Err(format!(
                "agent_call ordinal {}: control_reason requires aggregate progress",
                self.ordinal
            ));
        }
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
            attempt: 0,
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
            control_reason: None,
            ret: json!({"ok": true}),
            tokens_spent: Some(8123),
            progress: None,
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
            Some(500_000),
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
            Some(10),
            3,
            "2026-07-16T00:00:01Z".to_string(),
        );
        assert_byte_stable(&with_parent);
    }

    #[test]
    fn run_meta_owner_identity_is_backward_compatible() {
        let legacy_json = json!({
            "type": "run_meta",
            "run_id": "run-legacy",
            "parent_run_id": null,
            "script_hash": "blake3:script",
            "args_hash": "blake3:args",
            "name": "triage",
            "budget_total": 500_000,
            "key_algo_version": 1,
            "created_at": "2026-07-18T00:00:00Z",
        });
        let legacy_meta = WorkflowRunMeta::new(
            "run-legacy".to_string(),
            None,
            "blake3:script".to_string(),
            "blake3:args".to_string(),
            "triage".to_string(),
            Some(500_000),
            1,
            "2026-07-18T00:00:00Z".to_string(),
        );

        assert_eq!(
            serde_json::from_value::<WorkflowRunMeta>(legacy_json.clone())
                .expect("deserialize legacy run metadata"),
            legacy_meta
        );
        assert_eq!(
            serde_json::to_value(&legacy_meta).expect("serialize legacy run metadata"),
            legacy_json
        );

        let owned_meta =
            legacy_meta.with_owner_thread_id("01900000-0000-7000-8000-000000000001".to_string());
        let mut owned_json = legacy_json;
        owned_json["owner_thread_id"] = json!("01900000-0000-7000-8000-000000000001");
        assert_eq!(
            serde_json::to_value(&owned_meta).expect("serialize owned run metadata"),
            owned_json
        );
        assert_eq!(
            serde_json::from_value::<WorkflowRunMeta>(owned_json)
                .expect("deserialize owned run metadata"),
            owned_meta
        );
    }

    #[test]
    fn run_meta_distinguishes_null_budget_from_legacy_numeric_zero() {
        let unmetered = WorkflowRunMeta::new(
            "run-unmetered".to_string(),
            None,
            "sh".to_string(),
            "ah".to_string(),
            "unmetered".to_string(),
            None,
            1,
            "2026-07-18T00:00:00Z".to_string(),
        );
        let unmetered_json = serde_json::to_value(&unmetered).expect("serialize unmetered meta");
        assert_eq!(unmetered_json["budget_total"], Value::Null);

        // Legacy journals always encoded a number. Numeric zero remains an explicit
        // zero limit because the old representation cannot reveal sentinel intent.
        let mut legacy_zero_json = unmetered_json;
        legacy_zero_json["budget_total"] = json!(0);
        let legacy_zero: WorkflowRunMeta =
            serde_json::from_value(legacy_zero_json).expect("deserialize legacy numeric zero");
        assert_eq!(legacy_zero.budget_total, Some(0));
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
    fn agent_bound_round_trips_and_validates() {
        let bound = AgentBoundLine {
            timestamp: None,
            ordinal: 7,
            attempt: 0,
            child_thread_id: uuid::Uuid::now_v7().to_string(),
            rollout_path: std::env::temp_dir()
                .join("rollout.jsonl")
                .display()
                .to_string(),
        };
        assert_byte_stable(&JournalLine::AgentBound(bound.clone()));
        assert!(bound.validate().is_ok());

        let mut invalid = bound;
        invalid.rollout_path = "relative.jsonl".to_string();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn legacy_agent_records_default_to_initial_attempt() {
        let child_thread_id = uuid::Uuid::now_v7().to_string();
        let bound: AgentBoundLine = serde_json::from_value(json!({
            "timestamp": null,
            "ordinal": 4,
            "child_thread_id": child_thread_id,
            "rollout_path": "/tmp/legacy-rollout.jsonl"
        }))
        .expect("legacy binding");
        assert_eq!(bound.attempt, 0);

        let mut value = serde_json::to_value(sample_agent_call()).expect("agent call");
        value.as_object_mut().expect("object").remove("attempt");
        let call: AgentCallLine = serde_json::from_value(value).expect("legacy call");
        assert_eq!(call.attempt, 0);
        assert_eq!(call.control_reason, None);
        assert_eq!(call.progress, None);
    }

    #[test]
    fn selected_control_reason_requires_a_completed_null_anchor_with_progress() {
        let progress = AgentCallProgress {
            token_usage: AgentTokenUsage {
                input_tokens: 12,
                cached_input_tokens: 3,
                output_tokens: 4,
                reasoning_output_tokens: 2,
                total_tokens: 16,
            },
            tool_call_count: 5,
            duration_ms: 900,
        };
        for reason in [
            AgentControlReason::UserSkip,
            AgentControlReason::RetryLimitReached,
        ] {
            let mut call = sample_agent_call();
            call.control_reason = Some(reason);
            call.ret = Value::Null;
            call.progress = Some(progress.clone());
            assert!(call.validate().is_ok());

            let mut non_null = call.clone();
            non_null.ret = json!("not null");
            assert!(non_null.validate().is_err());

            let mut non_completed = call.clone();
            non_completed.status = Some(AgentStatus::Error);
            assert!(non_completed.validate().is_err());

            let mut missing_progress = call;
            missing_progress.progress = None;
            assert!(missing_progress.validate().is_err());
        }
    }

    #[test]
    fn aggregate_progress_rejects_negative_token_counters() {
        let mut call = sample_agent_call();
        call.progress = Some(AgentCallProgress {
            token_usage: AgentTokenUsage {
                total_tokens: -1,
                ..AgentTokenUsage::default()
            },
            tool_call_count: 0,
            duration_ms: 0,
        });
        let error = call.validate().expect_err("negative counters must fail");
        assert!(error.contains("total_tokens"), "unexpected error: {error}");
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

    #[test]
    fn validate_rejects_oversized_returns_using_serialized_bytes() {
        let mut call = sample_agent_call();
        call.ret = Value::String("x".repeat(WORKFLOW_AGENT_RETURN_MAX_BYTES));
        assert!(call.validate().is_err());

        // JSON escaping counts toward the durable replay representation.
        call.ret = Value::String("\0".repeat(WORKFLOW_AGENT_RETURN_MAX_BYTES / 2));
        assert!(call.validate().is_err());
    }
}
