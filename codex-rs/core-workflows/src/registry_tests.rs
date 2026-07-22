use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::WorkflowRegistry;
use crate::WorkflowLoadError;
use crate::WorkflowMetadata;
use crate::WorkflowScope;

fn path(root: &TempDir, relative: &str) -> PathUri {
    let path = AbsolutePathBuf::from_absolute_path_checked(root.path().join(relative)).unwrap();
    PathUri::from_abs_path(&path)
}

fn workflow(
    root: &TempDir,
    relative: &str,
    name: &str,
    description: &str,
    scope: WorkflowScope,
) -> WorkflowMetadata {
    WorkflowMetadata {
        name: name.to_string(),
        description: description.to_string(),
        phases: vec!["plan".to_string(), "ship".to_string()],
        path: path(root, relative),
        scope,
    }
}

#[test]
fn applies_scope_precedence_independently_of_discovery_order() {
    let root = TempDir::new().unwrap();
    let project = workflow(
        &root,
        "project/triage.js",
        "triage",
        "project",
        WorkflowScope::Project,
    );
    let personal = workflow(
        &root,
        "personal/triage.js",
        "triage",
        "personal",
        WorkflowScope::Personal,
    );
    let codex_home = workflow(
        &root,
        "codex-home/triage.js",
        "triage",
        "codex home",
        WorkflowScope::CodexHome,
    );
    let personal_only = workflow(
        &root,
        "personal/release.js",
        "release",
        "release",
        WorkflowScope::Personal,
    );

    let registry = WorkflowRegistry::new(
        vec![codex_home, personal, personal_only.clone(), project.clone()],
        Vec::new(),
    );

    assert_eq!(
        registry,
        WorkflowRegistry::new(vec![personal_only, project], Vec::new())
    );
}

#[test]
fn deduplicates_identical_paths_before_names_with_deterministic_ties() {
    let root = TempDir::new().unwrap();
    let shared_path_project = workflow(
        &root,
        "shared.js",
        "shared-project",
        "project",
        WorkflowScope::Project,
    );
    let shared_path_personal = workflow(
        &root,
        "shared.js",
        "shared-personal",
        "personal",
        WorkflowScope::Personal,
    );
    let lexically_first = workflow(
        &root,
        "a/deploy.js",
        "deploy",
        "first",
        WorkflowScope::Personal,
    );
    let lexically_later = workflow(
        &root,
        "z/deploy.js",
        "deploy",
        "later",
        WorkflowScope::Personal,
    );

    let registry = WorkflowRegistry::new(
        vec![
            lexically_later,
            shared_path_personal,
            lexically_first.clone(),
            shared_path_project.clone(),
        ],
        Vec::new(),
    );

    assert_eq!(
        registry,
        WorkflowRegistry::new(vec![lexically_first, shared_path_project], Vec::new())
    );
}

#[test]
fn sorts_diagnostics_and_resolves_exact_case_sensitive_names() {
    let root = TempDir::new().unwrap();
    let uppercase = workflow(
        &root,
        "uppercase.js",
        "Triage",
        "uppercase",
        WorkflowScope::Project,
    );
    let lowercase = workflow(
        &root,
        "lowercase.js",
        "triage",
        "lowercase",
        WorkflowScope::Project,
    );
    let first_error = WorkflowLoadError {
        path: path(&root, "a.js"),
        message: "invalid metadata".to_string(),
    };
    let second_error = WorkflowLoadError {
        path: path(&root, "z.js"),
        message: "unreadable".to_string(),
    };

    let registry = WorkflowRegistry::new(
        vec![lowercase.clone(), uppercase.clone()],
        vec![second_error.clone(), first_error.clone()],
    );

    assert_eq!(registry.resolve_by_name("Triage"), Some(&uppercase));
    assert_eq!(registry.resolve_by_name("triage"), Some(&lowercase));
    assert_eq!(registry.resolve_by_name("TRIAGE"), None);
    assert_eq!(
        registry.names().collect::<Vec<_>>(),
        vec!["Triage", "triage"]
    );
    assert_eq!(registry.errors(), &[first_error, second_error]);
}
