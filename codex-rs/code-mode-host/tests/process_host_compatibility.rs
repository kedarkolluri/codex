#![allow(clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::ExecuteOutputPolicy;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::NoopCodeModeSessionDelegate;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::SAVED_WORKFLOW_EXECUTION_FAILED;
use codex_code_mode::SAVED_WORKFLOW_OUTPUT_POLICY_UNAVAILABLE;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

const FIXTURE_BINARY: &str = "codex-code-mode-host-test-fixture";
const NO_SAVED_CAPABILITIES_MODE: &str = "no-saved-capabilities";
const LEGACY_OUTPUT_ONLY_MODE: &str = "legacy-output-only";
const INVALID_WORKFLOW_CAPABILITIES_MODE: &str = "invalid-workflow-capabilities";
const MISMATCHED_WORKFLOW_ID_MODE: &str = "mismatched-workflow-id";
const POST_MISMATCH_MARKER_SUFFIX: &str = ".post-mismatch-client-message";
const POST_MISMATCH_OBSERVER_DONE_SUFFIX: &str = ".post-mismatch-observer-done";
const INVALID_WORKFLOW_CAPABILITIES_ERROR: &str =
    "code-mode host selected an invalid workflow capability set";
const OBSERVER_TIMEOUT: Duration = Duration::from_secs(5);
const OBSERVER_POLL_INTERVAL: Duration = Duration::from_millis(10);

fn copy_fixture(mode: &str) -> (TempDir, PathBuf) {
    let source = codex_utils_cargo_bin::cargo_bin(FIXTURE_BINARY).expect("fixture binary");
    let source_permissions = fs::metadata(&source)
        .expect("fixture binary metadata")
        .permissions();
    let temp_dir = tempfile::tempdir().expect("fixture temp dir");
    let destination = temp_dir
        .path()
        .join(format!("{mode}{}", std::env::consts::EXE_SUFFIX));
    fs::copy(&source, &destination).expect("copy fixture binary");
    fs::set_permissions(&destination, source_permissions).expect("preserve fixture permissions");
    (temp_dir, destination)
}

fn sidecar_path(host_program: &Path, suffix: &str) -> PathBuf {
    let mut sidecar_name = host_program.as_os_str().to_os_string();
    sidecar_name.push(suffix);
    PathBuf::from(sidecar_name)
}

async fn wait_for_observer_done(observer_done: &Path) {
    tokio::time::timeout(OBSERVER_TIMEOUT, async {
        loop {
            if observer_done
                .try_exists()
                .expect("inspect post-mismatch observer sentinel")
            {
                break;
            }
            tokio::time::sleep(OBSERVER_POLL_INTERVAL).await;
        }
    })
    .await
    .expect("post-mismatch observer did not reach client EOF");
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

async fn execute_error(
    session: &Arc<dyn CodeModeSession>,
    output_policy: ExecuteOutputPolicy,
) -> String {
    session
        .execute(execute_request(output_policy))
        .await
        .err()
        .expect("fixture execution should fail before returning a StartedCell")
}

async fn execute_to_initial_response(
    session: &Arc<dyn CodeModeSession>,
    output_policy: ExecuteOutputPolicy,
) -> RuntimeResponse {
    session
        .execute(execute_request(output_policy))
        .await
        .expect("fixture execution should start")
        .initial_response()
        .await
        .expect("fixture initial response")
}

fn ordinary_fixture_response(cell_id: &str) -> RuntimeResponse {
    RuntimeResponse::Result {
        cell_id: CellId::new(cell_id.to_string()),
        content_items: vec![FunctionCallOutputContentItem::InputText {
            text: "fixture ordinary".to_string(),
        }],
        error_text: None,
    }
}

async fn assert_unsupported_valid_host_preserves_ordinary(mode: &str) {
    let (_fixture_dir, host_program) = copy_fixture(mode);
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(host_program);
    let session = provider
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await
        .expect("open unsupported-capability fixture session");

    assert_eq!(
        execute_error(&session, ExecuteOutputPolicy::SavedWorkflow).await,
        SAVED_WORKFLOW_OUTPUT_POLICY_UNAVAILABLE
    );
    assert_eq!(
        execute_to_initial_response(&session, ExecuteOutputPolicy::Ordinary).await,
        ordinary_fixture_response("1")
    );

    session
        .shutdown()
        .await
        .expect("shutdown unsupported-capability session");
}

#[tokio::test]
async fn no_saved_capabilities_host_rejects_saved_without_losing_the_session() {
    assert_unsupported_valid_host_preserves_ordinary(NO_SAVED_CAPABILITIES_MODE).await;
}

#[tokio::test]
async fn legacy_output_only_host_rejects_saved_without_losing_the_session() {
    assert_unsupported_valid_host_preserves_ordinary(LEGACY_OUTPUT_ONLY_MODE).await;
}

#[tokio::test]
async fn invalid_workflow_capabilities_fail_without_in_process_fallback() {
    let (_fixture_dir, host_program) = copy_fixture(INVALID_WORKFLOW_CAPABILITIES_MODE);
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(host_program);

    let error = provider
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await
        .err()
        .expect("invalid workflow capabilities should fail session creation");

    assert_eq!(error, INVALID_WORKFLOW_CAPABILITIES_ERROR);
}

#[tokio::test]
async fn mismatched_workflow_id_recovers_on_a_fresh_spawned_host() {
    let (_fixture_dir, host_program) = copy_fixture(MISMATCHED_WORKFLOW_ID_MODE);
    let post_mismatch_marker = sidecar_path(&host_program, POST_MISMATCH_MARKER_SUFFIX);
    let observer_done = sidecar_path(&host_program, POST_MISMATCH_OBSERVER_DONE_SUFFIX);
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(host_program);
    let session = provider
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await
        .expect("open mismatched-ID fixture session");

    assert_eq!(
        execute_error(&session, ExecuteOutputPolicy::SavedWorkflow).await,
        SAVED_WORKFLOW_EXECUTION_FAILED
    );
    wait_for_observer_done(&observer_done).await;
    assert!(
        !post_mismatch_marker
            .try_exists()
            .expect("inspect post-mismatch marker"),
        "fixture observed another frame after the mismatched workflow ID"
    );
    assert_eq!(
        execute_to_initial_response(&session, ExecuteOutputPolicy::Ordinary).await,
        ordinary_fixture_response("g2:1")
    );

    session
        .shutdown()
        .await
        .expect("shutdown recovered session");
    wait_for_observer_done(&observer_done).await;
    assert!(
        !post_mismatch_marker
            .try_exists()
            .expect("inspect post-recovery mismatch marker"),
        "fixture observed another frame after the mismatched workflow ID"
    );
}
