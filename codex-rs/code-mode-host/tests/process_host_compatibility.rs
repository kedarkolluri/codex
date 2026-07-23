#![allow(clippy::expect_used)]

use std::sync::Arc;

use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::ExecuteOutputPolicy;
use codex_code_mode::NoopCodeModeSessionDelegate;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_code_mode::SAVED_WORKFLOW_OUTPUT_POLICY_UNAVAILABLE;
use pretty_assertions::assert_eq;

mod process_host_fixture;

use process_host_fixture::FIXTURE_OPERATION_TIMEOUT;
use process_host_fixture::copy_fixture;
use process_host_fixture::create_fixture_session;
use process_host_fixture::execute_error;
use process_host_fixture::execute_to_initial_response;
use process_host_fixture::ordinary_fixture_response;
use process_host_fixture::remove_fixture_executable;
use process_host_fixture::shutdown_fixture_session;

const NO_SAVED_CAPABILITIES_MODE: &str = "no-saved-capabilities";
const LEGACY_OUTPUT_ONLY_MODE: &str = "legacy-output-only";
const INVALID_WORKFLOW_CAPABILITIES_MODE: &str = "invalid-workflow-capabilities";
const INVALID_WORKFLOW_CAPABILITIES_ERROR: &str =
    "code-mode host selected an invalid workflow capability set";

async fn assert_unsupported_valid_host_preserves_ordinary(mode: &str) {
    let (fixture_dir, host_program) = copy_fixture(mode);
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(host_program.clone());
    let session = create_fixture_session(&provider, Arc::new(NoopCodeModeSessionDelegate)).await;

    assert_eq!(
        execute_error(&session, ExecuteOutputPolicy::SavedWorkflow).await,
        SAVED_WORKFLOW_OUTPUT_POLICY_UNAVAILABLE
    );
    assert_eq!(
        execute_to_initial_response(&session, ExecuteOutputPolicy::Ordinary).await,
        ordinary_fixture_response("1")
    );

    shutdown_fixture_session(&session).await;
    drop(session);
    drop(provider);
    remove_fixture_executable(&host_program).await;
    drop(fixture_dir);
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
    let (fixture_dir, host_program) = copy_fixture(INVALID_WORKFLOW_CAPABILITIES_MODE);
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(host_program.clone());

    let error = tokio::time::timeout(
        FIXTURE_OPERATION_TIMEOUT,
        provider.create_session(Arc::new(NoopCodeModeSessionDelegate)),
    )
    .await
    .expect("invalid-capability fixture session creation timed out")
    .err()
    .expect("invalid workflow capabilities should fail session creation");

    assert_eq!(error, INVALID_WORKFLOW_CAPABILITIES_ERROR);
    drop(provider);
    remove_fixture_executable(&host_program).await;
    drop(fixture_dir);
}
