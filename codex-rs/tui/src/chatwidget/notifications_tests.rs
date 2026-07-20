use super::*;
use codex_app_server_protocol::CollabAgentStatus;
use pretty_assertions::assert_eq;

#[test]
fn workflow_complete_uses_dedicated_allowlist_name() {
    let notification = workflow_notification();

    assert!(notification.allowed_for(&Notifications::Custom(vec![
        "workflow-complete".to_string(),
    ])));
    assert!(!notification.allowed_for(&Notifications::Custom(vec![
        "agent-turn-complete".to_string(),
    ])));
    assert_eq!(
        notification.display(),
        "Workflow release-check completed · 3 agents · spent 3.6K weighted tokens"
    );
}

#[test]
fn workflow_complete_display_bounds_and_normalizes_all_content() {
    let notification = Notification::WorkflowComplete {
        name: format!("  {}\nignored", "界".repeat(/*n*/ 200)),
        status: CollabAgentStatus::NotFound,
        agent_count: Some(usize::MAX),
        spent: i64::MAX,
    };

    let display = notification.display();

    assert!(display.graphemes(true).count() <= WORKFLOW_NOTIFICATION_GRAPHEMES);
    assert!(!display.contains('\n'));
    assert!(display.starts_with(&format!("Workflow {}… not found", "界".repeat(/*n*/ 71))));
    assert!(display.contains(&format!("· {} agents", usize::MAX)));
    assert!(display.contains("· spent 9223372T weighted tokens"));
}

fn workflow_notification() -> Notification {
    Notification::WorkflowComplete {
        name: "release-check".to_string(),
        status: CollabAgentStatus::Completed,
        agent_count: Some(3),
        spent: 3_600,
    }
}
