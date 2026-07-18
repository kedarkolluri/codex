use std::fs;
use std::path::Path;

use codex_core_workflows::WorkflowRoot;
use codex_core_workflows::WorkflowScope;
use codex_core_workflows::load_workflows_from_roots;
use codex_core_workflows::workflow_roots;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[allow(clippy::unwrap_used)]
fn write_workflow(dir: &Path, file: &str, body: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join(file), body).unwrap();
}

fn meta(name: &str, description: &str) -> String {
    format!("export const meta = {{ name: '{name}', description: '{description}' }};\n")
}

/// Acceptance: loader lists saved workflows by name across the three roots with
/// correct precedence (project > personal > CODEX_HOME) and de-duplicates
/// same-name entries by scope.
#[tokio::test]
async fn lists_and_dedupes_across_roots_with_precedence() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo").join(".codex").join("workflows");
    let personal = tmp.path().join("home").join(".agents").join("workflows");
    let codex_home = tmp.path().join("codex_home").join("workflows");

    // `triage` exists in all three scopes; project must win.
    write_workflow(&repo, "triage.js", &meta("triage", "project triage"));
    write_workflow(&personal, "triage.js", &meta("triage", "personal triage"));
    write_workflow(
        &codex_home,
        "triage.js",
        &meta("triage", "codex_home triage"),
    );

    // `personal-only` exists only in personal; `home-only` only in CODEX_HOME.
    write_workflow(&personal, "p.js", &meta("personal-only", "p"));
    write_workflow(&codex_home, "h.js", &meta("home-only", "h"));

    let roots = vec![
        WorkflowRoot::new(&repo, WorkflowScope::Project),
        WorkflowRoot::new(&personal, WorkflowScope::Personal),
        WorkflowRoot::new(&codex_home, WorkflowScope::CodexHome),
    ];
    let registry = load_workflows_from_roots(roots).await;

    let names: Vec<&str> = registry.names().collect();
    assert_eq!(names, vec!["home-only", "personal-only", "triage"]);

    let triage = registry.resolve_by_name("triage").unwrap();
    assert_eq!(triage.scope, WorkflowScope::Project);
    assert_eq!(triage.description, "project triage");
    assert_eq!(triage.path, repo.join("triage.js"));

    assert_eq!(
        registry.resolve_by_name("personal-only").unwrap().scope,
        WorkflowScope::Personal
    );
    assert_eq!(
        registry.resolve_by_name("home-only").unwrap().scope,
        WorkflowScope::CodexHome
    );
}

/// Acceptance (security): discovery statically parses `meta` only and NEVER
/// evaluates a body — a workflow whose body throws / has side effects is still
/// discovered by name.
#[tokio::test]
async fn discovers_workflow_with_hostile_body_without_evaluating_it() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("workflows");
    let side_effect = tmp.path().join("side-effect.txt");

    let body = format!(
        "export const meta = {{ name: 'danger', description: 'still discovered' }};\n\
         // The following would blow up or write to disk if ever executed:\n\
         require('fs').writeFileSync('{}', 'pwned');\n\
         throw new Error('boom');\n\
         process.exit(1);\n",
        side_effect.to_string_lossy(),
    );
    write_workflow(&root, "danger.workflow.js", &body);

    let roots = vec![WorkflowRoot::new(&root, WorkflowScope::Project)];
    let registry = load_workflows_from_roots(roots).await;

    let danger = registry.resolve_by_name("danger").unwrap();
    assert_eq!(danger.description, "still discovered");
    // The body must never have run.
    assert!(!side_effect.exists(), "workflow body was evaluated!");
    assert!(registry.errors().is_empty());
}

/// Acceptance: a file with malformed / non-literal `meta` is skipped without
/// failing the whole discovery pass.
#[tokio::test]
async fn skips_malformed_meta_but_keeps_good_workflows() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("workflows");

    write_workflow(&root, "good.js", &meta("good", "good one"));
    // Non-literal meta (function call) — security-relevant, must be rejected.
    write_workflow(&root, "computed.js", "export const meta = buildMeta();\n");
    // Missing required field.
    write_workflow(&root, "no-desc.js", "export const meta = { name: 'x' };\n");
    // Not a workflow (no meta at all).
    write_workflow(&root, "random.js", "const x = 1;\n");

    let roots = vec![WorkflowRoot::new(&root, WorkflowScope::Project)];
    let registry = load_workflows_from_roots(roots).await;

    let names: Vec<&str> = registry.names().collect();
    assert_eq!(names, vec!["good"]);
    // Three files were skipped, and discovery still succeeded.
    assert_eq!(registry.errors().len(), 3);
}

/// Acceptance: `resolve_by_name` returns the highest-precedence entry for a
/// given name and its absolute script path.
#[tokio::test]
async fn resolve_by_name_returns_highest_precedence_absolute_path() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join(".codex").join("workflows");
    let codex_home = tmp.path().join("codex_home").join("workflows");

    write_workflow(&repo, "deploy.js", &meta("deploy", "from repo"));
    write_workflow(&codex_home, "deploy.js", &meta("deploy", "from codex_home"));

    let roots = vec![
        WorkflowRoot::new(&repo, WorkflowScope::Project),
        WorkflowRoot::new(&codex_home, WorkflowScope::CodexHome),
    ];
    let registry = load_workflows_from_roots(roots).await;

    let resolved = registry.resolve_by_name("deploy").unwrap();
    assert_eq!(resolved.description, "from repo");
    assert_eq!(resolved.path, repo.join("deploy.js"));
    assert!(resolved.path.is_absolute());
    assert!(registry.resolve_by_name("missing").is_none());
}

/// Discovery recurses into sub-directories and preserves declared phase order.
#[tokio::test]
async fn discovers_nested_files_and_preserves_phases() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("workflows");
    let nested = root.join("group");

    write_workflow(
        &nested,
        "phased.js",
        "export const meta = { name: 'phased', description: 'd', phases: ['plan', 'build', 'ship'] };\n",
    );

    let roots = vec![WorkflowRoot::new(&root, WorkflowScope::Project)];
    let registry = load_workflows_from_roots(roots).await;

    let phased = registry.resolve_by_name("phased").unwrap();
    assert_eq!(phased.phases, vec!["plan", "build", "ship"]);
}

/// Missing roots are tolerated silently (fail-open at the directory level).
#[tokio::test]
async fn missing_roots_are_tolerated() {
    let tmp = TempDir::new().unwrap();
    let roots = vec![WorkflowRoot::new(
        tmp.path().join("does-not-exist"),
        WorkflowScope::Project,
    )];
    let registry = load_workflows_from_roots(roots).await;
    assert!(registry.workflows().is_empty());
    assert!(registry.errors().is_empty());
}

/// Acceptance (resource cap): a candidate file with a small valid `meta` but a
/// huge body is still discovered, because discovery reads only a bounded prefix
/// of each file instead of materializing the whole body. A file whose `meta`
/// region itself exceeds the manifest cap fails open
/// (skipped-with-recorded-error).
///
/// The read *bound* itself is proven deterministically (by counting bytes) in
/// the `read_meta_prefix_reads_at_most_cap_bytes` unit test in `src/loader.rs`;
/// this end-to-end test only asserts the observable discovery behavior.
#[tokio::test]
async fn bounds_per_file_read_and_skips_oversized_meta() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("workflows");

    // Small valid meta followed by a large body (~12 MiB). Discovery reads only
    // the bounded leading prefix, so the body is never materialized.
    let mut huge = meta("huge", "small meta, huge body");
    huge.push_str("// giant body follows\n");
    huge.push_str(&"x".repeat(12 * 1024 * 1024));
    write_workflow(&root, "huge.js", &huge);

    // A file whose `meta` object literal itself exceeds the 256 KiB manifest
    // cap: an unterminated (multi-megabyte) string literal inside `meta`. The
    // static parser aborts at its own cap and the file is skipped.
    let oversized = format!(
        "export const meta = {{ name: 'oversized', description: '{}' }};\n",
        "a".repeat(512 * 1024)
    );
    write_workflow(&root, "oversized.js", &oversized);

    let roots = vec![WorkflowRoot::new(&root, WorkflowScope::Project)];
    let registry = load_workflows_from_roots(roots).await;

    // The small-meta/huge-body file is discovered and parses correctly.
    let huge_entry = registry.resolve_by_name("huge").unwrap();
    assert_eq!(huge_entry.description, "small meta, huge body");

    // The oversized-meta file is skipped, not discovered.
    assert!(registry.resolve_by_name("oversized").is_none());
    assert_eq!(registry.errors().len(), 1);
    assert_eq!(registry.errors()[0].path, root.join("oversized.js"));
}

/// Acceptance (security): an invalid UTF-8 byte inside the `meta` manifest must
/// NOT be discovered as a U+FFFD-mangled entry — the file is skipped with a
/// recorded error (strict UTF-8 for the manifest region), matching how a later
/// strict load of the same file would reject it.
#[tokio::test]
async fn skips_workflow_with_invalid_utf8_in_meta() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("workflows");
    fs::create_dir_all(&root).unwrap();

    // A valid neighbor plus a file with a lone invalid byte (0xFF) inside the
    // `meta.name` string. Writing raw bytes bypasses UTF-8 string construction.
    write_workflow(&root, "good.js", &meta("good", "ok"));
    let mut bad = b"export const meta = { name: 'b".to_vec();
    bad.push(0xFF);
    bad.extend_from_slice(b"ad', description: 'x' };\n");
    fs::write(root.join("bad.js"), &bad).unwrap();

    let roots = vec![WorkflowRoot::new(&root, WorkflowScope::Project)];
    let registry = load_workflows_from_roots(roots).await;

    // Only the valid file is discovered; nothing mangled entered the registry.
    let names: Vec<&str> = registry.names().collect();
    assert_eq!(names, vec!["good"]);
    assert!(!registry.names().any(|n| n.contains('\u{FFFD}')));

    // The invalid file is recorded as skipped, not silently dropped.
    assert_eq!(registry.errors().len(), 1);
    assert_eq!(registry.errors()[0].path, root.join("bad.js"));
}

/// A multi-byte character split across the bounded read boundary is tolerated:
/// the leading `meta` still parses and the workflow is discovered (the clipped
/// partial byte can only ever fall past the tiny manifest).
#[tokio::test]
async fn tolerates_multibyte_char_clipped_at_read_boundary() {
    // Read cap mirrored from the loader (256 KiB + 4 KiB slack).
    const CAP: usize = 256 * 1024 + 4 * 1024;

    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("workflows");
    fs::create_dir_all(&root).unwrap();

    // Valid tiny meta, then ASCII padding positioned so a two-byte `é`
    // (0xC3 0xA9) begins on the very last byte the read is allowed to consume.
    let mut bytes = meta("boundary", "clipped").into_bytes();
    // Pad with ASCII so the two-byte `é` starts exactly at index CAP-1.
    bytes.resize(CAP - 1, b'x');
    bytes.extend_from_slice("é".as_bytes()); // 0xC3 at index CAP-1, 0xA9 clipped
    fs::write(root.join("boundary.js"), &bytes).unwrap();

    let roots = vec![WorkflowRoot::new(&root, WorkflowScope::Project)];
    let registry = load_workflows_from_roots(roots).await;

    let entry = registry.resolve_by_name("boundary").unwrap();
    assert_eq!(entry.description, "clipped");
    assert!(registry.errors().is_empty());
}

/// Acceptance (resource cap): a root packed with more than
/// `MAX_WORKFLOW_FILES_PER_ROOT` candidate files is truncated deterministically
/// (sorted-first survive) and the drop is recorded rather than silently
/// discarded.
#[tokio::test]
async fn caps_candidate_count_per_root_and_records_drop() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("workflows");

    // 300 uniquely-named workflow files > the 256 per-root cap.
    const TOTAL: usize = 300;
    const CAP: usize = 256;
    for i in 0..TOTAL {
        let name = format!("wf{i:03}");
        write_workflow(&root, &format!("{name}.js"), &meta(&name, "ok"));
    }

    let roots = vec![WorkflowRoot::new(&root, WorkflowScope::Project)];
    let registry = load_workflows_from_roots(roots).await;

    // Exactly CAP workflows survive, and the surviving set is the sorted-first
    // slice (wf000..=wf255), i.e. deterministic — not filesystem order.
    assert_eq!(registry.workflows().len(), CAP);
    assert!(registry.resolve_by_name("wf000").is_some());
    assert!(registry.resolve_by_name("wf255").is_some());
    assert!(registry.resolve_by_name("wf256").is_none());
    assert!(registry.resolve_by_name("wf299").is_none());

    // The drop is recorded (no silent truncation). Because the bound is enforced
    // during the walk (it stops after `cap + 1` candidates), the diagnostic is a
    // "more than N" marker rather than an exact overflow count, and its wording
    // must match the actual behavior: the LOWEST-sorted survive, higher-sorted
    // are dropped.
    assert_eq!(registry.errors().len(), 1);
    let dropped_err = &registry.errors()[0];
    assert_eq!(dropped_err.path, root);
    assert!(
        dropped_err.message.contains(&CAP.to_string()),
        "drop record should mention the cap: {}",
        dropped_err.message
    );
    assert!(
        dropped_err.message.contains("more than"),
        "drop record should mark overflow as 'more than N': {}",
        dropped_err.message
    );
    assert!(
        dropped_err.message.contains("lowest-sorted")
            && dropped_err.message.contains("higher-sorted"),
        "drop record wording must match actual behavior (lowest-sorted survive): {}",
        dropped_err.message
    );
}

/// Regression (Finding 1): a single workflow buried under many EMPTY sibling
/// sub-directories must still be discovered. The old traversal capped the
/// sub-directory set by the candidate limit (`MAX_WORKFLOW_FILES_PER_ROOT` = 256,
/// bucket cap 257), so with more sub-directories than that cap the highest-sorted
/// sub-directory — here the only one holding a workflow — was silently dropped
/// with no truncation diagnostic. Sub-directories are no longer bounded by the
/// candidate cap, so the buried workflow is found.
#[tokio::test]
async fn discovers_workflow_buried_under_many_empty_sibling_subdirs() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("workflows");
    fs::create_dir_all(&root).unwrap();

    // 258 sub-directories (> the old 257 bucket cap). All empty except the
    // lexicographically-LAST one, which the old cap would have dropped.
    const SUBDIRS: usize = 258;
    for i in 0..SUBDIRS {
        fs::create_dir_all(root.join(format!("sub{i:03}"))).unwrap();
    }
    let buried = root.join(format!("sub{:03}", SUBDIRS - 1));
    write_workflow(&buried, "buried.js", &meta("buried", "found me"));

    let roots = vec![WorkflowRoot::new(&root, WorkflowScope::Project)];
    let registry = load_workflows_from_roots(roots).await;

    let entry = registry.resolve_by_name("buried").unwrap();
    assert_eq!(entry.description, "found me");
    // No truncation diagnostic: the fan-out is well under the raw entry ceiling.
    assert!(registry.errors().is_empty());
}

/// `workflow_roots` assembles the three scopes in precedence order.
#[test]
fn workflow_roots_are_precedence_ordered() {
    let repo = Path::new("/repo");
    let home = Path::new("/home/user");
    let codex_home = Path::new("/home/user/.codex");

    let roots = workflow_roots(Some(repo), Some(home), Some(codex_home));
    assert_eq!(
        roots,
        vec![
            WorkflowRoot::new(Path::new("/repo/.codex/workflows"), WorkflowScope::Project),
            WorkflowRoot::new(
                Path::new("/home/user/.agents/workflows"),
                WorkflowScope::Personal
            ),
            WorkflowRoot::new(
                Path::new("/home/user/.codex/workflows"),
                WorkflowScope::CodexHome
            ),
        ]
    );

    // Optional inputs are skipped.
    let only_codex = workflow_roots(None, None, Some(codex_home));
    assert_eq!(only_codex.len(), 1);
    assert_eq!(only_codex[0].scope, WorkflowScope::CodexHome);
}
