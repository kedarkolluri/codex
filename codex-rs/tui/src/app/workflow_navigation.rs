//! Workflow-monitor drill-in and return transitions.

use super::*;

const MAX_WORKFLOW_MONITOR_RETURN_TARGETS: usize = 4;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkflowMonitorReturnTarget {
    parent_thread_id: ThreadId,
    child_thread_id: ThreadId,
    run_id: String,
    node_id: u64,
    detach_child_on_return: bool,
}

#[derive(Clone, Copy, Debug)]
enum WorkflowChildDetachReason {
    DrillSelectionFailed,
    ReturnedToMonitor,
}

impl App {
    pub(super) async fn select_workflow_agent_thread(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        child_thread_id: ThreadId,
        run_id: String,
        node_id: u64,
    ) -> Result<()> {
        let Some(parent_thread_id) = self.current_displayed_thread_id() else {
            self.chat_widget
                .add_error_message("No active workflow monitor thread is available.".to_string());
            return Ok(());
        };
        if parent_thread_id == child_thread_id {
            return Ok(());
        }

        let detach_child_on_return = self.should_attach_live_thread_for_selection(child_thread_id);
        let selection_result = self
            .select_agent_thread(tui, app_server, child_thread_id)
            .await;
        if self.active_thread_id != Some(child_thread_id) {
            if detach_child_on_return {
                self.detach_workflow_child(
                    app_server,
                    child_thread_id,
                    WorkflowChildDetachReason::DrillSelectionFailed,
                )
                .await;
            }
            return selection_result;
        }

        self.push_workflow_monitor_return(WorkflowMonitorReturnTarget {
            parent_thread_id,
            child_thread_id,
            run_id,
            node_id,
            detach_child_on_return,
        });
        self.chat_widget.set_footer_hint_override(Some(vec![(
            "Esc".to_string(),
            "return to workflow monitor".to_string(),
        )]));
        selection_result
    }

    pub(super) async fn maybe_return_to_workflow_monitor(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        key_event: KeyEvent,
    ) -> bool {
        if self.overlay.is_some()
            || !self.chat_widget.no_modal_or_popup_active()
            || !self.chat_widget.composer_is_empty()
            || !matches!(
                key_event,
                KeyEvent {
                    code: KeyCode::Esc,
                    kind: KeyEventKind::Press | KeyEventKind::Repeat,
                    ..
                }
            )
        {
            return false;
        }

        let Some(target) = self.workflow_monitor_returns.last().cloned() else {
            return false;
        };
        if self.active_thread_id != Some(target.child_thread_id) {
            self.clear_workflow_monitor_returns();
            return false;
        }

        if let Err(error) = self
            .select_agent_thread(tui, app_server, target.parent_thread_id)
            .await
        {
            self.chat_widget
                .add_error_message(format!("Failed to return to the workflow monitor: {error}"));
            return true;
        }
        if self.active_thread_id != Some(target.parent_thread_id) {
            self.chat_widget
                .add_error_message("Failed to return to the workflow monitor thread.".to_string());
            return true;
        }

        self.workflow_monitor_returns.pop();
        self.chat_widget
            .restore_workflow_monitor_focus(&target.run_id, target.node_id);
        if target.detach_child_on_return {
            self.detach_workflow_child(
                app_server,
                target.child_thread_id,
                WorkflowChildDetachReason::ReturnedToMonitor,
            )
            .await;
        }
        true
    }

    pub(super) fn clear_workflow_monitor_returns(&mut self) {
        self.workflow_monitor_returns.clear();
    }

    fn push_workflow_monitor_return(&mut self, target: WorkflowMonitorReturnTarget) {
        if self.workflow_monitor_returns.len() >= MAX_WORKFLOW_MONITOR_RETURN_TARGETS {
            let promoted_root = self.workflow_monitor_returns.remove(/*index*/ 0);
            // Return targets form a strict parent-child chain. Once the oldest link is evicted,
            // its child becomes the retained chain's root, so its attachment is intentionally
            // promoted instead of detached out from under the next return target.
            debug_assert_eq!(
                self.workflow_monitor_returns
                    .first()
                    .map(|next| next.parent_thread_id),
                Some(promoted_root.child_thread_id)
            );
        }
        self.workflow_monitor_returns.push(target);
    }

    async fn detach_workflow_child(
        &mut self,
        app_server: &mut AppServerSession,
        child_thread_id: ThreadId,
        reason: WorkflowChildDetachReason,
    ) {
        let is_live_attachment = self
            .thread_event_channels
            .get(&child_thread_id)
            .is_some_and(|channel| channel.attachment() == ThreadEventAttachment::Live);
        if is_live_attachment
            && let Err(error) = app_server.thread_unsubscribe(child_thread_id).await
        {
            tracing::warn!(
                %child_thread_id,
                %error,
                ?reason,
                "failed to detach workflow child"
            );
            if matches!(reason, WorkflowChildDetachReason::ReturnedToMonitor) {
                self.chat_widget.add_error_message(format!(
                    "Returned to the workflow monitor, but failed to detach agent {child_thread_id}: {error}"
                ));
            }
            return;
        }

        self.abort_thread_event_listener(child_thread_id);
        self.thread_event_channels.remove(&child_thread_id);
        self.refresh_pending_thread_approvals().await;
    }
}

#[cfg(test)]
#[path = "workflow_navigation_tests.rs"]
mod tests;
