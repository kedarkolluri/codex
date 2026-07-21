use codex_extension_api::ExtensionData;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;
use tokio::select;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::turn_lifecycle::TurnGeneration;

/// Durable cursor for compensating callbacks entered during exact task start.
#[derive(Debug, Default, Eq, PartialEq)]
#[must_use = "entered turn-start callbacks must be compensated after cancellation"]
pub(super) struct TurnStartLifecycleProgress {
    entered: usize,
    next_abort: usize,
}

impl Session {
    pub(super) async fn emit_turn_start_lifecycle(
        &self,
        turn_context: &TurnContext,
        token_usage_at_turn_start: &TokenUsage,
    ) {
        let collaboration_mode = turn_context.collaboration_mode();
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            contributor
                .on_turn_start(codex_extension_api::TurnStartInput {
                    turn_id: turn_context.sub_id.as_str(),
                    collaboration_mode: &collaboration_mode,
                    token_usage_at_turn_start,
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store: turn_context.extension_data.as_ref(),
                })
                .await;
        }
    }

    /// Emits exact-start callbacks until cancellation while recording every
    /// callback entered before its first await.
    pub(super) async fn emit_cancellable_turn_start_lifecycle(
        &self,
        turn_context: &TurnContext,
        token_usage_at_turn_start: &TokenUsage,
        generation: &TurnGeneration,
        progress: &mut TurnStartLifecycleProgress,
    ) {
        let collaboration_mode = turn_context.collaboration_mode();
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            if generation.cancel_reason().is_some() {
                break;
            }
            progress.entered += 1;
            select! {
                _ = generation.cancelled() => break,
                _ = contributor.on_turn_start(codex_extension_api::TurnStartInput {
                    turn_id: turn_context.sub_id.as_str(),
                    collaboration_mode: &collaboration_mode,
                    token_usage_at_turn_start,
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store: turn_context.extension_data.as_ref(),
                }) => {}
            }
        }
    }

    pub(super) async fn emit_turn_stop_lifecycle(&self, turn_store: &ExtensionData) {
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            contributor
                .on_turn_stop(codex_extension_api::TurnStopInput {
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store,
                })
                .await;
        }
    }

    pub(crate) async fn emit_thread_idle_lifecycle_if_idle(&self) {
        if self.active_turn.lock().await.has_active_turn()
            || self.input_queue.has_trigger_turn_mailbox_items().await
        {
            return;
        }

        for contributor in self.services.extensions.thread_lifecycle_contributors() {
            contributor
                .on_thread_idle(codex_extension_api::ThreadIdleInput {
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                })
                .await;
        }
    }

    pub(super) async fn emit_turn_abort_lifecycle(
        &self,
        reason: TurnAbortReason,
        turn_store: &ExtensionData,
    ) {
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            contributor
                .on_turn_abort(codex_extension_api::TurnAbortInput {
                    reason: reason.clone(),
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store,
                })
                .await;
        }
    }

    /// Compensates only entered start callbacks. A cancelled in-flight abort
    /// is retried, while callbacks that returned successfully are not replayed.
    pub(super) async fn emit_entered_turn_abort_lifecycle(
        &self,
        reason: TurnAbortReason,
        turn_store: &ExtensionData,
        progress: &mut TurnStartLifecycleProgress,
    ) {
        let contributors = self.services.extensions.turn_lifecycle_contributors();
        while progress.next_abort < progress.entered {
            let Some(contributor) = contributors.get(progress.next_abort) else {
                unreachable!("entered turn-start contributor must remain registered");
            };
            contributor
                .on_turn_abort(codex_extension_api::TurnAbortInput {
                    reason: reason.clone(),
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store,
                })
                .await;
            progress.next_abort += 1;
        }
    }

    pub(crate) async fn emit_turn_error_lifecycle(
        &self,
        turn_context: &TurnContext,
        error: CodexErrorInfo,
    ) {
        for contributor in self.services.extensions.turn_lifecycle_contributors() {
            contributor
                .on_turn_error(codex_extension_api::TurnErrorInput {
                    turn_id: turn_context.sub_id.as_str(),
                    error: error.clone(),
                    session_store: &self.services.session_extension_data,
                    thread_store: &self.services.thread_extension_data,
                    turn_store: turn_context.extension_data.as_ref(),
                })
                .await;
        }
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
