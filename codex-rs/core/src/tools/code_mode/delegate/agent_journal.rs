use super::*;

pub(super) const WORKFLOW_AGENT_TURN_TOKEN_ESTIMATE: i64 = 1_024;
pub(super) const WORKFLOW_AGENT_JOURNAL_UNAVAILABLE: &str = "workflow agent journal is unavailable";

/// Per-`agent()`-call journal context (§7 write integration): the invariant fields (ordinal, cache
/// key, prompt hash, canonicalized opts, phase/label) computed once from the invocation, plus the
/// run's [`JournalRecorder`] (`None` for a non-journaled run). Wrapped in an `Arc` and cloned into the
/// admission closure so each terminal outcome appends exactly one `agent_call` line.
pub(super) struct AgentCallJournalCtx {
    /// The run's journal writer, or `None` for plain code-mode exec (no run / no journal).
    pub(super) recorder: Option<Arc<JournalRecorder>>,
    /// Invocation ordinal — the spine of prefix-replay (§7).
    pub(super) ordinal: u64,
    /// `blake3:` cache key over the canonical `(prompt, opts)` (label/phase excluded).
    pub(super) key: String,
    /// Content hash of the raw prompt text.
    pub(super) prompt_hash: String,
    /// Canonicalized cache-relevant opts recorded on the line.
    pub(super) opts: JournalAgentCallOpts,
    /// Progress-attribution phase (`opts.phase`), if any.
    pub(super) phase: Option<String>,
    /// Progress-attribution label (`opts.label`), if any.
    pub(super) label: Option<String>,
}

pub(super) struct AgentCallRecord {
    pub(super) attempt: u32,
    pub(super) status: Option<JournalAgentStatus>,
    pub(super) control_reason: Option<JournalAgentControlReason>,
    pub(super) ret: JsonValue,
    pub(super) child_thread_id: Option<String>,
    pub(super) rollout_path: Option<String>,
    pub(super) tokens_spent: Option<u64>,
    pub(super) progress: Option<JournalAgentCallProgress>,
}

impl AgentCallRecord {
    fn error(attempt: u32) -> Self {
        Self {
            attempt,
            status: Some(JournalAgentStatus::Error),
            control_reason: None,
            ret: JsonValue::Null,
            child_thread_id: None,
            rollout_path: None,
            tokens_spent: None,
            progress: None,
        }
    }
}

impl AgentCallJournalCtx {
    pub(super) fn for_invocation(
        recorder: Option<Arc<JournalRecorder>>,
        invocation: &WorkflowAgentInvocation,
    ) -> Arc<Self> {
        Arc::new(Self {
            recorder,
            ordinal: invocation.ordinal,
            key: KeyInputs {
                prompt: &invocation.prompt,
                model: invocation.opts.model.as_deref(),
                effort: invocation.opts.effort.as_deref(),
                agent_type: invocation.opts.agent_type.as_deref(),
                isolation: invocation.opts.isolation.as_deref(),
                schema: invocation.opts.schema.as_ref(),
            }
            .cache_key(),
            prompt_hash: prompt_hash(&invocation.prompt),
            opts: JournalAgentCallOpts {
                model: invocation.opts.model.clone(),
                effort: invocation.opts.effort.clone(),
                agent_type: invocation.opts.agent_type.clone(),
                isolation: invocation.opts.isolation.clone(),
                schema_hash: invocation.opts.schema.as_ref().map(schema_hash),
            },
            phase: invocation.opts.phase.clone(),
            label: invocation.opts.label.clone(),
        })
    }

    /// Append one `agent_call` line for this invocation's terminal outcome. A no-op when the run is
    /// not journaled. Best-effort: a write failure warns rather than failing the `agent()` call.
    pub(super) async fn record(&self, record: AgentCallRecord) -> Result<(), String> {
        let Some(recorder) = self.recorder.as_ref() else {
            return Ok(());
        };
        let line = AgentCallLine {
            timestamp: None,
            ordinal: self.ordinal,
            attempt: record.attempt,
            key: self.key.clone(),
            prompt_hash: self.prompt_hash.clone(),
            opts: self.opts.clone(),
            phase: self.phase.clone(),
            label: self.label.clone(),
            child_thread_id: record.child_thread_id,
            rollout_path: record.rollout_path,
            status: record.status,
            control_reason: record.control_reason,
            ret: record.ret,
            tokens_spent: record.tokens_spent,
            progress: record.progress,
            completion_seq: None,
        };
        recorder.record_agent_call(line).await.map_err(|err| {
            warn!(
                "failed to journal workflow agent() call (ordinal {}): {err}",
                self.ordinal
            );
            WORKFLOW_AGENT_JOURNAL_UNAVAILABLE.to_string()
        })
    }

    /// Record an `agent()` that THREW before (or instead of) spawning a child (a schema-bounds /
    /// budget / lifetime-cap rejection): `status:error`, `return:null`, no child linkage — which §7
    /// validation exempts from the completed-line linkage requirements.
    pub(super) async fn record_error(&self, attempt: u32) -> Result<(), String> {
        self.record(AgentCallRecord::error(attempt)).await
    }

    pub(super) async fn record_diagnostic(&self, message: String) {
        let Some(recorder) = self.recorder.as_ref() else {
            return;
        };
        if let Err(err) = recorder
            .record_log(LogLine {
                timestamp: None,
                ordinal: NullOrdinal,
                message,
            })
            .await
        {
            warn!(
                "failed to journal workflow agent() diagnostic (ordinal {}): {err}",
                self.ordinal
            );
        }
    }
}

pub(super) fn journal_progress(
    progress: &WorkflowChildProgress,
    duration_ms: u64,
) -> JournalAgentCallProgress {
    JournalAgentCallProgress {
        token_usage: JournalAgentTokenUsage {
            input_tokens: progress.token_usage.input_tokens,
            cached_input_tokens: progress.token_usage.cached_input_tokens,
            output_tokens: progress.token_usage.output_tokens,
            reasoning_output_tokens: progress.token_usage.reasoning_output_tokens,
            total_tokens: progress.token_usage.total_tokens,
        },
        tool_call_count: progress.tool_call_count,
        duration_ms,
    }
}

pub(super) struct WorkflowAgentExecution {
    pub(super) outcome: AgentSpawnOutcome,
    pub(super) cancelled: bool,
    pub(super) child_thread_id: Option<String>,
    pub(super) rollout_path: Option<String>,
    pub(super) tokens_spent: Option<u64>,
}

impl WorkflowAgentExecution {
    pub(super) fn finished(outcome: AgentSpawnOutcome) -> Self {
        Self {
            outcome,
            cancelled: false,
            child_thread_id: None,
            rollout_path: None,
            tokens_spent: None,
        }
    }

    pub(super) fn cancelled() -> Self {
        Self {
            outcome: AgentSpawnOutcome::Failed,
            cancelled: true,
            child_thread_id: None,
            rollout_path: None,
            tokens_spent: None,
        }
    }
}
