use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use codex_file_system::ExecutorFileSystem;
use crate::WorkflowLoadError;
use crate::WorkflowMetadata;

/// Deterministic saved-workflow catalog with scope precedence already applied.
#[derive(Clone, Default)]
pub struct WorkflowRegistry {
    workflows: Vec<WorkflowMetadata>,
    errors: Vec<WorkflowLoadError>,
    executor_file_systems: Vec<Option<Arc<dyn ExecutorFileSystem>>>,
}

impl WorkflowRegistry {
    /// Builds a registry from discovered entries and fail-open diagnostics.
    ///
    /// Duplicate script paths and names are resolved by scope, regardless of input order:
    /// project entries win over personal entries, which win over Codex-home entries. Ties within
    /// one scope use the lexicographically smallest absolute path. Final entries and diagnostics
    /// are sorted for deterministic consumers.
    pub fn new(workflows: Vec<WorkflowMetadata>, errors: Vec<WorkflowLoadError>) -> Self {
        Self::from_discovery(
            workflows
                .into_iter()
                .map(|workflow| (workflow, None))
                .collect(),
            errors,
        )
    }

    pub(crate) fn from_discovery(
        mut workflows: Vec<(WorkflowMetadata, Option<Arc<dyn ExecutorFileSystem>>)>,
        mut errors: Vec<WorkflowLoadError>,
    ) -> Self {
        workflows.sort_by(|left, right| {
            left.0
                .scope
                .precedence_rank()
                .cmp(&right.0.scope.precedence_rank())
                .then_with(|| left.0.path.to_string().cmp(&right.0.path.to_string()))
                .then_with(|| left.0.name.cmp(&right.0.name))
                .then_with(|| left.0.description.cmp(&right.0.description))
                .then_with(|| left.0.phases.cmp(&right.0.phases))
        });

        let mut seen_paths = HashMap::new();
        let mut seen_names = HashSet::new();
        workflows.retain(|(workflow, file_system)| {
            let authorities = seen_paths
                .entry(workflow.path.clone())
                .or_insert_with(Vec::new);
            if authorities
                .iter()
                .any(|seen| same_file_system_authority(seen, file_system))
            {
                return false;
            }
            authorities.push(file_system.clone());
            seen_names.insert(workflow.name.clone())
        });
        workflows.sort_by(|left, right| left.0.name.cmp(&right.0.name));
        errors.sort_by(|left, right| {
            left.path
                .to_string()
                .cmp(&right.path.to_string())
                .then_with(|| left.message.cmp(&right.message))
        });
        let (workflows, executor_file_systems) = workflows.into_iter().unzip();

        Self {
            workflows,
            errors,
            executor_file_systems,
        }
    }

    /// Workflows sorted by name after path and scope-precedence de-duplication.
    pub fn workflows(&self) -> &[WorkflowMetadata] {
        &self.workflows
    }

    /// Fail-open diagnostics sorted by path and message.
    pub fn errors(&self) -> &[WorkflowLoadError] {
        &self.errors
    }

    /// Resolves an exact, case-sensitive workflow name.
    pub fn resolve_by_name(&self, name: &str) -> Option<&WorkflowMetadata> {
        self.workflows
            .binary_search_by(|workflow| workflow.name.as_str().cmp(name))
            .ok()
            .map(|index| &self.workflows[index])
    }

    pub(crate) fn executor_file_system_for(
        &self,
        workflow: &WorkflowMetadata,
    ) -> Option<&Arc<dyn ExecutorFileSystem>> {
        let index = self
            .workflows
            .binary_search_by(|candidate| candidate.name.cmp(&workflow.name))
            .ok()?;
        if &self.workflows[index] != workflow {
            return None;
        }
        self.executor_file_systems.get(index)?.as_ref()
    }

    /// Workflow names in deterministic listing order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.workflows.iter().map(|workflow| workflow.name.as_str())
    }
}

impl fmt::Debug for WorkflowRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowRegistry")
            .field("workflows", &self.workflows)
            .field("errors", &self.errors)
            .finish_non_exhaustive()
    }
}

impl PartialEq for WorkflowRegistry {
    fn eq(&self, other: &Self) -> bool {
        self.workflows == other.workflows && self.errors == other.errors
    }
}

impl Eq for WorkflowRegistry {}

fn same_file_system_authority(
    left: &Option<Arc<dyn ExecutorFileSystem>>,
    right: &Option<Arc<dyn ExecutorFileSystem>>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        (None, Some(_)) | (Some(_), None) => false,
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
