//! Desktop notification coalescing for `ChatWidget`.

use super::*;

impl ChatWidget {
    pub(super) fn notify(&mut self, notification: Notification) {
        if !notification.allowed_for(&self.config.tui_notifications.notifications) {
            return;
        }
        if let Some(existing) = self.pending_notification.as_ref()
            && existing.priority() > notification.priority()
        {
            return;
        }
        self.pending_notification = Some(notification);
        self.request_redraw();
    }

    pub(crate) fn maybe_post_pending_notification(&mut self, tui: &mut crate::tui::Tui) {
        if let Some(notif) = self.pending_notification.take() {
            tui.notify(notif.display());
        }
    }
}

#[derive(Debug)]
pub(super) enum Notification {
    AgentTurnComplete {
        response: String,
    },
    WorkflowComplete {
        name: String,
        status: codex_app_server_protocol::CollabAgentStatus,
        agent_count: Option<usize>,
        spent: i64,
    },
    ExecApprovalRequested {
        command: String,
    },
    EditApprovalRequested {
        cwd: PathBuf,
        changes: Vec<PathBuf>,
    },
    ElicitationRequested {
        server_name: String,
    },
    PlanModePrompt {
        title: String,
    },
}

impl Notification {
    pub(super) fn display(&self) -> String {
        match self {
            Notification::AgentTurnComplete { response } => {
                Notification::agent_turn_preview(response)
                    .unwrap_or_else(|| "Agent turn complete".to_string())
            }
            Notification::WorkflowComplete {
                name,
                status,
                agent_count,
                spent,
            } => {
                let name = bounded_normalized_text(name, WORKFLOW_NOTIFICATION_NAME_GRAPHEMES);
                let status = workflow_status_label(status);
                let mut summary = match name {
                    Some(name) => format!("Workflow {name} {status}"),
                    None => format!("Workflow {status}"),
                };
                if let Some(agent_count) = agent_count {
                    let noun = if *agent_count == 1 { "agent" } else { "agents" };
                    summary.push_str(&format!(" · {agent_count} {noun}"));
                }
                summary.push_str(&format!(
                    " · spent {} weighted tokens",
                    format_tokens_compact(*spent)
                ));
                truncate_text(&summary, WORKFLOW_NOTIFICATION_GRAPHEMES)
            }
            Notification::ExecApprovalRequested { command } => {
                format!(
                    "Approval requested: {}",
                    truncate_text(command, /*max_graphemes*/ 30)
                )
            }
            Notification::EditApprovalRequested { cwd, changes } => {
                format!(
                    "Codex wants to edit {}",
                    if changes.len() == 1 {
                        #[allow(clippy::unwrap_used)]
                        display_path_for(changes.first().unwrap(), cwd)
                    } else {
                        format!("{} files", changes.len())
                    }
                )
            }
            Notification::ElicitationRequested { server_name } => {
                format!("Approval requested by {server_name}")
            }
            Notification::PlanModePrompt { title } => {
                format!("Plan mode prompt: {title}")
            }
        }
    }

    fn type_name(&self) -> &str {
        match self {
            Notification::AgentTurnComplete { .. } => "agent-turn-complete",
            Notification::WorkflowComplete { .. } => "workflow-complete",
            Notification::ExecApprovalRequested { .. }
            | Notification::EditApprovalRequested { .. }
            | Notification::ElicitationRequested { .. } => "approval-requested",
            Notification::PlanModePrompt { .. } => "plan-mode-prompt",
        }
    }

    fn priority(&self) -> u8 {
        match self {
            Notification::AgentTurnComplete { .. } => 0,
            Notification::WorkflowComplete { .. } => 1,
            Notification::ExecApprovalRequested { .. }
            | Notification::EditApprovalRequested { .. }
            | Notification::ElicitationRequested { .. }
            | Notification::PlanModePrompt { .. } => 2,
        }
    }

    pub(super) fn allowed_for(&self, settings: &Notifications) -> bool {
        match settings {
            Notifications::Enabled(enabled) => *enabled,
            Notifications::Custom(allowed) => allowed.iter().any(|a| a == self.type_name()),
        }
    }

    pub(super) fn agent_turn_preview(response: &str) -> Option<String> {
        let mut normalized = String::new();
        for part in response.split_whitespace() {
            if !normalized.is_empty() {
                normalized.push(' ');
            }
            normalized.push_str(part);
        }
        let trimmed = normalized.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(truncate_text(trimmed, AGENT_NOTIFICATION_PREVIEW_GRAPHEMES))
        }
    }

    pub(super) fn user_input_request_summary(
        questions: &[codex_app_server_protocol::ToolRequestUserInputQuestion],
    ) -> Option<String> {
        let first_question = questions.first()?;
        let summary = if first_question.header.trim().is_empty() {
            first_question.question.trim()
        } else {
            first_question.header.trim()
        };
        if summary.is_empty() {
            None
        } else {
            Some(truncate_text(summary, /*max_graphemes*/ 30))
        }
    }
}

const AGENT_NOTIFICATION_PREVIEW_GRAPHEMES: usize = 200;
const WORKFLOW_NOTIFICATION_NAME_GRAPHEMES: usize = 72;
const WORKFLOW_NOTIFICATION_GRAPHEMES: usize = 160;

fn bounded_normalized_text(text: &str, max_graphemes: usize) -> Option<String> {
    if max_graphemes == 0 {
        return None;
    }

    let mut normalized = String::new();
    let mut grapheme_count = 0;
    let mut truncated = false;
    'words: for word in text.split_whitespace() {
        if !normalized.is_empty() {
            if grapheme_count == max_graphemes {
                truncated = true;
                break;
            }
            normalized.push(' ');
            grapheme_count += 1;
        }
        for grapheme in word.graphemes(true) {
            if grapheme_count == max_graphemes {
                truncated = true;
                break 'words;
            }
            normalized.push_str(grapheme);
            grapheme_count += 1;
        }
    }

    if normalized.is_empty() {
        return None;
    }
    if truncated && let Some((last_grapheme, _)) = normalized.grapheme_indices(true).next_back() {
        normalized.truncate(last_grapheme);
        normalized.push('…');
    }
    Some(normalized)
}

fn workflow_status_label(status: &codex_app_server_protocol::CollabAgentStatus) -> &'static str {
    use codex_app_server_protocol::CollabAgentStatus;

    match status {
        CollabAgentStatus::PendingInit => "pending",
        CollabAgentStatus::Running => "running",
        CollabAgentStatus::Interrupted => "interrupted",
        CollabAgentStatus::Completed => "completed",
        CollabAgentStatus::Errored => "errored",
        CollabAgentStatus::Shutdown => "shut down",
        CollabAgentStatus::NotFound => "not found",
    }
}

#[cfg(test)]
#[path = "notifications_tests.rs"]
mod tests;
