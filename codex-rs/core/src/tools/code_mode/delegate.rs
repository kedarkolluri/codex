use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use super::workflow_agent_controls::WorkflowAgentAttemptActivation;
use super::workflow_context_bounds::ensure_agent_options_within_bounds;
use super::workflow_context_bounds::ensure_prompt_within_bounds;
use super::workflow_context_bounds::ensure_schema_within_bounds;
use super::workflow_handler::WorkflowRunLedger;
use super::workflow_handler::protocol_budget_snapshot;
use super::workflow_handler::run_workflow_by_name;
use super::workflow_progress;
use codex_code_mode::AgentCallOpts;
use codex_code_mode::AgentSpawnFuture;
use codex_code_mode::AgentSpawnOutcome;
use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ToolInvocationFuture;
use codex_code_mode::WorkflowBudgetSnapshotFuture;
use codex_code_mode::WorkflowHostProgress;
use codex_core_workflows::WorkflowBudget;
use codex_core_workflows::WorkflowBudgetLimit;
use codex_git_utils::WorktreeCleanupOutcome;
use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadGoal;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::ThreadGoalUpdatedEvent;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentAttemptReason;
use codex_protocol::user_input::UserInput;
use codex_workflow_journal::AgentCallLine;
use codex_workflow_journal::AgentCallOpts as JournalAgentCallOpts;
use codex_workflow_journal::AgentCallProgress as JournalAgentCallProgress;
use codex_workflow_journal::AgentControlReason as JournalAgentControlReason;
use codex_workflow_journal::AgentStatus as JournalAgentStatus;
use codex_workflow_journal::AgentTokenUsage as JournalAgentTokenUsage;
use codex_workflow_journal::JournalRecorder;
use codex_workflow_journal::KeyInputs;
use codex_workflow_journal::LogLine;
use codex_workflow_journal::NullOrdinal;
use codex_workflow_journal::PhaseLine;
use codex_workflow_journal::prompt_hash;
use codex_workflow_journal::schema_hash;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::warn;

use super::ExecContext;
use super::PUBLIC_TOOL_NAME;
use super::call_nested_tool;
use super::scheduler::AgentCapReached;
use super::scheduler::SpawnAttempt;
use super::scheduler::WorkflowAdmission;
use super::scheduler::WorkflowScheduler;
use crate::agent::control::SpawnAgentOptions;
use crate::agent::control::spawn_await::workflow_agent_nickname_preference;
use crate::agent::control::spawn_await_opts::SpawnAgentConfigOverrides;
use crate::agent::control::spawn_workspace::SpawnAgentWorkspace;
use crate::agent::control::workflow_child_progress::WorkflowChildEvent;
use crate::agent::control::workflow_child_progress::WorkflowChildObserver;
use crate::agent::control::workflow_child_progress::WorkflowChildProgress;
use crate::agent::control::worktree_isolation::WORKFLOW_WORKTREE_SETUP_FAILED;
use crate::agent::control::worktree_isolation::WorkflowAgentIsolation;
use crate::agent::control::worktree_isolation::WorktreeExecutionEnvironment;
use crate::agent::control::worktree_isolation::WorktreeIsolationError;
use crate::session::step_context::StepContext;
use crate::tools::ToolRouter;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::parallel::ToolCallRuntime;

mod agent_control_loop;
mod agent_execution;
mod agent_journal;
mod agent_output;
mod broker;
mod broker_state;
mod cell_dispatch;
mod session_delegate;
mod turn_host;
mod workflow_callbacks;

const WORKFLOW_AGENT_RETRY_LIMIT_ERROR: &str = "workflow agent retry limit reached";

use agent_execution::validate_agent_invocation;
use agent_journal::AgentCallJournalCtx;
use agent_journal::AgentCallRecord;
use agent_journal::WORKFLOW_AGENT_JOURNAL_UNAVAILABLE;
use agent_journal::WORKFLOW_AGENT_TURN_TOKEN_ESTIMATE;
use agent_journal::WorkflowAgentExecution;
use agent_journal::journal_progress;
#[cfg(test)]
use agent_output::build_budget_thread_goal;
use agent_output::emit_budget_thread_goal;
use agent_output::finalize_agent_output;
use broker::CLOSED_CELL_TOMBSTONE_CAP;
pub(super) use broker::CodeModeDispatchBroker;
pub(crate) use broker::CodeModeDispatchOrigin;
pub(crate) use broker::CodeModeDispatchWorker;
use broker::CoreTurnHostFactory;
use broker::MAX_PREPARED_CELL_MESSAGES;
use broker::MAX_UNBOUND_DISPATCH_MESSAGES;
use broker::MAX_UNBOUND_DISPATCH_MESSAGES_PER_CELL;
#[cfg(test)]
use broker::TestBoundHost;
#[cfg(test)]
use broker::TestDispatchRecord;
#[cfg(test)]
use broker::TestTurnHostFactory;
use broker_state::BoundDispatchHost;
use broker_state::BrokerCommand;
use broker_state::CellDispatchCommand;
use broker_state::DispatchContext;
use broker_state::run_dispatch_broker;
use cell_dispatch::DispatchMessage;
use cell_dispatch::run_cell_dispatcher;
use turn_host::CoreTurnHost;
use turn_host::WorkflowAgentAttemptContext;
use turn_host::WorkflowAgentExecutionContext;
use turn_host::WorkflowAgentInvocation;
use turn_host::WorkflowAgentProgressContext;
use turn_host::add_workflow_child_progress;

#[cfg(test)]
#[path = "delegate/agent_output_tests.rs"]
mod agent_output_tests;

#[cfg(test)]
#[path = "delegate/agent_execution_tests.rs"]
mod agent_execution_tests;

#[cfg(test)]
#[path = "delegate/broker_tests.rs"]
mod broker_tests;
