//! Thread-scoped saved-workflow discovery for Codex hosts.
//!
//! The extension freezes the first selected execution environment at thread start and lazily
//! builds one immutable registry. Executor paths never cross onto the host filesystem.

mod config;
mod extension;
mod session_registry;

pub use config::WorkflowExtensionConfig;
pub use extension::install;
pub use session_registry::WorkflowSessionRegistry;
pub use session_registry::WorkflowSessionRegistryError;
pub use session_registry::WorkflowSessionRegistryResult;
pub use session_registry::workflow_session_registry;
