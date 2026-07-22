use pretty_assertions::assert_eq;

use super::WorkflowSourceResolver;

#[tokio::test]
async fn resolver_preserves_missing_and_diagnostic_results() {
    let missing = WorkflowSourceResolver::new(|_name| async { Ok::<_, &'static str>(None) });
    assert_eq!(missing.source_snapshot_by_name("missing").await, Ok(None));

    let failed = WorkflowSourceResolver::new(|name| async move {
        Err::<Option<crate::WorkflowSourceSnapshot>, _>(format!("could not resolve {name}"))
    });
    let error = failed
        .source_snapshot_by_name("review")
        .await
        .expect_err("resolver should retain its host diagnostic");
    assert_eq!(error.diagnostic(), "could not resolve review");
}
