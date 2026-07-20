use super::WORKFLOW_WORKTREE_SETUP_FAILED;
use super::agent_execution::reject_worktree_setup;
use codex_code_mode::AgentSpawnOutcome;

#[test]
fn worktree_setup_rejection_keeps_host_details_out_of_public_outcome() {
    let raw_error = "Git failed at /private/host/project/.codex-worktrees-secret/agent-0";

    let execution = reject_worktree_setup(&raw_error);

    let AgentSpawnOutcome::Rejected(public_reason) = execution.outcome else {
        panic!("worktree setup failure must reject the agent call");
    };
    assert_eq!(public_reason, WORKFLOW_WORKTREE_SETUP_FAILED);
    assert!(!public_reason.contains("/private/host/project"));
}
