use std::sync::Arc;

use futures::future::BoxFuture;
use tracing::trace;

use super::RegularTask;
use super::TaskStartOutcome;
use crate::session::AutomaticTicketInvalidation;
use crate::session::PendingWorkStartRequest;
use crate::session::session::Session;
use crate::state::turn_lifecycle::TurnStartOutcome as LifecycleStartOutcome;

enum PendingWorkStartAttempt {
    Finished,
    Retry,
    AtCapacity(crate::agent::control::AgentExecutionCapacityWaiter),
}

impl Session {
    pub(super) fn maybe_start_turn_for_pending_work_with_ticket(
        self: &Arc<Self>,
        ticket: crate::session::AutomaticTurnStartTicket,
    ) -> BoxFuture<'static, ()> {
        self.drive_pending_work_start(PendingWorkStartRequest::new(
            uuid::Uuid::new_v4().to_string(),
            ticket,
            AutomaticTicketInvalidation::Suppress,
        ))
    }

    pub(crate) fn maybe_start_turn_for_pending_work_with_retry_ticket(
        self: &Arc<Self>,
        ticket: crate::session::AutomaticTurnStartTicket,
    ) -> BoxFuture<'static, ()> {
        self.drive_pending_work_start(PendingWorkStartRequest::new(
            uuid::Uuid::new_v4().to_string(),
            ticket,
            AutomaticTicketInvalidation::Retry,
        ))
    }

    /// Starts a regular turn with the provided sub-id when pending work should wake an idle
    /// session.
    ///
    /// The turn is created only when there is mailbox mail marked with `trigger_turn`, and only
    /// if the session is currently idle.
    pub(crate) fn maybe_start_turn_for_pending_work_with_sub_id(
        self: &Arc<Self>,
        sub_id: String,
    ) -> BoxFuture<'static, ()> {
        let Some(ticket) = self.turn_start_gate.automatic_start_ticket() else {
            return Box::pin(async {});
        };
        self.drive_pending_work_start(PendingWorkStartRequest::new(
            sub_id,
            ticket,
            AutomaticTicketInvalidation::Retry,
        ))
    }

    pub(crate) fn drive_pending_work_start(
        self: &Arc<Self>,
        request: PendingWorkStartRequest,
    ) -> BoxFuture<'static, ()> {
        let session = Arc::clone(self);
        let driver = self.services.runtime_handle.spawn(async move {
            session.drive_pending_work_start_owned(request).await;
        });
        Box::pin(async move {
            if let Err(error) = driver.await {
                if error.is_panic() {
                    tracing::error!(%error, "pending-work start driver panicked");
                } else {
                    trace!(%error, "pending-work start driver was cancelled during runtime shutdown");
                }
            }
        })
    }

    async fn drive_pending_work_start_owned(self: Arc<Self>, request: PendingWorkStartRequest) {
        let (mut sub_id, mut ticket, invalidation) = request.into_parts();
        loop {
            if !self.turn_start_gate.admits_automatic_start(ticket) {
                if !self.turn_start_gate.is_open() {
                    break;
                }
                match invalidation {
                    AutomaticTicketInvalidation::Suppress => break,
                    AutomaticTicketInvalidation::Retry => {
                        let Some(next_ticket) =
                            self.turn_start_gate.retry_ticket_after_invalidation(ticket)
                        else {
                            break;
                        };
                        ticket = next_ticket;
                        sub_id = uuid::Uuid::new_v4().to_string();
                    }
                }
            }
            let attempt = {
                let admission_permit = self.turn_start_gate.acquire_start_permit().await;
                self.try_start_turn_for_pending_work(sub_id.clone(), ticket, admission_permit)
                    .await
            };
            match attempt {
                PendingWorkStartAttempt::Finished => break,
                PendingWorkStartAttempt::AtCapacity(waiter) => {
                    self.schedule_trigger_turn_retry(
                        waiter,
                        PendingWorkStartRequest::new(sub_id, ticket, invalidation),
                    )
                    .await;
                    break;
                }
                PendingWorkStartAttempt::Retry => continue,
            }
        }
    }

    async fn try_start_turn_for_pending_work(
        self: &Arc<Self>,
        sub_id: String,
        ticket: crate::session::AutomaticTurnStartTicket,
        admission_permit: crate::session::TurnStartAdmissionPermit,
    ) -> PendingWorkStartAttempt {
        if !self.turn_start_gate.admits_automatic_start(ticket) {
            return if self.turn_start_gate.is_open() {
                PendingWorkStartAttempt::Retry
            } else {
                PendingWorkStartAttempt::Finished
            };
        }
        if !self.input_queue.has_trigger_turn_mailbox_items().await {
            return PendingWorkStartAttempt::Finished;
        }

        let turn_context = self.new_default_turn_with_sub_id(sub_id).await;
        self.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
            .await;
        match self
            .start_automatic_trigger_task(
                ticket,
                turn_context,
                RegularTask::new(),
                admission_permit,
            )
            .await
        {
            TaskStartOutcome::AtCapacity(waiter) => PendingWorkStartAttempt::AtCapacity(waiter),
            TaskStartOutcome::StartInProgress(generation) => {
                match generation.wait_finished().await {
                    LifecycleStartOutcome::Cancelled(_) => PendingWorkStartAttempt::Retry,
                    LifecycleStartOutcome::Committed | LifecycleStartOutcome::Poisoned(_) => {
                        PendingWorkStartAttempt::Finished
                    }
                }
            }
            TaskStartOutcome::PendingTriggerTurn => PendingWorkStartAttempt::Retry,
            TaskStartOutcome::Started | TaskStartOutcome::Busy | TaskStartOutcome::Poisoned => {
                PendingWorkStartAttempt::Finished
            }
            TaskStartOutcome::Cancelled(_) => {
                if self.turn_start_gate.is_open()
                    && !self.turn_start_gate.admits_automatic_start(ticket)
                {
                    PendingWorkStartAttempt::Retry
                } else {
                    PendingWorkStartAttempt::Finished
                }
            }
        }
    }
}
