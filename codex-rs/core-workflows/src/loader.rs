use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

use codex_code_mode_protocol::parse_workflow_meta;
use tokio::io::AsyncReadExt;
use tracing::debug;
use tracing::warn;

use crate::model::WorkflowLoadError;
use crate::model::WorkflowMetadata;
use crate::model::WorkflowRoot;
use crate::model::WorkflowScope;

/// Sub-directory name that holds saved workflows under a config/home root.
pub const WORKFLOWS_DIR_NAME: &str = "workflows";

/// Guard against pathological directory trees; workflow roots are shallow in
/// practice, this only bounds adversarial nesting.
const MAX_DISCOVERY_DEPTH: usize = 8;

/// Upper bound on how many leading bytes of a candidate file are read during
/// discovery.
///
/// `parse_workflow_meta` only ever inspects the leading `export const meta =
/// { ... }` manifest, and caps its own scan at 256 KiB (its private
/// `MAX_MANIFEST_BYTES`). Reading a bounded prefix — that cap plus a little
/// slack for the closing tokens/whitespace the parser needs to reach its own
/// guard — means an adversarial multi-gigabyte workflow body is never
/// materialized into memory during discovery.
///
/// A file whose `meta` region genuinely exceeds the manifest cap fails open:
/// `parse_workflow_meta` returns a "manifest is too large" error and the file
/// is skipped-with-recorded-error, exactly as if it had been read in full.
const MAX_META_READ_BYTES: u64 = 256 * 1024 + 4 * 1024;

/// Extra bytes read *past* [`MAX_META_READ_BYTES`] so that a multi-byte UTF-8
/// character straddling the read boundary can be **validated** rather than
/// assumed valid.
///
/// A UTF-8 scalar is at most four bytes, so a lead byte sitting on the final
/// capped byte needs at most three continuation bytes to complete. Reading that
/// small look-ahead lets [`decode_meta_prefix`] distinguish a genuinely clipped
/// *valid* character (tolerate) from a file that merely ends in a malformed or
/// invalidly-continued sequence (reject). Only the leading `MAX_META_READ_BYTES`
/// are ever returned; the look-ahead is inspected and discarded.
const UTF8_MAX_LOOKAHEAD: u64 = 3;

/// Maximum number of candidate `*.js` files considered from a single root per
/// discovery pass.
///
/// Discovery walks potentially untrusted repositories; without a cap a root
/// containing millions of `*.js` files would force millions of directory-entry
/// stats, path allocations, and file reads on every re-discovery. The cap is
/// therefore enforced **during** the traversal (see [`collect_workflow_files`]):
/// the walk stops accumulating once `cap + 1` candidates have been gathered — the
/// `+ 1` lets the caller detect that more existed without ever materializing
/// them.
///
/// This cap bounds only the number of **file candidates** carried forward; it is
/// deliberately *not* applied to the sub-directory fan-out (a sub-directory is
/// not itself a candidate, so capping the sub-directory set by the file limit
/// could drop a subtree that holds the only candidates). Raw per-directory work
/// is bounded separately by [`MAX_DIR_ENTRIES`].
///
/// The surviving set is deterministic: entries within each directory are visited
/// in lexicographically sorted order and a directory's own files are visited
/// before its subdirectories (a sorted pre-order walk), so the `cap` files that
/// survive are exactly the lexicographically-first `cap` candidates in that walk
/// order. When a root exceeds the cap the drop is recorded in
/// [`WorkflowRegistry::errors`] — never silently discarded.
const MAX_WORKFLOW_FILES_PER_ROOT: usize = 256;

/// Hard ceiling on how many **raw** directory entries a single directory level is
/// enumerated (and `file_type`-stat-ed) before the walk gives up on that
/// directory with a recorded "directory too large" diagnostic.
///
/// This is DISTINCT from [`MAX_WORKFLOW_FILES_PER_ROOT`]: that cap bounds the
/// deterministic surviving *candidate* set (the sorted-smallest files), while
/// this bounds the raw enumeration *work* so a single flat directory holding
/// millions of entries cannot force millions of `next_entry`/`file_type`
/// syscalls. A directory with at most this many entries is enumerated in full,
/// so its sorted surviving set stays fully deterministic; only a directory that
/// exceeds this ceiling loses that determinism guarantee (the survivors become
/// whichever entries the unsorted readdir stream yielded before the ceiling), and
/// that loss is always recorded in [`WorkflowRegistry::errors`], never silent. It
/// is intentionally generous — far above any legitimate workflow directory —
/// because its only job is to defang an adversarial fan-out.
const MAX_DIR_ENTRIES: usize = 16_384;

/// Assemble the precedence-ordered list of workflow roots (highest precedence
/// first), mirroring how `core-skills` assembles roots from the config layer
/// stack, but using the fixed roots from spec §9:
///
/// 1. `<repo>/.codex/workflows` (project-scoped, checked in — recommended default)
/// 2. `$HOME/.agents/workflows` (personal)
/// 3. `$CODEX_HOME/workflows`
///
/// Missing inputs are simply skipped; non-existent directories are tolerated by
/// discovery, so callers may pass roots optimistically.
pub fn workflow_roots(
    repo_root: Option<&Path>,
    home_dir: Option<&Path>,
    codex_home: Option<&Path>,
) -> Vec<WorkflowRoot> {
    let mut roots = Vec::new();
    if let Some(repo_root) = repo_root {
        roots.push(WorkflowRoot::new(
            repo_root.join(".codex").join(WORKFLOWS_DIR_NAME),
            WorkflowScope::Project,
        ));
    }
    if let Some(home_dir) = home_dir {
        roots.push(WorkflowRoot::new(
            home_dir.join(".agents").join(WORKFLOWS_DIR_NAME),
            WorkflowScope::Personal,
        ));
    }
    if let Some(codex_home) = codex_home {
        roots.push(WorkflowRoot::new(
            codex_home.join(WORKFLOWS_DIR_NAME),
            WorkflowScope::CodexHome,
        ));
    }
    roots
}

/// Registry of discovered workflows with precedence already applied.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkflowRegistry {
    workflows: Vec<WorkflowMetadata>,
    errors: Vec<WorkflowLoadError>,
}

impl WorkflowRegistry {
    /// Discovered workflows, de-duplicated by name with scope precedence
    /// applied, sorted by name for deterministic listing.
    pub fn workflows(&self) -> &[WorkflowMetadata] {
        &self.workflows
    }

    /// Files that were skipped during discovery (fail-open), for diagnostics.
    pub fn errors(&self) -> &[WorkflowLoadError] {
        &self.errors
    }

    /// Look up the highest-precedence workflow for `name`. Returns the entry and
    /// its absolute script path (via [`WorkflowMetadata::path`]).
    pub fn resolve_by_name(&self, name: &str) -> Option<&WorkflowMetadata> {
        self.workflows.iter().find(|workflow| workflow.name == name)
    }

    /// Names of the discovered workflows, in listing order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.workflows.iter().map(|workflow| workflow.name.as_str())
    }
}

/// Discover saved workflows across the given precedence-ordered `roots`.
///
/// For each `*.js` candidate, only the leading `export const meta = {...}`
/// literal is statically parsed (via `parse_workflow_meta`); the body is NEVER
/// executed. Files whose `meta` is missing/malformed/non-literal are skipped and
/// recorded in [`WorkflowRegistry::errors`] rather than aborting discovery
/// (fail-open, mirroring `load_skill_metadata`).
///
/// Same-name workflows are de-duplicated with scope precedence: the entry from
/// the earliest (highest-precedence) root wins, so a project workflow shadows a
/// personal or `$CODEX_HOME` one of the same name.
pub async fn load_workflows_from_roots<I>(roots: I) -> WorkflowRegistry
where
    I: IntoIterator<Item = WorkflowRoot>,
{
    let mut workflows: Vec<WorkflowMetadata> = Vec::new();
    let mut errors: Vec<WorkflowLoadError> = Vec::new();
    // Track names already claimed by a higher-precedence scope.
    let mut seen_names: HashSet<String> = HashSet::new();
    // Track physical files already visited (canonical path) so the same file
    // reached via two roots (or a symlink) is not counted twice.
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();

    for root in roots {
        // The cap is enforced *inside* the walk: it collects at most
        // `MAX_WORKFLOW_FILES_PER_ROOT + 1` candidates in a deterministic sorted
        // pre-order and stops accumulating there. The extra `+ 1` is a sentinel
        // that lets us detect an over-cap root without ever materializing (or
        // stat-ing/opening) the excess files.
        let mut candidates = Vec::new();
        collect_workflow_files(
            &root.path,
            root.scope,
            /*depth*/ 0,
            MAX_WORKFLOW_FILES_PER_ROOT,
            &mut candidates,
            &mut errors,
        )
        .await;

        // If the sentinel slot filled, more candidates existed than the cap.
        // Truncate to the cap and record the drop (never silent). Because the
        // walk stopped early we cannot (and must not, for boundedness) know the
        // exact overflow count, so the diagnostic uses a "more than N" marker.
        if candidates.len() > MAX_WORKFLOW_FILES_PER_ROOT {
            candidates.truncate(MAX_WORKFLOW_FILES_PER_ROOT);
            warn!(
                root = %root.path.display(),
                cap = MAX_WORKFLOW_FILES_PER_ROOT,
                "workflow root exceeds per-root candidate cap; higher-sorted files ignored",
            );
            errors.push(WorkflowLoadError {
                path: root.path.clone(),
                message: format!(
                    "workflow root has more than {MAX_WORKFLOW_FILES_PER_ROOT} candidate `*.js` \
                     files; only the {MAX_WORKFLOW_FILES_PER_ROOT} lowest-sorted were considered \
                     this discovery pass and the remaining higher-sorted file(s) were ignored"
                ),
            });
        }

        for path in candidates {
            let canonical = tokio::fs::canonicalize(&path)
                .await
                .unwrap_or_else(|_| path.clone());
            if !seen_paths.insert(canonical) {
                continue;
            }

            let source = match read_meta_prefix(&path).await {
                Ok(source) => source,
                Err(error) => {
                    debug!(path = %path.display(), %error, "skipping unreadable workflow file");
                    errors.push(WorkflowLoadError {
                        path: path.clone(),
                        message: format!("failed to read workflow file: {error}"),
                    });
                    continue;
                }
            };

            // Fail-open: only the static `meta` literal is parsed; the body is
            // never executed. Any parse failure skips this file.
            let meta = match parse_workflow_meta(&source) {
                Ok(meta) => meta,
                Err(message) => {
                    debug!(path = %path.display(), %message, "skipping workflow with invalid meta");
                    errors.push(WorkflowLoadError {
                        path: path.clone(),
                        message,
                    });
                    continue;
                }
            };

            // Scope precedence dedupe by name: first (highest precedence) wins.
            if !seen_names.insert(meta.name.clone()) {
                continue;
            }

            workflows.push(WorkflowMetadata {
                name: meta.name,
                description: meta.description,
                phases: meta.phases,
                path,
                scope: root.scope,
            });
        }
    }

    workflows.sort_by(|a, b| a.name.cmp(&b.name));
    WorkflowRegistry { workflows, errors }
}

/// Read at most [`MAX_META_READ_BYTES`] leading bytes of a candidate workflow
/// file.
///
/// Only the `meta` manifest region is needed by [`parse_workflow_meta`], so a
/// huge (or multi-gigabyte) workflow body is never fully materialized. The bytes
/// are decoded as **strict** UTF-8 (see [`decode_meta_prefix`]): tolerating an
/// arbitrary replacement character would let an invalid byte inside `meta.name`
/// silently enter the registry as U+FFFD even though a later strict load of the
/// same file rejects it. The ONLY tolerance is for a single multi-byte sequence
/// clipped at the exact read boundary, which can only ever land past the end of
/// the tiny leading manifest.
async fn read_meta_prefix(path: &Path) -> std::io::Result<String> {
    let file = tokio::fs::File::open(path).await?;
    let mut buf = Vec::new();
    // Read a few bytes *past* the cap (see [`UTF8_MAX_LOOKAHEAD`]) so a multi-byte
    // character straddling the cap boundary can be validated rather than assumed
    // valid. Whether the file actually extends beyond the cap is derived from
    // whether those look-ahead bytes materialized; only the leading cap bytes are
    // ever returned to the caller.
    file.take(MAX_META_READ_BYTES + UTF8_MAX_LOOKAHEAD)
        .read_to_end(&mut buf)
        .await?;
    decode_meta_prefix(&buf)
}

/// Decode a bounded workflow prefix as strict UTF-8.
///
/// A well-formed workflow file is valid UTF-8, so `str::from_utf8` succeeds and
/// the whole prefix is returned. Because [`read_meta_prefix`] stops at exactly
/// [`MAX_META_READ_BYTES`], a multi-byte character may be split across that
/// boundary; that single trailing partial sequence is dropped (the prefix is
/// truncated at the last complete character). It can only ever sit past the end
/// of the leading `meta` manifest, which the parser needs intact.
///
/// Any OTHER invalid UTF-8 — a bad byte *inside* the manifest region, or an
/// invalid/continuation byte mid-stream — is a genuine decode failure surfaced
/// as [`std::io::ErrorKind::InvalidData`] so the caller skips the file with a
/// recorded error, rather than smuggling a U+FFFD replacement char into
/// `meta.name`.
///
/// The passed `buf` is the leading cap bytes plus up to [`UTF8_MAX_LOOKAHEAD`]
/// look-ahead bytes (see [`read_meta_prefix`]). The look-ahead is what lets this
/// function *prove* a trailing incomplete sequence is a genuine boundary clip of
/// a valid character rather than assume it: a file whose bytes end exactly at the
/// cap in an incomplete lead byte, or whose next byte is an invalid continuation,
/// is rejected — only a sequence that the withheld bytes complete into a valid
/// scalar is tolerated.
fn decode_meta_prefix(buf: &[u8]) -> std::io::Result<String> {
    let cap = MAX_META_READ_BYTES as usize;
    // The logical prefix is at most `cap` bytes; anything past that is look-ahead
    // used only to validate a character straddling the cap boundary, never
    // returned. `has_lookahead` therefore also tells us the file genuinely
    // extends beyond the cap (the read was truncated), not merely ended at it.
    let prefix_len = buf.len().min(cap);
    let prefix = &buf[..prefix_len];
    let has_lookahead = buf.len() > prefix_len;
    match std::str::from_utf8(prefix) {
        Ok(text) => Ok(text.to_owned()),
        Err(error) => {
            // `error_len() == Some(_)` is a concretely invalid sequence *inside*
            // the prefix — genuine corruption, never a boundary clip. Only
            // `error_len() == None` (an incomplete trailing run, a valid prefix
            // of a multi-byte char) is a clip candidate, and only when (a) the
            // read was actually truncated at the cap so the continuation bytes
            // exist past the prefix, AND (b) those withheld bytes actually
            // complete the straddling char into a valid scalar. We never assume
            // the unseen bytes are valid — we inspect the look-ahead.
            let valid = error.valid_up_to();
            let clipped_valid_char = error.error_len().is_none()
                && has_lookahead
                && straddling_char_is_valid(&buf[valid..]);
            if clipped_valid_char {
                // `prefix[..valid]` ends on a char boundary and is valid UTF-8 by
                // construction, so this lossy decode produces no replacement
                // characters; the clipped char lies past the tiny manifest.
                Ok(String::from_utf8_lossy(&prefix[..valid]).into_owned())
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "workflow file is not valid UTF-8",
                ))
            }
        }
    }
}

/// Whether `bytes` (a character straddling the read boundary: its leading bytes
/// from the capped prefix followed by the look-ahead read past the cap) begins
/// with a complete, valid UTF-8 scalar.
///
/// Returns `true` when the first scalar decodes successfully (even if a later
/// look-ahead byte is bad), and `false` when the very first byte run is malformed
/// — i.e. the straddling character was not a genuinely clipped valid char.
fn straddling_char_is_valid(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        // Whole slice valid: the straddling char (and any trailing look-ahead)
        // decoded cleanly.
        Ok(text) => !text.is_empty(),
        // `valid_up_to() > 0`: the first scalar decoded before the error, so the
        // straddling char itself is valid. `valid_up_to() == 0`: the straddling
        // char is malformed (e.g. an invalid continuation byte).
        Err(error) => error.valid_up_to() > 0,
    }
}

/// Outcome of scanning a single directory level (see [`scan_directory`]).
struct DirScan {
    /// Lexicographically-sorted workflow files in this directory, bounded to the
    /// `files_cap` smallest (the only set the candidate cap applies to).
    files: Vec<PathBuf>,
    /// Lexicographically-sorted sub-directories. NOT bounded by the candidate
    /// cap — a sub-directory is not itself a candidate, and bounding it by the
    /// file limit could drop a subtree that holds the only candidates. Bounded
    /// only by the raw enumeration ceiling ([`MAX_DIR_ENTRIES`]).
    subdirs: Vec<PathBuf>,
    /// How many raw entries were enumerated/stat-ed. Always `<= MAX_DIR_ENTRIES`;
    /// used to prove the per-directory work bound in tests.
    enumerated: usize,
    /// `true` when the directory held more than [`MAX_DIR_ENTRIES`] raw entries so
    /// enumeration stopped at the ceiling. The surviving set for this directory is
    /// then no longer deterministic and the caller records a diagnostic.
    truncated: bool,
}

/// Scan a single directory level: enumerate its entries into a sorted, bounded
/// file bucket and a sorted sub-directory list, stopping raw enumeration at
/// `raw_ceiling` entries so a pathological flat directory of millions of entries
/// never forces millions of `next_entry`/`file_type` syscalls.
///
/// Returns `None` when the directory cannot be opened (missing directories are
/// tolerated silently). The `raw_ceiling` parameter is threaded through (rather
/// than reading the constant directly) so tests can drive the short-circuit with
/// a small ceiling and instrument the enumeration count without materializing
/// [`MAX_DIR_ENTRIES`] entries on disk.
async fn scan_directory(dir: &Path, files_cap: usize, raw_ceiling: usize) -> Option<DirScan> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(_) => return None,
    };
    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    let mut enumerated = 0usize;
    let mut truncated = false;
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(_) => break,
        };
        // Short-circuit raw enumeration at the hard ceiling *before* doing any
        // per-entry work (path alloc + `file_type` stat). A flat directory with
        // millions of entries therefore costs at most `raw_ceiling` stats, not
        // one per entry. Distinct from the candidate cap applied to `files`.
        if enumerated >= raw_ceiling {
            truncated = true;
            break;
        }
        enumerated += 1;
        let path = entry.path();
        let file_type = match entry.file_type().await {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            // Unbounded by the candidate cap; bounded only by `raw_ceiling` via
            // the short-circuit above (so `subdirs.len() <= raw_ceiling`).
            subdirs.push(path);
        } else if file_type.is_file() && is_workflow_script(&path) {
            // Only the FILE candidate set is bounded by the candidate cap.
            push_bounded(&mut files, path, files_cap);
        }
    }
    // `files` is kept sorted by `push_bounded`; sort `subdirs` for a deterministic
    // pre-order walk.
    subdirs.sort();
    Some(DirScan {
        files,
        subdirs,
        enumerated,
        truncated,
    })
}

/// Collect up to `limit + 1` candidate `*.js` files (which includes
/// `*.workflow.js`) under `dir`, enforcing the per-root candidate bound **during**
/// the walk so an adversarial tree of millions of files never forces millions of
/// path allocations or file reads.
///
/// Determinism: each directory level is scanned (see [`scan_directory`]) into a
/// lexicographically-sorted, candidate-capped file bucket and a
/// lexicographically-sorted sub-directory list; the level's own files are
/// appended before recursing into its sub-directories (a sorted pre-order walk).
/// The walk short-circuits as soon as `out` holds `limit + 1` entries — the
/// sentinel slot lets the caller detect an over-cap root without materializing
/// the excess.
///
/// The candidate cap bounds only the FILE bucket; sub-directories are followed in
/// full (in sorted order) until the global candidate budget is hit, so a workflow
/// buried under many empty sibling sub-directories is still discovered. Raw
/// per-directory work is bounded instead by [`MAX_DIR_ENTRIES`]; a directory that
/// exceeds it is truncated with a recorded diagnostic (never silent) via
/// `errors`.
///
/// Missing directories are tolerated silently.
async fn collect_workflow_files(
    dir: &Path,
    scope: WorkflowScope,
    depth: usize,
    limit: usize,
    out: &mut Vec<PathBuf>,
    errors: &mut Vec<WorkflowLoadError>,
) {
    // Already gathered the sentinel; stop descending (and stop stat-ing).
    if out.len() > limit {
        return;
    }
    if depth > MAX_DISCOVERY_DEPTH {
        return;
    }
    let files_cap = limit.saturating_add(1);
    let Some(scan) = scan_directory(dir, files_cap, MAX_DIR_ENTRIES).await else {
        return;
    };
    debug!(
        dir = %dir.display(),
        enumerated = scan.enumerated,
        "scanned workflow directory",
    );
    if scan.truncated {
        warn!(
            dir = %dir.display(),
            ceiling = MAX_DIR_ENTRIES,
            "workflow directory exceeds raw entry ceiling; enumeration truncated",
        );
        errors.push(WorkflowLoadError {
            path: dir.to_path_buf(),
            message: format!(
                "workflow directory has more than {MAX_DIR_ENTRIES} entries; raw enumeration \
                 was truncated at that ceiling this discovery pass and any remaining entries in \
                 this directory (files and sub-directories alike) were not scanned"
            ),
        });
    }
    // `scan.files`/`scan.subdirs` are already sorted ascending.
    for file in scan.files {
        if out.len() > limit {
            return;
        }
        out.push(file);
    }
    for subdir in scan.subdirs {
        if out.len() > limit {
            return;
        }
        // `$CODEX_HOME/workflows/runs` is durable execution state, not a
        // saved-workflow source. Its persisted `script.js` files must never
        // re-enter discovery. A project or personal directory named `runs`,
        // and a nested Codex-home directory with that name, remain ordinary
        // saved-workflow directories.
        if scope == WorkflowScope::CodexHome
            && depth == 0
            && subdir
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("runs"))
        {
            continue;
        }
        Box::pin(collect_workflow_files(
            &subdir,
            scope,
            depth + 1,
            limit,
            out,
            errors,
        ))
        .await;
    }
}

/// Insert `path` into `sorted` while keeping it ascending and holding at most
/// `cap` of the lexicographically-smallest paths seen. Larger paths are dropped
/// once the bucket is full, so the bucket never grows past `cap` no matter how
/// many entries stream through it.
fn push_bounded(sorted: &mut Vec<PathBuf>, path: PathBuf, cap: usize) {
    if cap == 0 {
        return;
    }
    match sorted.binary_search(&path) {
        // Duplicate path (should not happen within one directory listing).
        Ok(_) => {}
        Err(idx) => {
            if sorted.len() < cap {
                sorted.insert(idx, path);
            } else if idx < cap {
                // Bucket full but `path` sorts before the current max: make room.
                sorted.pop();
                sorted.insert(idx, path);
            }
            // else: bucket full and `path` sorts at/after every kept entry — drop.
        }
    }
}

/// A workflow script is any `*.js` file (this also matches `*.workflow.js`).
fn is_workflow_script(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("js"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CAP: usize = MAX_META_READ_BYTES as usize;

    #[test]
    fn decode_meta_prefix_accepts_valid_utf8() {
        let text = decode_meta_prefix(b"export const meta = {};\n").unwrap();
        assert_eq!(text, "export const meta = {};\n");
    }

    #[test]
    fn decode_meta_prefix_tolerates_multibyte_clipped_at_read_boundary() {
        // A genuine boundary clip: the prefix is exactly the read cap and ends on
        // the lead byte of a two-byte `é` (0xC3 0xA9), and the read-ahead byte
        // (the 0xA9 continuation, past the cap) is present — proving the withheld
        // byte completes the straddling char into a valid scalar. The clipped
        // char is dropped and the valid prefix is kept.
        let mut buf = vec![b'a'; CAP - 1];
        buf.push(0xC3); // lead byte at index CAP-1 (last capped byte)
        buf.push(0xA9); // continuation byte read as look-ahead (index CAP)
        assert_eq!(buf.len(), CAP + 1);

        let text = decode_meta_prefix(&buf).unwrap();
        // Truncated at the last complete character: all `a`s, no replacement char.
        assert_eq!(text.len(), CAP - 1);
        assert!(text.chars().all(|c| c == 'a'));
    }

    #[test]
    fn decode_meta_prefix_rejects_incomplete_lead_byte_at_exact_eof() {
        // A file whose bytes end EXACTLY at the cap in an incomplete lead byte,
        // with NO look-ahead available (buf.len() == CAP). Buffer length equal to
        // the cap does not prove a clip — the file simply ended malformed, so it
        // must be rejected rather than accepted-and-truncated.
        let mut buf = vec![b'a'; CAP - 1];
        buf.push(0xC3); // incomplete two-byte lead, but no more bytes exist
        assert_eq!(buf.len(), CAP);

        let error = decode_meta_prefix(&buf).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_meta_prefix_rejects_clip_with_invalid_continuation_lookahead() {
        // The prefix ends on a valid two-byte lead (0xC3) at the cap, but the
        // read-ahead byte past the cap is NOT a valid continuation (0x28 == '(').
        // The straddling char is therefore malformed in the real file, so we must
        // not assume the unseen byte is valid — reject.
        let mut buf = vec![b'a'; CAP - 1];
        buf.push(0xC3); // lead byte at index CAP-1
        buf.push(0x28); // invalid continuation read as look-ahead (index CAP)
        assert_eq!(buf.len(), CAP + 1);

        let error = decode_meta_prefix(&buf).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_meta_prefix_rejects_invalid_byte_in_manifest_region() {
        // An invalid byte in the middle of the manifest (not at the boundary)
        // must fail rather than smuggle in a U+FFFD replacement character.
        let mut buf = b"export const meta = { name: '".to_vec();
        buf.push(0xFF); // lone invalid byte
        buf.extend_from_slice(b"', description: 'x' };\n");

        let error = decode_meta_prefix(&buf).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn decode_meta_prefix_rejects_incomplete_sequence_not_at_boundary() {
        // A short buffer (well under the read cap) ending in a partial multi-byte
        // sequence is a genuinely malformed file, not a boundary clip — reject it.
        let buf = vec![b'a', 0xC3];
        assert!(buf.len() < CAP);

        let error = decode_meta_prefix(&buf).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn read_meta_prefix_derives_boundary_clip_from_actual_file_length() {
        // End-to-end of the read path: a file of exactly CAP bytes ending in an
        // incomplete lead byte is rejected (no look-ahead materializes because the
        // file ends at the cap), while a longer file whose straddling char is
        // valid is tolerated. This proves the clip decision comes from the actual
        // file length, not from `buf.len() == CAP`.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let tmp = tempfile::TempDir::new().unwrap();

            // File A: exactly CAP bytes, last byte an incomplete lead -> reject.
            let a = tmp.path().join("eof.js");
            let mut bytes_a = vec![b'a'; CAP - 1];
            bytes_a.push(0xC3);
            std::fs::write(&a, &bytes_a).unwrap();
            let err = read_meta_prefix(&a).await.unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

            // File B: CAP-1 `a`s then a full `é` (0xC3 0xA9), so the file extends
            // one byte past the cap and the straddling char is valid -> tolerate.
            let b = tmp.path().join("clip.js");
            let mut bytes_b = vec![b'a'; CAP - 1];
            bytes_b.extend_from_slice("é".as_bytes());
            std::fs::write(&b, &bytes_b).unwrap();
            let text = read_meta_prefix(&b).await.unwrap();
            assert_eq!(text.len(), CAP - 1);
            assert!(text.chars().all(|c| c == 'a'));
        });
    }

    /// Bound proof (instrumented byte count): `read_meta_prefix` must return at
    /// most [`MAX_META_READ_BYTES`] bytes even for a file far larger than the
    /// cap. This directly measures how many bytes the read path materialized —
    /// a regression to an unbounded `read_to_end` would return the whole
    /// (over-cap) file and fail the assertion, without relying on wall-clock
    /// timing.
    #[tokio::test]
    async fn read_meta_prefix_reads_at_most_cap_bytes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("oversized.js");

        // A valid tiny meta followed by a body that pushes the file well past
        // the cap (cap + 1 MiB of real bytes). Small in absolute terms, so this
        // is portable (no sparse-file / OOM reliance) yet strictly larger than
        // the read bound.
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"export const meta = { name: 'big', description: 'x' };\n")
            .unwrap();
        let body = vec![b'x'; CAP + 1024 * 1024];
        file.write_all(&body).unwrap();
        file.flush().unwrap();
        drop(file);

        let text = read_meta_prefix(&path).await.unwrap();
        assert!(
            text.len() <= CAP,
            "read materialized {} bytes, exceeding the {CAP}-byte cap",
            text.len(),
        );
        // Sanity: the leading manifest still landed inside the bounded prefix.
        assert!(parse_workflow_meta(&text).is_ok());
    }

    #[test]
    fn push_bounded_keeps_smallest_entries_sorted() {
        let mut sorted: Vec<PathBuf> = Vec::new();
        for name in ["d", "b", "e", "a", "c"] {
            push_bounded(&mut sorted, PathBuf::from(name), 3);
        }
        assert_eq!(
            sorted,
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
        );
    }

    /// Bound proof (instrumented enumeration count, not timing): a directory with
    /// more entries than the raw ceiling stops enumerating at exactly the ceiling
    /// and reports truncation, so a pathological flat directory does NOT force one
    /// stat per entry. Driven with a tiny ceiling so no huge directory is needed.
    #[tokio::test]
    async fn scan_directory_bounds_raw_enumeration_at_ceiling() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();

        // 20 workflow files; ceiling of 4 forces truncation well before the end.
        for i in 0..20 {
            std::fs::write(dir.join(format!("wf{i:02}.js")), b"// x").unwrap();
        }

        let scan = scan_directory(dir, 8, 4).await.unwrap();
        // Enumeration stopped at the ceiling — NOT all 20 entries were stat-ed.
        assert_eq!(scan.enumerated, 4);
        assert!(scan.truncated);
        // The bounded file bucket never exceeds its cap either.
        assert!(scan.files.len() <= 4);
    }

    /// A directory at or under the ceiling is enumerated in full (no truncation),
    /// so its sorted surviving set stays deterministic.
    #[tokio::test]
    async fn scan_directory_enumerates_fully_under_ceiling() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        for i in 0..3 {
            std::fs::write(dir.join(format!("wf{i}.js")), b"// x").unwrap();
        }
        std::fs::create_dir(dir.join("sub")).unwrap();

        let scan = scan_directory(dir, 8, 64).await.unwrap();
        assert_eq!(scan.enumerated, 4); // 3 files + 1 subdir, all seen
        assert!(!scan.truncated);
        assert_eq!(scan.files.len(), 3);
        assert_eq!(scan.subdirs, vec![dir.join("sub")]);
    }

    /// Sub-directories are NOT bounded by the candidate cap: with a candidate cap
    /// of 1, a directory of many sub-directories still surfaces every one of them
    /// (up to the raw ceiling), so a subtree that holds the only candidate is not
    /// silently dropped (Finding 1). Contrast with `files`, which the cap bounds.
    #[tokio::test]
    async fn scan_directory_does_not_cap_subdirs_by_candidate_limit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();
        for i in 0..10 {
            std::fs::create_dir(dir.join(format!("sub{i:02}"))).unwrap();
        }
        // Also drop several sibling files so we can confirm the file bucket IS
        // still capped while the subdir set is not.
        for i in 0..10 {
            std::fs::write(dir.join(format!("wf{i:02}.js")), b"// x").unwrap();
        }

        // files_cap = 1 (candidate cap of 1) but a generous raw ceiling.
        let scan = scan_directory(dir, 1, 64).await.unwrap();
        assert!(!scan.truncated);
        // All 10 subdirs retained despite the candidate cap of 1...
        assert_eq!(scan.subdirs.len(), 10);
        // ...while the FILE bucket is bounded by the candidate cap.
        assert_eq!(scan.files.len(), 1);
    }
}
