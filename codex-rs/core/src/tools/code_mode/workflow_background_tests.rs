use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeSessionProviderFuture;
use codex_code_mode::CodeModeSessionResultFuture;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::StartedCell;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use codex_features::Feature;
use codex_features::Features;
use codex_workflow_journal::WorkflowRunLease;
use codex_workflow_journal::WorkflowRunLeaseAcquire;
use codex_workflow_journal::WorkflowRunMeta;
use codex_workflow_journal::WorkflowRunStatus;
use codex_workflow_journal::ReplayJournal;
use codex_workflow_journal::storage::WorkflowRunPaths;
use pretty_assertions::assert_eq;
use tokio::sync::oneshot;

use super::WorkflowRunLineage;
use super::run_workflow_source_to_terminal;
use super::start_workflow_source;
use crate::session::tests::make_session_and_context;
use crate::tools::code_mode::CodeModeService;
use crate::tools::code_mode::ExecContext;
use crate::tools::code_mode::WorkflowRunCancelOutcome;
use crate::tools::code_mode::delegate::CodeModeDispatchOrigin;
use crate::tools::code_mode::workflow_progress::WorkflowEventTarget;
use crate::tools::code_mode::workflow_progress::durable::DurableProgressRead;
use crate::tools::code_mode::workflow_progress::durable::DurableRunState;
use crate::tools::code_mode::workflow_progress::durable::DurableRunStatus;
use crate::tools::code_mode::workflow_progress::durable::read as read_durable_progress;

const SOURCE: &str = concat!(
    "export const meta = { name: 'background-test', description: 'test' };\n",
    "text('done');\n",
);

#[derive(Default)]
struct ControlledSession {
    initial_response: Mutex<Option<oneshot::Sender<Result<RuntimeResponse, String>>>>,
    wait_response: Mutex<Option<oneshot::Sender<Result<WaitOutcome, String>>>>,
    panic_on_wait: AtomicBool,
    wait_calls: AtomicUsize,
    terminate_calls: AtomicUsize,
    shutdown_calls: AtomicUsize,
    terminate_started: tokio::sync::Notify,
    terminate_release: Mutex<Option<Arc<tokio::sync::Notify>>>,
}

impl ControlledSession {
    fn complete_initial(&self, response: RuntimeResponse) {
        self.initial_response
            .lock()
            .expect("initial response lock")
            .take()
            .expect("initial response sender")
            .send(Ok(response))
            .expect("initial response receiver");
    }

    fn complete_wait(&self, response: RuntimeResponse) {
        self.wait_response
            .lock()
            .expect("wait response lock")
            .take()
            .expect("wait response sender")
            .send(Ok(WaitOutcome::LiveCell(response)))
            .expect("wait response receiver");
    }

    fn block_termination(&self) -> Arc<tokio::sync::Notify> {
        let release = Arc::new(tokio::sync::Notify::new());
        *self
            .terminate_release
            .lock()
            .expect("terminate release lock") = Some(Arc::clone(&release));
        release
    }
}

impl CodeModeSession for ControlledSession {
    fn execute<'a>(
        &'a self,
        _request: ExecuteRequest,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        let (response_tx, response_rx) = oneshot::channel();
        *self.initial_response.lock().expect("initial response lock") = Some(response_tx);
        Box::pin(async move {
            Ok(StartedCell::from_result_receiver(
                CellId::new("background-cell".to_string()),
                response_rx,
            ))
        })
    }

    fn wait<'a>(&'a self, _request: WaitRequest) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        self.wait_calls.fetch_add(1, Ordering::AcqRel);
        if self.panic_on_wait.load(Ordering::Acquire) {
            return Box::pin(async { panic!("controlled workflow wait panic") });
        }
        let (response_tx, response_rx) = oneshot::channel();
        *self.wait_response.lock().expect("wait response lock") = Some(response_tx);
        Box::pin(async move {
            response_rx
                .await
                .map_err(|_| "controlled wait response dropped".to_string())?
        })
    }

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        self.terminate_calls.fetch_add(1, Ordering::AcqRel);
        self.terminate_started.notify_one();
        let terminate_release = self
            .terminate_release
            .lock()
            .expect("terminate release lock")
            .clone();
        self.initial_response
            .lock()
            .expect("initial response lock")
            .take();
        self.wait_response
            .lock()
            .expect("wait response lock")
            .take();
        Box::pin(async move {
            if let Some(release) = terminate_release {
                release.notified().await;
            }
            Ok(WaitOutcome::LiveCell(RuntimeResponse::Terminated {
                cell_id,
                content_items: Vec::new(),
            }))
        })
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        self.shutdown_calls.fetch_add(1, Ordering::AcqRel);
        Box::pin(async { Ok(()) })
    }
}

struct ControlledSessionProvider {
    session: Arc<ControlledSession>,
}

impl CodeModeSessionProvider for ControlledSessionProvider {
    fn create_session<'a>(
        &'a self,
        _delegate: Arc<dyn codex_code_mode::CodeModeSessionDelegate>,
    ) -> CodeModeSessionProviderFuture<'a> {
        let session: Arc<dyn CodeModeSession> = self.session.clone();
        Box::pin(async move { Ok(session) })
    }
}

fn enabled_features() -> Features {
    let mut features = Features::default();
    features.enable(Feature::Workflow);
    features
}

async fn session_event_target(codex_home: &Path) -> WorkflowEventTarget {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config.codex_home =
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(codex_home)
            .expect("temporary Codex home is absolute");
    turn.config = Arc::new(config);
    // Keep the production Session persistence/emission path while bypassing only the dispatch
    // broker: this harness supplies its own controlled code-mode session and has no live turn host.
    WorkflowEventTarget::Session {
        exec: ExecContext {
            session: Arc::new(session),
            turn: Arc::new(turn),
        },
        dispatch_origin: CodeModeDispatchOrigin::Disabled,
    }
}

async fn wait_for_status(paths: &WorkflowRunPaths, expected: WorkflowRunStatus) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let meta: WorkflowRunMeta = serde_json::from_str(
                &std::fs::read_to_string(paths.meta()).expect("read workflow meta"),
            )
            .expect("parse workflow meta");
            if meta.status == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("workflow status transition");
}

#[tokio::test]
async fn detached_start_returns_after_durable_init_and_drains_yields_to_terminal() {
    let controlled = Arc::new(ControlledSession::default());
    let service = CodeModeService::new(Arc::new(ControlledSessionProvider {
        session: Arc::clone(&controlled),
    }));
    let home = tempfile::tempdir().expect("temporary codex home");

    let run_id = start_workflow_source(
        &enabled_features(),
        &service,
        "background-start".to_string(),
        Vec::new(),
        SOURCE,
        serde_json::Value::Null,
        WorkflowRunLineage {
            parent_run_id: None,
            depth: 0,
        },
        home.path(),
        /*resume*/ None,
        WorkflowEventTarget::Disabled,
    )
    .await
    .expect("detached workflow starts");
    let paths = WorkflowRunPaths::new(home.path(), &run_id);

    assert!(paths.script().is_file());
    assert!(paths.meta().is_file());
    assert!(paths.journal().is_file());
    assert_eq!(controlled.terminate_calls.load(Ordering::Acquire), 0);

    controlled.complete_initial(RuntimeResponse::Yielded {
        cell_id: CellId::new("background-cell".to_string()),
        content_items: Vec::new(),
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while controlled.wait_calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached task observes after yield");
    let live = crate::workflow_recovery::reconcile_stale_workflow_run(
        home.path(),
        &run_id,
        /*state_db*/ None,
    )
    .await;
    assert_eq!(live.active, 1);
    assert_eq!(live.reconciled, 0);
    controlled.complete_wait(RuntimeResponse::Result {
        cell_id: CellId::new("background-cell".to_string()),
        content_items: Vec::new(),
        error_text: None,
    });

    wait_for_status(&paths, WorkflowRunStatus::Completed).await;
    service.shutdown().await.expect("shutdown service");
    assert_eq!(controlled.terminate_calls.load(Ordering::Acquire), 0);
    assert_eq!(controlled.shutdown_calls.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn foreground_run_retains_lease_and_content_across_yield() {
    let controlled = Arc::new(ControlledSession::default());
    let service = Arc::new(CodeModeService::new(Arc::new(ControlledSessionProvider {
        session: Arc::clone(&controlled),
    })));
    let home = tempfile::tempdir().expect("temporary codex home");
    let task_service = Arc::clone(&service);
    let task_home = home.path().to_path_buf();
    let run = tokio::spawn(async move {
        run_workflow_source_to_terminal(
            &enabled_features(),
            &task_service,
            "foreground-run".to_string(),
            Vec::new(),
            SOURCE,
            serde_json::Value::Null,
            WorkflowRunLineage {
                parent_run_id: None,
                depth: 0,
            },
            &task_home,
            /*resume*/ None,
            WorkflowEventTarget::Disabled,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
    });

    let run_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let root = codex_workflow_journal::storage::runs_root(home.path());
            if let Ok(mut entries) = tokio::fs::read_dir(root).await
                && let Ok(Some(entry)) = entries.next_entry().await
            {
                break entry.file_name().to_string_lossy().into_owned();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("foreground run directory");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if controlled
                .initial_response
                .lock()
                .expect("initial response lock")
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("foreground initial response waiter");
    controlled.complete_initial(RuntimeResponse::Yielded {
        cell_id: CellId::new("background-cell".to_string()),
        content_items: vec![FunctionCallOutputContentItem::InputText {
            text: "first".to_string(),
        }],
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while controlled.wait_calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("foreground task observes yield");

    let live = crate::workflow_recovery::reconcile_stale_workflow_run(
        home.path(),
        &run_id,
        /*state_db*/ None,
    )
    .await;
    assert_eq!(live.active, 1);
    assert_eq!(live.reconciled, 0);
    controlled.complete_wait(RuntimeResponse::Result {
        cell_id: CellId::new("background-cell".to_string()),
        content_items: vec![FunctionCallOutputContentItem::InputText {
            text: "second".to_string(),
        }],
        error_text: None,
    });

    let output = run
        .await
        .expect("join foreground run")
        .expect("foreground run completes");
    let RuntimeResponse::Result { content_items, .. } = output.response else {
        panic!("expected terminal result");
    };
    assert_eq!(
        content_items,
        vec![
            FunctionCallOutputContentItem::InputText {
                text: "first".to_string(),
            },
            FunctionCallOutputContentItem::InputText {
                text: "second".to_string(),
            },
        ]
    );
    assert_eq!(
        WorkflowRunPaths::new(home.path(), &run_id)
            .read_meta_bounded()
            .expect("read terminal foreground metadata")
            .status,
        WorkflowRunStatus::Completed
    );
    service.shutdown().await.expect("shutdown service");
}

#[tokio::test]
async fn foreground_completion_fails_closed_when_terminal_metadata_write_fails() {
    let controlled = Arc::new(ControlledSession::default());
    let service = Arc::new(CodeModeService::new(Arc::new(ControlledSessionProvider {
        session: Arc::clone(&controlled),
    })));
    let home = tempfile::tempdir().expect("temporary codex home");
    let event_target = session_event_target(home.path()).await;
    let task_service = Arc::clone(&service);
    let task_home = home.path().to_path_buf();
    let run = tokio::spawn(async move {
        run_workflow_source_to_terminal(
            &enabled_features(),
            &task_service,
            "foreground-terminal-persistence-failure".to_string(),
            Vec::new(),
            SOURCE,
            serde_json::Value::Null,
            WorkflowRunLineage {
                parent_run_id: None,
                depth: 0,
            },
            &task_home,
            /*resume*/ None,
            event_target,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
    });

    let run_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let root = codex_workflow_journal::storage::runs_root(home.path());
            if let Ok(mut entries) = tokio::fs::read_dir(root).await
                && let Ok(Some(entry)) = entries.next_entry().await
                && controlled
                    .initial_response
                    .lock()
                    .expect("initial response lock")
                    .is_some()
            {
                break entry.file_name().to_string_lossy().into_owned();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("foreground run initialization");
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    let corrupt_meta = b"{not valid workflow metadata json";
    std::fs::write(paths.meta(), corrupt_meta).expect("corrupt terminal metadata target");

    controlled.complete_initial(RuntimeResponse::Result {
        cell_id: CellId::new("background-cell".to_string()),
        content_items: Vec::new(),
        error_text: None,
    });

    let error = run
        .await
        .expect("join foreground run")
        .expect_err("terminal metadata failure must not return a successful workflow result");
    assert!(
        error
            .to_string()
            .contains("terminal state could not be durably committed")
    );
    assert_eq!(
        std::fs::read(paths.meta()).expect("read rejected metadata update"),
        corrupt_meta,
    );
    let progress: serde_json::Value = serde_json::from_slice(
        &std::fs::read(paths.progress()).expect("read raw progress after metadata failure"),
    )
    .expect("terminal progress remains valid JSON");
    assert_eq!(
        (progress["state"].clone(), progress["status"].clone()),
        (
            serde_json::json!("terminal"),
            serde_json::json!({ "completed": null }),
        ),
    );
    service.shutdown().await.expect("shutdown service");
}

#[tokio::test]
async fn foreground_completion_uses_authoritative_metadata_when_progress_write_fails() {
    let controlled = Arc::new(ControlledSession::default());
    let service = Arc::new(CodeModeService::new(Arc::new(ControlledSessionProvider {
        session: Arc::clone(&controlled),
    })));
    let home = tempfile::tempdir().expect("temporary codex home");
    let event_target = session_event_target(home.path()).await;
    let task_service = Arc::clone(&service);
    let task_home = home.path().to_path_buf();
    let run = tokio::spawn(async move {
        run_workflow_source_to_terminal(
            &enabled_features(),
            &task_service,
            "foreground-progress-persistence-failure".to_string(),
            Vec::new(),
            SOURCE,
            serde_json::Value::Null,
            WorkflowRunLineage {
                parent_run_id: None,
                depth: 0,
            },
            &task_home,
            /*resume*/ None,
            event_target,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
    });

    let run_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let root = codex_workflow_journal::storage::runs_root(home.path());
            if let Ok(mut entries) = tokio::fs::read_dir(root).await
                && let Ok(Some(entry)) = entries.next_entry().await
                && controlled
                    .initial_response
                    .lock()
                    .expect("initial response lock")
                    .is_some()
            {
                break entry.file_name().to_string_lossy().into_owned();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("foreground run initialization");
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    let corrupt_progress = b"{not valid progress json";
    std::fs::write(paths.progress(), corrupt_progress).expect("corrupt progress projection");

    controlled.complete_initial(RuntimeResponse::Result {
        cell_id: CellId::new("background-cell".to_string()),
        content_items: Vec::new(),
        error_text: None,
    });

    run.await
        .expect("join foreground run")
        .expect("metadata and recorder durability permit successful completion");
    assert_eq!(
        paths
            .read_meta_bounded()
            .expect("read authoritative terminal metadata")
            .status,
        WorkflowRunStatus::Completed
    );
    assert_eq!(
        std::fs::read(paths.progress()).expect("read rejected progress projection"),
        corrupt_progress,
    );
    ReplayJournal::load(&paths.journal()).expect("closed journal remains replayable");
    service.shutdown().await.expect("shutdown service");
}

#[tokio::test]
async fn foreground_run_rejects_aggregate_image_and_text_output_without_partial_result() {
    let controlled = Arc::new(ControlledSession::default());
    let service = Arc::new(CodeModeService::new(Arc::new(ControlledSessionProvider {
        session: Arc::clone(&controlled),
    })));
    let home = tempfile::tempdir().expect("temporary codex home");
    let task_service = Arc::clone(&service);
    let task_home = home.path().to_path_buf();
    let run = tokio::spawn(async move {
        run_workflow_source_to_terminal(
            &enabled_features(),
            &task_service,
            "foreground-output-cap".to_string(),
            Vec::new(),
            SOURCE,
            serde_json::Value::Null,
            WorkflowRunLineage {
                parent_run_id: None,
                depth: 0,
            },
            &task_home,
            /*resume*/ None,
            WorkflowEventTarget::Disabled,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
    });

    let run_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let root = codex_workflow_journal::storage::runs_root(home.path());
            if let Ok(mut entries) = tokio::fs::read_dir(root).await
                && let Ok(Some(entry)) = entries.next_entry().await
            {
                break entry.file_name().to_string_lossy().into_owned();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("foreground run directory");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if controlled
                .initial_response
                .lock()
                .expect("initial response lock")
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("foreground initial response waiter");
    controlled.complete_initial(RuntimeResponse::Yielded {
        cell_id: CellId::new("background-cell".to_string()),
        content_items: vec![FunctionCallOutputContentItem::InputImage {
            image_url: format!("data:image/png;base64,{}", "x".repeat(20_000)),
            detail: None,
        }],
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while controlled.wait_calls.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("foreground task observes yield");
    controlled.complete_wait(RuntimeResponse::Yielded {
        cell_id: CellId::new("background-cell".to_string()),
        content_items: vec![FunctionCallOutputContentItem::InputText {
            text: "y".repeat(20_000),
        }],
    });

    let error = run
        .await
        .expect("join foreground run")
        .expect_err("aggregate workflow output must fail closed");
    assert!(
        error
            .to_string()
            .contains("workflow runtime output rejected: workflow output byte cap exceeded"),
        "unexpected output-bound error: {error}"
    );
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    assert_eq!(
        paths
            .read_meta_bounded()
            .expect("read failed foreground metadata")
            .status,
        WorkflowRunStatus::Failed
    );
    assert!(matches!(
        WorkflowRunLease::try_acquire(&paths).expect("reacquire failed-run lease"),
        WorkflowRunLeaseAcquire::Acquired(_)
    ));
    assert_eq!(controlled.terminate_calls.load(Ordering::Acquire), 1);
    service.shutdown().await.expect("shutdown service");
}

#[tokio::test]
async fn service_shutdown_cancels_detached_run_and_waits_for_terminal_cleanup() {
    let controlled = Arc::new(ControlledSession::default());
    let service = CodeModeService::new(Arc::new(ControlledSessionProvider {
        session: Arc::clone(&controlled),
    }));
    let home = tempfile::tempdir().expect("temporary codex home");

    let run_id = start_workflow_source(
        &enabled_features(),
        &service,
        "background-cancel".to_string(),
        Vec::new(),
        SOURCE,
        serde_json::Value::Null,
        WorkflowRunLineage {
            parent_run_id: None,
            depth: 0,
        },
        home.path(),
        /*resume*/ None,
        WorkflowEventTarget::Disabled,
    )
    .await
    .expect("detached workflow starts");
    let paths = WorkflowRunPaths::new(home.path(), &run_id);

    service.shutdown().await.expect("shutdown service");

    assert_eq!(controlled.terminate_calls.load(Ordering::Acquire), 1);
    assert_eq!(controlled.shutdown_calls.load(Ordering::Acquire), 1);
    let meta: WorkflowRunMeta = serde_json::from_str(
        &std::fs::read_to_string(paths.meta()).expect("read terminal workflow meta"),
    )
    .expect("parse terminal workflow meta");
    assert_eq!(meta.status, WorkflowRunStatus::Failed);
}

#[tokio::test]
async fn explicit_stop_persists_stopped_and_later_shutdown_cannot_relabel_it() {
    let controlled = Arc::new(ControlledSession::default());
    let service = CodeModeService::new(Arc::new(ControlledSessionProvider {
        session: Arc::clone(&controlled),
    }));
    let home = tempfile::tempdir().expect("temporary codex home");

    let run_id = start_workflow_source(
        &enabled_features(),
        &service,
        "background-stop".to_string(),
        Vec::new(),
        SOURCE,
        serde_json::Value::Null,
        WorkflowRunLineage {
            parent_run_id: None,
            depth: 0,
        },
        home.path(),
        /*resume*/ None,
        WorkflowEventTarget::Disabled,
    )
    .await
    .expect("detached workflow starts");
    let paths = WorkflowRunPaths::new(home.path(), &run_id);

    assert_eq!(
        service.cancel_workflow_run(&run_id).await,
        WorkflowRunCancelOutcome::Applied
    );
    assert_eq!(
        paths
            .read_meta_bounded()
            .expect("read explicitly stopped metadata")
            .status,
        WorkflowRunStatus::Stopped
    );
    assert_eq!(controlled.terminate_calls.load(Ordering::Acquire), 1);
    assert_eq!(
        service.cancel_workflow_run(&run_id).await,
        WorkflowRunCancelOutcome::NotRunning
    );

    service.shutdown().await.expect("shutdown service");
    assert_eq!(
        paths
            .read_meta_bounded()
            .expect("reread stopped metadata after shutdown")
            .status,
        WorkflowRunStatus::Stopped
    );
    assert_eq!(controlled.terminate_calls.load(Ordering::Acquire), 1);
    assert_eq!(controlled.shutdown_calls.load(Ordering::Acquire), 1);
}

#[tokio::test]
async fn explicit_pause_publishes_paused_only_after_runtime_cleanup() {
    let controlled = Arc::new(ControlledSession::default());
    let service = Arc::new(CodeModeService::new(Arc::new(ControlledSessionProvider {
        session: Arc::clone(&controlled),
    })));
    let home = tempfile::tempdir().expect("temporary codex home");
    let run_id = start_workflow_source(
        &enabled_features(),
        &service,
        "background-pause".to_string(),
        Vec::new(),
        SOURCE,
        serde_json::Value::Null,
        WorkflowRunLineage {
            parent_run_id: None,
            depth: 0,
        },
        home.path(),
        /*resume*/ None,
        WorkflowEventTarget::Disabled,
    )
    .await
    .expect("detached workflow starts");
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    let release_cleanup = controlled.block_termination();

    let pause_service = Arc::clone(&service);
    let pause_run_id = run_id.clone();
    let pause = tokio::spawn(async move { pause_service.pause_workflow_run(&pause_run_id).await });
    controlled.terminate_started.notified().await;
    assert_eq!(
        paths
            .read_meta_bounded()
            .expect("read metadata during cleanup")
            .status,
        WorkflowRunStatus::Running
    );
    assert!(!pause.is_finished());

    release_cleanup.notify_one();
    assert_eq!(
        pause.await.expect("pause task joins"),
        WorkflowRunCancelOutcome::Applied
    );
    assert_eq!(
        paths
            .read_meta_bounded()
            .expect("read paused metadata")
            .status,
        WorkflowRunStatus::Paused
    );
    assert_eq!(controlled.terminate_calls.load(Ordering::Acquire), 1);
    let Some(WorkflowRunLeaseAcquire::Acquired(released_lease)) =
        WorkflowRunLease::try_acquire_existing(&paths).expect("check released pause lease")
    else {
        panic!("pause response must wait until the run lease is released");
    };
    drop(released_lease);
    codex_workflow_journal::ReplayJournal::load(&paths.journal())
        .expect("pause response must wait until the recorder is closed and readable");
    service.shutdown().await.expect("shutdown service");
}

#[tokio::test]
async fn completed_progress_claim_prevents_stop_from_winning_the_meta_write_gap() {
    use codex_protocol::protocol::AgentStatus;
    use codex_protocol::protocol::WorkflowEvent;
    use codex_protocol::protocol::WorkflowRunBeginEvent;
    use codex_protocol::protocol::WorkflowRunEndEvent;
    use codex_protocol::protocol::WorkflowRunTerminalReason;

    let controlled = Arc::new(ControlledSession::default());
    let service = CodeModeService::new(Arc::new(ControlledSessionProvider {
        session: Arc::clone(&controlled),
    }));
    let home = tempfile::tempdir().expect("temporary codex home");
    let run_id = start_workflow_source(
        &enabled_features(),
        &service,
        "completion-stop-race".to_string(),
        Vec::new(),
        SOURCE,
        serde_json::Value::Null,
        WorkflowRunLineage {
            parent_run_id: None,
            depth: 0,
        },
        home.path(),
        /*resume*/ None,
        WorkflowEventTarget::Disabled,
    )
    .await
    .expect("detached workflow starts");
    let paths = WorkflowRunPaths::new(home.path(), &run_id);
    let cell_id = CellId::new("background-cell".to_string());

    // Deterministically model the narrow live-event window: natural
    // completion has claimed the ledger and written terminal progress, but its
    // subsequent meta.json write has not run yet.
    assert!(
        service
            .workflow_run_ledger()
            .claim_terminal(&cell_id)
            .is_some()
    );
    crate::tools::code_mode::workflow_progress::durable::record_event(
        home.path(),
        &WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: run_id.clone(),
            resumed_from_run_id: None,
            name: "background-test".to_string(),
            phases: Vec::new(),
            args_digest: "blake3:args".to_string(),
        }),
    )
    .await
    .expect("record natural run begin");
    crate::tools::code_mode::workflow_progress::durable::record_event(
        home.path(),
        &WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: run_id.clone(),
            status: AgentStatus::Completed(None),
            terminal_reason: Some(WorkflowRunTerminalReason::Completed),
            spent: 0,
            total: None,
        }),
    )
    .await
    .expect("record natural completion before metadata");
    assert_eq!(
        paths.read_meta_bounded().expect("read metadata gap").status,
        WorkflowRunStatus::Running
    );

    assert_eq!(
        service.cancel_workflow_run(&run_id).await,
        WorkflowRunCancelOutcome::NotRunning
    );
    assert_eq!(
        paths
            .read_meta_bounded()
            .expect("stop must not write in a lost terminal race")
            .status,
        WorkflowRunStatus::Running
    );

    paths
        .update_status(WorkflowRunStatus::Completed)
        .expect("natural terminal writer completes metadata update");
    let DurableProgressRead::Snapshot(progress) = read_durable_progress(home.path(), &run_id)
        .await
        .expect("read natural terminal progress")
    else {
        panic!("expected natural terminal progress");
    };
    assert_eq!(
        (
            progress.state,
            progress.status,
            paths
                .read_meta_bounded()
                .expect("read natural terminal metadata")
                .status,
        ),
        (
            DurableRunState::Terminal,
            DurableRunStatus::Completed(None),
            WorkflowRunStatus::Completed,
        )
    );
    service.shutdown().await.expect("shutdown service");
}

#[tokio::test]
async fn foreground_cancellation_remains_failed_not_stopped() {
    let controlled = Arc::new(ControlledSession::default());
    let service = Arc::new(CodeModeService::new(Arc::new(ControlledSessionProvider {
        session: Arc::clone(&controlled),
    })));
    let home = tempfile::tempdir().expect("temporary codex home");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let task_service = Arc::clone(&service);
    let task_home = home.path().to_path_buf();
    let run = tokio::spawn(async move {
        run_workflow_source_to_terminal(
            &enabled_features(),
            &task_service,
            "foreground-cancel".to_string(),
            Vec::new(),
            SOURCE,
            serde_json::Value::Null,
            WorkflowRunLineage {
                parent_run_id: None,
                depth: 0,
            },
            &task_home,
            /*resume*/ None,
            WorkflowEventTarget::Disabled,
            task_cancellation,
        )
        .await
    });

    let run_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let root = codex_workflow_journal::storage::runs_root(home.path());
            if let Ok(mut entries) = tokio::fs::read_dir(root).await
                && let Ok(Some(entry)) = entries.next_entry().await
            {
                break entry.file_name().to_string_lossy().into_owned();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("foreground run directory");
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if controlled
                .initial_response
                .lock()
                .expect("initial response lock")
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("foreground initial response waiter");

    cancellation.cancel();
    let error = run
        .await
        .expect("join foreground cancellation")
        .expect_err("foreground cancellation should interrupt the run");
    assert!(error.to_string().contains("was cancelled"));
    assert_eq!(
        WorkflowRunPaths::new(home.path(), &run_id)
            .read_meta_bounded()
            .expect("read cancelled foreground metadata")
            .status,
        WorkflowRunStatus::Failed
    );
    assert_eq!(controlled.terminate_calls.load(Ordering::Acquire), 1);
    service.shutdown().await.expect("shutdown service");
}

#[tokio::test]
async fn detached_wait_panic_runs_terminal_cleanup_before_releasing_lease() {
    let controlled = Arc::new(ControlledSession::default());
    controlled.panic_on_wait.store(true, Ordering::Release);
    let service = CodeModeService::new(Arc::new(ControlledSessionProvider {
        session: Arc::clone(&controlled),
    }));
    let home = tempfile::tempdir().expect("temporary codex home");

    let run_id = start_workflow_source(
        &enabled_features(),
        &service,
        "background-panic".to_string(),
        Vec::new(),
        SOURCE,
        serde_json::Value::Null,
        WorkflowRunLineage {
            parent_run_id: None,
            depth: 0,
        },
        home.path(),
        /*resume*/ None,
        WorkflowEventTarget::Disabled,
    )
    .await
    .expect("detached workflow starts");
    let paths = WorkflowRunPaths::new(home.path(), &run_id);

    controlled.complete_initial(RuntimeResponse::Yielded {
        cell_id: CellId::new("background-cell".to_string()),
        content_items: Vec::new(),
    });
    wait_for_status(&paths, WorkflowRunStatus::Failed).await;

    assert_eq!(controlled.terminate_calls.load(Ordering::Acquire), 1);
    let progress = read_durable_progress(home.path(), &run_id)
        .await
        .expect("read panic-cleanup progress");
    let DurableProgressRead::Snapshot(progress) = progress else {
        panic!("panic cleanup should persist a terminal progress snapshot");
    };
    assert_eq!(
        (progress.state, progress.status),
        (DurableRunState::Terminal, DurableRunStatus::Interrupted,)
    );

    let lease = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match WorkflowRunLease::try_acquire(&paths).expect("try workflow run lease") {
                WorkflowRunLeaseAcquire::Acquired(lease) => break lease,
                WorkflowRunLeaseAcquire::Held => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("panic cleanup releases workflow run lease");
    drop(lease);

    service.shutdown().await.expect("shutdown service");
    assert_eq!(controlled.shutdown_calls.load(Ordering::Acquire), 1);
}
