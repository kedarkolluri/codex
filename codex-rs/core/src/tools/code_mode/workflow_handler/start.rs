use std::path::Path;
use std::sync::Arc;

use codex_code_mode::ToolDefinition;
use codex_code_mode::WorkflowHostCompletion;
use codex_core_workflows::WorkflowBudget;
use codex_features::Features;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use codex_protocol::protocol::WorkflowRunEndEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;
use codex_workflow_journal::JournalRecorder;
use codex_workflow_journal::KEY_ALGO_VERSION;
use codex_workflow_journal::WorkflowRecoveryCursor;
use codex_workflow_journal::WorkflowRecoveryCursorGuard;
use codex_workflow_journal::WorkflowRunLease;
use codex_workflow_journal::WorkflowRunLeaseAcquire;
use codex_workflow_journal::WorkflowRunMeta;
use codex_workflow_journal::WorkflowRunStatus as JournalRunStatus;
use codex_workflow_journal::canonical_value_hash;
use codex_workflow_journal::prompt_hash as content_hash;
use codex_workflow_journal::storage::WorkflowRunPaths;
use tracing::warn;

use crate::function_tool::FunctionCallError;
use crate::tools::code_mode::CodeModeService;
use crate::tools::code_mode::workflow_progress::WorkflowEventTarget;
use crate::tools::code_mode::workflow_progress::WorkflowRunIndexAdmission;
use crate::tools::code_mode::workflow_tasks::WorkflowCancellation;
use crate::tools::code_mode::workflow_tasks::WorkflowCancellationCause;

use super::admission::ResumeSuccessorAdmission;
use super::admission::admit_resume_successor;
use super::bounds::ensure_workflow_args_within_bounds;
use super::bounds::ensure_workflow_enabled;
use super::bounds::protocol_budget_snapshot;
use super::bounds::truncate_model_error;
use super::bounds::workflow_budget_limit;
use super::ledger::WorkflowRunLineage;
use super::lifecycle::WorkflowRunLifecycle;
use super::lifecycle::WorkflowRunOutput;
use super::lifecycle::WorkflowRunStart;
use super::resume::ResumeSeed;

enum WorkflowSourceAdmission {
    Started(WorkflowRunStart),
    Existing(String),
}

const PUBLICATION_LOCK_ATTEMPTS: usize = 1_200;
const PUBLICATION_LOCK_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

/// Feature-gate, parse+validate the manifest, then run the workflow body exactly
/// once in a fresh code-mode isolate via the shared service.
#[allow(clippy::too_many_arguments)]
async fn begin_workflow_source(
    features: &Features,
    service: &CodeModeService,
    call_id: String,
    enabled_tools: Vec<ToolDefinition>,
    source: &str,
    args: serde_json::Value,
    lineage: WorkflowRunLineage,
    codex_home: &Path,
    resume: Option<ResumeSeed>,
    event_target: WorkflowEventTarget,
) -> Result<WorkflowSourceAdmission, FunctionCallError> {
    ensure_workflow_enabled(features)?;
    ensure_workflow_args_within_bounds(&args)?;

    let exec_args =
        codex_code_mode::parse_exec_source(source).map_err(FunctionCallError::RespondToModel)?;
    let meta = codex_code_mode::parse_workflow_meta(&exec_args.code).map_err(|err| {
        FunctionCallError::RespondToModel(format!("invalid workflow `meta` manifest: {err}"))
    })?;

    let budget_limit = workflow_budget_limit(&args);
    let budget = match event_target.parent_budget(service.workflow_run_ledger()) {
        Some(parent) => WorkflowBudget::child(parent, budget_limit),
        None => WorkflowBudget::new(budget_limit),
    };
    let initial_budget = protocol_budget_snapshot(&budget);
    let budget_total = initial_budget.total;
    let claimed_successor_run_id = resume
        .as_ref()
        .and_then(|seed| seed.successor_run_id.as_deref());
    let is_checkpoint_resume = claimed_successor_run_id.is_some();
    let run_id = claimed_successor_run_id
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    let parent_run_id = lineage.parent_run_id.clone();
    let script_hash = content_hash(&exec_args.code);
    let args_hash = canonical_value_hash(&args);
    let name = meta.name.clone();
    let execution_fingerprint = match resume.as_ref() {
        Some(seed) => seed.execution_fingerprint.clone(),
        None => event_target.execution_fingerprint().await,
    };
    let created_at = if is_checkpoint_resume {
        resume_successor_created_at(&run_id)?
    } else {
        chrono::Utc::now().to_rfc3339()
    };
    let mut run_meta = WorkflowRunMeta::new(
        run_id.clone(),
        parent_run_id,
        script_hash,
        args_hash.clone(),
        name,
        budget_total,
        KEY_ALGO_VERSION,
        created_at,
    );
    if let Some(owner_thread_id) = event_target.owner_thread_id() {
        run_meta = run_meta.with_owner_thread_id(owner_thread_id);
    }
    if let Some(execution_fingerprint) = execution_fingerprint {
        run_meta = run_meta.with_execution_fingerprint(execution_fingerprint);
    }
    if let Some(seed) = resume.as_ref() {
        run_meta = run_meta.with_resumed_from_run_id(seed.source_run_id.clone());
    }

    let paths = WorkflowRunPaths::new(codex_home, &run_id);
    let publication_guard = acquire_publication_lock(codex_home).await?;
    let index_admission = event_target
        .begin_run_publication(&paths, &run_meta)
        .await
        .map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "failed to reserve workflow run `{run_id}` before publication: {error}"
            ))
        })?;
    if !is_checkpoint_resume
        && matches!(
            index_admission,
            WorkflowRunIndexAdmission::ExistingPending(_)
                | WorkflowRunIndexAdmission::ExistingCommitted(_)
        )
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "workflow run id collision for `{run_id}`"
        )));
    }
    let needs_index_commit = matches!(
        index_admission,
        WorkflowRunIndexAdmission::InsertedPending | WorkflowRunIndexAdmission::ExistingPending(_)
    );
    if index_admission == WorkflowRunIndexAdmission::Unindexed
        && let Err(error) = publication_guard.invalidate()
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "failed to invalidate workflow recovery before publishing `{run_id}`: {error}"
        )));
    }

    let artifact_admission = if is_checkpoint_resume {
        admit_resume_successor(codex_home, &run_id, &exec_args.code, &run_meta, &args).await
    } else {
        let paths = paths.clone();
        (|| {
            paths.create_dir().map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "failed to create workflow run directory `{run_id}`: {error}"
                ))
            })?;
            let lease = match WorkflowRunLease::try_acquire(&paths).map_err(|error| {
                FunctionCallError::RespondToModel(format!(
                    "failed to acquire workflow run lease `{run_id}`: {error}"
                ))
            })? {
                WorkflowRunLeaseAcquire::Acquired(lease) => lease,
                WorkflowRunLeaseAcquire::Held => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "workflow run id collision for live run `{run_id}`"
                    )));
                }
            };
            paths
                .initialize(&exec_args.code, &run_meta)
                .map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "failed to initialize workflow run `{run_id}`: {error}"
                    ))
                })?;
            if let Err(error) = paths.write_invocation_args(&args, &args_hash) {
                if let Err(status_error) = paths.update_status(JournalRunStatus::Failed) {
                    warn!(
                        "failed to persist invocation-init failure for workflow run {run_id}: \
                         {status_error}"
                    );
                }
                return Err(FunctionCallError::RespondToModel(format!(
                    "failed to initialize workflow invocation for `{run_id}`: {error}"
                )));
            }
            Ok(ResumeSuccessorAdmission::Start { paths, lease })
        })()
    };
    let artifact_admission = match artifact_admission {
        Ok(admission) => admission,
        Err(error) => {
            if !is_checkpoint_resume
                && let Err(status_error) = paths.update_status(JournalRunStatus::Failed)
            {
                warn!("failed to persist publication failure for {run_id}: {status_error}");
            }
            if let Err(cursor_error) = publication_guard.invalidate() {
                warn!("failed to invalidate recovery after publication error: {cursor_error}");
            }
            if needs_index_commit {
                event_target.abort_run_publication(&run_id).await;
            }
            return Err(error);
        }
    };
    let (paths, lease) = match artifact_admission {
        ResumeSuccessorAdmission::Start { paths, lease } => (paths, lease),
        ResumeSuccessorAdmission::Existing => {
            if needs_index_commit {
                let status = paths.read_meta_bounded().map_err(|error| {
                    FunctionCallError::RespondToModel(format!(
                        "workflow checkpoint successor is unavailable or inconsistent: {error}"
                    ))
                })?;
                event_target
                    .commit_run_publication(&run_id, status.status)
                    .await
                    .map_err(|error| {
                        FunctionCallError::RespondToModel(format!(
                            "failed to commit existing workflow run `{run_id}`: {error}"
                        ))
                    })?;
            }
            drop(publication_guard);
            return Ok(WorkflowSourceAdmission::Existing(run_id));
        }
    };
    let recorder = match JournalRecorder::new(&paths, &run_meta).await {
        Ok(recorder) => recorder,
        Err(error) => {
            warn!("failed to open workflow journal for {run_id}: {error}");
            if !is_checkpoint_resume
                && let Err(status_error) = paths.update_status(JournalRunStatus::Failed)
            {
                warn!(
                    "failed to persist journal-open failure for workflow run {run_id}: \
                    {status_error}"
                );
            }
            if let Err(cursor_error) = publication_guard.invalidate() {
                warn!("failed to invalidate recovery after journal error: {cursor_error}");
            }
            if needs_index_commit {
                event_target.abort_run_publication(&run_id).await;
            }
            return Err(FunctionCallError::RespondToModel(if is_checkpoint_resume {
                "workflow checkpoint successor is unavailable or inconsistent".to_string()
            } else {
                format!("failed to open workflow journal for `{run_id}`: {error}")
            }));
        }
    };
    if needs_index_commit
        && let Err(error) = event_target
            .commit_run_publication(&run_id, JournalRunStatus::Running)
            .await
    {
        if let Err(status_error) = paths.update_status(JournalRunStatus::Failed) {
            warn!("failed to persist publication-commit failure for {run_id}: {status_error}");
        }
        if let Err(shutdown_error) = recorder.shutdown().await {
            warn!("failed to close workflow journal for {run_id}: {shutdown_error}");
        }
        if let Err(cursor_error) = publication_guard.invalidate() {
            warn!("failed to invalidate recovery after index commit error: {cursor_error}");
        }
        event_target.abort_run_publication(&run_id).await;
        return Err(FunctionCallError::RespondToModel(format!(
            "failed to commit workflow run `{run_id}` before launch: {error}"
        )));
    }
    drop(publication_guard);

    let replay_entries = resume
        .as_ref()
        .into_iter()
        .flat_map(|seed| &seed.replay_entries)
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect();

    let budget_total_i64 = budget_total.map(|total| total.min(i64::MAX as u64) as i64);
    event_target
        .emit(WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: run_id.clone(),
            resumed_from_run_id: resume.as_ref().map(|seed| seed.source_run_id.clone()),
            name: meta.name,
            phases: meta.phases,
            args_digest: args_hash,
        }))
        .await;

    if is_checkpoint_resume && let Err(error) = paths.mark_execution_launched() {
        warn!("failed to commit workflow successor launch for {run_id}: {error}");
        if let Err(shutdown_error) = recorder.shutdown().await {
            warn!("failed to close workflow journal for {run_id}: {shutdown_error}");
        }
        event_target
            .emit(WorkflowEvent::RunEnd(WorkflowRunEndEvent {
                run_id: run_id.clone(),
                status: AgentStatus::Errored(
                    "workflow checkpoint successor could not be launched".to_string(),
                ),
                terminal_reason: Some(WorkflowRunTerminalReason::Failed),
                spent: i64::try_from(budget.snapshot().spent).unwrap_or(i64::MAX),
                total: budget_total_i64,
            }))
            .await;
        event_target
            .record_run_finished(&run_id, JournalRunStatus::Failed)
            .await;
        return Err(FunctionCallError::RespondToModel(
            "workflow checkpoint successor is unavailable or inconsistent".to_string(),
        ));
    }

    let started_cell = match service
        .execute(
            codex_code_mode::ExecuteRequest {
                tool_call_id: call_id,
                enabled_tools,
                source: exec_args.code.clone(),
                yield_time_ms: exec_args.yield_time_ms,
                max_output_tokens: exec_args.max_output_tokens,
                workflow: true,
                args: Some(args),
                run_id: Some(run_id.clone()),
                replay_entries,
                workflow_budget: Some(initial_budget),
            },
            event_target.dispatch_origin(),
        )
        .await
    {
        Ok(started_cell) => started_cell,
        Err(error) => {
            if let Err(shutdown_error) = recorder.shutdown().await {
                warn!("failed to close workflow journal for {run_id}: {shutdown_error}");
            }
            event_target
                .emit(WorkflowEvent::RunEnd(WorkflowRunEndEvent {
                    run_id: run_id.clone(),
                    status: AgentStatus::Errored(truncate_model_error(error.clone())),
                    terminal_reason: Some(WorkflowRunTerminalReason::Failed),
                    spent: i64::try_from(budget.snapshot().spent).unwrap_or(i64::MAX),
                    total: budget_total_i64,
                }))
                .await;
            event_target
                .record_run_finished(&run_id, JournalRunStatus::Failed)
                .await;
            return Err(FunctionCallError::RespondToModel(error));
        }
    };
    let cell_id = started_cell.cell_id.clone();
    service.workflow_run_ledger().register_run(
        cell_id.clone(),
        run_id.clone(),
        lineage.parent_run_id,
        lineage.depth,
        Arc::clone(&budget),
        budget_total_i64,
    );
    let recorder = Arc::new(recorder);
    service
        .workflow_run_ledger()
        .register_recorder(cell_id.clone(), Arc::clone(&recorder));
    service.mark_cell_ready_for_dispatch(&cell_id);
    let Some(session) = service.session.get().cloned() else {
        let message = "workflow cell started without an initialized code-mode session";
        let _ = service.terminate(cell_id.clone()).await;
        service.dispatch_broker.drain_workflow_cell(&cell_id).await;
        let publication = event_target
            .complete_cell(
                &service.workflow_run_ledger,
                &cell_id,
                WorkflowHostCompletion::Errored(message.to_string()).into(),
            )
            .await;
        if publication.is_some()
            && let Err(error) = paths.update_status(JournalRunStatus::Failed)
        {
            warn!("failed to persist workflow session-invariant failure for {run_id}: {error}");
        }
        if let Err(error) = recorder.shutdown().await {
            warn!("failed to close workflow journal for {run_id}: {error}");
        }
        service.dispatch_broker.close_cell(&cell_id);
        return Err(FunctionCallError::Fatal(message.to_string()));
    };

    Ok(WorkflowSourceAdmission::Started(WorkflowRunStart {
        started_cell,
        lifecycle: Arc::new(WorkflowRunLifecycle {
            run_id,
            cell_id,
            codex_home: codex_home.to_path_buf(),
            paths,
            recorder,
            session,
            dispatch_broker: Arc::clone(&service.dispatch_broker),
            ledger: Arc::clone(&service.workflow_run_ledger),
            event_target,
        }),
        lease,
    }))
}

async fn acquire_publication_lock(
    codex_home: &Path,
) -> Result<WorkflowRecoveryCursorGuard, FunctionCallError> {
    let cursor = WorkflowRecoveryCursor::open(codex_home).map_err(|error| {
        FunctionCallError::RespondToModel(format!(
            "workflow publication lock is unavailable: {error}"
        ))
    })?;
    for _ in 0..PUBLICATION_LOCK_ATTEMPTS {
        match cursor.try_lock().map_err(|error| {
            FunctionCallError::RespondToModel(format!(
                "workflow publication lock is unavailable: {error}"
            ))
        })? {
            Some(guard) => return Ok(guard),
            None => tokio::time::sleep(PUBLICATION_LOCK_DELAY).await,
        }
    }
    Err(FunctionCallError::RespondToModel(
        "workflow publication lock timed out".to_string(),
    ))
}

fn resume_successor_created_at(run_id: &str) -> Result<String, FunctionCallError> {
    let run_id = uuid::Uuid::parse_str(run_id).map_err(|_| {
        FunctionCallError::Fatal("durable workflow successor id is not a UUID".to_string())
    })?;
    let timestamp = run_id.get_timestamp().ok_or_else(|| {
        FunctionCallError::Fatal("durable workflow successor id has no timestamp".to_string())
    })?;
    let (seconds, nanos) = timestamp.to_unix();
    let seconds = i64::try_from(seconds).map_err(|_| {
        FunctionCallError::Fatal("durable workflow successor timestamp is invalid".to_string())
    })?;
    let created_at =
        chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, nanos).ok_or_else(|| {
            FunctionCallError::Fatal("durable workflow successor timestamp is invalid".to_string())
        })?;
    Ok(created_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// Run a foreground workflow through its terminal response while retaining its resources.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_workflow_source_to_terminal(
    features: &Features,
    service: &CodeModeService,
    call_id: String,
    enabled_tools: Vec<ToolDefinition>,
    source: &str,
    args: serde_json::Value,
    lineage: WorkflowRunLineage,
    codex_home: &Path,
    resume: Option<ResumeSeed>,
    event_target: WorkflowEventTarget,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<WorkflowRunOutput, FunctionCallError> {
    match begin_workflow_source(
        features,
        service,
        call_id,
        enabled_tools,
        source,
        args,
        lineage,
        codex_home,
        resume,
        event_target,
    )
    .await?
    {
        WorkflowSourceAdmission::Started(start) => {
            start
                .run_to_terminal(WorkflowCancellation::interrupted(cancellation))
                .await
        }
        WorkflowSourceAdmission::Existing(_) => Err(FunctionCallError::RespondToModel(
            "workflow checkpoint successor was already admitted".to_string(),
        )),
    }
}

/// Durably initialize a workflow and transfer its cell to the session-owned task manager.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_workflow_source(
    features: &Features,
    service: &CodeModeService,
    call_id: String,
    enabled_tools: Vec<ToolDefinition>,
    source: &str,
    args: serde_json::Value,
    lineage: WorkflowRunLineage,
    codex_home: &Path,
    resume: Option<ResumeSeed>,
    event_target: WorkflowEventTarget,
) -> Result<String, FunctionCallError> {
    let admission = begin_workflow_source(
        features,
        service,
        call_id,
        enabled_tools,
        source,
        args,
        lineage,
        codex_home,
        resume,
        event_target,
    )
    .await?;
    let start = match admission {
        WorkflowSourceAdmission::Started(start) => start,
        WorkflowSourceAdmission::Existing(run_id) => return Ok(run_id),
    };
    let run_id = start.lifecycle.run_id.clone();
    let task_run_id = run_id.clone();
    let task = move |cancellation| async move {
        let result = start.run_to_terminal(cancellation).await;
        if let Err(error) = &result {
            warn!("detached workflow run {task_run_id} failed: {error}");
        }
        result
    };
    match service
        .workflow_tasks
        .start_recoverable(run_id.clone(), task)
    {
        Ok(handle) => {
            debug_assert_eq!(handle.run_id(), run_id);
            drop(handle);
            Ok(run_id)
        }
        Err(rejected) => {
            let (error, task) = rejected.into_parts();
            let cancellation =
                WorkflowCancellation::pre_cancelled(WorkflowCancellationCause::Interrupted);
            let _ = task(cancellation).await;
            Err(FunctionCallError::RespondToModel(format!(
                "failed to start detached workflow run `{run_id}`: {error}"
            )))
        }
    }
}

#[cfg(test)]
#[path = "start_tests.rs"]
mod tests;
