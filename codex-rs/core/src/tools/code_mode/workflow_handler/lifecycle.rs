use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeSession;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use codex_code_mode::WorkflowOutputBounds;
use codex_workflow_journal::JournalRecorder;
use codex_workflow_journal::WorkflowRunLease;
use codex_workflow_journal::WorkflowRunStatus as JournalRunStatus;
use codex_workflow_journal::storage::WorkflowRunPaths;
use futures::FutureExt;
use tracing::warn;

use crate::function_tool::FunctionCallError;
use crate::tools::code_mode::delegate::CodeModeDispatchBroker;
use crate::tools::code_mode::workflow_progress::WorkflowEventTarget;
use crate::tools::code_mode::workflow_progress::WorkflowRunCompletion;
use crate::tools::code_mode::workflow_progress::durable::DurableRunStatus;
use crate::tools::code_mode::workflow_tasks::WorkflowCancellation;
use crate::tools::code_mode::workflow_tasks::WorkflowCancellationCause;

use super::bounds::truncate_model_error;
use super::ledger::WorkflowRunLedger;

/// Result of running a workflow body once in a fresh isolate.
#[derive(Debug)]
pub(crate) struct WorkflowRunOutput {
    pub(crate) response: RuntimeResponse,
    /// Host-minted uuid v7 run id of this run.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "read by resume_workflow_source_to_terminal; the --resume/workflow_run entrypoints are P4"
        )
    )]
    pub(crate) run_id: String,
}

/// A workflow cell that has been durably initialized and admitted, but whose
/// first runtime response has not been consumed yet.
pub(super) struct WorkflowRunStart {
    pub(super) started_cell: codex_code_mode::StartedCell,
    pub(super) lifecycle: Arc<WorkflowRunLifecycle>,
    pub(super) lease: WorkflowRunLease,
}

pub(super) struct WorkflowRunLifecycle {
    pub(super) run_id: String,
    pub(super) cell_id: CellId,
    pub(super) codex_home: PathBuf,
    pub(super) paths: WorkflowRunPaths,
    pub(super) recorder: Arc<JournalRecorder>,
    pub(super) session: Arc<dyn CodeModeSession>,
    pub(super) dispatch_broker: Arc<CodeModeDispatchBroker>,
    pub(super) ledger: Arc<WorkflowRunLedger>,
    pub(super) event_target: WorkflowEventTarget,
}

enum WorkflowDriveError {
    Cancelled(WorkflowCancellationCause),
    Runtime(String),
}

impl WorkflowRunStart {
    pub(super) async fn run_to_terminal(
        self,
        cancellation: WorkflowCancellation,
    ) -> Result<WorkflowRunOutput, FunctionCallError> {
        let Self {
            started_cell,
            lifecycle,
            lease,
        } = self;
        let run_lifecycle = Arc::clone(&lifecycle);
        let terminal_cancellation = cancellation.clone();
        let execution = async move {
            match run_lifecycle
                .drive_to_terminal(started_cell, cancellation)
                .await
            {
                Ok(response) => run_lifecycle.finish_terminal_response(response).await,
                Err(WorkflowDriveError::Cancelled(WorkflowCancellationCause::UserStop)) => {
                    run_lifecycle.stop(&terminal_cancellation).await
                }
                Err(WorkflowDriveError::Cancelled(WorkflowCancellationCause::Pause)) => {
                    run_lifecycle.pause(&terminal_cancellation).await
                }
                Err(WorkflowDriveError::Cancelled(WorkflowCancellationCause::Interrupted)) => {
                    run_lifecycle.cancel().await
                }
                Err(WorkflowDriveError::Runtime(error)) => run_lifecycle.fail(error).await,
            }
        };
        let result = match AssertUnwindSafe(execution).catch_unwind().await {
            Ok(result) => result,
            Err(_) => lifecycle.cleanup_after_panic().await,
        };
        drop(lease);
        result
    }
}

impl WorkflowRunLifecycle {
    async fn drive_to_terminal(
        &self,
        started_cell: codex_code_mode::StartedCell,
        cancellation: WorkflowCancellation,
    ) -> Result<RuntimeResponse, WorkflowDriveError> {
        let initial_response = tokio::select! {
            biased;
            cause = cancellation.cancelled() => return Err(WorkflowDriveError::Cancelled(cause)),
            response = started_cell.initial_response() => response,
        };
        let mut response = initial_response.map_err(WorkflowDriveError::Runtime)?;

        let mut content_items = Vec::new();
        let mut content_bounds = WorkflowOutputBounds::default();
        loop {
            response = match response {
                RuntimeResponse::Yielded {
                    content_items: mut next_items,
                    ..
                } => {
                    content_bounds.admit(&next_items).map_err(|error| {
                        WorkflowDriveError::Runtime(format!(
                            "workflow runtime output rejected: {error}"
                        ))
                    })?;
                    content_items.append(&mut next_items);
                    let wait = self.session.wait(WaitRequest {
                        cell_id: self.cell_id.clone(),
                        yield_time_ms: codex_code_mode::DEFAULT_WAIT_YIELD_TIME_MS,
                    });
                    match tokio::select! {
                        biased;
                        cause = cancellation.cancelled() => {
                            return Err(WorkflowDriveError::Cancelled(cause));
                        }
                        outcome = wait => outcome,
                    } {
                        Ok(
                            WaitOutcome::LiveCell(response) | WaitOutcome::MissingCell(response),
                        ) => response,
                        Err(error) => return Err(WorkflowDriveError::Runtime(error)),
                    }
                }
                RuntimeResponse::Result {
                    cell_id,
                    content_items: mut next_items,
                    error_text,
                } => {
                    content_bounds.admit(&next_items).map_err(|error| {
                        WorkflowDriveError::Runtime(format!(
                            "workflow runtime output rejected: {error}"
                        ))
                    })?;
                    content_items.append(&mut next_items);
                    return Ok(RuntimeResponse::Result {
                        cell_id,
                        content_items,
                        error_text,
                    });
                }
                RuntimeResponse::Terminated {
                    cell_id,
                    content_items: mut next_items,
                } => {
                    content_bounds.admit(&next_items).map_err(|error| {
                        WorkflowDriveError::Runtime(format!(
                            "workflow runtime output rejected: {error}"
                        ))
                    })?;
                    content_items.append(&mut next_items);
                    return Ok(RuntimeResponse::Terminated {
                        cell_id,
                        content_items,
                    });
                }
            };
        }
    }

    fn output(&self, response: RuntimeResponse) -> WorkflowRunOutput {
        WorkflowRunOutput {
            response,
            run_id: self.run_id.clone(),
        }
    }

    async fn finish_terminal_response(
        &self,
        response: RuntimeResponse,
    ) -> Result<WorkflowRunOutput, FunctionCallError> {
        let completion = match &response {
            RuntimeResponse::Result {
                error_text: Some(error),
                ..
            } => WorkflowRunCompletion::Errored(error.clone()),
            RuntimeResponse::Result {
                error_text: None, ..
            } => WorkflowRunCompletion::Completed,
            RuntimeResponse::Terminated { .. } => WorkflowRunCompletion::Interrupted,
            RuntimeResponse::Yielded { .. } => {
                return Err(FunctionCallError::Fatal(
                    "internal workflow lifecycle error: yielded response treated as terminal"
                        .to_string(),
                ));
            }
        };
        self.dispatch_broker
            .drain_workflow_cell(&self.cell_id)
            .await;
        let journal_status = completion.journal_status();
        let publication = self
            .event_target
            .complete_cell(&self.ledger, &self.cell_id, completion)
            .await;
        let progress_persisted = publication.map(|publication| publication.progress_persisted);
        let metadata_persisted_by_event = publication
            .map(|publication| publication.metadata_persisted)
            .unwrap_or(false);
        let metadata_persisted_directly =
            publication.is_some() && self.persist_terminal_status(journal_status);
        let metadata_persisted = metadata_persisted_by_event
            || metadata_persisted_directly
            || if publication.is_none() {
                match self.paths.read_meta_bounded() {
                    Ok(meta) => meta.status == journal_status,
                    Err(error) => {
                        warn!(
                            "failed to verify terminal workflow status for {}: {error}",
                            self.run_id
                        );
                        false
                    }
                }
            } else {
                false
            };
        let recorder_closed = self.shutdown_recorder().await;
        self.dispatch_broker.close_cell(&self.cell_id);
        if !metadata_persisted || !recorder_closed {
            warn!(
                "workflow run {} failed its terminal durability boundary (progress_persisted={progress_persisted:?}, metadata_persisted={metadata_persisted}, recorder_closed={recorder_closed})",
                self.run_id
            );
            return Err(FunctionCallError::RespondToModel(format!(
                "workflow run `{}` finished, but its terminal state could not be durably committed",
                self.run_id
            )));
        }
        Ok(self.output(response))
    }

    async fn fail(&self, error: String) -> Result<WorkflowRunOutput, FunctionCallError> {
        if let Err(terminate_error) = self.session.terminate(self.cell_id.clone()).await {
            warn!(
                "failed to terminate errored workflow run {}: {terminate_error}",
                self.run_id
            );
        }
        self.dispatch_broker
            .drain_workflow_cell(&self.cell_id)
            .await;
        let publication = self
            .event_target
            .complete_cell(
                &self.ledger,
                &self.cell_id,
                WorkflowRunCompletion::Errored(truncate_model_error(error.clone())),
            )
            .await;
        if publication.is_some() {
            self.persist_terminal_status(JournalRunStatus::Failed);
        }
        self.shutdown_recorder().await;
        self.dispatch_broker.close_cell(&self.cell_id);
        Err(FunctionCallError::RespondToModel(error))
    }

    async fn cancel(&self) -> Result<WorkflowRunOutput, FunctionCallError> {
        if let Err(error) = self.session.terminate(self.cell_id.clone()).await {
            warn!(
                "failed to terminate cancelled workflow run {}: {error}",
                self.run_id
            );
        }
        self.dispatch_broker
            .drain_workflow_cell(&self.cell_id)
            .await;
        let publication = self
            .event_target
            .complete_cell(
                &self.ledger,
                &self.cell_id,
                WorkflowRunCompletion::Interrupted,
            )
            .await;
        if publication.is_some() {
            self.persist_terminal_status(JournalRunStatus::Failed);
        }
        self.shutdown_recorder().await;
        self.dispatch_broker.close_cell(&self.cell_id);
        Err(FunctionCallError::RespondToModel(format!(
            "workflow run `{}` was cancelled",
            self.run_id
        )))
    }

    async fn stop(
        &self,
        cancellation: &WorkflowCancellation,
    ) -> Result<WorkflowRunOutput, FunctionCallError> {
        // Claim and durably publish the user-stop terminal before terminating
        // the runtime. Its host callback reports `Interrupted`; claiming first
        // prevents that cleanup callback from relabeling the run.
        let publication = self
            .event_target
            .complete_cell(&self.ledger, &self.cell_id, WorkflowRunCompletion::Stopped)
            .await;
        let stopped = if let Some(publication) = publication {
            let metadata_persisted = self.persist_terminal_status(JournalRunStatus::Stopped);
            publication.metadata_persisted || metadata_persisted
        } else {
            false
        };
        cancellation.resolve_user_stop(stopped);
        if let Err(error) = self.session.terminate(self.cell_id.clone()).await {
            warn!(
                "failed to terminate stopped workflow run {}: {error}",
                self.run_id
            );
        }
        self.dispatch_broker
            .drain_workflow_cell(&self.cell_id)
            .await;
        self.shutdown_recorder().await;
        self.dispatch_broker.close_cell(&self.cell_id);
        Err(FunctionCallError::RespondToModel(format!(
            "workflow run `{}` was stopped",
            self.run_id
        )))
    }

    async fn pause(
        &self,
        cancellation: &WorkflowCancellation,
    ) -> Result<WorkflowRunOutput, FunctionCallError> {
        // Reserve the first-writer terminal claim before terminating V8. The
        // runtime reports its own termination as Interrupted, so reserving is
        // what prevents that callback from stealing an authenticated pause.
        // Publication is deliberately delayed until all child callbacks drain
        // and the journal recorder closes successfully. A checkpoint whose
        // journal could not be flushed is not resumable.
        let terminal_facts = self
            .event_target
            .claim_cell_terminal(&self.ledger, &self.cell_id);
        if let Err(error) = self.session.terminate(self.cell_id.clone()).await {
            warn!(
                "failed to terminate paused workflow run {}: {error}",
                self.run_id
            );
        }
        self.dispatch_broker
            .drain_workflow_cell(&self.cell_id)
            .await;
        let recorder_closed = self.shutdown_recorder().await;
        let paused = if let Some(facts) = terminal_facts {
            if recorder_closed {
                let publication = self
                    .event_target
                    .complete_claimed_cell(facts, WorkflowRunCompletion::Paused)
                    .await;
                let metadata_persisted = self.persist_terminal_status(JournalRunStatus::Paused);
                publication.progress_persisted
                    && (publication.metadata_persisted || metadata_persisted)
            } else {
                let publication = self
                    .event_target
                    .complete_claimed_cell(facts, WorkflowRunCompletion::Interrupted)
                    .await;
                let _metadata_persisted = publication.metadata_persisted
                    || self.persist_terminal_status(JournalRunStatus::Failed);
                false
            }
        } else {
            false
        };
        cancellation.resolve_pause(paused);
        self.dispatch_broker.close_cell(&self.cell_id);
        Err(FunctionCallError::RespondToModel(format!(
            "workflow run `{}` was paused",
            self.run_id
        )))
    }

    async fn cleanup_after_panic(&self) -> Result<WorkflowRunOutput, FunctionCallError> {
        if let Err(error) = self.session.terminate(self.cell_id.clone()).await {
            warn!(
                "failed to terminate panicked workflow run {}: {error}",
                self.run_id
            );
        }
        self.dispatch_broker
            .drain_workflow_cell(&self.cell_id)
            .await;
        let publication = self
            .event_target
            .complete_cell(
                &self.ledger,
                &self.cell_id,
                WorkflowRunCompletion::Interrupted,
            )
            .await;
        if publication.is_some() {
            self.persist_status_after_panic().await;
        }
        self.shutdown_recorder().await;
        self.dispatch_broker.close_cell(&self.cell_id);
        Err(FunctionCallError::RespondToModel(format!(
            "workflow run `{}` panicked while executing",
            self.run_id
        )))
    }

    async fn persist_status_after_panic(&self) {
        let meta = match self.paths.read_meta_bounded() {
            Ok(meta) if meta.status == JournalRunStatus::Running => meta,
            Ok(_) => return,
            Err(error) => {
                warn!(
                    "failed to read workflow metadata after panic for {}: {error}",
                    self.run_id
                );
                return;
            }
        };
        let status = match crate::tools::code_mode::workflow_progress::durable::terminalize_interrupted(
            &self.codex_home,
            &meta,
        )
        .await
        {
            Ok(
                crate::tools::code_mode::workflow_progress::durable::DurableRecoveryTerminal::Existing(
                    DurableRunStatus::Completed(_),
                ),
            ) => JournalRunStatus::Completed,
            Ok(
                crate::tools::code_mode::workflow_progress::durable::DurableRecoveryTerminal::Existing(
                    DurableRunStatus::Stopped,
                ),
            ) => JournalRunStatus::Stopped,
            Ok(
                crate::tools::code_mode::workflow_progress::durable::DurableRecoveryTerminal::Existing(
                    DurableRunStatus::Paused,
                ),
            ) => JournalRunStatus::Paused,
            Ok(_) => JournalRunStatus::Failed,
            Err(error) => {
                warn!(
                    "failed to persist workflow progress after panic for {}: {error}",
                    self.run_id
                );
                JournalRunStatus::Failed
            }
        };
        if let Err(error) = self.paths.update_status(status) {
            warn!(
                "failed to persist workflow metadata after panic for {}: {error}",
                self.run_id
            );
        }
    }

    async fn shutdown_recorder(&self) -> bool {
        match self.recorder.shutdown().await {
            Ok(()) => true,
            Err(error) => {
                warn!(
                    "failed to close workflow journal for {}: {error}",
                    self.run_id
                );
                false
            }
        }
    }

    fn persist_terminal_status(&self, status: JournalRunStatus) -> bool {
        match self.paths.update_status(status) {
            Ok(meta) => meta.status == status,
            Err(error) => {
                warn!(
                    "failed to persist terminal workflow status for {}: {error}",
                    self.run_id
                );
                false
            }
        }
    }
}
