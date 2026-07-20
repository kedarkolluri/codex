use super::*;
use codex_utils_absolute_path::AbsolutePathBuf;
use tempfile::TempDir;

fn project_root(temp: &TempDir) -> WorkflowSaveRoot {
    let canonical = std::fs::canonicalize(temp.path()).unwrap();
    WorkflowSaveRoot::project(
        AbsolutePathBuf::from_absolute_path(canonical).expect("canonical path is absolute"),
    )
}

#[cfg(unix)]
#[test]
fn held_directory_capability_prevents_parent_swap_redirection() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let root = project_root(&temp);
    let prepared = prepare_workflow_root(&root).unwrap();
    let registry_parent = temp.path().join(".codex");
    let moved_registry_parent = temp.path().join("moved-codex");
    let attacker_registry_parent = temp.path().join("attacker-codex");
    std::fs::create_dir(&attacker_registry_parent).unwrap();

    std::fs::rename(&registry_parent, &moved_registry_parent).unwrap();
    symlink(&attacker_registry_parent, &registry_parent).unwrap();

    create_workflow(&prepared, "review.js", b"verified source").unwrap();

    assert_eq!(
        std::fs::read(moved_registry_parent.join("workflows/review.js")).unwrap(),
        b"verified source"
    );
    assert!(
        !attacker_registry_parent
            .join("workflows/review.js")
            .exists()
    );
}

#[cfg(windows)]
#[test]
fn held_directory_capability_prevents_parent_swap_redirection() {
    let temp = TempDir::new().unwrap();
    let root = project_root(&temp);
    let prepared = prepare_workflow_root(&root).unwrap();
    let registry_parent = temp.path().join(".codex");
    let moved_registry_parent = temp.path().join("moved-codex");

    let moved = std::fs::rename(&registry_parent, &moved_registry_parent).is_ok();
    if moved {
        std::fs::create_dir_all(registry_parent.join("workflows")).unwrap();
    }

    create_workflow(&prepared, "review.js", b"verified source").unwrap();

    let held_target = if moved {
        moved_registry_parent.join("workflows/review.js")
    } else {
        registry_parent.join("workflows/review.js")
    };
    assert_eq!(std::fs::read(held_target).unwrap(), b"verified source");
    if moved {
        assert!(!registry_parent.join("workflows/review.js").exists());
    }
}
