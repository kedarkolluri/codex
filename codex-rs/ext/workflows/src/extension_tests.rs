use std::fs;
use std::path::Path;
use std::sync::Arc;

use codex_core_workflows::WorkflowScope;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadStartInput;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::install;
use crate::WorkflowExtensionConfig;
use crate::WorkflowSessionRegistryError;
use crate::workflow_session_registry;

#[derive(Clone)]
struct TestHostConfig(WorkflowExtensionConfig);

fn absolute(path: impl AsRef<Path>) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).expect("absolute test path")
}

fn config(temp: &TempDir, fallback_cwd: AbsolutePathBuf) -> WorkflowExtensionConfig {
    WorkflowExtensionConfig {
        enabled: true,
        codex_home: absolute(temp.path().join("codex-home")),
        user_home: Some(absolute(temp.path().join("home"))),
        fallback_cwd,
        project_root_markers: vec![".git".to_string()],
    }
}

fn selection(environment_id: &str, cwd: &AbsolutePathBuf) -> TurnEnvironmentSelection {
    TurnEnvironmentSelection {
        environment_id: environment_id.to_string(),
        cwd: PathUri::from_abs_path(cwd),
        workspace_roots: Vec::new(),
    }
}

fn write_workflow(root: &Path, name: &str, description: &str) {
    fs::create_dir_all(root).expect("create workflow root");
    fs::write(
        root.join(format!("{name}.workflow.js")),
        format!(
            "export const meta = {{ name: '{name}', description: '{description}', phases: [] }};"
        ),
    )
    .expect("write workflow");
}

async fn start_thread(
    environment_manager: Arc<EnvironmentManager>,
    config: WorkflowExtensionConfig,
    environments: &[TurnEnvironmentSelection],
) -> ExtensionData {
    let mut builder = ExtensionRegistryBuilder::new();
    install(
        &mut builder,
        environment_manager,
        |config: &TestHostConfig| config.0.clone(),
    );
    let registry = builder.build();
    let session_store = ExtensionData::new("session");
    let thread_store = ExtensionData::new("thread");
    registry.thread_lifecycle_contributors()[0]
        .on_thread_start(ThreadStartInput {
            config: &TestHostConfig(config),
            session_source: &SessionSource::Exec,
            persistent_thread_state_available: true,
            environments,
            session_store: &session_store,
            thread_store: &thread_store,
        })
        .await;
    thread_store
}

#[tokio::test]
async fn first_selected_environment_keeps_executor_filesystem_authority() {
    let temp = TempDir::new().expect("temp dir");
    let project = absolute(temp.path().join("project"));
    let cwd = project.join("nested");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::write(project.join(".git"), "marker").expect("write marker");
    write_workflow(
        &project.join(".codex").join("workflows"),
        "executor",
        "selected executor",
    );
    let environment_manager = Arc::new(EnvironmentManager::default_for_tests());
    let expected_file_system = environment_manager
        .get_environment(LOCAL_ENVIRONMENT_ID)
        .expect("local environment")
        .get_filesystem();

    let thread_store = start_thread(
        Arc::clone(&environment_manager),
        config(&temp, cwd.clone()),
        &[selection(LOCAL_ENVIRONMENT_ID, &cwd)],
    )
    .await;
    let session_registry = workflow_session_registry(&thread_store)
        .await
        .expect("workflow state")
        .expect("workflow registry");
    let workflow = session_registry
        .registry()
        .resolve_by_name("executor")
        .expect("executor workflow");
    let actual_file_system = session_registry
        .registry()
        .executor_file_system_for(workflow)
        .expect("executor authority");

    assert!(Arc::ptr_eq(&actual_file_system, &expected_file_system));
    assert_eq!(
        session_registry
            .roots()
            .iter()
            .map(|root| root.scope)
            .collect::<Vec<_>>(),
        vec![
            WorkflowScope::Project,
            WorkflowScope::Personal,
            WorkflowScope::CodexHome,
        ]
    );
}

#[tokio::test]
async fn no_selection_uses_host_fallback_and_scope_precedence() {
    let temp = TempDir::new().expect("temp dir");
    let project = absolute(temp.path().join("project"));
    let cwd = project.join("nested");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::write(project.join(".git"), "marker").expect("write marker");
    let config = config(&temp, cwd);
    write_workflow(
        &project.join(".codex").join("workflows"),
        "shadowed",
        "project",
    );
    write_workflow(
        &config
            .user_home
            .as_ref()
            .expect("user home")
            .join(".agents")
            .join("workflows"),
        "shadowed",
        "personal",
    );
    write_workflow(
        &config.codex_home.join("workflows"),
        "shadowed",
        "codex home",
    );

    let thread_store = start_thread(
        Arc::new(EnvironmentManager::without_environments()),
        config,
        &[],
    )
    .await;
    let session_registry = workflow_session_registry(&thread_store)
        .await
        .expect("workflow state")
        .expect("workflow registry");
    let workflow = session_registry
        .registry()
        .resolve_by_name("shadowed")
        .expect("project workflow");

    assert_eq!(workflow.description, "project");
    assert_eq!(workflow.scope, WorkflowScope::Project);
    assert!(
        session_registry
            .registry()
            .executor_file_system_for(workflow)
            .is_none()
    );
}

#[tokio::test]
async fn unknown_first_selection_is_cached_without_using_later_environment() {
    let temp = TempDir::new().expect("temp dir");
    let cwd = absolute(temp.path().join("project"));
    fs::create_dir_all(&cwd).expect("create cwd");
    let manager = Arc::new(EnvironmentManager::default_for_tests());
    let selections = [
        selection("missing", &cwd),
        selection(LOCAL_ENVIRONMENT_ID, &cwd),
    ];
    let thread_store = start_thread(manager, config(&temp, cwd), &selections).await;

    let first = workflow_session_registry(&thread_store)
        .await
        .expect("workflow state")
        .expect_err("unknown first environment");
    let second = workflow_session_registry(&thread_store)
        .await
        .expect("workflow state")
        .expect_err("cached unknown environment");

    assert_eq!(
        first.as_ref(),
        &WorkflowSessionRegistryError::UnknownEnvironment {
            environment_id: "missing".to_string(),
        }
    );
    assert!(Arc::ptr_eq(&first, &second));
}

#[tokio::test]
async fn registry_snapshot_is_stable_until_a_new_thread_starts() {
    let temp = TempDir::new().expect("temp dir");
    let project = absolute(temp.path().join("project"));
    fs::create_dir_all(&project).expect("create project");
    let project_workflows = project.join(".codex").join("workflows");
    write_workflow(&project_workflows, "first", "first");
    let config = WorkflowExtensionConfig {
        project_root_markers: Vec::new(),
        ..config(&temp, project.clone())
    };
    let manager = Arc::new(EnvironmentManager::without_environments());
    let first_thread = start_thread(Arc::clone(&manager), config.clone(), &[]).await;
    let (first_snapshot, concurrent_snapshot) = tokio::join!(
        workflow_session_registry(&first_thread),
        workflow_session_registry(&first_thread),
    );
    let first_snapshot = first_snapshot
        .expect("workflow state")
        .expect("first snapshot");
    let concurrent_snapshot = concurrent_snapshot
        .expect("workflow state")
        .expect("concurrent snapshot");
    assert!(Arc::ptr_eq(&first_snapshot, &concurrent_snapshot));
    write_workflow(&project_workflows, "second", "second");
    let cached_snapshot = workflow_session_registry(&first_thread)
        .await
        .expect("workflow state")
        .expect("cached snapshot");

    assert!(Arc::ptr_eq(&first_snapshot, &cached_snapshot));
    assert_eq!(
        cached_snapshot.registry().names().collect::<Vec<_>>(),
        vec!["first"]
    );

    let second_thread = start_thread(manager, config, &[]).await;
    let refreshed_snapshot = workflow_session_registry(&second_thread)
        .await
        .expect("workflow state")
        .expect("refreshed snapshot");
    assert_eq!(
        refreshed_snapshot.registry().names().collect::<Vec<_>>(),
        vec!["first", "second"]
    );
}
