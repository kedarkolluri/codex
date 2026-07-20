use std::fs;
use std::path::Path;
use std::process::Command;
use std::process::Output;

use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::WorktreeCleanupOutcome;
use super::worktree_add;
use crate::GitToolingError;

const DISABLED_HOOKS_PATH: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

struct RepositoryFixture {
    _temp: TempDir,
    repo: AbsolutePathBuf,
    worktrees: AbsolutePathBuf,
}

impl RepositoryFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temp directory");
        let repo = absolute(temp.path().join("repo"));
        let worktrees = absolute(temp.path().join("worktrees"));
        fs::create_dir_all(&repo).expect("create repository directory");
        fs::create_dir_all(&worktrees).expect("create worktree directory");

        git_ok(&repo, &["init", "--initial-branch=main"]);
        git_ok(&repo, &["config", "user.name", "Codex Test"]);
        git_ok(&repo, &["config", "user.email", "codex-test@example.com"]);
        git_ok(&repo, &["config", "commit.gpgsign", "false"]);
        fs::write(repo.join("tracked.txt"), "baseline\n").expect("write tracked file");
        fs::write(repo.join(".gitignore"), "*.ignored\n").expect("write ignore file");
        git_ok(&repo, &["add", "--", "."]);
        git_ok(&repo, &["commit", "-m", "initial"]);

        Self {
            _temp: temp,
            repo,
            worktrees,
        }
    }

    fn destination(&self, index: u64) -> AbsolutePathBuf {
        self.worktrees.join(format!("agent-{index}"))
    }
}

#[test]
fn explicit_close_removes_an_unchanged_detached_worktree_idempotently() {
    let fixture = RepositoryFixture::new();
    let destination = fixture.destination(7);
    let mut guard = worktree_add(&fixture.repo, destination.clone()).expect("add worktree");

    assert_eq!(guard.path(), &destination);
    assert_eq!(
        guard.canonical_path(),
        &destination.canonicalize().expect("canonical worktree path")
    );
    let expected_git_dir =
        absolute(git_stdout(&destination, &["rev-parse", "--absolute-git-dir"]).trim());
    assert_eq!(guard.git_dir(), &expected_git_dir);
    assert!(destination.join(".git").is_file());
    assert!(
        !git(&destination, &["symbolic-ref", "-q", "HEAD"])
            .status
            .success()
    );

    assert_eq!(
        guard.close().expect("close clean worktree"),
        WorktreeCleanupOutcome::Removed
    );
    assert!(!destination.exists());
    assert_eq!(
        guard.close().expect("close worktree again"),
        WorktreeCleanupOutcome::AlreadyClosed
    );
    assert!(
        !git_stdout(&fixture.repo, &["worktree", "list", "--porcelain"])
            .contains(destination.to_string_lossy().as_ref())
    );
}

#[test]
fn explicit_close_retains_a_dirty_worktree_with_a_diagnostic() {
    let fixture = RepositoryFixture::new();
    let destination = fixture.destination(1);
    let mut guard = worktree_add(&fixture.repo, destination.clone()).expect("add worktree");
    fs::write(destination.join("tracked.txt"), "changed\n").expect("modify tracked file");
    fs::write(destination.join("notes.txt"), "partial output\n").expect("write untracked file");

    let outcome = guard.close().expect("retain dirty worktree");
    let WorktreeCleanupOutcome::RetainedDirty { diagnostic } = outcome else {
        panic!("expected dirty retention, got {outcome:?}");
    };
    assert!(diagnostic.contains("tracked.txt"));
    assert!(diagnostic.contains("notes.txt"));
    assert!(destination.exists());
    assert_eq!(
        guard.close().expect("release retained worktree again"),
        WorktreeCleanupOutcome::AlreadyClosed
    );
}

#[test]
fn drop_removes_clean_worktrees_and_retains_partially_written_worktrees() {
    let fixture = RepositoryFixture::new();
    let clean_destination = fixture.destination(10);
    let dirty_destination = fixture.destination(11);

    {
        let _guard =
            worktree_add(&fixture.repo, clean_destination.clone()).expect("add clean worktree");
    }
    assert!(!clean_destination.exists());

    {
        let _guard =
            worktree_add(&fixture.repo, dirty_destination.clone()).expect("add dirty worktree");
        fs::write(dirty_destination.join("partial.txt"), "incomplete").expect("write partial file");
    }
    assert!(dirty_destination.join("partial.txt").exists());
}

#[test]
fn ignored_build_artifacts_do_not_prevent_cleanup() {
    let fixture = RepositoryFixture::new();
    let destination = fixture.destination(4);
    let mut guard = worktree_add(&fixture.repo, destination.clone()).expect("add worktree");
    fs::write(destination.join("cache.ignored"), "generated").expect("write ignored artifact");

    assert_eq!(
        guard.close().expect("close worktree with ignored file"),
        WorktreeCleanupOutcome::Removed
    );
    assert!(!destination.exists());
}

#[test]
fn a_clean_commit_is_retained_as_a_change_from_the_baseline() {
    let fixture = RepositoryFixture::new();
    let destination = fixture.destination(12);
    let mut guard = worktree_add(&fixture.repo, destination.clone()).expect("add worktree");
    fs::write(destination.join("tracked.txt"), "committed change\n").expect("modify tracked file");
    git_ok(&destination, &["add", "--", "tracked.txt"]);
    git_ok(&destination, &["commit", "-m", "detached change"]);

    let outcome = guard.close().expect("retain committed worktree");
    let WorktreeCleanupOutcome::RetainedDirty { diagnostic } = outcome else {
        panic!("expected changed HEAD retention, got {outcome:?}");
    };
    assert!(diagnostic.contains("HEAD changed"));
    assert!(destination.exists());
}

#[test]
fn distinct_index_derived_destinations_create_distinct_worktrees() {
    let fixture = RepositoryFixture::new();
    let first_path = fixture.destination(20);
    let repeated_first_path = fixture.destination(20);
    let second_path = fixture.destination(21);
    assert_eq!(first_path, repeated_first_path);
    assert_ne!(first_path, second_path);

    let first = worktree_add(&fixture.repo, first_path.clone()).expect("add first worktree");
    let second = worktree_add(&fixture.repo, second_path.clone()).expect("add second worktree");
    assert!(first_path.join(".git").is_file());
    assert!(second_path.join(".git").is_file());

    drop((first, second));
    assert!(!first_path.exists());
    assert!(!second_path.exists());
}

#[test]
fn rejects_non_repositories_non_root_paths_and_unsafe_destinations() {
    let fixture = RepositoryFixture::new();
    let non_repo = fixture.worktrees.join("not-a-repo");
    fs::create_dir(&non_repo).expect("create non-repository");
    let err = worktree_add(&non_repo, fixture.destination(30)).expect_err("reject non-repo");
    assert!(matches!(err, GitToolingError::NotAGitRepository { .. }));

    let nested_repo_path = fixture.repo.join("nested");
    fs::create_dir(&nested_repo_path).expect("create nested repository path");
    let err =
        worktree_add(&nested_repo_path, fixture.destination(31)).expect_err("reject non-root path");
    assert!(matches!(
        err,
        GitToolingError::InvalidGitRepositoryRoot { .. }
    ));

    let existing_destination = fixture.destination(32);
    fs::create_dir(&existing_destination).expect("create existing destination");
    let err =
        worktree_add(&fixture.repo, existing_destination).expect_err("reject existing destination");
    assert!(matches!(
        err,
        GitToolingError::InvalidWorktreeDestination { .. }
    ));

    let inside_repo = fixture.repo.join("isolated-agent");
    let err =
        worktree_add(&fixture.repo, inside_repo).expect_err("reject destination inside repository");
    assert!(matches!(
        err,
        GitToolingError::InvalidWorktreeDestination { .. }
    ));
}

fn absolute(path: impl AsRef<Path>) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path_checked(path).expect("path should be absolute")
}

fn git_ok(cwd: &AbsolutePathBuf, args: &[&str]) {
    let output = git(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(cwd: &AbsolutePathBuf, args: &[&str]) -> String {
    let output = git(cwd, args);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git output should be UTF-8")
}

fn git(cwd: &AbsolutePathBuf, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-c")
        .arg(format!("core.hooksPath={DISABLED_HOOKS_PATH}"))
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .output()
        .expect("run git")
}
