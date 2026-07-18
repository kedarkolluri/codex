//! Tail-first prefix read of a prior run's `journal.jsonl` for resume.
//!
//! This is the **pure read/validation half** of resume (spec
//! `docs/dynamic-workflows-spec.md` §7, "Resume algorithm step 1"). The live
//! replay branch — the `agent_callback` prefix loop that resolves promises from
//! cache, re-adds budget, and flips to live at the first divergence — is a
//! separate ticket (`P3-resume-prefix-loop`); this module only *loads* the prior
//! journal and *validates* it.
//!
//! ## Why tail-first
//!
//! We clone the rollout recorder's proven append-only JSONL machinery
//! (`rollout/src/reverse_jsonl_scanner.rs`, mirrored here as
//! `ReverseJsonlScanner`) rather than depending on it — that scanner is
//! `pub(crate)` in `codex-rollout` and this crate is deliberately standalone
//! (see the crate-level docs). Reading the file backward chunk-by-chunk means a
//! **partially-written / truncated tail** (a crash mid-append leaves a final line
//! with no terminating newline and often invalid JSON) is the *first* record we
//! see: it fails to parse, is dropped, and the scanner keeps going. The intact
//! prefix behind it loads normally.
//!
//! ## Reconstruction is by ordinal, never by physical order
//!
//! The recorder appends an `agent_call` line when the call *resolves*, so for a
//! `parallel`/`pipeline` batch the physical journal order is **completion order**,
//! not invocation order (that is what `completion_seq` records). Prefix-replay
//! keys strictly on the invocation **ordinal** (§7, "We key cache on invocation
//! ordinal, never completion order"). [`ReplayJournal::load`] therefore places
//! each `agent_call` at `entries[ordinal]` regardless of the order the lines were
//! read, so `entries[i].ordinal == i` holds deterministically for any physical
//! interleaving.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;

use serde_json::Value;

use crate::AgentCallLine;
use crate::AgentStatus;
use crate::JournalLine;
use crate::WorkflowRunMeta;

/// A prior run's journal loaded for prefix-replay.
///
/// Construct with [`ReplayJournal::load`] (or [`ReplayJournal::from_reader`] in
/// tests). Holds the validated [`WorkflowRunMeta`] (line 0) plus the
/// ordinal-indexed `agent_call` entries; `phase`/`log` narration lines carry no
/// ordinal and are not needed by replay, so they are dropped on load.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayJournal {
    run_meta: WorkflowRunMeta,
    /// `agent_call` lines indexed by ordinal: `entries[i].ordinal == i` for all
    /// `i in 0..M`. Always a contiguous prefix from ordinal 0 (see
    /// [`ReplayJournal::load`] for how a gap is handled).
    entries: Vec<AgentCallLine>,
}

/// The recorded facts the resume loop reads for one replayed ordinal.
///
/// Borrows from the owning [`ReplayJournal`]. Exactly the §7 "Resume algorithm
/// step 3" inputs: the cache `key` to compare against the recomputed one, the
/// `status` (only `completed` is served from cache), the `return` value to
/// resolve the promise with, and `tokens_spent` to re-add to the budget counter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReplayEntry<'a> {
    /// The invocation ordinal this entry was recorded at (equals its index).
    pub ordinal: u64,
    /// The journaled `(prompt, opts)` cache key (e.g. `"blake3:..."`).
    pub key: &'a str,
    /// Completion status; `None` while in-flight/unknown.
    pub status: Option<AgentStatus>,
    /// The recorded return value (string, object, or `null`).
    pub ret: &'a Value,
    /// Tokens the agent spent, re-added to the budget on a cache hit.
    pub tokens_spent: Option<u64>,
}

/// Why a resumed run is structurally incompatible with the prior journal.
///
/// Any of these means the current run's shape differs from the recorded one, so
/// replay must **diverge at ordinal 0** and run entirely live (§7: "a structural
/// change simply produces early divergence — it is not a hard error"). This is a
/// signal for the caller, never a panic or load failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Divergence {
    /// The executed script changed (`run_meta.script_hash` differs).
    ScriptHash,
    /// The run arguments changed (`run_meta.args_hash` differs).
    ArgsHash,
    /// The cache-key algorithm changed across Codex versions
    /// (`run_meta.key_algo_version` differs), so every recomputed key would
    /// mismatch anyway.
    KeyAlgoVersion,
}

impl ReplayJournal {
    /// Load and validate a prior run's `journal.jsonl` from `path`.
    ///
    /// Convenience wrapper over [`ReplayJournal::from_reader`] that opens the
    /// file. See that method for the parsing/reconstruction contract.
    pub fn load(path: &Path) -> io::Result<Self> {
        Self::from_reader(File::open(path)?)
    }

    /// Load and validate a prior journal from any seekable reader.
    ///
    /// Reads tail-first via a reverse JSONL scanner. Records that fail to parse
    /// (a truncated/garbage tail line) are silently skipped so a crash-truncated
    /// journal still loads its intact prefix. `agent_call` lines are placed by
    /// ordinal into a contiguous `entries[0..M]`; `phase`/`log` lines are
    /// dropped. Fails only if the run-meta (line 0) is absent or unparseable —
    /// without it there is nothing to validate a resume against.
    ///
    /// If ordinals are non-contiguous (a gap, e.g. from a lost middle line),
    /// only the contiguous prefix from ordinal 0 is retained: replay can only
    /// ever serve an unbroken prefix, so a hole caps `M` at the hole. Duplicate
    /// ordinals keep the most-recently-appended line.
    pub fn from_reader<R: Read + Seek>(reader: R) -> io::Result<Self> {
        let mut scanner = ReverseJsonlScanner::new(reader)?;

        let mut run_meta: Option<WorkflowRunMeta> = None;
        // ordinal -> line. BTreeMap keeps a deterministic, sorted view so the
        // contiguous-prefix walk below is independent of read order.
        let mut by_ordinal: BTreeMap<u64, AgentCallLine> = BTreeMap::new();

        while let Some(line) = scanner.scan_next()? {
            // Try the body-line shape first: `JournalLine` is internally tagged
            // on `type` and covers agent_call/phase/log. The run-meta (line 0)
            // has `type:"run_meta"`, which is not a `JournalLine` variant, so it
            // falls through to the second parse. A truncated/garbage line fails
            // both and is skipped, leaving the intact prefix behind it usable.
            //
            // We deliberately do NOT fold run_meta into a single internally
            // tagged enum: `WorkflowRunMeta` carries its own `type` field (the
            // `kind` discriminant), and serde's internal tagging strips the tag
            // from the content before handing it to the variant, which would make
            // that field fail to deserialize.
            if let Ok(journal_line) = serde_json::from_slice::<JournalLine>(&line) {
                match journal_line {
                    JournalLine::AgentCall(call) => {
                        // Reading backward, `or_insert` keeps the most recently
                        // appended occurrence of a duplicated ordinal.
                        by_ordinal.entry(call.ordinal).or_insert(*call);
                    }
                    // Narration lines carry no ordinal and are not replayed.
                    JournalLine::Phase(_) | JournalLine::Log(_) => {}
                }
            } else if let Ok(meta) = serde_json::from_slice::<WorkflowRunMeta>(&line) {
                // The head line is read last; there is exactly one.
                run_meta = Some(meta);
            }
            // else: malformed/truncated line — skip and keep scanning.
        }

        let run_meta = run_meta.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow journal is missing its run_meta (line 0)",
            )
        })?;

        // Walk ordinals from 0 up, stopping at the first gap. This yields a
        // contiguous prefix in strict ordinal order regardless of the physical
        // (completion-order) layout on disk.
        let mut entries = Vec::new();
        let mut next: u64 = 0;
        while let Some(line) = by_ordinal.remove(&next) {
            entries.push(line);
            next = match next.checked_add(1) {
                Some(n) => n,
                None => break,
            };
        }

        Ok(Self { run_meta, entries })
    }

    /// The validated run-level metadata (line 0 of the journal).
    pub fn run_meta(&self) -> &WorkflowRunMeta {
        &self.run_meta
    }

    /// The number of contiguously-recorded `agent_call` ordinals, `M`.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the journal recorded zero `agent_call` ordinals.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The ordinal-indexed entries, `entries[i].ordinal == i`.
    pub fn entries(&self) -> &[AgentCallLine] {
        &self.entries
    }

    /// Look up the recorded facts for invocation `ordinal`, or `None` if the
    /// ordinal is at/after the end of the recorded prefix (`ordinal >= M`).
    ///
    /// Returns the recorded `key`, `status`, `return`, and `tokens_spent` — the
    /// exact inputs the resume loop needs to decide a cache hit and re-add
    /// budget (§7 step 3).
    pub fn lookup(&self, ordinal: u64) -> Option<ReplayEntry<'_>> {
        let idx = usize::try_from(ordinal).ok()?;
        let line = self.entries.get(idx)?;
        Some(ReplayEntry {
            ordinal: line.ordinal,
            key: &line.key,
            status: line.status,
            ret: &line.ret,
            tokens_spent: line.tokens_spent,
        })
    }

    /// Check the resumed run's shape against the recorded one.
    ///
    /// Returns `None` when the run is compatible (replay may proceed), or
    /// `Some(reason)` for the first structural mismatch — a **divergence signal**,
    /// not an error: on `Some`, the caller diverges at ordinal 0 and runs the
    /// whole body live (§7). Checked in priority order: key-algo version (a bump
    /// invalidates every recomputed key), then script hash, then args hash.
    pub fn check_compatibility(
        &self,
        script_hash: &str,
        args_hash: &str,
        key_algo_version: u32,
    ) -> Option<Divergence> {
        if self.run_meta.key_algo_version != key_algo_version {
            return Some(Divergence::KeyAlgoVersion);
        }
        if self.run_meta.script_hash != script_hash {
            return Some(Divergence::ScriptHash);
        }
        if self.run_meta.args_hash != args_hash {
            return Some(Divergence::ArgsHash);
        }
        None
    }

    /// Convenience predicate over [`check_compatibility`](Self::check_compatibility):
    /// `true` when the resumed run may replay this journal.
    pub fn is_compatible(&self, script_hash: &str, args_hash: &str, key_algo_version: u32) -> bool {
        self.check_compatibility(script_hash, args_hash, key_algo_version)
            .is_none()
    }
}

// --- Reverse JSONL scanner (cloned from rollout/src/reverse_jsonl_scanner.rs) ---
//
// `codex-rollout`'s scanner is `pub(crate)`, and this crate is deliberately
// standalone (crate docs), so the tail-first byte machinery is mirrored here.
// Kept private: only `ReplayJournal::from_reader` consumes it. Unlike the
// rollout original it yields the raw record bytes rather than a parsed value —
// the loader must attempt two distinct types per line (a body `JournalLine` or
// the head `WorkflowRunMeta`), so parsing stays with the caller and a line that
// parses as neither is simply dropped.

const READ_CHUNK_SIZE: usize = 8 * 1024;

/// Read-only scanner for newline-delimited records, starting from the end.
struct ReverseJsonlScanner<R> {
    reader: R,
    next_chunk_end: u64,
    chunk_position: usize,
    chunk: Vec<u8>,
    record_reversed: Vec<u8>,
}

impl<R> ReverseJsonlScanner<R>
where
    R: Read + Seek,
{
    fn new(mut reader: R) -> io::Result<Self> {
        let next_chunk_end = reader.seek(SeekFrom::End(0))?;
        Ok(Self {
            reader,
            next_chunk_end,
            chunk_position: 0,
            chunk: vec![0; READ_CHUNK_SIZE],
            record_reversed: Vec::new(),
        })
    }

    /// Returns the next nonblank record's raw bytes (tail-first), or `None` at
    /// the start of the file. Blank/whitespace-only records are skipped. I/O
    /// failures surface as [`Err`].
    fn scan_next(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            let Some(byte) = self.read_previous_byte()? else {
                return Ok(self.finish_record());
            };

            if byte != b'\n' {
                self.record_reversed.push(byte);
                continue;
            }

            if let Some(record) = self.finish_record() {
                return Ok(Some(record));
            }
        }
    }

    fn read_previous_byte(&mut self) -> io::Result<Option<u8>> {
        if self.chunk_position == 0 {
            if self.next_chunk_end == 0 {
                return Ok(None);
            }

            let read_size = usize::try_from(self.next_chunk_end.min(READ_CHUNK_SIZE as u64))
                .map_err(io::Error::other)?;
            self.next_chunk_end -= read_size as u64;
            self.reader.seek(SeekFrom::Start(self.next_chunk_end))?;
            self.reader.read_exact(&mut self.chunk[..read_size])?;
            self.chunk_position = read_size;
        }

        self.chunk_position -= 1;
        Ok(Some(self.chunk[self.chunk_position]))
    }

    fn finish_record(&mut self) -> Option<Vec<u8>> {
        self.record_reversed.reverse();
        let record = if self.record_reversed.iter().all(u8::is_ascii_whitespace) {
            None
        } else {
            Some(self.record_reversed.clone())
        };
        self.record_reversed.clear();
        record
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentCallOpts;
    use crate::KEY_ALGO_VERSION;
    use crate::LogLine;
    use crate::NullOrdinal;
    use crate::PhaseLine;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::io::Cursor;

    const SCRIPT_HASH: &str = "blake3:script";
    const ARGS_HASH: &str = "blake3:args";

    fn run_meta() -> WorkflowRunMeta {
        WorkflowRunMeta::new(
            "run_1".to_string(),
            None,
            SCRIPT_HASH.to_string(),
            ARGS_HASH.to_string(),
            "triage".to_string(),
            500_000,
            KEY_ALGO_VERSION,
            "2026-07-17T00:00:00Z".to_string(),
        )
    }

    fn agent_call(ordinal: u64) -> AgentCallLine {
        AgentCallLine {
            timestamp: None,
            ordinal,
            key: format!("blake3:key-{ordinal}"),
            prompt_hash: format!("blake3:ph-{ordinal}"),
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
            rollout_path: Some(format!("/p/rollout-{ordinal}.jsonl")),
            status: Some(AgentStatus::Completed),
            ret: json!({ "ordinal": ordinal }),
            tokens_spent: Some(1000 + ordinal),
            completion_seq: Some(ordinal),
        }
    }

    /// Serialize a run_meta + body lines into a `journal.jsonl` byte buffer, one
    /// JSON object per line, newline-terminated (the recorder's append
    /// discipline). Body lines are emitted in the given physical order.
    fn journal_bytes(meta: &WorkflowRunMeta, lines: &[JournalLine]) -> Vec<u8> {
        let mut out = serde_json::to_string(meta).expect("serialize meta");
        out.push('\n');
        for line in lines {
            out.push_str(&serde_json::to_string(line).expect("serialize line"));
            out.push('\n');
        }
        out.into_bytes()
    }

    fn load(bytes: Vec<u8>) -> ReplayJournal {
        ReplayJournal::from_reader(Cursor::new(bytes)).expect("load journal")
    }

    #[test]
    fn loads_m_entries_in_ordinal_order_with_run_meta() {
        let meta = run_meta();
        let lines: Vec<JournalLine> = (0..5)
            .map(|i| JournalLine::AgentCall(Box::new(agent_call(i))))
            .collect();
        let journal = load(journal_bytes(&meta, &lines));

        assert_eq!(journal.run_meta(), &meta);
        assert_eq!(journal.len(), 5);
        assert!(!journal.is_empty());
        for (i, entry) in journal.entries().iter().enumerate() {
            assert_eq!(entry.ordinal, i as u64, "entries[i].ordinal == i");
        }
    }

    #[test]
    fn prefix_read_reconstructs_ordinals_in_order_despite_shuffled_physical_layout() {
        // Physical journal order = completion order for a parallel batch, which
        // is NOT invocation order. Write the lines shuffled and interleave
        // narration; reconstruction must still be by ordinal.
        let meta = run_meta();
        let physical_order = [3u64, 0, 4, 2, 1];
        let mut lines: Vec<JournalLine> = Vec::new();
        for &ord in &physical_order {
            lines.push(JournalLine::Phase(PhaseLine {
                timestamp: None,
                ordinal: NullOrdinal,
                title: format!("phase-before-{ord}"),
            }));
            lines.push(JournalLine::AgentCall(Box::new(agent_call(ord))));
            lines.push(JournalLine::Log(LogLine {
                timestamp: None,
                ordinal: NullOrdinal,
                message: format!("log-after-{ord}"),
            }));
        }
        let journal = load(journal_bytes(&meta, &lines));

        assert_eq!(journal.len(), 5);
        let reconstructed: Vec<u64> = journal.entries().iter().map(|e| e.ordinal).collect();
        assert_eq!(
            reconstructed,
            vec![0, 1, 2, 3, 4],
            "entries reconstruct in ordinal order regardless of physical order",
        );
    }

    #[test]
    fn lookup_returns_recorded_key_status_return_tokens() {
        let meta = run_meta();
        let mut call = agent_call(2);
        call.key = "blake3:specific".to_string();
        call.status = Some(AgentStatus::Completed);
        call.ret = json!({ "verdict": "approve" });
        call.tokens_spent = Some(4242);
        let lines: Vec<JournalLine> = (0..3)
            .map(|i| {
                if i == 2 {
                    JournalLine::AgentCall(Box::new(call.clone()))
                } else {
                    JournalLine::AgentCall(Box::new(agent_call(i)))
                }
            })
            .collect();
        let journal = load(journal_bytes(&meta, &lines));

        let entry = journal.lookup(2).expect("ordinal 2 present");
        assert_eq!(entry.ordinal, 2);
        assert_eq!(entry.key, "blake3:specific");
        assert_eq!(entry.status, Some(AgentStatus::Completed));
        assert_eq!(entry.ret, &json!({ "verdict": "approve" }));
        assert_eq!(entry.tokens_spent, Some(4242));

        // Past the end of the recorded prefix -> None.
        assert!(journal.lookup(3).is_none());
        assert!(journal.lookup(u64::MAX).is_none());
    }

    #[test]
    fn lookup_surfaces_error_and_in_flight_status() {
        let meta = run_meta();
        let mut errored = agent_call(0);
        errored.status = Some(AgentStatus::Error);
        errored.ret = Value::Null;
        errored.tokens_spent = None;
        let mut in_flight = agent_call(1);
        in_flight.status = None;
        in_flight.ret = Value::Null;
        in_flight.tokens_spent = None;
        let journal = load(journal_bytes(
            &meta,
            &[
                JournalLine::AgentCall(Box::new(errored)),
                JournalLine::AgentCall(Box::new(in_flight)),
            ],
        ));

        assert_eq!(journal.lookup(0).unwrap().status, Some(AgentStatus::Error));
        assert_eq!(journal.lookup(0).unwrap().tokens_spent, None);
        assert_eq!(journal.lookup(1).unwrap().status, None);
    }

    #[test]
    fn compatible_journal_produces_no_divergence() {
        let journal = load(journal_bytes(&run_meta(), &[]));
        assert_eq!(
            journal.check_compatibility(SCRIPT_HASH, ARGS_HASH, KEY_ALGO_VERSION),
            None,
        );
        assert!(journal.is_compatible(SCRIPT_HASH, ARGS_HASH, KEY_ALGO_VERSION));
    }

    #[test]
    fn script_hash_mismatch_is_a_divergence_not_a_panic() {
        let journal = load(journal_bytes(&run_meta(), &[]));
        assert_eq!(
            journal.check_compatibility("blake3:OTHER", ARGS_HASH, KEY_ALGO_VERSION),
            Some(Divergence::ScriptHash),
        );
        assert!(!journal.is_compatible("blake3:OTHER", ARGS_HASH, KEY_ALGO_VERSION));
    }

    #[test]
    fn args_hash_mismatch_is_a_divergence() {
        let journal = load(journal_bytes(&run_meta(), &[]));
        assert_eq!(
            journal.check_compatibility(SCRIPT_HASH, "blake3:OTHER", KEY_ALGO_VERSION),
            Some(Divergence::ArgsHash),
        );
    }

    #[test]
    fn key_algo_version_mismatch_is_a_divergence() {
        let journal = load(journal_bytes(&run_meta(), &[]));
        assert_eq!(
            journal.check_compatibility(SCRIPT_HASH, ARGS_HASH, KEY_ALGO_VERSION + 1),
            Some(Divergence::KeyAlgoVersion),
        );
    }

    #[test]
    fn key_algo_version_takes_priority_when_multiple_fields_differ() {
        let journal = load(journal_bytes(&run_meta(), &[]));
        // All three differ; key-algo version is reported first.
        assert_eq!(
            journal.check_compatibility("blake3:X", "blake3:Y", KEY_ALGO_VERSION + 1),
            Some(Divergence::KeyAlgoVersion),
        );
    }

    #[test]
    fn truncated_tail_line_is_ignored_gracefully() {
        let meta = run_meta();
        let lines: Vec<JournalLine> = (0..3)
            .map(|i| JournalLine::AgentCall(Box::new(agent_call(i))))
            .collect();
        let mut bytes = journal_bytes(&meta, &lines);

        // Simulate a crash mid-append: a partial final line with no trailing
        // newline and invalid JSON.
        bytes.extend_from_slice(br#"{"type":"agent_call","ordinal":3,"key":"blake3:par"#);
        let journal = load(bytes);

        // The intact prefix (ordinals 0..2) loads; the torn line is dropped.
        assert_eq!(journal.len(), 3);
        assert_eq!(journal.run_meta(), &meta);
        assert!(journal.lookup(2).is_some());
        assert!(journal.lookup(3).is_none());
    }

    #[test]
    fn ordinal_gap_caps_the_prefix() {
        // A lost middle line (ordinal 2 missing) leaves 3 and 4 unreachable:
        // replay can only serve a contiguous prefix, so M caps at the gap.
        let meta = run_meta();
        let lines: Vec<JournalLine> = [0u64, 1, 3, 4]
            .into_iter()
            .map(|i| JournalLine::AgentCall(Box::new(agent_call(i))))
            .collect();
        let journal = load(journal_bytes(&meta, &lines));

        assert_eq!(
            journal.len(),
            2,
            "prefix stops at the first missing ordinal"
        );
        assert!(journal.lookup(1).is_some());
        assert!(journal.lookup(2).is_none());
    }

    #[test]
    fn missing_run_meta_is_a_load_error() {
        // Body lines but no run_meta head -> nothing to validate against.
        let bytes = {
            let mut out = String::new();
            let line = JournalLine::AgentCall(Box::new(agent_call(0)));
            out.push_str(&serde_json::to_string(&line).unwrap());
            out.push('\n');
            out.into_bytes()
        };
        let err = ReplayJournal::from_reader(Cursor::new(bytes))
            .expect_err("missing run_meta must error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn empty_journal_is_a_load_error() {
        let err =
            ReplayJournal::from_reader(Cursor::new(Vec::new())).expect_err("empty file must error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn run_meta_only_journal_loads_with_zero_entries() {
        let journal = load(journal_bytes(&run_meta(), &[]));
        assert_eq!(journal.len(), 0);
        assert!(journal.is_empty());
        assert!(journal.lookup(0).is_none());
    }

    #[test]
    fn missing_final_newline_still_parses_last_line() {
        // A well-formed journal whose last line simply lacks a trailing newline
        // (valid JSON, just no `\n`) must still parse fully.
        let meta = run_meta();
        let lines: Vec<JournalLine> = (0..2)
            .map(|i| JournalLine::AgentCall(Box::new(agent_call(i))))
            .collect();
        let mut bytes = journal_bytes(&meta, &lines);
        assert_eq!(bytes.pop(), Some(b'\n'), "drop the trailing newline");
        let journal = load(bytes);
        assert_eq!(journal.len(), 2);
    }

    #[test]
    fn load_from_file_round_trips() {
        let meta = run_meta();
        let lines: Vec<JournalLine> = (0..4)
            .map(|i| JournalLine::AgentCall(Box::new(agent_call(i))))
            .collect();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.jsonl");
        std::fs::write(&path, journal_bytes(&meta, &lines)).expect("write journal");

        let journal = ReplayJournal::load(&path).expect("load from file");
        assert_eq!(journal.len(), 4);
        assert_eq!(journal.run_meta(), &meta);
    }

    /// Property/fuzz test (§14.1 Layer 1): over many pseudo-random
    /// parallel/pipeline shapes — random `M`, random physical write order, random
    /// narration interleaving — the entries always reconstruct in strict ordinal
    /// order `0..M`. Uses a deterministic splitmix64 so the test itself is
    /// reproducible (fitting for a determinism crate).
    #[test]
    fn property_entries_reconstruct_in_deterministic_ordinal_order() {
        // Deterministic PRNG — no wall-clock, no std::random.
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };

        for _ in 0..200 {
            let m = next() % 24; // 0..=23 invocation ordinals
            let mut ordinals: Vec<u64> = (0..m).collect();
            // Fisher-Yates shuffle into a random physical (completion) order.
            for i in (1..ordinals.len()).rev() {
                let j = (next() as usize) % (i + 1);
                ordinals.swap(i, j);
            }

            let meta = run_meta();
            let mut lines: Vec<JournalLine> = Vec::new();
            for &ord in &ordinals {
                // Randomly interleave narration lines (no ordinal).
                if next() % 2 == 0 {
                    lines.push(JournalLine::Phase(PhaseLine {
                        timestamp: None,
                        ordinal: NullOrdinal,
                        title: format!("p{ord}"),
                    }));
                }
                lines.push(JournalLine::AgentCall(Box::new(agent_call(ord))));
                if next() % 2 == 0 {
                    lines.push(JournalLine::Log(LogLine {
                        timestamp: None,
                        ordinal: NullOrdinal,
                        message: format!("l{ord}"),
                    }));
                }
            }

            let journal = load(journal_bytes(&meta, &lines));
            assert_eq!(journal.len(), m as usize, "M entries recovered");
            let got: Vec<u64> = journal.entries().iter().map(|e| e.ordinal).collect();
            let want: Vec<u64> = (0..m).collect();
            assert_eq!(got, want, "reconstructed in strict ordinal order");
            // Lookup agrees with the ordinal-indexed vector at every position.
            for ord in 0..m {
                assert_eq!(journal.lookup(ord).unwrap().ordinal, ord);
            }
        }
    }
}
