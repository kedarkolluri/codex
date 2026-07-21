use super::input_queue::TurnInput;
use super::session::Session;
use super::turn_context::TurnContext;
use crate::codex_thread::TryStartTurnIfIdleError;
use crate::codex_thread::TryStartTurnIfIdleRejectionReason;
use crate::tasks::RegularTask;
use crate::tasks::TaskStartOutcome;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::ResponseItem;
use std::sync::Arc;

impl Session {
    /// Returns the input if there is no active turn to inject into.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    pub async fn inject_if_running(
        &self,
        input: Vec<ResponseItem>,
    ) -> Result<(), Vec<ResponseItem>> {
        let active = self.active_turn.lock().await;
        match active.running_turn() {
            Some(running_turn) => {
                self.input_queue
                    .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                        running_turn.turn_state().as_ref(),
                        input.into_iter().map(TurnInput::ResponseItem).collect(),
                    )
                    .await;
                Ok(())
            }
            None => Err(input),
        }
    }

    /// Starts a regular turn with the provided items only if automatic idle work
    /// is allowed for the current session state.
    ///
    /// This is the shared gate for extension-initiated idle work. It refuses to
    /// start a turn when user/client-triggered work is queued, any task is still
    /// active, or the session is currently in Plan mode. Active Review tasks are
    /// covered by the active-task check because Review turns are not steerable.
    pub(crate) async fn try_start_turn_if_idle(
        self: &Arc<Self>,
        input: Vec<ResponseItem>,
    ) -> Result<(), TryStartTurnIfIdleError> {
        if input.is_empty() {
            return Ok(());
        }
        let admission_permit = self.turn_start_gate.acquire_start_permit().await;
        let Some(ticket) = self.turn_start_gate.automatic_start_ticket() else {
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::Busy,
                input,
            ));
        };
        if self.input_queue.has_trigger_turn_mailbox_items().await {
            drop(admission_permit);
            self.maybe_start_turn_for_pending_work_with_retry_ticket(ticket)
                .await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTriggerTurn,
                input,
            ));
        }
        if self.collaboration_mode().await.mode == ModeKind::Plan {
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PlanMode,
                input,
            ));
        }

        {
            let active_turn = self.active_turn.lock().await;
            if !self.turn_start_gate.admits_automatic_start(ticket) || active_turn.has_active_turn()
            {
                return Err(TryStartTurnIfIdleError::new(
                    TryStartTurnIfIdleRejectionReason::Busy,
                    input,
                ));
            }
        }

        if self.input_queue.has_trigger_turn_mailbox_items().await {
            drop(admission_permit);
            self.maybe_start_turn_for_pending_work_with_retry_ticket(ticket)
                .await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTriggerTurn,
                input,
            ));
        }

        let turn_context = self
            .new_default_turn_with_sub_id(uuid::Uuid::new_v4().to_string())
            .await;
        if turn_context.mode == ModeKind::Plan {
            drop(admission_permit);
            self.maybe_start_turn_for_pending_work_with_retry_ticket(ticket)
                .await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PlanMode,
                input,
            ));
        }
        self.maybe_emit_model_warnings_for_turn(turn_context.as_ref())
            .await;
        if self.input_queue.has_trigger_turn_mailbox_items().await {
            drop(admission_permit);
            self.maybe_start_turn_for_pending_work_with_retry_ticket(ticket)
                .await;
            return Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::PendingTriggerTurn,
                input,
            ));
        }
        let rejected_input = input.clone();
        match self
            .start_automatic_task_with_pending_input(
                ticket,
                turn_context,
                input.into_iter().map(TurnInput::ResponseItem).collect(),
                RegularTask::new(),
                admission_permit,
            )
            .await
        {
            TaskStartOutcome::Started => Ok(()),
            TaskStartOutcome::PendingTriggerTurn => {
                self.maybe_start_turn_for_pending_work_with_retry_ticket(ticket)
                    .await;
                Err(TryStartTurnIfIdleError::new(
                    TryStartTurnIfIdleRejectionReason::PendingTriggerTurn,
                    rejected_input,
                ))
            }
            TaskStartOutcome::Busy
            | TaskStartOutcome::StartInProgress(_)
            | TaskStartOutcome::AtCapacity(_)
            | TaskStartOutcome::Cancelled(_)
            | TaskStartOutcome::Poisoned => Err(TryStartTurnIfIdleError::new(
                TryStartTurnIfIdleRejectionReason::Busy,
                rejected_input,
            )),
        }
    }

    /// Injects items into active work, or records them without starting a turn.
    pub(crate) async fn inject_no_new_turn(
        &self,
        items: Vec<ResponseItem>,
        current_turn_context: Option<&TurnContext>,
    ) {
        let Err(items) = self.inject_if_running(items).await else {
            return;
        };
        let default_turn_context;
        let turn_context = match current_turn_context {
            Some(turn_context) => turn_context,
            None => {
                default_turn_context = self.new_default_turn().await;
                default_turn_context.as_ref()
            }
        };
        self.record_conversation_items(turn_context, &items).await;
    }
}
