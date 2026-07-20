#![allow(clippy::expect_used)]

use std::sync::Arc;

use codex_features::Feature;
use codex_workflow_journal::JournalLine;
use codex_workflow_journal::LogLine;
use codex_workflow_journal::NullOrdinal;
use codex_workflow_journal::storage::WorkflowRunPaths;
use serde_json::json;

use super::MAX_NARRATION_LINES;
use super::MAX_NARRATION_OUTPUT_BYTES;
use super::NARRATION_TRUNCATION_MARKER;
use super::read_narration;
use super::run_workflow_cli;

#[tokio::test]
async fn narration_reader_caps_returned_lines_and_bytes() {
    let codex_home = tempfile::tempdir().expect("codex home");
    let paths = WorkflowRunPaths::new(codex_home.path(), "run-1");
    paths.create_dir().expect("run directory");
    let line = serde_json::to_string(&JournalLine::Log(LogLine {
        timestamp: None,
        ordinal: NullOrdinal,
        message: "x".repeat(4 * 1024),
    }))
    .expect("serialize log line");
    let journal = std::iter::repeat_n(line, MAX_NARRATION_LINES + 1)
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(paths.journal(), journal).expect("write journal");

    let narration = read_narration(codex_home.path(), "run-1").await;

    assert!(narration.len() <= MAX_NARRATION_LINES);
    assert!(
        narration.iter().map(|line| line.len() + 1).sum::<usize>() <= MAX_NARRATION_OUTPUT_BYTES
    );
    assert!(
        narration
            .last()
            .is_some_and(|line| line.ends_with(NARRATION_TRUNCATION_MARKER))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_owned_run_and_resume_use_their_current_root_threads() {
    let codex_home = tempfile::tempdir().expect("codex home");
    let mut config = crate::config::test_config().await;
    config.codex_home = codex_home
        .path()
        .to_path_buf()
        .try_into()
        .expect("codex home is absolute");
    config.sqlite_home = codex_home.path().to_path_buf();
    config.cwd = config.codex_home.clone();
    config.codex_self_exe = Some(std::env::current_exe().expect("current test executable"));
    config
        .features
        .enable(Feature::Workflow)
        .expect("enable workflow feature");
    config
        .features
        .disable(Feature::CodeModeHost)
        .expect("use the in-process workflow host");
    let source = "export const meta = { name: 'owned', description: 'owner test' };\n\
                  text('done');\n";
    let user_instructions_provider = Arc::new(crate::test_support::EmptyUserInstructionsProvider);

    let source_output = run_workflow_cli(
        &config,
        source,
        json!(null),
        None,
        user_instructions_provider.clone(),
    )
    .await
    .expect("run source workflow");
    let source_meta = WorkflowRunPaths::new(codex_home.path(), &source_output.run_id)
        .read_meta_bounded()
        .expect("read source run metadata");
    let source_owner = source_meta
        .owner_thread_id
        .clone()
        .expect("source run records its root thread owner");

    let resumed_output = run_workflow_cli(
        &config,
        source,
        json!(null),
        Some(source_output.run_id.clone()),
        user_instructions_provider,
    )
    .await
    .expect("resume source workflow");
    let resumed_meta = WorkflowRunPaths::new(codex_home.path(), &resumed_output.run_id)
        .read_meta_bounded()
        .expect("read resumed run metadata");

    assert_eq!(resumed_meta.parent_run_id, None);
    assert_eq!(
        resumed_meta.resumed_from_run_id,
        Some(source_output.run_id.clone())
    );
    assert!(resumed_meta.owner_thread_id.is_some());
    assert_ne!(
        resumed_meta.owner_thread_id.as_deref(),
        Some(source_owner.as_str()),
        "a resumed CLI invocation is owned by its fresh session, not the source run's session"
    );
}
