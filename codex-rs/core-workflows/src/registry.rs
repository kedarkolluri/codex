use std::collections::HashSet;

use crate::WorkflowLoadError;
use crate::WorkflowMetadata;

/// Deterministic saved-workflow catalog with scope precedence already applied.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkflowRegistry {
    workflows: Vec<WorkflowMetadata>,
    errors: Vec<WorkflowLoadError>,
}

impl WorkflowRegistry {
    /// Builds a registry from discovered entries and fail-open diagnostics.
    ///
    /// Duplicate script paths and names are resolved by scope, regardless of input order:
    /// project entries win over personal entries, which win over Codex-home entries. Ties within
    /// one scope use the lexicographically smallest absolute path. Final entries and diagnostics
    /// are sorted for deterministic consumers.
    pub fn new(mut workflows: Vec<WorkflowMetadata>, mut errors: Vec<WorkflowLoadError>) -> Self {
        workflows.sort_by(|left, right| {
            left.scope
                .precedence_rank()
                .cmp(&right.scope.precedence_rank())
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.description.cmp(&right.description))
                .then_with(|| left.phases.cmp(&right.phases))
        });

        let mut seen_paths = HashSet::new();
        let mut seen_names = HashSet::new();
        workflows.retain(|workflow| {
            seen_paths.insert(workflow.path.clone()) && seen_names.insert(workflow.name.clone())
        });
        workflows.sort_by(|left, right| left.name.cmp(&right.name));
        errors.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.message.cmp(&right.message))
        });

        Self { workflows, errors }
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

    /// Workflow names in deterministic listing order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.workflows.iter().map(|workflow| workflow.name.as_str())
    }
}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
