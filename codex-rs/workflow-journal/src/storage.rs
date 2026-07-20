//! Per-run on-disk storage layout for dynamic-workflow runs.
//!
//! Mirrors rollout's per-run file layout (`rollout/src/recorder.rs`
//! `precompute_log_file_info`), but keyed by `runId` instead of by date +
//! `thread_id`. Every workflow run gets its own directory:
//!
//! ```text
//! $CODEX_HOME/workflows/runs/<runId>/
//!   journal.jsonl   # source of truth for replay AND run->agent linkage (§7)
//!   script.js       # the executed program (re-invoke by scriptPath)
//!   invocation.json # host-private canonical invocation arguments
//!   meta.json       # run-meta projection (discovery-index rebuild source, §7)
//!   progress.json   # bounded atomic live-tree projection
//!   lease.lock      # advisory live-owner lock (the file persists after release)
//! ```
//!
//! See `docs/dynamic-workflows-spec.md` §7 ("Storage layout").
//!
//! ## Determinism
//!
//! `runId` is minted **host-side** in Rust with [`uuid::Uuid::now_v7`] — never
//! inside the workflow isolate, which has `Date`/`Math`/random disabled (§7).
//! It is the same id exposed to the script read-only via `workflow.runId`.
//! All path resolution takes an explicit `codex_home: &Path` rather than reading
//! the environment, so tests can isolate over a tempdir.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use crate::WorkflowRunLease;
use crate::WorkflowRunMeta;
use crate::WorkflowRunStatus;
use crate::canonical_value_hash;
use crate::key::canonical_value_json;
#[cfg(all(windows, test))]
use crate::private_fs::LocalSecurityDescriptor;
use crate::private_fs::ensure_private_permissions;
#[cfg(all(windows, test))]
use crate::private_fs::ensure_private_windows_dacl;
use crate::private_fs::is_link_or_reparse_point;
#[cfg(all(windows, test))]
use crate::private_fs::windows_descriptor_dacl;
use crate::private_fs::write_private_create_only;

/// Subdirectory under `$CODEX_HOME` holding all workflow state.
pub const WORKFLOWS_SUBDIR: &str = "workflows";
/// Subdirectory under `$CODEX_HOME/workflows` holding per-run directories.
pub const RUNS_SUBDIR: &str = "runs";

/// Filename of the append-only journal (source of truth for replay).
pub const JOURNAL_FILE: &str = "journal.jsonl";
/// Filename of the persisted executed program.
pub const SCRIPT_FILE: &str = "script.js";
/// Filename of the host-private canonical invocation arguments.
pub const INVOCATION_FILE: &str = "invocation.json";
/// Filename of the run-meta projection.
pub const META_FILE: &str = "meta.json";
/// Filename of the bounded, atomically replaced live-progress projection.
pub const PROGRESS_FILE: &str = "progress.json";
/// Filename of the advisory owner lease held for a live run.
pub const LEASE_FILE: &str = "lease.lock";
/// Create-only marker written immediately before a resumed body is exposed to execution.
pub const LAUNCH_MARKER_FILE: &str = "launch.marker";
/// Hard cap for metadata reads performed during discovery and recovery.
pub const MAX_META_FILE_BYTES: u64 = 64 * 1024;
/// Independent hard cap for the private invocation artifact.
pub const MAX_INVOCATION_FILE_BYTES: u64 = 32 * 1024;
/// Hard cap for persisted workflow source reads during resume.
pub const MAX_SCRIPT_FILE_BYTES: u64 = 1024 * 1024;

/// Mint a fresh workflow `runId` host-side.
///
/// Uses [`uuid::Uuid::now_v7`] (time-ordered) — the same pattern used for
/// thread ids elsewhere in the tree. This runs **outside** the isolate, so it
/// is safe despite the §7 determinism harden that disables time/random inside
/// the workflow script. The returned string is what gets injected read-only as
/// `workflow.runId`.
pub fn mint_run_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Absolute path to `$CODEX_HOME/workflows`.
pub fn workflows_root(codex_home: &Path) -> PathBuf {
    codex_home.join(WORKFLOWS_SUBDIR)
}

/// Absolute path to `$CODEX_HOME/workflows/runs`.
pub fn runs_root(codex_home: &Path) -> PathBuf {
    workflows_root(codex_home).join(RUNS_SUBDIR)
}

/// Create or harden the private workflow roots used for bounded run discovery.
pub fn ensure_private_runs_root(codex_home: &Path) -> io::Result<PathBuf> {
    let workflows_root = workflows_root(codex_home);
    let runs_root = runs_root(codex_home);
    crate::private_fs::prepare_private_directories(
        [workflows_root.as_path(), runs_root.as_path()],
        crate::private_fs::PrivateDirectoryOpenMode::CreateIfMissing,
    )?;
    Ok(runs_root)
}

pub(crate) fn harden_journal_for_read(path: &Path) -> io::Result<()> {
    let Some(run_dir) = path.parent() else {
        return crate::private_fs::harden_existing_file(path);
    };
    let Some(run_id) = run_dir.file_name().and_then(|name| name.to_str()) else {
        return crate::private_fs::harden_existing_file(path);
    };
    let Some(candidate_runs_root) = run_dir.parent() else {
        return crate::private_fs::harden_existing_file(path);
    };
    let Some(candidate_workflows_root) = candidate_runs_root.parent() else {
        return crate::private_fs::harden_existing_file(path);
    };
    let Some(codex_home) = candidate_workflows_root.parent() else {
        return crate::private_fs::harden_existing_file(path);
    };
    if ensure_canonical_run_id(run_id).is_ok()
        && candidate_runs_root
            .file_name()
            .is_some_and(|name| name == RUNS_SUBDIR)
        && candidate_workflows_root
            .file_name()
            .is_some_and(|name| name == WORKFLOWS_SUBDIR)
    {
        let paths = WorkflowRunPaths::new(codex_home, run_id);
        if paths.journal() == path {
            return paths.harden_existing_layout();
        }
    }
    crate::private_fs::harden_existing_file(path)
}

/// Resolved absolute paths for a single workflow run's files.
///
/// Construct with [`WorkflowRunPaths::new`]; every path is derived purely from
/// `codex_home` + `run_id`, so the recorder, resume, and discovery index all
/// resolve the identical layout from a `runId` alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunPaths {
    run_dir: PathBuf,
}

impl WorkflowRunPaths {
    /// Resolve the run directory `$CODEX_HOME/workflows/runs/<run_id>` and its
    /// member file paths. Does not touch the filesystem; call
    /// [`WorkflowRunPaths::create_dir`] or [`WorkflowRunPaths::initialize`] to
    /// materialize the directory.
    pub fn new(codex_home: &Path, run_id: &str) -> Self {
        Self {
            run_dir: runs_root(codex_home).join(run_id),
        }
    }

    /// The run's directory: `$CODEX_HOME/workflows/runs/<run_id>`.
    pub fn run_dir(&self) -> &Path {
        &self.run_dir
    }

    /// Path to `journal.jsonl` (written by the recorder in a later ticket).
    pub fn journal(&self) -> PathBuf {
        self.run_dir.join(JOURNAL_FILE)
    }

    /// Path to `script.js` (the executed program).
    pub fn script(&self) -> PathBuf {
        self.run_dir.join(SCRIPT_FILE)
    }

    /// Path to the host-private canonical invocation arguments.
    pub fn invocation(&self) -> PathBuf {
        self.run_dir.join(INVOCATION_FILE)
    }

    /// Path to `meta.json` (the run-meta projection).
    pub fn meta(&self) -> PathBuf {
        self.run_dir.join(META_FILE)
    }

    /// Path to `progress.json` (a bounded projection, never an event log).
    pub fn progress(&self) -> PathBuf {
        self.run_dir.join(PROGRESS_FILE)
    }

    /// Path to `lease.lock`, whose exclusive advisory lock identifies a live owner.
    pub fn lease(&self) -> PathBuf {
        self.run_dir.join(LEASE_FILE)
    }

    /// Path to the create-only pre-execution exposure marker.
    pub fn launch_marker(&self) -> PathBuf {
        self.run_dir.join(LAUNCH_MARKER_FILE)
    }

    /// Create the run directory tree (idempotent, like `mkdir -p`).
    pub fn create_dir(&self) -> io::Result<()> {
        let runs_root = self
            .run_dir
            .parent()
            .ok_or_else(|| invalid_data("workflow run directory has no runs root"))?;
        let workflows_root = runs_root
            .parent()
            .ok_or_else(|| invalid_data("workflow runs root has no workflows root"))?;
        crate::private_fs::create_run_directory_tree(workflows_root, runs_root, &self.run_dir)
    }

    /// Harden every existing directory and known artifact in this run layout.
    ///
    /// Missing artifacts remain missing. Links, reparse points, foreign-owned
    /// objects, and non-regular artifacts are rejected instead of mutated.
    pub fn harden_existing_layout(&self) -> io::Result<()> {
        let runs_root = self
            .run_dir
            .parent()
            .ok_or_else(|| invalid_data("workflow run directory has no runs root"))?;
        let workflows_root = runs_root
            .parent()
            .ok_or_else(|| invalid_data("workflow runs root has no workflows root"))?;
        crate::private_fs::prepare_private_directories(
            [workflows_root, runs_root, self.run_dir.as_path()],
            crate::private_fs::PrivateDirectoryOpenMode::Existing,
        )?;
        for path in [
            self.script(),
            self.invocation(),
            self.meta(),
            self.progress(),
            self.lease(),
            self.launch_marker(),
            self.journal(),
        ] {
            match crate::private_fs::harden_existing_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// Create the run directory and persist `script.js` + `meta.json` **before**
    /// body execution, so resume can validate the resumed program/args against
    /// the originals (§7 resume algorithm step 1) and so a completed run's
    /// `script.js` is byte-identical to the submitted program (re-invoke by
    /// scriptPath).
    ///
    /// `script` is written verbatim; `meta` is serialized as pretty JSON. The
    /// `journal.jsonl` is intentionally not created here — the recorder owns it.
    pub fn initialize(&self, script: &str, meta: &WorkflowRunMeta) -> io::Result<()> {
        self.create_dir()?;
        crate::private_fs::write_atomically(&self.script(), script.as_bytes())?;
        let meta_json = serde_json::to_string_pretty(meta)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let meta_path = self.meta();
        reject_non_regular_target(&meta_path)?;
        crate::private_fs::write_atomically(&meta_path, meta_json.as_bytes())?;
        Ok(())
    }

    /// Persist the exact canonical invocation bytes used by the run's argument hash.
    ///
    /// The artifact is create-only and owner-readable/writable on Unix. It is
    /// intentionally separate from metadata and progress so it never enters
    /// model-visible context or discovery projections.
    pub fn write_invocation_args(
        &self,
        args: &serde_json::Value,
        expected_hash: &str,
    ) -> io::Result<()> {
        let canonical = canonical_value_json(args);
        if canonical.len() as u64 > MAX_INVOCATION_FILE_BYTES {
            return Err(invalid_data(format!(
                "workflow invocation exceeds the {MAX_INVOCATION_FILE_BYTES}-byte cap"
            )));
        }
        if canonical_value_hash(args) != expected_hash {
            return Err(invalid_data(
                "workflow invocation does not match its durable argument fingerprint",
            ));
        }
        write_private_create_only(&self.invocation(), canonical.as_bytes())
    }

    /// Read and authenticate the host-private canonical invocation arguments.
    pub fn read_invocation_args_bounded(
        &self,
        expected_hash: &str,
    ) -> io::Result<serde_json::Value> {
        self.harden_existing_layout()?;
        let file = open_regular_file_bounded(
            &self.invocation(),
            MAX_INVOCATION_FILE_BYTES,
            "workflow invocation",
        )?;
        ensure_private_permissions(&file)?;
        let bytes = read_open_file_bounded(file, MAX_INVOCATION_FILE_BYTES, "workflow invocation")?;
        let args: serde_json::Value = serde_json::from_slice(&bytes).map_err(invalid_data)?;
        let canonical = canonical_value_json(&args);
        if canonical.as_bytes() != bytes {
            return Err(invalid_data(
                "workflow invocation is not encoded as canonical JSON",
            ));
        }
        if canonical_value_hash(&args) != expected_hash {
            return Err(invalid_data(
                "workflow invocation does not match its durable argument fingerprint",
            ));
        }
        Ok(args)
    }

    /// Read the exact persisted workflow source under a hard cap.
    pub fn read_script_bounded(&self) -> io::Result<String> {
        self.harden_existing_layout()?;
        let file =
            open_regular_file_bounded(&self.script(), MAX_SCRIPT_FILE_BYTES, "workflow script")?;
        let bytes = read_open_file_bounded(file, MAX_SCRIPT_FILE_BYTES, "workflow script")?;
        String::from_utf8(bytes).map_err(invalid_data)
    }

    /// Read `meta.json` without following symlinks or allocating beyond the hard cap.
    pub fn read_meta_bounded(&self) -> io::Result<WorkflowRunMeta> {
        self.harden_existing_layout()?;
        let meta_path = self.meta();
        let file = open_regular_file_bounded(&meta_path, MAX_META_FILE_BYTES, "workflow metadata")?;
        let bytes = read_open_file_bounded(file, MAX_META_FILE_BYTES, "workflow metadata")?;
        serde_json::from_slice(&bytes).map_err(invalid_data)
    }

    /// Read and migrate the private progress projection under a caller cap.
    pub fn read_progress_bounded(&self, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
        self.harden_existing_layout()?;
        match crate::private_fs::read_private_bounded(
            &self.progress(),
            max_bytes,
            "workflow progress",
        ) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Read and migrate at most one caller-bounded prefix of the private journal.
    pub fn read_journal_prefix_bounded(&self, max_bytes: u64) -> io::Result<Option<Vec<u8>>> {
        self.harden_existing_layout()?;
        match crate::private_fs::read_private_prefix(&self.journal(), max_bytes, "workflow journal")
        {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Atomically persist the terminal lifecycle status in `meta.json`.
    ///
    /// This file is the source used to rebuild the SQLite `workflow_runs`
    /// projection, so a terminal transition must survive removal of that index.
    pub fn update_status(&self, status: WorkflowRunStatus) -> io::Result<WorkflowRunMeta> {
        if status == WorkflowRunStatus::Running {
            return Err(invalid_data(
                "workflow terminal status cannot transition back to running",
            ));
        }
        let meta_path = self.meta();
        let mut meta = self.read_meta_bounded()?;
        if meta.status != WorkflowRunStatus::Running {
            return Ok(meta);
        }
        meta.status = status;
        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        reject_non_regular_target(&meta_path)?;
        crate::private_fs::write_atomically(&meta_path, meta_json.as_bytes())?;
        Ok(meta)
    }

    /// Atomically claim the sole successor of a paused source checkpoint.
    ///
    /// The caller must hold this run's lease. The first candidate wins; every
    /// later caller receives that same canonical id. The source remains paused.
    pub fn claim_resume_successor(
        &self,
        lease: &WorkflowRunLease,
        candidate_run_id: &str,
    ) -> io::Result<String> {
        ensure_canonical_run_id(candidate_run_id)?;
        if lease.path() != self.lease() {
            return Err(invalid_data(
                "workflow resume claim requires the source run lease",
            ));
        }

        let meta_path = self.meta();
        let mut meta = self.read_meta_bounded()?;
        if meta.status != WorkflowRunStatus::Paused {
            return Err(invalid_data(
                "workflow resume claim requires a paused source run",
            ));
        }
        if let Some(run_id) = meta.resumed_by_run_id {
            ensure_canonical_run_id(&run_id)?;
            return Ok(run_id);
        }

        meta.resumed_by_run_id = Some(candidate_run_id.to_string());
        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        reject_non_regular_target(&meta_path)?;
        crate::private_fs::write_atomically(&meta_path, meta_json.as_bytes())?;
        Ok(candidate_run_id.to_string())
    }

    /// Read an already-persisted successor claim without acquiring the source
    /// lease. This is used only to join a first writer that currently owns it.
    pub fn read_resume_successor_claim(&self) -> io::Result<Option<String>> {
        let claim = self.read_meta_bounded()?.resumed_by_run_id;
        if let Some(run_id) = claim.as_deref() {
            ensure_canonical_run_id(run_id)?;
        }
        Ok(claim)
    }

    /// Create or authenticate every immutable artifact of a claimed resume successor.
    ///
    /// Existing files must match exactly (apart from a terminal `meta.status`),
    /// so retrying a partial pre-launch initialization can never overwrite or
    /// silently adopt a different run.
    pub fn ensure_resume_successor_artifacts(
        &self,
        script: &str,
        meta: &WorkflowRunMeta,
        args: &serde_json::Value,
    ) -> io::Result<()> {
        if script.len() as u64 > MAX_SCRIPT_FILE_BYTES {
            return Err(invalid_data(format!(
                "workflow script exceeds the {MAX_SCRIPT_FILE_BYTES}-byte cap"
            )));
        }
        self.create_dir()?;
        ensure_exact_file(
            &self.script(),
            script.as_bytes(),
            MAX_SCRIPT_FILE_BYTES,
            "workflow script",
        )?;

        let meta_json = serde_json::to_string_pretty(meta)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        match ensure_exact_file(
            &self.meta(),
            meta_json.as_bytes(),
            MAX_META_FILE_BYTES,
            "workflow metadata",
        ) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                let mut existing = self.read_meta_bounded()?;
                let existing_status = existing.status;
                existing.status = WorkflowRunStatus::Running;
                if existing != *meta || existing_status == WorkflowRunStatus::Running {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }

        match self.write_invocation_args(args, &meta.args_hash) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let existing = self.read_invocation_args_bounded(&meta.args_hash)?;
                if existing == *args {
                    Ok(())
                } else {
                    Err(invalid_data(
                        "workflow invocation does not match the claimed successor",
                    ))
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Authenticate immutable successor artifacts without creating or changing them.
    pub fn validate_resume_successor_artifacts(
        &self,
        script: &str,
        meta: &WorkflowRunMeta,
        args: &serde_json::Value,
    ) -> io::Result<WorkflowRunStatus> {
        if self.read_script_bounded()?.as_bytes() != script.as_bytes() {
            return Err(invalid_data(
                "workflow script does not match the claimed successor",
            ));
        }
        let mut persisted = self.read_meta_bounded()?;
        let status = persisted.status;
        persisted.status = WorkflowRunStatus::Running;
        if persisted != *meta {
            return Err(invalid_data(
                "workflow metadata does not match the claimed successor",
            ));
        }
        if self.read_invocation_args_bounded(&meta.args_hash)? != *args {
            return Err(invalid_data(
                "workflow invocation does not match the claimed successor",
            ));
        }
        Ok(status)
    }

    /// Validate an existing journal's line-zero identity and report whether it
    /// already contains body records. A missing journal is a safe pre-launch partial.
    pub fn journal_state(&self, expected: &WorkflowRunMeta) -> io::Result<WorkflowJournalState> {
        self.harden_existing_layout()?;
        let file = match open_regular_file_bounded(
            &self.journal(),
            crate::WORKFLOW_JOURNAL_MAX_BYTES,
            "workflow journal",
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(WorkflowJournalState::Missing);
            }
            Err(error) => return Err(error),
        };
        let journal_len = file.metadata()?.len();
        if journal_len == 0 {
            return Ok(WorkflowJournalState::Empty);
        }
        let expected_header = serde_json::to_vec(expected).map_err(invalid_data)?;
        if expected_header.len() > crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES {
            return Err(invalid_data(
                "workflow journal line-zero record exceeds its byte cap",
            ));
        }
        let mut reader = BufReader::new(file);
        let mut header = Vec::new();
        reader
            .by_ref()
            .take(crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES as u64 + 2)
            .read_until(b'\n', &mut header)?;
        if header.last() != Some(&b'\n') {
            if !header.is_empty()
                && header.len() <= expected_header.len()
                && expected_header.starts_with(&header)
                && journal_len == header.len() as u64
            {
                return Ok(WorkflowJournalState::PartialHeader);
            }
            return Err(invalid_data(
                "workflow journal has no bounded line-zero record",
            ));
        }
        if header.len() > crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES + 1 {
            return Err(invalid_data(
                "workflow journal has no bounded line-zero record",
            ));
        }
        header.pop();
        let actual: WorkflowRunMeta = serde_json::from_slice(&header).map_err(invalid_data)?;
        if actual != *expected {
            return Err(invalid_data(
                "workflow journal line zero does not match the claimed successor",
            ));
        }
        let mut next = [0_u8; 1];
        if reader.read(&mut next)? == 0 {
            Ok(WorkflowJournalState::HeaderOnly)
        } else {
            Ok(WorkflowJournalState::HasBody)
        }
    }

    /// Replace only an authenticated torn prefix of the expected line zero.
    ///
    /// The caller owns run admission. This revalidates the exact prefix on the
    /// same non-following file handle immediately before truncation; mismatched,
    /// overlong, newline-terminated, or body-bearing bytes fail closed.
    pub(crate) fn repair_partial_journal_header(
        &self,
        expected: &WorkflowRunMeta,
    ) -> io::Result<()> {
        let expected_header = serde_json::to_vec(expected).map_err(invalid_data)?;
        if expected_header.len() > crate::WORKFLOW_JOURNAL_RECORD_MAX_BYTES {
            return Err(invalid_data(
                "workflow journal line-zero record exceeds its byte cap",
            ));
        }
        let path = self.journal();
        let mut file = crate::private_fs::open_private_read_write(
            &path,
            crate::private_fs::PrivateFileOpenMode::Existing,
            "workflow journal",
        )?;
        let mut actual = Vec::new();
        (&mut file)
            .take(expected_header.len() as u64 + 1)
            .read_to_end(&mut actual)?;
        if actual.is_empty()
            || actual.len() > expected_header.len()
            || !expected_header.starts_with(&actual)
            || actual.contains(&b'\n')
            || file.metadata()?.len() != actual.len() as u64
        {
            return Err(invalid_data(
                "workflow journal is not an exact torn line-zero prefix",
            ));
        }
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&expected_header)?;
        file.write_all(b"\n")?;
        file.sync_all()
    }

    /// Return whether execution exposure was already committed for this successor.
    pub fn was_execution_launched(&self) -> io::Result<bool> {
        self.harden_existing_layout()?;
        match open_regular_file_bounded(&self.launch_marker(), 0, "workflow launch marker") {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Commit the pre-execution exposure boundary exactly once.
    pub fn mark_execution_launched(&self) -> io::Result<()> {
        write_private_create_only(&self.launch_marker(), &[])
    }

    /// Atomically persist the bounded live-progress projection with private permissions.
    pub fn write_progress_atomically(&self, json: &str) -> io::Result<()> {
        self.create_dir()?;
        crate::private_fs::write_atomically(&self.progress(), json.as_bytes())
    }
}

/// Existing durable state of a claimed successor's journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowJournalState {
    Missing,
    Empty,
    PartialHeader,
    HeaderOnly,
    HasBody,
}

fn ensure_canonical_run_id(run_id: &str) -> io::Result<()> {
    let parsed = uuid::Uuid::parse_str(run_id)
        .map_err(|_| invalid_data("workflow run id is not canonical"))?;
    if parsed.to_string() != run_id {
        return Err(invalid_data("workflow run id is not canonical"));
    }
    Ok(())
}

fn open_regular_file_bounded(path: &Path, max_bytes: u64, label: &str) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;

        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if is_link_or_reparse_point(&metadata) || !metadata.file_type().is_file() {
        return Err(invalid_data(format!(
            "{label} is not a regular, non-symlink file"
        )));
    }
    if metadata.len() > max_bytes {
        return Err(invalid_data(format!(
            "{label} exceeds the {max_bytes}-byte cap"
        )));
    }
    Ok(file)
}

fn read_open_file_bounded(file: File, max_bytes: u64, label: &str) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(invalid_data(format!(
            "{label} exceeds the {max_bytes}-byte cap"
        )));
    }
    Ok(bytes)
}

fn ensure_exact_file(path: &Path, expected: &[u8], max_bytes: u64, label: &str) -> io::Result<()> {
    match open_regular_file_bounded(path, max_bytes, label) {
        Ok(file) => {
            if read_open_file_bounded(file, max_bytes, label)? == expected {
                Ok(())
            } else {
                Err(invalid_data(format!(
                    "{label} does not match the claimed successor"
                )))
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match write_private_create_only(path, expected) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let file = open_regular_file_bounded(path, max_bytes, label)?;
                    if read_open_file_bounded(file, max_bytes, label)? == expected {
                        Ok(())
                    } else {
                        Err(invalid_data(format!(
                            "{label} does not match the claimed successor"
                        )))
                    }
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}
fn reject_non_regular_target(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => Err(invalid_data(
            "workflow metadata target is not a regular, non-symlink file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn invalid_data(error: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
