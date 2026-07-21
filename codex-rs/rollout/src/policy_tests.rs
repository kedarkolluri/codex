use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use pretty_assertions::assert_eq;

use super::should_persist_event_msg;

#[test]
fn workflow_progress_is_persisted_in_every_history_mode() {
    let event = EventMsg::from(WorkflowRunBeginEvent {
        run_id: "run-1".to_string(),
        resumed_from_run_id: None,
        name: "review".to_string(),
        phases: vec!["inspect".to_string()],
        args_digest: "digest".to_string(),
    });
    let actual = [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated]
        .map(|history_mode| should_persist_event_msg(&event, history_mode));

    assert_eq!(actual, [true, true]);
}
