use super::*;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn workflow_source(name: &str, marker: &str) -> Vec<u8> {
    format!(
        "export const meta = {{ name: '{name}', description: 'saved' }};\n\
         export default async function main() {{ return '{marker}'; }}\n"
    )
    .into_bytes()
}

fn source_identity(name: &str, source: &[u8]) -> WorkflowSaveSourceIdentity {
    WorkflowSaveSourceIdentity::new(
        name,
        codex_workflow_journal::prompt_hash(std::str::from_utf8(source).unwrap()),
    )
}

fn project_save_root(base: &Path) -> WorkflowSaveRoot {
    let canonical_base = std::fs::canonicalize(base).unwrap();
    WorkflowSaveRoot::project(
        AbsolutePathBuf::from_absolute_path(canonical_base).expect("canonical path is absolute"),
    )
}

#[cfg(unix)]
fn personal_save_root(base: &Path) -> WorkflowSaveRoot {
    let canonical_base = std::fs::canonicalize(base).unwrap();
    WorkflowSaveRoot::personal(
        AbsolutePathBuf::from_absolute_path(canonical_base).expect("canonical path is absolute"),
    )
}

fn write_run_script(temp: &TempDir, source: &[u8]) -> PathBuf {
    let run_directory = temp.path().join("runs").join("run-1");
    std::fs::create_dir_all(&run_directory).unwrap();
    std::fs::write(run_directory.join(RUN_SCRIPT_FILE_NAME), source).unwrap();
    run_directory
}

#[tokio::test]
async fn create_copies_exact_script_to_canonical_filename() {
    let temp = TempDir::new().unwrap();
    let source = workflow_source("daily_review", "exact");
    let run_directory = write_run_script(&temp, &source);
    let save_base = temp.path().join("new");
    std::fs::create_dir(&save_base).unwrap();
    let save_root = project_save_root(&save_base);
    let workflow_root = save_root.path().into_path_buf();
    let target = workflow_root.join("daily_review.js");

    let outcome = save_run_workflow(
        &run_directory,
        &save_root,
        &source_identity("daily_review", &source),
        WorkflowSaveMode::Create,
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        WorkflowSaveOutcome::Created {
            path: target.clone()
        }
    );
    assert_eq!(std::fs::read(target).unwrap(), source);
}

#[tokio::test]
async fn create_returns_typed_conflict_without_changing_target() {
    let temp = TempDir::new().unwrap();
    let source = workflow_source("review", "new");
    let run_directory = write_run_script(&temp, &source);
    let save_root = project_save_root(temp.path());
    let workflow_root = save_root.path().into_path_buf();
    std::fs::create_dir_all(&workflow_root).unwrap();
    let target = workflow_root.join("review.js");
    std::fs::write(&target, b"existing").unwrap();

    let outcome = save_run_workflow(
        &run_directory,
        &save_root,
        &source_identity("review", &source),
        WorkflowSaveMode::Create,
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        WorkflowSaveOutcome::Conflict {
            path: target.clone()
        }
    );
    assert_eq!(std::fs::read(target).unwrap(), b"existing");
}

#[tokio::test]
async fn explicit_overwrite_replaces_regular_target_and_can_create() {
    let temp = TempDir::new().unwrap();
    let source = workflow_source("review", "replacement");
    let run_directory = write_run_script(&temp, &source);
    let save_root = project_save_root(temp.path());
    let workflow_root = save_root.path().into_path_buf();
    std::fs::create_dir_all(&workflow_root).unwrap();
    let target = workflow_root.join("review.js");
    std::fs::write(&target, b"existing-longer-contents").unwrap();

    let overwritten = save_run_workflow(
        &run_directory,
        &save_root,
        &source_identity("review", &source),
        WorkflowSaveMode::Overwrite,
    )
    .await
    .unwrap();
    assert_eq!(
        overwritten,
        WorkflowSaveOutcome::Overwritten {
            path: target.clone()
        }
    );
    assert_eq!(std::fs::read(&target).unwrap(), source);

    std::fs::remove_file(&target).unwrap();
    let created = save_run_workflow(
        &run_directory,
        &save_root,
        &source_identity("review", &source),
        WorkflowSaveMode::Overwrite,
    )
    .await
    .unwrap();
    assert_eq!(created, WorkflowSaveOutcome::Created { path: target });
}

#[tokio::test]
async fn overwrite_replaces_a_hard_link_entry_without_mutating_its_sibling() {
    let temp = TempDir::new().unwrap();
    let source = workflow_source("review", "replacement");
    let run_directory = write_run_script(&temp, &source);
    let save_root = project_save_root(temp.path());
    let workflow_root = save_root.path().into_path_buf();
    std::fs::create_dir_all(&workflow_root).unwrap();
    let target = workflow_root.join("review.js");
    let sibling_link = workflow_root.join("previous-source.txt");
    std::fs::write(&target, b"previous source").unwrap();
    std::fs::hard_link(&target, &sibling_link).unwrap();

    let outcome = save_run_workflow(
        &run_directory,
        &save_root,
        &source_identity("review", &source),
        WorkflowSaveMode::Overwrite,
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        WorkflowSaveOutcome::Overwritten {
            path: target.clone()
        }
    );
    assert_eq!(std::fs::read(target).unwrap(), source);
    assert_eq!(std::fs::read(sibling_link).unwrap(), b"previous source");
}

#[tokio::test]
async fn concurrent_overwrites_publish_only_complete_sources() {
    let first_temp = TempDir::new().unwrap();
    let second_temp = TempDir::new().unwrap();
    let target_temp = TempDir::new().unwrap();
    let first_source = workflow_source("review", &"a".repeat(128 * 1024));
    let second_source = workflow_source("review", &"b".repeat(128 * 1024));
    let first_run = write_run_script(&first_temp, &first_source);
    let second_run = write_run_script(&second_temp, &second_source);
    let save_root = project_save_root(target_temp.path());
    let workflow_root = save_root.path().into_path_buf();
    let first_identity = source_identity("review", &first_source);
    let second_identity = source_identity("review", &second_source);

    let (first, second) = tokio::join!(
        save_run_workflow(
            &first_run,
            &save_root,
            &first_identity,
            WorkflowSaveMode::Overwrite,
        ),
        save_run_workflow(
            &second_run,
            &save_root,
            &second_identity,
            WorkflowSaveMode::Overwrite,
        ),
    );

    first.unwrap();
    second.unwrap();
    let published = std::fs::read(workflow_root.join("review.js")).unwrap();
    assert!(published == first_source || published == second_source);
    let entries = std::fs::read_dir(workflow_root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, vec![std::ffi::OsString::from("review.js")]);
}

#[test]
fn workflow_names_are_portable_single_component_stems() {
    for invalid in [
        "",
        "../escape",
        "/absolute",
        r"C:\absolute",
        r"nested\escape",
        "nested/escape",
        "already.js",
        ".hidden",
        "two words",
        "con",
        "LPT9",
        "é",
    ] {
        assert!(
            matches!(
                workflow_file_name(invalid),
                Err(WorkflowSaveError::InvalidName { .. })
            ),
            "{invalid:?} must be rejected"
        );
    }

    let exact = "a".repeat(MAX_SAVE_NAME_BYTES);
    assert_eq!(
        workflow_file_name(&exact).unwrap(),
        format!("{exact}{JAVASCRIPT_FILE_SUFFIX}")
    );
    assert!(matches!(
        workflow_file_name(&"a".repeat(MAX_SAVE_NAME_BYTES + 1)),
        Err(WorkflowSaveError::InvalidName { .. })
    ));
}

#[tokio::test]
async fn source_must_be_bounded_valid_utf8_with_matching_metadata_name() {
    let temp = TempDir::new().unwrap();
    let save_root = project_save_root(temp.path());
    let workflow_root = save_root.path().into_path_buf();

    let mismatch_source = workflow_source("actual", "x");
    let mismatch = write_run_script(&temp, &mismatch_source);
    assert!(matches!(
        save_run_workflow(
            &mismatch,
            &save_root,
            &source_identity("requested", &mismatch_source),
            WorkflowSaveMode::Create
        )
        .await,
        Err(WorkflowSaveError::SourceNameMismatch { .. })
    ));

    let invalid_utf8 = [0xff];
    std::fs::write(mismatch.join(RUN_SCRIPT_FILE_NAME), invalid_utf8).unwrap();
    assert!(matches!(
        save_run_workflow(
            &mismatch,
            &save_root,
            &WorkflowSaveSourceIdentity::new("requested", "blake3:unused"),
            WorkflowSaveMode::Create
        )
        .await,
        Err(WorkflowSaveError::InvalidWorkflowSource { .. })
    ));

    let mut oversized = workflow_source("requested", "x");
    oversized.resize(WORKFLOW_SOURCE_MAX_BYTES as usize + 1, b' ');
    std::fs::write(mismatch.join(RUN_SCRIPT_FILE_NAME), &oversized).unwrap();
    assert!(matches!(
        save_run_workflow(
            &mismatch,
            &save_root,
            &source_identity("requested", &oversized),
            WorkflowSaveMode::Create
        )
        .await,
        Err(WorkflowSaveError::SourceTooLarge { .. })
    ));
    assert!(!workflow_root.exists());
}

#[tokio::test]
async fn source_hash_must_match_durable_identity() {
    let temp = TempDir::new().unwrap();
    let source = workflow_source("review", "exact");
    let run_directory = write_run_script(&temp, &source);
    let save_root = project_save_root(temp.path());
    let workflow_root = save_root.path().into_path_buf();

    assert!(matches!(
        save_run_workflow(
            &run_directory,
            &save_root,
            &WorkflowSaveSourceIdentity::new("review", "blake3:stale"),
            WorkflowSaveMode::Create,
        )
        .await,
        Err(WorkflowSaveError::SourceHashMismatch { .. })
    ));
    assert!(!workflow_root.exists());
}

#[tokio::test]
async fn exact_source_size_limit_is_accepted() {
    let temp = TempDir::new().unwrap();
    let mut source = workflow_source("bounded", "x");
    source.resize(WORKFLOW_SOURCE_MAX_BYTES as usize, b' ');
    let run_directory = write_run_script(&temp, &source);
    let save_root = project_save_root(temp.path());
    let workflow_root = save_root.path().into_path_buf();

    save_run_workflow(
        &run_directory,
        &save_root,
        &source_identity("bounded", &source),
        WorkflowSaveMode::Create,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read(workflow_root.join("bounded.js")).unwrap(),
        source
    );
}

#[tokio::test]
async fn non_regular_source_and_target_are_rejected() {
    let temp = TempDir::new().unwrap();
    let run_directory = temp.path().join("run");
    std::fs::create_dir(&run_directory).unwrap();
    std::fs::create_dir(run_directory.join(RUN_SCRIPT_FILE_NAME)).unwrap();
    let save_root = project_save_root(temp.path());
    let workflow_root = save_root.path().into_path_buf();

    assert!(matches!(
        save_run_workflow(
            &run_directory,
            &save_root,
            &WorkflowSaveSourceIdentity::new("review", "blake3:unused"),
            WorkflowSaveMode::Create
        )
        .await,
        Err(WorkflowSaveError::InvalidSource { .. })
    ));

    std::fs::remove_dir(run_directory.join(RUN_SCRIPT_FILE_NAME)).unwrap();
    let source = workflow_source("review", "x");
    std::fs::write(run_directory.join(RUN_SCRIPT_FILE_NAME), &source).unwrap();
    std::fs::create_dir_all(&workflow_root).unwrap();
    std::fs::create_dir(workflow_root.join("review.js")).unwrap();
    assert!(matches!(
        save_run_workflow(
            &run_directory,
            &save_root,
            &source_identity("review", &source),
            WorkflowSaveMode::Overwrite
        )
        .await,
        Err(WorkflowSaveError::InvalidTarget { .. })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_sources_roots_and_targets_are_rejected_without_touching_referents() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let real_run = temp.path().join("real-run");
    std::fs::create_dir(&real_run).unwrap();
    let source = workflow_source("review", "x");
    std::fs::write(real_run.join(RUN_SCRIPT_FILE_NAME), &source).unwrap();
    let identity = source_identity("review", &source);
    let linked_run = temp.path().join("linked-run");
    symlink(&real_run, &linked_run).unwrap();
    let safe_base = temp.path().join("safe-base");
    std::fs::create_dir(&safe_base).unwrap();
    let save_root = project_save_root(&safe_base);
    assert!(matches!(
        save_run_workflow(&linked_run, &save_root, &identity, WorkflowSaveMode::Create).await,
        Err(WorkflowSaveError::InvalidSource { .. })
    ));

    let source_link_run = temp.path().join("source-link-run");
    std::fs::create_dir(&source_link_run).unwrap();
    symlink(
        real_run.join(RUN_SCRIPT_FILE_NAME),
        source_link_run.join(RUN_SCRIPT_FILE_NAME),
    )
    .unwrap();
    assert!(matches!(
        save_run_workflow(
            &source_link_run,
            &save_root,
            &identity,
            WorkflowSaveMode::Create
        )
        .await,
        Err(WorkflowSaveError::InvalidSource { .. })
    ));

    let linked_root_base = temp.path().join("linked-root-base");
    let linked_root_parent = linked_root_base.join(".codex");
    std::fs::create_dir_all(&linked_root_parent).unwrap();
    let real_root = temp.path().join("real-root");
    std::fs::create_dir(&real_root).unwrap();
    symlink(&real_root, linked_root_parent.join("workflows")).unwrap();
    let linked_root = project_save_root(&linked_root_base);
    assert!(matches!(
        save_run_workflow(&real_run, &linked_root, &identity, WorkflowSaveMode::Create).await,
        Err(WorkflowSaveError::InvalidRoot { .. })
    ));

    let linked_ancestor_base = temp.path().join("linked-ancestor-base");
    std::fs::create_dir(&linked_ancestor_base).unwrap();
    let real_ancestor = temp.path().join("real-ancestor");
    let existing_beneath_ancestor = real_ancestor.join("workflows");
    std::fs::create_dir_all(&existing_beneath_ancestor).unwrap();
    symlink(&real_ancestor, linked_ancestor_base.join(".codex")).unwrap();
    let existing_beneath_link = project_save_root(&linked_ancestor_base);
    assert!(matches!(
        save_run_workflow(
            &real_run,
            &existing_beneath_link,
            &identity,
            WorkflowSaveMode::Create,
        )
        .await,
        Err(WorkflowSaveError::InvalidRoot { .. })
    ));
    assert!(!existing_beneath_ancestor.join("review.js").exists());

    let linked_missing_base = temp.path().join("linked-missing-base");
    std::fs::create_dir(&linked_missing_base).unwrap();
    let missing_referent = temp.path().join("missing-referent");
    std::fs::create_dir(&missing_referent).unwrap();
    symlink(&missing_referent, linked_missing_base.join(".codex")).unwrap();
    let nested_missing_root = project_save_root(&linked_missing_base);
    assert!(matches!(
        save_run_workflow(
            &real_run,
            &nested_missing_root,
            &identity,
            WorkflowSaveMode::Create,
        )
        .await,
        Err(WorkflowSaveError::InvalidRoot { .. })
    ));
    assert!(!missing_referent.join("workflows").exists());

    let referent = temp.path().join("referent.js");
    std::fs::write(&referent, b"do not touch").unwrap();
    let target_base = temp.path().join("target-base");
    let target_root = target_base.join(".codex").join("workflows");
    std::fs::create_dir_all(&target_root).unwrap();
    symlink(&referent, target_root.join("review.js")).unwrap();
    let target_save_root = project_save_root(&target_base);
    assert!(matches!(
        save_run_workflow(
            &real_run,
            &target_save_root,
            &identity,
            WorkflowSaveMode::Overwrite
        )
        .await,
        Err(WorkflowSaveError::InvalidTarget { .. })
    ));
    assert_eq!(std::fs::read(referent).unwrap(), b"do not touch");
}

#[cfg(unix)]
#[tokio::test]
async fn created_directories_and_files_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let source = workflow_source("private", "x");
    let run_directory = write_run_script(&temp, &source);
    let save_root = project_save_root(temp.path());
    let parent = temp.path().join(".codex");
    let workflow_root = save_root.path().into_path_buf();

    save_run_workflow(
        &run_directory,
        &save_root,
        &source_identity("private", &source),
        WorkflowSaveMode::Create,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&workflow_root)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(workflow_root.join("private.js"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[tokio::test]
async fn personal_existing_writable_directories_are_rejected_without_mutation() {
    use std::os::unix::fs::PermissionsExt;

    for unsafe_component in [".agents", "workflows"] {
        let temp = TempDir::new().unwrap();
        let source = workflow_source("private", "x");
        let run_directory = write_run_script(&temp, &source);
        let agents_root = temp.path().join(".agents");
        let workflow_root = agents_root.join("workflows");
        std::fs::create_dir(&agents_root).unwrap();
        std::fs::set_permissions(&agents_root, std::fs::Permissions::from_mode(0o755)).unwrap();
        if unsafe_component == "workflows" {
            std::fs::create_dir(&workflow_root).unwrap();
            std::fs::set_permissions(&workflow_root, std::fs::Permissions::from_mode(0o777))
                .unwrap();
        } else {
            std::fs::set_permissions(&agents_root, std::fs::Permissions::from_mode(0o777)).unwrap();
        }

        let result = save_run_workflow(
            &run_directory,
            &personal_save_root(temp.path()),
            &source_identity("private", &source),
            WorkflowSaveMode::Create,
        )
        .await;

        assert!(matches!(result, Err(WorkflowSaveError::InvalidRoot { .. })));
        assert_eq!(
            std::fs::metadata(&agents_root)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            if unsafe_component == ".agents" {
                0o777
            } else {
                0o755
            }
        );
        if unsafe_component == ".agents" {
            assert!(!workflow_root.exists());
        } else {
            assert_eq!(
                std::fs::metadata(&workflow_root)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o777
            );
            assert_eq!(std::fs::read_dir(&workflow_root).unwrap().count(), 0);
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn personal_existing_safe_directories_are_preserved() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().unwrap();
    let source = workflow_source("private", "x");
    let run_directory = write_run_script(&temp, &source);
    let agents_root = temp.path().join(".agents");
    let workflow_root = agents_root.join("workflows");
    std::fs::create_dir_all(&workflow_root).unwrap();
    std::fs::set_permissions(&agents_root, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&workflow_root, std::fs::Permissions::from_mode(0o755)).unwrap();

    let result = save_run_workflow(
        &run_directory,
        &personal_save_root(temp.path()),
        &source_identity("private", &source),
        WorkflowSaveMode::Create,
    )
    .await
    .unwrap();

    assert_eq!(
        result,
        WorkflowSaveOutcome::Created {
            path: workflow_root.join("private.js")
        }
    );
    assert_eq!(
        std::fs::metadata(&agents_root)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::metadata(&workflow_root)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        std::fs::metadata(workflow_root.join("private.js"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
