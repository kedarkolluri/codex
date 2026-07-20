use codex_code_mode_protocol::WorkflowBudgetHandle;
use codex_code_mode_protocol::WorkflowBudgetSnapshot;
use pretty_assertions::assert_eq;

use super::WorkflowBudgetMirror;

#[test]
fn refresh_is_monotonic() {
    let mirror = WorkflowBudgetMirror::new(WorkflowBudgetSnapshot {
        total: Some(100),
        spent: 10,
        remaining: Some(90),
    });
    mirror.update(WorkflowBudgetSnapshot {
        total: Some(100),
        spent: 40,
        remaining: Some(60),
    });
    mirror.update(WorkflowBudgetSnapshot {
        total: Some(100),
        spent: 30,
        remaining: Some(70),
    });

    assert_eq!(mirror.total(), 100);
    assert_eq!(mirror.spent(), 40);
    assert_eq!(mirror.remaining(), 60);
}
