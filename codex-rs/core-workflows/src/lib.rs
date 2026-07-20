//! Saved-workflow discovery loader.
//!
//! Clones the shape of the `core-skills` loader (`core-skills/src/loader.rs`):
//! discover workflow scripts across precedence-ordered roots, statically parse
//! ONLY the leading `export const meta = {...}` literal for the picker/registry
//! (never executing the body — security-critical), and de-duplicate same-name
//! entries with scope precedence.
//!
//! Roots, in precedence order (spec §9):
//! 1. `<repo>/.codex/workflows` (project-scoped, checked in — recommended default)
//! 2. `$HOME/.agents/workflows` (personal)
//! 3. `$CODEX_HOME/workflows`

mod budget;
pub mod loader;
pub mod model;
mod run_model;
mod save;

pub use budget::WorkflowBudget;
pub use budget::WorkflowBudgetExceeded;
pub use budget::WorkflowBudgetLimit;
pub use budget::WorkflowBudgetReservation;
pub use budget::WorkflowBudgetSnapshot;
pub use loader::WORKFLOWS_DIR_NAME;
pub use loader::WorkflowRegistry;
pub use loader::load_workflows_from_roots;
pub use loader::workflow_roots;
pub use model::WorkflowLoadError;
pub use model::WorkflowMetadata;
pub use model::WorkflowRoot;
pub use model::WorkflowScope;
pub use run_model::WorkflowAgent;
pub use run_model::WorkflowAggregate;
pub use run_model::WorkflowBudgetSummary;
pub use run_model::WorkflowGroup;
pub use run_model::WorkflowModelError;
pub use run_model::WorkflowNodeState;
pub use run_model::WorkflowPhase;
pub use run_model::WorkflowPhaseState;
pub use run_model::WorkflowRunModel;
pub use run_model::WorkflowRunState;
pub use run_model::WorkflowTopologyNode;
pub use save::WORKFLOW_SOURCE_MAX_BYTES;
pub use save::WorkflowSaveError;
pub use save::WorkflowSaveMode;
pub use save::WorkflowSaveOutcome;
pub use save::WorkflowSaveRoot;
pub use save::WorkflowSaveSourceIdentity;
pub use save::save_run_workflow;
