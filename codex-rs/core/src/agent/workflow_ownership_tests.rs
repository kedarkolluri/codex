use super::*;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn session_meta(
    thread_id: ThreadId,
    ownership: Option<WorkflowSupervisorOwnership>,
) -> RolloutItem {
    RolloutItem::SessionMeta(SessionMetaLine {
        meta: SessionMeta {
            id: thread_id,
            workflow_supervisor_ownership: ownership,
            ..SessionMeta::default()
        },
        git: None,
    })
}

fn resumed(thread_id: ThreadId, history: Vec<RolloutItem>) -> InitialHistory {
    InitialHistory::Resumed(ResumedHistory {
        conversation_id: thread_id,
        history: Arc::new(history),
        rollout_path: None,
    })
}

#[test]
fn resumed_delivery_uses_first_canonical_same_thread_metadata() {
    let thread_id = ThreadId::new();
    let future_ownership = WorkflowSupervisorOwnership { version: 2 };
    let cases = vec![
        (
            vec![session_meta(
                thread_id,
                Some(WorkflowSupervisorOwnership::V1),
            )],
            ParentCompletionDelivery::WorkflowSupervisor {
                ownership: WorkflowSupervisorOwnership::V1,
            },
        ),
        (
            vec![session_meta(thread_id, Some(future_ownership))],
            ParentCompletionDelivery::WorkflowSupervisor {
                ownership: future_ownership,
            },
        ),
        (
            vec![session_meta(thread_id, /*ownership*/ None)],
            ParentCompletionDelivery::NotifyParent,
        ),
        (
            vec![
                session_meta(ThreadId::new(), /*ownership*/ None),
                session_meta(thread_id, Some(WorkflowSupervisorOwnership::V1)),
            ],
            ParentCompletionDelivery::WorkflowSupervisor {
                ownership: WorkflowSupervisorOwnership::V1,
            },
        ),
        (
            vec![
                session_meta(thread_id, /*ownership*/ None),
                session_meta(thread_id, Some(WorkflowSupervisorOwnership::V1)),
            ],
            ParentCompletionDelivery::NotifyParent,
        ),
        (
            vec![session_meta(
                ThreadId::new(),
                Some(WorkflowSupervisorOwnership::V1),
            )],
            ParentCompletionDelivery::NotifyParent,
        ),
        (Vec::new(), ParentCompletionDelivery::NotifyParent),
    ];

    for (history, expected) in cases {
        assert_eq!(
            resolve_parent_completion_delivery(
                thread_id,
                &resumed(thread_id, history),
                ParentCompletionDelivery::WorkflowSupervisor {
                    ownership: WorkflowSupervisorOwnership::V1,
                },
            ),
            expected
        );
    }
}

#[test]
fn fresh_delivery_uses_only_trusted_spawn_intent() {
    let requested = ParentCompletionDelivery::WorkflowSupervisor {
        ownership: WorkflowSupervisorOwnership::V1,
    };

    for history in [InitialHistory::New, InitialHistory::Cleared] {
        assert_eq!(
            resolve_parent_completion_delivery(ThreadId::new(), &history, requested),
            requested
        );
        assert_eq!(
            resolve_parent_completion_delivery(
                ThreadId::new(),
                &history,
                ParentCompletionDelivery::NotifyParent,
            ),
            ParentCompletionDelivery::NotifyParent
        );
    }
}

#[test]
fn copied_fork_metadata_cannot_mint_workflow_ownership() {
    let copied_marker = session_meta(ThreadId::new(), Some(WorkflowSupervisorOwnership::V1));

    assert_eq!(
        resolve_parent_completion_delivery(
            ThreadId::new(),
            &InitialHistory::Forked(vec![copied_marker]),
            ParentCompletionDelivery::NotifyParent,
        ),
        ParentCompletionDelivery::NotifyParent
    );
}
