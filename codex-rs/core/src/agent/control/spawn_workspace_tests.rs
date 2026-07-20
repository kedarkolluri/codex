use crate::ThreadManager;
use crate::agent::control::SpawnAgentOptions;
use crate::config::ConfigBuilder;
use crate::init_state_db;
use codex_login::CodexAuth;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use toml::Value as TomlValue;

use super::SpawnAgentWorkspace;

#[tokio::test]
async fn isolated_worktree_override_moves_cwd_roots_and_permission_profile_together() {
    let temp = tempfile::tempdir().expect("create temporary directory");
    let worktree = AbsolutePathBuf::from_absolute_path_checked(temp.path().join("worktree"))
        .expect("worktree path should be absolute");
    let git_dir = AbsolutePathBuf::from_absolute_path_checked(temp.path().join("git-dir"))
        .expect("Git directory path should be absolute");
    let mut config = crate::config::test_config().await;
    let canonical_profile = PermissionProfile::workspace_write();
    config
        .permissions
        .set_permission_profile(canonical_profile.clone())
        .expect("select workspace-write profile");
    let parent_permissions = config.permissions.clone();
    let mut expected_permissions = parent_permissions.clone();
    expected_permissions.set_workspace_roots(vec![worktree.clone()]);
    expected_permissions
        .add_explicit_runtime_readable_root(git_dir.clone())
        .expect("add explicit Git directory read");
    let expected_canonical_profile = canonical_profile.with_explicit_readable_root(git_dir.clone());
    let expected_effective_profile = expected_canonical_profile
        .clone()
        .materialize_project_roots_with_workspace_roots(std::slice::from_ref(&worktree));
    let workspace = SpawnAgentWorkspace::isolated_worktree(
        worktree.clone(),
        git_dir.clone(),
        parent_permissions,
    )
    .expect("build isolated workspace");

    workspace.apply(&mut config);

    assert_eq!(config.cwd, worktree.clone());
    assert_eq!(config.workspace_roots, vec![worktree.clone()]);
    assert!(config.workspace_roots_explicit);
    assert_eq!(config.permissions, expected_permissions);
    assert_eq!(
        config.permissions.workspace_roots(),
        std::slice::from_ref(&worktree)
    );
    assert_eq!(
        config.permissions.permission_profile(),
        &expected_canonical_profile
    );
    assert_eq!(
        config.permissions.effective_permission_profile(),
        expected_effective_profile
    );
    let effective_file_system = config
        .permissions
        .effective_permission_profile()
        .file_system_sandbox_policy();
    let explicit_git_dir_access = effective_file_system
        .entries
        .iter()
        .filter_map(|entry| match &entry.path {
            FileSystemPath::Path { path } if path == &git_dir => Some(entry.access),
            FileSystemPath::Path { .. }
            | FileSystemPath::GlobPattern { .. }
            | FileSystemPath::Special { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(explicit_git_dir_access, vec![FileSystemAccessMode::Read]);
    assert!(effective_file_system.can_read_path_with_cwd(git_dir.as_path(), worktree.as_path()));
    assert!(!effective_file_system.can_write_path_with_cwd(git_dir.as_path(), worktree.as_path()));
}

#[tokio::test]
async fn spawn_path_preserves_exact_parent_permissions_with_isolated_root() {
    let home = tempfile::tempdir().expect("create Codex home");
    let worktree = AbsolutePathBuf::from_absolute_path_checked(home.path().join("worktree"))
        .expect("worktree path should be absolute");
    let git_dir = AbsolutePathBuf::from_absolute_path_checked(home.path().join("git-dir"))
        .expect("Git directory path should be absolute");
    std::fs::create_dir(&worktree).expect("create worktree directory");
    let mut config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .cli_overrides(vec![(
            "model".to_string(),
            TomlValue::String("gpt-5.5".to_string()),
        )])
        .build()
        .await
        .expect("load test config");
    let canonical_profile = PermissionProfile::workspace_write();
    config
        .permissions
        .set_permission_profile(canonical_profile.clone())
        .expect("select workspace-write profile");
    let parent_permissions = config.permissions.clone();
    let mut expected_permissions = parent_permissions.clone();
    expected_permissions.set_workspace_roots(vec![worktree.clone()]);
    expected_permissions
        .add_explicit_runtime_readable_root(git_dir.clone())
        .expect("add explicit Git directory read");
    let expected_canonical_profile = canonical_profile
        .clone()
        .with_explicit_readable_root(git_dir.clone());
    let expected_effective_profile = expected_canonical_profile
        .clone()
        .materialize_project_roots_with_workspace_roots(std::slice::from_ref(&worktree));
    let state_db = init_state_db(&config).await;
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        state_db,
    );
    let control = manager.agent_control();

    let spawned = control
        .spawn_agent_deferred_input(
            config,
            /*session_source*/ None,
            SpawnAgentOptions {
                spawn_workspace: Some(
                    SpawnAgentWorkspace::isolated_worktree(
                        worktree.clone(),
                        git_dir,
                        parent_permissions,
                    )
                    .expect("build isolated workspace"),
                ),
                ..Default::default()
            },
        )
        .await
        .expect("spawn child with isolated workspace");
    let child = manager
        .get_thread(spawned.thread_id)
        .await
        .expect("load spawned child");
    let child_config = child.codex.session.get_config().await;

    assert_eq!(child_config.cwd, worktree.clone());
    assert_eq!(child_config.workspace_roots, vec![worktree.clone()]);
    assert_eq!(child_config.permissions, expected_permissions);
    assert_eq!(child_config.permissions.workspace_roots(), &[worktree]);
    assert_eq!(
        child_config.permissions.permission_profile(),
        &expected_canonical_profile
    );
    assert_eq!(
        child_config.permissions.effective_permission_profile(),
        expected_effective_profile
    );

    control.reap_spawned_child(spawned.thread_id).await;
}
