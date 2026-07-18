use super::active_turn_sub_id;
use super::drain_buffered_final_message;
use crate::ThreadManager;
use crate::agent::control::SpawnAgentOptions;
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
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
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

/// `active_turn_sub_id` (the signal the helper uses to detect foreign steering) reports `None` for
/// an idle deferred child and the running turn's id once a turn is active.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_turn_sub_id_reflects_running_turn() -> anyhow::Result<()> {
    let server = start_mock_server().await;
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

    let child_thread_id = spawn_deferred_child(
        &manager,
        &base_instructions,
        parent_turn.as_ref(),
        parent_thread_id,
    )
    .await?;
    let state = agent_control.upgrade().expect("state should be alive");
    let child_thread = state.get_thread(child_thread_id).await?;

    // Deferred spawn submits no prompt: the child runs no turn of its own.
    assert_eq!(active_turn_sub_id(&child_thread).await, None);

    // A fresh turn on an idle child is stamped with its submission id.
    let submission_id = submit_and_await_active_turn(&child_thread, "start a turn").await?;
    assert_eq!(
        active_turn_sub_id(&child_thread).await.as_deref(),
        Some(submission_id.as_str()),
        "the running turn's id should match the submission id of the fresh turn"
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
