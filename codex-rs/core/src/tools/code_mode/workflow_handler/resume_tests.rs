use std::path::Path;
use std::sync::Arc;

use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeSessionProviderFuture;
use codex_code_mode::InProcessCodeModeSession;
use codex_features::Feature;
use codex_features::Features;
use codex_protocol::protocol::AgentStatus as ProtocolAgentStatus;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use codex_protocol::protocol::WorkflowRunEndEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;
use codex_workflow_journal::AgentCallLine;
use codex_workflow_journal::AgentCallOpts as JournalAgentCallOpts;
use codex_workflow_journal::AgentStatus;
use codex_workflow_journal::Divergence;
use codex_workflow_journal::JournalLine;
use codex_workflow_journal::JournalRecorder;
use codex_workflow_journal::KEY_ALGO_VERSION;
use codex_workflow_journal::WorkflowRunLease;
use codex_workflow_journal::WorkflowRunLeaseAcquire;
use codex_workflow_journal::WorkflowRunMeta;
use codex_workflow_journal::WorkflowRunStatus;
use codex_workflow_journal::canonical_value_hash;
use codex_workflow_journal::prompt_hash as content_hash;
use codex_workflow_journal::storage::WorkflowRunPaths;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::RESUME_UNAVAILABLE_MESSAGE;
use super::LEGACY_UNKNOWN_RESUME_MESSAGE;
use super::WorkflowReplayAccess;
use super::WorkflowResumeInvocation;
use super::load_resume_seed;
use super::load_resume_seed_off_thread;
use super::prepare_paused_workflow_resume_for_owner;
use super::prepare_workflow_prefix_replay;
use super::run_workflow_source_to_terminal;
use crate::function_tool::FunctionCallError;
use crate::tools::code_mode::CodeModeService;
use crate::tools::code_mode::workflow_progress::WorkflowEventTarget;
use crate::tools::code_mode::workflow_progress::durable::DurableRunStatus;
use crate::tools::code_mode::workflow_progress::durable::record_event;
use crate::tools::code_mode::workflow_progress::durable::record_terminal_event;

const OWNER_THREAD_ID: &str = "01900000-0000-7000-8000-000000000001";
const SOURCE_RUN_ID: &str = "01900000-0000-7000-8000-000000000002";
const LEGACY_JOURNAL_RECORD_MAX_BYTES: usize = 128 * 1024;

fn completed_agent_call(ordinal: u64) -> AgentCallLine {
    AgentCallLine {
        timestamp: None,
        ordinal,
        attempt: 0,
        key: format!("blake3:k{ordinal}"),
        prompt_hash: "ph".to_string(),
        opts: JournalAgentCallOpts {
            model: Some("gpt".to_string()),
            effort: Some("high".to_string()),
            agent_type: Some("reviewer".to_string()),
            isolation: None,
            schema_hash: None,
        },
        phase: Some("analyze".to_string()),
        label: Some(format!("file-{ordinal}")),
        child_thread_id: Some(format!("th_{ordinal}")),
        rollout_path: Some(format!("/home/u/.codex/sessions/rollout-{ordinal}.jsonl")),
        status: Some(AgentStatus::Completed),
        control_reason: None,
        ret: json!({ "ok": true, "n": ordinal }),
        tokens_spent: Some(1000 + ordinal),
        progress: None,
        completion_seq: None,
    }
}

async fn write_source_run(
    home: &Path,
    run_id: &str,
    script: &str,
    args: &serde_json::Value,
    n_completed: u64,
    execution_fingerprint: Option<&str>,
) -> WorkflowRunPaths {
    let exec = codex_code_mode::parse_exec_source(script).expect("parse source");
    let script_hash = content_hash(&exec.code);
    let args_hash = canonical_value_hash(args);
    let mut meta = WorkflowRunMeta::new(
        run_id.to_string(),
        None,
        script_hash,
        args_hash,
        "triage".to_string(),
        Some(0),
        KEY_ALGO_VERSION,
        "2026-07-17T00:00:00Z".to_string(),
    );
    if let Some(execution_fingerprint) = execution_fingerprint {
        meta = meta.with_execution_fingerprint(execution_fingerprint.to_string());
    }
    meta = meta.with_owner_thread_id(OWNER_THREAD_ID.to_string());
    let paths = WorkflowRunPaths::new(home, run_id);
    paths
        .initialize(&exec.code, &meta)
        .expect("initialize source run");
    paths
        .write_invocation_args(args, &meta.args_hash)
        .expect("persist private invocation arguments");
    let WorkflowRunLeaseAcquire::Acquired(lease) =
        WorkflowRunLease::try_acquire(&paths).expect("create source run lease marker")
    else {
        panic!("new source run should acquire its lease");
    };
    drop(lease);
    let recorder = JournalRecorder::new(&paths, &meta)
        .await
        .expect("open recorder");
    for ordinal in 0..n_completed {
        recorder
            .record_agent_call(completed_agent_call(ordinal))
            .await
            .expect("append agent_call");
    }
    recorder.shutdown().await.expect("flush + close journal");
    paths
}

async fn mark_source_paused(home: &Path, paths: &WorkflowRunPaths) {
    let meta = paths.read_meta_bounded().expect("read source metadata");
    record_event(
        home,
        &WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: meta.run_id.clone(),
            resumed_from_run_id: None,
            name: meta.name,
            phases: Vec::new(),
            args_digest: meta.args_hash,
        }),
    )
    .await
    .expect("record source begin");
    record_terminal_event(
        home,
        &WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: meta.run_id.clone(),
            status: ProtocolAgentStatus::Interrupted,
            terminal_reason: Some(WorkflowRunTerminalReason::Paused),
            spent: 0,
            total: None,
        }),
        DurableRunStatus::Paused,
    )
    .await
    .expect("record paused source ledger");
    paths
        .update_status(codex_workflow_journal::WorkflowRunStatus::Paused)
        .expect("persist paused source metadata");
}

async fn mark_source_terminal(
    home: &Path,
    paths: &WorkflowRunPaths,
    protocol_status: ProtocolAgentStatus,
    journal_status: codex_workflow_journal::WorkflowRunStatus,
) {
    let meta = paths.read_meta_bounded().expect("read source metadata");
    record_event(
        home,
        &WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: meta.run_id.clone(),
            resumed_from_run_id: None,
            name: meta.name,
            phases: Vec::new(),
            args_digest: meta.args_hash,
        }),
    )
    .await
    .expect("record source begin");
    let terminal_reason = match &protocol_status {
        ProtocolAgentStatus::Completed(_) => WorkflowRunTerminalReason::Completed,
        ProtocolAgentStatus::Errored(_) => WorkflowRunTerminalReason::Failed,
        ProtocolAgentStatus::Interrupted => WorkflowRunTerminalReason::Interrupted,
        ProtocolAgentStatus::Shutdown => WorkflowRunTerminalReason::Stopped,
        ProtocolAgentStatus::PendingInit
        | ProtocolAgentStatus::Running
        | ProtocolAgentStatus::NotFound => panic!("fixture must use a terminal run status"),
    };
    record_event(
        home,
        &WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: meta.run_id,
            status: protocol_status,
            terminal_reason: Some(terminal_reason),
            spent: 0,
            total: None,
        }),
    )
    .await
    .expect("record source terminal");
    paths
        .update_status(journal_status)
        .expect("persist source terminal metadata");
}

fn model_error_text(error: FunctionCallError) -> String {
    match error {
        FunctionCallError::RespondToModel(message) | FunctionCallError::Fatal(message) => message,
    }
}

fn assert_resume_unavailable(error: FunctionCallError) {
    assert_eq!(model_error_text(error), RESUME_UNAVAILABLE_MESSAGE);
}

fn write_legacy_agent_calls(paths: &WorkflowRunPaths, calls: Vec<AgentCallLine>) {
    let mut meta = paths.read_meta_bounded().expect("read source metadata");
    meta.owner_thread_id = None;
    meta.execution_fingerprint = None;
    assert_eq!(
        (meta.owner_thread_id.as_ref(), meta.execution_fingerprint.as_ref()),
        (None, None),
        "the original unbounded writer predates both record-cap markers",
    );
    let mut bytes = serde_json::to_vec(&meta).expect("serialize source metadata");
    bytes.push(b'\n');
    for call in calls {
        bytes.extend(
            serde_json::to_vec(&JournalLine::AgentCall(Box::new(call)))
                .expect("serialize legacy agent call"),
        );
        bytes.push(b'\n');
    }
    std::fs::write(paths.journal(), bytes).expect("write legacy journal");
}

const SAMPLE_WORKFLOW: &str = "export const meta = { name: 'triage', description: 'triage workflow' };\n\
     export default async () => {};\n";

#[tokio::test]
async fn load_resume_seed_identical_script_and_args_returns_full_prefix() {
    let home = tempfile::tempdir().expect("tempdir");
    let args = json!({ "target": "src" });
    write_source_run(
        home.path(),
        SOURCE_RUN_ID,
        SAMPLE_WORKFLOW,
        &args,
        /*n_completed*/ 3,
        /*execution_fingerprint*/ None,
    )
    .await;

    let seed = load_resume_seed(
        home.path(),
        SOURCE_RUN_ID,
        SAMPLE_WORKFLOW,
        &args,
        /*execution_fingerprint*/ None,
    )
    .expect("load seed");

    assert_eq!(seed.source_run_id, SOURCE_RUN_ID);
    assert_eq!(seed.divergence, None);
    assert_eq!(seed.replay_entries.len(), 3);
    assert_eq!(
        seed.replay_entries
            .iter()
            .map(|entry| entry.ordinal)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

#[tokio::test]
async fn load_resume_seed_runs_live_from_an_original_writer_record_over_the_scanner_cap() {
    let home = tempfile::tempdir().expect("tempdir");
    let args = json!({ "target": "src" });
    let paths = write_source_run(
        home.path(),
        SOURCE_RUN_ID,
        SAMPLE_WORKFLOW,
        &args,
        /*n_completed*/ 0,
        /*execution_fingerprint*/ None,
    )
    .await;
    let mut oversized = completed_agent_call(0);
    // The original writer had no 128 KiB serialized-record ceiling. Keep this return at that
    // historical boundary so the complete record crosses the scanner cap, rather than merely
    // crossing today's smaller return-value limit.
    oversized.ret = serde_json::Value::String("x".repeat(LEGACY_JOURNAL_RECORD_MAX_BYTES));
    write_legacy_agent_calls(&paths, vec![oversized, completed_agent_call(1)]);
    let journal = std::fs::read(paths.journal()).expect("read oversized legacy journal");
    let first_agent_call = journal
        .split(|byte| *byte == b'\n')
        .nth(1)
        .expect("journal contains the oversized agent_call");
    assert!(first_agent_call.starts_with(br#"{"type":"agent_call","#));
    assert!(
        first_agent_call.len() > LEGACY_JOURNAL_RECORD_MAX_BYTES,
        "fixture agent_call must cross the scanner record cap",
    );

    let seed = load_resume_seed_off_thread(
        home.path(),
        SOURCE_RUN_ID,
        SAMPLE_WORKFLOW,
        &args,
        /*execution_fingerprint*/ None,
    )
    .await
    .expect("oversized legacy return should be a live replay boundary");

    assert_eq!(seed.divergence, None);
    assert!(seed.replay_entries.is_empty());
}

#[tokio::test]
async fn load_resume_seed_keeps_the_safe_prefix_before_an_oversized_legacy_return() {
    let home = tempfile::tempdir().expect("tempdir");
    let args = json!({ "target": "src" });
    let paths = write_source_run(
        home.path(),
        SOURCE_RUN_ID,
        SAMPLE_WORKFLOW,
        &args,
        /*n_completed*/ 0,
        /*execution_fingerprint*/ None,
    )
    .await;
    let mut oversized = completed_agent_call(2);
    oversized.ret = serde_json::Value::String(
        "x".repeat(codex_workflow_journal::WORKFLOW_AGENT_RETURN_MAX_BYTES),
    );
    write_legacy_agent_calls(
        &paths,
        vec![
            completed_agent_call(0),
            completed_agent_call(1),
            oversized,
            completed_agent_call(3),
        ],
    );

    let seed = load_resume_seed(
        home.path(),
        SOURCE_RUN_ID,
        SAMPLE_WORKFLOW,
        &args,
        /*execution_fingerprint*/ None,
    )
    .expect("oversized legacy return should preserve its safe prefix");

    assert_eq!(seed.divergence, None);
    assert_eq!(
        seed.replay_entries
            .iter()
            .map(|entry| entry.ordinal)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[tokio::test]
async fn load_resume_seed_reordered_args_preserves_prefix() {
    let home = tempfile::tempdir().expect("tempdir");
    let source_args: serde_json::Value =
        serde_json::from_str(r#"{"target":"src","filters":{"kind":"rust","changed":true}}"#)
            .expect("source args");
    let reordered_args: serde_json::Value =
        serde_json::from_str(r#"{"filters":{"changed":true,"kind":"rust"},"target":"src"}"#)
            .expect("reordered args");
    write_source_run(
        home.path(),
        SOURCE_RUN_ID,
        SAMPLE_WORKFLOW,
        &source_args,
        /*n_completed*/ 2,
        /*execution_fingerprint*/ None,
    )
    .await;

    let seed = load_resume_seed(
        home.path(),
        SOURCE_RUN_ID,
        SAMPLE_WORKFLOW,
        &reordered_args,
        /*execution_fingerprint*/ None,
    )
    .expect("load seed");

    assert_eq!(seed.divergence, None);
    assert_eq!(
        seed.replay_entries
            .iter()
            .map(|entry| entry.ordinal)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[tokio::test]
async fn load_resume_seed_requires_matching_execution_fingerprint() {
    let home = tempfile::tempdir().expect("tempdir");
    let args = json!({ "target": "src" });
    write_source_run(
        home.path(),
        SOURCE_RUN_ID,
        SAMPLE_WORKFLOW,
        &args,
        /*n_completed*/ 2,
        /*execution_fingerprint*/ Some("blake3:provider-a"),
    )
    .await;

    let matching = load_resume_seed(
        home.path(),
        SOURCE_RUN_ID,
        SAMPLE_WORKFLOW,
        &args,
        /*execution_fingerprint*/ Some("blake3:provider-a"),
    )
    .expect("matching execution environment loads the prefix");
    assert_eq!(matching.divergence, None);
    assert_eq!(matching.replay_entries.len(), 2);

    let changed = load_resume_seed(
        home.path(),
        SOURCE_RUN_ID,
        SAMPLE_WORKFLOW,
        &args,
        /*execution_fingerprint*/ Some("blake3:provider-b"),
    )
    .expect("changed execution environment is soft divergence");
    assert_eq!(changed.divergence, Some(Divergence::ExecutionFingerprint));
    assert!(changed.replay_entries.is_empty());
}

#[tokio::test]
async fn load_resume_seed_changed_script_diverges_at_zero() {
    let home = tempfile::tempdir().expect("tempdir");
    let args = json!({ "target": "src" });
    write_source_run(
        home.path(),
        SOURCE_RUN_ID,
        SAMPLE_WORKFLOW,
        &args,
        /*n_completed*/ 3,
        /*execution_fingerprint*/ None,
    )
    .await;

    let edited = "export const meta = { name: \"triage\", version: \"1\" };\n\
         export default async () => { /* edited */ };\n";
    let seed = load_resume_seed(
        home.path(),
        SOURCE_RUN_ID,
        edited,
        &args,
        /*execution_fingerprint*/ None,
    )
    .expect("load seed");

    assert_eq!(seed.divergence, Some(Divergence::ScriptHash));
    assert!(seed.replay_entries.is_empty());
}

#[tokio::test]
async fn load_resume_seed_changed_args_diverges_at_zero() {
    let home = tempfile::tempdir().expect("tempdir");
    write_source_run(
        home.path(),
        SOURCE_RUN_ID,
        SAMPLE_WORKFLOW,
        &json!({ "target": "src" }),
        /*n_completed*/ 2,
        /*execution_fingerprint*/ None,
    )
    .await;

    let seed = load_resume_seed(
        home.path(),
        SOURCE_RUN_ID,
        SAMPLE_WORKFLOW,
        &json!({ "target": "OTHER" }),
        /*execution_fingerprint*/ None,
    )
    .expect("load seed");

    assert_eq!(seed.divergence, Some(Divergence::ArgsHash));
    assert!(seed.replay_entries.is_empty());
}

#[tokio::test]
async fn load_resume_seed_missing_source_journal_is_error() {
    let home = tempfile::tempdir().expect("tempdir");
    let missing_run_id = uuid::Uuid::now_v7().to_string();
    let error = load_resume_seed(
        home.path(),
        &missing_run_id,
        SAMPLE_WORKFLOW,
        &json!(null),
        /*execution_fingerprint*/ None,
    )
    .expect_err("missing source run must error");
    assert_resume_unavailable(error);
}

#[tokio::test]
async fn load_resume_seed_rejects_a_journal_copied_under_another_run_id() {
    let home = tempfile::tempdir().expect("tempdir");
    let actual_run_id = uuid::Uuid::now_v7().to_string();
    let requested_run_id = uuid::Uuid::now_v7().to_string();
    let actual_paths = write_source_run(
        home.path(),
        &actual_run_id,
        SAMPLE_WORKFLOW,
        &json!(null),
        /*n_completed*/ 1,
        /*execution_fingerprint*/ None,
    )
    .await;
    let requested_paths = WorkflowRunPaths::new(home.path(), &requested_run_id);
    requested_paths
        .create_dir()
        .expect("create requested run dir");
    std::fs::copy(actual_paths.journal(), requested_paths.journal())
        .expect("copy foreign journal under requested run id");

    let error = load_resume_seed(
        home.path(),
        &requested_run_id,
        SAMPLE_WORKFLOW,
        &json!(null),
        /*execution_fingerprint*/ None,
    )
    .expect_err("line-zero run id must authenticate the requested journal path");
    assert_resume_unavailable(error);
}

#[tokio::test]
async fn prefix_replay_is_owner_scoped_for_models_and_excludes_live_or_paused_sources() {
    let home = tempfile::tempdir().expect("tempdir");
    let args = json!({ "target": "src" });
    let source_run_id = uuid::Uuid::now_v7().to_string();
    let source_paths = write_source_run(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        &args,
        /*n_completed*/ 2,
        /*execution_fingerprint*/ None,
    )
    .await;
    let mut features = Features::default();
    features.enable(Feature::Workflow);

    let wrong_owner = prepare_workflow_prefix_replay(
        &features,
        &WorkflowEventTarget::Disabled,
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        &args,
        WorkflowReplayAccess::ThreadOwner("01900000-0000-7000-8000-000000000099".to_string()),
    )
    .await
    .expect_err("a model thread cannot replay another thread's source");
    assert_resume_unavailable(wrong_owner);

    let owned = prepare_workflow_prefix_replay(
        &features,
        &WorkflowEventTarget::Disabled,
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        &args,
        WorkflowReplayAccess::ThreadOwner(OWNER_THREAD_ID.to_string()),
    )
    .await
    .expect("the owning model thread can replay an inactive source");
    assert_eq!(owned.source_run_id, source_run_id);
    assert_eq!(owned.successor_run_id, None);
    assert_eq!(owned.replay_entries.len(), 2);
    assert_eq!(owned.divergence, None);

    let markerless_run_id = uuid::Uuid::now_v7().to_string();
    let markerless_paths = write_source_run(
        home.path(),
        &markerless_run_id,
        SAMPLE_WORKFLOW,
        &args,
        /*n_completed*/ 1,
        /*execution_fingerprint*/ None,
    )
    .await;
    std::fs::remove_file(markerless_paths.lease()).expect("remove legacy lease marker");
    let markerless_running = prepare_workflow_prefix_replay(
        &features,
        &WorkflowEventTarget::Disabled,
        home.path(),
        &markerless_run_id,
        SAMPLE_WORKFLOW,
        &args,
        WorkflowReplayAccess::ThreadOwner(OWNER_THREAD_ID.to_string()),
    )
    .await
    .expect_err("markerless running metadata is unknown, not inactive");
    assert_resume_unavailable(markerless_running);

    let markerless_local = prepare_workflow_prefix_replay(
        &features,
        &WorkflowEventTarget::Disabled,
        home.path(),
        &markerless_run_id,
        SAMPLE_WORKFLOW,
        &args,
        WorkflowReplayAccess::LocalProcess,
    )
    .await
    .expect_err("local CLI should receive actionable legacy-unknown guidance");
    assert_eq!(
        model_error_text(markerless_local),
        LEGACY_UNKNOWN_RESUME_MESSAGE
    );
    markerless_paths
        .update_status(codex_workflow_journal::WorkflowRunStatus::Completed)
        .expect("publish durable legacy terminal status");
    let markerless_terminal = prepare_workflow_prefix_replay(
        &features,
        &WorkflowEventTarget::Disabled,
        home.path(),
        &markerless_run_id,
        SAMPLE_WORKFLOW,
        &args,
        WorkflowReplayAccess::ThreadOwner(OWNER_THREAD_ID.to_string()),
    )
    .await
    .expect("owner-authenticated terminal metadata is sufficient legacy proof");
    assert_eq!(markerless_terminal.source_run_id, markerless_run_id);
    assert_eq!(markerless_terminal.replay_entries.len(), 1);

    let Some(WorkflowRunLeaseAcquire::Acquired(live_lease)) =
        WorkflowRunLease::try_acquire_existing(&source_paths).expect("acquire source lease")
    else {
        panic!("inactive source lease should be acquirable");
    };
    let live = prepare_workflow_prefix_replay(
        &features,
        &WorkflowEventTarget::Disabled,
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        &args,
        WorkflowReplayAccess::LocalProcess,
    )
    .await
    .expect_err("a local replay cannot fork a live source");
    assert_resume_unavailable(live);
    drop(live_lease);

    mark_source_paused(home.path(), &source_paths).await;
    let paused = prepare_workflow_prefix_replay(
        &features,
        &WorkflowEventTarget::Disabled,
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        &args,
        WorkflowReplayAccess::LocalProcess,
    )
    .await
    .expect_err("a paused checkpoint stays on authenticated one-successor resume");
    assert_resume_unavailable(paused);
}

#[tokio::test]
async fn prefix_replay_accepts_authoritative_terminal_proof_with_stale_running_metadata() {
    let home = tempfile::tempdir().expect("tempdir");
    let source_run_id = uuid::Uuid::now_v7().to_string();
    let paths = write_source_run(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        &json!(null),
        /*n_completed*/ 1,
        /*execution_fingerprint*/ None,
    )
    .await;
    let meta = paths.read_meta_bounded().expect("read source metadata");
    record_event(
        home.path(),
        &WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: source_run_id.clone(),
            resumed_from_run_id: None,
            name: meta.name,
            phases: Vec::new(),
            args_digest: meta.args_hash,
        }),
    )
    .await
    .expect("record source begin");
    record_event(
        home.path(),
        &WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: source_run_id.clone(),
            status: ProtocolAgentStatus::Completed(None),
            terminal_reason: Some(WorkflowRunTerminalReason::Completed),
            spent: 0,
            total: None,
        }),
    )
    .await
    .expect("record authoritative completion");
    std::fs::remove_file(paths.lease()).expect("remove legacy lease marker");

    let mut features = Features::default();
    features.enable(Feature::Workflow);
    let seed = prepare_workflow_prefix_replay(
        &features,
        &WorkflowEventTarget::Disabled,
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        &json!(null),
        WorkflowReplayAccess::ThreadOwner(OWNER_THREAD_ID.to_string()),
    )
    .await
    .expect("terminal durable progress overrides markerless stale running metadata");

    assert_eq!(seed.source_run_id, source_run_id);
    assert_eq!(seed.replay_entries.len(), 1);
}

#[tokio::test]
async fn prefix_replay_uses_terminal_metadata_when_progress_is_corrupt() {
    let home = tempfile::tempdir().expect("tempdir");
    let mut features = Features::default();
    features.enable(Feature::Workflow);

    for status in [
        WorkflowRunStatus::Completed,
        WorkflowRunStatus::Stopped,
        WorkflowRunStatus::Failed,
    ] {
        let source_run_id = uuid::Uuid::now_v7().to_string();
        let paths = write_source_run(
            home.path(),
            &source_run_id,
            SAMPLE_WORKFLOW,
            &json!(null),
            /*n_completed*/ 1,
            /*execution_fingerprint*/ None,
        )
        .await;
        paths
            .update_status(status)
            .expect("publish authoritative terminal metadata");
        std::fs::write(paths.progress(), b"{").expect("corrupt rebuildable progress");
        std::fs::remove_file(paths.lease()).expect("remove legacy lease marker");

        let seed = prepare_workflow_prefix_replay(
            &features,
            &WorkflowEventTarget::Disabled,
            home.path(),
            &source_run_id,
            SAMPLE_WORKFLOW,
            &json!(null),
            WorkflowReplayAccess::ThreadOwner(OWNER_THREAD_ID.to_string()),
        )
        .await
        .expect("terminal metadata remains sufficient replay authority");
        assert_eq!(
            (seed.source_run_id, seed.replay_entries.len()),
            (source_run_id, 1)
        );
    }

    for status in [WorkflowRunStatus::Running, WorkflowRunStatus::Paused] {
        let source_run_id = uuid::Uuid::now_v7().to_string();
        let paths = write_source_run(
            home.path(),
            &source_run_id,
            SAMPLE_WORKFLOW,
            &json!(null),
            /*n_completed*/ 1,
            /*execution_fingerprint*/ None,
        )
        .await;
        if status == WorkflowRunStatus::Paused {
            paths
                .update_status(status)
                .expect("publish ambiguous paused metadata");
        }
        std::fs::write(paths.progress(), b"{").expect("corrupt rebuildable progress");
        std::fs::remove_file(paths.lease()).expect("remove legacy lease marker");

        let error = prepare_workflow_prefix_replay(
            &features,
            &WorkflowEventTarget::Disabled,
            home.path(),
            &source_run_id,
            SAMPLE_WORKFLOW,
            &json!(null),
            WorkflowReplayAccess::ThreadOwner(OWNER_THREAD_ID.to_string()),
        )
        .await
        .expect_err("ambiguous status with corrupt progress must fail closed");
        assert_resume_unavailable(error);
    }
}

#[tokio::test]
async fn paused_resume_mints_fresh_run_id_and_records_distinct_resume_lineage() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = "export const meta = { name: 'triage', description: 'triage workflow' };\n\
         text('resumed');";
    let args = json!({ "target": "src" });
    let source_run_id = uuid::Uuid::now_v7().to_string();
    let source_paths = write_source_run(
        home.path(),
        &source_run_id,
        source,
        &args,
        /*n_completed*/ 3,
        /*execution_fingerprint*/ None,
    )
    .await;
    mark_source_paused(home.path(), &source_paths).await;

    struct NoopProgressSessionProvider;

    impl CodeModeSessionProvider for NoopProgressSessionProvider {
        fn create_session<'a>(
            &'a self,
            _delegate: Arc<dyn CodeModeSessionDelegate>,
        ) -> CodeModeSessionProviderFuture<'a> {
            Box::pin(async {
                Ok(Arc::new(InProcessCodeModeSession::new()) as Arc<dyn CodeModeSession>)
            })
        }
    }

    let service = CodeModeService::new(Arc::new(NoopProgressSessionProvider));
    let mut features = Features::default();
    features.enable(Feature::Workflow);

    let prepared = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &source_run_id,
        source,
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ None,
    )
    .await
    .expect("authenticate paused checkpoint");
    let claimed_successor = prepared
        .seed
        .successor_run_id
        .clone()
        .expect("paused source claims one successor");
    let output = run_workflow_source_to_terminal(
        &features,
        &service,
        "wf-resume-1".to_string(),
        Vec::new(),
        &prepared.source,
        prepared.args.clone(),
        super::WorkflowRunLineage {
            parent_run_id: None,
            depth: 0,
        },
        home.path(),
        Some(prepared.seed.clone()),
        WorkflowEventTarget::Disabled,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("resume runs the body");

    assert_ne!(output.run_id, source_run_id);
    assert_eq!(output.run_id, claimed_successor);
    let fresh_paths = WorkflowRunPaths::new(home.path(), &output.run_id);
    let meta_json =
        std::fs::read_to_string(fresh_paths.meta()).expect("resumed run meta.json exists");
    let meta: WorkflowRunMeta = serde_json::from_str(&meta_json).expect("resumed meta.json parses");
    assert_eq!(meta.run_id, output.run_id);
    assert_eq!(meta.parent_run_id, None);
    assert_eq!(
        meta.resumed_from_run_id.as_deref(),
        Some(source_run_id.as_str())
    );
    assert_eq!(
        fresh_paths
            .read_invocation_args_bounded(&meta.args_hash)
            .expect("read successor invocation"),
        args
    );
    assert_eq!(
        source_paths
            .read_meta_bounded()
            .expect("source remains paused")
            .status,
        codex_workflow_journal::WorkflowRunStatus::Paused
    );
    assert_eq!(
        source_paths
            .read_resume_successor_claim()
            .expect("read source successor claim")
            .as_deref(),
        Some(output.run_id.as_str())
    );

    drop(prepared);
    service.shutdown().await.expect("shutdown service");
}

#[tokio::test]
async fn explicit_resume_args_remain_compatible_under_canonical_json_ordering() {
    let home = tempfile::tempdir().expect("tempdir");
    let source_args: serde_json::Value =
        serde_json::from_str(r#"{"target":"src","filters":{"kind":"rust","changed":true}}"#)
            .expect("source args");
    let reordered_args: serde_json::Value =
        serde_json::from_str(r#"{"filters":{"changed":true,"kind":"rust"},"target":"src"}"#)
            .expect("reordered args");
    let source_run_id = uuid::Uuid::now_v7().to_string();
    let paths = write_source_run(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        &source_args,
        /*n_completed*/ 1,
        /*execution_fingerprint*/ None,
    )
    .await;
    mark_source_paused(home.path(), &paths).await;

    let prepared = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Explicit(reordered_args),
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ None,
    )
    .await
    .expect("canonical-equivalent explicit args are accepted");

    assert_eq!(prepared.args, source_args);
    assert_eq!(prepared.seed.divergence, None);
    assert_eq!(prepared.seed.replay_entries.len(), 1);
    drop(prepared);

    let error = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ Some("blake3:new-provider"),
    )
    .await
    .expect_err("checkpoint missing a current fingerprint");
    assert_resume_unavailable(error);
}

#[tokio::test]
async fn resume_rejects_every_nonpaused_authoritative_status() {
    let home = tempfile::tempdir().expect("tempdir");
    let cases = [
        (
            ProtocolAgentStatus::Completed(None),
            codex_workflow_journal::WorkflowRunStatus::Completed,
        ),
        (
            ProtocolAgentStatus::Shutdown,
            codex_workflow_journal::WorkflowRunStatus::Stopped,
        ),
        (
            ProtocolAgentStatus::Interrupted,
            codex_workflow_journal::WorkflowRunStatus::Failed,
        ),
        (
            ProtocolAgentStatus::Errored("fixture failure".to_string()),
            codex_workflow_journal::WorkflowRunStatus::Failed,
        ),
    ];
    for (protocol_status, journal_status) in cases {
        let source_run_id = uuid::Uuid::now_v7().to_string();
        let paths = write_source_run(
            home.path(),
            &source_run_id,
            SAMPLE_WORKFLOW,
            &serde_json::Value::Null,
            /*n_completed*/ 0,
            /*execution_fingerprint*/ None,
        )
        .await;
        mark_source_terminal(home.path(), &paths, protocol_status, journal_status).await;

        let error = prepare_paused_workflow_resume_for_owner(
            home.path(),
            &source_run_id,
            SAMPLE_WORKFLOW,
            WorkflowResumeInvocation::Persisted,
            OWNER_THREAD_ID,
            /*execution_fingerprint*/ None,
        )
        .await
        .expect_err("nonpaused terminal status cannot replay");
        assert_resume_unavailable(error);
    }

    let running_run_id = uuid::Uuid::now_v7().to_string();
    write_source_run(
        home.path(),
        &running_run_id,
        SAMPLE_WORKFLOW,
        &serde_json::Value::Null,
        /*n_completed*/ 0,
        /*execution_fingerprint*/ None,
    )
    .await;
    let error = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &running_run_id,
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ None,
    )
    .await
    .expect_err("running source cannot replay");
    assert_resume_unavailable(error);
}

#[tokio::test]
async fn authoritative_completed_ledger_overrides_stale_paused_metadata() {
    let home = tempfile::tempdir().expect("tempdir");
    let source_run_id = uuid::Uuid::now_v7().to_string();
    let paths = write_source_run(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        &serde_json::Value::Null,
        /*n_completed*/ 0,
        /*execution_fingerprint*/ None,
    )
    .await;
    let meta = paths.read_meta_bounded().expect("read source metadata");
    record_event(
        home.path(),
        &WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: source_run_id.clone(),
            resumed_from_run_id: None,
            name: meta.name,
            phases: Vec::new(),
            args_digest: meta.args_hash,
        }),
    )
    .await
    .expect("record begin");
    record_event(
        home.path(),
        &WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: source_run_id.clone(),
            status: ProtocolAgentStatus::Completed(None),
            terminal_reason: Some(WorkflowRunTerminalReason::Completed),
            spent: 0,
            total: None,
        }),
    )
    .await
    .expect("record completed ledger");
    paths
        .update_status(codex_workflow_journal::WorkflowRunStatus::Paused)
        .expect("write stale paused projection");

    let error = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ None,
    )
    .await
    .expect_err("completed authoritative ledger wins");
    assert_resume_unavailable(error);
}

#[tokio::test]
async fn resume_rejects_corrupt_ledger_ownerless_and_mismatched_metadata() {
    let home = tempfile::tempdir().expect("tempdir");

    let corrupt_run_id = uuid::Uuid::now_v7().to_string();
    let corrupt_paths = write_source_run(
        home.path(),
        &corrupt_run_id,
        SAMPLE_WORKFLOW,
        &serde_json::Value::Null,
        /*n_completed*/ 0,
        /*execution_fingerprint*/ None,
    )
    .await;
    mark_source_paused(home.path(), &corrupt_paths).await;
    std::fs::write(corrupt_paths.progress(), b"{").expect("corrupt progress fixture");
    let error = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &corrupt_run_id,
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ None,
    )
    .await
    .expect_err("corrupt ledger rejected");
    assert_resume_unavailable(error);

    let ownerless_run_id = uuid::Uuid::now_v7().to_string();
    let ownerless_paths = write_source_run(
        home.path(),
        &ownerless_run_id,
        SAMPLE_WORKFLOW,
        &serde_json::Value::Null,
        /*n_completed*/ 0,
        /*execution_fingerprint*/ None,
    )
    .await;
    mark_source_paused(home.path(), &ownerless_paths).await;
    let mut ownerless_meta = ownerless_paths
        .read_meta_bounded()
        .expect("read ownerless fixture metadata");
    ownerless_meta.owner_thread_id = None;
    std::fs::write(
        ownerless_paths.meta(),
        serde_json::to_vec_pretty(&ownerless_meta).expect("serialize ownerless metadata"),
    )
    .expect("write ownerless metadata");
    let error = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &ownerless_run_id,
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ None,
    )
    .await
    .expect_err("ownerless checkpoint rejected");
    assert_resume_unavailable(error);

    let mismatched_run_id = uuid::Uuid::now_v7().to_string();
    let mismatched_paths = write_source_run(
        home.path(),
        &mismatched_run_id,
        SAMPLE_WORKFLOW,
        &serde_json::Value::Null,
        /*n_completed*/ 0,
        /*execution_fingerprint*/ None,
    )
    .await;
    mark_source_paused(home.path(), &mismatched_paths).await;
    let mut mismatched_meta = mismatched_paths
        .read_meta_bounded()
        .expect("read mismatched fixture metadata");
    mismatched_meta.run_id = uuid::Uuid::now_v7().to_string();
    std::fs::write(
        mismatched_paths.meta(),
        serde_json::to_vec_pretty(&mismatched_meta).expect("serialize mismatched metadata"),
    )
    .expect("write mismatched metadata");
    let error = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &mismatched_run_id,
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ None,
    )
    .await
    .expect_err("mismatched metadata rejected");
    assert_resume_unavailable(error);

    let error = prepare_paused_workflow_resume_for_owner(
        home.path(),
        "NOT-A-CANONICAL-RUN-ID",
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ None,
    )
    .await
    .expect_err("noncanonical id rejected");
    assert_resume_unavailable(error);

    let malicious_run_id = format!("{}TOP-SECRET-RUN-ID", "x".repeat(1024 * 1024));
    let error = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &malicious_run_id,
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ None,
    )
    .await
    .expect_err("oversized malicious id rejected before any filesystem work");
    let message = model_error_text(error);
    assert_eq!(message, RESUME_UNAVAILABLE_MESSAGE);
    assert!(!message.contains("TOP-SECRET-RUN-ID"));
}

#[tokio::test]
async fn source_lease_serializes_resume_admission() {
    let home = tempfile::tempdir().expect("tempdir");
    let source_run_id = uuid::Uuid::now_v7().to_string();
    let paths = write_source_run(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        &serde_json::Value::Null,
        /*n_completed*/ 0,
        /*execution_fingerprint*/ None,
    )
    .await;
    mark_source_paused(home.path(), &paths).await;
    let first = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ None,
    )
    .await
    .expect("first resume holds source lease");

    let expected_successor = first
        .seed
        .successor_run_id
        .clone()
        .expect("first admission claims a successor");
    let waiter_home = home.path().to_path_buf();
    let waiter_source_run_id = source_run_id.clone();
    let waiter = tokio::spawn(async move {
        prepare_paused_workflow_resume_for_owner(
            &waiter_home,
            &waiter_source_run_id,
            SAMPLE_WORKFLOW,
            WorkflowResumeInvocation::Persisted,
            OWNER_THREAD_ID,
            /*execution_fingerprint*/ None,
        )
        .await
    });
    tokio::task::yield_now().await;
    assert!(
        !waiter.is_finished(),
        "waiter joins the source admission lease"
    );
    drop(first);
    let joined = waiter
        .await
        .expect("waiter task")
        .expect("waiter acquires the released source lease");
    assert_eq!(joined.seed.successor_run_id, Some(expected_successor));
}

#[tokio::test]
async fn resume_identity_errors_are_bounded_and_do_not_echo_private_args() {
    let home = tempfile::tempdir().expect("tempdir");
    let source_run_id = uuid::Uuid::now_v7().to_string();
    let private_args = json!({ "api_key": "TOP-SECRET-CHECKPOINT-VALUE" });
    let paths = write_source_run(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        &private_args,
        /*n_completed*/ 0,
        /*execution_fingerprint*/ Some("blake3:provider-a"),
    )
    .await;
    mark_source_paused(home.path(), &paths).await;

    let exact = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ Some("blake3:provider-a"),
    )
    .await
    .expect("exact execution fingerprint accepted");
    assert_eq!(exact.args, private_args);
    drop(exact);

    let wrong_args = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Explicit(json!({ "api_key": "LEAK-ME" })),
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ Some("blake3:provider-a"),
    )
    .await
    .expect_err("changed args rejected");
    let message = model_error_text(wrong_args);
    assert!(message.len() < 512);
    assert!(!message.contains("TOP-SECRET"));
    assert!(!message.contains("LEAK-ME"));
    assert!(!message.contains("blake3:"));

    let wrong_source = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &source_run_id,
        "export const meta = { name: 'triage' }; text('changed');",
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ Some("blake3:provider-a"),
    )
    .await
    .expect_err("changed source rejected");
    assert_resume_unavailable(wrong_source);

    let wrong_fingerprint = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ Some("blake3:provider-b"),
    )
    .await
    .expect_err("changed execution environment rejected");
    assert_resume_unavailable(wrong_fingerprint);

    let missing_fingerprint = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ None,
    )
    .await
    .expect_err("missing current execution fingerprint rejected");
    assert_resume_unavailable(missing_fingerprint);

    let wrong_owner = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &source_run_id,
        SAMPLE_WORKFLOW,
        WorkflowResumeInvocation::Persisted,
        "01900000-0000-7000-8000-000000000002",
        /*execution_fingerprint*/ Some("blake3:provider-a"),
    )
    .await
    .expect_err("wrong owner rejected");
    assert_resume_unavailable(wrong_owner);

    let tampered_source = "export const meta = { name: 'triage' }; text('tampered');";
    std::fs::write(paths.script(), tampered_source).expect("tamper persisted script");
    let wrong_script_hash = prepare_paused_workflow_resume_for_owner(
        home.path(),
        &source_run_id,
        tampered_source,
        WorkflowResumeInvocation::Persisted,
        OWNER_THREAD_ID,
        /*execution_fingerprint*/ Some("blake3:provider-a"),
    )
    .await
    .expect_err("persisted script hash mismatch rejected");
    assert_resume_unavailable(wrong_script_hash);
}
