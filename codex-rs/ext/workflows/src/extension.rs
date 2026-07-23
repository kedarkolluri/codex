use std::sync::Arc;

use codex_core_workflows::WorkflowSourceResolver;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadLifecycleContributor;
use codex_extension_api::ThreadStartInput;

use crate::WorkflowExtensionConfig;
use crate::session_registry::WorkflowProjectSource;
use crate::session_registry::WorkflowThreadState;

struct WorkflowsExtension<C> {
    environment_manager: Arc<EnvironmentManager>,
    config_from_host: Arc<dyn Fn(&C) -> WorkflowExtensionConfig + Send + Sync>,
}

impl<C> ThreadLifecycleContributor<C> for WorkflowsExtension<C>
where
    C: Send + Sync + 'static,
{
    fn on_thread_start<'a>(&'a self, input: ThreadStartInput<'a, C>) -> ExtensionFuture<'a, ()> {
        let config = (self.config_from_host)(input.config);
        if !config.enabled {
            input.thread_store.remove::<WorkflowThreadState>();
            input.thread_store.remove::<WorkflowSourceResolver>();
            return Box::pin(std::future::ready(()));
        }

        // Resolve the first selection synchronously. Capturing its exact Environment Arc here
        // prevents later manager replacement or a ready secondary environment from changing the
        // thread's filesystem authority while the lazy registry waits for startup.
        let project_source = match input.environments.first() {
            Some(selection) => match self
                .environment_manager
                .get_environment(&selection.environment_id)
            {
                Some(environment) => WorkflowProjectSource::Executor {
                    environment_id: selection.environment_id.clone(),
                    cwd: selection.cwd.clone(),
                    file_system: environment.get_filesystem(),
                    environment,
                },
                None => WorkflowProjectSource::UnknownEnvironment {
                    environment_id: selection.environment_id.clone(),
                },
            },
            None => WorkflowProjectSource::Host {
                cwd: config.fallback_cwd.clone(),
            },
        };
        let state = WorkflowThreadState::new(config, project_source);
        input.thread_store.insert(state.source_resolver());
        input.thread_store.insert(state);
        Box::pin(std::future::ready(()))
    }
}

/// Installs immutable, lazy workflow-registry discovery into a host extension registry.
pub fn install<C>(
    builder: &mut ExtensionRegistryBuilder<C>,
    environment_manager: Arc<EnvironmentManager>,
    config_from_host: impl Fn(&C) -> WorkflowExtensionConfig + Send + Sync + 'static,
) where
    C: Send + Sync + 'static,
{
    builder.thread_lifecycle_contributor(Arc::new(WorkflowsExtension {
        environment_manager,
        config_from_host: Arc::new(config_from_host),
    }));
}

#[cfg(test)]
#[path = "extension_tests.rs"]
mod tests;
