//! Saved-workflow picker and `/workflow` request preparation.
//!
//! Runtime execution stays in app-server. This module owns only the TUI-facing registry picker,
//! bounded inline argument parsing, and request/result presentation.

use super::ChatWidget;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use codex_app_server_protocol::WorkflowMetadata;
use codex_app_server_protocol::WorkflowScope;
use codex_features::Feature;
use codex_protocol::ThreadId;
use serde_json::Value as JsonValue;

pub(super) const WORKFLOW_PICKER_VIEW_ID: &str = "workflow-picker";

const WORKFLOW_START_MAX_NAME_BYTES: usize = 256;
const WORKFLOW_START_MAX_ARGS_BYTES: usize = 32 * 1024;
const WORKFLOW_ERROR_MAX_CHARS: usize = 512;
const WORKFLOW_METADATA_MAX_CHARS: usize = 180;
const WORKFLOW_USAGE: &str = "Usage: /workflow <name> [JSON]";

impl ChatWidget {
    pub(super) fn open_workflow_picker(&mut self) {
        if !self.config.features.enabled(Feature::Workflow) {
            return;
        }
        let Some(thread_id) = self.thread_id else {
            self.add_error_message(
                "'/workflow' is unavailable before the session starts.".to_string(),
            );
            return;
        };

        self.bottom_pane
            .show_selection_view(workflow_picker_loading_params());
        self.request_redraw();
        self.request_workflow_list(thread_id);
    }

    pub(super) fn start_workflow_from_inline_args(&mut self, args: &str) {
        if !self.config.features.enabled(Feature::Workflow) {
            return;
        }
        let Some(thread_id) = self.thread_id else {
            self.add_error_message(
                "'/workflow' is unavailable before the session starts.".to_string(),
            );
            return;
        };

        match parse_workflow_invocation(args) {
            Ok((name, args)) => {
                self.app_event_tx.send(AppEvent::StartSavedWorkflow {
                    thread_id,
                    name,
                    args,
                });
            }
            Err(message) => self.add_error_message(message),
        }
    }

    pub(crate) fn on_workflow_list_result(
        &mut self,
        thread_id: ThreadId,
        result: Result<Vec<WorkflowMetadata>, String>,
    ) {
        if self.thread_id != Some(thread_id) {
            return;
        }

        let params = match result {
            Ok(workflows) => workflow_picker_params(thread_id, workflows),
            Err(error) => {
                let error = bounded_workflow_text(&error, WORKFLOW_ERROR_MAX_CHARS);
                self.add_error_message(format!("Failed to load saved workflows: {error}"));
                workflow_picker_error_params(error)
            }
        };
        self.bottom_pane
            .replace_selection_view_if_active(WORKFLOW_PICKER_VIEW_ID, params);
        self.request_redraw();
    }

    pub(crate) fn on_workflow_start_requested(&mut self, thread_id: ThreadId, name: &str) {
        if self.thread_id != Some(thread_id) {
            return;
        }
        let name = bounded_workflow_text(name, WORKFLOW_START_MAX_NAME_BYTES);
        self.add_info_message(
            format!("Starting saved workflow `{name}`…"),
            /*hint*/ None,
        );
    }

    pub(crate) fn on_workflow_start_failed(
        &mut self,
        thread_id: ThreadId,
        name: &str,
        error: &str,
    ) {
        if self.thread_id != Some(thread_id) {
            return;
        }
        let name = bounded_workflow_text(name, WORKFLOW_START_MAX_NAME_BYTES);
        let error = bounded_workflow_text(error, WORKFLOW_ERROR_MAX_CHARS);
        self.add_error_message(format!("Failed to start saved workflow `{name}`: {error}"));
    }

    pub(super) fn on_workflows_changed(&mut self) {
        if !self.config.features.enabled(Feature::Workflow) {
            return;
        }
        let Some(thread_id) = self.thread_id else {
            return;
        };
        if self.bottom_pane.replace_selection_view_if_active(
            WORKFLOW_PICKER_VIEW_ID,
            workflow_picker_loading_params(),
        ) {
            self.request_workflow_list(thread_id);
        }
    }

    fn request_workflow_list(&self, thread_id: ThreadId) {
        self.app_event_tx
            .send(AppEvent::LoadSavedWorkflows { thread_id });
    }
}

fn parse_workflow_invocation(args: &str) -> Result<(String, Option<JsonValue>), String> {
    let args = args.trim();
    let name_end = args.find(char::is_whitespace).unwrap_or(args.len());
    let name = &args[..name_end];
    if name.is_empty() {
        return Err(WORKFLOW_USAGE.to_string());
    }
    if name.len() > WORKFLOW_START_MAX_NAME_BYTES {
        return Err(format!(
            "Workflow name exceeds the {WORKFLOW_START_MAX_NAME_BYTES}-byte limit."
        ));
    }

    let raw_json = args[name_end..].trim();
    if raw_json.is_empty() {
        return Ok((name.to_string(), None));
    }
    if raw_json.len() > WORKFLOW_START_MAX_ARGS_BYTES {
        return Err(format!(
            "Workflow arguments exceed the {WORKFLOW_START_MAX_ARGS_BYTES}-byte limit."
        ));
    }
    let parsed = serde_json::from_str(raw_json).map_err(|error| {
        let error = bounded_workflow_text(&error.to_string(), WORKFLOW_ERROR_MAX_CHARS);
        format!("Invalid workflow JSON arguments: {error}. {WORKFLOW_USAGE}")
    })?;
    Ok((name.to_string(), Some(parsed)))
}

fn workflow_picker_loading_params() -> SelectionViewParams {
    SelectionViewParams {
        view_id: Some(WORKFLOW_PICKER_VIEW_ID),
        title: Some("Saved workflows".to_string()),
        subtitle: Some("Refreshing workflows available to this thread…".to_string()),
        items: vec![SelectionItem {
            name: "Loading saved workflows…".to_string(),
            is_disabled: true,
            ..Default::default()
        }],
        footer_hint: Some("Press esc to go back".into()),
        ..Default::default()
    }
}

fn workflow_picker_error_params(error: String) -> SelectionViewParams {
    SelectionViewParams {
        view_id: Some(WORKFLOW_PICKER_VIEW_ID),
        title: Some("Saved workflows".to_string()),
        subtitle: Some("The workflow registry could not be refreshed.".to_string()),
        items: vec![SelectionItem {
            name: "Could not load saved workflows".to_string(),
            description: Some(error),
            is_disabled: true,
            ..Default::default()
        }],
        footer_hint: Some("Press esc to go back".into()),
        ..Default::default()
    }
}

fn workflow_picker_params(
    thread_id: ThreadId,
    workflows: Vec<WorkflowMetadata>,
) -> SelectionViewParams {
    let has_workflows = !workflows.is_empty();
    let items = if !has_workflows {
        vec![SelectionItem {
            name: "No saved workflows found".to_string(),
            description: Some("Add a saved workflow, then reopen /workflow.".to_string()),
            is_disabled: true,
            ..Default::default()
        }]
    } else {
        workflows
            .into_iter()
            .map(|workflow| workflow_picker_item(thread_id, workflow))
            .collect()
    };
    SelectionViewParams {
        view_id: Some(WORKFLOW_PICKER_VIEW_ID),
        title: Some("Saved workflows".to_string()),
        subtitle: Some("Choose a saved workflow to start in this thread.".to_string()),
        items,
        is_searchable: true,
        search_placeholder: Some("Search workflows".to_string()),
        footer_hint: (!has_workflows).then(|| "Press esc to go back".into()),
        ..Default::default()
    }
}

fn workflow_picker_item(thread_id: ThreadId, workflow: WorkflowMetadata) -> SelectionItem {
    let WorkflowMetadata {
        name,
        description,
        phases,
        scope,
        ..
    } = workflow;
    let display_name = bounded_workflow_text(&name, WORKFLOW_START_MAX_NAME_BYTES);
    let description = bounded_workflow_text(&description, WORKFLOW_METADATA_MAX_CHARS);
    let phase_label = match phases.len() {
        1 => "1 phase".to_string(),
        count => format!("{count} phases"),
    };
    let scope_label = match scope {
        WorkflowScope::Project => "project",
        WorkflowScope::Personal => "personal",
        WorkflowScope::CodexHome => "Codex home",
    };
    let row_description = if description.is_empty() {
        format!("{scope_label} · {phase_label}")
    } else {
        format!("{description} · {scope_label} · {phase_label}")
    };
    let search_name = name.clone();

    SelectionItem {
        name: display_name,
        description: Some(row_description),
        search_value: Some(search_name),
        actions: vec![Box::new(move |tx| {
            tx.send(AppEvent::StartSavedWorkflow {
                thread_id,
                name: name.clone(),
                args: None,
            });
        })],
        dismiss_on_select: true,
        ..Default::default()
    }
}

fn bounded_workflow_text(text: &str, max_chars: usize) -> String {
    let mut output = String::with_capacity(text.len().min(max_chars));
    let mut chars_written: usize = 0;
    let mut pending_space = false;
    let mut truncated = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        let required_chars = 1 + usize::from(pending_space);
        if chars_written.saturating_add(required_chars) > max_chars {
            truncated = true;
            break;
        }
        if pending_space {
            output.push(' ');
            pending_space = false;
            chars_written += 1;
        }
        output.push(ch);
        chars_written += 1;
    }
    if truncated {
        output.push('…');
    }
    output
}
