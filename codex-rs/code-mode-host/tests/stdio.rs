#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use codex_code_mode::AgentCallOpts;
use codex_code_mode::AgentSpawnFuture;
use codex_code_mode::AgentSpawnOutcome;
use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeToolKind;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::ToolDefinition;
use codex_code_mode::ToolInvocationFuture;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use codex_code_mode::WorkflowBudgetSnapshot;
use codex_code_mode::WorkflowBudgetSnapshotFuture;
use codex_code_mode::WorkflowHostCompletion;
use codex_code_mode::WorkflowHostProgress;
use codex_code_mode::host::MAX_FRAME_BYTES;
use codex_protocol::ToolName;
use codex_protocol::protocol::WorkflowEvent;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use serde_json::json;
use tokio::sync::Barrier;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct RecordingDelegate {
    invocations: Mutex<Vec<CodeModeNestedToolCall>>,
    notifications: Mutex<Vec<(String, CellId, String)>>,
    closed_cells: Mutex<Vec<CellId>>,
}

#[derive(Debug, Eq, PartialEq)]
enum CallbackEvent {
    Started(String),
    Cancelled(String),
    CellClosed(CellId),
}

struct CancellationDelegate {
    events_tx: mpsc::UnboundedSender<CallbackEvent>,
    fast_tool_release: Semaphore,
    slow_tool_started: Semaphore,
    hold_slow_cleanup: AtomicBool,
    slow_cleanup_release: Semaphore,
}

struct OversizedResultDelegate;

impl CodeModeSessionDelegate for OversizedResultDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async { Ok(json!("x".repeat(MAX_FRAME_BYTES))) })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

impl CancellationDelegate {
    fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<CallbackEvent>) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                events_tx,
                fast_tool_release: Semaphore::new(/*permits*/ 0),
                slow_tool_started: Semaphore::new(/*permits*/ 0),
                hold_slow_cleanup: AtomicBool::new(false),
                slow_cleanup_release: Semaphore::new(/*permits*/ 0),
            }),
            events_rx,
        )
    }

    #[cfg(unix)]
    fn hold_slow_cleanup(&self) {
        self.hold_slow_cleanup.store(true, Ordering::Release);
    }

    #[cfg(unix)]
    fn release_slow_cleanup(&self) {
        self.slow_cleanup_release.add_permits(1);
    }
}

impl CodeModeSessionDelegate for CancellationDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            let tool_name = invocation.tool_name.name.clone();
            if tool_name == "tool_call_barrier" {
                let permit = self
                    .slow_tool_started
                    .acquire()
                    .await
                    .map_err(|_| "slow tool barrier closed".to_string())?;
                permit.forget();
                return Ok(json!({ "tool": tool_name }));
            }
            let _ = self
                .events_tx
                .send(CallbackEvent::Started(tool_name.clone()));
            if tool_name == "tool_call_slow" {
                self.slow_tool_started.add_permits(1);
                cancellation_token.cancelled().await;
                let _ = self.events_tx.send(CallbackEvent::Cancelled(tool_name));
                if self.hold_slow_cleanup.load(Ordering::Acquire) {
                    let permit = self
                        .slow_cleanup_release
                        .acquire()
                        .await
                        .map_err(|_| "slow tool cleanup release closed".to_string())?;
                    permit.forget();
                }
                return Err("slow tool cancelled".to_string());
            }
            let permit = self
                .fast_tool_release
                .acquire()
                .await
                .map_err(|_| "fast tool release closed".to_string())?;
            permit.forget();
            Ok(json!({ "tool": tool_name }))
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        let _ = self
            .events_tx
            .send(CallbackEvent::CellClosed(cell_id.clone()));
    }
}

impl CodeModeSessionDelegate for RecordingDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        self.invocations
            .lock()
            .expect("invocations lock")
            .push(invocation);
        Box::pin(async { Ok(json!({ "value": "output" })) })
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        self.notifications
            .lock()
            .expect("notifications lock")
            .push((call_id, cell_id, text));
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.closed_cells
            .lock()
            .expect("closed cells lock")
            .push(cell_id.clone());
    }
}

fn cell_id(value: &str) -> CellId {
    CellId::new(value.to_string())
}

fn execute_request(source: &str) -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "call-1".to_string(),
        enabled_tools: Vec::new(),
        source: source.to_string(),
        yield_time_ms: None,
        max_output_tokens: None,
        workflow: false,
        args: None,
        run_id: None,
        replay_entries: Vec::new(),
        workflow_budget: None,
    }
}

async fn execute(session: &Arc<dyn CodeModeSession>, request: ExecuteRequest) -> RuntimeResponse {
    session
        .execute(request)
        .await
        .expect("start execution")
        .initial_response()
        .await
        .expect("initial response")
}

async fn execute_to_terminal(
    session: &Arc<dyn CodeModeSession>,
    request: ExecuteRequest,
) -> RuntimeResponse {
    let started = session.execute(request).await.expect("start execution");
    let mut response = started.initial_response().await.expect("initial response");
    loop {
        match response {
            RuntimeResponse::Yielded { cell_id, .. } => {
                response = match session
                    .wait(WaitRequest {
                        cell_id,
                        yield_time_ms: 60_000,
                    })
                    .await
                    .expect("wait for terminal response")
                {
                    WaitOutcome::LiveCell(response) | WaitOutcome::MissingCell(response) => {
                        response
                    }
                };
            }
            response => return response,
        }
    }
}

async fn next_callback_event(
    events_rx: &mut mpsc::UnboundedReceiver<CallbackEvent>,
) -> CallbackEvent {
    tokio::time::timeout(Duration::from_secs(5), events_rx.recv())
        .await
        .expect("callback event timeout")
        .expect("callback event stream closed")
}

#[tokio::test]
async fn remote_session_persists_values_forwards_delegates_and_controls_cells() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let delegate = Arc::new(RecordingDelegate::default());
    let session = provider
        .create_session(delegate.clone())
        .await
        .expect("create remote session");

    assert_eq!(
        execute(&session, execute_request(r#"store("key", "persisted");"#),).await,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
            error_text: None,
        }
    );

    let mut callback_request = execute_request(
        r#"
const result = await tools.echo({ value: String(load("key")) });
notify("notice");
text(result.value);
"#,
    );
    callback_request.tool_call_id = "call-2".to_string();
    callback_request.enabled_tools = vec![ToolDefinition {
        name: "echo".to_string(),
        tool_name: ToolName::plain("echo"),
        description: String::new(),
        kind: CodeModeToolKind::Function,
        input_schema: None,
        output_schema: None,
    }];
    assert_eq!(
        execute(&session, callback_request).await,
        RuntimeResponse::Result {
            cell_id: cell_id("2"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "output".to_string(),
            }],
            error_text: None,
        }
    );
    assert_eq!(
        *delegate.invocations.lock().expect("invocations lock"),
        vec![CodeModeNestedToolCall {
            cell_id: cell_id("2"),
            runtime_tool_call_id: "tool-1".to_string(),
            tool_name: ToolName::plain("echo"),
            tool_kind: CodeModeToolKind::Function,
            input: Some(json!({ "value": "persisted" })),
        }]
    );
    assert_eq!(
        *delegate.notifications.lock().expect("notifications lock"),
        vec![("call-2".to_string(), cell_id("2"), "notice".to_string())]
    );

    let mut pending_request = execute_request("await new Promise(() => {});");
    pending_request.tool_call_id = "call-3".to_string();
    pending_request.yield_time_ms = Some(1);
    assert_eq!(
        execute(&session, pending_request).await,
        RuntimeResponse::Yielded {
            cell_id: cell_id("3"),
            content_items: Vec::new(),
        }
    );
    assert_eq!(
        session
            .wait(WaitRequest {
                cell_id: cell_id("3"),
                yield_time_ms: 1,
            })
            .await
            .expect("wait for cell"),
        WaitOutcome::LiveCell(RuntimeResponse::Yielded {
            cell_id: cell_id("3"),
            content_items: Vec::new(),
        })
    );
    assert_eq!(
        session
            .terminate(cell_id("3"))
            .await
            .expect("terminate cell"),
        WaitOutcome::LiveCell(RuntimeResponse::Terminated {
            cell_id: cell_id("3"),
            content_items: Vec::new(),
        })
    );

    session.shutdown().await.expect("shutdown remote session");
    assert_eq!(
        *delegate.closed_cells.lock().expect("closed cells lock"),
        vec![cell_id("1"), cell_id("2"), cell_id("3")]
    );
}

#[tokio::test]
async fn dropping_long_wait_releases_observer_before_next_wait() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let session = provider
        .create_session(Arc::new(RecordingDelegate::default()))
        .await
        .expect("create remote session");
    let mut request = execute_request("await new Promise(() => {});");
    request.yield_time_ms = Some(1);
    let started = session.execute(request).await.expect("start execution");
    let running_cell_id = started.cell_id.clone();
    assert_eq!(
        started.initial_response().await.expect("initial response"),
        RuntimeResponse::Yielded {
            cell_id: running_cell_id.clone(),
            content_items: Vec::new(),
        }
    );

    let wait_session = Arc::clone(&session);
    let wait_cell_id = running_cell_id.clone();
    let first_wait = tokio::spawn(async move {
        wait_session
            .wait(WaitRequest {
                cell_id: wait_cell_id,
                yield_time_ms: 60_000,
            })
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    first_wait.abort();
    let _ = first_wait.await;

    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(2),
            session.wait(WaitRequest {
                cell_id: running_cell_id.clone(),
                yield_time_ms: 1,
            })
        )
        .await
        .expect("second wait timeout")
        .expect("second wait"),
        WaitOutcome::LiveCell(RuntimeResponse::Yielded {
            cell_id: running_cell_id.clone(),
            content_items: Vec::new(),
        })
    );
    session
        .terminate(running_cell_id)
        .await
        .expect("terminate cell");
    session.shutdown().await.expect("shutdown remote session");
}

#[tokio::test]
async fn unawaited_slow_tool_is_cancelled_after_parallel_tools_complete() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let (delegate, mut events_rx) = CancellationDelegate::new();
    let session = provider
        .create_session(delegate.clone())
        .await
        .expect("create remote session");
    let mut request = execute_request(
        r#"
await (async () => {
text("hello world");
yield_control();
await Promise.all([
    tools.tool_call_a({}),
    tools.tool_call_b({}),
]);
text("hello");
tools.tool_call_slow({});
await tools.tool_call_barrier({});
return;
})();
"#,
    );
    request.enabled_tools = [
        "tool_call_a",
        "tool_call_b",
        "tool_call_slow",
        "tool_call_barrier",
    ]
    .into_iter()
    .map(|name| ToolDefinition {
        name: name.to_string(),
        tool_name: ToolName::plain(name),
        description: String::new(),
        kind: CodeModeToolKind::Function,
        input_schema: None,
        output_schema: None,
    })
    .collect();

    let started = session.execute(request).await.expect("start execution");
    let running_cell_id = started.cell_id.clone();
    assert_eq!(
        started.initial_response().await.expect("initial response"),
        RuntimeResponse::Yielded {
            cell_id: running_cell_id.clone(),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "hello world".to_string(),
            }],
        }
    );

    let wait_session = Arc::clone(&session);
    let wait_cell_id = running_cell_id.clone();
    let wait_task = tokio::spawn(async move {
        wait_session
            .wait(WaitRequest {
                cell_id: wait_cell_id,
                yield_time_ms: 60_000,
            })
            .await
    });

    let mut parallel_tools = vec![
        next_callback_event(&mut events_rx).await,
        next_callback_event(&mut events_rx).await,
    ];
    parallel_tools.sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));
    assert_eq!(
        parallel_tools,
        vec![
            CallbackEvent::Started("tool_call_a".to_string()),
            CallbackEvent::Started("tool_call_b".to_string()),
        ]
    );
    delegate.fast_tool_release.add_permits(2);

    assert_eq!(
        next_callback_event(&mut events_rx).await,
        CallbackEvent::Started("tool_call_slow".to_string())
    );
    let mut closure_events = vec![
        next_callback_event(&mut events_rx).await,
        next_callback_event(&mut events_rx).await,
    ];
    closure_events.sort_by(|left, right| format!("{left:?}").cmp(&format!("{right:?}")));
    assert_eq!(
        closure_events,
        vec![
            CallbackEvent::Cancelled("tool_call_slow".to_string()),
            CallbackEvent::CellClosed(running_cell_id.clone()),
        ]
    );
    assert_eq!(
        wait_task
            .await
            .expect("wait task")
            .expect("wait for terminal response"),
        WaitOutcome::LiveCell(RuntimeResponse::Result {
            cell_id: running_cell_id,
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "hello".to_string(),
            }],
            error_text: None,
        })
    );
    session.shutdown().await.expect("shutdown remote session");
}

#[tokio::test]
async fn oversized_execute_request_does_not_close_the_shared_host() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let session = provider
        .create_session(Arc::new(RecordingDelegate::default()))
        .await
        .expect("create remote session");
    let error = session
        .execute(execute_request(&"x".repeat(MAX_FRAME_BYTES)))
        .await
        .err()
        .expect("oversized execute should fail");
    assert!(
        error.contains("IPC frame limit"),
        "unexpected error: {error}"
    );

    assert_eq!(
        execute(&session, execute_request(r#"text("still alive");"#)).await,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "still alive".to_string(),
            }],
            error_text: None,
        }
    );
    session.shutdown().await.expect("shutdown remote session");
}

#[tokio::test]
async fn oversized_delegate_payloads_fail_only_the_tool_call() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let session = provider
        .create_session(Arc::new(OversizedResultDelegate))
        .await
        .expect("create remote session");
    let tool = |name: &str| ToolDefinition {
        name: name.to_string(),
        tool_name: ToolName::plain(name),
        description: String::new(),
        kind: CodeModeToolKind::Function,
        input_schema: None,
        output_schema: None,
    };

    let mut oversized_argument = execute_request(&format!(
        r#"
try {{
    await tools.big_argument({{ value: "x".repeat({MAX_FRAME_BYTES}) }});
}} catch (_) {{
    text("argument rejected");
}}
"#
    ));
    oversized_argument.enabled_tools = vec![tool("big_argument")];
    oversized_argument.yield_time_ms = Some(60_000);
    assert_eq!(
        execute_to_terminal(&session, oversized_argument).await,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "argument rejected".to_string(),
            }],
            error_text: None,
        }
    );

    let mut oversized_result = execute_request(
        r#"
try {
    await tools.big_result({});
} catch (_) {
    text("result rejected");
}
"#,
    );
    oversized_result.enabled_tools = vec![tool("big_result")];
    oversized_result.yield_time_ms = Some(60_000);
    assert_eq!(
        execute_to_terminal(&session, oversized_result).await,
        RuntimeResponse::Result {
            cell_id: cell_id("2"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "result rejected".to_string(),
            }],
            error_text: None,
        }
    );

    assert_eq!(
        execute(&session, execute_request(r#"text("still alive");"#)).await,
        RuntimeResponse::Result {
            cell_id: cell_id("3"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "still alive".to_string(),
            }],
            error_text: None,
        }
    );
    session.shutdown().await.expect("shutdown remote session");
}

#[tokio::test]
async fn oversized_initial_response_does_not_close_the_shared_host() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let session = provider
        .create_session(Arc::new(RecordingDelegate::default()))
        .await
        .expect("create remote session");
    let started = session
        .execute(execute_request(&format!(
            r#"text("x".repeat({MAX_FRAME_BYTES}));"#
        )))
        .await
        .expect("start oversized response");
    let error = started
        .initial_response()
        .await
        .expect_err("oversized initial response should fail");
    assert!(
        error.contains("IPC frame limit"),
        "unexpected error: {error}"
    );

    assert_eq!(
        execute(&session, execute_request(r#"text("still alive");"#)).await,
        RuntimeResponse::Result {
            cell_id: cell_id("2"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "still alive".to_string(),
            }],
            error_text: None,
        }
    );
    session.shutdown().await.expect("shutdown remote session");
}

#[cfg(unix)]
#[tokio::test]
async fn child_process_loss_cleans_up_and_rebuilds_the_shared_host() {
    let host_program =
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary");
    let proxy_dir = tempfile::tempdir().expect("create host proxy directory");
    let proxy_program = proxy_dir.path().join("host-proxy.sh");
    let pid_path = proxy_dir.path().join("host.pid");
    std::fs::write(
        &proxy_program,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > '{}'\nexec '{}'\n",
            pid_path.display(),
            host_program.display()
        ),
    )
    .expect("write host proxy");
    let mut permissions = std::fs::metadata(&proxy_program)
        .expect("host proxy metadata")
        .permissions();
    permissions.set_mode(/*mode*/ 0o700);
    std::fs::set_permissions(&proxy_program, permissions).expect("make host proxy executable");

    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(proxy_program);
    let (delegate_a, mut events_a) = CancellationDelegate::new();
    delegate_a.hold_slow_cleanup();
    let delegate_b = Arc::new(RecordingDelegate::default());
    let session_a = provider
        .create_session(delegate_a.clone())
        .await
        .expect("create first remote session");
    let session_b = provider
        .create_session(delegate_b.clone())
        .await
        .expect("create second remote session");

    let mut request_a = execute_request("await tools.tool_call_slow({});");
    request_a.yield_time_ms = Some(1);
    request_a.enabled_tools = vec![ToolDefinition {
        name: "tool_call_slow".to_string(),
        tool_name: ToolName::plain("tool_call_slow"),
        description: String::new(),
        kind: CodeModeToolKind::Function,
        input_schema: None,
        output_schema: None,
    }];
    let started_a = session_a
        .execute(request_a)
        .await
        .expect("start first cell");
    let cell_a = started_a.cell_id.clone();
    assert_eq!(
        started_a
            .initial_response()
            .await
            .expect("first initial response"),
        RuntimeResponse::Yielded {
            cell_id: cell_a.clone(),
            content_items: Vec::new(),
        }
    );
    assert_eq!(
        next_callback_event(&mut events_a).await,
        CallbackEvent::Started("tool_call_slow".to_string())
    );

    let mut request_b = execute_request("await new Promise(() => {});");
    request_b.yield_time_ms = Some(1);
    let started_b = session_b
        .execute(request_b)
        .await
        .expect("start second cell");
    let cell_b = started_b.cell_id.clone();
    assert_eq!(
        started_b
            .initial_response()
            .await
            .expect("second initial response"),
        RuntimeResponse::Yielded {
            cell_id: cell_b.clone(),
            content_items: Vec::new(),
        }
    );

    let wait_a_session = Arc::clone(&session_a);
    let wait_a_cell = cell_a.clone();
    let wait_a = tokio::spawn(async move {
        wait_a_session
            .wait(WaitRequest {
                cell_id: wait_a_cell,
                yield_time_ms: 60_000,
            })
            .await
    });
    let wait_b_session = Arc::clone(&session_b);
    let wait_b_cell = cell_b.clone();
    let wait_b = tokio::spawn(async move {
        wait_b_session
            .wait(WaitRequest {
                cell_id: wait_b_cell,
                yield_time_ms: 60_000,
            })
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let pid = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_path)
                && let Ok(pid) = pid.trim().parse::<u32>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("host pid timeout");
    let kill_status = std::process::Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .status()
        .expect("kill host process");
    assert!(kill_status.success());

    assert!(
        tokio::time::timeout(Duration::from_secs(5), wait_a)
            .await
            .expect("first wait failure timeout")
            .expect("first wait task")
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), wait_b)
            .await
            .expect("second wait failure timeout")
            .expect("second wait task")
            .is_err()
    );
    let closure_events = [
        next_callback_event(&mut events_a).await,
        next_callback_event(&mut events_a).await,
    ];
    assert!(closure_events.contains(&CallbackEvent::Cancelled("tool_call_slow".to_string())));
    assert!(closure_events.contains(&CallbackEvent::CellClosed(cell_a.clone())));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if delegate_b
                .closed_cells
                .lock()
                .expect("closed cells lock")
                .contains(&cell_b)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("unrelated session cleanup timeout");

    assert_eq!(
        execute(&session_b, execute_request(r#"text("replacement");"#)).await,
        RuntimeResponse::Result {
            cell_id: cell_id("g2:1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "replacement".to_string(),
            }],
            error_text: None,
        }
    );
    let stale_error = session_b
        .wait(WaitRequest {
            cell_id: cell_b.clone(),
            yield_time_ms: 1,
        })
        .await
        .expect_err("stale cell should be rejected");
    assert!(stale_error.contains("stale code-mode host generation"));

    tokio::time::timeout(Duration::from_secs(5), session_a.shutdown())
        .await
        .expect("failed session shutdown timeout")
        .expect("shutdown failed session");
    tokio::time::timeout(Duration::from_secs(5), session_b.shutdown())
        .await
        .expect("unrelated session shutdown timeout")
        .expect("shutdown replacement session");

    delegate_a.release_slow_cleanup();
    tokio::task::yield_now().await;
    assert!(matches!(
        events_a.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

/// A process-host `agent()` delegate that lives on the *client* side of the wire. The real host
/// binary runs the V8 isolate; each `agent(prompt, opts?)` inside a workflow travels host ->
/// `RemoteDelegate::spawn_agent` -> `DelegateRequest::SpawnAgent` over stdio -> this delegate, whose
/// [`AgentSpawnOutcome`] travels back as `DelegateResponse::AgentSpawned` and settles the isolate
/// promise. It stands in for core's spawn broker so a code-mode test can exercise the entire wire
/// round-trip without a real core.
struct SpawningDelegate {
    spawn_calls: AtomicUsize,
    barrier: Option<Arc<Barrier>>,
    seen_ordinals: Mutex<Vec<u64>>,
    seen_topology: Mutex<Vec<(u64, Option<u64>, Option<String>, u64)>>,
    seen_schema_ordinals: Mutex<Vec<u64>>,
    workflow_calls: AtomicUsize,
    seen_workflow_names: Mutex<Vec<String>>,
    phases: Mutex<Vec<(CellId, String)>>,
    logs: Mutex<Vec<(CellId, String)>>,
    replayed_agents: Mutex<Vec<(CellId, JsonValue)>>,
    progress: Mutex<Vec<(CellId, WorkflowHostProgress)>>,
}

impl SpawningDelegate {
    fn new() -> Self {
        Self {
            spawn_calls: AtomicUsize::new(0),
            barrier: None,
            seen_ordinals: Mutex::new(Vec::new()),
            seen_topology: Mutex::new(Vec::new()),
            seen_schema_ordinals: Mutex::new(Vec::new()),
            workflow_calls: AtomicUsize::new(0),
            seen_workflow_names: Mutex::new(Vec::new()),
            phases: Mutex::new(Vec::new()),
            logs: Mutex::new(Vec::new()),
            replayed_agents: Mutex::new(Vec::new()),
            progress: Mutex::new(Vec::new()),
        }
    }

    fn with_barrier(n: usize) -> Self {
        let mut delegate = Self::new();
        delegate.barrier = Some(Arc::new(Barrier::new(n)));
        delegate
    }

    fn spawn_calls(&self) -> usize {
        self.spawn_calls.load(Ordering::Acquire)
    }

    fn seen_ordinals(&self) -> Vec<u64> {
        self.seen_ordinals.lock().expect("ordinals lock").clone()
    }

    fn seen_topology(&self) -> Vec<(u64, Option<u64>, Option<String>, u64)> {
        self.seen_topology.lock().expect("topology lock").clone()
    }

    fn seen_schema_ordinals(&self) -> Vec<u64> {
        self.seen_schema_ordinals
            .lock()
            .expect("schema ordinals lock")
            .clone()
    }

    fn workflow_calls(&self) -> usize {
        self.workflow_calls.load(Ordering::Acquire)
    }

    fn seen_workflow_names(&self) -> Vec<String> {
        self.seen_workflow_names
            .lock()
            .expect("workflow names lock")
            .clone()
    }

    fn phases(&self) -> Vec<(CellId, String)> {
        self.phases.lock().expect("phases lock").clone()
    }

    fn logs(&self) -> Vec<(CellId, String)> {
        self.logs.lock().expect("logs lock").clone()
    }

    fn replayed_agents(&self) -> Vec<(CellId, JsonValue)> {
        self.replayed_agents
            .lock()
            .expect("replayed agents lock")
            .clone()
    }

    fn progress(&self) -> Vec<(CellId, WorkflowHostProgress)> {
        self.progress.lock().expect("progress lock").clone()
    }
}

impl CodeModeSessionDelegate for SpawningDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async { Err("unexpected tool call".to_string()) })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn spawn_agent<'a>(
        &'a self,
        _cell_id: CellId,
        node_id: u64,
        parent_node_id: Option<u64>,
        phase: Option<String>,
        prompt: String,
        ordinal: u64,
        opts: AgentCallOpts,
        _cancellation_token: CancellationToken,
    ) -> AgentSpawnFuture<'a> {
        self.spawn_calls.fetch_add(1, Ordering::AcqRel);
        self.seen_ordinals
            .lock()
            .expect("ordinals lock")
            .push(ordinal);
        self.seen_topology.lock().expect("topology lock").push((
            node_id,
            parent_node_id,
            phase,
            ordinal,
        ));
        // Prove `opts.schema` was forwarded across the wire (not dropped by the bridge).
        let has_schema = opts.schema.is_some();
        if has_schema {
            self.seen_schema_ordinals
                .lock()
                .expect("schema ordinals lock")
                .push(ordinal);
        }
        let barrier = self.barrier.clone();
        Box::pin(async move {
            if let Some(barrier) = barrier {
                // Rendezvous: fills only if every concurrent `agent()` is in flight at once, so a
                // bridge that serialized the wire round-trip would deadlock here.
                barrier.wait().await;
            }
            match prompt.as_str() {
                // Death-is-null: resolves the promise to JS `null`.
                "dead" => AgentSpawnOutcome::Failed,
                // Admission-time cap rejection: the isolate throws.
                "cap" => AgentSpawnOutcome::Rejected("AgentCapReached".to_string()),
                // A structured-output call: stands in for core's parsed+validated object, which the
                // bridge must marshal into a real JS object via `json_to_v8`.
                prompt if has_schema => {
                    AgentSpawnOutcome::Completed(json!({ "answer": prompt, "score": 7 }))
                }
                // A schemaless call: a plain JS string.
                prompt => {
                    AgentSpawnOutcome::Completed(JsonValue::String(format!("final:{prompt}")))
                }
            }
        })
    }

    fn spawn_workflow<'a>(
        &'a self,
        _cell_id: CellId,
        name: String,
        args: Option<JsonValue>,
        _cancellation_token: CancellationToken,
    ) -> AgentSpawnFuture<'a> {
        self.workflow_calls.fetch_add(1, Ordering::AcqRel);
        self.seen_workflow_names
            .lock()
            .expect("workflow names lock")
            .push(name.clone());
        Box::pin(async move {
            match name.as_str() {
                // A name that resolves in the registry: the nested run's top-level result, echoing
                // back the `args` payload to prove it crossed the wire.
                "child" => {
                    let echoed = args
                        .as_ref()
                        .and_then(|args| args.get("tag"))
                        .and_then(JsonValue::as_str)
                        .unwrap_or("none")
                        .to_string();
                    AgentSpawnOutcome::Completed(JsonValue::String(format!("child:{echoed}")))
                }
                // A nested run that produced no result: resolves the promise to JS `null`.
                "empty" => AgentSpawnOutcome::Failed,
                // A name that does not resolve in the registry: the isolate throws.
                _ => AgentSpawnOutcome::Rejected("WorkflowNotFound".to_string()),
            }
        })
    }

    fn workflow_budget_snapshot<'a>(
        &'a self,
        _cell_id: CellId,
    ) -> WorkflowBudgetSnapshotFuture<'a> {
        let spent = (self.spawn_calls() as u64).saturating_mul(40).min(100);
        Box::pin(async move {
            Ok(Some(WorkflowBudgetSnapshot {
                total: Some(100),
                spent,
                remaining: Some(100 - spent),
            }))
        })
    }

    fn journal_phase<'a>(&'a self, cell_id: CellId, title: String) -> NotificationFuture<'a> {
        self.phases
            .lock()
            .expect("phases lock")
            .push((cell_id, title));
        Box::pin(async { Ok(()) })
    }

    fn journal_log<'a>(&'a self, cell_id: CellId, message: String) -> NotificationFuture<'a> {
        self.logs
            .lock()
            .expect("logs lock")
            .push((cell_id, message));
        Box::pin(async { Ok(()) })
    }

    fn replay_agent<'a>(
        &'a self,
        cell_id: CellId,
        _node_id: u64,
        _parent_node_id: Option<u64>,
        _phase: Option<String>,
        entry: JsonValue,
    ) -> NotificationFuture<'a> {
        self.replayed_agents
            .lock()
            .expect("replayed agents lock")
            .push((cell_id, entry));
        Box::pin(async { Ok(()) })
    }

    fn workflow_progress<'a>(
        &'a self,
        cell_id: CellId,
        progress: WorkflowHostProgress,
    ) -> NotificationFuture<'a> {
        self.progress
            .lock()
            .expect("progress lock")
            .push((cell_id, progress));
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

struct RejectingReplayDelegate;

const RAW_PHASE_JOURNAL_ERROR: &str =
    "phase journal write failed at /host/private/workflows/run/journal.jsonl";
const RAW_LOG_JOURNAL_ERROR: &str =
    "log journal write failed at /host/private/workflows/run/journal.jsonl";

struct RejectingJournalDelegate;

impl CodeModeSessionDelegate for RejectingJournalDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async { Err("unexpected tool call".to_string()) })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn journal_phase<'a>(&'a self, _cell_id: CellId, _title: String) -> NotificationFuture<'a> {
        Box::pin(async { Err(RAW_PHASE_JOURNAL_ERROR.to_string()) })
    }

    fn journal_log<'a>(&'a self, _cell_id: CellId, _message: String) -> NotificationFuture<'a> {
        Box::pin(async { Err(RAW_LOG_JOURNAL_ERROR.to_string()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

impl CodeModeSessionDelegate for RejectingReplayDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async { Err("unexpected tool call".to_string()) })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn replay_agent<'a>(
        &'a self,
        _cell_id: CellId,
        _node_id: u64,
        _parent_node_id: Option<u64>,
        _phase: Option<String>,
        _entry: JsonValue,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Err("replay accounting unavailable".to_string()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

fn workflow_request(source: &str) -> ExecuteRequest {
    let mut request = execute_request(source);
    // Workflow mode installs the `agent()` global; a large yield window lets the cell run straight to
    // its terminal `Result` (the `agent()` promises resolve promptly).
    request.workflow = true;
    request.run_id = Some("remote-run".to_string());
    request.yield_time_ms = Some(60_000);
    request
}

fn result_texts(response: &RuntimeResponse) -> Vec<String> {
    let RuntimeResponse::Result {
        content_items,
        error_text,
        ..
    } = response
    else {
        panic!("expected terminal Result, got {response:?}");
    };
    assert_eq!(*error_text, None, "workflow errored: {error_text:?}");
    content_items
        .iter()
        .filter_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The process host observes the same initial and post-agent budget values as
/// the in-process host, proving the snapshot refresh crosses stdio before the
/// `agent()` promise settles.
#[tokio::test]
async fn remote_workflow_budget_refreshes_after_agent_over_stdio() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let delegate = Arc::new(SpawningDelegate::new());
    let session = provider
        .create_session(delegate)
        .await
        .expect("create remote session");
    let source = r#"
text(String(budget.spent()));
text(String(budget.remaining()));
await agent("p");
text(String(budget.spent()));
text(String(budget.remaining()));
"#;
    let mut request = workflow_request(source);
    request.workflow_budget = Some(WorkflowBudgetSnapshot {
        total: Some(100),
        spent: 0,
        remaining: Some(100),
    });

    let response = tokio::time::timeout(Duration::from_secs(30), execute(&session, request))
        .await
        .expect("workflow completed before timeout");
    assert_eq!(
        result_texts(&response),
        vec![
            "0".to_string(),
            "100".to_string(),
            "40".to_string(),
            "60".to_string(),
        ]
    );
    session.shutdown().await.expect("shutdown remote session");
}

fn replay_entry() -> JsonValue {
    // These hashes are the workflow-journal v1 KeyInputs key and prompt hash for
    // the default-opts prompt `cached`. Keeping the wire fixture explicit makes
    // these tests independent of a test-only dependency on the journal crate.
    json!({
        "ordinal": 0,
        "key": "blake3:991de39fb811986852f1ef2869b7957b2d33159ea3b6a33dc6154c0631b9c7d9",
        "prompt_hash": "blake3:3f6ad07bbbec251dcafac8f42b026b6dae9a44dd81f9d01d115a3e42891cf234",
        "opts": {
            "model": null,
            "effort": null,
            "agentType": null,
            "isolation": null,
            "schema_hash": null
        },
        "phase": null,
        "label": null,
        "child_thread_id": "prior-thread",
        "rollout_path": "/prior/rollout.jsonl",
        "status": "completed",
        "return": "cached-answer",
        "tokens_spent": 17,
        "completion_seq": 0
    })
}

/// Process-owned workflow execution must carry the replay seed to the host
/// before the isolate starts, then route narration and the replay-accounting
/// callback back to the client-owned delegate. This is the full stdio proof for
/// the callbacks that cannot be represented by an in-process-only handle.
#[tokio::test]
async fn remote_workflow_replays_and_journals_over_the_wire() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let delegate = Arc::new(SpawningDelegate::new());
    let session = provider
        .create_session(delegate.clone())
        .await
        .expect("create remote session");

    let replay_entry = replay_entry();
    let source = r#"
phase("resume");
log("using cache");
text(String(await agent("cached")));
"#;
    let mut request = workflow_request(source);
    request.replay_entries = vec![replay_entry.clone()];
    let response = tokio::time::timeout(Duration::from_secs(30), execute(&session, request))
        .await
        .expect("workflow completed before timeout");

    assert_eq!(result_texts(&response), vec!["cached-answer".to_string()]);
    assert_eq!(delegate.spawn_calls(), 0, "replay must not spawn a child");
    let RuntimeResponse::Result { cell_id: cell, .. } = &response else {
        panic!("expected terminal Result, got {response:?}");
    };
    let cell = cell.clone();
    assert_eq!(
        delegate.phases(),
        vec![(cell.clone(), "resume".to_string())]
    );
    assert_eq!(
        delegate.logs(),
        vec![(cell.clone(), "using cache".to_string())]
    );
    assert_eq!(delegate.replayed_agents(), vec![(cell, replay_entry)]);
    assert!(matches!(
        delegate.progress().as_slice(),
        [
            (_, WorkflowHostProgress::Event { event: phase_begin }),
            (_, WorkflowHostProgress::Event { event: log }),
            (_, WorkflowHostProgress::Event { event: phase_end }),
            (
                _,
                WorkflowHostProgress::Complete {
                    status: WorkflowHostCompletion::Completed
                }
            ),
        ] if matches!(phase_begin.as_ref(), WorkflowEvent::PhaseBegin(_))
            && matches!(log.as_ref(), WorkflowEvent::Log(_))
            && matches!(phase_end.as_ref(), WorkflowEvent::PhaseEnd(_))
    ));

    session.shutdown().await.expect("shutdown remote session");
}

#[tokio::test]
async fn remote_replay_acknowledgement_failure_rejects_cached_result_over_stdio() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let session = provider
        .create_session(Arc::new(RejectingReplayDelegate))
        .await
        .expect("create remote session");
    let mut request = workflow_request(r#"text(String(await agent("cached")));"#);
    request.replay_entries = vec![replay_entry()];

    let response = tokio::time::timeout(Duration::from_secs(30), execute(&session, request))
        .await
        .expect("workflow completed before timeout");
    let RuntimeResponse::Result {
        content_items,
        error_text,
        ..
    } = response
    else {
        panic!("expected terminal Result, got {response:?}");
    };
    assert_eq!(content_items, Vec::new());
    assert_eq!(
        error_text,
        Some("workflow journal is unavailable".to_string())
    );

    session.shutdown().await.expect("shutdown remote session");
}

#[tokio::test]
async fn remote_journal_acknowledgement_failures_stop_workflows_without_leaking_host_details() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let session = provider
        .create_session(Arc::new(RejectingJournalDelegate))
        .await
        .expect("create remote session");

    for (cell, source, raw_error) in [
        (
            "1",
            r#"phase("persist me"); text("must not escape");"#,
            RAW_PHASE_JOURNAL_ERROR,
        ),
        (
            "2",
            r#"log("persist me"); text("must not escape");"#,
            RAW_LOG_JOURNAL_ERROR,
        ),
    ] {
        let response = tokio::time::timeout(
            Duration::from_secs(30),
            execute(&session, workflow_request(source)),
        )
        .await
        .expect("workflow completed before timeout");

        assert!(!format!("{response:?}").contains(raw_error));
        assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: cell_id(cell),
                content_items: Vec::new(),
                error_text: Some("workflow journal is unavailable".to_string()),
            }
        );
    }

    // The first failed acknowledgement must not poison the process-host connection; the second
    // callback and orderly session shutdown travel over the same stdio channel.
    session.shutdown().await.expect("shutdown remote session");
}

#[tokio::test]
async fn remote_workflow_error_reports_terminal_progress_over_the_wire() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let delegate = Arc::new(SpawningDelegate::new());
    let session = provider
        .create_session(delegate.clone())
        .await
        .expect("create remote session");

    let response = tokio::time::timeout(
        Duration::from_secs(30),
        execute(
            &session,
            workflow_request("throw new Error('remote boom');"),
        ),
    )
    .await
    .expect("workflow completed before timeout");
    let RuntimeResponse::Result { error_text, .. } = response else {
        panic!("expected terminal workflow result");
    };
    assert!(
        error_text
            .as_deref()
            .is_some_and(|error| error.contains("remote boom")),
        "unexpected runtime error: {error_text:?}"
    );
    assert!(matches!(
        delegate.progress().last(),
        Some((_, WorkflowHostProgress::Complete {
            status: WorkflowHostCompletion::Errored(error),
        })) if error.contains("remote boom")
    ));

    session.shutdown().await.expect("shutdown remote session");
}

/// The real process-host bridge round-trips all three `AgentSpawnOutcome` variants end-to-end: a
/// schemaless `Completed` (JS string), a `Failed` (JS `null`, no throw), a `Rejected` (the isolate
/// throws the cap message), and a schema `Completed` (a real JS object, property-accessible), and it
/// forwards `opts.schema` across the wire.
#[tokio::test]
async fn remote_agent_spawn_round_trips_all_three_outcomes_over_the_wire() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let delegate = Arc::new(SpawningDelegate::new());
    let session = provider
        .create_session(delegate.clone())
        .await
        .expect("create remote session");

    let source = r#"
const ok = await agent("ok");
const dead = await agent("dead");
let capText;
try {
  await agent("cap");
  capText = "resolved";
} catch (err) {
  capText = "threw:" + String(err);
}
const structured = await agent("s", { schema: { type: "object" } });
text(String(ok));
text(String(dead));
text(capText);
text(typeof structured + ":" + structured.answer + ":" + structured.score);
"#;
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        execute(&session, workflow_request(source)),
    )
    .await
    .expect("workflow completed before timeout");
    let texts = result_texts(&response);
    assert_eq!(texts.len(), 4, "expected four output lines, got {texts:?}");
    assert_eq!(
        texts[0], "final:ok",
        "schemaless Completed marshals to a JS string"
    );
    assert_eq!(
        texts[1], "null",
        "Failed resolves to JS null without throwing"
    );
    assert!(
        texts[2].starts_with("threw:") && texts[2].contains("AgentCapReached"),
        "Rejected must throw the cap message in the isolate, got {:?}",
        texts[2],
    );
    assert_eq!(
        texts[3], "object:s:7",
        "schema Completed marshals to a property-accessible JS object",
    );

    session.shutdown().await.expect("shutdown remote session");

    assert_eq!(delegate.spawn_calls(), 4, "one wire spawn per agent() call");
    let ordinals = delegate.seen_ordinals();
    assert_eq!(ordinals.len(), 4);
    let mut distinct = ordinals.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(
        distinct.len(),
        4,
        "each agent() carried a distinct ordinal, got {ordinals:?}"
    );
    assert_eq!(
        delegate.seen_schema_ordinals().len(),
        1,
        "exactly the schema call forwarded opts.schema across the wire",
    );
}

/// 16 concurrent `agent()` calls in one `parallel()` group each cross the real host bridge with
/// their source-ordered node ID and explicit group parent, then resolve by id independently. The
/// client-side barrier only fills if all 16 wire round-trips are in flight at once, so nothing
/// serializes them, and `parallel()` preserves input order.
#[tokio::test]
async fn remote_sixteen_concurrent_agents_resolve_by_id_without_serialization() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let delegate = Arc::new(SpawningDelegate::with_barrier(16));
    let session = provider
        .create_session(delegate.clone())
        .await
        .expect("create remote session");

    let source = r#"
const results = await parallel(
  Array.from({ length: 16 }, (_, i) => () => agent("a" + i)),
);
text(results.join(","));
"#;
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        execute(&session, workflow_request(source)),
    )
    .await
    .expect("all sixteen agents resolved before timeout");

    let expected = (0..16)
        .map(|i| format!("final:a{i}"))
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(result_texts(&response), vec![expected]);

    session.shutdown().await.expect("shutdown remote session");

    assert_eq!(delegate.spawn_calls(), 16);
    let mut ordinals = delegate.seen_ordinals();
    assert_eq!(ordinals.len(), 16);
    ordinals.sort_unstable();
    ordinals.dedup();
    assert_eq!(
        ordinals.len(),
        16,
        "each of the 16 agent() calls carried a distinct ordinal"
    );
    assert_eq!(
        delegate.seen_topology(),
        (0..16)
            .map(|ordinal| (ordinal + 1, Some(0), None, ordinal))
            .collect::<Vec<_>>(),
        "workflow-v1 must carry source-order node IDs and the parallel group parent",
    );
}

/// The real process-host bridge round-trips nested `workflow(nameOrRef, args)` calls end-to-end over
/// the default `SpawnWorkflow`/`WorkflowSpawned` wire: a `Completed` (JS value, with `args` forwarded
/// across the wire), a `Failed` (JS `null`, no throw), and a `Rejected` (the isolate throws). Before
/// the wire variant existed this resolved to `null` on the process host regardless of outcome.
#[tokio::test]
async fn remote_workflow_spawn_round_trips_outcomes_over_the_wire() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary"),
    );
    let delegate = Arc::new(SpawningDelegate::new());
    let session = provider
        .create_session(delegate.clone())
        .await
        .expect("create remote session");

    let source = r#"
const child = await workflow("child", { tag: "hello" });
const empty = await workflow("empty");
let missingText;
try {
  await workflow("missing");
  missingText = "resolved";
} catch (err) {
  missingText = "threw:" + String(err);
}
text(String(child));
text(String(empty));
text(missingText);
"#;
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        execute(&session, workflow_request(source)),
    )
    .await
    .expect("workflow completed before timeout");
    let texts = result_texts(&response);
    assert_eq!(texts.len(), 3, "expected three output lines, got {texts:?}");
    assert_eq!(
        texts[0], "child:hello",
        "Completed marshals to a JS value and args crossed the wire",
    );
    assert_eq!(
        texts[1], "null",
        "Failed resolves the workflow() promise to JS null without throwing",
    );
    assert!(
        texts[2].starts_with("threw:") && texts[2].contains("WorkflowNotFound"),
        "Rejected must throw the registry-miss message in the isolate, got {:?}",
        texts[2],
    );

    session.shutdown().await.expect("shutdown remote session");

    assert_eq!(
        delegate.workflow_calls(),
        3,
        "one wire spawn per workflow() call"
    );
    assert_eq!(
        delegate.seen_workflow_names(),
        vec![
            "child".to_string(),
            "empty".to_string(),
            "missing".to_string()
        ],
        "each nested workflow name reached the client delegate over the wire",
    );
    assert_eq!(
        delegate.spawn_calls(),
        0,
        "no agent() calls in this workflow"
    );
}
