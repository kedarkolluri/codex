use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::ExecuteOutputPolicy;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_code_mode::RuntimeResponse;
use tempfile::TempDir;

const FIXTURE_BINARY: &str = "codex-code-mode-host-test-fixture";
const FIXTURE_PROCESS_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
pub(super) const FIXTURE_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) fn copy_fixture(mode: &str) -> (TempDir, PathBuf) {
    let source = codex_utils_cargo_bin::cargo_bin(FIXTURE_BINARY).expect("fixture binary");
    #[cfg(unix)]
    let source_permissions = fs::metadata(&source)
        .expect("fixture binary metadata")
        .permissions();
    let temp_dir = tempfile::tempdir().expect("fixture temp dir");
    let destination = temp_dir
        .path()
        .join(format!("{mode}{}", std::env::consts::EXE_SUFFIX));
    fs::copy(&source, &destination).expect("copy fixture binary");
    #[cfg(unix)]
    fs::set_permissions(&destination, source_permissions).expect("preserve fixture permissions");
    #[cfg(windows)]
    {
        let mut destination_permissions = fs::metadata(&destination)
            .expect("copied fixture binary metadata")
            .permissions();
        destination_permissions.set_readonly(false);
        fs::set_permissions(&destination, destination_permissions)
            .expect("clear copied fixture read-only attribute");
    }
    (temp_dir, destination)
}

pub(super) async fn remove_fixture_executable(host_program: &Path) {
    tokio::time::timeout(FIXTURE_OPERATION_TIMEOUT, async {
        loop {
            match fs::remove_file(host_program) {
                Ok(()) => break,
                Err(error) if error.kind() == ErrorKind::NotFound => break,
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::PermissionDenied | ErrorKind::WouldBlock
                    ) =>
                {
                    tokio::time::sleep(FIXTURE_PROCESS_EXIT_POLL_INTERVAL).await;
                }
                Err(error) => panic!(
                    "failed to remove fixture executable `{}`: {error}",
                    host_program.display()
                ),
            }
        }
    })
    .await
    .expect("fixture executable remained locked after host shutdown");
}

pub(super) async fn create_fixture_session(
    provider: &ProcessOwnedCodeModeSessionProvider,
    delegate: Arc<dyn CodeModeSessionDelegate>,
) -> Arc<dyn CodeModeSession> {
    tokio::time::timeout(FIXTURE_OPERATION_TIMEOUT, provider.create_session(delegate))
        .await
        .expect("fixture session creation timed out")
        .expect("open fixture session")
}

fn execute_request(output_policy: ExecuteOutputPolicy) -> ExecuteRequest {
    ExecuteRequest {
        tool_call_id: "fixture-call".to_string(),
        enabled_tools: Vec::new(),
        source: "text('fixture request');".to_string(),
        output_policy,
        yield_time_ms: None,
        max_output_tokens: None,
    }
}

pub(super) async fn execute_error(
    session: &Arc<dyn CodeModeSession>,
    output_policy: ExecuteOutputPolicy,
) -> String {
    tokio::time::timeout(
        FIXTURE_OPERATION_TIMEOUT,
        session.execute(execute_request(output_policy)),
    )
    .await
    .expect("fixture execution timed out")
    .err()
    .expect("fixture execution should fail before returning a StartedCell")
}

pub(super) async fn execute_to_initial_response(
    session: &Arc<dyn CodeModeSession>,
    output_policy: ExecuteOutputPolicy,
) -> RuntimeResponse {
    let started_cell = tokio::time::timeout(
        FIXTURE_OPERATION_TIMEOUT,
        session.execute(execute_request(output_policy)),
    )
    .await
    .expect("fixture execution timed out")
    .expect("fixture execution should start");
    tokio::time::timeout(FIXTURE_OPERATION_TIMEOUT, started_cell.initial_response())
        .await
        .expect("fixture initial response timed out")
        .expect("fixture initial response")
}

pub(super) async fn shutdown_fixture_session(session: &Arc<dyn CodeModeSession>) {
    tokio::time::timeout(FIXTURE_OPERATION_TIMEOUT, session.shutdown())
        .await
        .expect("fixture session shutdown timed out")
        .expect("shutdown fixture session");
}

pub(super) fn ordinary_fixture_response(cell_id: &str) -> RuntimeResponse {
    RuntimeResponse::Result {
        cell_id: CellId::new(cell_id.to_string()),
        content_items: vec![FunctionCallOutputContentItem::InputText {
            text: "fixture ordinary".to_string(),
        }],
        error_text: None,
    }
}
