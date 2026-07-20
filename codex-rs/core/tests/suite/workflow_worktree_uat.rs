#![allow(clippy::expect_used, clippy::unwrap_used)]
//! UAT-8: real process-host workflow worktree isolation.
//!
//! Lower-level tests prove the allocation and child-config helpers independently. This gate drives
//! the assembled path: a saved workflow runs in the real `codex-code-mode-host`, two registered
//! children execute shell tools, and the test observes their tool outputs, captured spawn configs, Git
//! registrations, and durable journal. One unchanged checkout is removed; one intentionally
//! changed checkout is retained with a diagnostic. Neither child mutates the parent repository.
//! This real-process lane deliberately selects one local executor; production rejects worktree
//! isolation before allocation when remote, starting, absent, or multiple executors are selected.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WorkflowEvent;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::skip_if_no_remote_env;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const WORKFLOW_NAME: &str = "uat8-worktree";
const WORKFLOW_CALL_ID: &str = "call-uat8-workflow";
const CLEAN_MARKER: &str = "UAT8_CLEAN";
const DIRTY_MARKER: &str = "UAT8_DIRTY";
const DIRTY_FILE: &str = "isolated-only.txt";
const ESCAPE_FILE: &str = "uat8-escape-from-child.txt";
const ESCAPE_DENIED: &str = "UAT8_ESCAPE_WRITE_DENIED";
const ESCAPE_SUCCEEDED: &str = "UAT8_ESCAPE_WRITE_SUCCEEDED";

const WORKFLOW_SOURCE: &str = r#"export const meta = {
  name: 'uat8-worktree',
  description: 'real process-host worktree isolation',
  phases: ['isolate'],
};
phase('isolate');
const clean = await agent('UAT8_CLEAN inspect isolated cwd', {
  label: 'clean checkout', isolation: 'worktree'
});
const dirty = await agent('UAT8_DIRTY mutate only the isolated checkout', {
  label: 'dirty checkout', isolation: 'worktree'
});
log('uat8 children complete');
text(JSON.stringify([clean, dirty]));
"#;

const REMOTE_FAIL_CLOSED_SOURCE: &str = r#"export const meta = {
  name: 'uat8-worktree',
  description: 'remote worktree isolation must fail before spawning',
  phases: ['reject'],
};
phase('reject');
let rejection = null;
try {
  await agent('UAT8_REMOTE_WORKTREE must never spawn', {
    label: 'remote rejection', phase: 'reject', isolation: 'worktree'
  });
} catch (error) {
  rejection = String(error);
}
if (rejection === null) {
  throw new Error('remote worktree isolation unexpectedly admitted a child');
}
log('UAT8_REMOTE_REJECTION:' + rejection);
text(rejection);
"#;

const REMOTE_REJECTION: &str = "workflow agent worktree setup failed";

#[derive(Clone, Default)]
struct WorktreeRouter {
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Respond for WorktreeRouter {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body = request_body_json(request);
        self.requests.lock().unwrap().push(body.clone());

        if let Some(marker) = child_marker(&body) {
            if contains_function_output(&body) {
                let suffix = if marker == CLEAN_MARKER {
                    "clean"
                } else {
                    "dirty"
                };
                return sse_response(sse(vec![
                    ev_response_created(&format!("resp-{suffix}-done")),
                    ev_assistant_message(
                        &format!("msg-{suffix}-done"),
                        &format!("{suffix}-child-complete"),
                    ),
                    ev_completed(&format!("resp-{suffix}-done")),
                ]));
            }

            let (call_id, command) = if marker == CLEAN_MARKER {
                ("uat8-clean-pwd", clean_checkout_command())
            } else {
                ("uat8-dirty-write", dirty_checkout_command())
            };
            let args = json!({
                "command": command,
                "login": false,
                "timeout_ms": 10_000,
            })
            .to_string();
            return sse_response(sse(vec![
                ev_response_created(&format!("resp-{call_id}")),
                ev_function_call(call_id, "shell_command", &args),
                ev_completed(&format!("resp-{call_id}")),
            ]));
        }

        if contains_workflow_output(&body) {
            return sse_response(sse(vec![
                ev_response_created("resp-parent-done"),
                ev_assistant_message("msg-parent-done", "workflow admitted"),
                ev_completed("resp-parent-done"),
            ]));
        }

        let arguments = json!({"name": WORKFLOW_NAME, "args": {"lane": "uat8"}}).to_string();
        sse_response(sse(vec![
            ev_response_created("resp-parent-open"),
            ev_function_call(WORKFLOW_CALL_ID, "workflow_run", &arguments),
            ev_completed("resp-parent-open"),
        ]))
    }
}

#[cfg(windows)]
fn clean_checkout_command() -> &'static str {
    "Write-Output (Get-Location).Path"
}

#[cfg(not(windows))]
fn clean_checkout_command() -> &'static str {
    "pwd"
}

#[cfg(windows)]
fn dirty_checkout_command() -> &'static str {
    "$line = (git worktree list --porcelain | Select-String '^worktree ' | Select-Object -First 1).Line; $main = $line.Substring(9); Write-Output ('UAT8_MAIN=' + $main); try { Set-Content -NoNewline -Path (Join-Path $main 'uat8-escape-from-child.txt') -Value escaped -ErrorAction Stop; Write-Output 'UAT8_ESCAPE_WRITE_SUCCEEDED' } catch { Write-Output 'UAT8_ESCAPE_WRITE_DENIED' }; Write-Output (Get-Location).Path; Set-Content -NoNewline -Path isolated-only.txt -Value dirty"
}

#[cfg(not(windows))]
fn dirty_checkout_command() -> &'static str {
    "main=$(git worktree list --porcelain | sed -n 's/^worktree //p' | head -n 1); printf 'UAT8_MAIN=%s\\n' \"$main\"; if printf %s escaped > \"$main/uat8-escape-from-child.txt\" 2>/dev/null; then printf '%s\\n' UAT8_ESCAPE_WRITE_SUCCEEDED; else printf '%s\\n' UAT8_ESCAPE_WRITE_DENIED; fi; pwd; printf %s dirty > isolated-only.txt"
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uat8_process_host_binds_guards_and_cleans_real_worktrees() -> Result<()> {
    let server = responses::start_mock_server().await;
    let router = WorktreeRouter::default();
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(router.clone())
        .mount(&server)
        .await;

    let server_uri = server.uri();
    let mut builder = test_codex()
        .with_model("gpt-5.5")
        .with_pre_build_hook(move |codex_home| {
            std::fs::write(
                codex_home.join("config.toml"),
                format!("openai_base_url = \"{server_uri}/v1\"\n"),
            )
            .expect("write persisted fixture provider");
            let workflows = codex_home.join("workflows");
            std::fs::create_dir_all(&workflows).expect("create workflow root");
            std::fs::write(workflows.join("uat8.workflow.js"), WORKFLOW_SOURCE)
                .expect("write saved UAT-8 workflow");
        })
        .with_workspace_setup(|cwd, _fs| async move { initialize_git_repository(cwd.as_path()) })
        .with_config(|config| {
            config
                .features
                .enable(Feature::Workflow)
                .expect("enable workflow feature");
            config
                .features
                .enable(Feature::CodeModeHost)
                .expect("enable process-owned workflow host");
            #[cfg(target_os = "linux")]
            config
                .features
                .enable(Feature::UseLegacyLandlock)
                .expect("use unprivileged Landlock filesystem enforcement in constrained hosts");
        });
    builder = builder.with_code_mode_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")
            .context("UAT-8 requires the real process-owned code-mode host")?,
    );
    let test = builder.build(&server).await?;
    let parent_environments = test
        .codex
        .config_snapshot()
        .await
        .environment_selections()
        .to_vec();
    #[cfg(target_os = "linux")]
    {
        let effective = uat_permission_profile()
            .materialize_project_roots_with_workspace_roots(std::slice::from_ref(&test.config.cwd));
        assert!(
            !effective
                .file_system_sandbox_policy()
                .needs_direct_runtime_enforcement(
                    NetworkSandboxPolicy::Enabled,
                    test.config.cwd.as_path(),
                ),
            "UAT-8 Linux profile must remain compatible with the Landlock backend"
        );
        let sandbox_exe = test
            .config
            .codex_linux_sandbox_exe
            .as_deref()
            .context("UAT-8 Linux guard requires the sandbox helper")?;
        assert_linux_sandbox_preflight(sandbox_exe, test.config.cwd.as_path(), &effective).await?;
        assert_linux_worktree_sandbox_preflight(
            sandbox_exe,
            test.config.cwd.as_path(),
            &uat_permission_profile(),
        )
        .await?;
    }
    let mut child_created = test.thread_manager.subscribe_thread_created();
    let child_manager = Arc::clone(&test.thread_manager);
    let child_configs = tokio::spawn(async move {
        let mut configs = HashMap::new();
        while configs.len() < 2 {
            let child_id = tokio::time::timeout(Duration::from_secs(60), child_created.recv())
                .await
                .context("timed out waiting for an isolated child registration")??;
            let child = child_manager
                .get_thread(child_id)
                .await
                .context("isolated child was reaped before its spawn config was captured")?;
            configs.insert(child_id, child.config_snapshot().await);
        }
        Ok::<_, anyhow::Error>(configs)
    });
    let mut receiver = test.codex.subscribe_events();
    test.submit_turn_with_permission_profile(
        "run the UAT-8 saved workflow",
        uat_permission_profile(),
    )
    .await?;
    let (run_id, events) = collect_workflow_events(&mut receiver).await?;
    let child_configs = child_configs
        .await
        .context("join isolated child config collector")??;

    let repo = test.config.cwd.as_path();
    let repo_parent = repo.parent().context("test repository has a parent")?;
    let allocation_root = repo_parent.join(format!(".codex-worktrees-{run_id}"));
    let clean_checkout = allocation_root.join("agent-0");
    let dirty_checkout = allocation_root.join("agent-1");
    let _retained_worktree_cleanup = RetainedWorktreeCleanup {
        repo: repo.to_path_buf(),
        allocation_root: allocation_root.clone(),
        dirty_checkout: dirty_checkout.clone(),
    };

    let tool_outputs = child_function_outputs(&router.requests.lock().unwrap());
    assert_output_mentions_path(&tool_outputs[CLEAN_MARKER], &clean_checkout);
    assert_output_mentions_path(&tool_outputs[DIRTY_MARKER], &dirty_checkout);
    assert!(
        normalized(&tool_outputs[DIRTY_MARKER])
            .contains(&format!("uat8_main={}", normalized_path(repo))),
        "dirty child must resolve the first registered worktree to the exact main checkout {}: {}",
        repo.display(),
        tool_outputs[DIRTY_MARKER]
    );
    assert!(
        tool_outputs[DIRTY_MARKER].contains(ESCAPE_DENIED),
        "dirty child must visibly report that the managed filesystem guard denied its main-checkout write: {}",
        tool_outputs[DIRTY_MARKER]
    );
    assert!(!tool_outputs[DIRTY_MARKER].contains(ESCAPE_SUCCEEDED));
    assert!(
        !repo.join(DIRTY_FILE).exists(),
        "isolated child mutation must not escape into the parent checkout"
    );
    assert!(
        !repo.join(ESCAPE_FILE).exists(),
        "the main-checkout escape marker must be blocked by the child filesystem guard"
    );
    assert!(
        !clean_checkout.exists(),
        "unchanged child checkout must be removed after its process and tools finish"
    );
    assert!(
        dirty_checkout.join(DIRTY_FILE).is_file(),
        "changed child checkout must be retained for recovery; dirty tool response: {}",
        tool_outputs[DIRTY_MARKER]
    );
    assert_eq!(
        std::fs::read_to_string(dirty_checkout.join(DIRTY_FILE))?,
        "dirty"
    );

    let journal_path = test
        .codex_home_path()
        .join("workflows/runs")
        .join(&run_id)
        .join("journal.jsonl");
    let journal = std::fs::read_to_string(&journal_path)?;
    let journal_children = journal_children(&journal)?;
    assert_eq!(
        journal_children.len(),
        2,
        "both agents have durable bindings"
    );
    let event_children = bound_child_ids(&events)?;
    assert_eq!(
        event_children,
        journal_children.values().map(ToString::to_string).collect(),
        "live bindings and durable bindings must identify the same children"
    );
    for (ordinal, child_id) in journal_children {
        let expected_cwd = if ordinal == 0 {
            &clean_checkout
        } else {
            &dirty_checkout
        };
        let config = child_configs
            .get(&child_id)
            .context("durably bound child has no captured spawn config")?;
        assert_eq!(config.cwd().as_path(), expected_cwd);
        assert_eq!(
            config.workspace_roots,
            vec![config.cwd().clone()],
            "child runtime roots must move with its isolated cwd"
        );
        let expected_environments = parent_environments
            .iter()
            .cloned()
            .map(|mut environment| {
                environment.cwd = PathUri::from_host_native_path(expected_cwd)
                    .expect("isolated checkout has a valid native path");
                environment
            })
            .collect::<Vec<_>>();
        assert_eq!(
            config.environment_selections(),
            expected_environments,
            "the full local executor selection must preserve identity and rebind cwd"
        );
        assert_eq!(
            config.permission_profile,
            uat_permission_profile()
                .with_explicit_readable_root(AbsolutePathBuf::from_absolute_path_checked(
                    repo.join(".git/worktrees").join(format!("agent-{ordinal}")),
                )?)
                .materialize_project_roots_with_workspace_roots(std::slice::from_ref(config.cwd())),
            "the child sandbox must be guarded by only its isolated worktree root"
        );
    }

    let worktree_list = git_stdout(repo, &["worktree", "list", "--porcelain"])?;
    assert!(!normalized(&worktree_list).contains(&normalized_path(&clean_checkout)));
    assert!(normalized(&worktree_list).contains(&normalized_path(&dirty_checkout)));
    assert_eq!(
        git_stdout(repo, &["status", "--porcelain"])?,
        "",
        "parent repository must remain clean"
    );
    assert!(
        journal.contains(
            "workflow agent worktree was retained because it contains uncommitted changes"
        )
    );
    assert!(
        !normalized(&journal).contains(&normalized_path(&dirty_checkout)),
        "CLI-readable journal narration must not expose the retained checkout path"
    );
    let public_events = serde_json::to_string(&events)?;
    assert!(
        !public_events.contains(
            "workflow agent worktree was retained because it contains uncommitted changes"
        ),
        "host cleanup diagnostics must remain outside public workflow progress"
    );
    assert!(
        !normalized(&public_events).contains(&normalized_path(&dirty_checkout)),
        "the retained checkout path must not enter public workflow progress"
    );
    assert!(
        router
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| !request
                .to_string()
                .contains("retained dirty workflow agent worktree")),
        "cleanup diagnostics must not enter a subsequent model request"
    );

    test.codex.shutdown_and_wait().await?;
    Ok(())
}

/// Remote executors cannot consume a Git checkout created on the workflow host. Drive the saved
/// workflow and real process host to prove that this boundary rejects before allocation or spawn,
/// while retaining one null-linked error record as durable audit evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uat8_remote_executor_rejects_worktree_before_allocation_or_child_spawn() -> Result<()> {
    skip_if_no_remote_env!(Ok(()));

    let server = responses::start_mock_server().await;
    let router = WorktreeRouter::default();
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(router.clone())
        .mount(&server)
        .await;

    let mut builder = test_codex()
        .with_model("gpt-5.5")
        .with_pre_build_hook(|codex_home| {
            let workflows = codex_home.join("workflows");
            std::fs::create_dir_all(&workflows).expect("create remote UAT-8 workflow root");
            std::fs::write(
                workflows.join("uat8.workflow.js"),
                REMOTE_FAIL_CLOSED_SOURCE,
            )
            .expect("write saved remote UAT-8 workflow");
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::Workflow)
                .expect("enable workflow feature");
            config
                .features
                .enable(Feature::CodeModeHost)
                .expect("enable process-owned workflow host");
        });
    builder = builder.with_code_mode_host_program(
        codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")
            .context("remote UAT-8 requires the real process-owned code-mode host")?,
    );
    let test = builder.build_with_auto_env(&server).await?;

    let selections = test
        .codex
        .config_snapshot()
        .await
        .environment_selections()
        .to_vec();
    assert_eq!(
        selections.len(),
        1,
        "the remote auto-env lane must select exactly one executor"
    );
    assert_eq!(
        selections[0].environment_id,
        codex_exec_server::REMOTE_ENVIRONMENT_ID,
        "the fail-closed UAT must exercise a genuinely remote executor"
    );

    let thread_ids_before = test.thread_manager.list_thread_ids().await;
    let mut child_created = test.thread_manager.subscribe_thread_created();
    let mut workflow_events = test.codex.subscribe_events();
    test.submit_turn("run the saved remote worktree rejection workflow")
        .await?;

    let collected = tokio::select! {
        biased;
        child = child_created.recv() => {
            anyhow::bail!("remote worktree rejection unexpectedly registered child {child:?}");
        }
        result = collect_workflow_events(&mut workflow_events) => result,
    }?;
    let (run_id, events) = collected;
    assert!(
        matches!(
            child_created.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "no child registration may be queued after the workflow reaches terminal state"
    );
    assert_eq!(
        test.thread_manager.list_thread_ids().await,
        thread_ids_before,
        "a rejected worktree request must not add a child thread"
    );
    assert!(
        events.iter().all(|event| !matches!(
            event,
            WorkflowEvent::AgentBegin(_)
                | WorkflowEvent::AgentBound(_)
                | WorkflowEvent::AgentUpdated(_)
                | WorkflowEvent::AgentEnd(_)
        )),
        "a pre-spawn rejection must not publish child lifecycle events: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            WorkflowEvent::Log(event)
                if event.message.starts_with("UAT8_REMOTE_REJECTION:")
                    && event.message.contains(REMOTE_REJECTION)
        )),
        "the running workflow must observe the exact remote fail-closed rejection: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            WorkflowEvent::RunEnd(event)
                if event.status == AgentStatus::Completed(None)
        )),
        "the fixture must catch the rejection and finish normally: {events:?}"
    );

    {
        let requests = router.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            2,
            "only the parent workflow admission round trip may reach the model"
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.to_string().contains("UAT8_REMOTE_WORKTREE")),
            "the rejected child prompt must never appear in a model request"
        );
    }

    let journal_path = test
        .codex_home_path()
        .join("workflows/runs")
        .join(&run_id)
        .join("journal.jsonl");
    let journal_lines = std::fs::read_to_string(&journal_path)?
        .lines()
        .filter(|line| !line.is_empty())
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let agent_calls = journal_lines
        .iter()
        .filter(|line| line["type"] == "agent_call")
        .collect::<Vec<_>>();
    assert_eq!(agent_calls.len(), 1, "one rejection audit record");
    let key = agent_calls[0]["key"]
        .as_str()
        .context("rejection audit record has a string cache key")?;
    let prompt_hash = agent_calls[0]["prompt_hash"]
        .as_str()
        .context("rejection audit record has a string prompt hash")?;
    let timestamp = agent_calls[0]["timestamp"]
        .as_str()
        .context("rejection audit record has a string timestamp")?;
    assert!(key.starts_with("blake3:"));
    assert!(prompt_hash.starts_with("blake3:"));
    assert!(timestamp.ends_with('Z'));
    assert_eq!(
        agent_calls[0],
        &json!({
            "type": "agent_call",
            "timestamp": timestamp,
            "ordinal": 0,
            "key": key,
            "prompt_hash": prompt_hash,
            "opts": {
                "model": null,
                "effort": null,
                "agentType": null,
                "isolation": "worktree",
                "schema_hash": null,
            },
            "phase": "reject",
            "label": "remote rejection",
            "child_thread_id": null,
            "rollout_path": null,
            "status": "error",
            "return": null,
            "tokens_spent": null,
            "completion_seq": null,
        }),
        "the durable error must carry no child or rollout linkage"
    );
    assert!(
        journal_lines
            .iter()
            .all(|line| line["type"] != "agent_bound"),
        "no child binding may be persisted"
    );

    let selected_cwd = &selections[0].cwd;
    let allocation_root = selected_cwd
        .parent()
        .context("remote test cwd has a parent")?
        .join(&format!(".codex-worktrees-{run_id}"))?;
    let git_worktrees = selected_cwd.join(".git")?.join("worktrees")?;
    for (description, path) in [
        ("workflow allocation root", allocation_root),
        ("Git worktree registration directory", git_worktrees),
    ] {
        let error = test
            .fs()
            .get_metadata(&path, /*sandbox*/ None)
            .await
            .expect_err("remote rejection must leave the executor filesystem untouched");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "unexpected {description} state at {path}"
        );
    }

    let host_allocation_root = test
        .config
        .cwd
        .parent()
        .context("projected remote cwd has a host-side parent")?
        .join(format!(".codex-worktrees-{run_id}"));
    assert!(
        !host_allocation_root.exists(),
        "the workflow host must not allocate a checkout for a remote executor at {}",
        host_allocation_root.display()
    );

    test.codex.shutdown_and_wait().await?;
    Ok(())
}

fn uat_permission_profile() -> PermissionProfile {
    // The tracked fixture contains every metadata directory protected by workspace-write, keeping
    // this profile exactly representable by the Linux Landlock backend selected above. macOS and
    // Windows retain their native managed sandbox. Network stays enabled because this lane tests
    // filesystem guards, not network policy.
    PermissionProfile::workspace_write_with(
        &[],
        NetworkSandboxPolicy::Enabled,
        /*exclude_tmpdir_env_var*/ true,
        /*exclude_slash_tmp*/ true,
    )
}

#[cfg(target_os = "linux")]
async fn assert_linux_sandbox_preflight(
    sandbox_exe: &std::path::Path,
    cwd: &std::path::Path,
    permission_profile: &PermissionProfile,
) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let mut command = tokio::process::Command::new(sandbox_exe);
    command.as_std_mut().arg0("codex-linux-sandbox");
    let output = command
        .arg("--sandbox-policy-cwd")
        .arg(cwd)
        .arg("--command-cwd")
        .arg(cwd)
        .arg("--permission-profile")
        .arg(serde_json::to_string(permission_profile)?)
        .arg("--use-legacy-landlock")
        .arg("--")
        .args(["sh", "-c", "pwd"])
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "UAT-8 Linux Landlock preflight failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[cfg(target_os = "linux")]
async fn assert_linux_worktree_sandbox_preflight(
    sandbox_exe: &std::path::Path,
    repo: &std::path::Path,
    permission_profile: &PermissionProfile,
) -> Result<()> {
    let checkout = repo
        .parent()
        .context("UAT-8 repository has a parent")?
        .join(".uat8-landlock-worktree-preflight");
    let output = Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .current_dir(repo)
        .args(["worktree", "add", "--detach", "--"])
        .arg(&checkout)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "failed to create UAT-8 Landlock probe checkout: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _cleanup = PreflightWorktreeCleanup {
        repo: repo.to_path_buf(),
        checkout: checkout.clone(),
    };
    let checkout =
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path_checked(checkout)?;
    let git_dir = AbsolutePathBuf::from_absolute_path_checked(PathBuf::from(git_stdout(
        checkout.as_path(),
        &["rev-parse", "--absolute-git-dir"],
    )?))?;
    let effective = permission_profile
        .clone()
        .with_explicit_readable_root(git_dir)
        .materialize_project_roots_with_workspace_roots(std::slice::from_ref(&checkout));
    anyhow::ensure!(
        !effective
            .file_system_sandbox_policy()
            .needs_direct_runtime_enforcement(NetworkSandboxPolicy::Enabled, checkout.as_path()),
        "UAT-8 detached-worktree profile requires direct runtime enforcement: {}",
        serde_json::to_string(&effective)?
    );
    assert_linux_sandbox_preflight(sandbox_exe, checkout.as_path(), &effective).await
}

#[cfg(target_os = "linux")]
struct PreflightWorktreeCleanup {
    repo: PathBuf,
    checkout: PathBuf,
}

#[cfg(target_os = "linux")]
impl Drop for PreflightWorktreeCleanup {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .current_dir(&self.repo)
            .args(["worktree", "remove", "--force", "--"])
            .arg(&self.checkout)
            .output();
    }
}

fn initialize_git_repository(repo: &std::path::Path) -> Result<()> {
    git(repo, &["init", "--quiet"])?;
    std::fs::write(repo.join("tracked.txt"), "parent baseline\n")?;
    std::fs::create_dir_all(repo.join(".agents"))?;
    std::fs::write(repo.join(".agents/uat8-fixture.txt"), "protected\n")?;
    std::fs::create_dir_all(repo.join(".codex"))?;
    std::fs::write(repo.join(".codex/uat8-fixture.txt"), "protected\n")?;
    git(repo, &["add", "--", "."])?;
    git(
        repo,
        &[
            "-c",
            "user.name=Codex UAT",
            "-c",
            "user.email=codex-uat@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "-m",
            "UAT baseline",
        ],
    )?;
    Ok(())
}

struct RetainedWorktreeCleanup {
    repo: PathBuf,
    allocation_root: PathBuf,
    dirty_checkout: PathBuf,
}

impl Drop for RetainedWorktreeCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.dirty_checkout.join(DIRTY_FILE));
        let _ = Command::new("git")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .current_dir(&self.repo)
            .args(["worktree", "remove", "--"])
            .arg(&self.dirty_checkout)
            .output();
        let _ = std::fs::remove_dir(&self.allocation_root);
    }
}

fn git(repo: &std::path::Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .current_dir(repo)
        .args(args)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn git_stdout(repo: &std::path::Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .current_dir(repo)
        .args(args)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

#[cfg(windows)]
fn null_device() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
fn null_device() -> &'static str {
    "/dev/null"
}

async fn collect_workflow_events(
    receiver: &mut tokio::sync::broadcast::Receiver<codex_protocol::protocol::Event>,
) -> Result<(String, Vec<WorkflowEvent>)> {
    let mut run_id = None;
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(60), receiver.recv())
            .await
            .context("timed out waiting for UAT-8 workflow completion")??;
        let EventMsg::Workflow(workflow_event) = event.msg else {
            continue;
        };
        let event_run_id = workflow_event_run_id(&workflow_event);
        match &run_id {
            Some(expected) if expected != event_run_id => continue,
            None => run_id = Some(event_run_id.to_string()),
            Some(_) => {}
        }
        let terminal = matches!(workflow_event, WorkflowEvent::RunEnd(_));
        events.push(workflow_event);
        if terminal {
            return Ok((run_id.expect("run id observed"), events));
        }
    }
}

fn workflow_event_run_id(event: &WorkflowEvent) -> &str {
    match event {
        WorkflowEvent::RunBegin(event) => &event.run_id,
        WorkflowEvent::RunEnd(event) => &event.run_id,
        WorkflowEvent::PhaseBegin(event) => &event.run_id,
        WorkflowEvent::PhaseEnd(event) => &event.run_id,
        WorkflowEvent::GroupBegin(event) => &event.run_id,
        WorkflowEvent::GroupEnd(event) => &event.run_id,
        WorkflowEvent::AgentBegin(event) => &event.run_id,
        WorkflowEvent::AgentBound(event) => &event.run_id,
        WorkflowEvent::AgentUpdated(event) => &event.run_id,
        WorkflowEvent::AgentEnd(event) => &event.run_id,
        WorkflowEvent::Log(event) => &event.run_id,
    }
}

fn bound_child_ids(events: &[WorkflowEvent]) -> Result<BTreeSet<String>> {
    events
        .iter()
        .filter_map(|event| match event {
            WorkflowEvent::AgentBound(event) => Some(
                ThreadId::from_string(&event.child_thread_id)
                    .map(|thread_id| thread_id.to_string()),
            ),
            _ => None,
        })
        .collect::<std::result::Result<_, _>>()
        .map_err(Into::into)
}

fn journal_children(journal: &str) -> Result<BTreeMap<u64, ThreadId>> {
    journal
        .lines()
        .filter(|line| !line.is_empty())
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|line| line["type"] == "agent_call")
        .map(|line| {
            let ordinal = line["ordinal"].as_u64().context("agent ordinal")?;
            let child_id = line["child_thread_id"]
                .as_str()
                .context("agent child thread id")?;
            Ok((ordinal, ThreadId::from_string(child_id)?))
        })
        .collect()
}

fn child_function_outputs(requests: &[Value]) -> BTreeMap<&'static str, String> {
    [CLEAN_MARKER, DIRTY_MARKER]
        .into_iter()
        .map(|marker| {
            let body = requests
                .iter()
                .find(|body| child_marker(body) == Some(marker) && contains_function_output(body))
                .unwrap_or_else(|| panic!("missing tool follow-up for {marker}"));
            let output = body["input"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|item| {
                    item.get("type").and_then(Value::as_str) == Some("function_call_output")
                })
                .expect("captured request carries function output");
            (
                marker,
                output["output"]
                    .as_str()
                    .expect("function output is text")
                    .to_string(),
            )
        })
        .collect()
}

fn assert_output_mentions_path(output: &str, path: &std::path::Path) {
    assert!(
        normalized(output).contains(&normalized_path(path)),
        "tool output must report isolated cwd {}: {output}",
        path.display()
    );
}

fn normalized_path(path: &std::path::Path) -> String {
    normalized(&path.to_string_lossy())
}

fn normalized(value: &str) -> String {
    value.replace('\\', "/").to_ascii_lowercase()
}

fn request_body_json(request: &wiremock::Request) -> Value {
    let is_zstd = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|entry| entry.trim() == "zstd"));
    let bytes = if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(request.body.as_slice()))
            .expect("decode zstd request")
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&bytes).expect("request body is JSON")
}

fn child_marker(body: &Value) -> Option<&'static str> {
    let input = body["input"].as_array()?;
    [CLEAN_MARKER, DIRTY_MARKER]
        .into_iter()
        .find(|&marker| input.iter().any(|item| item.to_string().contains(marker)))
        .map(|v| v as _)
}

fn contains_function_output(body: &Value) -> bool {
    body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
}

fn contains_workflow_output(body: &Value) -> bool {
    body["input"].as_array().into_iter().flatten().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("function_call_output")
            && item.get("call_id").and_then(Value::as_str) == Some(WORKFLOW_CALL_ID)
    })
}
