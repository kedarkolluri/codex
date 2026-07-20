//! Model-callable `workflow_run.resumeFromRunId` integration coverage.
//!
//! This drives the public tool through real parent turns, real workflow isolates, and real child
//! threads. It intentionally does not call the workflow handler or replay helpers directly.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_core::CodexThread;
use codex_core::NewThread;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::user_input::UserInput;
use codex_workflow_journal::ReplayJournal;
use codex_workflow_journal::WorkflowRunStatus;
use codex_workflow_journal::storage::WorkflowRunPaths;
use codex_workflow_journal::storage::runs_root;
use core_test_support::responses;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

use self::router::ModelRouter;
use self::router::RAW_CHILD_COMPLETION;
use self::router::mount_router;

mod router;

const WORKFLOW_NAME: &str = "model-resume";
const WORKFLOW_FILE: &str = "model-resume.workflow.js";
const RESUME_UNAVAILABLE: &str = "workflow checkpoint is unavailable or not resumable";

const SOURCE_V1: &str = r#"export const meta = { name: 'model-resume', description: 'model resume integration' };
const answer = await agent('MODEL_RESUME_CHILD_V1:' + args.tag);
text(answer);
"#;

const SOURCE_V2: &str = r#"export const meta = { name: 'model-resume', description: 'model resume integration' };
const answer = await agent('MODEL_RESUME_CHILD_V2:' + args.tag);
text(answer);
"#;

#[derive(Clone, Copy, Debug)]
enum HostMode {
    InProcess,
    ProcessOwned,
}

struct RunObservation {
    run_id: String,
    resumed_from_run_id: Option<String>,
}

fn workflow_builder(host_mode: HostMode) -> TestCodexBuilder {
    let builder = test_codex()
        .with_model("gpt-5.5")
        .with_pre_build_hook(|home| {
            let workflows = home.join("workflows");
            std::fs::create_dir_all(&workflows).expect("create workflow registry");
            std::fs::write(workflows.join(WORKFLOW_FILE), SOURCE_V1).expect("write saved workflow");
        })
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Workflow)
                .expect("enable workflow feature");
            match host_mode {
                HostMode::InProcess => config
                    .features
                    .disable(Feature::CodeModeHost)
                    .expect("disable process-owned host"),
                HostMode::ProcessOwned => config
                    .features
                    .enable(Feature::CodeModeHost)
                    .expect("enable process-owned host"),
            }
        });
    match host_mode {
        HostMode::InProcess => builder,
        HostMode::ProcessOwned => builder.with_code_mode_host_program(
            codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")
                .expect("resolve process-owned host"),
        ),
    }
}

async fn invoke_owner(
    test: &TestCodex,
    router: &ModelRouter,
    args: Value,
    resume_from_run_id: Option<&str>,
    prompt: &str,
) -> Result<(RunObservation, String)> {
    let output_index = router.enqueue(args, resume_from_run_id);
    let mut receiver = test.codex.subscribe_events();
    test.submit_turn(prompt).await?;
    let mut run = None;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), receiver.recv())
            .await
            .context("timed out waiting for workflow terminal event")??;
        let EventMsg::Workflow(workflow_event) = event.msg else {
            continue;
        };
        match workflow_event {
            WorkflowEvent::RunBegin(event) if run.is_none() => {
                run = Some(RunObservation {
                    run_id: event.run_id,
                    resumed_from_run_id: event.resumed_from_run_id,
                });
            }
            WorkflowEvent::RunEnd(event)
                if run
                    .as_ref()
                    .is_some_and(|run: &RunObservation| run.run_id == event.run_id) =>
            {
                return Ok((
                    run.expect("workflow emitted RunBegin"),
                    router.output(output_index),
                ));
            }
            _ => {}
        }
    }
}

async fn invoke_other_thread(
    thread: &CodexThread,
    router: &ModelRouter,
    source_run_id: &str,
) -> Result<(bool, String)> {
    let output_index = router.enqueue(json!({ "tag": "a" }), Some(source_run_id));
    let mut receiver = thread.subscribe_events();
    thread
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "try to resume the other thread's workflow".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    let mut saw_workflow_event = false;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), receiver.recv())
            .await
            .context("timed out waiting for foreign-thread turn")??;
        saw_workflow_event |= matches!(event.msg, EventMsg::Workflow(_));
        let terminal = matches!(event.msg, EventMsg::TurnComplete(_));
        if terminal {
            return Ok((saw_workflow_event, router.output(output_index)));
        }
    }
}

fn run_ids(home: &std::path::Path) -> HashSet<String> {
    std::fs::read_dir(runs_root(home))
        .expect("read workflow runs root")
        .map(|entry| {
            entry
                .expect("read workflow run directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn journal(home: &std::path::Path, run_id: &str) -> ReplayJournal {
    ReplayJournal::load(&WorkflowRunPaths::new(home, run_id).journal())
        .expect("load workflow replay journal")
}

fn assert_success_output(output: &str, run_id: &str) {
    assert!(output.len() < 256, "workflow tool output must stay bounded");
    assert_eq!(
        serde_json::from_str::<Value>(output).expect("workflow tool output is JSON"),
        json!({ "runId": run_id, "status": "running" })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_callable_resume_enforces_replay_contract_across_hosts() -> Result<()> {
    for host_mode in [HostMode::InProcess, HostMode::ProcessOwned] {
        let server = responses::start_mock_server().await;
        let router = mount_router(&server).await;
        let test = workflow_builder(host_mode)
            .build_with_auto_env(&server)
            .await?;
        let home = test.codex_home_path();

        let (source_run, source_output) = invoke_owner(
            &test,
            &router,
            json!({ "tag": "a" }),
            None,
            "start the model resume fixture",
        )
        .await?;
        let source_run_id = source_run.run_id;
        assert_eq!(source_run.resumed_from_run_id, None);
        assert_success_output(&source_output, &source_run_id);
        let source_paths = WorkflowRunPaths::new(home, &source_run_id);
        let source_meta = source_paths.read_meta_bounded()?;
        assert_eq!(source_meta.status, WorkflowRunStatus::Completed);
        assert_eq!(
            source_meta.owner_thread_id.as_deref(),
            Some(test.session_configured.thread_id.to_string().as_str())
        );
        let source_journal = journal(home, &source_run_id);
        assert_eq!(source_journal.entries().len(), 1);
        assert_eq!(router.child_prompts(), ["MODEL_RESUME_CHILD_V1:a"]);

        let NewThread {
            thread: other_thread,
            session_configured: other_session,
            ..
        } = test
            .thread_manager
            .start_thread(test.config.clone())
            .await?;
        assert_ne!(other_session.thread_id, test.session_configured.thread_id);
        let runs_before_denial = run_ids(home);
        let children_before_denial = router.child_prompts();
        let (saw_foreign_workflow_event, foreign_output) =
            invoke_other_thread(&other_thread, &router, &source_run_id).await?;
        assert!(!saw_foreign_workflow_event);
        assert_eq!(run_ids(home), runs_before_denial);
        assert_eq!(router.child_prompts(), children_before_denial);
        assert_eq!(foreign_output, RESUME_UNAVAILABLE);
        assert!(foreign_output.len() < 256);
        assert!(!foreign_output.contains(&source_run_id));
        other_thread.shutdown_and_wait().await?;

        let (same_run, same_output) = invoke_owner(
            &test,
            &router,
            json!({ "tag": "a" }),
            Some(&source_run_id),
            "resume the unchanged fixture",
        )
        .await?;
        let same_run_id = same_run.run_id;
        assert_ne!(same_run_id, source_run_id);
        assert_eq!(
            same_run.resumed_from_run_id.as_deref(),
            Some(source_run_id.as_str())
        );
        assert_success_output(&same_output, &same_run_id);
        assert_eq!(router.child_prompts(), ["MODEL_RESUME_CHILD_V1:a"]);
        let same_paths = WorkflowRunPaths::new(home, &same_run_id);
        let same_meta = same_paths.read_meta_bounded()?;
        assert_eq!(same_meta.status, WorkflowRunStatus::Completed);
        assert_eq!(
            same_meta.resumed_from_run_id.as_deref(),
            Some(source_run_id.as_str())
        );
        assert_eq!(
            journal(home, &same_run_id).entries(),
            source_journal.entries()
        );

        let (args_run, args_output) = invoke_owner(
            &test,
            &router,
            json!({ "tag": "b" }),
            Some(&source_run_id),
            "resume with changed arguments",
        )
        .await?;
        let args_run_id = args_run.run_id;
        assert_ne!(args_run_id, source_run_id);
        assert_ne!(args_run_id, same_run_id);
        assert_eq!(
            args_run.resumed_from_run_id.as_deref(),
            Some(source_run_id.as_str())
        );
        assert_success_output(&args_output, &args_run_id);
        let args_meta = WorkflowRunPaths::new(home, &args_run_id).read_meta_bounded()?;
        assert_eq!(args_meta.script_hash, source_meta.script_hash);
        assert_ne!(args_meta.args_hash, source_meta.args_hash);

        std::fs::write(home.join("workflows").join(WORKFLOW_FILE), SOURCE_V2)?;
        let (changed_run, changed_output) = invoke_owner(
            &test,
            &router,
            json!({ "tag": "a" }),
            Some(&source_run_id),
            "resume with changed source",
        )
        .await?;
        let changed_run_id = changed_run.run_id;
        assert_ne!(changed_run_id, source_run_id);
        assert_ne!(changed_run_id, same_run_id);
        assert_ne!(changed_run_id, args_run_id);
        assert_eq!(
            changed_run.resumed_from_run_id.as_deref(),
            Some(source_run_id.as_str())
        );
        assert_success_output(&changed_output, &changed_run_id);
        let changed_meta = WorkflowRunPaths::new(home, &changed_run_id).read_meta_bounded()?;
        assert_ne!(changed_meta.script_hash, source_meta.script_hash);
        assert_eq!(changed_meta.args_hash, source_meta.args_hash);

        assert_eq!(
            router.child_prompts(),
            [
                "MODEL_RESUME_CHILD_V1:a",
                "MODEL_RESUME_CHILD_V1:b",
                "MODEL_RESUME_CHILD_V2:a",
            ],
            "only the source and the two structurally divergent resumes dispatch live children"
        );
        let live_child_ids = [
            source_run_id.as_str(),
            args_run_id.as_str(),
            changed_run_id.as_str(),
        ]
        .map(|run_id| {
            journal(home, run_id).entries()[0]
                .child_thread_id
                .clone()
                .expect("live journal retains child binding")
        });
        assert_eq!(live_child_ids.iter().collect::<HashSet<_>>().len(), 3);
        assert!(
            router
                .parent_requests()
                .iter()
                .all(|body| !body.to_string().contains(RAW_CHILD_COMPLETION)),
            "raw child completion leaked into parent model context for {host_mode:?}"
        );
        assert!(
            router
                .tool_outputs()
                .iter()
                .all(|output| output.len() < 256),
            "model-visible workflow outputs must remain bounded for {host_mode:?}"
        );

        test.codex.shutdown_and_wait().await?;
    }
    Ok(())
}
