#![allow(clippy::expect_used)]

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::ExecuteOutputPolicy;
use codex_code_mode::NoopCodeModeSessionDelegate;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_code_mode::SAVED_WORKFLOW_EXECUTION_FAILED;
use codex_code_mode::ToolInvocationFuture;
use pretty_assertions::assert_eq;
use tokio_util::sync::CancellationToken;

mod process_host_fixture;

use process_host_fixture::FIXTURE_OPERATION_TIMEOUT;
use process_host_fixture::copy_fixture;
use process_host_fixture::create_fixture_session;
use process_host_fixture::execute_error;
use process_host_fixture::execute_to_initial_response;
use process_host_fixture::ordinary_fixture_response;
use process_host_fixture::remove_fixture_executable;
use process_host_fixture::shutdown_fixture_session;

const MISMATCHED_WORKFLOW_ID_MODE: &str = "mismatched-workflow-id";
const POST_MISMATCH_MARKER_SUFFIX: &str = ".post-mismatch-client-message";
const POST_MISMATCH_OBSERVER_DONE_SUFFIX: &str = ".post-mismatch-observer-done";
const OBSERVER_TIMEOUT: Duration = Duration::from_secs(5);
const OBSERVER_POLL_INTERVAL: Duration = Duration::from_millis(10);

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

#[tokio::test]
async fn mismatched_workflow_id_recovers_on_a_fresh_spawned_host() {
    let (fixture_dir, host_program) = copy_fixture(MISMATCHED_WORKFLOW_ID_MODE);
    let post_mismatch_marker = sidecar_path(&host_program, POST_MISMATCH_MARKER_SUFFIX);
    let observer_done = sidecar_path(&host_program, POST_MISMATCH_OBSERVER_DONE_SUFFIX);
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(host_program.clone());
    let delegate = Arc::new(RecordingDelegate::default());
    let session = create_fixture_session(&provider, delegate.clone()).await;

    assert_eq!(
        execute_error(&session, ExecuteOutputPolicy::SavedWorkflow).await,
        SAVED_WORKFLOW_EXECUTION_FAILED
    );
    let recovered_response = tokio::time::timeout(FIXTURE_OPERATION_TIMEOUT, async {
        wait_for_observer_done(&observer_done).await;
        assert_eq!(
            *delegate.closed_cells.lock().expect("closed cells lock"),
            Vec::<CellId>::new()
        );
        assert!(
            !post_mismatch_marker
                .try_exists()
                .expect("inspect post-mismatch marker"),
            "fixture observed another frame after the mismatched workflow ID"
        );
        execute_to_initial_response(&session, ExecuteOutputPolicy::Ordinary).await
    })
    .await
    .expect("fixture session recovery timed out");
    assert_eq!(recovered_response, ordinary_fixture_response("g2:1"));

    shutdown_fixture_session(&session).await;
    assert_eq!(
        *delegate.closed_cells.lock().expect("closed cells lock"),
        vec![CellId::new("g2:1".to_string())]
    );
    assert!(
        !post_mismatch_marker
            .try_exists()
            .expect("inspect post-recovery mismatch marker"),
        "fixture observed another frame after the mismatched workflow ID"
    );

    drop(session);
    drop(provider);
    wait_for_observer_done(&observer_done).await;
    remove_fixture_executable(&host_program).await;
    drop(fixture_dir);
}
