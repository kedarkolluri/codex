use super::drain_buffered_final_message;
use super::format_ordinal_nickname;
use super::workflow_agent_nickname_preference;
use crate::ThreadManager;
use crate::agent::control::SpawnAgentOptions;
use crate::agent::control::spawn::default_agent_nickname_list;
use crate::agent::control::spawn_await_opts::SpawnAgentConfigOverrides;
use crate::agent::control::spawn_await_opts::map_workflow_effort;
use crate::agent::control::spawn_workspace::SpawnAgentWorkspace;
use crate::agent::control::workflow_child_progress::WorkflowChildEvent;
use crate::agent::control::workflow_child_progress::WorkflowChildObserver;
use crate::agent::registry::AgentRegistry;
use crate::agent::registry::next_thread_spawn_depth;
use crate::config::Config;
use crate::config::test_config;
use crate::init_state_db;
use crate::local_agent_graph_store_from_state_db;
use crate::session::turn_context::TurnContext;
use crate::thread_store_from_config;
use crate::tools::handlers::multi_agents::build_agent_spawn_config;
use codex_agent_graph_store::ThreadSpawnEdgeStatus;
use codex_extension_api::empty_extension_registry;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::ThreadId;
use codex_protocol::models::BaseInstructions;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_workflow_journal::JournalRecorder;
use codex_workflow_journal::WorkflowRunMeta;
use codex_workflow_journal::storage::WorkflowRunPaths;
use codex_workflow_journal::storage::mint_run_id;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::ev_shell_command_call;
use core_test_support::responses::mount_response_once;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use wiremock::MockServer;

const CHILD_FINAL_MESSAGE: &str = "child final answer";
const FOREIGN_MESSAGE: &str = "foreign turn answer";
const TEST_INSTALLATION_ID: &str = "11111111-1111-4111-8111-111111111111";

/// Builds a real (non-op-capturing) in-crate manager whose model provider points at `server`, so
/// spawned children run live turns and every `pub(crate)` internal stays in this crate instance.
async fn build_manager(config: &Config) -> ThreadManager {
    let state_db = init_state_db(config).await;
    ThreadManager::new(
        config,
        AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy")),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        thread_store_from_config(config, state_db.clone()),
        local_agent_graph_store_from_state_db(state_db.as_ref()),
        TEST_INSTALLATION_ID.to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    )
}

async fn test_config_for_server(
    codex_home: &TempDir,
    server: &MockServer,
) -> anyhow::Result<Config> {
    let home = AbsolutePathBuf::from_absolute_path(codex_home.path())?;
    let mut config = test_config().await;
    config.codex_home = home.clone();
    config.cwd = home;
    config.sqlite_home = codex_home.path().to_path_buf();
    config.model_provider.base_url = Some(server.uri());
    Ok(config)
}

/// Registers a parent thread and returns the pieces a subagent spawn needs to inherit its context.
async fn parent_spawn_context(
    manager: &ThreadManager,
    config: &Config,
) -> anyhow::Result<(ThreadId, BaseInstructions, Arc<TurnContext>)> {
    let parent = manager.start_thread(config.clone()).await?;
    let base_instructions = parent.thread.codex.session.get_base_instructions().await;
    let parent_turn = parent.thread.codex.session.new_default_turn().await;
    Ok((parent.thread_id, base_instructions, parent_turn))
}

/// End-to-end proof that the spawn-and-await helper drives a subagent to completion through the
/// **registering** spawn path: the child's final assistant message is returned, and every
/// registering-path side effect that the workflow observability features depend on fires —
/// `notify_thread_created`, an `agent-graph-store` spawn edge, a per-thread rollout file, and
/// registration in `thread_manager.threads`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_and_await_final_message_uses_registering_path() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    // Single mounted turn: the child produces one assistant message, then completes.
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-child"),
            ev_assistant_message("msg-child", CHILD_FINAL_MESSAGE),
            ev_completed("resp-child"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;

    let (parent_thread_id, base_instructions, parent_turn) =
        parent_spawn_context(&manager, &config).await?;
    let agent_control = manager.agent_control();

    // Subscribe before spawning so the child's `notify_thread_created` broadcast is observable.
    let mut thread_created_rx = manager.subscribe_thread_created();

    let options = SpawnAgentOptions {
        environments: Some(parent_turn.environments.to_selections()),
        ..Default::default()
    };

    let final_message = timeout(
        Duration::from_secs(30),
        agent_control.spawn_and_await_final_message(
            &base_instructions,
            parent_turn.as_ref(),
            parent_thread_id,
            vec![UserInput::Text {
                text: "run the child".to_string(),
                text_elements: Vec::new(),
            }],
            /*final_output_json_schema*/ None,
            SpawnAgentConfigOverrides::default(),
            options,
        ),
    )
    .await
    .expect("helper should finish before timeout");

    // Helper returns the child's final assistant message on `TurnComplete`.
    assert_eq!(final_message.as_deref(), Some(CHILD_FINAL_MESSAGE));

    // `notify_thread_created` fired: the child id was broadcast on the aggregate feed.
    let child_thread_id = thread_created_rx
        .try_recv()
        .expect("notify_thread_created should have broadcast the child thread id");

    // The child is registered in `thread_manager.threads`.
    let child_thread = manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered in thread_manager.threads");

    // An `agent-graph-store` spawn edge parent -> child was persisted.
    let state = agent_control
        .upgrade()
        .expect("thread manager state should still be alive");
    let agent_graph_store = state
        .agent_graph_store()
        .expect("manager should have an agent-graph-store");
    let persisted_children = agent_graph_store
        .list_thread_spawn_children(parent_thread_id, Some(ThreadSpawnEdgeStatus::Open))
        .await
        .expect("spawn-edge children should load");
    assert!(
        persisted_children.contains(&child_thread_id),
        "expected a persisted spawn edge {parent_thread_id} -> {child_thread_id}, got {persisted_children:?}"
    );

    // The child persisted its own `rollout-<date>-<thread_id>.jsonl`.
    child_thread.ensure_rollout_materialized().await;
    child_thread
        .flush_rollout()
        .await
        .expect("child rollout should flush");
    let rollout_path = child_thread
        .rollout_path()
        .expect("child thread should have a rollout path");
    assert!(
        rollout_path.exists(),
        "expected child rollout file at {rollout_path:?}"
    );
    let rollout_file_name = rollout_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    assert!(
        rollout_file_name.contains(&child_thread_id.to_string()),
        "child rollout file {rollout_file_name} should be keyed by the child thread id {child_thread_id}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellable_spawn_explicitly_reaps_child_before_returning() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let expected_usage = codex_protocol::protocol::TokenUsage {
        input_tokens: 11,
        cached_input_tokens: 0,
        output_tokens: 7,
        reasoning_output_tokens: 0,
        total_tokens: 18,
    };
    let long_running_command = if cfg!(windows) {
        "ping -n 31 127.0.0.1 >NUL"
    } else {
        "sleep 30"
    };
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-cancelled"),
            ev_shell_command_call("call-cancelled", long_running_command),
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": "resp-cancelled",
                    "usage": {
                        "input_tokens": 11,
                        "input_tokens_details": null,
                        "output_tokens": 7,
                        "output_tokens_details": null,
                        "total_tokens": 18
                    }
                }
            }),
        ]),
    )
    .await;
    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;
    let (parent_thread_id, base_instructions, parent_turn) =
        parent_spawn_context(&manager, &config).await?;
    let child_config = build_agent_spawn_config(&base_instructions, parent_turn.as_ref())
        .expect("build child config");
    let agent_control = manager.agent_control();
    let mut thread_created_rx = manager.subscribe_thread_created();
    let cancellation_token = CancellationToken::new();
    let spawn = agent_control.spawn_and_await_journaled_with_config_cancellable(
        child_config,
        parent_turn.as_ref(),
        parent_thread_id,
        vec![UserInput::Text {
            text: "run until cancelled".to_string(),
            text_elements: Vec::new(),
        }],
        /*final_output_json_schema*/ None,
        SpawnAgentOptions {
            environments: Some(parent_turn.environments.to_selections()),
            ..Default::default()
        },
        /*observer*/ None,
        cancellation_token.clone(),
    );
    tokio::pin!(spawn);
    let child_thread_id = tokio::select! {
        child_thread_id = thread_created_rx.recv() => child_thread_id.expect("child registered"),
        outcome = &mut spawn => panic!("child finished before cancellation: {outcome:?}"),
    };
    let child = manager
        .get_thread(child_thread_id)
        .await
        .expect("hold the registered child through cancellation");
    let persisted_facts = timeout(Duration::from_secs(10), async {
        loop {
            tokio::select! {
                outcome = &mut spawn => {
                    panic!("child finished before cancellation: {outcome:?}")
                }
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
            let facts = super::workflow_child_progress::child_journal_facts(
                &agent_control,
                child_thread_id,
            )
            .await;
            if facts.token_usage == expected_usage && facts.tool_call_count == 1 {
                break facts;
            }
        }
    })
    .await
    .expect("child should persist usage and tool facts before cancellation");
    assert!(persisted_facts.rollout_path.is_some());

    cancellation_token.cancel();
    let outcome = timeout(Duration::from_secs(10), &mut spawn)
        .await
        .expect("cancellable helper should reap promptly");

    assert!(outcome.cancelled);
    assert_eq!(outcome.child_thread_id, Some(child_thread_id));
    assert_eq!(outcome.tokens_spent, Some(7));
    assert_eq!(outcome.token_usage, expected_usage);
    assert_eq!(outcome.tool_call_count, 1);
    let rollout_path = outcome
        .rollout_path
        .as_deref()
        .expect("cancelled outcome should retain its materialized rollout path");
    assert!(rollout_path.exists());
    timeout(Duration::from_secs(10), child.wait_until_terminated())
        .await
        .expect("reaping must await child session shutdown");
    assert!(manager.get_thread(child_thread_id).await.is_err());
    assert!(agent_control.get_agent_metadata(child_thread_id).is_none());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn isolated_workspace_child_is_shutdown_before_success_returns() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-isolated"),
            ev_assistant_message("msg-isolated", CHILD_FINAL_MESSAGE),
            ev_completed("resp-isolated"),
        ]),
    )
    .await;
    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let worktree =
        AbsolutePathBuf::from_absolute_path_checked(codex_home.path().join("isolated-worktree"))?;
    let git_dir =
        AbsolutePathBuf::from_absolute_path_checked(codex_home.path().join("isolated-git-dir"))?;
    std::fs::create_dir(&worktree)?;
    let manager = build_manager(&config).await;
    let (parent_thread_id, base_instructions, parent_turn) =
        parent_spawn_context(&manager, &config).await?;
    let child_config = build_agent_spawn_config(&base_instructions, parent_turn.as_ref())
        .expect("build child config");
    let agent_control = manager.agent_control();
    let mut thread_created_rx = manager.subscribe_thread_created();
    let spawn = agent_control.spawn_and_await_journaled_with_config_cancellable(
        child_config,
        parent_turn.as_ref(),
        parent_thread_id,
        vec![UserInput::Text {
            text: "complete in the isolated checkout".to_string(),
            text_elements: Vec::new(),
        }],
        /*final_output_json_schema*/ None,
        SpawnAgentOptions {
            environments: Some(parent_turn.environments.to_selections()),
            spawn_workspace: Some(SpawnAgentWorkspace::isolated_worktree(
                worktree,
                git_dir,
                parent_turn.config.permissions.clone(),
            )?),
            ..Default::default()
        },
        /*observer*/ None,
        CancellationToken::new(),
    );
    tokio::pin!(spawn);
    let child_thread_id = tokio::select! {
        biased;
        child_thread_id = thread_created_rx.recv() => child_thread_id.expect("child registered"),
        outcome = &mut spawn => panic!("isolated child finished before registration was observed: {outcome:?}"),
    };
    let child = manager
        .get_thread(child_thread_id)
        .await
        .expect("hold isolated child while its turn completes");

    let outcome = timeout(Duration::from_secs(10), &mut spawn)
        .await
        .expect("isolated child should complete promptly");

    assert_eq!(outcome.final_text.as_deref(), Some(CHILD_FINAL_MESSAGE));
    assert!(!outcome.cancelled);
    timeout(Duration::from_secs(10), child.wait_until_terminated())
        .await
        .expect("isolated child session must stop before its workspace owner resumes");
    assert!(manager.get_thread(child_thread_id).await.is_err());
    assert!(agent_control.get_agent_metadata(child_thread_id).is_none());
    Ok(())
}

/// A deferred workflow child cannot submit its first turn until the host has published and
/// acknowledged the Begin -> Bound topology prefix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_child_waits_for_binding_ack_before_first_turn() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-bound"),
            ev_assistant_message("msg-bound", CHILD_FINAL_MESSAGE),
            ev_completed("resp-bound"),
        ]),
    )
    .await;
    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;
    let (parent_thread_id, base_instructions, parent_turn) =
        parent_spawn_context(&manager, &config).await?;
    let child_config = build_agent_spawn_config(&base_instructions, parent_turn.as_ref())
        .expect("build child config");
    let agent_control = manager.agent_control();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let observer = WorkflowChildObserver::new(event_tx);
    let spawn = agent_control.spawn_and_await_journaled_with_config(
        child_config,
        parent_turn.as_ref(),
        parent_thread_id,
        vec![UserInput::Text {
            text: "run only after binding".to_string(),
            text_elements: Vec::new(),
        }],
        /*final_output_json_schema*/ None,
        SpawnAgentOptions {
            environments: Some(parent_turn.environments.to_selections()),
            ..Default::default()
        },
        Some(observer),
    );
    tokio::pin!(spawn);

    let (child_thread_id, acknowledged) = tokio::select! {
        event = event_rx.recv() => match event.expect("binding event") {
            WorkflowChildEvent::Bound {
                child_thread_id,
                acknowledged,
            } => (child_thread_id, acknowledged),
            WorkflowChildEvent::Progress(progress) => {
                panic!("progress preceded workflow child binding: {progress:?}")
            }
        },
        outcome = &mut spawn => panic!("spawn finished before binding acknowledgment: {outcome:?}"),
    };

    assert!(
        server
            .received_requests()
            .await
            .expect("mock server request log")
            .is_empty(),
        "the child must not issue its first model request before Bound is acknowledged"
    );
    let child_thread = manager.get_thread(child_thread_id).await?;
    assert!(
        child_thread
            .codex
            .session
            .active_turn
            .lock()
            .await
            .is_none()
    );

    acknowledged
        .send(Ok(()))
        .expect("spawn should still be awaiting the binding acknowledgment");
    let outcome = timeout(Duration::from_secs(30), &mut spawn)
        .await
        .expect("spawn should finish after binding is acknowledged");
    assert_eq!(outcome.final_text.as_deref(), Some(CHILD_FINAL_MESSAGE));
    assert_eq!(outcome.child_thread_id, Some(child_thread_id));
    Ok(())
}

/// A configured journal is a fail-closed durability boundary: if its binding append fails, the
/// deferred child is reaped and never starts a turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_child_journal_failure_prevents_first_turn() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;
    let (parent_thread_id, base_instructions, parent_turn) =
        parent_spawn_context(&manager, &config).await?;
    let child_config = build_agent_spawn_config(&base_instructions, parent_turn.as_ref())
        .expect("build child config");
    let agent_control = manager.agent_control();
    let mut thread_created_rx = manager.subscribe_thread_created();

    let run_id = mint_run_id();
    let paths = WorkflowRunPaths::new(codex_home.path(), &run_id);
    let meta = WorkflowRunMeta::new(
        run_id,
        /*parent_run_id*/ None,
        "blake3:script".to_string(),
        "blake3:args".to_string(),
        "binding failure".to_string(),
        Some(1_000),
        codex_workflow_journal::KEY_ALGO_VERSION,
        "2026-07-18T00:00:00Z".to_string(),
    );
    let recorder = Arc::new(JournalRecorder::new(&paths, &meta).await?);
    recorder.shutdown().await?;
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let observer = WorkflowChildObserver::new(event_tx).with_binding_journal(recorder, 7, 0);

    let outcome = timeout(
        Duration::from_secs(30),
        agent_control.spawn_and_await_journaled_with_config(
            child_config,
            parent_turn.as_ref(),
            parent_thread_id,
            vec![UserInput::Text {
                text: "must never run".to_string(),
                text_elements: Vec::new(),
            }],
            /*final_output_json_schema*/ None,
            SpawnAgentOptions {
                environments: Some(parent_turn.environments.to_selections()),
                ..Default::default()
            },
            Some(observer),
        ),
    )
    .await
    .expect("journal failure should resolve promptly");

    assert!(outcome.final_text.is_none());
    assert!(outcome.child_thread_id.is_none());
    assert!(!outcome.cancelled);
    let requests = server
        .received_requests()
        .await
        .expect("mock server request log");
    let turn_requests = requests
        .iter()
        .filter(|request| request.method.as_str() == "POST" && request.url.path() == "/responses")
        .collect::<Vec<_>>();
    assert!(
        turn_requests.is_empty(),
        "a child whose binding was not durable must never issue a first-turn request: \
         {turn_requests:#?}"
    );
    let child_thread_id = thread_created_rx
        .try_recv()
        .expect("the deferred child should have registered before binding failed");
    assert!(manager.get_thread(child_thread_id).await.is_err());
    assert!(agent_control.get_agent_metadata(child_thread_id).is_none());
    assert!(event_rx.try_recv().is_err(), "Bound must not be announced");
    Ok(())
}

/// Structured output (spec §6): an `opts.schema` threaded into `final_output_json_schema` reaches the
/// child's turn as a strict `output_schema`, so the child's model request carries
/// `text.format.schema` == the requested schema with `strict = true`. This proves the
/// `opts.schema -> final_output_json_schema -> output_schema_strict` plumbing end-to-end (the parse +
/// `jsonschema` recheck on the *return* is unit-tested in `code_mode::delegate::finalize_agent_output`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_and_await_final_message_threads_schema_into_child_turn() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-child"),
            ev_assistant_message("msg-child", r#"{"answer":"ok"}"#),
            ev_completed("resp-child"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;

    let (parent_thread_id, base_instructions, parent_turn) =
        parent_spawn_context(&manager, &config).await?;
    let agent_control = manager.agent_control();

    let schema = serde_json::json!({
        "type": "object",
        "properties": { "answer": { "type": "string" } },
        "required": ["answer"],
        "additionalProperties": false,
    });

    let options = SpawnAgentOptions {
        environments: Some(parent_turn.environments.to_selections()),
        ..Default::default()
    };

    let final_message = timeout(
        Duration::from_secs(30),
        agent_control.spawn_and_await_final_message(
            &base_instructions,
            parent_turn.as_ref(),
            parent_thread_id,
            vec![UserInput::Text {
                text: "run the child".to_string(),
                text_elements: Vec::new(),
            }],
            Some(schema.clone()),
            SpawnAgentConfigOverrides::default(),
            options,
        ),
    )
    .await
    .expect("helper should finish before timeout");

    // The helper still returns the raw final text; the caller does the parse + recheck.
    assert_eq!(final_message.as_deref(), Some(r#"{"answer":"ok"}"#));

    // The child's turn request carried the schema as a strict Responses-API `text.format`.
    let requests = server
        .received_requests()
        .await
        .expect("mock server should record requests");
    let child_request = requests
        .last()
        .expect("the child turn should have issued a model request");
    let body: serde_json::Value = child_request.body_json().expect("request body is JSON");
    let format = body
        .pointer("/text/format")
        .expect("child request should carry text.format for the output schema");
    assert_eq!(
        format.get("type"),
        Some(&serde_json::Value::String("json_schema".into())),
    );
    assert_eq!(format.get("strict"), Some(&serde_json::Value::Bool(true)));
    assert_eq!(format.get("schema"), Some(&schema));

    Ok(())
}

/// A spawn failure resolves to `None` (never a panic/throw): with no agent thread slots available,
/// the registering spawn path fails before creating a child, and the helper reports `None` without
/// broadcasting `notify_thread_created`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_and_await_final_message_returns_none_on_spawn_error() -> anyhow::Result<()> {
    let server = start_mock_server().await;

    let codex_home = TempDir::new()?;
    let mut config = test_config_for_server(&codex_home, &server).await?;
    // Zero available agent-thread slots forces `reserve_spawn_slot` to fail, so the child is never
    // created and no model turn runs.
    config.agent_max_threads = Some(0);
    let manager = build_manager(&config).await;

    let (parent_thread_id, base_instructions, parent_turn) =
        parent_spawn_context(&manager, &config).await?;
    let agent_control = manager.agent_control();
    let mut thread_created_rx = manager.subscribe_thread_created();

    let options = SpawnAgentOptions {
        environments: Some(parent_turn.environments.to_selections()),
        ..Default::default()
    };

    let final_message = timeout(
        Duration::from_secs(30),
        agent_control.spawn_and_await_final_message(
            &base_instructions,
            parent_turn.as_ref(),
            parent_thread_id,
            vec![UserInput::Text {
                text: "run the child".to_string(),
                text_elements: Vec::new(),
            }],
            /*final_output_json_schema*/ None,
            SpawnAgentConfigOverrides::default(),
            options,
        ),
    )
    .await
    .expect("helper should finish before timeout");

    assert_eq!(final_message, None, "a spawn error must resolve to None");
    assert!(
        thread_created_rx.try_recv().is_err(),
        "a failed spawn must not broadcast notify_thread_created"
    );

    Ok(())
}

/// An interrupted child turn resolves to `None`: while the child's (delayed) turn is in flight, a
/// concurrent `Op::Interrupt` forces `TurnAborted`, and the helper observes that terminal event on
/// its event tap and returns `None` rather than the (never-delivered) final message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_and_await_final_message_returns_none_on_turn_abort() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    // Delay the child's model response so its turn stays in flight long enough to interrupt.
    mount_response_once(
        &server,
        sse_response(sse(vec![
            ev_response_created("resp-child"),
            ev_assistant_message("msg-child", CHILD_FINAL_MESSAGE),
            ev_completed("resp-child"),
        ]))
        .set_delay(Duration::from_secs(30)),
    )
    .await;

    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;

    let (parent_thread_id, base_instructions, parent_turn) =
        parent_spawn_context(&manager, &config).await?;
    let agent_control = manager.agent_control();

    // Interrupter: as soon as the child registers and its turn goes active, abort it.
    let interrupter_control = agent_control.clone();
    let mut thread_created_rx = manager.subscribe_thread_created();
    let interrupter = tokio::spawn(async move {
        let Ok(child_thread_id) = thread_created_rx.recv().await else {
            return;
        };
        let state = interrupter_control
            .upgrade()
            .expect("thread manager state should be alive");
        let child_thread = state
            .get_thread(child_thread_id)
            .await
            .expect("child thread should be registered");
        // Wait until the child's turn is actually running before interrupting, so the interrupt
        // hits an active turn and deterministically produces `TurnAborted`.
        loop {
            if child_thread
                .codex
                .session
                .active_turn
                .lock()
                .await
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _ = child_thread.submit(Op::Interrupt).await;
    });

    let options = SpawnAgentOptions {
        environments: Some(parent_turn.environments.to_selections()),
        ..Default::default()
    };

    let final_message = timeout(
        Duration::from_secs(30),
        agent_control.spawn_and_await_final_message(
            &base_instructions,
            parent_turn.as_ref(),
            parent_thread_id,
            vec![UserInput::Text {
                text: "run the child".to_string(),
                text_elements: Vec::new(),
            }],
            /*final_output_json_schema*/ None,
            SpawnAgentConfigOverrides::default(),
            options,
        ),
    )
    .await
    .expect("helper should finish before timeout");

    assert_eq!(final_message, None, "an aborted turn must resolve to None");
    interrupter.abort();

    Ok(())
}

/// Race regression: the helper must observe the child's terminal event through its **non-competing**
/// event tap even while a competing consumer continuously drains the child's `next_event()` — the
/// exact shape of the production app-server listener that auto-attaches to every registered thread.
///
/// Before the tap fix, the helper drained `next_event()` directly (a work-stealing `async_channel`
/// receiver), so the competitor could steal `TurnComplete` and the helper would hang forever. With
/// the tap, both consumers see the terminal event independently and the helper still returns the
/// child's final message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_and_await_final_message_survives_competing_next_event_drain() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-child"),
            ev_assistant_message("msg-child", CHILD_FINAL_MESSAGE),
            ev_completed("resp-child"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;

    let (parent_thread_id, base_instructions, parent_turn) =
        parent_spawn_context(&manager, &config).await?;
    let agent_control = manager.agent_control();

    // Competing drain: mirrors the app-server listener that steals from the child's `rx_event`.
    //
    // Two handshakes make the contention observable and deterministic (no sleeps):
    // - `entered_loop_tx` fires the instant the competitor holds the child and is entering its
    //   drain loop, proving it is positioned to steal from the work-stealing `rx_event` receiver
    //   before we assert anything about it;
    // - `first_event_tx` fires on the competitor's first stolen event, and `drained` counts every
    //   stolen event, so the test can prove the competitor actually consumed `rx_event` traffic
    //   rather than idling.
    let competitor_control = agent_control.clone();
    let mut thread_created_rx = manager.subscribe_thread_created();
    let drained = Arc::new(AtomicUsize::new(0));
    let competitor_drained = Arc::clone(&drained);
    let (entered_loop_tx, entered_loop_rx) = oneshot::channel::<()>();
    let (first_event_tx, first_event_rx) = oneshot::channel::<()>();
    let competitor = tokio::spawn(async move {
        let Ok(child_thread_id) = thread_created_rx.recv().await else {
            return;
        };
        let state = competitor_control
            .upgrade()
            .expect("thread manager state should be alive");
        let child_thread = state
            .get_thread(child_thread_id)
            .await
            .expect("child thread should be registered");
        // Handshake: the competitor holds the child and is about to contend for `rx_event`.
        let _ = entered_loop_tx.send(());
        let mut first_event_tx = Some(first_event_tx);
        // Continuously steal every event from the work-stealing `rx_event` receiver.
        while child_thread.next_event().await.is_ok() {
            competitor_drained.fetch_add(1, Ordering::SeqCst);
            if let Some(first_event_tx) = first_event_tx.take() {
                let _ = first_event_tx.send(());
            }
        }
    });

    // NOTE: the competitor can only obtain the child once the helper's spawn fires
    // `notify_thread_created`, which happens *inside* `spawn_and_await_final_message`. The child
    // therefore does not exist until the helper runs, so the readiness handshake is verified after
    // the helper returns rather than gating the submit (which the helper performs internally).
    let options = SpawnAgentOptions {
        environments: Some(parent_turn.environments.to_selections()),
        ..Default::default()
    };

    let final_message = timeout(
        Duration::from_secs(30),
        agent_control.spawn_and_await_final_message(
            &base_instructions,
            parent_turn.as_ref(),
            parent_thread_id,
            vec![UserInput::Text {
                text: "run the child".to_string(),
                text_elements: Vec::new(),
            }],
            /*final_output_json_schema*/ None,
            SpawnAgentConfigOverrides::default(),
            options,
        ),
    )
    .await
    .expect("helper should finish before timeout despite the competing drain");

    // The tap delivered `TurnComplete` even though the competitor drained `rx_event`.
    assert_eq!(final_message.as_deref(), Some(CHILD_FINAL_MESSAGE));

    // The competitor was live and contending: it obtained the child and entered its drain loop.
    entered_loop_rx
        .await
        .expect("competitor should obtain the child and enter its drain loop");
    // The competitor provably consumed the contended `rx_event` traffic: at least one event was
    // stolen by the work-stealing receiver while the helper still recovered the final message from
    // its independent tap.
    timeout(Duration::from_secs(30), first_event_rx)
        .await
        .expect("competitor should steal at least one event before the outer timeout")
        .expect("first-event handshake sender should not be dropped");
    assert!(
        drained.load(Ordering::SeqCst) >= 1,
        "competitor must have drained at least one event from rx_event"
    );
    competitor.abort();

    Ok(())
}

/// Teardown no-hang regression: while the child's (delayed) turn is in flight, its session is torn
/// down. The helper holds an `Arc<CodexThread>` for the child, so the broadcast tap's sender never
/// drops and `RecvError::Closed` never fires — without a deterministic secondary wake the helper
/// would block forever. The session-loop termination future must wake it so it resolves to `None`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_and_await_final_message_returns_none_on_session_teardown() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    // Delay the child's model response so its turn stays in flight while we tear the session down;
    // the turn's own terminal event therefore never reaches the helper on the tap.
    mount_response_once(
        &server,
        sse_response(sse(vec![
            ev_response_created("resp-child"),
            ev_assistant_message("msg-child", CHILD_FINAL_MESSAGE),
            ev_completed("resp-child"),
        ]))
        .set_delay(Duration::from_secs(30)),
    )
    .await;

    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;

    let (parent_thread_id, base_instructions, parent_turn) =
        parent_spawn_context(&manager, &config).await?;
    let agent_control = manager.agent_control();

    // Teardown driver: once the child's turn is active, shut its session loop down. A retained
    // `Arc` (the helper's) keeps the child alive, so the loop-termination future — not a closed
    // tap — is what must wake the helper.
    let teardown_control = agent_control.clone();
    let mut thread_created_rx = manager.subscribe_thread_created();
    let teardown = tokio::spawn(async move {
        let Ok(child_thread_id) = thread_created_rx.recv().await else {
            return;
        };
        let state = teardown_control
            .upgrade()
            .expect("thread manager state should be alive");
        let child_thread = state
            .get_thread(child_thread_id)
            .await
            .expect("child thread should be registered");
        loop {
            if child_thread
                .codex
                .session
                .active_turn
                .lock()
                .await
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let _ = child_thread.shutdown_and_wait().await;
    });

    let options = SpawnAgentOptions {
        environments: Some(parent_turn.environments.to_selections()),
        ..Default::default()
    };

    let final_message = timeout(
        Duration::from_secs(30),
        agent_control.spawn_and_await_final_message(
            &base_instructions,
            parent_turn.as_ref(),
            parent_thread_id,
            vec![UserInput::Text {
                text: "run the child".to_string(),
                text_elements: Vec::new(),
            }],
            /*final_output_json_schema*/ None,
            SpawnAgentConfigOverrides::default(),
            options,
        ),
    )
    .await
    .expect("helper must not hang when the child session is torn down");

    assert_eq!(
        final_message, None,
        "a torn-down child session must resolve to None"
    );
    teardown.abort();

    Ok(())
}

/// Spawns a subagent through the same **registering, deferred** path that
/// `spawn_and_await_final_message` uses, but returns before the prompt is submitted so a test can
/// drive the two phases (announce, then submit) manually.
async fn spawn_deferred_child(
    manager: &ThreadManager,
    base_instructions: &BaseInstructions,
    parent_turn: &TurnContext,
    parent_thread_id: ThreadId,
) -> anyhow::Result<ThreadId> {
    let agent_control = manager.agent_control();
    let child_config = build_agent_spawn_config(base_instructions, parent_turn)
        .expect("child spawn config should build");
    let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: next_thread_spawn_depth(&parent_turn.session_source),
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    let options = SpawnAgentOptions {
        parent_thread_id: Some(parent_thread_id),
        environments: Some(parent_turn.environments.to_selections()),
        ..Default::default()
    };
    let spawned = agent_control
        .spawn_agent_deferred_input(child_config, Some(session_source), options)
        .await?;
    Ok(spawned.thread_id)
}

/// Submits a `UserInput` turn to `child_thread` and waits until it is actually the active turn.
async fn submit_and_await_active_turn(
    child_thread: &Arc<crate::CodexThread>,
    text: &str,
) -> anyhow::Result<String> {
    let submission_id = child_thread
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    timeout(Duration::from_secs(30), async {
        loop {
            if child_thread
                .codex
                .session
                .active_turn
                .lock()
                .await
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("submitted turn should become active before timeout");
    Ok(submission_id)
}

/// Foreign-steering regression (P1): a client races a turn onto the freshly-announced child during
/// the announce->submit window. The public `Op::UserInput` path would *steer* the helper's prompt
/// into that already-active foreign turn, whose terminal event carries the foreign id — not ours —
/// so filtering on our submission id would drop every terminal event (hanging without a lag, or
/// surfacing the foreign turn's message from the child's aggregate status after a lag). The helper
/// must instead detect the active foreign turn and resolve to `None`: never the foreign message,
/// never a hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn await_first_turn_returns_none_when_a_foreign_turn_is_active() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    // The foreign turn's response is delayed so its turn stays active while we drive the helper.
    mount_response_once(
        &server,
        sse_response(sse(vec![
            ev_response_created("resp-foreign"),
            ev_assistant_message("msg-foreign", FOREIGN_MESSAGE),
            ev_completed("resp-foreign"),
        ]))
        .set_delay(Duration::from_secs(30)),
    )
    .await;

    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;

    let (parent_thread_id, base_instructions, parent_turn) =
        parent_spawn_context(&manager, &config).await?;
    let agent_control = manager.agent_control();

    let child_thread_id = spawn_deferred_child(
        &manager,
        &base_instructions,
        parent_turn.as_ref(),
        parent_thread_id,
    )
    .await?;

    // A client races a turn onto the freshly-announced child before the helper submits its prompt.
    let state = agent_control.upgrade().expect("state should be alive");
    let child_thread = state.get_thread(child_thread_id).await?;
    submit_and_await_active_turn(&child_thread, "foreign prompt").await?;

    // The helper must refuse to steer our prompt into the foreign turn and report None rather than
    // hang on an id that will never arrive or surface the foreign turn's message.
    let result = timeout(
        Duration::from_secs(30),
        agent_control.await_first_turn_final_message(
            child_thread_id,
            vec![UserInput::Text {
                text: "our prompt".to_string(),
                text_elements: Vec::new(),
            }],
            /*final_output_json_schema*/ None,
            /*observer*/ None,
        ),
    )
    .await
    .expect("helper must not hang when a foreign turn is active");

    assert_eq!(
        result, None,
        "a prompt that would be steered into a foreign turn must resolve to None, never the \
         foreign turn's message"
    );

    Ok(())
}

fn turn_complete_event(last_agent_message: Option<&str>) -> TurnCompleteEvent {
    TurnCompleteEvent {
        turn_id: "ours".to_string(),
        last_agent_message: last_agent_message.map(str::to_string),
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }
}

/// Teardown-race regression (P1 #2): `drain_buffered_final_message` recovers our turn's terminal
/// outcome from events already buffered on the tap, skipping foreign-turn events and stopping at
/// the buffer end. This backs the biased-select drain that keeps a `TurnComplete` racing session
/// teardown from being dropped in favor of `None`.
#[test]
fn drain_buffered_final_message_recovers_our_turn_message() {
    let (tx, mut rx) = tokio::sync::broadcast::channel::<Event>(16);

    // Nothing buffered: the teardown really did happen without our terminal event.
    assert_eq!(drain_buffered_final_message(&mut rx, "ours"), None);

    // A foreign turn's terminal event, then ours, are both already buffered when teardown fires.
    tx.send(Event {
        id: "foreign".to_string(),
        msg: EventMsg::TurnComplete(turn_complete_event(Some(FOREIGN_MESSAGE))),
    })
    .expect("send foreign terminal event");
    tx.send(Event {
        id: "ours".to_string(),
        msg: EventMsg::TurnComplete(turn_complete_event(Some(CHILD_FINAL_MESSAGE))),
    })
    .expect("send our terminal event");

    assert_eq!(
        drain_buffered_final_message(&mut rx, "ours").as_deref(),
        Some(CHILD_FINAL_MESSAGE),
        "our buffered TurnComplete must win over a foreign turn's event, not be dropped for None"
    );

    // A buffered abort for our turn resolves to None.
    tx.send(Event {
        id: "ours".to_string(),
        msg: EventMsg::TurnAborted(TurnAbortedEvent {
            turn_id: Some("ours".to_string()),
            reason: TurnAbortReason::Interrupted,
            started_at: None,
            completed_at: None,
            duration_ms: None,
        }),
    })
    .expect("send our abort event");
    assert_eq!(drain_buffered_final_message(&mut rx, "ours"), None);
}

/// Registers a parent thread and returns the pieces a config-override test drives directly: the
/// parent thread (whose session owns the `ModelsManager` used to resolve `opts.model`), the parent
/// turn, and a freshly-built child config — exactly the config `spawn_and_await_final_message`
/// applies overrides on top of before spawning.
async fn override_test_fixture(
    manager: &ThreadManager,
    config: &Config,
) -> anyhow::Result<(Arc<crate::CodexThread>, Arc<TurnContext>, Config)> {
    let (parent_thread_id, base_instructions, parent_turn) =
        parent_spawn_context(manager, config).await?;
    let parent_thread = manager.get_thread(parent_thread_id).await?;
    let child_config = build_agent_spawn_config(&base_instructions, parent_turn.as_ref())
        .expect("child spawn config should build");
    Ok((parent_thread, parent_turn, child_config))
}

/// `opts.model` sets the child model and `opts.effort` sets the child `ReasoningEffort` when both
/// are requested and supported by the resolved model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_config_applies_requested_model_and_effort() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;
    let (parent_thread, parent_turn, mut child_config) =
        override_test_fixture(&manager, &config).await?;

    SpawnAgentConfigOverrides {
        model: Some("gpt-5.4".to_string()),
        effort: Some("high".to_string()),
        agent_type: None,
    }
    .apply(
        &parent_thread.codex.session,
        parent_turn.as_ref(),
        &mut child_config,
    )
    .await
    .expect("a supported model/effort override should apply");

    assert_eq!(
        child_config.model.as_deref(),
        Some("gpt-5.4"),
        "opts.model must set the child model"
    );
    assert_eq!(
        child_config.model_reasoning_effort,
        Some(ReasoningEffort::High),
        "opts.effort must set the child reasoning effort"
    );

    Ok(())
}

/// An `opts.effort`-only request (no `opts.model`) sets the child reasoning effort while inheriting
/// the parent turn's model, and is validated against that inherited model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_config_effort_only_override_inherits_model() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;
    let (parent_thread, parent_turn, mut child_config) =
        override_test_fixture(&manager, &config).await?;
    let inherited_model = child_config.model.clone();

    SpawnAgentConfigOverrides {
        model: None,
        effort: Some("high".to_string()),
        agent_type: None,
    }
    .apply(
        &parent_thread.codex.session,
        parent_turn.as_ref(),
        &mut child_config,
    )
    .await
    .expect("an effort-only override should apply against the inherited model");

    assert_eq!(
        child_config.model, inherited_model,
        "an effort-only override must not change the inherited model"
    );
    assert_eq!(
        child_config.model_reasoning_effort,
        Some(ReasoningEffort::High)
    );

    Ok(())
}

/// An effort outside the resolved model's `supported_reasoning_levels` is rejected with an
/// actionable error naming the effort, the model, and the supported set (`gpt-5.4` supports only
/// `low|medium|high|xhigh`, so `max` — a valid *workflow* effort — is unsupported *by the model*).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_config_rejects_effort_unsupported_by_model() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;
    let (parent_thread, parent_turn, mut child_config) =
        override_test_fixture(&manager, &config).await?;

    let error = SpawnAgentConfigOverrides {
        model: Some("gpt-5.4".to_string()),
        effort: Some("max".to_string()),
        agent_type: None,
    }
    .apply(
        &parent_thread.codex.session,
        parent_turn.as_ref(),
        &mut child_config,
    )
    .await
    .expect_err("an effort unsupported by the model must be rejected");

    let message = error.to_string();
    assert!(
        message.contains("max") && message.contains("gpt-5.4") && message.contains("not supported"),
        "error must actionably name the effort, model, and that it is unsupported: {message}"
    );

    Ok(())
}

/// A malformed `opts.effort` (not one of the documented `low|medium|high|xhigh|max`) is rejected
/// before any model resolution, with an actionable error listing the accepted values.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_config_rejects_out_of_contract_effort_string() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;
    let (parent_thread, parent_turn, mut child_config) =
        override_test_fixture(&manager, &config).await?;

    let error = SpawnAgentConfigOverrides {
        model: None,
        effort: Some("ultra".to_string()),
        agent_type: None,
    }
    .apply(
        &parent_thread.codex.session,
        parent_turn.as_ref(),
        &mut child_config,
    )
    .await
    .expect_err("an out-of-contract workflow effort must be rejected");

    let message = error.to_string();
    assert!(
        message.contains("ultra") && message.contains("low, medium, high, xhigh, max"),
        "error must name the offending value and the accepted set: {message}"
    );

    Ok(())
}

/// Omitting both `opts.model` and `opts.effort` leaves the parent-inherited child config unchanged
/// (and `is_empty()` reports the no-op so the helper can skip session resolution entirely).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_config_without_overrides_is_unchanged() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;
    let (parent_thread, parent_turn, mut child_config) =
        override_test_fixture(&manager, &config).await?;

    let inherited = child_config.clone();
    let overrides = SpawnAgentConfigOverrides::default();
    assert!(
        overrides.is_empty(),
        "a default override carries no requested model/effort"
    );

    overrides
        .apply(
            &parent_thread.codex.session,
            parent_turn.as_ref(),
            &mut child_config,
        )
        .await
        .expect("a no-op override must succeed");

    assert_eq!(
        child_config, inherited,
        "omitting model/effort must leave the parent-inherited config unchanged"
    );

    Ok(())
}

/// Each documented workflow effort (`low..max`) maps to the correct `ReasoningEffort` variant, and
/// every out-of-contract value is rejected rather than silently forwarded.
#[test]
fn map_workflow_effort_maps_documented_levels() {
    assert_eq!(map_workflow_effort("low").unwrap(), ReasoningEffort::Low);
    assert_eq!(
        map_workflow_effort("medium").unwrap(),
        ReasoningEffort::Medium
    );
    assert_eq!(map_workflow_effort("high").unwrap(), ReasoningEffort::High);
    assert_eq!(
        map_workflow_effort("xhigh").unwrap(),
        ReasoningEffort::XHigh
    );
    assert_eq!(map_workflow_effort("max").unwrap(), ReasoningEffort::Max);

    for invalid in ["", "none", "minimal", "ultra", "MAX", "extreme"] {
        assert!(
            map_workflow_effort(invalid).is_err(),
            "out-of-contract effort `{invalid}` must be rejected"
        );
    }
}

/// Distinctive model slug a registered `reviewer` role layer stamps onto the child config, so tests
/// can prove the role was resolved and applied by observing `config.model`.
const REVIEWER_ROLE_MODEL: &str = "reviewer-role-model";

/// Registers a user-defined `reviewer` role (whose role layer locks `model = REVIEWER_ROLE_MODEL`)
/// on `config` so `opts.agentType = "reviewer"` resolves to it. Mirrors the user-role wiring the
/// role-layer tests use (`agent/role_tests.rs`): a `reviewer.toml` on disk referenced from
/// `config.agent_roles`.
fn register_reviewer_role(config: &mut Config, codex_home: &TempDir) {
    let role_path = codex_home.path().join("reviewer.toml");
    std::fs::write(&role_path, format!("model = \"{REVIEWER_ROLE_MODEL}\"\n"))
        .expect("write reviewer role config");
    config.agent_roles.insert(
        "reviewer".to_string(),
        crate::config::AgentRoleConfig {
            description: Some("Review carefully.".to_string()),
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    );
}

/// `opts.agentType = "reviewer"` resolves the `reviewer` role and applies its role layer to the
/// child config (its locked `model` lands on the child), proving the role name is resolved rather
/// than dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_config_applies_requested_agent_type() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let codex_home = TempDir::new()?;
    let mut config = test_config_for_server(&codex_home, &server).await?;
    register_reviewer_role(&mut config, &codex_home);
    let manager = build_manager(&config).await;
    let (parent_thread, parent_turn, mut child_config) =
        override_test_fixture(&manager, &config).await?;

    SpawnAgentConfigOverrides {
        model: None,
        effort: None,
        agent_type: Some("reviewer".to_string()),
    }
    .apply(
        &parent_thread.codex.session,
        parent_turn.as_ref(),
        &mut child_config,
    )
    .await
    .expect("a registered role should resolve and apply");

    assert_eq!(
        child_config.model.as_deref(),
        Some(REVIEWER_ROLE_MODEL),
        "opts.agentType must apply the resolved role's layer to the child config"
    );

    Ok(())
}

/// An absent `opts.agentType` falls back to `DEFAULT_ROLE_NAME` (whose role layer is a no-op), so a
/// blank/whitespace value is treated as absent (`is_empty()` reports it) and applying leaves the
/// parent-inherited config unchanged rather than erroring or picking a foreign role.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_config_absent_agent_type_falls_back_to_default_role() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;
    let (parent_thread, parent_turn, mut child_config) =
        override_test_fixture(&manager, &config).await?;
    let inherited = child_config.clone();

    // A blank/whitespace `agent_type` is treated as absent: it resolves to the no-op default role.
    let overrides = SpawnAgentConfigOverrides {
        model: None,
        effort: None,
        agent_type: Some("   ".to_string()),
    };
    assert!(
        overrides.is_empty(),
        "a blank agent_type resolves to the default role and must not count as a requested override"
    );

    overrides
        .apply(
            &parent_thread.codex.session,
            parent_turn.as_ref(),
            &mut child_config,
        )
        .await
        .expect("the default-role fallback must succeed");

    assert_eq!(
        child_config, inherited,
        "an absent agent_type must fall back to the no-op default role and leave the config unchanged"
    );

    Ok(())
}

/// An unknown `opts.agentType` surfaces an actionable error naming the offending role rather than
/// silently spawning under the default role.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_config_rejects_unknown_agent_type() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;
    let (parent_thread, parent_turn, mut child_config) =
        override_test_fixture(&manager, &config).await?;

    let error = SpawnAgentConfigOverrides {
        model: None,
        effort: None,
        agent_type: Some("nonexistent-role".to_string()),
    }
    .apply(
        &parent_thread.codex.session,
        parent_turn.as_ref(),
        &mut child_config,
    )
    .await
    .expect_err("an unknown role must be rejected, not silently defaulted");

    let message = error.to_string();
    assert!(
        message.contains("unknown agent_type") && message.contains("nonexistent-role"),
        "error must actionably name the unknown role: {message}"
    );

    Ok(())
}

/// Ordering guarantee: the role layer is applied **after** the model/effort overrides (matching V2
/// `spawn_agent`), so a role that locks a model takes precedence over a requested `opts.model`. The
/// requested model resolves and is applied first, then the `reviewer` role's locked model wins —
/// which is observable only if the role ran last.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_config_applies_agent_type_after_model_effort() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let codex_home = TempDir::new()?;
    let mut config = test_config_for_server(&codex_home, &server).await?;
    register_reviewer_role(&mut config, &codex_home);
    let manager = build_manager(&config).await;
    let (parent_thread, parent_turn, mut child_config) =
        override_test_fixture(&manager, &config).await?;

    SpawnAgentConfigOverrides {
        model: Some("gpt-5.4".to_string()),
        effort: Some("high".to_string()),
        agent_type: Some("reviewer".to_string()),
    }
    .apply(
        &parent_thread.codex.session,
        parent_turn.as_ref(),
        &mut child_config,
    )
    .await
    .expect("a supported model/effort plus a registered role should apply");

    assert_eq!(
        child_config.model.as_deref(),
        Some(REVIEWER_ROLE_MODEL),
        "the role layer must run after the model override, so the role's locked model wins over \
         the requested opts.model"
    );

    Ok(())
}

/// The ordinal-derived nickname preference is a pure function of the ordinal (no `Date`/`Math`/
/// `rand` inputs), so two runs of the same fan-out assign identical nicknames per ordinal, and the
/// documented pool-cycling mapping holds: `0..N` are the bare pool names, and each full wrap of the
/// `N`-name pool advances the deterministic `the Nth` suffix.
#[test]
fn workflow_nickname_preference_is_deterministic_and_ordinal_only() {
    let pool = default_agent_nickname_list();
    let pool_len = pool.len();
    assert!(
        pool_len > 0,
        "the shipped agent name pool must be non-empty"
    );

    // Same ordinal twice → identical (pure function).
    assert_eq!(
        workflow_agent_nickname_preference(0),
        workflow_agent_nickname_preference(0),
    );

    // Two identical fan-outs (0..2N) assign identical nicknames per ordinal.
    let run_a: Vec<String> = (0..pool_len * 2)
        .map(workflow_agent_nickname_preference)
        .collect();
    let run_b: Vec<String> = (0..pool_len * 2)
        .map(workflow_agent_nickname_preference)
        .collect();
    assert_eq!(
        run_a, run_b,
        "two runs of the same fan-out must assign identical nicknames per ordinal"
    );

    // Documented cycling mapping: bare pool names on the first pass, `the 2nd` on the next wrap.
    assert_eq!(workflow_agent_nickname_preference(0), pool[0]);
    assert_eq!(
        workflow_agent_nickname_preference(pool_len - 1),
        pool[pool_len - 1]
    );
    assert_eq!(
        workflow_agent_nickname_preference(pool_len),
        format!("{} the 2nd", pool[0]),
    );
    assert_eq!(
        workflow_agent_nickname_preference(pool_len + 1),
        format!("{} the 2nd", pool[1]),
    );
    assert_eq!(
        workflow_agent_nickname_preference(pool_len * 2),
        format!("{} the 3rd", pool[0]),
    );
}

/// The ordinal → nickname mapping is **injective**: a fan-out never derives the same nickname for
/// two distinct ordinals, so there is no collision for the registry to break (deterministically or
/// otherwise). Cover several full pool cycles so the `the Nth` suffix path is exercised.
#[test]
fn workflow_nickname_preference_is_injective_across_cycles() {
    let pool_len = default_agent_nickname_list().len();
    let count = pool_len * 3 + 7;
    let nicknames: std::collections::HashSet<String> =
        (0..count).map(workflow_agent_nickname_preference).collect();
    assert_eq!(
        nicknames.len(),
        count,
        "distinct ordinals must derive distinct nicknames (no intra-fan-out collision)"
    );
}

/// `format_ordinal_nickname` matches the registry's own reset-cycle naming (`registry.rs:44`):
/// cycle 0 is the bare name, and subsequent cycles append the correct English ordinal suffix
/// (including the 11th..13th special case).
#[test]
fn format_ordinal_nickname_matches_registry_suffix_scheme() {
    assert_eq!(format_ordinal_nickname("Plato", 0), "Plato");
    assert_eq!(format_ordinal_nickname("Plato", 1), "Plato the 2nd");
    assert_eq!(format_ordinal_nickname("Plato", 2), "Plato the 3rd");
    assert_eq!(format_ordinal_nickname("Plato", 3), "Plato the 4th");
    // value = cycle + 1 == 11 → the 11th..13th special-case yields "th".
    assert_eq!(format_ordinal_nickname("Plato", 10), "Plato the 11th");
    assert_eq!(format_ordinal_nickname("Plato", 20), "Plato the 21st");
}

/// Rand-bypass proof (spec §13 Q4): reserving with an ordinal-derived **preference** and an *empty*
/// candidate pool returns the preferred name verbatim. With no candidates, the non-preferred branch
/// (the one that calls `rand::rng()`, `registry.rs:232`) can only return `None` → an error; getting
/// the preferred name back therefore proves the `rand`-free preferred branch was taken. Because the
/// preference is a pure function of the ordinal, the assignment is fully deterministic.
#[test]
fn preferred_nickname_bypasses_rand_pool_pick() {
    let preferred = workflow_agent_nickname_preference(3);

    let registry = Arc::new(AgentRegistry::default());
    let mut reservation = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve slot");
    let reserved = reservation
        .reserve_agent_nickname_with_preference(/*names*/ &[], Some(preferred.as_str()))
        .expect(
            "the preferred branch must reserve the name without consulting the empty rand pool",
        );
    assert_eq!(
        reserved, preferred,
        "an ordinal-derived preference must be reserved verbatim, bypassing rand::rng()"
    );

    // Determinism: a second, independent registry given the same ordinal reserves the same name.
    let other_registry = Arc::new(AgentRegistry::default());
    let mut other_reservation = other_registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve slot");
    let other_reserved = other_reservation
        .reserve_agent_nickname_with_preference(
            /*names*/ &[],
            Some(workflow_agent_nickname_preference(3).as_str()),
        )
        .expect("preferred branch reserves without rand");
    assert_eq!(
        reserved, other_reserved,
        "the same ordinal must reserve the same nickname across runs"
    );
}

/// Collision resolution (finding: registry preferred branch): when two agents ask for the same
/// preferred nickname, the second is resolved DETERMINISTICALLY (a `-2`/`-3` suffix), actually
/// reserved (so a third advances again), and never duplicated — and never via `rand`.
#[test]
fn preferred_nickname_collision_resolves_deterministically() {
    let registry = Arc::new(AgentRegistry::default());
    let preferred = workflow_agent_nickname_preference(0);

    let mut first = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve slot");
    let first_name = first
        .reserve_agent_nickname_with_preference(/*names*/ &[], Some(preferred.as_str()))
        .expect("the first reservation takes the preferred name verbatim");
    assert_eq!(first_name, preferred);

    // A second agent requesting the same preferred name must not get a duplicate: it resolves to a
    // deterministic `-2` suffix.
    let mut second = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve slot");
    let second_name = second
        .reserve_agent_nickname_with_preference(/*names*/ &[], Some(preferred.as_str()))
        .expect("the second reservation resolves the collision deterministically");
    assert_eq!(second_name, format!("{preferred}-2"));

    // The resolved `-2` was actually reserved, so a third collides with both and advances to `-3`.
    let mut third = registry
        .reserve_spawn_slot(/*max_threads*/ None)
        .expect("reserve slot");
    let third_name = third
        .reserve_agent_nickname_with_preference(/*names*/ &[], Some(preferred.as_str()))
        .expect("the third reservation advances the deterministic suffix");
    assert_eq!(third_name, format!("{preferred}-3"));
}

/// End-to-end plumbing: a `SpawnAgentOptions::preferred_agent_nickname` set by the caller threads
/// all the way through `spawn_agent_deferred_input` → `spawn_agent_internal` → `prepare_thread_spawn`
/// and is the nickname the child actually registers with — so a workflow's ordinal-derived
/// preference (not a random pool pick) is what lands on the spawned agent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_options_preferred_nickname_is_reserved_verbatim() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-child"),
            ev_assistant_message("msg-child", CHILD_FINAL_MESSAGE),
            ev_completed("resp-child"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    let config = test_config_for_server(&codex_home, &server).await?;
    let manager = build_manager(&config).await;

    let (parent_thread_id, base_instructions, parent_turn) =
        parent_spawn_context(&manager, &config).await?;
    let agent_control = manager.agent_control();
    let mut thread_created_rx = manager.subscribe_thread_created();

    let preferred = workflow_agent_nickname_preference(0);
    let options = SpawnAgentOptions {
        environments: Some(parent_turn.environments.to_selections()),
        preferred_agent_nickname: Some(preferred.clone()),
        ..Default::default()
    };

    let final_message = timeout(
        Duration::from_secs(30),
        agent_control.spawn_and_await_final_message(
            &base_instructions,
            parent_turn.as_ref(),
            parent_thread_id,
            vec![UserInput::Text {
                text: "run the child".to_string(),
                text_elements: Vec::new(),
            }],
            /*final_output_json_schema*/ None,
            SpawnAgentConfigOverrides::default(),
            options,
        ),
    )
    .await
    .expect("helper should finish before timeout");
    assert_eq!(final_message.as_deref(), Some(CHILD_FINAL_MESSAGE));

    let child_thread_id = thread_created_rx
        .try_recv()
        .expect("notify_thread_created should have broadcast the child thread id");
    let metadata = agent_control
        .get_agent_metadata(child_thread_id)
        .expect("the spawned child should be registered with metadata");
    assert_eq!(
        metadata.agent_nickname.as_deref(),
        Some(preferred.as_str()),
        "the ordinal-derived preferred nickname must be the one the child registers with"
    );

    Ok(())
}
