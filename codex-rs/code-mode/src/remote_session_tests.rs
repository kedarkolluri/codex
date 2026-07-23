use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use codex_code_mode_protocol::CodeModeSessionProvider;
use codex_code_mode_protocol::ExecuteOutputPolicy;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::SAVED_WORKFLOW_OUTPUT_POLICY_UNAVAILABLE;
use pretty_assertions::assert_eq;

use super::OwnedProcessHost;
use super::ProcessOwnedCodeModeSession;
use super::ProcessOwnedCodeModeSessionProvider;
use super::SessionState;
use super::resolve_host_program;
use crate::NoopCodeModeSessionDelegate;

#[test]
fn provider_reuses_its_live_process_host() {
    let provider = ProcessOwnedCodeModeSessionProvider::default();

    let first = provider.process_host().expect("owned process host");
    let second = provider.process_host().expect("owned process host");

    assert!(Arc::ptr_eq(&first, &second));
}

#[test]
fn host_program_override_takes_precedence() {
    assert_eq!(
        resolve_host_program(
            Some("custom-code-mode-host".into()),
            Ok(PathBuf::from("/opt/codex/bin/codex")),
        ),
        PathBuf::from("custom-code-mode-host")
    );
}

#[test]
fn host_program_is_next_to_the_main_executable_even_when_missing() {
    let executable_name = if cfg!(windows) {
        "codex-code-mode-host.exe"
    } else {
        "codex-code-mode-host"
    };

    assert_eq!(
        resolve_host_program(
            /*override_path*/ None,
            Ok(PathBuf::from("/opt/codex/bin/codex")),
        ),
        PathBuf::from("/opt/codex/bin").join(executable_name)
    );
}

#[test]
fn host_program_falls_back_to_its_name_when_main_executable_is_unknown() {
    let executable_name = if cfg!(windows) {
        "codex-code-mode-host.exe"
    } else {
        "codex-code-mode-host"
    };

    assert_eq!(
        resolve_host_program(
            /*override_path*/ None,
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "missing executable"
            )),
        ),
        PathBuf::from(executable_name)
    );
}

#[tokio::test]
async fn provider_falls_back_to_in_process_session_when_host_is_missing() {
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(
        "codex-code-mode-host-does-not-exist".into(),
    );

    let session = provider
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await
        .expect("missing host should fall back to an in-process session");
    let error = Arc::clone(&session)
        .execute_bound(ExecuteRequest {
            tool_call_id: "call-saved".to_string(),
            enabled_tools: Vec::new(),
            source: "text('unreachable')".to_string(),
            output_policy: ExecuteOutputPolicy::SavedWorkflow,
            yield_time_ms: None,
            max_output_tokens: None,
        })
        .await
        .err()
        .expect("in-process fallback should reject saved workflow execution");

    assert_eq!(error, SAVED_WORKFLOW_OUTPUT_POLICY_UNAVAILABLE);

    let response = session
        .execute(ExecuteRequest {
            tool_call_id: "call-1".to_string(),
            enabled_tools: Vec::new(),
            source: "text('fallback')".to_string(),
            output_policy: ExecuteOutputPolicy::Ordinary,
            yield_time_ms: None,
            max_output_tokens: None,
        })
        .await
        .expect("execute fallback session")
        .initial_response()
        .await
        .expect("read fallback response");

    assert_eq!(
        response,
        RuntimeResponse::Result {
            cell_id: codex_code_mode_protocol::CellId::new("1".to_string()),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "fallback".to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn shutdown_before_open_does_not_spawn_the_host() {
    let session = ProcessOwnedCodeModeSession::new();

    session.shutdown().await.expect("shutdown session");
    let error = session
        .execute(codex_code_mode_protocol::ExecuteRequest {
            tool_call_id: "call-1".to_string(),
            enabled_tools: Vec::new(),
            source: "text('unreachable')".to_string(),
            output_policy: ExecuteOutputPolicy::Ordinary,
            yield_time_ms: None,
            max_output_tokens: None,
        })
        .await
        .err()
        .expect("shutdown session should reject execution");

    assert_eq!(error, "code mode session is shutting down");
}

#[tokio::test]
async fn saved_execute_attempts_to_open_the_process_host() {
    let process_host = Arc::new(OwnedProcessHost::new("host-must-not-start".into()));
    let session = ProcessOwnedCodeModeSession::with_process_host(
        Arc::new(NoopCodeModeSessionDelegate),
        Arc::clone(&process_host),
    );
    let error = session
        .execute(ExecuteRequest {
            tool_call_id: "call-saved".to_string(),
            enabled_tools: Vec::new(),
            source: "text('unreachable')".to_string(),
            output_policy: ExecuteOutputPolicy::SavedWorkflow,
            yield_time_ms: None,
            max_output_tokens: None,
        })
        .await
        .err()
        .expect("saved execute should attempt to open the process host");

    assert!(
        error.starts_with("failed to spawn code-mode host host-must-not-start:"),
        "unexpected spawn error: {error}"
    );
    assert_eq!(process_host.next_session_id.load(Ordering::Relaxed), 2);
    assert!(matches!(
        *session
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        SessionState::New
    ));
}
