//! Saved-workflow registry contracts for host-local and executor-backed roots.
//!
//! Discovery populates this registry from project, personal, and Codex-home roots. The registry
//! applies scope precedence independently of discovery order so project workflows reliably shadow
//! lower-precedence entries with the same name.

mod executor_loader;
mod loader;
mod model;
mod registry;

pub use loader::load_workflows_from_roots;
pub use model::WorkflowLoadError;
pub use model::WorkflowMetadata;
pub use model::WorkflowRoot;
pub use model::WorkflowScope;
pub use registry::WorkflowRegistry;
