use super::*;
use crate::agent::role_context_bounds::MAX_ROLE_PROJECT_DOC_BYTES;
use crate::agent::role_context_bounds::MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES;
use crate::config::ConfigBuilder;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use toml::Value as TomlValue;

async fn test_config() -> (TempDir, Config) {
    let home = TempDir::new().expect("create temp Codex home");
    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .cli_overrides(vec![(
            "model".to_string(),
            TomlValue::String("gpt-5.5".to_string()),
        )])
        .build()
        .await
        .expect("load test config");
    (home, config)
}

#[tokio::test]
async fn workflow_supervisor_rejects_full_history_fork() {
    let (_home, mut config) = test_config().await;

    let error = enforce_workflow_child_spawn_bounds(
        &mut config,
        ParentCompletionDelivery::WorkflowSupervisor,
        Some(&SpawnAgentForkMode::FullHistory),
    )
    .expect_err("workflow-managed full-history fork should be rejected");

    assert_eq!(
        error.to_string(),
        "workflow-managed child sessions cannot fork parent history"
    );
}

#[tokio::test]
async fn workflow_supervisor_rejects_last_n_turns_fork() {
    let (_home, mut config) = test_config().await;

    let error = enforce_workflow_child_spawn_bounds(
        &mut config,
        ParentCompletionDelivery::WorkflowSupervisor,
        Some(&SpawnAgentForkMode::LastNTurns(1)),
    )
    .expect_err("workflow-managed truncated-history fork should be rejected");

    assert_eq!(
        error.to_string(),
        "workflow-managed child sessions cannot fork parent history"
    );
}

#[tokio::test]
async fn workflow_supervisor_reapplies_context_bounds_without_echoing_content() {
    let (_home, mut config) = test_config().await;
    let secret = "private-resume-context";
    config.developer_instructions = Some(format!(
        "{secret}{}",
        "x".repeat(MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES)
    ));

    let error = enforce_workflow_child_spawn_bounds(
        &mut config,
        ParentCompletionDelivery::WorkflowSupervisor,
        /*fork_mode*/ None,
    )
    .expect_err("oversized workflow child context should be rejected");

    assert_eq!(
        error.to_string(),
        "workflow child configuration exceeds model-context limits"
    );
    assert!(!error.to_string().contains(secret));
}

#[tokio::test]
async fn workflow_supervisor_clamps_project_context_on_spawn_and_resume_gate() {
    let (_home, mut config) = test_config().await;
    config.project_doc_max_bytes = MAX_ROLE_PROJECT_DOC_BYTES + 1;

    enforce_workflow_child_spawn_bounds(
        &mut config,
        ParentCompletionDelivery::WorkflowSupervisor,
        /*fork_mode*/ None,
    )
    .expect("workflow child config should be bounded");

    assert_eq!(config.project_doc_max_bytes, MAX_ROLE_PROJECT_DOC_BYTES);
}

#[tokio::test]
async fn ordinary_agent_spawn_preserves_existing_config_and_fork_behavior() {
    let (_home, mut config) = test_config().await;
    config.project_doc_max_bytes = MAX_ROLE_PROJECT_DOC_BYTES + 1;

    enforce_workflow_child_spawn_bounds(
        &mut config,
        ParentCompletionDelivery::NotifyParent,
        Some(&SpawnAgentForkMode::FullHistory),
    )
    .expect("ordinary agent forks should remain allowed");

    assert_eq!(config.project_doc_max_bytes, MAX_ROLE_PROJECT_DOC_BYTES + 1);
}

#[test]
fn resumed_completion_delivery_uses_stored_or_matching_history_workflow_source() {
    let thread_id = ThreadId::new();
    let workflow_source = ThreadSource::Feature("workflow".to_string());
    let other_source = ThreadSource::Feature("other".to_string());

    for (stored_source, history_source, expected) in [
        (
            workflow_source.clone(),
            other_source.clone(),
            ParentCompletionDelivery::WorkflowSupervisor,
        ),
        (
            other_source.clone(),
            workflow_source,
            ParentCompletionDelivery::WorkflowSupervisor,
        ),
        (
            other_source.clone(),
            other_source,
            ParentCompletionDelivery::NotifyParent,
        ),
    ] {
        let history = InitialHistory::Resumed(ResumedHistory {
            conversation_id: thread_id,
            history: Arc::new(vec![RolloutItem::SessionMeta(SessionMetaLine {
                meta: SessionMeta {
                    id: thread_id,
                    thread_source: Some(history_source),
                    ..SessionMeta::default()
                },
                git: None,
            })]),
            rollout_path: None,
        });

        assert_eq!(
            resumed_parent_completion_delivery(thread_id, Some(&stored_source), &history),
            expected
        );
    }
}

#[test]
fn resumed_completion_delivery_ignores_foreign_workflow_session_metadata() {
    let thread_id = ThreadId::new();
    let history = InitialHistory::Resumed(ResumedHistory {
        conversation_id: thread_id,
        history: Arc::new(vec![RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                id: ThreadId::new(),
                thread_source: Some(ThreadSource::Feature("workflow".to_string())),
                ..SessionMeta::default()
            },
            git: None,
        })]),
        rollout_path: None,
    });

    assert_eq!(
        resumed_parent_completion_delivery(
            thread_id,
            Some(&ThreadSource::Feature("other".to_string())),
            &history,
        ),
        ParentCompletionDelivery::NotifyParent
    );
}
