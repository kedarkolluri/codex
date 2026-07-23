//! Saved-workflow registry contracts for host-local and executor-backed roots.
//!
//! Discovery populates this registry from project, personal, and Codex-home roots. The registry
//! applies scope precedence independently of discovery order so project workflows reliably shadow
//! lower-precedence entries with the same name.

mod executor_loader;
mod loader;
mod model;
mod registry;
mod root_assembly;
mod source;
mod source_resolver;

pub use codex_code_mode_protocol::WORKFLOW_SOURCE_MAX_BYTES;
pub use loader::load_workflows_from_roots;
pub use model::WorkflowLoadError;
pub use model::WorkflowMetadata;
pub use model::WorkflowRoot;
pub use model::WorkflowScope;
pub use registry::WorkflowRegistry;
pub use root_assembly::WorkflowRootAssembly;
pub use source::WorkflowSourceLoadError;
pub use source::WorkflowSourceSnapshot;
pub use source_resolver::WorkflowSourceResolver;
pub use source_resolver::WorkflowSourceResolverError;
