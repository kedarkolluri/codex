use std::fs;
use std::path::Path;

use codex_code_mode_protocol::WORKFLOW_META_MAX_BYTES;
use codex_code_mode_protocol::parse_workflow_meta;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::DISCOVERY_LIMITS;
use super::Diagnostics;
use super::DiscoveryLimits;
use super::MAX_DIAGNOSTIC_MESSAGE_BYTES;
use super::MAX_DIAGNOSTICS_PER_ROOT;
use super::MAX_WORKFLOW_ROOTS;
use super::META_LOGICAL_READ_BYTES;
use super::UTF8_BOUNDARY_LOOKAHEAD_BYTES;
use super::decode_meta_prefix;
use super::load_workflows_from_roots;
use super::read_meta_prefix;
use super::scan_workflow_root;
use crate::WorkflowLoadError;
use crate::WorkflowMetadata;
use crate::WorkflowRoot;
use crate::WorkflowScope;

fn absolute(path: impl AsRef<Path>) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path_checked(path).unwrap()
}

fn canonical(path: impl AsRef<Path>) -> AbsolutePathBuf {
    absolute(path).canonicalize().unwrap()
}

fn write_workflow(directory: &Path, file_name: &str, source: impl AsRef<[u8]>) -> AbsolutePathBuf {
    fs::create_dir_all(directory).unwrap();
    let path = directory.join(file_name);
    fs::write(&path, source).unwrap();
    absolute(path)
}

fn metadata(name: &str, description: &str, phases: &str, body: &str) -> String {
    format!(
        "export const meta = {{ name: '{name}', description: '{description}', phases: {phases} }};\n{body}"
    )
}

fn workflow(
    name: &str,
    description: &str,
    phases: &[&str],
    path: AbsolutePathBuf,
    scope: WorkflowScope,
) -> WorkflowMetadata {
    WorkflowMetadata {
        name: name.to_string(),
        description: description.to_string(),
        phases: phases.iter().map(ToString::to_string).collect(),
        path,
        scope,
    }
}

#[tokio::test]
async fn loads_three_scopes_with_precedence_without_evaluating_the_body() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let personal = temp.path().join("personal");
    let codex_home = temp.path().join("codex-home");
    let sentinel = temp.path().join("body-ran");
    let body_path = sentinel.to_string_lossy().replace('\\', "\\\\");
    let mut project_source = metadata(
        "triage",
        "project",
        "[{ title: 'Plan', detail: 'fan out' }, 'Ship']",
        &format!("require('fs').writeFileSync('{body_path}', 'bad'); throw new Error('not run');"),
    );
    project_source.push_str(&"x".repeat(META_LOGICAL_READ_BYTES + 1_024));
    let project_path = write_workflow(&project, "triage.workflow.js", project_source);
    write_workflow(
        &personal,
        "triage.js",
        metadata("triage", "personal", "[]", ""),
    );
    let release_path = write_workflow(
        &personal,
        "release.workflow.js",
        metadata("release", "personal only", "[]", ""),
    );
    write_workflow(
        &codex_home,
        "triage.js",
        metadata("triage", "codex home", "[]", ""),
    );
    let nested_path = write_workflow(
        &codex_home.join("group"),
        "nested.JS",
        metadata("nested", "nested", "['Inspect']", ""),
    );
    write_workflow(
        &codex_home,
        "ignored.txt",
        metadata("ignored", "not js", "[]", ""),
    );

    let registry = load_workflows_from_roots(vec![
        WorkflowRoot::new(absolute(&codex_home), WorkflowScope::CodexHome),
        WorkflowRoot::new(absolute(&personal), WorkflowScope::Personal),
        WorkflowRoot::new(absolute(&project), WorkflowScope::Project),
    ])
    .await;
    let expected = vec![
        workflow(
            "nested",
            "nested",
            &["Inspect"],
            canonical(nested_path),
            WorkflowScope::CodexHome,
        ),
        workflow(
            "release",
            "personal only",
            &[],
            canonical(release_path),
            WorkflowScope::Personal,
        ),
        workflow(
            "triage",
            "project",
            &["Plan", "Ship"],
            canonical(project_path),
            WorkflowScope::Project,
        ),
    ];
    assert_eq!(registry.workflows(), expected);
    assert_eq!(registry.resolve_by_name("triage"), expected.get(2));
    assert_eq!(registry.errors(), &[]);
    assert!(!sentinel.exists());
}

#[tokio::test]
async fn skips_invalid_metadata_and_keeps_valid_neighbors_and_missing_roots() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("workflows");
    let good_path = write_workflow(&root, "good.js", metadata("good", "valid", "[]", ""));
    let computed_path = write_workflow(&root, "computed.js", "export const meta = buildMeta();\n");
    let mut invalid_utf8 = b"export const meta = { name: 'b".to_vec();
    invalid_utf8.push(0xff);
    invalid_utf8.extend_from_slice(b"ad', description: 'invalid', phases: [] };\n");
    let invalid_path = write_workflow(&root, "invalid.js", invalid_utf8);
    fs::create_dir_all(root.join("directory.js")).unwrap();

    let scope = WorkflowScope::Project;
    let mut roots = (0..MAX_WORKFLOW_ROOTS)
        .map(|index| WorkflowRoot::new(absolute(temp.path().join(index.to_string())), scope))
        .collect::<Vec<_>>();
    roots[0] = WorkflowRoot::new(absolute(&root), WorkflowScope::Personal);
    let ignored_root = absolute(temp.path().join("zz-ignored"));
    roots.push(WorkflowRoot::new(ignored_root.clone(), scope));
    let registry = load_workflows_from_roots(roots).await;
    assert_eq!(
        registry.workflows(),
        &[workflow(
            "good",
            "valid",
            &[],
            canonical(good_path),
            WorkflowScope::Personal,
        )]
    );
    assert_eq!(
        registry.errors(),
        &[
            WorkflowLoadError {
                path: canonical(computed_path),
                message: "`meta` must be a static object literal beginning with `{`".to_string(),
            },
            WorkflowLoadError {
                path: canonical(invalid_path),
                message:
                    "failed to read workflow metadata: workflow metadata prefix is not valid UTF-8"
                        .to_string(),
            },
            WorkflowLoadError {
                path: ignored_root,
                message: format!(
                    "workflow root limit {MAX_WORKFLOW_ROOTS} exceeded; extras ignored"
                ),
            },
        ]
    );
}

#[tokio::test]
async fn excludes_only_direct_codex_home_runs_state() {
    let temp = TempDir::new().unwrap();
    let project = temp.path().join("project");
    let codex_home = temp.path().join("codex-home");
    let project_path = write_workflow(
        &project.join("runs"),
        "project.js",
        metadata("project-runs", "project", "[]", ""),
    );
    write_workflow(
        &codex_home.join("runs").join("run-id"),
        "script.js",
        metadata("run-state", "must not load", "[]", ""),
    );
    let nested_path = write_workflow(
        &codex_home.join("group").join("runs"),
        "nested.js",
        metadata("nested-runs", "saved", "[]", ""),
    );

    let registry = load_workflows_from_roots(vec![
        WorkflowRoot::new(absolute(&project), WorkflowScope::Project),
        WorkflowRoot::new(absolute(&codex_home), WorkflowScope::CodexHome),
    ])
    .await;
    assert_eq!(
        registry.workflows(),
        &[
            workflow(
                "nested-runs",
                "saved",
                &[],
                canonical(nested_path),
                WorkflowScope::CodexHome,
            ),
            workflow(
                "project-runs",
                "project",
                &[],
                canonical(project_path),
                WorkflowScope::Project,
            ),
        ]
    );
    assert_eq!(registry.errors(), &[]);
}

#[tokio::test]
async fn preserves_parser_lookahead_and_bounds_utf8_prefix_reads() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("workflows");
    let start = "export const meta = { name: 'boundary', description: 'd', phases: []";
    let padding = " ".repeat(WORKFLOW_META_MAX_BYTES - start.len() - 1);
    let source = format!("{start}{padding}}}+ sideEffect();");
    let boundary_path = write_workflow(&root, "boundary.js", source);

    let registry = load_workflows_from_roots(vec![WorkflowRoot::new(
        absolute(&root),
        WorkflowScope::Project,
    )])
    .await;
    assert_eq!(registry.workflows(), &[]);
    assert_eq!(
        registry.errors(),
        &[WorkflowLoadError {
            path: canonical(boundary_path),
            message: "the `meta` object must end its statement before the workflow body; add `;` or a non-continuing line break".to_string(),
        }]
    );

    let huge_path = write_workflow(
        &root,
        "huge.js",
        metadata(
            "huge",
            "bounded",
            "[]",
            &"x".repeat(2 * META_LOGICAL_READ_BYTES),
        ),
    );
    let prefix = read_meta_prefix(&canonical(huge_path)).await.unwrap();
    assert!(prefix.len() <= META_LOGICAL_READ_BYTES + UTF8_BOUNDARY_LOOKAHEAD_BYTES);
    assert_eq!(parse_workflow_meta(&prefix).unwrap().name, "huge");

    let mut split = vec![b'a'; META_LOGICAL_READ_BYTES - 1];
    split.extend_from_slice("é".as_bytes());
    assert!(decode_meta_prefix(&split).unwrap().ends_with('é'));
    split.pop();
    assert!(decode_meta_prefix(&split).is_err());
}

#[tokio::test]
async fn bounded_traversal_discards_truncated_root_results() {
    let temp = TempDir::new().unwrap();
    let root_path = temp.path().join("workflows");
    let first = write_workflow(&root_path, "a.js", metadata("a", "first", "[]", ""));
    write_workflow(&root_path, "b.js", metadata("b", "second", "[]", ""));
    let at_limit = write_workflow(
        &root_path.join("nested"),
        "at-limit.js",
        metadata("at-limit", "included", "[]", ""),
    );
    write_workflow(
        &root_path.join("nested").join("too-deep"),
        "excluded.js",
        metadata("excluded", "too deep", "[]", ""),
    );
    let root = WorkflowRoot::new(absolute(&root_path), WorkflowScope::Project);
    let entry_limit = DiscoveryLimits {
        max_entries: 1,
        ..DISCOVERY_LIMITS
    };
    let scan = scan_workflow_root(&root, entry_limit).await;
    assert_eq!(scan.candidates, Vec::<AbsolutePathBuf>::new());
    assert_eq!(scan.diagnostics.finish().len(), 1);

    let candidate_limit = DiscoveryLimits {
        max_candidates: 1,
        ..DISCOVERY_LIMITS
    };
    let scan = scan_workflow_root(&root, candidate_limit).await;
    assert_eq!(scan.candidates, Vec::<AbsolutePathBuf>::new());
    assert_eq!(scan.diagnostics.finish().len(), 1);

    let directory_limit = DiscoveryLimits {
        max_directories: 1,
        ..DISCOVERY_LIMITS
    };
    let scan = scan_workflow_root(&root, directory_limit).await;
    assert_eq!(scan.candidates, Vec::<AbsolutePathBuf>::new());
    assert_eq!(scan.diagnostics.finish().len(), 1);

    let depth_limit = DiscoveryLimits {
        max_depth: 1,
        ..DISCOVERY_LIMITS
    };
    let scan = scan_workflow_root(&root, depth_limit).await;
    assert_eq!(
        scan.candidates,
        vec![
            canonical(first),
            canonical(root_path.join("b.js")),
            canonical(at_limit),
        ]
    );
    assert_eq!(scan.diagnostics.finish().len(), 1);
}

#[test]
fn bounds_diagnostic_count_and_message_size() {
    let path = absolute(std::env::current_dir().unwrap());
    let mut diagnostics = Diagnostics::new(path.clone());
    diagnostics.push(path.clone(), "é".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES));
    for _ in 0..MAX_DIAGNOSTICS_PER_ROOT {
        diagnostics.push(path.clone(), "error".to_string());
    }
    let errors = diagnostics.finish();
    assert_eq!(errors.len(), MAX_DIAGNOSTICS_PER_ROOT);
    assert!(errors[0].message.len() <= MAX_DIAGNOSTIC_MESSAGE_BYTES);
    let last = &errors.last().unwrap().message;
    assert_eq!(
        last,
        "2 additional workflow discovery diagnostic(s) omitted"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn follows_explicit_root_alias_but_ignores_descendant_symlinks() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().unwrap();
    let actual = temp.path().join("actual");
    let external = temp.path().join("external");
    let real_path = write_workflow(&actual, "real.js", metadata("real", "real", "[]", ""));
    let outside_path = write_workflow(
        &external,
        "outside.js",
        metadata("outside", "outside", "[]", ""),
    );
    symlink(&outside_path, actual.join("linked.js")).unwrap();
    symlink(&external, actual.join("linked-directory")).unwrap();
    let alias = temp.path().join("alias");
    symlink(&actual, &alias).unwrap();

    let registry = load_workflows_from_roots(vec![WorkflowRoot::new(
        absolute(alias),
        WorkflowScope::Project,
    )])
    .await;

    assert_eq!(
        registry.workflows(),
        &[workflow(
            "real",
            "real",
            &[],
            canonical(real_path),
            WorkflowScope::Project,
        )]
    );
    assert_eq!(registry.errors(), &[]);
}
