//! Host-local saved-workflow registry contracts.
//!
//! Discovery populates this registry from project, personal, and Codex-home roots. The registry
//! applies scope precedence independently of discovery order so project workflows reliably shadow
//! lower-precedence entries with the same name.

mod model;
mod registry;

pub use model::WorkflowLoadError;
pub use model::WorkflowMetadata;
pub use model::WorkflowRoot;
pub use model::WorkflowScope;
pub use registry::WorkflowRegistry;
