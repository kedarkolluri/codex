use std::path::Path;

use codex_code_mode::ToolDefinition;
use codex_features::Features;
use codex_workflow_journal::AgentCallLine;
use codex_workflow_journal::KEY_ALGO_VERSION;
use codex_workflow_journal::ReplayJournal;
use codex_workflow_journal::WorkflowRunLease;
use codex_workflow_journal::WorkflowRunLeaseAcquire;
use codex_workflow_journal::WorkflowRunStatus;
use codex_workflow_journal::canonical_value_hash;
use codex_workflow_journal::prompt_hash as content_hash;
use codex_workflow_journal::storage::WorkflowRunPaths;
use codex_workflow_journal::storage::mint_run_id;

use crate::function_tool::FunctionCallError;
use crate::tools::code_mode::CodeModeService;
use crate::tools::code_mode::workflow_progress::WorkflowEventTarget;
use crate::tools::code_mode::workflow_progress::durable::DurableProgressRead;
use crate::tools::code_mode::workflow_progress::durable::DurableRunState;
use crate::tools::code_mode::workflow_progress::durable::DurableRunStatus;
use crate::tools::code_mode::workflow_progress::durable::read as read_durable_progress;

use super::bounds::ensure_workflow_args_within_bounds;
use super::bounds::ensure_workflow_enabled;
use super::ledger::WorkflowRunLineage;
use super::lifecycle::WorkflowRunOutput;
use super::start::run_workflow_source_to_terminal;

const RESUME_UNAVAILABLE_MESSAGE: &str = "workflow checkpoint is unavailable or not resumable";
const LEGACY_UNKNOWN_RESUME_MESSAGE: &str = "legacy workflow checkpoint state is unknown; start a fresh run without --resume only after confirming no older Codex process still owns it";

/// The prefix-replay seed handed to a resumed workflow run.
#[derive(Debug, Clone)]
#[cfg_attr(
    not(test),
    allow(
        dead_code,
        reason = "consumed by resume_workflow_source_to_terminal; the --resume/workflow_run entrypoints are P4"
    )
)]
pub(crate) struct ResumeSeed {
    /// The source run id being resumed.
    pub(crate) source_run_id: String,
    /// The one successor durably claimed by an authenticated paused checkpoint.
    /// General prefix replay leaves this unset and mints a fresh run at admission.
    pub(crate) successor_run_id: Option<String>,
    /// The journaled `agent_call` prefix, or empty on divergence.
    pub(crate) replay_entries: Vec<AgentCallLine>,
    /// The first structural mismatch against the prior journal, if any.
    pub(crate) divergence: Option<codex_workflow_journal::Divergence>,
    /// Current run's provider/router/model fingerprint.
    pub(crate) execution_fingerprint: Option<String>,
}

/// Authorization boundary for ordinary, non-consuming prefix replay.
#[derive(Debug)]
pub(crate) enum WorkflowReplayAccess {
    /// An explicit local CLI invocation already executes with the user's
    /// filesystem authority and starts a fresh session owner.
    LocalProcess,
    /// A model-visible request may replay only a run owned by its thread.
    ThreadOwner(String),
}

/// Prepare ordinary prefix replay from any durable source journal.
///
/// This is the `--resume` / `resumeFromRunId` contract: it deliberately does
/// not mutate or consume a completed, failed, stopped, or crashed source run.
/// Live runs are rejected, and paused checkpoints stay on the authenticated
/// one-successor control path. A changed script, arguments, or execution
/// fingerprint produces an empty replay prefix and runs live from ordinal zero.
pub(crate) async fn prepare_workflow_prefix_replay(
    features: &Features,
    event_target: &WorkflowEventTarget,
    codex_home: &Path,
    source_run_id: &str,
    current_source: &str,
    args: &serde_json::Value,
    access: WorkflowReplayAccess,
) -> Result<ResumeSeed, FunctionCallError> {
    ensure_workflow_enabled(features)?;
    ensure_workflow_args_within_bounds(args)?;
    validate_source_run_id(source_run_id)?;
    let source_paths = WorkflowRunPaths::new(codex_home, source_run_id);
    let lease_paths = source_paths.clone();
    let source_lease =
        tokio::task::spawn_blocking(move || WorkflowRunLease::try_acquire_existing(&lease_paths))
            .await
            .map_err(|_| resume_error(source_run_id, "source lease check failed"))?
            .map_err(|_| resume_error(source_run_id, "source lease is unavailable"))?;
    let source_has_lease_marker = source_lease.is_some();
    let _source_lease = match source_lease {
        Some(WorkflowRunLeaseAcquire::Acquired(lease)) => Some(lease),
        Some(WorkflowRunLeaseAcquire::Held) => {
            return Err(resume_error(source_run_id, "source run is still active"));
        }
        None => None,
    };

    let meta_paths = source_paths.clone();
    let meta = tokio::task::spawn_blocking(move || meta_paths.read_meta_bounded())
        .await
        .map_err(|_| resume_error(source_run_id, "source metadata read failed"))?
        .map_err(|_| resume_error(source_run_id, "source metadata is unavailable"))?;
    if meta.run_id != source_run_id {
        return Err(resume_error(
            source_run_id,
            "source metadata does not match the requested run",
        ));
    }
    if let WorkflowReplayAccess::ThreadOwner(owner_thread_id) = &access
        && meta.owner_thread_id.as_deref() != Some(owner_thread_id.as_str())
    {
        return Err(resume_error(
            source_run_id,
            "source run is not owned by this thread",
        ));
    }
    let progress = read_durable_progress(codex_home, source_run_id)
        .await
        .map_err(|_| resume_error(source_run_id, "source ledger is unavailable"))?;
    let has_terminal_metadata = matches!(
        meta.status,
        WorkflowRunStatus::Completed | WorkflowRunStatus::Stopped | WorkflowRunStatus::Failed
    );
    let has_durable_terminal_proof = match progress {
        DurableProgressRead::Snapshot(progress) if progress.status == DurableRunStatus::Paused => {
            return Err(resume_error(
                source_run_id,
                "paused checkpoints require authenticated checkpoint resume",
            ));
        }
        DurableProgressRead::Corrupt(_) if has_terminal_metadata => false,
        DurableProgressRead::Corrupt(_) => {
            return Err(resume_error(source_run_id, "source ledger is corrupt"));
        }
        DurableProgressRead::Missing => false,
        DurableProgressRead::Snapshot(progress) => progress.state == DurableRunState::Terminal,
    };
    if !source_has_lease_marker
        && meta.status == WorkflowRunStatus::Running
        && !has_durable_terminal_proof
    {
        if matches!(access, WorkflowReplayAccess::LocalProcess) {
            return Err(FunctionCallError::RespondToModel(
                LEGACY_UNKNOWN_RESUME_MESSAGE.to_string(),
            ));
        }
        return Err(resume_error(
            source_run_id,
            "source run has no trustworthy inactive lease state",
        ));
    }
    if meta.status == WorkflowRunStatus::Paused && !has_durable_terminal_proof {
        return Err(resume_error(
            source_run_id,
            "paused checkpoints require authenticated checkpoint resume",
        ));
    }

    let execution_fingerprint = event_target.execution_fingerprint().await;
    load_resume_seed_off_thread(
        codex_home,
        source_run_id,
        current_source,
        args,
        execution_fingerprint.as_deref(),
    )
    .await
}

/// Where checkpoint-resume obtains the exact invocation arguments.
#[derive(Debug)]
pub(crate) enum WorkflowResumeInvocation {
    /// Exercise canonical argument authentication without exposing the private
    /// artifact through a production checkpoint-resume surface.
    #[cfg(test)]
    Explicit(serde_json::Value),
    /// Load the host-private invocation artifact without exposing it to a
    /// protocol or model-visible context.
    Persisted,
}

/// A fully authenticated paused checkpoint held under its source lease.
#[derive(Debug)]
pub(crate) struct PreparedWorkflowResume {
    pub(crate) seed: ResumeSeed,
    pub(crate) source: String,
    pub(crate) args: serde_json::Value,
    _source_lease: WorkflowRunLease,
}

/// Load and validate the source run's journal to build a replay seed.
pub(crate) fn load_resume_seed(
    codex_home: &Path,
    source_run_id: &str,
    source: &str,
    args: &serde_json::Value,
    execution_fingerprint: Option<&str>,
) -> Result<ResumeSeed, FunctionCallError> {
    validate_source_run_id(source_run_id)?;
    let source_paths = WorkflowRunPaths::new(codex_home, source_run_id);
    let prior = ReplayJournal::load(&source_paths.journal())
        .map_err(|_| resume_error(source_run_id, "checkpoint journal is unavailable"))?;
    if prior.run_meta().run_id != source_run_id {
        return Err(resume_error(
            source_run_id,
            "checkpoint journal metadata does not match the requested run",
        ));
    }

    let exec_args = codex_code_mode::parse_exec_source(source)
        .map_err(|_| resume_error(source_run_id, "checkpoint source is invalid"))?;
    let script_hash = content_hash(&exec_args.code);
    let args_hash = canonical_value_hash(args);

    let divergence = match execution_fingerprint {
        Some(execution_fingerprint) => prior.check_execution_compatibility(
            &script_hash,
            &args_hash,
            KEY_ALGO_VERSION,
            execution_fingerprint,
        ),
        None => prior.check_compatibility(&script_hash, &args_hash, KEY_ALGO_VERSION),
    };
    let replay_entries = if divergence.is_some() {
        Vec::new()
    } else {
        prior.entries().to_vec()
    };

    Ok(ResumeSeed {
        source_run_id: source_run_id.to_string(),
        successor_run_id: None,
        replay_entries,
        divergence,
        execution_fingerprint: execution_fingerprint.map(str::to_string),
    })
}

async fn load_resume_seed_off_thread(
    codex_home: &Path,
    source_run_id: &str,
    source: &str,
    args: &serde_json::Value,
    execution_fingerprint: Option<&str>,
) -> Result<ResumeSeed, FunctionCallError> {
    let codex_home = codex_home.to_path_buf();
    let owned_source_run_id = source_run_id.to_string();
    let source = source.to_string();
    let args = args.clone();
    let execution_fingerprint = execution_fingerprint.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        load_resume_seed(
            &codex_home,
            &owned_source_run_id,
            &source,
            &args,
            execution_fingerprint.as_deref(),
        )
    })
    .await
    .map_err(|_| resume_error(source_run_id, "checkpoint journal read failed"))?
}

/// Authenticate and hold a resumable checkpoint until its successor has been admitted.
pub(crate) async fn prepare_paused_workflow_resume(
    features: &Features,
    event_target: &WorkflowEventTarget,
    codex_home: &Path,
    source_run_id: &str,
    current_source: &str,
    invocation: WorkflowResumeInvocation,
) -> Result<PreparedWorkflowResume, FunctionCallError> {
    ensure_workflow_enabled(features)?;
    validate_source_run_id(source_run_id)?;
    let owner_thread_id = event_target
        .owner_thread_id()
        .ok_or_else(|| resume_error(source_run_id, "checkpoint ownership is unavailable"))?;
    let execution_fingerprint = event_target.execution_fingerprint().await;
    prepare_paused_workflow_resume_for_owner(
        codex_home,
        source_run_id,
        current_source,
        invocation,
        &owner_thread_id,
        execution_fingerprint.as_deref(),
    )
    .await
}

async fn prepare_paused_workflow_resume_for_owner(
    codex_home: &Path,
    source_run_id: &str,
    current_source: &str,
    invocation: WorkflowResumeInvocation,
    owner_thread_id: &str,
    execution_fingerprint: Option<&str>,
) -> Result<PreparedWorkflowResume, FunctionCallError> {
    validate_source_run_id(source_run_id)?;
    let source_paths = WorkflowRunPaths::new(codex_home, source_run_id);
    let lease_paths = source_paths.clone();
    let lease = match tokio::task::spawn_blocking(move || {
        WorkflowRunLease::try_acquire_existing(&lease_paths)
    })
    .await
    .map_err(|_| resume_error(source_run_id, "checkpoint lease check failed"))?
    .map_err(|_| resume_error(source_run_id, "checkpoint lease is unavailable"))?
    {
        Some(WorkflowRunLeaseAcquire::Acquired(lease)) => lease,
        Some(WorkflowRunLeaseAcquire::Held) => {
            wait_for_resume_source_lease(source_paths.clone(), source_run_id).await?
        }
        None => {
            return Err(resume_error(
                source_run_id,
                "checkpoint has no durable lease marker",
            ));
        }
    };

    let meta_paths = source_paths.clone();
    let meta = tokio::task::spawn_blocking(move || meta_paths.read_meta_bounded())
        .await
        .map_err(|_| resume_error(source_run_id, "checkpoint metadata read failed"))?
        .map_err(|_| resume_error(source_run_id, "checkpoint metadata is unavailable"))?;
    if meta.run_id != source_run_id {
        return Err(resume_error(
            source_run_id,
            "checkpoint metadata does not match the requested run",
        ));
    }
    if meta.owner_thread_id.as_deref() != Some(owner_thread_id) {
        return Err(resume_error(
            source_run_id,
            "checkpoint is not owned by this thread",
        ));
    }

    let progress = read_durable_progress(codex_home, source_run_id)
        .await
        .map_err(|_| resume_error(source_run_id, "checkpoint ledger is unavailable"))?;
    let DurableProgressRead::Snapshot(progress) = progress else {
        return Err(resume_error(
            source_run_id,
            "checkpoint has no authoritative paused ledger",
        ));
    };
    if progress.state != DurableRunState::Terminal || progress.status != DurableRunStatus::Paused {
        return Err(resume_error(
            source_run_id,
            "only an authoritative paused checkpoint can be resumed",
        ));
    }

    match meta.status {
        WorkflowRunStatus::Paused => {}
        WorkflowRunStatus::Running => {
            let status_paths = source_paths.clone();
            let updated = tokio::task::spawn_blocking(move || {
                status_paths.update_status(WorkflowRunStatus::Paused)
            })
            .await
            .map_err(|_| resume_error(source_run_id, "checkpoint metadata repair failed"))?
            .map_err(|_| resume_error(source_run_id, "checkpoint metadata repair failed"))?;
            if updated.status != WorkflowRunStatus::Paused {
                return Err(resume_error(
                    source_run_id,
                    "checkpoint metadata conflicts with its paused ledger",
                ));
            }
        }
        WorkflowRunStatus::Completed | WorkflowRunStatus::Stopped | WorkflowRunStatus::Failed => {
            return Err(resume_error(
                source_run_id,
                "checkpoint metadata conflicts with its paused ledger",
            ));
        }
    }

    let artifact_paths = source_paths.clone();
    let expected_args_hash = meta.args_hash.clone();
    let (persisted_source, persisted_args) = tokio::task::spawn_blocking(move || {
        let source = artifact_paths.read_script_bounded()?;
        let args = artifact_paths.read_invocation_args_bounded(&expected_args_hash)?;
        Ok::<_, std::io::Error>((source, args))
    })
    .await
    .map_err(|_| resume_error(source_run_id, "checkpoint artifact read failed"))?
    .map_err(|_| resume_error(source_run_id, "checkpoint artifacts are unavailable"))?;

    let current_exec = codex_code_mode::parse_exec_source(current_source)
        .map_err(|_| resume_error(source_run_id, "saved workflow source is invalid"))?;
    let current_meta = codex_code_mode::parse_workflow_meta(&current_exec.code)
        .map_err(|_| resume_error(source_run_id, "saved workflow metadata is invalid"))?;
    if current_exec.code != persisted_source
        || content_hash(&persisted_source) != meta.script_hash
        || current_meta.name != meta.name
    {
        return Err(resume_error(
            source_run_id,
            "saved workflow source does not match the checkpoint",
        ));
    }
    if meta.execution_fingerprint.as_deref() != execution_fingerprint {
        return Err(resume_error(
            source_run_id,
            "execution environment does not match the checkpoint",
        ));
    }

    let args = match invocation {
        #[cfg(test)]
        WorkflowResumeInvocation::Explicit(args) => {
            ensure_workflow_args_within_bounds(&args)?;
            if args != persisted_args || canonical_value_hash(&args) != meta.args_hash {
                return Err(resume_error(
                    source_run_id,
                    "explicit arguments do not match the checkpoint",
                ));
            }
            args
        }
        WorkflowResumeInvocation::Persisted => persisted_args,
    };
    let mut seed = load_resume_seed_off_thread(
        codex_home,
        source_run_id,
        &persisted_source,
        &args,
        execution_fingerprint,
    )
    .await
    .map_err(|_| resume_error(source_run_id, "checkpoint replay identity is unavailable"))?;
    if seed.divergence.is_some() {
        return Err(resume_error(
            source_run_id,
            "checkpoint replay identity is inconsistent",
        ));
    }
    let candidate = mint_run_id();
    let successor_run_id = source_paths
        .claim_resume_successor(&lease, &candidate)
        .map_err(|_| resume_error(source_run_id, "checkpoint successor claim failed"))?;
    seed.successor_run_id = Some(successor_run_id);
    Ok(PreparedWorkflowResume {
        seed,
        source: persisted_source,
        args,
        _source_lease: lease,
    })
}

async fn wait_for_resume_source_lease(
    source_paths: WorkflowRunPaths,
    source_run_id: &str,
) -> Result<WorkflowRunLease, FunctionCallError> {
    const CLAIM_JOIN_ATTEMPTS: usize = 1_200;
    const CLAIM_JOIN_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

    for _ in 0..CLAIM_JOIN_ATTEMPTS {
        let lease_paths = source_paths.clone();
        let lease = tokio::task::spawn_blocking(move || {
            WorkflowRunLease::try_acquire_existing(&lease_paths)
        })
        .await
        .map_err(|_| resume_error(source_run_id, "checkpoint lease retry failed"))?
        .map_err(|_| resume_error(source_run_id, "checkpoint lease retry failed"))?;
        match lease {
            Some(WorkflowRunLeaseAcquire::Acquired(lease)) => return Ok(lease),
            Some(WorkflowRunLeaseAcquire::Held) => {}
            None => {
                return Err(resume_error(
                    source_run_id,
                    "checkpoint lease marker disappeared",
                ));
            }
        }
        tokio::time::sleep(CLAIM_JOIN_DELAY).await;
    }
    Err(resume_error(
        source_run_id,
        "checkpoint successor admission did not finish",
    ))
}

fn validate_source_run_id(source_run_id: &str) -> Result<(), FunctionCallError> {
    let parsed = uuid::Uuid::parse_str(source_run_id)
        .map_err(|_| resume_error(source_run_id, "run id is not canonical"))?;
    if parsed.to_string() != source_run_id {
        return Err(resume_error(source_run_id, "run id is not canonical"));
    }
    Ok(())
}

fn resume_error(_source_run_id: &str, _message: &str) -> FunctionCallError {
    FunctionCallError::RespondToModel(RESUME_UNAVAILABLE_MESSAGE.to_string())
}

/// Prefix-replay a prior workflow run through terminal completion under a fresh run id.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resume_workflow_source_to_terminal(
    features: &Features,
    service: &CodeModeService,
    call_id: String,
    enabled_tools: Vec<ToolDefinition>,
    source: &str,
    args: serde_json::Value,
    codex_home: &Path,
    source_run_id: &str,
    event_target: WorkflowEventTarget,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<WorkflowRunOutput, FunctionCallError> {
    let seed = prepare_workflow_prefix_replay(
        features,
        &event_target,
        codex_home,
        source_run_id,
        source,
        &args,
        WorkflowReplayAccess::LocalProcess,
    )
    .await?;
    run_workflow_source_to_terminal(
        features,
        service,
        call_id,
        enabled_tools,
        source,
        args,
        WorkflowRunLineage {
            parent_run_id: None,
            depth: 0,
        },
        codex_home,
        Some(seed),
        event_target,
        cancellation,
    )
    .await
}

#[cfg(test)]
#[path = "resume_tests.rs"]
mod tests;
