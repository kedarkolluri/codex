use std::io;
use std::sync::Arc;

use codex_core_workflows::WorkflowRegistry;
use codex_core_workflows::WorkflowRoot;
use codex_core_workflows::WorkflowRootAssembly;
use codex_core_workflows::load_workflows_from_roots;
use codex_exec_server::Environment;
use codex_exec_server::LocalFileSystem;
use codex_extension_api::ExtensionData;
use codex_file_system::ExecutorFileSystem;
use codex_file_system::FindUpErrorPolicy;
use codex_file_system::find_nearest_ancestor_with_markers;
use codex_file_system::find_nearest_native_ancestor_with_markers;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use thiserror::Error;
use tokio::sync::OnceCell;

use crate::WorkflowExtensionConfig;

/// One immutable root set and its discovered saved-workflow catalog for a thread.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowSessionRegistry {
    roots: Vec<WorkflowRoot>,
    registry: WorkflowRegistry,
}

impl WorkflowSessionRegistry {
    /// Roots in fixed Project, Personal, Codex-home precedence order.
    pub fn roots(&self) -> &[WorkflowRoot] {
        &self.roots
    }

    /// The immutable catalog loaded from [`Self::roots`].
    pub fn registry(&self) -> &WorkflowRegistry {
        &self.registry
    }
}

/// A fail-closed error encountered before safe workflow discovery could begin.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum WorkflowSessionRegistryError {
    /// The first selected environment did not exist when the thread started.
    #[error("workflow environment `{environment_id}` was not available at thread start")]
    UnknownEnvironment { environment_id: String },
    /// The captured environment could not become ready.
    #[error("workflow environment `{environment_id}` could not become ready: {message}")]
    EnvironmentUnavailable {
        environment_id: String,
        message: String,
    },
    /// The project boundary could not be resolved without ignoring a filesystem error.
    #[error("could not resolve the workflow project boundary from {cwd}: {message}")]
    ProjectBoundary { cwd: PathUri, message: String },
    /// The resolved executor project path could not safely produce a workflow root.
    #[error("could not assemble the executor workflow root from {project_root}: {message}")]
    InvalidExecutorProjectRoot {
        project_root: PathUri,
        message: String,
    },
}

/// Shared result returned by the thread-scoped registry accessor.
pub type WorkflowSessionRegistryResult =
    Result<Arc<WorkflowSessionRegistry>, Arc<WorkflowSessionRegistryError>>;

pub(crate) enum WorkflowProjectSource {
    Executor {
        environment_id: String,
        cwd: PathUri,
        environment: Arc<Environment>,
        file_system: Arc<dyn ExecutorFileSystem>,
    },
    Host {
        cwd: AbsolutePathBuf,
    },
    UnknownEnvironment {
        environment_id: String,
    },
}

pub(crate) struct WorkflowThreadState {
    config: WorkflowExtensionConfig,
    project_source: WorkflowProjectSource,
    registry: OnceCell<WorkflowSessionRegistryResult>,
}

impl WorkflowThreadState {
    pub(crate) fn new(
        config: WorkflowExtensionConfig,
        project_source: WorkflowProjectSource,
    ) -> Self {
        Self {
            config,
            project_source,
            registry: OnceCell::new(),
        }
    }

    async fn registry(&self) -> WorkflowSessionRegistryResult {
        self.registry
            .get_or_init(|| self.build_registry())
            .await
            .clone()
    }

    async fn build_registry(&self) -> WorkflowSessionRegistryResult {
        let mut assembly = WorkflowRootAssembly::new(self.config.codex_home.clone());
        if let Some(user_home) = self.config.user_home.clone() {
            assembly = assembly.with_user_home(user_home);
        }

        assembly = match &self.project_source {
            WorkflowProjectSource::Executor {
                environment_id,
                cwd,
                environment,
                file_system,
            } => {
                environment.wait_until_ready().await.map_err(|error| {
                    Arc::new(WorkflowSessionRegistryError::EnvironmentUnavailable {
                        environment_id: environment_id.clone(),
                        message: error.to_string(),
                    })
                })?;
                let project_root = executor_project_root(
                    file_system.as_ref(),
                    cwd,
                    &self.config.project_root_markers,
                )
                .await
                .map_err(Arc::new)?;
                assembly
                    .with_executor_project(&project_root, Arc::clone(file_system))
                    .map_err(|error| {
                        Arc::new(WorkflowSessionRegistryError::InvalidExecutorProjectRoot {
                            project_root,
                            message: error.to_string(),
                        })
                    })?
            }
            WorkflowProjectSource::Host { cwd } => {
                let file_system = LocalFileSystem::unsandboxed();
                let project_root =
                    host_project_root(&file_system, cwd, &self.config.project_root_markers)
                        .await
                        .map_err(Arc::new)?;
                assembly.with_host_project(project_root)
            }
            WorkflowProjectSource::UnknownEnvironment { environment_id } => {
                return Err(Arc::new(WorkflowSessionRegistryError::UnknownEnvironment {
                    environment_id: environment_id.clone(),
                }));
            }
        };

        let roots = assembly.into_roots();
        let registry = load_workflows_from_roots(roots.clone()).await;
        Ok(Arc::new(WorkflowSessionRegistry { roots, registry }))
    }
}

/// Returns the lazily initialized immutable workflow registry for this thread.
///
/// `None` means workflows were disabled when the thread started. Both successful registries and
/// fail-closed initialization errors are cached for the lifetime of the thread store.
pub async fn workflow_session_registry(
    thread_store: &ExtensionData,
) -> Option<WorkflowSessionRegistryResult> {
    let state = thread_store.get::<WorkflowThreadState>()?;
    Some(state.registry().await)
}

async fn executor_project_root(
    file_system: &dyn codex_file_system::ExecutorFileSystem,
    cwd: &PathUri,
    markers: &[String],
) -> Result<PathUri, WorkflowSessionRegistryError> {
    if markers.is_empty() {
        return Ok(cwd.clone());
    }
    find_nearest_ancestor_with_markers(
        file_system,
        cwd,
        markers.to_vec(),
        FindUpErrorPolicy::Propagate,
        /*sandbox*/ None,
    )
    .await
    .map(|project_root| project_root.unwrap_or_else(|| cwd.clone()))
    .map_err(|error| project_boundary_error(cwd.clone(), error))
}

async fn host_project_root(
    file_system: &dyn codex_file_system::ExecutorFileSystem,
    cwd: &AbsolutePathBuf,
    markers: &[String],
) -> Result<AbsolutePathBuf, WorkflowSessionRegistryError> {
    if markers.is_empty() {
        return Ok(cwd.clone());
    }
    find_nearest_native_ancestor_with_markers(
        file_system,
        cwd,
        markers.to_vec(),
        FindUpErrorPolicy::Propagate,
        /*sandbox*/ None,
    )
    .await
    .map(|project_root| project_root.unwrap_or_else(|| cwd.clone()))
    .map_err(|error| project_boundary_error(PathUri::from_abs_path(cwd), error))
}

fn project_boundary_error(cwd: PathUri, error: io::Error) -> WorkflowSessionRegistryError {
    WorkflowSessionRegistryError::ProjectBoundary {
        cwd,
        message: error.to_string(),
    }
}

#[cfg(test)]
#[path = "session_registry_tests.rs"]
mod tests;
