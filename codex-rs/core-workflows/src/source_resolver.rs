use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::WorkflowSourceSnapshot;

type ResolveFuture = Pin<
    Box<
        dyn Future<Output = Result<Option<WorkflowSourceSnapshot>, WorkflowSourceResolverError>>
            + Send
            + 'static,
    >,
>;
type ResolveFn = dyn Fn(String) -> ResolveFuture + Send + Sync;

/// A cloneable thread-scoped capability for capturing saved-workflow source.
///
/// Hosts install a resolver backed by their frozen discovery and filesystem authority. Consumers
/// receive only immutable source snapshots and do not need to depend on the concrete extension
/// that assembled the registry.
#[derive(Clone)]
pub struct WorkflowSourceResolver {
    resolve: Arc<ResolveFn>,
}

impl WorkflowSourceResolver {
    /// Wraps an asynchronous resolver whose failures are retained as host diagnostics.
    pub fn new<F, Fut, E>(resolve: F) -> Self
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<WorkflowSourceSnapshot>, E>> + Send + 'static,
        E: fmt::Display,
    {
        Self {
            resolve: Arc::new(move |name| {
                let future = resolve(name);
                Box::pin(async move {
                    future.await.map_err(|error| WorkflowSourceResolverError {
                        diagnostic: error.to_string().into_boxed_str(),
                    })
                })
            }),
        }
    }

    /// Captures the exact case-sensitive saved workflow selected by this thread's registry.
    pub async fn source_snapshot_by_name(
        &self,
        name: &str,
    ) -> Result<Option<WorkflowSourceSnapshot>, WorkflowSourceResolverError> {
        (self.resolve)(name.to_string()).await
    }
}

impl fmt::Debug for WorkflowSourceResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowSourceResolver")
            .finish_non_exhaustive()
    }
}

/// Host-only diagnostic returned when a thread-scoped source resolver fails.
///
/// The diagnostic can contain executor identifiers or paths and must not be copied into model
/// context or other user-visible output without redaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowSourceResolverError {
    diagnostic: Box<str>,
}

impl WorkflowSourceResolverError {
    /// Returns the host diagnostic for logs and debugging.
    pub fn diagnostic(&self) -> &str {
        &self.diagnostic
    }
}

impl fmt::Display for WorkflowSourceResolverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.diagnostic)
    }
}

impl std::error::Error for WorkflowSourceResolverError {}

#[cfg(test)]
#[path = "source_resolver_tests.rs"]
mod tests;
