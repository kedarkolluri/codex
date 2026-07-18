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

pub mod loader;
pub mod model;

pub use loader::WORKFLOWS_DIR_NAME;
pub use loader::WorkflowRegistry;
pub use loader::load_workflows_from_roots;
pub use loader::workflow_roots;
pub use model::WorkflowLoadError;
pub use model::WorkflowMetadata;
pub use model::WorkflowRoot;
pub use model::WorkflowScope;
