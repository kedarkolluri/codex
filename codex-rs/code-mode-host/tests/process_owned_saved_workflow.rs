#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::sync::Mutex;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::ExecuteOutputPolicy;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::NoopCodeModeSessionDelegate;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::ToolInvocationFuture;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use codex_code_mode::host::WireWorkflowCellId;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

struct RecordingDelegate {
    inner: NoopCodeModeSessionDelegate,
    closed_cells: Mutex<Vec<CellId>>,
}

impl Default for RecordingDelegate {
    fn default() -> Self {
        Self {
            inner: NoopCodeModeSessionDelegate,
            closed_cells: Mutex::new(Vec::new()),
        }
    }
}

impl CodeModeSessionDelegate for RecordingDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        self.inner.invoke_tool(invocation, cancellation_token)
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        self.inner
            .notify(call_id, cell_id, text, cancellation_token)
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.closed_cells
            .lock()
            .expect("closed cells lock")
            .push(cell_id.clone());
    }
}

fn execute_request(source: &str, output_policy: ExecuteOutputPolicy) -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "call-1".to_string(),
        enabled_tools: Vec::new(),
        source: source.to_string(),
        output_policy,
        yield_time_ms: None,
        max_output_tokens: None,
    }
}

#[tokio::test]
async fn process_owned_saved_workflow_runs_and_controls_cells_on_spawned_host() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let delegate = Arc::new(RecordingDelegate::default());
    let session = provider
        .create_session(delegate.clone())
        .await
        .expect("create remote session");
    let completed = session
        .execute(execute_request(
            r#"text("before"); yield_control(); text("after");"#,
            ExecuteOutputPolicy::SavedWorkflow,
        ))
        .await
        .expect("start saved workflow");
    let completed_cell_id = completed.cell_id.clone();
    let completed_identity = WireWorkflowCellId::try_new(completed_cell_id.as_str())
        .expect("client-assigned workflow cell ID");

    assert_eq!(completed_identity.sequence(), 1);
    assert_eq!(
        completed
            .initial_response()
            .await
            .expect("saved workflow initial response"),
        RuntimeResponse::Yielded {
            cell_id: completed_cell_id.clone(),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "before".to_string(),
            }],
        }
    );
    assert_eq!(
        session
            .wait(WaitRequest {
                cell_id: completed_cell_id.clone(),
                yield_time_ms: 60_000,
            })
            .await
            .expect("wait for saved workflow completion"),
        WaitOutcome::LiveCell(RuntimeResponse::Result {
            cell_id: completed_cell_id.clone(),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "after".to_string(),
            }],
            error_text: None,
        })
    );

    let ordinary_cell_id = CellId::new("1".to_string());
    let ordinary_response = session
        .execute(execute_request(
            r#"text("ordinary");"#,
            ExecuteOutputPolicy::Ordinary,
        ))
        .await
        .expect("start ordinary execution")
        .initial_response()
        .await
        .expect("ordinary initial response");
    assert_eq!(
        ordinary_response,
        RuntimeResponse::Result {
            cell_id: ordinary_cell_id.clone(),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "ordinary".to_string(),
            }],
            error_text: None,
        }
    );
    session.shutdown().await.expect("shutdown remote session");

    let mut closed_cells = delegate
        .closed_cells
        .lock()
        .expect("closed cells lock")
        .clone();
    closed_cells.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    let mut expected_closed_cells = vec![completed_cell_id, ordinary_cell_id];
    expected_closed_cells.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    assert_eq!(closed_cells, expected_closed_cells);
}
