use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::WorkflowSourceLoadError;
use super::WorkflowSourceSnapshot;
use crate::WORKFLOW_SOURCE_MAX_BYTES;
use crate::WorkflowMetadata;
use crate::WorkflowRegistry;
use crate::WorkflowScope;

fn workflow_source(name: &str, description: &str, phases: &str, marker: &str) -> String {
    format!(
        "export const meta = {{ name: '{name}', description: '{description}', phases: {phases} }};\n// {marker}\n"
    )
}

fn canonical_uri(path: &Path) -> PathUri {
    let path = AbsolutePathBuf::from_absolute_path_checked(path.canonicalize().unwrap()).unwrap();
    PathUri::from_abs_path(&path)
}

fn metadata(path: PathUri) -> WorkflowMetadata {
    WorkflowMetadata {
        name: "review".to_string(),
        description: "Review changes".to_string(),
        phases: vec!["Inspect".to_string(), "Report".to_string()],
        path,
        scope: WorkflowScope::Project,
    }
}

struct HostFixture {
    _temp: TempDir,
    path: PathBuf,
    source: String,
    metadata: WorkflowMetadata,
    registry: WorkflowRegistry,
}

impl HostFixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("review.js");
        let source = workflow_source(
            "review",
            "Review changes",
            "['Inspect', 'Report']",
            "original-secret-body",
        );
        std::fs::write(&path, &source).unwrap();
        let metadata = metadata(canonical_uri(&path));
        let registry = WorkflowRegistry::from_discovery(vec![(metadata.clone(), None)], Vec::new());
        Self {
            _temp: temp,
            path,
            source,
            metadata,
            registry,
        }
    }

    async fn load_error(&self) -> WorkflowSourceLoadError {
        self.registry
            .source_snapshot_by_name("review")
            .await
            .unwrap_err()
    }
}

#[tokio::test]
async fn host_snapshot_is_exact_immutable_and_redacted() {
    let fixture = HostFixture::new();
    let snapshot = fixture
        .registry
        .source_snapshot_by_name("review")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot,
        WorkflowSourceSnapshot {
            metadata: fixture.metadata.clone(),
            source: Arc::from(fixture.source.clone()),
        }
    );
    let debug = format!("{snapshot:?}");
    assert!(!debug.contains("original-secret-body"));
    assert!(debug.contains(&format!("source_bytes: {}", fixture.source.len())));

    let replacement = fixture
        .source
        .replace("original-secret-body", "replacement");
    std::fs::write(&fixture.path, &replacement).unwrap();
    assert_eq!(snapshot.source(), fixture.source);
    assert_eq!(snapshot.metadata(), &fixture.metadata);
    assert_eq!(
        fixture
            .registry
            .source_snapshot_by_name("review")
            .await
            .unwrap()
            .unwrap()
            .source(),
        replacement
    );
}

#[tokio::test]
async fn host_snapshot_enforces_size_utf8_and_presence() {
    let fixture = HostFixture::new();
    let mut exact = fixture.source.clone();
    exact.push_str(&" ".repeat(WORKFLOW_SOURCE_MAX_BYTES - exact.len()));
    std::fs::write(&fixture.path, &exact).unwrap();
    let snapshot = fixture
        .registry
        .source_snapshot_by_name("review")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.source().len(), WORKFLOW_SOURCE_MAX_BYTES);

    std::fs::write(&fixture.path, format!("{exact}x")).unwrap();
    assert!(fixture.load_error().await.message().contains("exceeds"));
    let mut invalid = fixture.source.as_bytes().to_vec();
    invalid.push(0xff);
    std::fs::write(&fixture.path, invalid).unwrap();
    assert!(
        fixture
            .load_error()
            .await
            .message()
            .contains("not valid UTF-8")
    );
    std::fs::remove_file(&fixture.path).unwrap();
    assert!(
        fixture
            .load_error()
            .await
            .message()
            .contains("failed to inspect source")
    );
}

#[test]
fn backend_io_error_text_is_redacted() {
    let path = PathUri::parse("file:///workflow.js").unwrap();
    let error = super::source_io_error(&path, "read source", io::Error::other("source-secret"));
    assert_eq!(error.message(), "failed to read source (Other)");
}

#[tokio::test]
async fn host_snapshot_rejects_each_metadata_drift_and_invalid_metadata() {
    let fixture = HostFixture::new();
    let replacements = [
        workflow_source(
            "deploy",
            "Review changes",
            "['Inspect', 'Report']",
            "secret",
        ),
        workflow_source("review", "Changed", "['Inspect', 'Report']", "secret"),
        workflow_source("review", "Review changes", "['Different']", "secret"),
    ];
    for replacement in replacements {
        std::fs::write(&fixture.path, replacement).unwrap();
        let error = fixture.load_error().await;
        assert_eq!(error.message(), "workflow metadata changed after discovery");
        assert!(!format!("{error:?}").contains("secret"));
        assert!(!error.to_string().contains("secret"));
    }

    std::fs::write(
        &fixture.path,
        "export const meta = buildMeta(); // invalid-secret\n",
    )
    .unwrap();
    let error = fixture.load_error().await;
    assert!(
        error
            .message()
            .starts_with("workflow metadata is no longer valid:")
    );
    assert!(!format!("{error:?}").contains("secret"));
    assert!(!error.to_string().contains("secret"));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn host_snapshot_rejects_final_symlink_and_non_file_replacements() {
    let fixture = HostFixture::new();
    let referent = fixture.path.with_file_name("referent.js");
    std::fs::write(&referent, &fixture.source).unwrap();
    std::fs::remove_file(&fixture.path).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&referent, &fixture.path).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&referent, &fixture.path).unwrap();
    let error = fixture.load_error().await;
    assert_eq!(error.path(), &fixture.metadata.path);
    assert!(error.message().contains("non-symlink"));

    let fixture = HostFixture::new();
    std::fs::remove_file(&fixture.path).unwrap();
    std::fs::create_dir(&fixture.path).unwrap();
    let error = fixture.load_error().await;
    assert_eq!(error.path(), &fixture.metadata.path);
    assert!(error.message().contains("regular"));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_no_follow_open_keeps_file_symlink_as_reparse_point() {
    let temp = TempDir::new().unwrap();
    let referent = temp.path().join("referent.js");
    let link = temp.path().join("link.js");
    std::fs::write(&referent, "referent").unwrap();
    std::os::windows::fs::symlink_file(&referent, &link).unwrap();
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    super::configure_no_follow(&mut options);

    let file = options.open(&link).await.unwrap();
    let metadata = file.metadata().await.unwrap();
    assert!(super::is_link_or_reparse_point(&metadata));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn host_snapshot_rejects_ancestor_path_drift() {
    let temp = TempDir::new().unwrap();
    let selected = temp.path().join("selected");
    let redirected = temp.path().join("redirected");
    std::fs::create_dir(&selected).unwrap();
    std::fs::create_dir(&redirected).unwrap();
    let path = selected.join("review.js");
    let source = workflow_source("review", "Review changes", "['Inspect', 'Report']", "body");
    std::fs::write(&path, &source).unwrap();
    std::fs::write(redirected.join("review.js"), source).unwrap();
    let metadata = metadata(canonical_uri(&path));
    let registry = WorkflowRegistry::from_discovery(vec![(metadata.clone(), None)], Vec::new());
    std::fs::remove_dir_all(&selected).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&redirected, &selected).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&redirected, &selected).unwrap();

    let error = registry
        .source_snapshot_by_name("review")
        .await
        .unwrap_err();
    assert_eq!(error.path(), &metadata.path);
    assert!(error.message().contains("now resolves"));
}
