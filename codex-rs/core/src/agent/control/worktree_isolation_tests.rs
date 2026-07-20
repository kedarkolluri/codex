use std::fs;
use std::path::Path;
use std::process::Command;
use std::process::Output;
use std::sync::Arc;
use std::sync::Barrier;

use codex_exec_server::Environment;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_exec_server::REMOTE_ENVIRONMENT_ID;
use codex_git_utils::GitToolingError;
use codex_git_utils::WorktreeCleanupOutcome;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::turn_context::TurnEnvironment;

use super::WorkflowAgentIsolation;
use super::WorktreeAllocation;
use super::WorktreeExecutionEnvironment;
use super::WorktreeIsolationError;

const DISABLED_HOOKS_PATH: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

struct RepositoryFixture {
    _temp: TempDir,
    repo: AbsolutePathBuf,
    nested_cwd: AbsolutePathBuf,
}

#[test]
fn isolation_validation_accepts_only_omitted_or_exact_worktree() {
    assert_eq!(
        WorkflowAgentIsolation::parse(None).expect("accept omitted isolation"),
        WorkflowAgentIsolation::Inherited
    );
    assert_eq!(
        WorkflowAgentIsolation::parse(Some("worktree")).expect("accept worktree isolation"),
        WorkflowAgentIsolation::Worktree
    );
    for invalid in ["", "Worktree", "worktree ", "shared"] {
        let err = WorkflowAgentIsolation::parse(Some(invalid))
            .expect_err("reject unknown isolation value");
        assert_eq!(
            err.to_string(),
            format!(
                "unsupported workflow agent isolation `{invalid}`; expected exact `worktree` or an omitted option"
            )
        );
    }
}

#[tokio::test]
async fn worktree_execution_environment_rejects_remote_and_uses_selected_cwd() {
    let config_fixture = RepositoryFixture::new();
    let selected_fixture = RepositoryFixture::new();
    let isolated_cwd = selected_fixture.repo.join("isolated-checkout");
    let cwd = PathUri::from_abs_path(&selected_fixture.nested_cwd);
    let local = TurnEnvironment::new(
        LOCAL_ENVIRONMENT_ID.to_string(),
        Arc::new(Environment::default_for_tests()),
        cwd.clone(),
        /*shell*/ None,
    );
    let remote = TurnEnvironment::new(
        REMOTE_ENVIRONMENT_ID.to_string(),
        Arc::new(
            Environment::create_for_tests(Some("ws://127.0.0.1:8765".to_string()))
                .expect("remote environment"),
        ),
        cwd,
        /*shell*/ None,
    );

    let admitted = WorktreeExecutionEnvironment::from_snapshot(&TurnEnvironmentSnapshot {
        turn_environments: vec![local.clone()],
        starting: Vec::new(),
    })
    .expect("one ready local environment is safe");
    assert_eq!(
        admitted.selection_at(&isolated_cwd),
        codex_protocol::protocol::TurnEnvironmentSelection {
            environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
            cwd: PathUri::from_abs_path(&isolated_cwd),
        }
    );
    let allocation = admitted
        .allocation("run-selected-cwd", 9, 0)
        .expect("derive allocation from selected local environment");
    assert_eq!(
        &allocation.repo_root,
        &selected_fixture.repo.canonicalize().expect("selected repo")
    );
    assert_ne!(
        &allocation.repo_root,
        &config_fixture.repo.canonicalize().expect("config repo"),
        "a stale Config.cwd repository must not control worktree allocation"
    );

    for environments in [vec![remote.clone()], vec![local, remote]] {
        let err = WorktreeExecutionEnvironment::from_snapshot(&TurnEnvironmentSnapshot {
            turn_environments: environments,
            starting: Vec::new(),
        })
        .expect_err("host-local checkout must reject every remote execution lane");
        assert!(matches!(
            err,
            WorktreeIsolationError::UnsupportedExecutionEnvironments
        ));
    }
}

impl RepositoryFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temporary directory");
        let repo = absolute(temp.path().join("repo"));
        let nested_cwd = repo.join("nested").join("cwd");
        fs::create_dir_all(&nested_cwd).expect("create nested cwd");
        git_ok(&repo, &["init", "--initial-branch=main"]);
        git_ok(&repo, &["config", "user.name", "Codex Test"]);
        git_ok(&repo, &["config", "user.email", "codex-test@example.com"]);
        git_ok(&repo, &["config", "commit.gpgsign", "false"]);
        fs::write(repo.join("tracked.txt"), "baseline\n").expect("write tracked file");
        git_ok(&repo, &["add", "--", "."]);
        git_ok(&repo, &["commit", "-m", "initial"]);
        Self {
            _temp: temp,
            repo,
            nested_cwd,
        }
    }
}

#[test]
fn allocation_is_deterministic_distinct_and_outside_the_exact_repo_root() {
    let fixture = RepositoryFixture::new();
    let first = WorktreeAllocation::for_workflow_agent(
        &fixture.nested_cwd,
        "0192f000-0000-7000-8000-000000000000",
        3,
        0,
    )
    .expect("derive first allocation");
    let repeated = WorktreeAllocation::for_workflow_agent(
        &fixture.nested_cwd,
        "0192f000-0000-7000-8000-000000000000",
        3,
        0,
    )
    .expect("derive repeated allocation");
    let second = WorktreeAllocation::for_workflow_agent(
        &fixture.nested_cwd,
        "0192f000-0000-7000-8000-000000000000",
        4,
        0,
    )
    .expect("derive second allocation");

    assert_eq!(first, repeated);
    assert_ne!(first.destination, second.destination);
    assert_eq!(
        &first.repo_root,
        &fixture.repo.canonicalize().expect("canonical repo")
    );
    assert!(!first.destination.starts_with(&first.repo_root));
    assert!(!first.repo_root.starts_with(&first.destination));
}

#[test]
fn invalid_identity_and_non_git_cwd_fail_before_checkout_creation() {
    let fixture = RepositoryFixture::new();
    let err = WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "../same", 0, 0)
        .expect_err("reject unsafe identity");
    assert!(matches!(err, WorktreeIsolationError::InvalidRunId { .. }));

    let non_git = fixture
        .repo
        .parent()
        .expect("repository parent")
        .join("not-git");
    fs::create_dir(&non_git).expect("create non-git directory");
    let err = WorktreeAllocation::for_workflow_agent(&non_git, "run-valid", 0, 0)
        .expect_err("reject non-git cwd");
    assert!(matches!(
        err,
        WorktreeIsolationError::NonGitWorkingDirectory { .. }
    ));
}

#[test]
fn preexisting_unowned_allocation_root_is_rejected_without_removal() {
    let fixture = RepositoryFixture::new();
    let allocation =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-collision", 8, 0)
            .expect("derive allocation");
    let allocation_claim = allocation.allocation_claim.clone();
    fs::create_dir(&allocation.allocation_root).expect("reserve unowned allocation root");

    let err = allocation
        .create()
        .expect_err("reject unowned allocation root");
    assert!(matches!(
        err,
        WorktreeIsolationError::UnownedAllocationRoot { .. }
    ));
    assert!(
        fixture
            .repo
            .parent()
            .expect("repository parent")
            .join(".codex-worktrees-run-collision")
            .is_dir()
    );
    assert!(!allocation_claim.exists());
}

#[test]
fn existing_deterministic_destination_is_rejected_without_overwrite() {
    let fixture = RepositoryFixture::new();
    let allocation =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-collision", 8, 0)
            .expect("derive allocation");
    let existing = allocation
        .clone()
        .create()
        .expect("create occupied checkout");

    let err = allocation
        .create()
        .expect_err("reject occupied destination");
    assert!(matches!(
        err,
        WorktreeIsolationError::Git(GitToolingError::InvalidWorktreeDestination { .. })
    ));
    assert!(existing.path().exists());
}

#[cfg(unix)]
#[test]
fn symlinked_allocation_root_is_rejected_without_following_it() {
    use std::os::unix::fs::symlink;

    let fixture = RepositoryFixture::new();
    let allocation =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-symlink", 0, 0)
            .expect("derive allocation");
    let redirected = fixture
        .repo
        .parent()
        .expect("repository parent")
        .join("redirected");
    fs::create_dir(&redirected).expect("create redirect target");
    symlink(&redirected, &allocation.allocation_root).expect("create allocation symlink");

    let err = allocation
        .create()
        .expect_err("reject symlinked allocation root");
    assert!(matches!(
        err,
        WorktreeIsolationError::UnownedAllocationRoot { .. }
    ));
    assert!(!redirected.join("agent-0").exists());

    let claimed =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-claim-symlink", 0, 0)
            .expect("derive claimed allocation");
    let claim_target = redirected.join("claim-target");
    fs::write(&claim_target, []).expect("create empty claim target");
    symlink(&claim_target, &claimed.allocation_claim).expect("create allocation claim symlink");
    let err = claimed
        .create()
        .expect_err("reject symlinked allocation claim");
    assert!(matches!(
        err,
        WorktreeIsolationError::UnownedAllocationRoot { .. }
    ));
}

#[test]
fn clean_checkout_is_removed_and_dirty_checkout_is_retained_with_diagnostic() {
    let fixture = RepositoryFixture::new();
    let clean_allocation =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-clean-dirty", 0, 0)
            .expect("derive clean allocation");
    let clean_path = clean_allocation.destination.clone();
    let allocation_root = clean_allocation.allocation_root.clone();
    let allocation_claim = clean_allocation.allocation_claim;
    let mut clean =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-clean-dirty", 0, 0)
            .expect("derive clean allocation")
            .create()
            .expect("create clean checkout");
    assert_eq!(
        clean.close().expect("close clean checkout"),
        WorktreeCleanupOutcome::Removed
    );
    assert!(!clean_path.exists());
    assert!(
        !allocation_root.exists(),
        "the last clean checkout should remove its empty run-scoped allocation directory"
    );
    assert!(!allocation_claim.exists());
    assert_eq!(
        clean.close().expect("close clean checkout again"),
        WorktreeCleanupOutcome::AlreadyClosed
    );

    let mut dirty =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-clean-dirty", 1, 0)
            .expect("derive dirty allocation")
            .create()
            .expect("create dirty checkout");
    fs::write(dirty.path().join("tracked.txt"), "agent mutation\n")
        .expect("mutate isolated checkout");
    let dirty_path = dirty.path().clone();
    let outcome = dirty.close().expect("close dirty checkout");
    let WorktreeCleanupOutcome::RetainedDirty { diagnostic } = outcome else {
        panic!("expected retained dirty checkout, got {outcome:?}");
    };
    assert!(diagnostic.contains("tracked.txt"));
    assert!(dirty_path.exists());
    assert!(allocation_root.exists());
    assert!(allocation_claim.is_file());
    assert_eq!(
        fs::read_to_string(fixture.repo.join("tracked.txt")).expect("read source checkout"),
        "baseline\n"
    );
}

#[test]
fn last_parallel_clean_checkout_removes_the_shared_allocation_directory() {
    let fixture = RepositoryFixture::new();
    let first_allocation =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-clean-parallel", 0, 0)
            .expect("derive first allocation");
    let allocation_root = first_allocation.allocation_root.clone();
    let allocation_claim = first_allocation.allocation_claim.clone();
    let mut first = first_allocation.create().expect("create first checkout");
    let mut second =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-clean-parallel", 1, 0)
            .expect("derive second allocation")
            .create()
            .expect("create second checkout");

    assert_eq!(
        first.close().expect("close first checkout"),
        WorktreeCleanupOutcome::Removed
    );
    assert!(
        allocation_root.exists(),
        "a live sibling must retain the shared allocation directory"
    );
    assert_eq!(
        second.close().expect("close second checkout"),
        WorktreeCleanupOutcome::Removed
    );
    assert!(
        !allocation_root.exists(),
        "the last clean sibling should remove the shared allocation directory"
    );
    assert!(!allocation_claim.exists());
}

#[test]
fn allocation_creation_and_last_clean_close_do_not_race_the_shared_root() {
    let fixture = RepositoryFixture::new();
    let first_allocation =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-create-close", 0, 0)
            .expect("derive first allocation");
    let allocation_root = first_allocation.allocation_root.clone();
    let mut first = first_allocation.create().expect("create first checkout");
    let second_allocation =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-create-close", 1, 0)
            .expect("derive second allocation");
    let barrier = Arc::new(Barrier::new(2));

    std::thread::scope(|scope| {
        let create_barrier = Arc::clone(&barrier);
        let second = scope.spawn(move || {
            create_barrier.wait();
            second_allocation.create().expect("create racing checkout")
        });
        barrier.wait();
        assert_eq!(
            first.close().expect("close first checkout"),
            WorktreeCleanupOutcome::Removed
        );
        let mut second = second.join().expect("join racing creation");
        assert_eq!(
            second.close().expect("close second checkout"),
            WorktreeCleanupOutcome::Removed
        );
    });

    assert!(!allocation_root.exists());
}

#[test]
fn allocation_claim_recovers_a_crash_before_root_creation() {
    let fixture = RepositoryFixture::new();
    let allocation =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-crash-recovery", 0, 0)
            .expect("derive allocation");
    super::namespace::prepare_allocation_root(
        &allocation.allocation_root,
        &allocation.allocation_claim,
        &allocation.allocation_tombstone,
    )
    .expect("prepare claimed allocation root");
    fs::remove_dir(&allocation.allocation_root)
        .expect("simulate crash state with claim but no allocation root");
    assert!(!allocation.allocation_root.exists());

    let allocation_root = allocation.allocation_root.clone();
    let allocation_claim = allocation.allocation_claim.clone();
    let mut guard = allocation
        .create()
        .expect("recover claimed allocation root");
    assert!(guard.path().is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        assert_eq!(
            fs::metadata(&allocation_root)
                .expect("allocation root metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&allocation_claim)
                .expect("allocation claim metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert_eq!(
        guard.close().expect("close recovered checkout"),
        WorktreeCleanupOutcome::Removed
    );
    assert!(!allocation_root.exists());
    assert!(!allocation_claim.exists());
}

#[test]
fn cleanup_retries_tombstone_and_claim_transition_failures() {
    let fixture = RepositoryFixture::new();
    let tombstone_failure =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-tombstone-retry", 0, 0)
            .expect("derive tombstone retry allocation");
    super::namespace::prepare_allocation_root(
        &tombstone_failure.allocation_root,
        &tombstone_failure.allocation_claim,
        &tombstone_failure.allocation_tombstone,
    )
    .expect("prepare tombstone retry namespace");
    fs::rename(
        &tombstone_failure.allocation_root,
        &tombstone_failure.allocation_tombstone,
    )
    .expect("simulate completed tombstone transition");
    let blocker = tombstone_failure.allocation_tombstone.join("busy");
    fs::write(&blocker, "busy\n").expect("block tombstone removal");

    let err = super::namespace::remove_allocation_root_if_empty(
        &tombstone_failure.allocation_root,
        &tombstone_failure.allocation_claim,
        &tombstone_failure.allocation_tombstone,
    )
    .expect_err("retain non-empty tombstone for retry");
    assert!(matches!(
        err,
        WorktreeIsolationError::UnownedAllocationRoot { .. }
    ));
    assert!(tombstone_failure.allocation_claim.is_file());
    assert!(tombstone_failure.allocation_tombstone.is_dir());
    fs::remove_file(blocker).expect("release tombstone removal");
    super::namespace::remove_allocation_root_if_empty(
        &tombstone_failure.allocation_root,
        &tombstone_failure.allocation_claim,
        &tombstone_failure.allocation_tombstone,
    )
    .expect("retry tombstone and claim cleanup");
    assert!(!tombstone_failure.allocation_tombstone.exists());
    assert!(!tombstone_failure.allocation_claim.exists());

    let claim_failure =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-claim-retry", 0, 0)
            .expect("derive claim retry allocation");
    super::namespace::prepare_allocation_root(
        &claim_failure.allocation_root,
        &claim_failure.allocation_claim,
        &claim_failure.allocation_tombstone,
    )
    .expect("prepare claim retry namespace");
    fs::rename(
        &claim_failure.allocation_root,
        &claim_failure.allocation_tombstone,
    )
    .expect("simulate claim cleanup transition");
    fs::remove_dir(&claim_failure.allocation_tombstone)
        .expect("simulate removed cleanup tombstone");
    fs::write(&claim_failure.allocation_claim, "invalid")
        .expect("make claim temporarily unsafe to remove");

    let err = super::namespace::remove_allocation_root_if_empty(
        &claim_failure.allocation_root,
        &claim_failure.allocation_claim,
        &claim_failure.allocation_tombstone,
    )
    .expect_err("retain invalid claim for retry");
    assert!(matches!(
        err,
        WorktreeIsolationError::UnownedAllocationRoot { .. }
    ));
    fs::write(&claim_failure.allocation_claim, []).expect("restore empty ownership claim");
    super::namespace::remove_allocation_root_if_empty(
        &claim_failure.allocation_root,
        &claim_failure.allocation_claim,
        &claim_failure.allocation_tombstone,
    )
    .expect("retry claim cleanup");
    assert!(!claim_failure.allocation_claim.exists());
}

#[test]
fn wrapper_close_retries_namespace_cleanup_after_checkout_is_closed() {
    let fixture = RepositoryFixture::new();
    let allocation = WorktreeAllocation::for_workflow_agent(
        &fixture.nested_cwd,
        "run-wrapper-cleanup-retry",
        0,
        0,
    )
    .expect("derive wrapper cleanup retry allocation");
    let allocation_root = allocation.allocation_root.clone();
    let allocation_claim = allocation.allocation_claim.clone();
    let allocation_tombstone = allocation.allocation_tombstone.clone();
    let mut guard = allocation.create().expect("create clean checkout");
    let checkout = guard.path().clone();
    fs::create_dir(&allocation_tombstone).expect("create blocking tombstone");
    let blocker = allocation_tombstone.join("busy");
    fs::write(&blocker, "busy\n").expect("block first namespace cleanup");

    let err = guard
        .close()
        .expect_err("first close must surface blocked tombstone cleanup");
    assert!(matches!(
        err,
        WorktreeIsolationError::UnownedAllocationRoot { .. }
    ));
    assert!(!checkout.exists(), "inner Git worktree is already closed");
    assert!(allocation_root.is_dir());
    assert!(allocation_claim.is_file());
    assert!(allocation_tombstone.is_dir());

    fs::remove_file(blocker).expect("release tombstone cleanup");
    assert_eq!(
        guard.close().expect("retry namespace cleanup"),
        WorktreeCleanupOutcome::AlreadyClosed
    );
    assert!(!allocation_root.exists());
    assert!(!allocation_claim.exists());
    assert!(!allocation_tombstone.exists());
}

#[test]
fn failed_git_creation_removes_a_fresh_allocation_namespace() {
    let temp = tempfile::tempdir().expect("create temporary directory");
    let repo = absolute(temp.path().join("unborn-repo"));
    fs::create_dir(&repo).expect("create unborn repository directory");
    git_ok(&repo, &["init", "--initial-branch=main"]);
    let allocation = WorktreeAllocation::for_workflow_agent(&repo, "run-unborn", 0, 0)
        .expect("derive allocation for unborn repository");
    let allocation_root = allocation.allocation_root.clone();
    let allocation_claim = allocation.allocation_claim.clone();

    let err = allocation
        .create()
        .expect_err("Git must reject a detached worktree without HEAD");
    assert!(matches!(err, WorktreeIsolationError::Git(_)));
    assert!(!allocation_root.exists());
    assert!(!allocation_claim.exists());
}

#[test]
fn dirty_drop_and_replaced_checkout_preserve_the_claimed_namespace() {
    let fixture = RepositoryFixture::new();
    let dirty_allocation =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-drop-dirty", 0, 0)
            .expect("derive dirty allocation");
    let dirty_root = dirty_allocation.allocation_root.clone();
    let dirty_claim = dirty_allocation.allocation_claim.clone();
    let dirty_path = dirty_allocation.destination.clone();
    {
        let dirty = dirty_allocation.create().expect("create dirty checkout");
        fs::write(dirty.path().join("partial.txt"), "partial\n")
            .expect("write partial child output");
    }
    assert!(dirty_path.exists());
    assert!(dirty_root.exists());
    assert!(dirty_claim.is_file());

    let replaced_allocation =
        WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-replaced-checkout", 0, 0)
            .expect("derive replaced allocation");
    let replaced_root = replaced_allocation.allocation_root.clone();
    let replaced_claim = replaced_allocation.allocation_claim.clone();
    let mut replaced = replaced_allocation
        .create()
        .expect("create checkout to replace");
    let original_path = replaced.path().clone();
    let moved_path = replaced_root.join("moved-agent-0");
    fs::rename(&original_path, &moved_path).expect("move original checkout");
    fs::create_dir(&original_path).expect("replace checkout path");

    let outcome = replaced.close().expect("retain replaced checkout identity");
    let WorktreeCleanupOutcome::RetainedDirty { diagnostic } = outcome else {
        panic!("expected replaced checkout retention, got {outcome:?}");
    };
    assert!(diagnostic.contains("metadata"));
    assert!(original_path.exists());
    assert!(moved_path.exists());
    assert!(replaced_root.exists());
    assert!(replaced_claim.is_file());
}

#[test]
fn parallel_ordinals_can_mutate_the_same_relative_file_without_conflict() {
    let fixture = RepositoryFixture::new();
    let first = WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-parallel", 20, 0)
        .expect("derive first allocation")
        .create()
        .expect("create first checkout");
    let second = WorktreeAllocation::for_workflow_agent(&fixture.nested_cwd, "run-parallel", 21, 0)
        .expect("derive second allocation")
        .create()
        .expect("create second checkout");
    let first_path = first.path().clone();
    let second_path = second.path().clone();
    assert_ne!(first_path, second_path);

    let first_writer =
        std::thread::spawn(move || fs::write(first_path.join("tracked.txt"), "first agent\n"));
    let second_writer =
        std::thread::spawn(move || fs::write(second_path.join("tracked.txt"), "second agent\n"));
    first_writer
        .join()
        .expect("join first writer")
        .expect("write first checkout");
    second_writer
        .join()
        .expect("join second writer")
        .expect("write second checkout");

    assert_eq!(
        fs::read_to_string(first.path().join("tracked.txt")).expect("read first checkout"),
        "first agent\n"
    );
    assert_eq!(
        fs::read_to_string(second.path().join("tracked.txt")).expect("read second checkout"),
        "second agent\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.repo.join("tracked.txt")).expect("read source checkout"),
        "baseline\n"
    );
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
