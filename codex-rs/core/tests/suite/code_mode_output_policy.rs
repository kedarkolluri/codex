use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use codex_code_mode::CellId;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeSessionProviderFuture;
use codex_code_mode::CodeModeSessionResultFuture;
use codex_code_mode::ExecuteOutputPolicy;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::StartedCell;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use tokio::sync::oneshot;

#[derive(Default)]
struct RecordingSessionProvider {
    requests: Arc<Mutex<Vec<ExecuteRequest>>>,
}

impl CodeModeSessionProvider for RecordingSessionProvider {
    fn create_session<'a>(
        &'a self,
        _delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> CodeModeSessionProviderFuture<'a> {
        let session: Arc<dyn CodeModeSession> = Arc::new(RecordingSession {
            requests: Arc::clone(&self.requests),
        });
        Box::pin(async move { Ok(session) })
    }
}

struct RecordingSession {
    requests: Arc<Mutex<Vec<ExecuteRequest>>>,
}

impl CodeModeSession for RecordingSession {
    fn execute<'a>(
        &'a self,
        request: ExecuteRequest,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        self.requests.lock().expect("requests lock").push(request);
        let cell_id = CellId::new("recorded-cell".to_string());
        let (response_tx, response_rx) = oneshot::channel();
        let _ = response_tx.send(RuntimeResponse::Result {
            cell_id: cell_id.clone(),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "recorded output".to_string(),
            }],
            error_text: None,
        });
        Box::pin(async move { Ok(StartedCell::new(cell_id, response_rx)) })
    }

    fn wait<'a>(&'a self, _request: WaitRequest) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async { Err("recording session does not support wait".to_string()) })
    }

    fn terminate<'a>(&'a self, _cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async { Err("recording session does not support terminate".to_string()) })
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn model_exec_submits_an_ordinary_runtime_request() -> Result<()> {
    let server = responses::start_mock_server().await;
    let session_provider = Arc::new(RecordingSessionProvider::default());
    let mut builder = test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_code_mode_session_provider(session_provider.clone())
        .with_config(|config| {
            config
                .features
                .enable(Feature::CodeMode)
                .expect("code mode should be enabled");
        });
    let test = builder.build_with_auto_env(&server).await?;
    let first_response = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_custom_tool_call("call-1", "exec", "text('model boundary');"),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let follow_up_response = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn("Run model-facing exec").await?;

    let _ = first_response.single_request();
    let _ = follow_up_response.single_request();
    let requests = session_provider
        .requests
        .lock()
        .expect("requests lock")
        .clone();
    let [request] = requests.as_slice() else {
        panic!("expected exactly one code-mode request, got {requests:?}");
    };
    assert_eq!(request.output_policy, ExecuteOutputPolicy::Ordinary);

    Ok(())
}
