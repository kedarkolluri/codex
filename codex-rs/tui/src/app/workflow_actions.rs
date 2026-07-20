//! App-server request routing for the TUI saved-workflow entry point.

use super::App;
use crate::app_event::WorkflowAgentControlRequest;
use crate::app_event::WorkflowRunControlTarget;
use crate::app_event::WorkflowSaveIntent;
use crate::app_event::WorkflowSaveRequest;
use crate::app_server_session::AppServerSession;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::WorkflowAgentControlParams;
use codex_app_server_protocol::WorkflowAgentControlResponse;
use codex_app_server_protocol::WorkflowListParams;
use codex_app_server_protocol::WorkflowMetadata;
use codex_app_server_protocol::WorkflowPauseDisposition;
use codex_app_server_protocol::WorkflowPauseParams;
use codex_app_server_protocol::WorkflowPauseResponse;
use codex_app_server_protocol::WorkflowReadParams;
use codex_app_server_protocol::WorkflowReadResponse;
use codex_app_server_protocol::WorkflowResumeParams;
use codex_app_server_protocol::WorkflowResumeResponse;
use codex_app_server_protocol::WorkflowRunStatus;
use codex_app_server_protocol::WorkflowSaveDisposition;
use codex_app_server_protocol::WorkflowSaveParams;
use codex_app_server_protocol::WorkflowSaveResponse;
use codex_app_server_protocol::WorkflowStartParams;
use codex_app_server_protocol::WorkflowStopDisposition;
use codex_app_server_protocol::WorkflowStopParams;
use codex_app_server_protocol::WorkflowStopResponse;
use codex_protocol::ThreadId;
use color_eyre::eyre::Result;
use color_eyre::eyre::WrapErr;
use serde_json::Value as JsonValue;
use uuid::Uuid;

const WORKFLOW_LIST_PAGE_SIZE: u32 = 100;
const WORKFLOW_LIST_MAX_PAGES: usize = 16;
const WORKFLOW_LIST_MAX_CURSOR_BYTES: usize = 256;
const WORKFLOW_PICKER_MAX_ITEMS: usize = 1_024;

impl App {
    pub(super) async fn load_saved_workflows(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) {
        let mut workflows = Vec::<WorkflowMetadata>::new();
        let mut cursor = None;
        let mut pages_loaded = 0;
        let result = loop {
            if pages_loaded == WORKFLOW_LIST_MAX_PAGES {
                break Err(format!(
                    "workflow/list exceeds the {WORKFLOW_LIST_MAX_PAGES}-page picker limit"
                ));
            }
            pages_loaded += 1;
            let response = match app_server
                .workflow_list(WorkflowListParams {
                    thread_id: thread_id.to_string(),
                    cursor: cursor.clone(),
                    limit: Some(WORKFLOW_LIST_PAGE_SIZE),
                })
                .await
            {
                Ok(response) => response,
                Err(error) => break Err(error.to_string()),
            };
            if workflows.len().saturating_add(response.data.len()) > WORKFLOW_PICKER_MAX_ITEMS {
                break Err(format!(
                    "workflow registry exceeds the {WORKFLOW_PICKER_MAX_ITEMS}-item picker limit"
                ));
            }
            workflows.extend(response.data);
            match response.next_cursor {
                Some(next_cursor) if next_cursor.len() > WORKFLOW_LIST_MAX_CURSOR_BYTES => {
                    break Err(format!(
                        "workflow/list cursor exceeds the {WORKFLOW_LIST_MAX_CURSOR_BYTES}-byte limit"
                    ));
                }
                Some(next_cursor) if Some(&next_cursor) != cursor.as_ref() => {
                    cursor = Some(next_cursor);
                }
                Some(_) => break Err("workflow/list returned a repeated cursor".to_string()),
                None => break Ok(workflows),
            }
        };
        self.chat_widget.on_workflow_list_result(thread_id, result);
    }

    pub(super) async fn start_saved_workflow(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        name: String,
        args: Option<JsonValue>,
    ) {
        self.chat_widget
            .on_workflow_start_requested(thread_id, &name);
        let result = app_server
            .workflow_start(WorkflowStartParams {
                thread_id: thread_id.to_string(),
                name: name.clone(),
                args,
            })
            .await;
        match result {
            Ok(response) => {
                self.chat_widget
                    .on_workflow_start_succeeded(thread_id, name, response.run_id)
            }
            Err(error) => {
                self.chat_widget
                    .on_workflow_start_failed(thread_id, &name, &error.to_string())
            }
        }
    }

    pub(super) fn request_workflow_read(
        &self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        run_id: String,
        revision: u64,
    ) {
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = send_workflow_read(
                request_handle,
                WorkflowReadParams {
                    thread_id: thread_id.to_string(),
                    run_id: run_id.clone(),
                },
            )
            .await
            .map_err(|error| error.to_string());
            app_event_tx.send(crate::app_event::AppEvent::WorkflowReadFinished {
                thread_id,
                run_id,
                revision,
                result,
            });
        });
    }

    pub(super) fn request_workflow_stop(
        &mut self,
        app_server: &AppServerSession,
        thread_id: ThreadId,
        run_id: String,
    ) {
        if !self
            .chat_widget
            .on_workflow_stop_requested(thread_id, &run_id)
        {
            return;
        }

        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = send_workflow_stop(
                request_handle,
                WorkflowStopParams {
                    thread_id: thread_id.to_string(),
                    run_id: run_id.clone(),
                },
            )
            .await
            .map_err(|error| error.to_string());
            app_event_tx.send(crate::app_event::AppEvent::WorkflowStopFinished {
                thread_id,
                run_id,
                result,
            });
        });
    }

    pub(super) fn request_workflow_pause(
        &mut self,
        app_server: &AppServerSession,
        target: WorkflowRunControlTarget,
    ) {
        if !self.chat_widget.on_workflow_pause_requested(&target) {
            return;
        }

        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = send_workflow_pause(
                request_handle,
                WorkflowPauseParams {
                    thread_id: target.thread_id.to_string(),
                    run_id: target.run_id.clone(),
                },
            )
            .await
            .map_err(|error| error.to_string());
            app_event_tx.send(crate::app_event::AppEvent::WorkflowPauseFinished { target, result });
        });
    }

    pub(super) fn request_workflow_resume(
        &mut self,
        app_server: &AppServerSession,
        target: WorkflowRunControlTarget,
    ) {
        if !self.chat_widget.on_workflow_resume_requested(&target) {
            return;
        }

        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = send_workflow_resume(
                request_handle,
                WorkflowResumeParams {
                    thread_id: target.thread_id.to_string(),
                    run_id: target.run_id.clone(),
                },
            )
            .await
            .map_err(|error| error.to_string());
            app_event_tx
                .send(crate::app_event::AppEvent::WorkflowResumeFinished { target, result });
        });
    }

    pub(super) fn request_workflow_agent_control(
        &mut self,
        app_server: &AppServerSession,
        request: WorkflowAgentControlRequest,
    ) {
        if !self
            .chat_widget
            .on_workflow_agent_control_requested(&request)
        {
            return;
        }

        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = send_workflow_agent_control(
                request_handle,
                WorkflowAgentControlParams {
                    thread_id: request.target.thread_id.to_string(),
                    run_id: request.target.run_id.clone(),
                    node_id: request.target.node_id,
                    attempt: request.target.attempt,
                    action: request.action,
                },
            )
            .await
            .map_err(|error| error.to_string());
            app_event_tx
                .send(crate::app_event::AppEvent::WorkflowAgentControlFinished { request, result });
        });
    }

    pub(super) fn request_workflow_save(
        &mut self,
        app_server: &AppServerSession,
        request: WorkflowSaveRequest,
    ) {
        if !self.chat_widget.on_workflow_save_requested(&request) {
            return;
        }

        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = send_workflow_save(request_handle, workflow_save_params(&request))
                .await
                .map_err(|error| error.to_string());
            app_event_tx.send(crate::app_event::AppEvent::WorkflowSaveFinished { request, result });
        });
    }
}

async fn send_workflow_read(
    request_handle: AppServerRequestHandle,
    params: WorkflowReadParams,
) -> Result<WorkflowRunStatus> {
    let expected_run_id = params.run_id.clone();
    let request_id = RequestId::String(format!("tui-workflow-read-{}", Uuid::new_v4()));
    let response: WorkflowReadResponse = request_handle
        .request_typed(ClientRequest::WorkflowRead { request_id, params })
        .await
        .wrap_err("workflow/read failed in TUI")?;
    if response.run_id != expected_run_id {
        color_eyre::eyre::bail!("workflow/read returned a mismatched run id");
    }
    Ok(response.status)
}

async fn send_workflow_stop(
    request_handle: AppServerRequestHandle,
    params: WorkflowStopParams,
) -> Result<WorkflowStopDisposition> {
    let request_id = RequestId::String(format!("tui-workflow-stop-{}", Uuid::new_v4()));
    let response: WorkflowStopResponse = request_handle
        .request_typed(ClientRequest::WorkflowStop { request_id, params })
        .await
        .wrap_err("workflow/stop failed in TUI")?;
    Ok(response.disposition)
}

async fn send_workflow_pause(
    request_handle: AppServerRequestHandle,
    params: WorkflowPauseParams,
) -> Result<WorkflowPauseDisposition> {
    let request_id = RequestId::String(format!("tui-workflow-pause-{}", Uuid::new_v4()));
    let response: WorkflowPauseResponse = request_handle
        .request_typed(ClientRequest::WorkflowPause { request_id, params })
        .await
        .wrap_err("workflow/pause failed in TUI")?;
    Ok(response.disposition)
}

async fn send_workflow_resume(
    request_handle: AppServerRequestHandle,
    params: WorkflowResumeParams,
) -> Result<String> {
    let request_id = RequestId::String(format!("tui-workflow-resume-{}", Uuid::new_v4()));
    let response: WorkflowResumeResponse = request_handle
        .request_typed(ClientRequest::WorkflowResume { request_id, params })
        .await
        .wrap_err("workflow/resume failed in TUI")?;
    Ok(response.run_id)
}

async fn send_workflow_agent_control(
    request_handle: AppServerRequestHandle,
    params: WorkflowAgentControlParams,
) -> Result<WorkflowAgentControlResponse> {
    let request_id = RequestId::String(format!("tui-workflow-agent-control-{}", Uuid::new_v4()));
    request_handle
        .request_typed(ClientRequest::WorkflowAgentControl { request_id, params })
        .await
        .wrap_err("workflow/agent/control failed in TUI")
}

fn workflow_save_params(request: &WorkflowSaveRequest) -> WorkflowSaveParams {
    WorkflowSaveParams {
        thread_id: request.target.run.thread_id.to_string(),
        run_id: request.target.run.run_id.clone(),
        name: request.target.run.name.clone(),
        scope: request.target.scope,
        overwrite: match request.intent {
            WorkflowSaveIntent::Create => false,
            WorkflowSaveIntent::Overwrite => true,
        },
    }
}

async fn send_workflow_save(
    request_handle: AppServerRequestHandle,
    params: WorkflowSaveParams,
) -> Result<WorkflowSaveDisposition> {
    let request_id = RequestId::String(format!("tui-workflow-save-{}", Uuid::new_v4()));
    let response: WorkflowSaveResponse = request_handle
        .request_typed(ClientRequest::WorkflowSave { request_id, params })
        .await
        .wrap_err("workflow/save failed in TUI")?;
    Ok(response.disposition)
}

#[cfg(test)]
#[path = "workflow_actions_tests.rs"]
mod tests;
