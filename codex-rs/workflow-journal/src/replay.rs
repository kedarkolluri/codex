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
//! see: it fails to parse, is dropped, and the scanner keeps going. Framing is
//! retained for every scanned record, so malformed newline-terminated records
//! remain hard errors rather than being mistaken for crash tails. The intact
//! prefix behind a genuine unterminated tail loads normally.
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
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde::de::IgnoredAny;
use serde_json::Value;

use crate::AgentCallLine;
use crate::AgentCallOpts;
use crate::AgentCallProgress;
use crate::AgentControlReason;
use crate::AgentStatus;
use crate::JournalLine;
use crate::WorkflowRunMeta;
use crate::ensure_workflow_agent_return;

const MAX_RUN_AGENT_LINKS: usize = 10_000;
const LEGACY_AGENT_CALL_RETURN_FIELD: &[u8] = b",\"return\":";
const LEGACY_PHASE_TITLE_FIELD: &[u8] = b"\"ordinal\":null,\"title\":\"";
const LEGACY_LOG_MESSAGE_FIELD: &[u8] = b"\"ordinal\":null,\"message\":\"";

#[derive(Debug, Deserialize)]
struct OversizedLegacyAgentLink {
    #[serde(rename = "type")]
    kind: OversizedLegacyKind,
    ordinal: u64,
    #[serde(default)]
    attempt: u32,
    child_thread_id: Option<String>,
    rollout_path: Option<String>,
}

#[derive(Debug)]
struct IgnoredLegacyReturn {
    is_null: bool,
}

#[derive(Debug)]
struct IgnoredString;

impl<'de> Deserialize<'de> for IgnoredString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct IgnoredStringVisitor;

        impl<'de> serde::de::Visitor<'de> for IgnoredStringVisitor {
            type Value = IgnoredString;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a string")
            }

            fn visit_borrowed_str<E>(self, _value: &'de str) -> Result<Self::Value, E> {
                Ok(IgnoredString)
            }

            fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
                Ok(IgnoredString)
            }

            fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
                Ok(IgnoredString)
            }
        }

        deserializer.deserialize_str(IgnoredStringVisitor)
    }
}

impl<'de> Deserialize<'de> for IgnoredLegacyReturn {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self {
            is_null: Option::<IgnoredAny>::deserialize(deserializer)?.is_none(),
        })
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OversizedLegacyKind {
    AgentCall,
    Phase,
    Log,
}

#[derive(Debug, Deserialize)]
struct OversizedLegacyAgentCallOpts {
    model: Option<IgnoredString>,
    effort: Option<IgnoredString>,
    #[serde(rename = "agentType")]
    agent_type: Option<IgnoredString>,
    isolation: Option<IgnoredString>,
    schema_hash: Option<IgnoredString>,
}

/// The complete legacy `agent_call` schema with the return value streamed past.
///
/// The initial journal writer had no record-size cap, so a return can be much
/// larger than the current replay limit. Every other field retains the same
/// serde type and defaults as [`AgentCallLine`], while `return` records only
/// nullness. Nullness is the sole property of the return needed by
/// [`AgentCallLine::validate_structure`].
#[derive(Debug, Deserialize)]
struct OversizedLegacyAgentCall {
    #[serde(rename = "type")]
    kind: OversizedLegacyKind,
    #[serde(default)]
    timestamp: Option<IgnoredString>,
    ordinal: u64,
    #[serde(default)]
    attempt: u32,
    key: IgnoredString,
    prompt_hash: IgnoredString,
    opts: OversizedLegacyAgentCallOpts,
    phase: Option<IgnoredString>,
    label: Option<IgnoredString>,
    child_thread_id: Option<IgnoredString>,
    rollout_path: Option<IgnoredString>,
    status: Option<AgentStatus>,
    #[serde(default)]
    control_reason: Option<AgentControlReason>,
    #[serde(rename = "return")]
    ret: IgnoredLegacyReturn,
    tokens_spent: Option<u64>,
    #[serde(default)]
    progress: Option<AgentCallProgress>,
    completion_seq: Option<u64>,
}

impl OversizedLegacyAgentCall {
    fn validate_and_link(
        self,
        link: OversizedLegacyAgentLink,
    ) -> io::Result<OversizedLegacyAgentLink> {
        if self.kind != OversizedLegacyKind::AgentCall {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow journal oversized legacy record changed type while parsing",
            ));
        }
        if link.kind != OversizedLegacyKind::AgentCall
            || (self.ordinal, self.attempt) != (link.ordinal, link.attempt)
            || self.child_thread_id.is_some() != link.child_thread_id.is_some()
            || self.rollout_path.is_some() != link.rollout_path.is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow journal oversized legacy linkage changed while parsing",
            ));
        }
        let _ = (
            self.timestamp,
            self.key,
            self.prompt_hash,
            self.opts.model,
            self.opts.effort,
            self.opts.agent_type,
            self.opts.isolation,
            self.opts.schema_hash,
            self.phase,
            self.label,
        );
        let call = AgentCallLine {
            timestamp: None,
            ordinal: self.ordinal,
            attempt: self.attempt,
            key: String::new(),
            prompt_hash: String::new(),
            opts: AgentCallOpts {
                model: None,
                effort: None,
                agent_type: None,
                isolation: None,
                schema_hash: None,
            },
            phase: None,
            label: None,
            child_thread_id: self.child_thread_id.map(|_| String::new()),
            rollout_path: self.rollout_path.map(|_| String::new()),
            status: self.status,
            control_reason: self.control_reason,
            ret: if self.ret.is_null {
                Value::Null
            } else {
                Value::Bool(true)
            },
            tokens_spent: self.tokens_spent,
            progress: self.progress,
            completion_seq: self.completion_seq,
        };
        call.validate_structure()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(link)
    }
}

#[derive(Debug, Deserialize)]
struct OversizedLegacyPhase {
    #[serde(rename = "type")]
    kind: OversizedLegacyKind,
    #[serde(default)]
    timestamp: Option<IgnoredString>,
    ordinal: crate::NullOrdinal,
    title: IgnoredAny,
}

#[derive(Debug, Deserialize)]
struct OversizedLegacyLog {
    #[serde(rename = "type")]
    kind: OversizedLegacyKind,
    #[serde(default)]
    timestamp: Option<IgnoredString>,
    ordinal: crate::NullOrdinal,
    message: IgnoredAny,
}

enum OversizedLegacyLine {
    AgentCall(OversizedLegacyAgentLink),
    Narration,
}

fn parse_bounded_legacy_narration_prefix(
    prefix: &[u8],
    body_prefix: &[u8],
    field_marker: &[u8],
) -> io::Result<JournalLine> {
    let marker_offset = body_prefix
        .windows(field_marker.len())
        .position(|window| window == field_marker)
        .ok_or_else(malformed_record_error)?;
    let body_offset = prefix.len().checked_sub(body_prefix.len()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "workflow journal record has invalid prefix bounds",
        )
    })?;
    let value_quote_offset = body_offset
        .checked_add(marker_offset)
        .and_then(|offset| offset.checked_add(field_marker.len() - 1))
        .ok_or_else(malformed_record_error)?;
    let mut bounded = Vec::with_capacity(value_quote_offset + 3);
    bounded.extend_from_slice(&prefix[..value_quote_offset]);
    bounded.extend_from_slice(br#"""}"#);
    serde_json::from_slice::<JournalLine>(&bounded).map_err(|_| malformed_record_error())
}

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
    /// Final zero-based attempt generation for this logical call.
    pub attempt: u32,
    /// The journaled `(prompt, opts)` cache key (e.g. `"blake3:..."`).
    pub key: &'a str,
    /// Completion status; `None` while in-flight/unknown.
    pub status: Option<AgentStatus>,
    /// Intentional selected-attempt terminal outcome, if one settled the call.
    pub control_reason: Option<AgentControlReason>,
    /// The recorded return value (string, object, or `null`).
    pub ret: &'a Value,
    /// Tokens the agent spent, re-added to the budget on a cache hit.
    pub tokens_spent: Option<u64>,
    /// Aggregate progress across every attempt, absent on legacy journals.
    pub progress: Option<&'a AgentCallProgress>,
}

/// One durable run-to-agent transcript link recovered from the journal.
///
/// Unlike replay entries, links need not form a contiguous ordinal prefix: a
/// crashed run can still expose every child transcript whose binding reached
/// durable storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAgentLink {
    pub ordinal: u64,
    /// Zero-based attempt generation. Legacy bindings deserialize as attempt zero.
    pub attempt: u32,
    pub child_thread_id: String,
    /// Host-local absolute path recorded for the child's rollout.
    pub rollout_path: PathBuf,
}

/// Journal metadata plus every durable child-transcript link, sorted by ordinal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAgentJournal {
    run_meta: WorkflowRunMeta,
    links: Vec<RunAgentLink>,
}

impl RunAgentJournal {
    /// Load run-to-agent links from a workflow journal on disk.
    pub fn load(path: &Path) -> io::Result<Self> {
        crate::storage::harden_journal_for_read(path)?;
        Self::from_reader(crate::private_fs::open_private_read(
            path,
            "workflow journal",
        )?)
    }

    /// Recover every linked agent, independent of replay-prefix gaps.
    ///
    /// The journal is scanned tail-first, so a crash-truncated tail is ignored.
    /// Duplicate ordinals may repeat the same binding as an in-flight call
    /// becomes terminal; conflicting bindings are rejected rather than silently
    /// associating a run with the wrong transcript. Physical line zero must be
    /// the journal's sole `run_meta` record.
    pub fn from_reader<R: Read + Seek>(mut reader: R) -> io::Result<Self> {
        let authenticated_run_meta = read_run_meta_at_line_zero(&mut reader)?;
        let may_skip_oversized_legacy_records =
            permits_oversized_legacy_records(&authenticated_run_meta);
        let mut scanner = ReverseJsonlScanner::new(reader)?;
        let mut saw_run_meta = false;
        let mut links = BTreeMap::<(u64, u32), RunAgentLink>::new();

        while let Some(record) = scanner.scan_next()? {
            if record.oversized {
                if may_skip_oversized_legacy_records {
                    if let OversizedLegacyLine::AgentCall(link) =
                        scanner.parse_oversized_legacy_line(&record)?
                        && let (Some(child_thread_id), Some(rollout_path)) =
                            (link.child_thread_id, link.rollout_path)
                    {
                        insert_run_agent_link(
                            &mut links,
                            link.ordinal,
                            link.attempt,
                            &child_thread_id,
                            &rollout_path,
                        )?;
                    }
                    continue;
                }
                return Err(oversized_record_error());
            }
            if let Ok(line) = serde_json::from_slice::<JournalLine>(&record.bytes) {
                match line {
                    JournalLine::AgentCall(call) => {
                        call.validate_structure()
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        if let (Some(child_thread_id), Some(rollout_path)) =
                            (&call.child_thread_id, &call.rollout_path)
                        {
                            insert_run_agent_link(
                                &mut links,
                                call.ordinal,
                                call.attempt,
                                child_thread_id,
                                rollout_path,
                            )?;
                        }
                    }
                    JournalLine::AgentBound(bound) => {
                        bound
                            .validate()
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        insert_run_agent_link(
                            &mut links,
                            bound.ordinal,
                            bound.attempt,
                            &bound.child_thread_id,
                            &bound.rollout_path,
                        )?;
                    }
                    JournalLine::Phase(_) | JournalLine::Log(_) => {}
                }
            } else if let Ok(meta) = serde_json::from_slice::<WorkflowRunMeta>(&record.bytes) {
                if saw_run_meta {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "workflow journal contains more than one run_meta record",
                    ));
                }
                if meta != authenticated_run_meta {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "workflow journal run_meta changed while it was read",
                    ));
                }
                saw_run_meta = true;
            } else {
                reject_complete_unrecognized_record(&record)?;
            }
        }

        if !saw_run_meta {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow journal is missing its run_meta (line 0)",
            ));
        }
        Ok(Self {
            run_meta: authenticated_run_meta,
            links: links.into_values().collect(),
        })
    }

    pub fn run_meta(&self) -> &WorkflowRunMeta {
        &self.run_meta
    }

    pub fn links(&self) -> &[RunAgentLink] {
        &self.links
    }
}

fn insert_run_agent_link(
    links: &mut BTreeMap<(u64, u32), RunAgentLink>,
    ordinal: u64,
    attempt: u32,
    child_thread_id: &str,
    rollout_path: &str,
) -> io::Result<()> {
    let rollout_path = PathBuf::from(rollout_path);
    if !rollout_path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "agent ordinal {ordinal} has non-absolute rollout_path: {}",
                rollout_path.display()
            ),
        ));
    }
    let link = RunAgentLink {
        ordinal,
        attempt,
        child_thread_id: child_thread_id.to_string(),
        rollout_path,
    };
    let key = (ordinal, attempt);
    if let Some(existing) = links.get(&key) {
        if existing != &link {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "agent ordinal {ordinal} attempt {attempt} has conflicting child transcript links"
                ),
            ));
        }
    } else {
        if links.len() >= MAX_RUN_AGENT_LINKS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("workflow journal exceeds the {MAX_RUN_AGENT_LINKS}-agent linkage cap"),
            ));
        }
        links.insert(key, link);
    }
    Ok(())
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
    /// The effective provider/router/model environment differs, or the prior
    /// journal predates execution fingerprints. Replaying inherited defaults
    /// in either case could return an answer produced by a different backend.
    ExecutionFingerprint,
}

impl ReplayJournal {
    /// Load and validate a prior run's `journal.jsonl` from `path`.
    ///
    /// Convenience wrapper over [`ReplayJournal::from_reader`] that opens the
    /// file. See that method for the parsing/reconstruction contract.
    pub fn load(path: &Path) -> io::Result<Self> {
        crate::storage::harden_journal_for_read(path)?;
        Self::from_reader(crate::private_fs::open_private_read(
            path,
            "workflow journal",
        )?)
    }

    /// Load and validate a prior journal from any seekable reader.
    ///
    /// Reads tail-first via a reverse JSONL scanner. An unparsable final record
    /// is skipped only when it has no terminating newline, which is the framing
    /// left by a crash mid-append. Malformed newline-terminated records are hard
    /// errors. Physical line zero must be the journal's sole `run_meta` record.
    /// `agent_call` lines are placed by ordinal into a contiguous `entries[0..M]`;
    /// `phase`/`log` lines are dropped. A legacy return above the current size
    /// bound caps that prefix at its ordinal so resume continues live without
    /// loading the value. A record above the scanner cap is discarded without
    /// materializing it only for metadata from the original unbounded writer;
    /// the whole replay prefix then runs live. Current journals fail closed.
    /// Missing or misplaced run metadata and structural record violations remain
    /// hard errors.
    ///
    /// If ordinals are non-contiguous (a gap, e.g. from a lost middle line),
    /// only the contiguous prefix from ordinal 0 is retained: replay can only
    /// ever serve an unbroken prefix, so a hole caps `M` at the hole. Duplicate
    /// ordinals keep the most-recently-appended line.
    pub fn from_reader<R: Read + Seek>(mut reader: R) -> io::Result<Self> {
        let authenticated_run_meta = read_run_meta_at_line_zero(&mut reader)?;
        let may_skip_oversized_legacy_records =
            permits_oversized_legacy_records(&authenticated_run_meta);
        let mut scanner = ReverseJsonlScanner::new(reader)?;

        let mut saw_run_meta = false;
        let mut saw_oversized_legacy_record = false;
        // ordinal -> line. BTreeMap keeps a deterministic, sorted view so the
        // contiguous-prefix walk below is independent of read order. `None`
        // marks a legacy record whose return exceeds the current replay bound:
        // the ordinal remains occupied so an older duplicate cannot revive it,
        // but the safe prefix stops before that value reaches the runtime.
        let mut by_ordinal: BTreeMap<u64, Option<AgentCallLine>> = BTreeMap::new();

        while let Some(record) = scanner.scan_next()? {
            if record.oversized {
                if may_skip_oversized_legacy_records {
                    if matches!(
                        scanner.parse_oversized_legacy_line(&record)?,
                        OversizedLegacyLine::AgentCall(_)
                    ) {
                        saw_oversized_legacy_record = true;
                    }
                    continue;
                }
                return Err(oversized_record_error());
            }
            // Try the body-line shape first: `JournalLine` is internally tagged
            // on `type` and covers agent_call/phase/log. The run-meta (line 0)
            // has `type:"run_meta"`, which is not a `JournalLine` variant, so it
            // falls through to the second parse. Only a syntactically incomplete
            // unterminated crash tail may fail both and be skipped; complete JSON
            // and newline-terminated corruption fail closed.
            //
            // We deliberately do NOT fold run_meta into a single internally
            // tagged enum: `WorkflowRunMeta` carries its own `type` field (the
            // `kind` discriminant), and serde's internal tagging strips the tag
            // from the content before handing it to the variant, which would make
            // that field fail to deserialize.
            if let Ok(journal_line) = serde_json::from_slice::<JournalLine>(&record.bytes) {
                match journal_line {
                    JournalLine::AgentCall(call) => {
                        // Reading backward, `or_insert` keeps the most recently
                        // appended occurrence of a duplicated ordinal.
                        if by_ordinal.contains_key(&call.ordinal) {
                            continue;
                        }
                        if by_ordinal.len() >= MAX_RUN_AGENT_LINKS {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "workflow journal exceeds the {MAX_RUN_AGENT_LINKS}-agent replay cap"
                                ),
                            ));
                        }
                        call.validate_structure()
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        let replayable = ensure_workflow_agent_return(&call.ret).is_ok();
                        by_ordinal.insert(call.ordinal, replayable.then_some(*call));
                    }
                    // Narration lines carry no ordinal and are not replayed.
                    JournalLine::AgentBound(_) | JournalLine::Phase(_) | JournalLine::Log(_) => {}
                }
            } else if let Ok(meta) = serde_json::from_slice::<WorkflowRunMeta>(&record.bytes) {
                if saw_run_meta {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "workflow journal contains more than one run_meta record",
                    ));
                }
                if meta != authenticated_run_meta {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "workflow journal run_meta changed while it was read",
                    ));
                }
                saw_run_meta = true;
            } else {
                reject_complete_unrecognized_record(&record)?;
            }
            // Else: a syntactically incomplete, unterminated tail — skip and
            // keep scanning.
        }

        if !saw_run_meta {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow journal is missing its run_meta (line 0)",
            ));
        }

        if saw_oversized_legacy_record {
            by_ordinal.clear();
        }

        // Walk ordinals from 0 up, stopping at the first gap. This yields a
        // contiguous prefix in strict ordinal order regardless of the physical
        // (completion-order) layout on disk.
        let mut entries = Vec::new();
        let mut next: u64 = 0;
        while let Some(Some(line)) = by_ordinal.remove(&next) {
            entries.push(line);
            next = match next.checked_add(1) {
                Some(n) => n,
                None => break,
            };
        }

        Ok(Self {
            run_meta: authenticated_run_meta,
            entries,
        })
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
            attempt: line.attempt,
            key: &line.key,
            status: line.status,
            control_reason: line.control_reason,
            ret: &line.ret,
            tokens_spent: line.tokens_spent,
            progress: line.progress.as_ref(),
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

    /// Check structural compatibility plus the current host execution
    /// environment. Unlike [`check_compatibility`](Self::check_compatibility),
    /// this deliberately rejects legacy journals whose run metadata has no
    /// execution fingerprint.
    pub fn check_execution_compatibility(
        &self,
        script_hash: &str,
        args_hash: &str,
        key_algo_version: u32,
        execution_fingerprint: &str,
    ) -> Option<Divergence> {
        if let Some(divergence) = self.check_compatibility(script_hash, args_hash, key_algo_version)
        {
            return Some(divergence);
        }
        if self.run_meta.execution_fingerprint.as_deref() != Some(execution_fingerprint) {
            return Some(Divergence::ExecutionFingerprint);
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
// Kept private: only the journal loaders above consume it. Unlike the
// rollout original it yields the raw record bytes rather than a parsed value —
// the loader must attempt two distinct types per line (a body `JournalLine` or
// the head `WorkflowRunMeta`), so parsing stays with the caller. A line that
// parses as neither is dropped only when its framing and JSON syntax prove it is
// an incomplete crash tail.

const READ_CHUNK_SIZE: usize = 8 * 1024;

fn malformed_record_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "workflow journal contains a malformed newline-terminated record",
    )
}

fn oversized_record_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "workflow journal record exceeds the {}-byte cap",
            crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES
        ),
    )
}

/// The initial workflow writer had neither marker and no per-record bound.
/// Later writers always carry at least one marker before they can emit records
/// governed by the cap, so an oversized record in those journals is corruption.
fn permits_oversized_legacy_records(run_meta: &WorkflowRunMeta) -> bool {
    run_meta.owner_thread_id.is_none() && run_meta.execution_fingerprint.is_none()
}

/// Accept only the framing left by a crash in the middle of JSON serialization.
///
/// A newline-terminated record is durable corruption. An unterminated but
/// syntactically complete JSON value is also a complete write rather than a torn
/// suffix, so silently dropping it could repeat work after a schema change.
fn reject_complete_unrecognized_record(record: &ScannedRecord) -> io::Result<()> {
    if record.newline_terminated {
        return Err(malformed_record_error());
    }
    match serde_json::from_slice::<Value>(&record.bytes) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workflow journal contains an unrecognized complete final record",
        )),
        Err(error) if error.is_eof() => Ok(()),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workflow journal contains a malformed final record",
        )),
    }
}

/// Authenticate the bounded physical line-zero record before tail-first scanning.
fn read_run_meta_at_line_zero<R: Read + Seek>(reader: &mut R) -> io::Result<WorkflowRunMeta> {
    reader.seek(SeekFrom::Start(0))?;
    let mut header_reader = BufReader::new(&mut *reader);
    let mut header = Vec::new();
    header_reader
        .by_ref()
        .take(crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES as u64 + 2)
        .read_until(b'\n', &mut header)?;
    if header.last() == Some(&b'\n') {
        header.pop();
    }
    if header.len() > crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workflow journal line-zero record exceeds its byte cap",
        ));
    }
    serde_json::from_slice(&header).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "workflow journal does not begin with run_meta (line 0)",
        )
    })
}

/// Read-only scanner for newline-delimited records, starting from the end.
struct ScannedRecord {
    bytes: Vec<u8>,
    newline_terminated: bool,
    oversized: bool,
    start: u64,
    end: u64,
}

struct ReverseJsonlScanner<R> {
    reader: R,
    next_chunk_end: u64,
    chunk_position: usize,
    chunk: Vec<u8>,
    record_reversed: Vec<u8>,
    current_record_end: u64,
    current_record_oversized: bool,
    current_record_newline_terminated: bool,
    records_scanned: usize,
}

impl<R> ReverseJsonlScanner<R>
where
    R: Read + Seek,
{
    fn new(mut reader: R) -> io::Result<Self> {
        let next_chunk_end = reader.seek(SeekFrom::End(0))?;
        if next_chunk_end > crate::WORKFLOW_JOURNAL_MAX_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "workflow journal exceeds the {}-byte run cap",
                    crate::WORKFLOW_JOURNAL_MAX_BYTES
                ),
            ));
        }
        Ok(Self {
            reader,
            next_chunk_end,
            chunk_position: 0,
            chunk: vec![0; READ_CHUNK_SIZE],
            record_reversed: Vec::new(),
            current_record_end: next_chunk_end,
            current_record_oversized: false,
            current_record_newline_terminated: false,
            records_scanned: 0,
        })
    }

    /// Returns the next nonblank record's raw bytes (tail-first), or `None` at
    /// the start of the file. Blank/whitespace-only records are skipped. I/O
    /// failures surface as [`Err`].
    fn scan_next(&mut self) -> io::Result<Option<ScannedRecord>> {
        loop {
            let Some((offset, byte)) = self.read_previous_byte()? else {
                let record = self.finish_record(
                    /*start*/ 0,
                    self.current_record_end,
                    self.current_record_newline_terminated,
                );
                if record.is_some() {
                    self.record_scanned()?;
                }
                return Ok(record);
            };

            if byte != b'\n' {
                if self.current_record_oversized {
                    continue;
                }
                if self.record_reversed.len() >= crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES {
                    self.record_reversed.clear();
                    self.current_record_oversized = true;
                } else {
                    self.record_reversed.push(byte);
                }
                continue;
            }

            let newline_terminated = self.current_record_newline_terminated;
            self.current_record_newline_terminated = true;
            let record_end = self.current_record_end;
            self.current_record_end = offset;
            if let Some(record) = self.finish_record(offset + 1, record_end, newline_terminated) {
                self.record_scanned()?;
                return Ok(Some(record));
            }
        }
    }

    /// Validate a complete oversized original-writer record without materializing its return,
    /// then recover the canonical pre-return linkage fields from a bounded prefix.
    fn parse_oversized_legacy_line(
        &mut self,
        record: &ScannedRecord,
    ) -> io::Result<OversizedLegacyLine> {
        debug_assert!(record.oversized);

        let record_len = record.end.checked_sub(record.start).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow journal record has invalid byte bounds",
            )
        })?;
        let prefix_len = record_len.min(crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES as u64);
        let prefix_len = usize::try_from(prefix_len).map_err(io::Error::other)?;
        self.reader.seek(SeekFrom::Start(record.start))?;
        let mut prefix = vec![0; prefix_len];
        self.reader.read_exact(&mut prefix)?;
        enum LegacyRecordKind {
            AgentCall,
            Phase,
            Log,
        }
        let (kind, body_prefix) = prefix
            .strip_prefix(br#"{"type":"agent_call","#)
            .map(|rest| (LegacyRecordKind::AgentCall, rest))
            .or_else(|| {
                prefix
                    .strip_prefix(br#"{"type":"phase","#)
                    .map(|rest| (LegacyRecordKind::Phase, rest))
            })
            .or_else(|| {
                prefix
                    .strip_prefix(br#"{"type":"log","#)
                    .map(|rest| (LegacyRecordKind::Log, rest))
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "workflow journal contains an unrecognized oversized legacy record",
                )
            })?;
        match kind {
            LegacyRecordKind::AgentCall => {
                let return_offset = body_prefix
                    .windows(LEGACY_AGENT_CALL_RETURN_FIELD.len())
                    .position(|window| window == LEGACY_AGENT_CALL_RETURN_FIELD)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "oversized legacy agent_call linkage fields exceed their bounded prefix",
                        )
                    })?;
                let original_prefix_len = br#"{"type":"agent_call","#.len();
                let mut link_record = Vec::with_capacity(original_prefix_len + return_offset + 1);
                link_record.extend_from_slice(br#"{"type":"agent_call","#);
                link_record.extend_from_slice(&body_prefix[..return_offset]);
                link_record.push(b'}');
                let link = serde_json::from_slice::<OversizedLegacyAgentLink>(&link_record)
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "workflow journal contains malformed oversized legacy linkage fields",
                        )
                    })?;
                let call = self.deserialize_complete_record::<OversizedLegacyAgentCall>(record)?;
                Ok(OversizedLegacyLine::AgentCall(
                    call.validate_and_link(link)?,
                ))
            }
            LegacyRecordKind::Phase => {
                if !matches!(
                    parse_bounded_legacy_narration_prefix(
                        &prefix,
                        body_prefix,
                        LEGACY_PHASE_TITLE_FIELD,
                    )?,
                    JournalLine::Phase(_)
                ) {
                    return Err(malformed_record_error());
                }
                let phase = self.deserialize_complete_record::<OversizedLegacyPhase>(record)?;
                if phase.kind != OversizedLegacyKind::Phase {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "workflow journal oversized legacy record changed type while parsing",
                    ));
                }
                let _ = (phase.timestamp, phase.ordinal, phase.title);
                Ok(OversizedLegacyLine::Narration)
            }
            LegacyRecordKind::Log => {
                if !matches!(
                    parse_bounded_legacy_narration_prefix(
                        &prefix,
                        body_prefix,
                        LEGACY_LOG_MESSAGE_FIELD,
                    )?,
                    JournalLine::Log(_)
                ) {
                    return Err(malformed_record_error());
                }
                let log = self.deserialize_complete_record::<OversizedLegacyLog>(record)?;
                if log.kind != OversizedLegacyKind::Log {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "workflow journal oversized legacy record changed type while parsing",
                    ));
                }
                let _ = (log.timestamp, log.ordinal, log.message);
                Ok(OversizedLegacyLine::Narration)
            }
        }
    }

    fn deserialize_complete_record<T>(&mut self, record: &ScannedRecord) -> io::Result<T>
    where
        T: DeserializeOwned,
    {
        self.reader.seek(SeekFrom::Start(record.start))?;
        let record_len = record.end.checked_sub(record.start).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow journal record has invalid byte bounds",
            )
        })?;
        let reader = (&mut self.reader).take(record_len);
        let mut deserializer = serde_json::Deserializer::from_reader(reader);
        let value = T::deserialize(&mut deserializer).map_err(|_| malformed_record_error())?;
        deserializer.end().map_err(|_| malformed_record_error())?;
        Ok(value)
    }

    fn record_scanned(&mut self) -> io::Result<()> {
        self.records_scanned = self.records_scanned.checked_add(1).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow journal record count overflow",
            )
        })?;
        if self.records_scanned > crate::WORKFLOW_JOURNAL_MAX_RECORDS {
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

    fn read_previous_byte(&mut self) -> io::Result<Option<(u64, u8)>> {
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
        let offset = self.next_chunk_end + self.chunk_position as u64;
        Ok(Some((offset, self.chunk[self.chunk_position])))
    }

    fn finish_record(
        &mut self,
        start: u64,
        end: u64,
        newline_terminated: bool,
    ) -> Option<ScannedRecord> {
        if self.current_record_oversized {
            self.current_record_oversized = false;
            self.record_reversed.clear();
            return Some(ScannedRecord {
                bytes: Vec::new(),
                newline_terminated,
                oversized: true,
                start,
                end,
            });
        }
        self.record_reversed.reverse();
        let record = if self.record_reversed.iter().all(u8::is_ascii_whitespace) {
            None
        } else {
            Some(ScannedRecord {
                bytes: self.record_reversed.clone(),
                newline_terminated,
                oversized: false,
                start,
                end,
            })
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
            Some(500_000),
            KEY_ALGO_VERSION,
            "2026-07-17T00:00:00Z".to_string(),
        )
    }

    fn current_run_meta() -> WorkflowRunMeta {
        run_meta()
            .with_owner_thread_id("01900000-0000-7000-8000-000000000001".to_string())
            .with_execution_fingerprint("blake3:execution".to_string())
    }

    fn agent_call(ordinal: u64) -> AgentCallLine {
        AgentCallLine {
            timestamp: None,
            ordinal,
            attempt: 0,
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
            control_reason: None,
            ret: json!({ "ordinal": ordinal }),
            tokens_spent: Some(1000 + ordinal),
            progress: None,
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

    fn legacy_journal_with_raw_line(meta: &WorkflowRunMeta, line: &str) -> Vec<u8> {
        let mut out = serde_json::to_string(meta).expect("serialize meta");
        out.push('\n');
        out.push_str(line);
        out.push('\n');
        out.into_bytes()
    }

    fn assert_oversized_legacy_line_rejected(bytes: Vec<u8>, expected: &str) {
        for error in [
            ReplayJournal::from_reader(Cursor::new(bytes.clone()))
                .expect_err("oversized legacy replay record must reject"),
            RunAgentJournal::from_reader(Cursor::new(bytes.clone()))
                .expect_err("oversized legacy linkage record must reject"),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(
                error.to_string().contains(expected),
                "expected {expected:?} in {error}"
            );
        }
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
    fn execution_fingerprint_must_match_and_legacy_metadata_diverges() {
        let mut meta = run_meta().with_execution_fingerprint("blake3:provider-a".to_string());
        let journal = load(journal_bytes(&meta, &[]));
        assert_eq!(
            journal.check_execution_compatibility(
                SCRIPT_HASH,
                ARGS_HASH,
                KEY_ALGO_VERSION,
                "blake3:provider-a",
            ),
            None
        );
        assert_eq!(
            journal.check_execution_compatibility(
                SCRIPT_HASH,
                ARGS_HASH,
                KEY_ALGO_VERSION,
                "blake3:provider-b",
            ),
            Some(Divergence::ExecutionFingerprint)
        );

        meta.execution_fingerprint = None;
        let legacy = load(journal_bytes(&meta, &[]));
        assert_eq!(
            legacy.check_execution_compatibility(
                SCRIPT_HASH,
                ARGS_HASH,
                KEY_ALGO_VERSION,
                "blake3:provider-a",
            ),
            Some(Divergence::ExecutionFingerprint)
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
        let links = RunAgentJournal::from_reader(Cursor::new(bytes.clone()))
            .expect("run-agent loader ignores the same torn tail");
        let journal = load(bytes);

        // The intact prefix (ordinals 0..2) loads; the torn line is dropped.
        assert_eq!(journal.len(), 3);
        assert_eq!(journal.run_meta(), &meta);
        assert_eq!(links.run_meta(), &meta);
        assert!(journal.lookup(2).is_some());
        assert!(journal.lookup(3).is_none());
    }

    #[test]
    fn both_loaders_reject_malformed_newline_terminated_records() {
        let meta = run_meta();
        let meta_json = serde_json::to_string(&meta).expect("serialize metadata");
        let first = serde_json::to_string(&JournalLine::AgentCall(Box::new(agent_call(0))))
            .expect("serialize first call");
        let second = serde_json::to_string(&JournalLine::AgentCall(Box::new(agent_call(1))))
            .expect("serialize second call");

        for bytes in [
            format!("{meta_json}\n{first}\n{{malformed}}\n{second}\n").into_bytes(),
            format!("{meta_json}\n{first}\n{{malformed}}\n").into_bytes(),
        ] {
            let replay_error = ReplayJournal::from_reader(Cursor::new(bytes.clone()))
                .expect_err("replay loader must reject durable corruption");
            let links_error = RunAgentJournal::from_reader(Cursor::new(bytes))
                .expect_err("run-agent loader must reject durable corruption");
            assert_eq!(
                (replay_error.kind(), links_error.kind()),
                (io::ErrorKind::InvalidData, io::ErrorKind::InvalidData),
            );
            assert!(replay_error.to_string().contains("newline-terminated"));
            assert!(links_error.to_string().contains("newline-terminated"));
        }
    }

    #[test]
    fn both_loaders_reject_complete_unrecognized_unterminated_final_records() {
        let meta_json = serde_json::to_string(&run_meta()).expect("serialize metadata");

        for tail in [r#"{"type":"future_record"}"#, r#"{"type":"log"}"#] {
            let bytes = format!("{meta_json}\n{tail}").into_bytes();
            let replay_error = ReplayJournal::from_reader(Cursor::new(bytes.clone()))
                .expect_err("replay loader must reject a complete unknown final record");
            let links_error = RunAgentJournal::from_reader(Cursor::new(bytes))
                .expect_err("run-agent loader must reject a complete unknown final record");
            assert_eq!(
                (replay_error.kind(), links_error.kind()),
                (io::ErrorKind::InvalidData, io::ErrorKind::InvalidData),
            );
            assert!(replay_error.to_string().contains("complete final record"));
            assert!(links_error.to_string().contains("complete final record"));
        }
    }

    #[test]
    fn both_loaders_reject_non_eof_invalid_unterminated_final_records() {
        let meta_json = serde_json::to_string(&run_meta()).expect("serialize metadata");

        for tail in ["garbage", "{]"] {
            let bytes = format!("{meta_json}\n{tail}").into_bytes();
            let replay_error = ReplayJournal::from_reader(Cursor::new(bytes.clone()))
                .expect_err("replay loader must reject malformed final syntax");
            let links_error = RunAgentJournal::from_reader(Cursor::new(bytes))
                .expect_err("run-agent loader must reject malformed final syntax");
            assert_eq!(
                (replay_error.kind(), links_error.kind()),
                (io::ErrorKind::InvalidData, io::ErrorKind::InvalidData),
            );
            assert!(replay_error.to_string().contains("malformed final record"));
            assert!(links_error.to_string().contains("malformed final record"));
        }
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
    fn both_loaders_require_one_run_meta_on_physical_line_zero() {
        let meta = run_meta();
        let body = serde_json::to_string(&JournalLine::AgentCall(Box::new(agent_call(0))))
            .expect("serialize body");
        let meta_json = serde_json::to_string(&meta).expect("serialize metadata");

        let malformed_head_then_meta = format!("{{\n{meta_json}\n{body}\n").into_bytes();
        let body_head_then_meta = format!("{body}\n{meta_json}\n").into_bytes();
        let blank_head_then_meta = format!("\n{meta_json}\n{body}\n").into_bytes();
        let duplicate_meta = format!("{meta_json}\n{meta_json}\n{body}\n").into_bytes();

        for bytes in [
            malformed_head_then_meta,
            body_head_then_meta,
            blank_head_then_meta,
            duplicate_meta,
        ] {
            let replay_error = ReplayJournal::from_reader(Cursor::new(bytes.clone()))
                .expect_err("replay requires unique physical line-zero metadata");
            let links_error = RunAgentJournal::from_reader(Cursor::new(bytes))
                .expect_err("run-agent links require unique physical line-zero metadata");
            assert_eq!(
                (replay_error.kind(), links_error.kind()),
                (io::ErrorKind::InvalidData, io::ErrorKind::InvalidData)
            );
        }
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

    #[cfg(unix)]
    #[test]
    fn file_loaders_harden_a_legacy_journal_and_its_layout() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().expect("tempdir");
        let mut meta = run_meta();
        meta.run_id = uuid::Uuid::now_v7().to_string();
        let paths = crate::storage::WorkflowRunPaths::new(home.path(), &meta.run_id);
        std::fs::create_dir_all(paths.run_dir()).expect("create legacy run layout");
        let directories = [
            crate::storage::workflows_root(home.path()),
            crate::storage::runs_root(home.path()),
            paths.run_dir().to_path_buf(),
        ];
        for directory in &directories {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o775))
                .expect("make legacy directory permissive");
        }
        std::fs::write(paths.journal(), journal_bytes(&meta, &[])).expect("write journal");
        std::fs::set_permissions(paths.journal(), std::fs::Permissions::from_mode(0o664))
            .expect("make legacy journal permissive");

        let replay = ReplayJournal::load(&paths.journal()).expect("load replay journal");
        assert_eq!(replay.run_meta(), &meta);
        assert_private_layout(&directories, &paths.journal());

        for directory in &directories {
            std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o775))
                .expect("restore permissive legacy directory");
        }
        std::fs::set_permissions(paths.journal(), std::fs::Permissions::from_mode(0o664))
            .expect("restore permissive legacy journal");
        let links = RunAgentJournal::load(&paths.journal()).expect("load run-agent journal");
        assert_eq!(links.run_meta(), &meta);
        assert_private_layout(&directories, &paths.journal());
    }

    #[cfg(unix)]
    #[test]
    fn file_loaders_reject_a_symlink_instead_of_following_it() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("real-journal.jsonl");
        let link = dir.path().join("journal.jsonl");
        std::fs::write(&target, journal_bytes(&run_meta(), &[])).expect("write target journal");
        symlink(&target, &link).expect("create journal symlink");

        ReplayJournal::load(&link).expect_err("replay loader must not follow a journal symlink");
        RunAgentJournal::load(&link)
            .expect_err("run-agent loader must not follow a journal symlink");
    }

    #[cfg(unix)]
    fn assert_private_layout(directories: &[PathBuf], journal: &Path) {
        use std::os::unix::fs::PermissionsExt;

        for directory in directories {
            assert_eq!(
                std::fs::metadata(directory)
                    .expect("directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        assert_eq!(
            std::fs::metadata(journal)
                .expect("journal metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn run_agent_links_ignore_replay_gaps_and_sort_by_ordinal() {
        let meta = run_meta();
        let lines = [4, 0, 2]
            .into_iter()
            .map(|ordinal| JournalLine::AgentCall(Box::new(agent_call(ordinal))))
            .collect::<Vec<_>>();

        let journal = RunAgentJournal::from_reader(Cursor::new(journal_bytes(&meta, &lines)))
            .expect("load links");

        assert_eq!(journal.run_meta(), &meta);
        assert_eq!(
            journal
                .links()
                .iter()
                .map(|link| link.ordinal)
                .collect::<Vec<_>>(),
            vec![0, 2, 4]
        );
    }

    #[test]
    fn run_agent_links_merge_matching_bound_and_terminal_records() {
        let meta = run_meta();
        let child_thread_id = uuid::Uuid::now_v7().to_string();
        let rollout_path = "/p/rollout-0.jsonl";
        let bound = crate::AgentBoundLine {
            timestamp: None,
            ordinal: 0,
            attempt: 0,
            child_thread_id: child_thread_id.clone(),
            rollout_path: rollout_path.to_string(),
        };
        let mut completed = agent_call(0);
        completed.child_thread_id = Some(child_thread_id.clone());
        completed.rollout_path = Some(rollout_path.to_string());

        let journal = RunAgentJournal::from_reader(Cursor::new(journal_bytes(
            &meta,
            &[
                JournalLine::AgentBound(bound),
                JournalLine::AgentCall(Box::new(completed)),
            ],
        )))
        .expect("matching duplicates are valid");

        assert_eq!(journal.links().len(), 1);
        assert_eq!(journal.links()[0].child_thread_id, child_thread_id);
    }

    #[test]
    fn run_agent_links_retain_every_retry_attempt_in_generation_order() {
        let meta = run_meta();
        let first_thread_id = uuid::Uuid::now_v7().to_string();
        let retry_thread_id = uuid::Uuid::now_v7().to_string();
        let first = crate::AgentBoundLine {
            timestamp: None,
            ordinal: 0,
            attempt: 0,
            child_thread_id: first_thread_id.clone(),
            rollout_path: "/p/rollout-0-attempt-0.jsonl".to_string(),
        };
        let retry = crate::AgentBoundLine {
            timestamp: None,
            ordinal: 0,
            attempt: 1,
            child_thread_id: retry_thread_id.clone(),
            rollout_path: "/p/rollout-0-attempt-1.jsonl".to_string(),
        };
        let mut completed = agent_call(0);
        completed.attempt = 1;
        completed.child_thread_id = Some(retry_thread_id.clone());
        completed.rollout_path = Some("/p/rollout-0-attempt-1.jsonl".to_string());
        completed.tokens_spent = Some(2_111);

        let lines = vec![
            JournalLine::AgentBound(first),
            JournalLine::AgentBound(retry),
            JournalLine::AgentCall(Box::new(completed.clone())),
        ];
        let journal = RunAgentJournal::from_reader(Cursor::new(journal_bytes(&meta, &lines)))
            .expect("all attempt bindings");
        assert_eq!(
            journal.links(),
            &[
                RunAgentLink {
                    ordinal: 0,
                    attempt: 0,
                    child_thread_id: first_thread_id,
                    rollout_path: PathBuf::from("/p/rollout-0-attempt-0.jsonl"),
                },
                RunAgentLink {
                    ordinal: 0,
                    attempt: 1,
                    child_thread_id: retry_thread_id,
                    rollout_path: PathBuf::from("/p/rollout-0-attempt-1.jsonl"),
                },
            ]
        );

        let replay = ReplayJournal::from_reader(Cursor::new(journal_bytes(&meta, &lines)))
            .expect("logical replay");
        assert_eq!(replay.entries(), &[completed]);
    }

    #[test]
    fn run_agent_links_reject_conflicting_duplicate_bindings() {
        let meta = run_meta();
        let first = agent_call(0);
        let mut conflicting = agent_call(0);
        conflicting.child_thread_id = Some("different-thread".to_string());

        let error = RunAgentJournal::from_reader(Cursor::new(journal_bytes(
            &meta,
            &[
                JournalLine::AgentCall(Box::new(first)),
                JournalLine::AgentCall(Box::new(conflicting)),
            ],
        )))
        .expect_err("conflicting binding must fail closed");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("conflicting"));
    }

    #[test]
    fn run_agent_links_ignore_partial_nonterminal_linkage() {
        let meta = run_meta();
        let mut partial = agent_call(0);
        partial.status = None;
        partial.tokens_spent = None;
        partial.rollout_path = None;

        let journal = RunAgentJournal::from_reader(Cursor::new(journal_bytes(
            &meta,
            &[JournalLine::AgentCall(Box::new(partial))],
        )))
        .expect("partial in-flight record is not yet a durable link");

        assert!(journal.links().is_empty());
    }

    #[test]
    fn run_agent_links_retain_an_oversized_legacy_return() {
        let meta = run_meta();
        let mut legacy = agent_call(0);
        legacy.ret = Value::String("x".repeat(crate::WORKFLOW_AGENT_RETURN_MAX_BYTES));

        let journal = RunAgentJournal::from_reader(Cursor::new(journal_bytes(
            &meta,
            &[JournalLine::AgentCall(Box::new(legacy))],
        )))
        .expect("return size must not hide an otherwise valid transcript link");

        assert_eq!(
            journal.links(),
            &[RunAgentLink {
                ordinal: 0,
                attempt: 0,
                child_thread_id: "th_0".to_string(),
                rollout_path: PathBuf::from("/p/rollout-0.jsonl"),
            }]
        );
    }

    #[test]
    fn run_agent_links_require_absolute_rollout_paths() {
        let meta = run_meta();
        let mut relative = agent_call(0);
        relative.rollout_path = Some("rollout.jsonl".to_string());

        let error = RunAgentJournal::from_reader(Cursor::new(journal_bytes(
            &meta,
            &[JournalLine::AgentCall(Box::new(relative))],
        )))
        .expect_err("relative transcript path must fail closed");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("non-absolute"));
    }

    #[test]
    fn oversized_legacy_return_at_ordinal_zero_yields_an_empty_prefix() {
        let meta = run_meta();
        let mut oversized = agent_call(0);
        oversized.ret = Value::String("x".repeat(crate::WORKFLOW_AGENT_RETURN_MAX_BYTES));
        let journal = ReplayJournal::from_reader(Cursor::new(journal_bytes(
            &meta,
            &[
                JournalLine::AgentCall(Box::new(oversized)),
                JournalLine::AgentCall(Box::new(agent_call(1))),
            ],
        )))
        .expect("legacy return should become live divergence, not a load failure");

        assert!(journal.is_empty());
        assert!(journal.lookup(0).is_none());
    }

    #[test]
    fn oversized_legacy_return_truncates_a_mid_run_prefix() {
        let meta = run_meta();
        let mut oversized = agent_call(2);
        oversized.ret = Value::String("x".repeat(crate::WORKFLOW_AGENT_RETURN_MAX_BYTES));
        let journal = ReplayJournal::from_reader(Cursor::new(journal_bytes(
            &meta,
            &[
                JournalLine::AgentCall(Box::new(agent_call(0))),
                JournalLine::AgentCall(Box::new(agent_call(1))),
                JournalLine::AgentCall(Box::new(oversized)),
                JournalLine::AgentCall(Box::new(agent_call(3))),
            ],
        )))
        .expect("legacy return should cap the safe replay prefix");

        assert_eq!(
            journal
                .entries()
                .iter()
                .map(|entry| entry.ordinal)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(journal.lookup(2).is_none());
        assert!(journal.lookup(3).is_none());
    }

    #[test]
    fn scanner_rejects_an_oversized_record_before_materializing_it() {
        let mut bytes = serde_json::to_vec(&current_run_meta()).expect("serialize meta");
        bytes.push(b'\n');
        bytes.extend(std::iter::repeat_n(
            b'x',
            crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES + 1,
        ));
        bytes.push(b'\n');

        let error = ReplayJournal::from_reader(Cursor::new(bytes.clone()))
            .expect_err("oversized record must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("record"));

        let error = RunAgentJournal::from_reader(Cursor::new(bytes))
            .expect_err("oversized record must also block transcript-link recovery");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("record"));
    }

    #[test]
    fn scanner_recovers_linkage_from_an_oversized_original_writer_record() {
        let meta = run_meta();
        let mut bytes = journal_bytes(&meta, &[JournalLine::AgentCall(Box::new(agent_call(0)))]);
        let mut oversized = agent_call(1);
        oversized.ret = json!({
            "payload": "x".repeat(crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES),
        });
        bytes.extend(
            serde_json::to_vec(&JournalLine::AgentCall(Box::new(oversized)))
                .expect("serialize original-writer record"),
        );
        bytes.push(b'\n');

        let replay = ReplayJournal::from_reader(Cursor::new(bytes.clone()))
            .expect("original unbounded journal should degrade to live replay");
        assert!(replay.is_empty());

        let links = RunAgentJournal::from_reader(Cursor::new(bytes))
            .expect("bounded legacy link recovery should retain every canonical link");
        assert_eq!(
            links.links(),
            &[
                RunAgentLink {
                    ordinal: 0,
                    attempt: 0,
                    child_thread_id: "th_0".to_string(),
                    rollout_path: PathBuf::from("/p/rollout-0.jsonl"),
                },
                RunAgentLink {
                    ordinal: 1,
                    attempt: 0,
                    child_thread_id: "th_1".to_string(),
                    rollout_path: PathBuf::from("/p/rollout-1.jsonl"),
                },
            ]
        );
    }

    #[test]
    fn scanner_validates_oversized_legacy_records_before_degrading() {
        let mut bytes = serde_json::to_vec(&run_meta()).expect("serialize meta");
        bytes.push(b'\n');
        bytes.extend(std::iter::repeat_n(
            b'x',
            crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES + 1,
        ));
        bytes.push(b'\n');

        for error in [
            ReplayJournal::from_reader(Cursor::new(bytes.clone()))
                .expect_err("malformed oversized legacy replay record must reject"),
            RunAgentJournal::from_reader(Cursor::new(bytes))
                .expect_err("malformed oversized legacy linkage record must reject"),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("unrecognized"));
        }
    }

    #[test]
    fn scanner_rejects_schema_invalid_oversized_legacy_agent_calls() {
        let mut call = agent_call(0);
        call.ret = json!({
            "payload": "x".repeat(crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES),
        });
        let valid = serde_json::to_string(&JournalLine::AgentCall(Box::new(call)))
            .expect("serialize oversized call");
        let invalid_type =
            valid.replacen("\"tokens_spent\":1000", "\"tokens_spent\":\"invalid\"", 1);
        let missing_required = valid.replacen("\"prompt_hash\":\"blake3:ph-0\",", "", 1);
        let duplicate_field = format!(
            "{},\"tokens_spent\":1000}}",
            valid.strip_suffix('}').expect("object suffix")
        );
        for invalid in [invalid_type, missing_required, duplicate_field] {
            assert_ne!(invalid, valid, "fixture must corrupt the typed schema");
            assert!(invalid.len() > crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES);
            assert_oversized_legacy_line_rejected(
                legacy_journal_with_raw_line(&run_meta(), &invalid),
                "malformed",
            );
        }
    }

    #[test]
    fn scanner_rejects_structurally_invalid_oversized_legacy_agent_calls() {
        let mut missing_tokens = agent_call(0);
        missing_tokens.tokens_spent = None;
        missing_tokens.ret = json!({
            "payload": "x".repeat(crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES),
        });

        let mut nonnull_control_return = agent_call(0);
        nonnull_control_return.control_reason = Some(AgentControlReason::UserSkip);
        nonnull_control_return.progress = Some(AgentCallProgress::default());
        nonnull_control_return.ret = json!({
            "payload": "x".repeat(crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES),
        });

        let mut missing_control_progress = agent_call(0);
        missing_control_progress.control_reason = Some(AgentControlReason::UserSkip);
        missing_control_progress.ret = Value::Null;
        missing_control_progress.progress = None;
        let base = serde_json::to_string(&JournalLine::AgentCall(Box::new(
            missing_control_progress.clone(),
        )))
        .expect("serialize control call");
        let return_offset = base
            .find(std::str::from_utf8(LEGACY_AGENT_CALL_RETURN_FIELD).expect("UTF-8 marker"))
            .expect("canonical return field");
        missing_control_progress.key = "x".repeat(
            crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES
                .checked_sub(return_offset + 1)
                .expect("base linkage fits below the record cap"),
        );

        let mut negative_progress = agent_call(0);
        let mut progress = AgentCallProgress::default();
        progress.token_usage.total_tokens = -1;
        negative_progress.progress = Some(progress);
        negative_progress.ret = json!({
            "payload": "x".repeat(crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES),
        });

        for (call, expected) in [
            (missing_tokens, "tokens_spent"),
            (nonnull_control_return, "return=null"),
            (missing_control_progress, "aggregate progress"),
            (negative_progress, "must be non-negative"),
        ] {
            let line = serde_json::to_string(&JournalLine::AgentCall(Box::new(call)))
                .expect("serialize invalid oversized call");
            assert!(
                line.len() > crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES,
                "{expected} fixture must cross the record cap",
            );
            assert!(
                line.find(
                    std::str::from_utf8(LEGACY_AGENT_CALL_RETURN_FIELD).expect("UTF-8 marker")
                )
                .is_some_and(|offset| offset < crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES),
                "{expected} fixture must keep linkage within the bounded prefix",
            );
            assert_oversized_legacy_line_rejected(
                legacy_journal_with_raw_line(&run_meta(), &line),
                expected,
            );
        }
    }

    #[test]
    fn scanner_rejects_schema_invalid_oversized_legacy_narration() {
        let narration = [
            JournalLine::Phase(PhaseLine {
                timestamp: Some("stamp".to_string()),
                ordinal: NullOrdinal,
                title: "x".repeat(crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES),
            }),
            JournalLine::Log(LogLine {
                timestamp: Some("stamp".to_string()),
                ordinal: NullOrdinal,
                message: "x".repeat(crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES),
            }),
        ];
        for line in narration {
            let valid = serde_json::to_string(&line).expect("serialize oversized narration");
            let invalid_type = valid.replacen("\"timestamp\":\"stamp\"", "\"timestamp\":7", 1);
            let duplicate_field = format!(
                "{},\"ordinal\":null}}",
                valid.strip_suffix('}').expect("object suffix")
            );
            for invalid in [invalid_type, duplicate_field] {
                assert_ne!(invalid, valid, "fixture must corrupt the typed schema");
                assert!(invalid.len() > crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES);
                assert_oversized_legacy_line_rejected(
                    legacy_journal_with_raw_line(&run_meta(), &invalid),
                    "malformed",
                );
            }
        }
    }

    #[test]
    fn oversized_legacy_narration_does_not_discard_a_safe_replay_prefix() {
        let meta = run_meta();
        for narration in [
            JournalLine::Phase(PhaseLine {
                timestamp: None,
                ordinal: NullOrdinal,
                title: "x".repeat(crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES),
            }),
            JournalLine::Log(LogLine {
                timestamp: None,
                ordinal: NullOrdinal,
                message: "x".repeat(crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES),
            }),
        ] {
            let lines = [JournalLine::AgentCall(Box::new(agent_call(0))), narration];

            let replay = ReplayJournal::from_reader(Cursor::new(journal_bytes(&meta, &lines)))
                .expect("oversized original-writer narration should remain ignorable");
            assert_eq!(replay.entries(), &[agent_call(0)]);
        }
    }

    #[test]
    fn scanner_rejects_too_many_nonblank_records() {
        let mut bytes = serde_json::to_vec(&run_meta()).expect("serialize meta");
        bytes.push(b'\n');
        for _ in 0..crate::WORKFLOW_JOURNAL_MAX_RECORDS {
            bytes.extend_from_slice(b"{}\n");
        }

        let mut scanner = ReverseJsonlScanner::new(Cursor::new(bytes)).expect("create scanner");
        let error = loop {
            match scanner.scan_next() {
                Ok(Some(_)) => {}
                Ok(None) => panic!("record flood must fail closed"),
                Err(error) => break error,
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("record cap"));
    }

    #[test]
    fn scanner_accepts_exactly_the_nonblank_record_cap() {
        let bytes = "{}\n".repeat(crate::WORKFLOW_JOURNAL_MAX_RECORDS);
        let mut scanner = ReverseJsonlScanner::new(Cursor::new(bytes)).expect("create scanner");
        let mut records_scanned = 0;

        while scanner
            .scan_next()
            .expect("scan bounded record set")
            .is_some()
        {
            records_scanned += 1;
        }

        assert_eq!(records_scanned, crate::WORKFLOW_JOURNAL_MAX_RECORDS);
    }

    #[test]
    fn scanner_rejects_a_sparse_file_over_the_run_cap_without_reading_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("journal.jsonl");
        let file = std::fs::File::create(&path).expect("create sparse journal");
        file.set_len(crate::WORKFLOW_JOURNAL_MAX_BYTES + 1)
            .expect("set sparse length");

        let error = ReplayJournal::load(&path).expect_err("oversized journal must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
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
