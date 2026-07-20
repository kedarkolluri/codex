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
//! * Every line is newline-terminated and data-synced before its append is
//!   acknowledged, so when [`JournalRecorder::append`] returns, that line has
//!   crossed the filesystem durability barrier. This is the boundary for the run→agent linkage (§7): a
//!   `completed` `agent_call` line is durable the instant its append resolves.
//! * On open, a complete final record that merely lacks its newline is
//!   terminated, while an unparsable crash tail is truncated
//!   ([`repair_crash_tail`]). A crash mid-line therefore never corrupts the next
//!   append or becomes a durable malformed record.
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

use crate::AgentBoundLine;
use crate::AgentCallLine;
use crate::JournalLine;
use crate::LogLine;
use crate::PhaseLine;
use crate::WorkflowRunMeta;
use crate::storage::WorkflowJournalState;
use crate::storage::WorkflowRunPaths;

/// Bound on the writer's command queue. Matches the rollout recorder's channel
/// depth (`recorder.rs:850`).
const CHANNEL_CAPACITY: usize = 256;

/// Commands processed by the single background writer task.
enum JournalCmd {
    /// Append one pre-serialized, newline-terminated line and sync its data.
    Append {
        line: String,
        ack: oneshot::Sender<io::Result<()>>,
    },
    /// Flush and sync any buffered bytes to disk before acknowledging.
    Flush {
        ack: oneshot::Sender<io::Result<()>>,
    },
    /// Flush, data-sync, acknowledge, then stop the writer loop.
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
    /// have a repaired crash tail before any append, so a torn trailing line
    /// cannot corrupt the next record or become durable corruption.
    pub async fn new(paths: &WorkflowRunPaths, meta: &WorkflowRunMeta) -> io::Result<Self> {
        paths.create_dir()?;
        // A fixed-id resume may reopen a crash-partial successor. Authenticate
        // line zero before appending so a claimed id can never adopt a journal
        // belonging to different immutable metadata.
        let journal_state = paths.journal_state(meta)?;
        if journal_state == WorkflowJournalState::PartialHeader {
            paths.repair_partial_journal_header(meta)?;
        }
        let journal_path = paths.journal();
        let mut file = open_journal_for_append(&journal_path).await?;

        // Line 0 = run_meta, written exactly once (only for a fresh journal).
        if file.metadata().await?.len() == 0 {
            let mut json = serde_json::to_string(meta).map_err(io::Error::other)?;
            ensure_record_within_bounds(&json)?;
            json.push('\n');
            file.write_all(json.as_bytes()).await?;
            sync_file_data(&mut file).await?;
        }

        let journal_len = file.metadata().await?.len();
        let journal_record_count = count_journal_records(&mut file).await?;
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let handle = tokio::spawn(journal_writer(file, journal_len, journal_record_count, rx));
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

    /// Persist a child binding before the child's first turn starts.
    pub async fn record_agent_bound(&self, line: AgentBoundLine) -> io::Result<()> {
        self.append(JournalLine::AgentBound(line)).await
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
        ensure_record_within_bounds(&json)?;
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

    /// Sync any buffered bytes to disk. Redundant with the per-line sync, but
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

fn ensure_record_within_bounds(record: &str) -> io::Result<()> {
    if record.len() > crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "workflow journal record exceeds the {}-byte cap",
                crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES
            ),
        ));
    }
    Ok(())
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
        JournalLine::AgentBound(bound) => {
            if bound.timestamp.is_none() {
                bound.timestamp = Some(host_timestamp());
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
async fn journal_writer(
    mut file: tokio::fs::File,
    mut journal_len: u64,
    mut journal_record_count: usize,
    mut rx: mpsc::Receiver<JournalCmd>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            JournalCmd::Append { line, ack } => {
                let result = write_line(
                    &mut file,
                    &mut journal_len,
                    &mut journal_record_count,
                    line.as_bytes(),
                )
                .await;
                let failed = result.is_err();
                let _ = ack.send(result);
                if failed {
                    break;
                }
            }
            JournalCmd::Flush { ack } => {
                let result = sync_file_data(&mut file).await;
                let failed = result.is_err();
                let _ = ack.send(result);
                if failed {
                    break;
                }
            }
            JournalCmd::Shutdown { ack } => {
                let _ = ack.send(sync_file_data(&mut file).await);
                break;
            }
        }
    }
}

/// Write one already-newline-terminated line and sync it to disk. Mirrors
/// `JsonlWriter::write_line` (`recorder.rs:1869`).
async fn write_line(
    file: &mut tokio::fs::File,
    journal_len: &mut u64,
    journal_record_count: &mut usize,
    bytes: &[u8],
) -> io::Result<()> {
    let next_record_count = journal_record_count
        .checked_add(1)
        .filter(|count| *count <= crate::WORKFLOW_JOURNAL_MAX_RECORDS)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "workflow journal exceeds the {}-record cap",
                    crate::WORKFLOW_JOURNAL_MAX_RECORDS
                ),
            )
        })?;
    let next_len = journal_len
        .checked_add(bytes.len() as u64)
        .filter(|len| *len <= crate::WORKFLOW_JOURNAL_MAX_BYTES)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "workflow journal exceeds the {}-byte run cap",
                    crate::WORKFLOW_JOURNAL_MAX_BYTES
                ),
            )
        })?;
    file.write_all(bytes).await?;
    sync_file_data(file).await?;
    *journal_len = next_len;
    *journal_record_count = next_record_count;
    Ok(())
}

/// Count and validate existing nonblank records before reopening the append writer.
///
/// The replay reader enforces the same record, per-line, and total-byte limits. Recounting on
/// reopen keeps crash recovery from bypassing the writer-side count admission without holding the
/// full journal in memory.
async fn count_journal_records(file: &mut tokio::fs::File) -> io::Result<usize> {
    file.seek(SeekFrom::Start(0)).await?;
    let mut chunk = [0u8; 8 * 1024];
    let mut count = 0usize;
    let mut record_len = 0usize;
    let mut record_nonblank = false;

    loop {
        let read = file.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        for byte in &chunk[..read] {
            if *byte == b'\n' {
                if record_nonblank {
                    count = count.checked_add(1).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "workflow journal record count overflow",
                        )
                    })?;
                    ensure_record_count_within_bounds(count)?;
                }
                record_len = 0;
                record_nonblank = false;
                continue;
            }

            record_len = record_len.checked_add(1).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "workflow journal record length overflow",
                )
            })?;
            if record_len > crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "workflow journal record exceeds the {}-byte cap",
                        crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES
                    ),
                ));
            }
            record_nonblank |= !byte.is_ascii_whitespace();
        }
    }

    if record_nonblank {
        count = count.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow journal record count overflow",
            )
        })?;
        ensure_record_count_within_bounds(count)?;
    }
    file.seek(SeekFrom::End(0)).await?;
    Ok(count)
}

fn ensure_record_count_within_bounds(count: usize) -> io::Result<()> {
    if count > crate::WORKFLOW_JOURNAL_MAX_RECORDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "workflow journal exceeds the {}-record cap",
                crate::WORKFLOW_JOURNAL_MAX_RECORDS
            ),
        ));
    }
    Ok(())
}

async fn sync_file_data(file: &mut tokio::fs::File) -> io::Result<()> {
    file.flush().await?;
    file.sync_data().await
}

/// Open the journal for append (creating it if missing) and repair any crash
/// tail. The file is opened with `O_APPEND`, so every write lands at the end
/// regardless of the seeks used while inspecting the tail.
async fn open_journal_for_append(path: &std::path::Path) -> io::Result<tokio::fs::File> {
    let file = crate::private_fs::open_private_for_append(path)?;
    let mut file = tokio::fs::File::from_std(file);
    repair_crash_tail(&mut file).await?;
    Ok(file)
}

/// Repair a final record without a newline before the writer starts appending.
///
/// A complete, structurally valid record is preserved and terminated. A suffix
/// that cannot parse as a journal record is a torn crash append and is removed
/// back to the preceding newline. Parsed metadata or structurally invalid body
/// records are corruption rather than partial appends and fail closed.
async fn repair_crash_tail(file: &mut tokio::fs::File) -> io::Result<()> {
    let len = file.metadata().await?.len();
    if len > crate::WORKFLOW_JOURNAL_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "workflow journal exceeds the {}-byte run cap",
                crate::WORKFLOW_JOURNAL_MAX_BYTES
            ),
        ));
    }
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1)).await?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last).await?;
    if last[0] == b'\n' {
        return Ok(());
    }

    let max_tail_bytes = crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES as u64 + 1;
    let tail_window_len = len.min(max_tail_bytes);
    let tail_window_start = len - tail_window_len;
    file.seek(SeekFrom::Start(tail_window_start)).await?;
    let mut tail_window = vec![0; tail_window_len as usize];
    file.read_exact(&mut tail_window).await?;
    let Some(relative_record_start) = tail_window.iter().rposition(|byte| *byte == b'\n') else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "workflow journal record exceeds the {}-byte cap",
                crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES
            ),
        ));
    };
    let record_start = tail_window_start + relative_record_start as u64 + 1;
    let record = &tail_window[relative_record_start + 1..];

    if let Ok(line) = serde_json::from_slice::<JournalLine>(record) {
        line.validate()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if len == crate::WORKFLOW_JOURNAL_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow journal has no room for crash-tail repair",
            ));
        }
        file.write_all(b"\n").await?;
    } else if serde_json::from_slice::<WorkflowRunMeta>(record).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workflow journal contains more than one run_meta record",
        ));
    } else {
        match serde_json::from_slice::<serde_json::Value>(record) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "workflow journal contains an unrecognized complete final record",
                ));
            }
            Err(error) if error.is_eof() => {
                file.set_len(record_start).await?;
                file.seek(SeekFrom::End(0)).await?;
            }
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "workflow journal contains a malformed final record",
                ));
            }
        }
    }
    sync_file_data(file).await?;
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
            Some(500_000),
            1,
            "2026-07-17T00:00:00Z".to_string(),
        )
    }

    fn completed_agent_call(ordinal: u64) -> AgentCallLine {
        AgentCallLine {
            timestamp: None,
            ordinal,
            attempt: 0,
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
            control_reason: None,
            ret: json!({"ok": true, "n": ordinal}),
            tokens_spent: Some(1000 + ordinal),
            progress: None,
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
    async fn reopening_rejects_a_journal_over_the_record_cap() {
        use std::io::Write;

        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        let meta = sample_meta(&run_id);
        JournalRecorder::new(&paths, &meta)
            .await
            .expect("create journal")
            .shutdown()
            .await
            .expect("shutdown journal");

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(paths.journal())
            .expect("open journal for raw append");
        let extra_records = "{}\n".repeat(crate::WORKFLOW_JOURNAL_MAX_RECORDS);
        file.write_all(extra_records.as_bytes())
            .expect("write over-cap record set");
        file.sync_all().expect("sync over-cap record set");

        let Err(error) = JournalRecorder::new(&paths, &meta).await else {
            panic!("over-cap journal must not reopen");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            error.to_string(),
            format!(
                "workflow journal exceeds the {}-record cap",
                crate::WORKFLOW_JOURNAL_MAX_RECORDS
            )
        );
    }

    #[tokio::test]
    async fn reopening_accepts_a_journal_at_the_record_cap() {
        use std::io::Write;

        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        let meta = sample_meta(&run_id);
        JournalRecorder::new(&paths, &meta)
            .await
            .expect("create journal")
            .shutdown()
            .await
            .expect("shutdown journal");

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(paths.journal())
            .expect("open journal for raw append");
        let extra_records = "{}\n".repeat(crate::WORKFLOW_JOURNAL_MAX_RECORDS - 1);
        file.write_all(extra_records.as_bytes())
            .expect("write records through cap");
        file.sync_all().expect("sync records through cap");

        JournalRecorder::new(&paths, &meta)
            .await
            .expect("journal at cap reopens")
            .shutdown()
            .await
            .expect("shutdown journal at cap");
    }

    #[tokio::test]
    async fn append_accepts_the_record_at_the_count_cap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("journal.jsonl");
        let mut file = tokio::fs::File::create(&path)
            .await
            .expect("create journal");
        let mut journal_len = 0;
        let mut record_count = crate::WORKFLOW_JOURNAL_MAX_RECORDS - 1;

        write_line(&mut file, &mut journal_len, &mut record_count, b"{}\n")
            .await
            .expect("record at cap must succeed");

        assert_eq!(journal_len, 3);
        assert_eq!(record_count, crate::WORKFLOW_JOURNAL_MAX_RECORDS);
        assert_eq!(file.metadata().await.expect("journal metadata").len(), 3);
    }

    #[tokio::test]
    async fn append_rejects_the_record_after_the_count_cap_without_writing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("journal.jsonl");
        let mut file = tokio::fs::File::create(&path)
            .await
            .expect("create journal");
        let mut journal_len = 0;
        let mut record_count = crate::WORKFLOW_JOURNAL_MAX_RECORDS;

        let error = write_line(&mut file, &mut journal_len, &mut record_count, b"{}\n")
            .await
            .expect_err("record past cap must fail");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(journal_len, 0);
        assert_eq!(record_count, crate::WORKFLOW_JOURNAL_MAX_RECORDS);
        assert_eq!(file.metadata().await.expect("journal metadata").len(), 0);
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
                JournalLine::AgentBound(bound) => bound.timestamp.is_some(),
                JournalLine::Phase(p) => p.timestamp.is_some(),
                JournalLine::Log(l) => l.timestamp.is_some(),
            };
            assert!(has_ts, "recorder stamps a host timestamp");
        }
    }

    #[tokio::test]
    async fn agent_binding_is_flushed_before_a_terminal_call() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        let recorder = JournalRecorder::new(&paths, &sample_meta(&run_id))
            .await
            .expect("new");
        let child_thread_id = uuid::Uuid::now_v7().to_string();
        let rollout_path = tmp.path().join("rollout.jsonl");

        recorder
            .record_agent_bound(AgentBoundLine {
                timestamp: None,
                ordinal: 0,
                attempt: 0,
                child_thread_id: child_thread_id.clone(),
                rollout_path: rollout_path.display().to_string(),
            })
            .await
            .expect("binding append");
        let mut completed = completed_agent_call(0);
        completed.child_thread_id = Some(child_thread_id);
        completed.rollout_path = Some(rollout_path.display().to_string());
        recorder
            .record_agent_call(completed)
            .await
            .expect("terminal append");
        recorder.shutdown().await.expect("shutdown");

        let (_, lines) = read_journal(&paths.journal());
        assert!(matches!(lines[0], JournalLine::AgentBound(_)));
        assert!(matches!(lines[1], JournalLine::AgentCall(_)));
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
    async fn append_rejects_an_oversized_serialized_record_without_writing_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        let recorder = JournalRecorder::new(&paths, &sample_meta(&run_id))
            .await
            .expect("new");

        let before = std::fs::metadata(paths.journal()).expect("metadata").len();
        let error = recorder
            .record_log(LogLine {
                timestamp: None,
                ordinal: NullOrdinal,
                message: "x".repeat(crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES),
            })
            .await
            .expect_err("oversized record must reject");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::metadata(paths.journal()).expect("metadata").len(),
            before
        );
        recorder.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn reopening_rejects_a_journal_over_the_run_cap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        paths.create_dir().expect("mkdir");
        let file = std::fs::File::create(paths.journal()).expect("create sparse journal");
        file.set_len(crate::WORKFLOW_JOURNAL_MAX_BYTES + 1)
            .expect("set sparse length");

        let error = JournalRecorder::new(&paths, &sample_meta(&run_id))
            .await
            .err()
            .expect("oversized journal must reject");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
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
        // The append acknowledgement itself is the durability boundary; no
        // explicit flush or shutdown is needed before the complete line is visible.
        let (_, appended) = read_journal(&paths.journal());
        assert_eq!(appended.len(), 1);
        recorder.flush().await.expect("flush");

        // The explicit sync barrier preserves the same complete journal.
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

        // Reopening must remove the incomplete record before appending, so it
        // cannot be promoted into durable newline-terminated corruption.
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
        assert_eq!(text.lines().count(), 2, "the torn record was truncated");
        assert!(!text.contains("torn"));
    }

    #[tokio::test]
    async fn reopening_preserves_a_complete_record_missing_only_its_newline() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        paths.create_dir().expect("mkdir");
        let meta = sample_meta(&run_id);
        let final_line = JournalLine::Log(LogLine {
            timestamp: None,
            ordinal: NullOrdinal,
            message: "complete before crash".to_string(),
        });
        let seed = format!(
            "{}\n{}",
            serde_json::to_string(&meta).expect("meta"),
            serde_json::to_string(&final_line).expect("final line"),
        );
        std::fs::write(paths.journal(), seed).expect("seed");

        let recorder = JournalRecorder::new(&paths, &meta).await.expect("reopen");
        recorder.shutdown().await.expect("shutdown");

        let (_, lines) = read_journal(&paths.journal());
        assert_eq!(lines, vec![final_line]);
    }

    #[tokio::test]
    async fn reopening_rejects_complete_unrecognized_final_json_without_modifying_it() {
        for tail in [r#"{"type":"future_record"}"#, r#"{"type":"log"}"#] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let run_id = mint_run_id();
            let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
            paths.create_dir().expect("mkdir");
            let meta = sample_meta(&run_id);
            let seed = format!(
                "{}\n{tail}",
                serde_json::to_string(&meta).expect("serialize metadata")
            );
            std::fs::write(paths.journal(), &seed).expect("seed journal");

            let Err(error) = JournalRecorder::new(&paths, &meta).await else {
                panic!("complete unknown final record must not be truncated");
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("complete final record"));
            assert_eq!(
                std::fs::read(paths.journal()).expect("read unchanged journal"),
                seed.as_bytes()
            );
        }
    }

    #[tokio::test]
    async fn reopening_rejects_non_eof_invalid_final_json_without_modifying_it() {
        for tail in ["garbage", "{]"] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let run_id = mint_run_id();
            let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
            paths.create_dir().expect("mkdir");
            let meta = sample_meta(&run_id);
            let seed = format!(
                "{}\n{tail}",
                serde_json::to_string(&meta).expect("serialize metadata")
            );
            std::fs::write(paths.journal(), &seed).expect("seed journal");

            let Err(error) = JournalRecorder::new(&paths, &meta).await else {
                panic!("malformed final record must not be truncated");
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("malformed final record"));
            assert_eq!(
                std::fs::read(paths.journal()).expect("read unchanged journal"),
                seed.as_bytes()
            );
        }
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
