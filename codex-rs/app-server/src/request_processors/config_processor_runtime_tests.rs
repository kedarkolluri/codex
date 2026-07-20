use super::*;
use crate::mcp_refresh::tests::config_refresh_test_state;
use crate::outgoing_message::OutgoingMessageSender;
use codex_app_server_protocol::ConfigEdit;
use codex_app_server_protocol::MergeStrategy;
use codex_app_server_protocol::WriteStatus;
use codex_config::types::ToolSuggestDisabledTool;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio::sync::mpsc;

#[tokio::test]
async fn config_batch_write_reload_skips_workflow_managed_thread() -> anyhow::Result<()> {
    let (_temp_dir, thread_manager, config_manager) = config_refresh_test_state().await?;
    let mut ordinary_thread = None;
    let mut workflow_thread = None;
    for thread_id in thread_manager.list_thread_ids().await {
        let thread = thread_manager.get_thread(thread_id).await?;
        if !thread.config().await.cwd.ends_with("good") {
            continue;
        }
        if thread.is_workflow_managed_agent() {
            workflow_thread = Some(thread);
        } else {
            ordinary_thread = Some(thread);
        }
    }
    let ordinary_thread = ordinary_thread.expect("ordinary good-cwd thread should exist");
    let workflow_thread = workflow_thread.expect("workflow-managed good-cwd thread should exist");
    let ordinary_before = ordinary_thread.config().await;
    let workflow_before = workflow_thread.config().await;
    assert_eq!(ordinary_before.tool_suggest.disabled_tools, Vec::new());
    assert_eq!(workflow_before.tool_suggest.disabled_tools, Vec::new());

    let (outgoing_tx, _outgoing_rx) = mpsc::channel(/*buffer*/ 8);
    let analytics_events_client = AnalyticsEventsClient::disabled();
    let processor = ConfigRequestProcessor::new(
        Arc::new(OutgoingMessageSender::new(
            outgoing_tx,
            analytics_events_client.clone(),
        )),
        config_manager,
        Arc::clone(&thread_manager),
        analytics_events_client,
    );
    let response = processor
        .batch_write(ConfigBatchWriteParams {
            edits: vec![ConfigEdit {
                key_path: "tool_suggest.disabled_tools".to_string(),
                value: json!([{"type": "connector", "id": "calendar"}]),
                merge_strategy: MergeStrategy::Replace,
            }],
            file_path: None,
            expected_version: None,
            reload_user_config: true,
        })
        .await
        .map_err(|err| anyhow::anyhow!("config/batchWrite failed: {}", err.message))?;
    let ClientResponsePayload::ConfigBatchWrite(response) = response else {
        panic!("config/batchWrite returned an unexpected response payload");
    };
    assert_eq!(response.status, WriteStatus::Ok);

    let ordinary_after = ordinary_thread.config().await;
    let workflow_after = workflow_thread.config().await;
    assert!(!Arc::ptr_eq(&ordinary_before, &ordinary_after));
    assert!(Arc::ptr_eq(&workflow_before, &workflow_after));
    assert_eq!(ordinary_after.model, ordinary_before.model);
    assert_eq!(workflow_after.model, workflow_before.model);
    assert_eq!(
        ordinary_after.tool_suggest.disabled_tools,
        vec![ToolSuggestDisabledTool::connector("calendar")]
    );
    assert_eq!(workflow_after.tool_suggest, workflow_before.tool_suggest);
    Ok(())
}
