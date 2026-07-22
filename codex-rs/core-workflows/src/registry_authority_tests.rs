use std::collections::hash_map::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;

use codex_file_system as fs;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use pretty_assertions::assert_ne;

use super::WorkflowRegistry;
use crate::WorkflowMetadata;
use crate::WorkflowRoot;
use crate::WorkflowScope;
use crate::model::WorkflowFileSystemAuthority;
use crate::model::WorkflowRootSource;

struct IdentityFileSystem;

impl fs::ExecutorFileSystem for IdentityFileSystem {
    fn canonicalize<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, PathUri> {
        panic!("identity-only filesystem must not be used")
    }

    fn read_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, Vec<u8>> {
        panic!("identity-only filesystem must not be used")
    }

    fn read_file_stream<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, fs::FileSystemReadStream> {
        panic!("identity-only filesystem must not be used")
    }

    fn write_file<'a>(
        &'a self,
        _path: &'a PathUri,
        _contents: Vec<u8>,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        panic!("identity-only filesystem must not be used")
    }

    fn create_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: fs::CreateDirectoryOptions,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        panic!("identity-only filesystem must not be used")
    }

    fn get_metadata<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, fs::FileMetadata> {
        panic!("identity-only filesystem must not be used")
    }

    fn read_directory<'a>(
        &'a self,
        _path: &'a PathUri,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, Vec<fs::ReadDirectoryEntry>> {
        panic!("identity-only filesystem must not be used")
    }

    fn remove<'a>(
        &'a self,
        _path: &'a PathUri,
        _options: fs::RemoveOptions,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        panic!("identity-only filesystem must not be used")
    }

    fn copy<'a>(
        &'a self,
        _source_path: &'a PathUri,
        _destination_path: &'a PathUri,
        _options: fs::CopyOptions,
        _sandbox: Option<&'a fs::FileSystemSandboxContext>,
    ) -> fs::ExecutorFileSystemFuture<'a, ()> {
        panic!("identity-only filesystem must not be used")
    }
}

fn identity_file_system() -> Arc<dyn fs::ExecutorFileSystem> {
    Arc::new(IdentityFileSystem)
}

fn authority(file_system: &Arc<dyn fs::ExecutorFileSystem>) -> WorkflowFileSystemAuthority {
    let root = WorkflowRoot::project_on_executor(
        PathUri::parse("file:///authority").unwrap(),
        Arc::clone(file_system),
    );
    let WorkflowRootSource::Executor(authority) = root.source() else {
        unreachable!("project executor root changed source")
    };
    authority.clone()
}

fn workflow(path: &str, name: &str, description: &str, scope: WorkflowScope) -> WorkflowMetadata {
    WorkflowMetadata {
        name: name.to_string(),
        description: description.to_string(),
        phases: vec!["plan".to_string(), "ship".to_string()],
        path: PathUri::parse(path).unwrap(),
        scope,
    }
}

fn assert_executor(
    registry: &WorkflowRegistry,
    workflow: &WorkflowMetadata,
    expected: &Arc<dyn fs::ExecutorFileSystem>,
) {
    let actual = registry.executor_file_system_for(workflow).unwrap();
    assert!(Arc::ptr_eq(&actual, expected));
}

#[test]
fn isolates_path_identity_by_authority_without_poisoning_dedupe_state() {
    let file_system_a = identity_file_system();
    let file_system_b = identity_file_system();
    let authority_a = authority(&file_system_a);
    let authority_a_clone = authority(&file_system_a);
    let authority_b = authority(&file_system_b);
    assert_eq!(authority_a, authority_a_clone);
    assert_ne!(authority_a, authority_b);
    let mut hasher_a = DefaultHasher::new();
    authority_a.hash(&mut hasher_a);
    let mut hasher_a_clone = DefaultHasher::new();
    authority_a_clone.hash(&mut hasher_a_clone);
    assert_eq!(hasher_a.finish(), hasher_a_clone.finish());

    let host = workflow(
        "file:///matrix/shared.js",
        "host",
        "host",
        WorkflowScope::Personal,
    );
    let a_primary = workflow(
        "file:///matrix/shared.js",
        "a-primary",
        "a",
        WorkflowScope::Personal,
    );
    let a_shadow = workflow(
        "file:///matrix/shared.js",
        "a-shadow",
        "a",
        WorkflowScope::Personal,
    );
    let b_primary = workflow(
        "file:///matrix/shared.js",
        "b-primary",
        "b",
        WorkflowScope::Personal,
    );
    let shared_a = workflow(
        "file:///matrix/poison.js",
        "shared",
        "a",
        WorkflowScope::Personal,
    );
    let shared_b = workflow(
        "file:///matrix/poison.js",
        "shared",
        "b",
        WorkflowScope::Personal,
    );
    let survivor = workflow(
        "file:///matrix/poison.js",
        "survivor",
        "b",
        WorkflowScope::Personal,
    );
    let discovered = vec![
        (a_shadow.clone(), Some(authority_a_clone)),
        (shared_b.clone(), Some(authority_b.clone())),
        (host.clone(), None),
        (b_primary.clone(), Some(authority_b.clone())),
        (shared_a.clone(), Some(authority_a.clone())),
        (a_primary.clone(), Some(authority_a)),
        (survivor.clone(), Some(authority_b)),
    ];

    let registry = WorkflowRegistry::from_discovery(discovered.clone(), Vec::new());
    let mut reversed = discovered;
    reversed.reverse();
    let reversed = WorkflowRegistry::from_discovery(reversed, Vec::new());

    assert_eq!(registry, reversed);
    assert_eq!(
        registry.workflows(),
        &[
            a_primary.clone(),
            b_primary.clone(),
            host.clone(),
            shared_a.clone(),
            survivor.clone(),
        ]
    );
    assert!(registry.executor_file_system_for(&host).is_none());
    assert_executor(&registry, &a_primary, &file_system_a);
    assert_executor(&registry, &shared_a, &file_system_a);
    assert_executor(&registry, &b_primary, &file_system_b);
    assert_executor(&registry, &survivor, &file_system_b);
    assert_eq!(registry.resolve_by_name(&a_shadow.name), None);
    assert!(registry.executor_file_system_for(&shared_b).is_none());
}

#[test]
fn applies_scope_and_uri_precedence_before_routing_exact_metadata() {
    let file_system_a = identity_file_system();
    let file_system_b = identity_file_system();
    let authority_a = authority(&file_system_a);
    let authority_b = authority(&file_system_b);
    let deploy_project = workflow(
        "file:///z/deploy.js",
        "deploy",
        "project",
        WorkflowScope::Project,
    );
    let deploy_personal = workflow(
        "file:///a/deploy.js",
        "deploy",
        "personal",
        WorkflowScope::Personal,
    );
    let deploy_codex_home = workflow(
        "file:///0/deploy.js",
        "deploy",
        "codex home",
        WorkflowScope::CodexHome,
    );
    let release_a = workflow(
        "file:///a/release.js",
        "release",
        "same",
        WorkflowScope::Personal,
    );
    let release_b = workflow(
        "file:///z/release.js",
        "release",
        "same",
        WorkflowScope::Personal,
    );
    let discovered = vec![
        (deploy_codex_home, None),
        (release_b, Some(authority_b.clone())),
        (deploy_personal.clone(), Some(authority_a.clone())),
        (release_a.clone(), Some(authority_a)),
        (deploy_project.clone(), Some(authority_b)),
    ];

    let registry = WorkflowRegistry::from_discovery(discovered.clone(), Vec::new());
    let mut reversed = discovered;
    reversed.reverse();

    assert_eq!(
        registry,
        WorkflowRegistry::from_discovery(reversed, Vec::new())
    );
    assert_eq!(
        registry.workflows(),
        &[deploy_project.clone(), release_a.clone()]
    );
    assert_executor(&registry, &deploy_project, &file_system_b);
    assert_executor(&registry, &release_a, &file_system_a);
    assert!(
        registry
            .executor_file_system_for(&deploy_personal)
            .is_none()
    );
    let stale = [
        WorkflowMetadata {
            description: "stale".to_string(),
            ..deploy_project.clone()
        },
        WorkflowMetadata {
            phases: vec!["stale".to_string()],
            ..deploy_project.clone()
        },
        WorkflowMetadata {
            path: PathUri::parse("file:///stale/deploy.js").unwrap(),
            ..deploy_project.clone()
        },
        WorkflowMetadata {
            scope: WorkflowScope::Personal,
            ..deploy_project
        },
    ];
    assert!(
        stale
            .iter()
            .all(|workflow| registry.executor_file_system_for(workflow).is_none())
    );
}

#[test]
fn exact_cross_authority_tie_follows_root_order_without_stable_authority_id() {
    let file_system_a = identity_file_system();
    let file_system_b = identity_file_system();
    let authority_a = authority(&file_system_a);
    let authority_b = authority(&file_system_b);
    let workflow = workflow(
        "file:///matrix/exact.js",
        "exact",
        "same",
        WorkflowScope::Project,
    );
    let discovered = vec![
        (workflow.clone(), None),
        (workflow.clone(), Some(authority_a)),
        (workflow.clone(), Some(authority_b)),
    ];

    let host_first = WorkflowRegistry::from_discovery(discovered.clone(), Vec::new());
    let mut reversed = discovered;
    reversed.reverse();
    let executor_b_first = WorkflowRegistry::from_discovery(reversed, Vec::new());

    assert_eq!(host_first.workflows(), std::slice::from_ref(&workflow));
    assert_eq!(host_first.workflows(), executor_b_first.workflows());
    assert_ne!(host_first, executor_b_first);
    assert!(host_first.executor_file_system_for(&workflow).is_none());
    assert_executor(&executor_b_first, &workflow, &file_system_b);
}
