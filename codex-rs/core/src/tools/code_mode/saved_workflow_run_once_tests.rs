use std::collections::VecDeque;
use std::convert::Infallible;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

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
use codex_code_mode::SAVED_WORKFLOW_EXECUTION_FAILED;
use codex_code_mode::StartedCell;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use codex_core_workflows::WorkflowRoot;
use codex_core_workflows::WorkflowScope;
use codex_core_workflows::WorkflowSourceResolver;
use codex_core_workflows::WorkflowSourceSnapshot;
use codex_core_workflows::load_workflows_from_roots;
use codex_features::Feature;
use codex_protocol::protocol::SessionSource;
use codex_rollout_trace::CodeCellRuntimeStatus;
use codex_rollout_trace::RawTraceEvent;
use codex_rollout_trace::RawTraceEventPayload;
use codex_rollout_trace::ThreadStartedTraceMetadata;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::sync::oneshot;

use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;

use super::DEFAULT_WAIT_YIELD_TIME_MS;
use super::SOURCE_LOAD_FAILED;
use super::SOURCE_NOT_FOUND;
use super::SOURCE_RESOLVER_UNAVAILABLE;
use super::run_saved_workflow_once;
use crate::tools::code_mode::CodeModeService;
use crate::tools::code_mode::ExecContext;

const CELL_ID: &str = "saved-cell";

struct RecordingState {
    create_count: AtomicUsize,
    shutdown_count: AtomicUsize,
    shutdown_result: Mutex<Result<(), String>>,
    requests: Mutex<Vec<ExecuteRequest>>,
    wait_requests: Mutex<Vec<WaitRequest>>,
    initial_response: Mutex<Option<Result<RuntimeResponse, String>>>,
    wait_responses: Mutex<VecDeque<Result<WaitOutcome, String>>>,
    wait_block: Option<(Arc<Notify>, Arc<Notify>)>,
}

impl RecordingState {
    fn new(
        initial_response: Result<RuntimeResponse, String>,
        wait_responses: impl IntoIterator<Item = Result<WaitOutcome, String>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            create_count: AtomicUsize::new(/*v*/ 0),
            shutdown_count: AtomicUsize::new(/*v*/ 0),
            shutdown_result: Mutex::new(Ok(())),
            requests: Mutex::new(Vec::new()),
            wait_requests: Mutex::new(Vec::new()),
            initial_response: Mutex::new(Some(initial_response)),
            wait_responses: Mutex::new(wait_responses.into_iter().collect()),
            wait_block: None,
        })
    }

    fn block_wait(mut self: Arc<Self>, started: Arc<Notify>, release: Arc<Notify>) -> Arc<Self> {
        let state = Arc::get_mut(&mut self).expect("recording state is not shared yet");
        state.wait_block = Some((started, release));
        self
    }
}

struct RecordingProvider {
    state: Arc<RecordingState>,
    create_block: Option<(Arc<Notify>, Arc<Notify>)>,
}

impl CodeModeSessionProvider for RecordingProvider {
    fn create_session<'a>(
        &'a self,
        _delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> CodeModeSessionProviderFuture<'a> {
        let state = Arc::clone(&self.state);
        let create_block = self.create_block.clone();
        Box::pin(async move {
            state.create_count.fetch_add(1, Ordering::SeqCst);
            if let Some((started, release)) = create_block {
                let released = release.notified();
                started.notify_one();
                released.await;
            }
            let session: Arc<dyn CodeModeSession> = Arc::new(RecordingSession { state });
            Ok(session)
        })
    }
}

struct RecordingSession {
    state: Arc<RecordingState>,
}

impl CodeModeSession for RecordingSession {
    fn execute<'a>(
        &'a self,
        request: ExecuteRequest,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        self.state
            .requests
            .lock()
            .expect("request lock")
            .push(request);
        let response = self
            .state
            .initial_response
            .lock()
            .expect("initial response lock")
            .take()
            .expect("one execute request");
        let cell_id = CellId::new(CELL_ID.to_string());
        let (response_tx, response_rx) = oneshot::channel();
        let _ = response_tx.send(response);
        Box::pin(async move { Ok(StartedCell::from_result_receiver(cell_id, response_rx)) })
    }

    fn wait<'a>(&'a self, request: WaitRequest) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        self.state
            .wait_requests
            .lock()
            .expect("wait request lock")
            .push(request);
        let wait_block = self.state.wait_block.clone();
        let response = self
            .state
            .wait_responses
            .lock()
            .expect("wait response lock")
            .pop_front()
            .expect("queued wait response");
        Box::pin(async move {
            if let Some((started, release)) = wait_block {
                started.notify_one();
                release.notified().await;
            }
            response
        })
    }

    fn terminate<'a>(&'a self, _cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async { Err("terminate is not expected".to_string()) })
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        self.state.shutdown_count.fetch_add(1, Ordering::SeqCst);
        let result = self
            .state
            .shutdown_result
            .lock()
            .expect("shutdown result lock")
            .clone();
        Box::pin(async move { result })
    }
}

#[tokio::test]
async fn preflight_failures_do_not_initialize_runtime_or_trace() -> anyhow::Result<()> {
    let disabled = WorkflowSourceResolver::new(|_name| async {
        panic!("disabled workflow must not call its resolver");
        #[allow(unreachable_code)]
        Ok::<Option<WorkflowSourceSnapshot>, Infallible>(None)
    });
    let not_found = WorkflowSourceResolver::new(|_name| async {
        Ok::<Option<WorkflowSourceSnapshot>, Infallible>(None)
    });
    let load_error = WorkflowSourceResolver::new(|_name| async {
        Err::<Option<WorkflowSourceSnapshot>, _>("/private/executor/workflows/review.js")
    });
    for (workflow_enabled, resolver, expected_error) in [
        (false, Some(disabled), SOURCE_RESOLVER_UNAVAILABLE),
        (true, None, SOURCE_RESOLVER_UNAVAILABLE),
        (true, Some(not_found), SOURCE_NOT_FOUND),
        (true, Some(load_error), SOURCE_LOAD_FAILED),
    ] {
        assert_preflight_failure(workflow_enabled, resolver, expected_error).await?;
    }

    Ok(())
}

#[tokio::test]
async fn exact_snapshot_executes_once_and_is_observed_to_terminal() -> anyhow::Result<()> {
    let source_temp = TempDir::new()?;
    let (snapshot, exact_source, workflow_path) = captured_snapshot(&source_temp).await?;
    fs::write(
        workflow_path,
        workflow_source("review", "replacement after capture"),
    )?;

    let resolve_count = Arc::new(AtomicUsize::new(/*v*/ 0));
    let resolver = snapshot_resolver(snapshot, Arc::clone(&resolve_count));
    let initial = RuntimeResponse::Yielded {
        cell_id: CellId::new(CELL_ID.to_string()),
        content_items: vec![text_item("first")],
    };
    let terminal = RuntimeResponse::Result {
        cell_id: CellId::new(CELL_ID.to_string()),
        content_items: vec![text_item("second")],
        error_text: None,
    };
    let state = RecordingState::new(Ok(initial), [Ok(WaitOutcome::LiveCell(terminal.clone()))]);
    let trace_temp = TempDir::new()?;
    let exec = test_exec(
        Arc::clone(&state),
        /*workflow_enabled*/ true,
        /*resolver*/ Some(resolver),
        trace_temp.path(),
    )
    .await?;

    let output = run_saved_workflow_once(&exec, "workflow-call", "review").await?;

    assert_eq!(resolve_count.load(Ordering::SeqCst), 1);
    let requests = state.requests.lock().expect("request lock").clone();
    let [request] = requests.as_slice() else {
        panic!("expected one request, got {requests:?}");
    };
    assert_eq!(
        request,
        &ExecuteRequest {
            tool_call_id: "workflow-call".to_string(),
            enabled_tools: Vec::new(),
            source: exact_source.clone(),
            output_policy: ExecuteOutputPolicy::SavedWorkflow,
            yield_time_ms: None,
            max_output_tokens: None,
        }
    );
    assert_eq!(
        output,
        RuntimeResponse::Result {
            cell_id: CellId::new(CELL_ID.to_string()),
            content_items: vec![text_item("first"), text_item("second")],
            error_text: None,
        }
    );
    assert_eq!(
        state
            .wait_requests
            .lock()
            .expect("wait request lock")
            .clone(),
        vec![WaitRequest {
            cell_id: CellId::new(CELL_ID.to_string()),
            yield_time_ms: DEFAULT_WAIT_YIELD_TIME_MS,
        }]
    );

    let events = raw_trace_events(trace_temp.path())?;
    let started_sources = events
        .iter()
        .filter_map(|event| match &event.payload {
            RawTraceEventPayload::CodeCellStarted { source_js, .. } => Some(source_js.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(started_sources, vec![exact_source.as_str()]);
    assert_eq!(
        ended_statuses(trace_temp.path())?,
        vec![CodeCellRuntimeStatus::Completed]
    );

    Ok(())
}

#[tokio::test]
async fn caller_cancellation_does_not_drop_the_runtime_owner() -> anyhow::Result<()> {
    let source_temp = TempDir::new()?;
    let (snapshot, _exact_source, _workflow_path) = captured_snapshot(&source_temp).await?;
    let wait_started = Arc::new(Notify::new());
    let wait_release = Arc::new(Notify::new());
    let state = RecordingState::new(
        Ok(yielded(CELL_ID)),
        [Ok(WaitOutcome::LiveCell(completed(CELL_ID)))],
    )
    .block_wait(Arc::clone(&wait_started), Arc::clone(&wait_release));
    let trace_temp = TempDir::new()?;
    let resolver = snapshot_resolver(snapshot, Arc::new(AtomicUsize::new(/*v*/ 0)));
    let exec = test_exec(
        state,
        /*workflow_enabled*/ true,
        /*resolver*/ Some(resolver),
        trace_temp.path(),
    )
    .await?;
    let session_owner = Arc::clone(&exec.session);

    let task = tokio::spawn(async move {
        run_saved_workflow_once(&exec, "cancelled-workflow-call", "review").await
    });
    let timeout = std::time::Duration::from_secs(/*secs*/ 5);
    tokio::time::timeout(timeout, wait_started.notified()).await?;
    task.abort();
    assert!(
        task.await
            .expect_err("caller task should be cancelled")
            .is_cancelled()
    );
    wait_release.notify_one();
    tokio::time::timeout(timeout, async {
        loop {
            if ended_statuses(trace_temp.path())
                .is_ok_and(|statuses| statuses == [CodeCellRuntimeStatus::Completed])
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    drop(session_owner);

    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn detached_runtime_owner_does_not_retain_the_session() -> anyhow::Result<()> {
    let source_temp = TempDir::new()?;
    let (snapshot, _source, _path) = captured_snapshot(&source_temp).await?;
    let wait_started = Arc::new(Notify::new());
    let wait_release = Arc::new(Notify::new());
    let state = RecordingState::new(
        Ok(yielded(CELL_ID)),
        [Ok(WaitOutcome::LiveCell(completed(CELL_ID)))],
    )
    .block_wait(Arc::clone(&wait_started), wait_release);
    let trace_temp = TempDir::new()?;
    let resolver = snapshot_resolver(snapshot, Arc::new(AtomicUsize::new(/*v*/ 0)));
    let exec = test_exec(
        state,
        /*workflow_enabled*/ true,
        /*resolver*/ Some(resolver),
        trace_temp.path(),
    )
    .await?;
    let session_owner = Arc::clone(&exec.session);
    let weak_session = Arc::downgrade(&session_owner);
    let task = tokio::spawn(async move {
        run_saved_workflow_once(&exec, "detached-workflow-call", "review").await
    });
    let timeout = std::time::Duration::from_secs(/*secs*/ 5);
    tokio::time::timeout(timeout, wait_started.notified()).await?;
    let weak_runtime_session = Arc::downgrade(
        session_owner
            .services
            .code_mode_service
            .session
            .get()
            .expect("runtime session"),
    );
    task.abort();
    assert!(
        task.await
            .expect_err("caller task should be cancelled")
            .is_cancelled()
    );

    drop(session_owner);

    assert!(weak_session.upgrade().is_none());
    assert!(weak_runtime_session.upgrade().is_some());
    tokio::time::timeout(timeout, async {
        loop {
            if ended_statuses(trace_temp.path())
                .is_ok_and(|statuses| statuses == [CodeCellRuntimeStatus::Failed])
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert!(weak_runtime_session.upgrade().is_none());

    Ok(())
}

#[tokio::test]
async fn service_shutdown_waits_for_tasks_and_preserves_the_runtime_result() -> anyhow::Result<()> {
    let state = RecordingState::new(
        Err("runtime must not initialize".to_string()),
        std::iter::empty(),
    );
    let service = CodeModeService::new(Arc::new(RecordingProvider {
        state: Arc::clone(&state),
        create_block: None,
    }));
    let permit = service.reserve_runtime_task().expect("reserve task");
    let shutdown = service.shutdown();
    tokio::pin!(shutdown);

    assert!(futures::poll!(&mut shutdown).is_pending());
    assert!(service.reserve_runtime_task().is_err());
    drop(permit);
    assert_eq!(shutdown.await, Ok(()));
    assert_eq!(state.create_count.load(Ordering::SeqCst), 0);

    let state = RecordingState::new(Err("unused".to_string()), std::iter::empty());
    *state.shutdown_result.lock().expect("shutdown result lock") =
        Err("runtime shutdown failed".to_string());
    let service = CodeModeService::new(Arc::new(RecordingProvider {
        state: Arc::clone(&state),
        create_block: None,
    }));
    let _runtime_session = service.session().await.map_err(anyhow::Error::msg)?;
    let permit = service.reserve_runtime_task().expect("reserve task");
    let shutdown = service.shutdown();
    tokio::pin!(shutdown);
    assert!(futures::poll!(&mut shutdown).is_pending());
    assert_eq!(state.shutdown_count.load(Ordering::SeqCst), 1);
    drop(permit);
    assert_eq!(shutdown.await, Err("runtime shutdown failed".to_string()));

    Ok(())
}

#[tokio::test]
async fn session_initialization_finishing_during_shutdown_is_rejected() -> anyhow::Result<()> {
    let state = RecordingState::new(Err("unused".to_string()), std::iter::empty());
    let create_started = Arc::new(Notify::new());
    let create_release = Arc::new(Notify::new());
    let service = Arc::new(CodeModeService::new(Arc::new(RecordingProvider {
        state: Arc::clone(&state),
        create_block: Some((Arc::clone(&create_started), Arc::clone(&create_release))),
    })));
    let init_service = Arc::clone(&service);
    let init = tokio::spawn(async move { init_service.session().await });
    create_started.notified().await;
    let shutdown = service.shutdown();
    tokio::pin!(shutdown);
    assert!(futures::poll!(&mut shutdown).is_pending());
    assert!(service.reserve_runtime_task().is_err());

    create_release.notify_one();

    assert_eq!(shutdown.await, Ok(()));
    let Err(error) = init.await? else {
        panic!("initialization completed during shutdown");
    };
    assert_eq!(error, "code mode session is shutting down");
    assert_eq!(
        (
            state.create_count.load(Ordering::SeqCst),
            state.shutdown_count.load(Ordering::SeqCst),
        ),
        (1, 1)
    );

    Ok(())
}

#[tokio::test]
async fn service_shutdown_cancels_and_joins_the_runtime_owner() -> anyhow::Result<()> {
    let source_temp = TempDir::new()?;
    let (snapshot, _source, _path) = captured_snapshot(&source_temp).await?;
    let wait_started = Arc::new(Notify::new());
    let wait_release = Arc::new(Notify::new());
    let state = RecordingState::new(
        Ok(yielded(CELL_ID)),
        [Ok(WaitOutcome::LiveCell(completed(CELL_ID)))],
    )
    .block_wait(Arc::clone(&wait_started), wait_release);
    let trace_temp = TempDir::new()?;
    let resolver = snapshot_resolver(snapshot, Arc::new(AtomicUsize::new(/*v*/ 0)));
    let exec = test_exec(
        Arc::clone(&state),
        /*workflow_enabled*/ true,
        /*resolver*/ Some(resolver),
        trace_temp.path(),
    )
    .await?;
    let session = Arc::clone(&exec.session);
    let task =
        tokio::spawn(
            async move { run_saved_workflow_once(&exec, "shutdown-call", "review").await },
        );
    let timeout = std::time::Duration::from_secs(/*secs*/ 5);
    tokio::time::timeout(timeout, wait_started.notified()).await?;

    tokio::time::timeout(timeout, session.services.code_mode_service.shutdown())
        .await?
        .map_err(anyhow::Error::msg)?;

    assert_eq!(
        tokio::time::timeout(timeout, task).await??,
        Err(FunctionCallError::RespondToModel(
            SAVED_WORKFLOW_EXECUTION_FAILED.to_string()
        ))
    );
    assert_eq!(state.shutdown_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        ended_statuses(trace_temp.path())?,
        vec![CodeCellRuntimeStatus::Failed]
    );

    Ok(())
}

#[tokio::test]
async fn post_start_failures_are_redacted_and_end_the_trace() -> anyhow::Result<()> {
    let source_temp = TempDir::new()?;
    let (snapshot, _source, _path) = captured_snapshot(&source_temp).await?;
    let cases = vec![
        (
            "initial",
            Err("private initial response error".to_string()),
            vec![],
        ),
        (
            "identity",
            Ok(yielded(CELL_ID)),
            vec![Ok(WaitOutcome::LiveCell(completed("other-cell")))],
        ),
    ];

    for (case, initial_response, wait_responses) in cases {
        let state = RecordingState::new(initial_response, wait_responses);
        let trace_temp = TempDir::new()?;
        let exec = test_exec(
            state,
            /*workflow_enabled*/ true,
            /*resolver*/
            Some(snapshot_resolver(
                snapshot.clone(),
                Arc::new(AtomicUsize::new(/*v*/ 0)),
            )),
            trace_temp.path(),
        )
        .await?;

        assert_eq!(
            run_saved_workflow_once(&exec, "workflow-call", "review").await,
            Err(FunctionCallError::RespondToModel(
                SAVED_WORKFLOW_EXECUTION_FAILED.to_string()
            )),
            "{case}"
        );
        assert_eq!(
            ended_statuses(trace_temp.path())?,
            vec![CodeCellRuntimeStatus::Failed],
            "{case}"
        );
    }

    Ok(())
}

async fn assert_preflight_failure(
    workflow_enabled: bool,
    resolver: Option<WorkflowSourceResolver>,
    expected_error: &str,
) -> anyhow::Result<()> {
    let trace_temp = TempDir::new()?;
    let state = RecordingState::new(
        Err("runtime must not initialize".to_string()),
        std::iter::empty(),
    );
    let exec = test_exec(
        Arc::clone(&state),
        workflow_enabled,
        resolver,
        trace_temp.path(),
    )
    .await?;

    assert_eq!(
        run_saved_workflow_once(&exec, "workflow-call", "review").await,
        Err(FunctionCallError::RespondToModel(
            expected_error.to_string()
        ))
    );
    assert_eq!(state.create_count.load(Ordering::SeqCst), 0);
    assert!(
        !raw_trace_events(trace_temp.path())?
            .iter()
            .any(|event| matches!(event.payload, RawTraceEventPayload::CodeCellStarted { .. }))
    );
    Ok(())
}

async fn test_exec(
    state: Arc<RecordingState>,
    workflow_enabled: bool,
    resolver: Option<WorkflowSourceResolver>,
    trace_root: &Path,
) -> anyhow::Result<ExecContext> {
    let (mut session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .set_enabled(Feature::Workflow, workflow_enabled)
        .expect("workflow feature should be configurable");
    turn.config = Arc::new(config);
    session.services.code_mode_service = CodeModeService::new(Arc::new(RecordingProvider {
        state,
        create_block: None,
    }));
    if let Some(resolver) = resolver {
        session.services.thread_extension_data.insert(resolver);
    }
    attach_test_trace(&mut session, &turn, trace_root)?;
    Ok(ExecContext {
        session: Arc::new(session),
        turn: Arc::new(turn),
    })
}

fn attach_test_trace(session: &mut Session, turn: &TurnContext, root: &Path) -> anyhow::Result<()> {
    let rollout_thread_trace =
        codex_rollout_trace::ThreadTraceContext::start_root_in_root_for_test(
            root,
            ThreadStartedTraceMetadata {
                thread_id: session.thread_id.to_string(),
                agent_path: "/root".to_string(),
                task_name: None,
                nickname: None,
                agent_role: None,
                session_source: SessionSource::Exec,
                cwd: PathBuf::from("/workspace"),
                rollout_path: None,
                model: "gpt-test".to_string(),
                provider_name: "test-provider".to_string(),
                approval_policy: "never".to_string(),
                sandbox_policy: "danger-full-access".to_string(),
            },
        )?;
    rollout_thread_trace.record_codex_turn_started(turn.sub_id.as_str());
    session.services.rollout_thread_trace = rollout_thread_trace;
    Ok(())
}

fn raw_trace_events(root: &Path) -> anyhow::Result<Vec<RawTraceEvent>> {
    let event_log = fs::read_to_string(single_bundle_dir(root)?.join("trace.jsonl"))?;
    event_log
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

fn single_bundle_dir(root: &Path) -> anyhow::Result<PathBuf> {
    let mut entries = fs::read_dir(root)?;
    let entry = entries.next().expect("one trace bundle")?;
    assert!(entries.next().is_none());
    Ok(entry.path())
}

async fn captured_snapshot(
    temp: &TempDir,
) -> anyhow::Result<(WorkflowSourceSnapshot, String, PathBuf)> {
    let root = temp.path().join("workflows");
    fs::create_dir_all(&root)?;
    let path = root.join("review.js");
    fs::write(&path, workflow_source("review", "captured"))?;
    let registry =
        load_workflows_from_roots([WorkflowRoot::new(absolute(&root), WorkflowScope::Project)])
            .await;
    let snapshot = registry
        .source_snapshot_by_name("review")
        .await?
        .expect("discovered workflow snapshot");
    let source = snapshot.source().to_string();
    Ok((snapshot, source, path))
}

fn snapshot_resolver(
    snapshot: WorkflowSourceSnapshot,
    resolve_count: Arc<AtomicUsize>,
) -> WorkflowSourceResolver {
    WorkflowSourceResolver::new(move |_name| {
        resolve_count.fetch_add(1, Ordering::SeqCst);
        let snapshot = snapshot.clone();
        async move { Ok::<_, Infallible>(Some(snapshot)) }
    })
}

fn workflow_source(name: &str, description: &str) -> String {
    format!(
        "export const meta = {{ name: '{name}', description: '{description}', phases: [] }};\n\
         text('run-once');\n"
    )
}

fn absolute(path: &Path) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path_checked(path).expect("absolute test path")
}

fn text_item(text: &str) -> FunctionCallOutputContentItem {
    FunctionCallOutputContentItem::InputText {
        text: text.to_string(),
    }
}

fn yielded(cell_id: &str) -> RuntimeResponse {
    RuntimeResponse::Yielded {
        cell_id: CellId::new(cell_id.to_string()),
        content_items: Vec::new(),
    }
}

fn completed(cell_id: &str) -> RuntimeResponse {
    RuntimeResponse::Result {
        cell_id: CellId::new(cell_id.to_string()),
        content_items: Vec::new(),
        error_text: None,
    }
}

fn ended_statuses(root: &Path) -> anyhow::Result<Vec<CodeCellRuntimeStatus>> {
    Ok(raw_trace_events(root)?
        .into_iter()
        .filter_map(|event| match event.payload {
            RawTraceEventPayload::CodeCellEnded { status, .. } => Some(status),
            _ => None,
        })
        .collect())
}
