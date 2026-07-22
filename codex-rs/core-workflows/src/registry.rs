use std::collections::BTreeMap;
use std::collections::HashSet;
use std::sync::Arc;

use codex_file_system::ExecutorFileSystem;

use crate::WorkflowLoadError;
use crate::WorkflowMetadata;
use crate::model::WorkflowFileSystemAuthority;

/// Deterministic saved-workflow catalog with scope precedence already applied.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkflowRegistry {
    workflows: Vec<WorkflowMetadata>,
    errors: Vec<WorkflowLoadError>,
    executor_file_systems_by_name: BTreeMap<String, WorkflowFileSystemAuthority>,
}

impl WorkflowRegistry {
    /// Builds a registry from discovered entries and fail-open diagnostics.
    ///
    /// Duplicate script paths within one filesystem authority and duplicate names are resolved by
    /// scope: project entries win over personal entries, which win over Codex-home entries. Ties
    /// within one scope use the lexicographically smallest canonical `file:` URI. Exact
    /// cross-authority ties retain root order; all other output is deterministic.
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
        mut workflows: Vec<(WorkflowMetadata, Option<WorkflowFileSystemAuthority>)>,
        mut errors: Vec<WorkflowLoadError>,
    ) -> Self {
        // PathUri has no environment ID, so otherwise-identical cross-authority ties retain root
        // order until the selected executor exposes a stable authority identifier.
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

        let mut seen_paths = HashSet::new();
        let mut seen_names = HashSet::new();
        workflows.retain(|(workflow, file_system)| {
            let path_identity = (workflow.path.clone(), file_system.clone());
            if seen_paths.contains(&path_identity) || seen_names.contains(&workflow.name) {
                return false;
            }
            seen_paths.insert(path_identity);
            seen_names.insert(workflow.name.clone());
            true
        });
        workflows.sort_by(|left, right| left.0.name.cmp(&right.0.name));
        errors.sort_by(|left, right| {
            left.path
                .to_string()
                .cmp(&right.path.to_string())
                .then_with(|| left.message.cmp(&right.message))
        });
        let executor_file_systems_by_name = workflows
            .iter()
            .filter_map(|(workflow, file_system)| {
                file_system
                    .clone()
                    .map(|file_system| (workflow.name.clone(), file_system))
            })
            .collect();
        let workflows = workflows
            .into_iter()
            .map(|(workflow, _file_system)| workflow)
            .collect();

        Self {
            workflows,
            errors,
            executor_file_systems_by_name,
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

    /// Returns the executor only when `workflow` exactly matches an executor-backed entry.
    pub fn executor_file_system_for(
        &self,
        workflow: &WorkflowMetadata,
    ) -> Option<Arc<dyn ExecutorFileSystem>> {
        let index = self
            .workflows
            .binary_search_by(|candidate| candidate.name.cmp(&workflow.name))
            .ok()?;
        if &self.workflows[index] != workflow {
            return None;
        }
        self.executor_file_systems_by_name
            .get(&workflow.name)
            .map(WorkflowFileSystemAuthority::file_system)
            .cloned()
    }

    /// Workflow names in deterministic listing order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.workflows.iter().map(|workflow| workflow.name.as_str())
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
