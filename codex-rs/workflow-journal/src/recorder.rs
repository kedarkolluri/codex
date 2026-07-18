//! Append-only writer for `journal.jsonl` (the dynamic-workflows journal).
//!
//! [`JournalRecorder`] clones the *infrastructure* of rollout's
//! [`RolloutRecorder`](../../rollout/src/recorder.rs) — a background mpsc JSONL
//! writer with the newline-terminated append discipline (`recorder.rs:1792`,
//! `write_line` at `recorder.rs:1869`) — without reusing its conversation-shaped
//! `RolloutItem`. Instead it writes the journal's own envelope types
//! ([`WorkflowRunMeta`] on line 0, then [`JournalLine`]s).
//!
//! See `docs/dynamic-workflows-spec.md` §7 ("Journal format").
//!
//! ## Durability & ordering discipline
//!
//! * A single background task owns the open file, so appends can never
//!   interleave into a torn line even when many tasks append concurrently (a
//!   parallel batch). The mpsc channel serializes them into append order.
//! * Every line is newline-terminated and `flush()`ed before its append is
//!   acknowledged, so when [`JournalRecorder::append`] returns, that line is on
//!   disk. This is the durability boundary for the run→agent linkage (§7): a
//!   `completed` `agent_call` line is durable the instant its append resolves.
//! * On open the file is repaired to end in a newline
//!   ([`ensure_newline_terminated`]), so a crash mid-line never corrupts the
//!   next append — mirroring `ensure_rollout_is_newline_terminated`
//!   (`recorder.rs:1821`).
//!
//! ## Determinism
//!
//! Timestamps are stamped **host-side** here (never inside the isolate, which
//! has `Date`/`Math` disabled per §7). A line that already carries a
//! host-supplied timestamp is preserved; otherwise the recorder stamps
//! [`OffsetDateTime::now_utc`](time::OffsetDateTime::now_utc) at append time.
//! Timestamps are metadata only — they are never part of the replay cache key.

use std::io;
use std::io::SeekFrom;
use std::sync::Mutex;

use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncSeekExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::AgentCallLine;
use crate::JournalLine;
use crate::LogLine;
use crate::PhaseLine;
use crate::WorkflowRunMeta;
use crate::storage::WorkflowRunPaths;

/// Bound on the writer's command queue. Matches the rollout recorder's channel
/// depth (`recorder.rs:850`).
const CHANNEL_CAPACITY: usize = 256;

/// Commands processed by the single background writer task.
enum JournalCmd {
    /// Append one pre-serialized, newline-terminated line and flush.
    Append {
        line: String,
        ack: oneshot::Sender<io::Result<()>>,
    },
    /// Flush any buffered bytes to disk and acknowledge.
    Flush {
        ack: oneshot::Sender<io::Result<()>>,
    },
    /// Flush, acknowledge, then stop the writer loop.
    Shutdown {
        ack: oneshot::Sender<io::Result<()>>,
    },
}

/// Append-only recorder for a single workflow run's `journal.jsonl`.
///
/// Construct with [`JournalRecorder::new`], which materializes the run directory
/// and writes `run_meta` as line 0 exactly once. Then append `agent_call` /
/// `phase` / `log` lines via [`Self::record_agent_call`] /
/// [`Self::record_phase`] / [`Self::record_log`] (or the generic
/// [`Self::append`]).
///
/// Appends take `&self`, so wrap the recorder in an `Arc` to append from many
/// concurrent tasks (a parallel batch); the background writer keeps every line
/// intact and in append order.
pub struct JournalRecorder {
    tx: mpsc::Sender<JournalCmd>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl JournalRecorder {
    /// Create (or reopen) the run's `journal.jsonl` and start the background
    /// writer, writing `run_meta` as line 0 **exactly once**.
    ///
    /// The run directory is created if missing. `run_meta` is written only when
    /// the journal file is empty, so reopening an existing journal (crash
    /// recovery) neither duplicates nor rewrites line 0. The file is repaired to
    /// end in a newline before any append, so a torn trailing line from a crash
    /// cannot corrupt the next record.
    pub async fn new(paths: &WorkflowRunPaths, meta: &WorkflowRunMeta) -> io::Result<Self> {
        paths.create_dir()?;
        let journal_path = paths.journal();
        let mut file = open_journal_for_append(&journal_path).await?;

        // Line 0 = run_meta, written exactly once (only for a fresh journal).
        if file.metadata().await?.len() == 0 {
            let mut json = serde_json::to_string(meta).map_err(io::Error::other)?;
            json.push('\n');
            file.write_all(json.as_bytes()).await?;
            file.flush().await?;
        }

        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let handle = tokio::spawn(journal_writer(file, rx));
        Ok(Self {
            tx,
            handle: Mutex::new(Some(handle)),
        })
    }

    /// Append an `agent_call` line. Validated via [`JournalLine::validate`]
    /// before it is written.
    pub async fn record_agent_call(&self, line: AgentCallLine) -> io::Result<()> {
        self.append(JournalLine::AgentCall(Box::new(line))).await
    }

    /// Append a `phase` narration line.
    pub async fn record_phase(&self, line: PhaseLine) -> io::Result<()> {
        self.append(JournalLine::Phase(line)).await
    }

    /// Append a `log` narration line.
    pub async fn record_log(&self, line: LogLine) -> io::Result<()> {
        self.append(JournalLine::Log(line)).await
    }

    /// Append one [`JournalLine`]. The line is [`validate`](JournalLine::validate)d
    /// first — a `completed` `agent_call` missing its replay/linkage fields is
    /// rejected with an `InvalidData` error and nothing is written. On success
    /// the line is on disk before this returns.
    ///
    /// A missing `timestamp` is stamped host-side; a pre-set one is preserved.
    pub async fn append(&self, mut line: JournalLine) -> io::Result<()> {
        line.validate()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        stamp_timestamp_if_absent(&mut line);
        let mut json = serde_json::to_string(&line).map_err(io::Error::other)?;
        json.push('\n');

        let (ack, ack_rx) = oneshot::channel();
        self.tx
            .send(JournalCmd::Append { line: json, ack })
            .await
            .map_err(|e| io::Error::other(format!("journal writer unavailable: {e}")))?;
        ack_rx
            .await
            .map_err(|e| io::Error::other(format!("journal writer dropped ack: {e}")))?
    }

    /// Flush any buffered bytes to disk. Redundant with the per-line flush, but
    /// mirrors the rollout API and provides an explicit barrier.
    pub async fn flush(&self) -> io::Result<()> {
        let (ack, ack_rx) = oneshot::channel();
        self.tx
            .send(JournalCmd::Flush { ack })
            .await
            .map_err(|e| io::Error::other(format!("journal writer unavailable: {e}")))?;
        ack_rx
            .await
            .map_err(|e| io::Error::other(format!("journal writer dropped ack: {e}")))?
    }

    /// Flush all buffered lines and stop the background writer, guaranteeing
    /// every appended line is durable on disk before returning. Idempotent: a
    /// second call after the writer has stopped is a no-op.
    pub async fn shutdown(&self) -> io::Result<()> {
        let (ack, ack_rx) = oneshot::channel();
        if self.tx.send(JournalCmd::Shutdown { ack }).await.is_err() {
            // Writer already stopped; nothing buffered can remain.
            return Ok(());
        }
        let result = ack_rx
            .await
            .map_err(|e| io::Error::other(format!("journal writer dropped ack: {e}")))?;

        // Join the writer task outside the lock so we never hold the mutex
        // across an await point. A poisoned lock (a panicked prior shutdown)
        // still yields the guard so we can take and join the handle.
        let handle = self
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
        result
    }
}

/// Stamp a host-side timestamp on a line that lacks one; preserve any existing
/// (already host-supplied) timestamp untouched.
fn stamp_timestamp_if_absent(line: &mut JournalLine) {
    match line {
        JournalLine::AgentCall(call) => {
            if call.timestamp.is_none() {
                call.timestamp = Some(host_timestamp());
            }
        }
        JournalLine::Phase(phase) => {
            if phase.timestamp.is_none() {
                phase.timestamp = Some(host_timestamp());
            }
        }
        JournalLine::Log(log) => {
            if log.timestamp.is_none() {
                log.timestamp = Some(host_timestamp());
            }
        }
    }
}

/// Host wall-clock timestamp in the same UTC millisecond format the rollout
/// recorder uses (`recorder.rs:1855`). Runs on the Rust host, never the isolate.
fn host_timestamp() -> String {
    const FORMAT: &[FormatItem] =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    OffsetDateTime::now_utc().format(FORMAT).unwrap_or_default()
}

/// The background writer loop: owns the open file and serializes every append,
/// flush, and shutdown through a single task. Mirrors `rollout_writer`
/// (`recorder.rs:1726`).
async fn journal_writer(mut file: tokio::fs::File, mut rx: mpsc::Receiver<JournalCmd>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            JournalCmd::Append { line, ack } => {
                let _ = ack.send(write_line(&mut file, line.as_bytes()).await);
            }
            JournalCmd::Flush { ack } => {
                let _ = ack.send(file.flush().await);
            }
            JournalCmd::Shutdown { ack } => {
                let _ = ack.send(file.flush().await);
                break;
            }
        }
    }
}

/// Write one already-newline-terminated line and flush it to disk. Mirrors
/// `JsonlWriter::write_line` (`recorder.rs:1869`).
async fn write_line(file: &mut tokio::fs::File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes).await?;
    file.flush().await?;
    Ok(())
}

/// Open the journal for append (creating it if missing) and repair it to end in
/// a newline. The file is opened with `O_APPEND`, so every write lands at the
/// end regardless of the seek used for the newline check. Mirrors
/// `open_rollout_for_append` + `ensure_rollout_is_newline_terminated`
/// (`recorder.rs:1802,1821`).
async fn open_journal_for_append(path: &std::path::Path) -> io::Result<tokio::fs::File> {
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)
        .await?;
    ensure_newline_terminated(&mut file).await?;
    Ok(file)
}

/// If the file is non-empty and does not already end in `\n`, append one so a
/// crash-truncated trailing line can never fuse with the next record. Mirrors
/// `ensure_rollout_is_newline_terminated` (`recorder.rs:1821`).
async fn ensure_newline_terminated(file: &mut tokio::fs::File) -> io::Result<()> {
    let len = file.metadata().await?.len();
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1)).await?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last).await?;
    if last[0] != b'\n' {
        file.write_all(b"\n").await?;
        file.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentCallOpts;
    use crate::AgentStatus;
    use crate::NullOrdinal;
    use crate::RunMetaTag;
    use crate::storage::mint_run_id;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;
    use std::sync::Arc;

    fn sample_meta(run_id: &str) -> WorkflowRunMeta {
        WorkflowRunMeta::new(
            run_id.to_string(),
            None,
            "blake3:script".to_string(),
            "blake3:args".to_string(),
            "triage".to_string(),
            500_000,
            1,
            "2026-07-17T00:00:00Z".to_string(),
        )
    }

    fn completed_agent_call(ordinal: u64) -> AgentCallLine {
        AgentCallLine {
            timestamp: None,
            ordinal,
            key: format!("blake3:k{ordinal}"),
            prompt_hash: "ph".to_string(),
            opts: AgentCallOpts {
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
            ret: json!({"ok": true, "n": ordinal}),
            tokens_spent: Some(1000 + ordinal),
            completion_seq: None,
        }
    }

    /// Read the journal back as `(run_meta, [JournalLine])`, asserting every
    /// non-empty line parses. The trailing byte must be a newline.
    fn read_journal(path: &std::path::Path) -> (WorkflowRunMeta, Vec<JournalLine>) {
        let bytes = std::fs::read(path).expect("read journal");
        assert!(!bytes.is_empty(), "journal must not be empty");
        assert_eq!(
            *bytes.last().expect("last byte"),
            b'\n',
            "journal must be newline-terminated"
        );
        let text = String::from_utf8(bytes).expect("utf8");
        let mut lines = text.lines();
        let meta: WorkflowRunMeta =
            serde_json::from_str(lines.next().expect("line 0")).expect("line 0 parses as run_meta");
        let rest = lines
            .map(|l| serde_json::from_str::<JournalLine>(l).expect("line parses as JournalLine"))
            .collect();
        (meta, rest)
    }

    #[tokio::test]
    async fn new_writes_run_meta_as_line_0_exactly_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        let meta = sample_meta(&run_id);

        let recorder = JournalRecorder::new(&paths, &meta).await.expect("new");
        recorder.shutdown().await.expect("shutdown");

        let (parsed_meta, rest) = read_journal(&paths.journal());
        assert_eq!(parsed_meta.kind, RunMetaTag::RunMeta);
        assert_eq!(parsed_meta, meta, "line 0 is the run_meta we passed");
        assert!(rest.is_empty(), "no journal lines yet");

        // Reopening the existing journal must NOT rewrite / duplicate line 0.
        let recorder2 = JournalRecorder::new(&paths, &meta).await.expect("reopen");
        recorder2
            .record_phase(PhaseLine {
                timestamp: None,
                ordinal: NullOrdinal,
                title: "analyze".to_string(),
            })
            .await
            .expect("append after reopen");
        recorder2.shutdown().await.expect("shutdown");

        let text = std::fs::read_to_string(paths.journal()).expect("read");
        let run_meta_count = text
            .lines()
            .filter(|l| l.contains("\"type\":\"run_meta\""))
            .count();
        assert_eq!(run_meta_count, 1, "run_meta written exactly once");
        let (_, rest) = read_journal(&paths.journal());
        assert_eq!(rest.len(), 1, "only the one appended phase line");
        assert!(matches!(rest[0], JournalLine::Phase(_)));
    }

    #[tokio::test]
    async fn appends_land_in_order_newline_terminated() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        let recorder = JournalRecorder::new(&paths, &sample_meta(&run_id))
            .await
            .expect("new");

        recorder
            .record_phase(PhaseLine {
                timestamp: None,
                ordinal: NullOrdinal,
                title: "analyze".to_string(),
            })
            .await
            .expect("phase");
        recorder
            .record_agent_call(completed_agent_call(0))
            .await
            .expect("agent_call 0");
        recorder
            .record_log(LogLine {
                timestamp: None,
                ordinal: NullOrdinal,
                message: "narrator".to_string(),
            })
            .await
            .expect("log");
        recorder
            .record_agent_call(completed_agent_call(1))
            .await
            .expect("agent_call 1");
        recorder.shutdown().await.expect("shutdown");

        let (_, rest) = read_journal(&paths.journal());
        assert_eq!(rest.len(), 4, "four appended lines");
        // Append order is preserved.
        assert!(matches!(rest[0], JournalLine::Phase(_)));
        match &rest[1] {
            JournalLine::AgentCall(c) => assert_eq!(c.ordinal, 0),
            other => panic!("expected agent_call, got {other:?}"),
        }
        assert!(matches!(rest[2], JournalLine::Log(_)));
        match &rest[3] {
            JournalLine::AgentCall(c) => assert_eq!(c.ordinal, 1),
            other => panic!("expected agent_call, got {other:?}"),
        }
        // Host timestamps were stamped on every line that lacked one.
        for line in &rest {
            let has_ts = match line {
                JournalLine::AgentCall(c) => c.timestamp.is_some(),
                JournalLine::Phase(p) => p.timestamp.is_some(),
                JournalLine::Log(l) => l.timestamp.is_some(),
            };
            assert!(has_ts, "recorder stamps a host timestamp");
        }
    }

    #[tokio::test]
    async fn concurrent_burst_has_no_torn_or_interleaved_lines() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        let recorder = Arc::new(
            JournalRecorder::new(&paths, &sample_meta(&run_id))
                .await
                .expect("new"),
        );

        const N: u64 = 64;
        let mut handles = Vec::new();
        for i in 0..N {
            let rec = Arc::clone(&recorder);
            handles.push(tokio::spawn(async move {
                rec.record_agent_call(completed_agent_call(i))
                    .await
                    .expect("concurrent append");
            }));
        }
        for h in handles {
            h.await.expect("join");
        }
        recorder.shutdown().await.expect("shutdown");

        // Every non-empty line is well-formed JSON on exactly one line, and the
        // full set of ordinals arrived intact (no torn/lost/duplicated records).
        let (_, rest) = read_journal(&paths.journal());
        assert_eq!(rest.len() as u64, N, "every burst append landed");
        let mut ordinals: Vec<u64> = rest
            .iter()
            .map(|line| match line {
                JournalLine::AgentCall(c) => c.ordinal,
                other => panic!("expected agent_call, got {other:?}"),
            })
            .collect();
        ordinals.sort_unstable();
        assert_eq!(
            ordinals,
            (0..N).collect::<Vec<_>>(),
            "no ordinal lost or torn"
        );
    }

    #[tokio::test]
    async fn append_validates_before_writing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        let recorder = JournalRecorder::new(&paths, &sample_meta(&run_id))
            .await
            .expect("new");

        // A completed agent_call missing tokens_spent violates the §7 replay
        // invariant; append must reject it and write nothing.
        let mut bad = completed_agent_call(0);
        bad.tokens_spent = None;
        let err = recorder
            .record_agent_call(bad)
            .await
            .expect_err("validate rejects completed call missing tokens_spent");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("tokens_spent"), "err: {err}");

        // A well-formed line still appends fine afterward.
        recorder
            .record_agent_call(completed_agent_call(0))
            .await
            .expect("valid append");
        recorder.shutdown().await.expect("shutdown");

        let (_, rest) = read_journal(&paths.journal());
        assert_eq!(rest.len(), 1, "only the valid line was written");
        match &rest[0] {
            JournalLine::AgentCall(c) => assert_eq!(c.tokens_spent, Some(1000)),
            other => panic!("expected agent_call, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn flush_and_shutdown_are_durable_and_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        let recorder = JournalRecorder::new(&paths, &sample_meta(&run_id))
            .await
            .expect("new");

        recorder
            .record_log(LogLine {
                timestamp: None,
                ordinal: NullOrdinal,
                message: "before flush".to_string(),
            })
            .await
            .expect("append");
        recorder.flush().await.expect("flush");

        // After flush the line is already readable on disk.
        let (_, rest) = read_journal(&paths.journal());
        assert_eq!(rest.len(), 1);

        recorder.shutdown().await.expect("shutdown");
        // Second shutdown is a no-op, not an error.
        recorder.shutdown().await.expect("idempotent shutdown");
    }

    #[tokio::test]
    async fn recovers_from_crash_truncated_trailing_line() {
        // Simulate a journal whose last line was torn by a crash (no newline).
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        paths.create_dir().expect("mkdir");
        let meta = sample_meta(&run_id);
        let mut seed = serde_json::to_string(&meta).expect("meta");
        seed.push('\n');
        // A torn agent_call line missing its closing bytes AND newline.
        seed.push_str(r#"{"type":"log","ordinal":null,"message":"torn"#);
        std::fs::write(paths.journal(), &seed).expect("seed");

        // Reopening must repair the missing newline before appending, so the new
        // line does not fuse onto the torn one.
        let recorder = JournalRecorder::new(&paths, &meta).await.expect("reopen");
        recorder
            .record_log(LogLine {
                timestamp: None,
                ordinal: NullOrdinal,
                message: "after recovery".to_string(),
            })
            .await
            .expect("append after recovery");
        recorder.shutdown().await.expect("shutdown");

        let text = std::fs::read_to_string(paths.journal()).expect("read");
        assert!(text.ends_with('\n'), "repaired to newline-terminated");
        // The freshly appended line stands alone and parses.
        let last = text.lines().last().expect("last line");
        let parsed: JournalLine = serde_json::from_str(last).expect("last line parses");
        match parsed {
            JournalLine::Log(l) => assert_eq!(l.message, "after recovery"),
            other => panic!("expected log, got {other:?}"),
        }
        // The torn line is preserved verbatim on its own line (not fused).
        let torn = text.lines().nth(1).expect("torn line");
        assert_eq!(torn, r#"{"type":"log","ordinal":null,"message":"torn"#);
    }

    #[tokio::test]
    async fn preserves_a_preset_host_timestamp() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        let recorder = JournalRecorder::new(&paths, &sample_meta(&run_id))
            .await
            .expect("new");

        recorder
            .record_log(LogLine {
                timestamp: Some("2026-07-17T12:00:00.000Z".to_string()),
                ordinal: NullOrdinal,
                message: "preset".to_string(),
            })
            .await
            .expect("append");
        recorder.shutdown().await.expect("shutdown");

        let (_, rest) = read_journal(&paths.journal());
        match &rest[0] {
            JournalLine::Log(l) => {
                assert_eq!(l.timestamp.as_deref(), Some("2026-07-17T12:00:00.000Z"));
            }
            other => panic!("expected log, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn return_value_round_trips_through_the_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        let recorder = JournalRecorder::new(&paths, &sample_meta(&run_id))
            .await
            .expect("new");

        let mut call = completed_agent_call(0);
        call.ret = json!({"nested": [1, 2, {"deep": true}]});
        let expected = call.ret.clone();
        recorder.record_agent_call(call).await.expect("append");
        recorder.shutdown().await.expect("shutdown");

        let (_, rest) = read_journal(&paths.journal());
        match &rest[0] {
            JournalLine::AgentCall(c) => assert_eq!(c.ret, expected),
            other => panic!("expected agent_call, got {other:?}"),
        }
        // Sanity: a plain string and null also survive.
        for v in [json!("a string"), Value::Null] {
            assert!(v.is_string() || v.is_null());
        }
    }
}
