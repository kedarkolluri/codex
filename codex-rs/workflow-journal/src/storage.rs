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
//!   meta.json       # run-meta projection (discovery-index rebuild source, §7)
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
use std::io;
use std::path::Path;
use std::path::PathBuf;

use crate::WorkflowRunMeta;

/// Subdirectory under `$CODEX_HOME` holding all workflow state.
pub const WORKFLOWS_SUBDIR: &str = "workflows";
/// Subdirectory under `$CODEX_HOME/workflows` holding per-run directories.
pub const RUNS_SUBDIR: &str = "runs";

/// Filename of the append-only journal (source of truth for replay).
pub const JOURNAL_FILE: &str = "journal.jsonl";
/// Filename of the persisted executed program.
pub const SCRIPT_FILE: &str = "script.js";
/// Filename of the run-meta projection.
pub const META_FILE: &str = "meta.json";

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

    /// Path to `meta.json` (the run-meta projection).
    pub fn meta(&self) -> PathBuf {
        self.run_dir.join(META_FILE)
    }

    /// Create the run directory tree (idempotent, like `mkdir -p`).
    pub fn create_dir(&self) -> io::Result<()> {
        fs::create_dir_all(&self.run_dir)
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
        fs::write(self.script(), script)?;
        let meta_json = serde_json::to_string_pretty(meta)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(self.meta(), meta_json)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunMetaTag;
    use pretty_assertions::assert_eq;
    use uuid::Uuid;

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

    #[test]
    fn paths_resolve_from_run_id_and_honour_codex_home_override() {
        let codex_home = Path::new("/tmp/some-codex-home");
        let run_id = "0190a000-0000-7000-8000-000000000000";
        let paths = WorkflowRunPaths::new(codex_home, run_id);

        let expected_dir = codex_home.join("workflows").join("runs").join(run_id);
        assert_eq!(paths.run_dir(), expected_dir.as_path());
        assert_eq!(paths.journal(), expected_dir.join("journal.jsonl"));
        assert_eq!(paths.script(), expected_dir.join("script.js"));
        assert_eq!(paths.meta(), expected_dir.join("meta.json"));

        // A different CODEX_HOME relocates the whole tree (tempdir-isolatable).
        let other = Path::new("/var/other-home");
        let other_paths = WorkflowRunPaths::new(other, run_id);
        assert_eq!(
            other_paths.run_dir(),
            other.join("workflows").join("runs").join(run_id).as_path()
        );
        assert_ne!(paths.run_dir(), other_paths.run_dir());
    }

    #[test]
    fn runs_root_matches_layout() {
        let codex_home = Path::new("/home/u/.codex");
        assert_eq!(
            runs_root(codex_home),
            codex_home.join("workflows").join("runs")
        );
        assert_eq!(workflows_root(codex_home), codex_home.join("workflows"));
    }

    #[test]
    fn create_dir_is_idempotent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        assert!(!paths.run_dir().exists());
        paths.create_dir().expect("first create");
        assert!(paths.run_dir().is_dir());
        // Second call must not error.
        paths.create_dir().expect("second create");
        assert!(paths.run_dir().is_dir());
    }

    #[test]
    fn initialize_writes_script_and_meta_before_body() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run_id = mint_run_id();
        let paths = WorkflowRunPaths::new(tmp.path(), &run_id);
        let script = "export const meta = { name: 'triage' };\nawait agent('go');\n";
        let meta = sample_meta(&run_id);

        paths.initialize(script, &meta).expect("initialize");

        // Directory and both up-front files exist; journal is NOT created here.
        assert!(paths.run_dir().is_dir());
        assert!(paths.script().is_file());
        assert!(paths.meta().is_file());
        assert!(!paths.journal().exists());

        // script.js is byte-identical to the submitted program.
        let on_disk = fs::read(paths.script()).expect("read script");
        assert_eq!(on_disk, script.as_bytes());

        // meta.json round-trips back to the same WorkflowRunMeta.
        let meta_bytes = fs::read_to_string(paths.meta()).expect("read meta");
        let parsed: WorkflowRunMeta = serde_json::from_str(&meta_bytes).expect("parse meta");
        assert_eq!(parsed, meta);
        assert_eq!(parsed.kind, RunMetaTag::RunMeta);
    }

    #[test]
    fn mint_run_id_is_unique_uuid_v7() {
        let a = mint_run_id();
        let b = mint_run_id();
        assert_ne!(a, b);
        let parsed = Uuid::parse_str(&a).expect("valid uuid");
        assert_eq!(parsed.get_version_num(), 7);
    }
}
