mod delegate;
mod execute_handler;
pub(crate) mod execute_spec;
mod response_adapter;
mod scheduler;
mod wait_handler;
pub(crate) mod wait_spec;
mod workflow_handler;
pub(crate) mod workflow_spec;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeToolKind;
use codex_code_mode::RuntimeResponse;
use codex_protocol::models::FunctionCallOutputContentItem;
use serde_json::Value as JsonValue;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::function_tool::FunctionCallError;
use crate::original_image_detail::can_request_original_image_detail;
use crate::original_image_detail::sanitize_original_image_detail as sanitize_image_detail_items;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::ToolRouter;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolPayload;
use crate::tools::effective_tool_mode;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolCallSource;
use crate::unified_exec::resolve_max_tokens;
use codex_protocol::openai_models::ToolMode;
use codex_tools::ToolName;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::formatted_truncate_text_content_items_with_policy;
use codex_utils_output_truncation::truncate_function_output_items_with_policy;

use delegate::CodeModeDispatchBroker;
use delegate::CodeModeDispatchWorker;
pub(crate) use execute_handler::CodeModeExecuteHandler;
use response_adapter::into_function_call_output_content_items;
pub(crate) use wait_handler::CodeModeWaitHandler;
pub(crate) use workflow_handler::CodeModeWorkflowHandler;
use workflow_handler::WorkflowRunLedger;

pub(crate) const PUBLIC_TOOL_NAME: &str = codex_code_mode::PUBLIC_TOOL_NAME;
pub(crate) const WAIT_TOOL_NAME: &str = codex_code_mode::WAIT_TOOL_NAME;
pub(crate) const DEFAULT_WAIT_YIELD_TIME_MS: u64 = codex_code_mode::DEFAULT_WAIT_YIELD_TIME_MS;
/// Un-namespaced name of the workflow host tool (P0-host-tool-skeleton).
pub(crate) const WORKFLOW_TOOL_NAME: &str = "workflow";

/// Returns true for the un-namespaced code-mode `exec` tool.
pub(crate) fn is_exec_tool_name(tool_name: &ToolName) -> bool {
    tool_name.namespace.is_none() && tool_name.name == PUBLIC_TOOL_NAME
}

/// Returns true for the un-namespaced `workflow` host tool.
pub(crate) fn is_workflow_tool_name(tool_name: &ToolName) -> bool {
    tool_name.namespace.is_none() && tool_name.name == WORKFLOW_TOOL_NAME
}

#[derive(Clone)]
pub(crate) struct ExecContext {
    pub(super) session: Arc<Session>,
    pub(super) turn: Arc<TurnContext>,
}

pub(crate) struct CodeModeService {
    session: OnceCell<Arc<dyn CodeModeSession>>,
    session_provider: Arc<dyn CodeModeSessionProvider>,
    dispatch_broker: Arc<CodeModeDispatchBroker>,
    /// Run→parent ledger shared with the dispatch broker so a nested `workflow()`
    /// call can recover its parent run id (and the broker can forget a closed
    /// cell). See [`WorkflowRunLedger`].
    workflow_run_ledger: Arc<WorkflowRunLedger>,
    shutting_down: AtomicBool,
}

impl CodeModeService {
    pub(crate) fn new(session_provider: Arc<dyn CodeModeSessionProvider>) -> Self {
        let workflow_run_ledger = Arc::new(WorkflowRunLedger::default());
        let dispatch_broker = Arc::new(CodeModeDispatchBroker::new(Arc::clone(
            &workflow_run_ledger,
        )));
        Self {
            session: OnceCell::new(),
            session_provider,
            dispatch_broker,
            workflow_run_ledger,
            shutting_down: AtomicBool::new(false),
        }
    }

    /// The workflow run→parent ledger for this service.
    pub(crate) fn workflow_run_ledger(&self) -> &WorkflowRunLedger {
        &self.workflow_run_ledger
    }

    /// Stage the prior run's journal `agent_call` lines (serialized to JSON) as the
    /// prefix-replay seed for the NEXT cell this service spawns — the resumed top-level
    /// run (`P3-resume-entry`, spec §7 steps 1-3). Must be called immediately before
    /// that run's [`execute`](Self::execute) so the top-level cell (never a nested
    /// `workflow()` cell) consumes it; the seed is taken exactly once. An empty vec
    /// clears any prior staging.
    pub(crate) fn stage_replay_entries(&self, entries: Vec<serde_json::Value>) {
        self.dispatch_broker.stage_replay_entries(entries);
    }

    pub(crate) fn session_provider(&self) -> Arc<dyn CodeModeSessionProvider> {
        Arc::clone(&self.session_provider)
    }

    pub(crate) async fn execute(
        &self,
        request: codex_code_mode::ExecuteRequest,
    ) -> Result<codex_code_mode::StartedCell, String> {
        self.session().await?.execute(request).await
    }

    pub(crate) async fn wait(
        &self,
        request: codex_code_mode::WaitRequest,
    ) -> Result<codex_code_mode::WaitOutcome, String> {
        self.session().await?.wait(request).await
    }

    pub(crate) async fn terminate(
        &self,
        cell_id: CellId,
    ) -> Result<codex_code_mode::WaitOutcome, String> {
        self.session().await?.terminate(cell_id).await
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        self.shutting_down.store(true, Ordering::Release);
        // Join any initialization already in progress without initializing an unused service.
        match self
            .session
            .get_or_try_init(|| async {
                Err::<Arc<dyn CodeModeSession>, String>(
                    "code mode session is shutting down".to_string(),
                )
            })
            .await
        {
            Ok(session) => session.shutdown().await,
            Err(_) => Ok(()),
        }
    }

    pub(crate) fn mark_cell_ready_for_dispatch(&self, cell_id: &codex_code_mode::CellId) {
        self.dispatch_broker.mark_cell_ready_for_dispatch(cell_id);
    }

    pub(crate) fn finish_cell_dispatch(&self, cell_id: &CellId) {
        self.dispatch_broker.close_cell(cell_id);
    }

    pub(crate) fn start_turn_worker(
        &self,
        session: &Arc<Session>,
        step_context: Arc<StepContext>,
        router: Arc<ToolRouter>,
        tracker: SharedTurnDiffTracker,
    ) -> Option<CodeModeDispatchWorker> {
        let turn = &step_context.turn;
        let tool_mode = effective_tool_mode(turn);
        if !matches!(tool_mode, ToolMode::CodeMode | ToolMode::CodeModeOnly) {
            return None;
        }

        let exec = ExecContext {
            session: Arc::clone(session),
            turn: Arc::clone(turn),
        };
        Some(
            self.dispatch_broker
                .start_turn_worker(exec, router, step_context, tracker),
        )
    }

    async fn session(&self) -> Result<Arc<dyn CodeModeSession>, String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("code mode session is shutting down".to_string());
        }
        self.session
            .get_or_try_init(|| async {
                if self.shutting_down.load(Ordering::Acquire) {
                    return Err("code mode session is shutting down".to_string());
                }
                let session = self
                    .session_provider
                    .create_session(self.dispatch_broker.clone())
                    .await?;
                if self.shutting_down.load(Ordering::Acquire) {
                    let _ = session.shutdown().await;
                    return Err("code mode session is shutting down".to_string());
                }
                Ok(session)
            })
            .await
            .map(Arc::clone)
    }
}

pub(super) async fn handle_runtime_response(
    exec: &ExecContext,
    response: RuntimeResponse,
    max_output_tokens: Option<usize>,
    started_at: std::time::Instant,
) -> Result<FunctionToolOutput, String> {
    let script_status = format_script_status(&response);

    match response {
        RuntimeResponse::Yielded { content_items, .. } => {
            let mut content_items = into_function_call_output_content_items(content_items);
            sanitize_runtime_image_detail(exec.turn.as_ref(), &mut content_items);
            content_items = truncate_code_mode_result(content_items, max_output_tokens);
            prepend_script_status(&mut content_items, &script_status, started_at.elapsed());
            Ok(FunctionToolOutput::from_content(content_items, Some(true)))
        }
        RuntimeResponse::Terminated { content_items, .. } => {
            let mut content_items = into_function_call_output_content_items(content_items);
            sanitize_runtime_image_detail(exec.turn.as_ref(), &mut content_items);
            content_items = truncate_code_mode_result(content_items, max_output_tokens);
            prepend_script_status(&mut content_items, &script_status, started_at.elapsed());
            Ok(FunctionToolOutput::from_content(content_items, Some(true)))
        }
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => {
            let mut content_items = into_function_call_output_content_items(content_items);
            sanitize_runtime_image_detail(exec.turn.as_ref(), &mut content_items);
            let success = error_text.is_none();
            if let Some(error_text) = error_text {
                content_items.push(FunctionCallOutputContentItem::InputText {
                    text: format!("Script error:\n{error_text}"),
                });
            }
            content_items = truncate_code_mode_result(content_items, max_output_tokens);
            prepend_script_status(&mut content_items, &script_status, started_at.elapsed());
            Ok(FunctionToolOutput::from_content(
                content_items,
                Some(success),
            ))
        }
    }
}

fn sanitize_runtime_image_detail(turn: &TurnContext, items: &mut [FunctionCallOutputContentItem]) {
    sanitize_image_detail_items(can_request_original_image_detail(&turn.model_info), items);
}

fn format_script_status(response: &RuntimeResponse) -> String {
    match response {
        RuntimeResponse::Yielded { cell_id, .. } => {
            format!("Script running with cell ID {cell_id}")
        }
        RuntimeResponse::Terminated { .. } => "Script terminated".to_string(),
        RuntimeResponse::Result { error_text, .. } => {
            if error_text.is_none() {
                "Script completed".to_string()
            } else {
                "Script failed".to_string()
            }
        }
    }
}

fn prepend_script_status(
    content_items: &mut Vec<FunctionCallOutputContentItem>,
    status: &str,
    wall_time: Duration,
) {
    let wall_time_seconds = ((wall_time.as_secs_f32()) * 10.0).round() / 10.0;
    let header = format!("{status}\nWall time {wall_time_seconds:.1} seconds\nOutput:\n");
    content_items.insert(0, FunctionCallOutputContentItem::InputText { text: header });
}

fn truncate_code_mode_result(
    items: Vec<FunctionCallOutputContentItem>,
    max_output_tokens: Option<usize>,
) -> Vec<FunctionCallOutputContentItem> {
    let max_output_tokens = resolve_max_tokens(max_output_tokens);
    let policy = TruncationPolicy::Tokens(max_output_tokens);
    if items
        .iter()
        .all(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }))
    {
        let (truncated_items, _) =
            formatted_truncate_text_content_items_with_policy(&items, policy);
        return truncated_items;
    }

    truncate_function_output_items_with_policy(&items, policy)
}

async fn call_nested_tool(
    _exec: ExecContext,
    tool_runtime: ToolCallRuntime,
    invocation: CodeModeNestedToolCall,
    cancellation_token: CancellationToken,
) -> Result<JsonValue, FunctionCallError> {
    let CodeModeNestedToolCall {
        cell_id,
        runtime_tool_call_id,
        tool_name,
        tool_kind,
        input,
    } = invocation;
    if is_exec_tool_name(&tool_name) {
        return Err(FunctionCallError::RespondToModel(format!(
            "{PUBLIC_TOOL_NAME} cannot invoke itself"
        )));
    }

    let payload = match build_nested_tool_payload(tool_kind, &tool_name, input) {
        Ok(payload) => payload,
        Err(error) => return Err(FunctionCallError::RespondToModel(error)),
    };

    let call = ToolCall {
        tool_name,
        call_id: format!("{PUBLIC_TOOL_NAME}-{}", uuid::Uuid::new_v4()),
        payload,
    };
    let result = tool_runtime
        .handle_tool_call_with_source(
            call,
            ToolCallSource::CodeMode {
                cell_id: cell_id.to_string(),
                runtime_tool_call_id,
            },
            cancellation_token,
        )
        .await?;
    Ok(result.code_mode_result())
}

fn build_nested_tool_payload(
    tool_kind: CodeModeToolKind,
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    match tool_kind {
        CodeModeToolKind::Function => build_function_tool_payload(tool_name, input),
        CodeModeToolKind::Freeform => build_freeform_tool_payload(tool_name, input),
    }
}

fn build_function_tool_payload(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    let arguments = serialize_function_tool_arguments(tool_name, input)?;
    Ok(ToolPayload::Function { arguments })
}

fn serialize_function_tool_arguments(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<String, String> {
    match input {
        None => Ok("{}".to_string()),
        Some(JsonValue::Object(map)) => serde_json::to_string(&JsonValue::Object(map))
            .map_err(|err| format!("failed to serialize tool `{tool_name}` arguments: {err}")),
        Some(_) => Err(format!(
            "tool `{tool_name}` expects a JSON object for arguments"
        )),
    }
}

fn build_freeform_tool_payload(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    match input {
        Some(JsonValue::String(input)) => Ok(ToolPayload::Custom { input }),
        _ => Err(format!("tool `{tool_name}` expects a string input")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::CodeModeService;
    use super::build_nested_tool_payload;
    use super::truncate_code_mode_result;
    use crate::tools::context::ToolPayload;
    use codex_code_mode::CodeModeToolKind;
    use codex_code_mode::ExecuteRequest;
    use codex_code_mode::FunctionCallOutputContentItem as CodeModeOutputContentItem;
    use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
    use codex_code_mode::RuntimeResponse;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_tools::ToolName;
    use serde_json::json;

    #[test]
    fn build_nested_tool_payload_uses_function_kind() {
        let payload = build_nested_tool_payload(
            CodeModeToolKind::Function,
            &ToolName::plain("example"),
            Some(json!({ "value": 1 })),
        )
        .expect("function payload should serialize");

        match payload {
            ToolPayload::Function { arguments } => {
                assert_eq!(arguments, r#"{"value":1}"#.to_string());
            }
            other => panic!("expected function payload, got {other:?}"),
        }
    }

    #[test]
    fn build_nested_tool_payload_uses_freeform_kind() {
        let payload = build_nested_tool_payload(
            CodeModeToolKind::Freeform,
            &ToolName::plain("example"),
            Some(json!("hello")),
        )
        .expect("freeform payload should preserve string input");

        match payload {
            ToolPayload::Custom { input } => {
                assert_eq!(input, "hello".to_string());
            }
            other => panic!("expected freeform payload, got {other:?}"),
        }
    }

    #[test]
    fn truncated_text_output_starts_with_warning() {
        let items = vec![FunctionCallOutputContentItem::InputText {
            text: "0123456789012345678901234567890123456789".to_string(),
        }];

        assert_eq!(
            truncate_code_mode_result(items, Some(5)),
            vec![FunctionCallOutputContentItem::InputText {
                text: concat!(
                    "Warning: truncated output (original token count: 10)\n",
                    "Total output lines: 1\n\n",
                    "0123456789…5 tokens truncated…0123456789"
                )
                .to_string(),
            }]
        );
    }

    #[test]
    fn workflow_disabled_feature_is_unreachable() {
        use super::workflow_handler::ensure_workflow_enabled;
        use crate::function_tool::FunctionCallError;
        use codex_features::Feature;
        use codex_features::Features;

        let disabled = Features::default();
        let err = ensure_workflow_enabled(&disabled).expect_err("workflow must be gated off");
        assert!(matches!(err, FunctionCallError::RespondToModel(_)));

        let mut enabled = Features::default();
        enabled.enable(Feature::Workflow);
        ensure_workflow_enabled(&enabled).expect("enabled feature is reachable");
    }

    #[test]
    fn workflow_invalid_meta_is_rejected() {
        use super::workflow_handler::validate_workflow_meta;

        // A body with no `export const meta` manifest must be rejected before any
        // isolate execution.
        validate_workflow_meta("text('no meta here');")
            .expect_err("missing meta manifest must be rejected");
        // A computed (non-static) meta is likewise rejected.
        validate_workflow_meta("export const meta = buildMeta();")
            .expect_err("non-static meta manifest must be rejected");
    }

    #[tokio::test]
    async fn workflow_meta_only_body_runs_once_end_to_end() {
        use super::workflow_handler::run_workflow_source;
        use codex_features::Feature;
        use codex_features::Features;

        let service = CodeModeService::new(Arc::new(
            ProcessOwnedCodeModeSessionProvider::with_host_program(
                "codex-code-mode-host-does-not-exist".into(),
            ),
        ));

        let mut features = Features::default();
        features.enable(Feature::Workflow);

        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo workflow' };\n",
            "text('workflow-ran');",
        );

        let scratch = tempfile::tempdir().expect("scratch codex home");
        let output = run_workflow_source(
            &features,
            &service,
            "wf-call-1".to_string(),
            Vec::new(),
            source,
            serde_json::Value::Null,
            super::workflow_handler::WorkflowRunLineage {
                parent_run_id: None,
                depth: 0,
            },
            scratch.path(),
            0,
            None,
        )
        .await
        .expect("valid workflow runs its body once");

        assert_eq!(
            output.response,
            RuntimeResponse::Result {
                cell_id: codex_code_mode::CellId::new("1".to_string()),
                content_items: vec![CodeModeOutputContentItem::InputText {
                    text: "workflow-ran".to_string(),
                }],
                error_text: None,
            }
        );
        service.shutdown().await.expect("shutdown service");
    }

    #[tokio::test]
    async fn workflow_args_and_run_id_reach_the_isolate() {
        use super::workflow_handler::run_workflow_source;
        use codex_features::Feature;
        use codex_features::Features;

        let service = CodeModeService::new(Arc::new(
            ProcessOwnedCodeModeSessionProvider::with_host_program(
                "codex-code-mode-host-does-not-exist".into(),
            ),
        ));

        let mut features = Features::default();
        features.enable(Feature::Workflow);

        // The invocation JSON reaches the fresh isolate as the read-only `args`
        // global, and the host-minted `workflow.runId` is a non-empty uuid the
        // body can read (minted in Rust by `run_workflow_source`, never in JS).
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo' };\n",
            "text(String(args.foo));\n",
            "text(String(typeof workflow.runId === 'string' && workflow.runId.length > 0));\n",
        );

        let scratch = tempfile::tempdir().expect("scratch codex home");
        let output = run_workflow_source(
            &features,
            &service,
            "wf-call-args".to_string(),
            Vec::new(),
            source,
            serde_json::json!({ "foo": "from-invocation" }),
            super::workflow_handler::WorkflowRunLineage {
                parent_run_id: None,
                depth: 0,
            },
            scratch.path(),
            0,
            None,
        )
        .await
        .expect("workflow with args runs its body once");

        assert_eq!(
            output.response,
            RuntimeResponse::Result {
                cell_id: codex_code_mode::CellId::new("1".to_string()),
                content_items: vec![
                    CodeModeOutputContentItem::InputText {
                        text: "from-invocation".to_string(),
                    },
                    CodeModeOutputContentItem::InputText {
                        text: "true".to_string(),
                    },
                ],
                error_text: None,
            }
        );
        service.shutdown().await.expect("shutdown service");
    }

    #[tokio::test]
    async fn workflow_phase_and_log_body_runs_end_to_end() {
        use super::workflow_handler::run_workflow_source;
        use codex_features::Feature;
        use codex_features::Features;

        let service = CodeModeService::new(Arc::new(
            ProcessOwnedCodeModeSessionProvider::with_host_program(
                "codex-code-mode-host-does-not-exist".into(),
            ),
        ));

        let mut features = Features::default();
        features.enable(Feature::Workflow);

        // A `phase()`/`log()`-only workflow (no `agent()`) must run body-once to
        // completion: reaching the trailing `text(...)` proves the workflow
        // narrator globals were installed for this run and did not throw.
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo', phases: ['plan'] };\n",
            "phase('plan');\n",
            "log('starting');\n",
            "text('workflow-ran');\n",
        );

        let scratch = tempfile::tempdir().expect("scratch codex home");
        let output = run_workflow_source(
            &features,
            &service,
            "wf-call-phase-log".to_string(),
            Vec::new(),
            source,
            serde_json::Value::Null,
            super::workflow_handler::WorkflowRunLineage {
                parent_run_id: None,
                depth: 0,
            },
            scratch.path(),
            0,
            None,
        )
        .await
        .expect("phase/log workflow runs its body once");

        assert_eq!(
            output.response,
            RuntimeResponse::Result {
                cell_id: codex_code_mode::CellId::new("1".to_string()),
                content_items: vec![CodeModeOutputContentItem::InputText {
                    text: "workflow-ran".to_string(),
                }],
                error_text: None,
            }
        );
        service.shutdown().await.expect("shutdown service");
    }

    #[tokio::test]
    async fn workflow_invalid_meta_never_reaches_isolate() {
        use super::workflow_handler::run_workflow_source;
        use codex_features::Feature;
        use codex_features::Features;

        let service = CodeModeService::new(Arc::new(
            ProcessOwnedCodeModeSessionProvider::with_host_program(
                "codex-code-mode-host-does-not-exist".into(),
            ),
        ));

        let mut features = Features::default();
        features.enable(Feature::Workflow);

        // No `meta` manifest: rejected before the isolate ever runs. If it had run,
        // `text(...)` would have produced a `Result` output instead of an error.
        let scratch = tempfile::tempdir().expect("scratch codex home");
        let err = run_workflow_source(
            &features,
            &service,
            "wf-call-2".to_string(),
            Vec::new(),
            "text('should-not-run');",
            serde_json::Value::Null,
            super::workflow_handler::WorkflowRunLineage {
                parent_run_id: None,
                depth: 0,
            },
            scratch.path(),
            0,
            None,
        )
        .await
        .expect_err("invalid meta must be rejected");
        match err {
            crate::function_tool::FunctionCallError::RespondToModel(message) => {
                assert!(
                    message.contains("meta"),
                    "expected a meta rejection, got: {message}"
                );
            }
            other => panic!("expected RespondToModel, got {other:?}"),
        }
        service.shutdown().await.expect("shutdown service");
    }

    #[tokio::test]
    async fn workflow_disabled_feature_skips_execution() {
        use super::workflow_handler::run_workflow_source;
        use codex_features::Features;

        let service = CodeModeService::new(Arc::new(
            ProcessOwnedCodeModeSessionProvider::with_host_program(
                "codex-code-mode-host-does-not-exist".into(),
            ),
        ));

        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo workflow' };\n",
            "text('workflow-ran');",
        );

        let scratch = tempfile::tempdir().expect("scratch codex home");
        run_workflow_source(
            &Features::default(),
            &service,
            "wf-call-3".to_string(),
            Vec::new(),
            source,
            serde_json::Value::Null,
            super::workflow_handler::WorkflowRunLineage {
                parent_run_id: None,
                depth: 0,
            },
            scratch.path(),
            0,
            None,
        )
        .await
        .expect_err("workflow is unreachable when the feature is disabled");
        service.shutdown().await.expect("shutdown service");
    }

    #[tokio::test]
    async fn missing_process_host_falls_back_to_in_process_session() {
        let service = CodeModeService::new(Arc::new(
            ProcessOwnedCodeModeSessionProvider::with_host_program(
                "codex-code-mode-host-does-not-exist".into(),
            ),
        ));

        let response = service
            .execute(ExecuteRequest {
                tool_call_id: "call-1".to_string(),
                enabled_tools: Vec::new(),
                source: "text('fallback')".to_string(),
                yield_time_ms: None,
                max_output_tokens: None,
                workflow: false,
                args: None,
                run_id: None,
            })
            .await
            .expect("missing host should fall back to an in-process session")
            .initial_response()
            .await
            .expect("read fallback response");

        assert_eq!(
            response,
            RuntimeResponse::Result {
                cell_id: codex_code_mode::CellId::new("1".to_string()),
                content_items: vec![CodeModeOutputContentItem::InputText {
                    text: "fallback".to_string(),
                }],
                error_text: None,
            }
        );
        service.shutdown().await.expect("shutdown service");
    }

    /// Drive [`super::CodeModeWorkflowHandler`] end-to-end through its
    /// [`crate::tools::registry::ToolExecutor`] surface — the same path that
    /// `build_code_mode_executors` registers the tool on. Unlike the
    /// `run_workflow_source` tests, this exercises payload matching, the
    /// per-session `code_mode_service`, and the model-facing response adapter
    /// (`to_response_item`), so a registration/gating/adaptation regression that
    /// leaves `run_workflow_source` intact would still be caught. Returns the
    /// model-facing [`ResponseInputItem`] the tool would emit.
    async fn dispatch_workflow_via_handler(
        workflow_enabled: bool,
        source: &str,
    ) -> Result<codex_protocol::models::ResponseInputItem, crate::function_tool::FunctionCallError>
    {
        use super::CodeModeWorkflowHandler;
        use super::workflow_spec::create_workflow_tool;
        use crate::session::step_context::StepContext;
        use crate::session::tests::make_session_and_context;
        use crate::tools::context::ToolCallSource;
        use crate::tools::context::ToolInvocation;
        use crate::tools::context::ToolOutput;
        use crate::tools::registry::ToolExecutor;
        use crate::turn_diff_tracker::TurnDiffTracker;
        use codex_features::Feature;

        let (session, mut turn) = make_session_and_context().await;
        let mut config = (*turn.config).clone();
        if workflow_enabled {
            config
                .features
                .enable(Feature::Workflow)
                .expect("test feature should be enableable in config");
        } else {
            config
                .features
                .disable(Feature::Workflow)
                .expect("test feature should be disableable in config");
        }
        turn.config = Arc::new(config);

        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let step_context = StepContext::for_test(Arc::clone(&turn));

        // Register the handler exactly as `build_code_mode_executors` does: the
        // model-visible spec plus the (here empty) nested-tool specs.
        let handler = CodeModeWorkflowHandler::new(create_workflow_tool(), Vec::new());

        let payload = ToolPayload::Custom {
            input: source.to_string(),
        };
        let invocation = ToolInvocation {
            session: Arc::clone(&session),
            turn: Arc::clone(&turn),
            step_context,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
            call_id: "wf-handler-call".to_string(),
            tool_name: ToolName::plain(super::WORKFLOW_TOOL_NAME),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        };

        let result = handler.handle(invocation).await;
        session
            .services
            .code_mode_service
            .shutdown()
            .await
            .expect("shutdown service");
        result.map(|output| output.to_response_item("wf-handler-call", &payload))
    }

    /// Pull the `(text, success)` out of the custom-tool output the workflow tool
    /// emits, so a test can assert on the model-facing rendering.
    fn custom_tool_output(
        item: codex_protocol::models::ResponseInputItem,
    ) -> (String, Option<bool>) {
        match item {
            codex_protocol::models::ResponseInputItem::CustomToolCallOutput { output, .. } => {
                (output.body.to_text().unwrap_or_default(), output.success)
            }
            other => panic!("expected a custom-tool call output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn workflow_handler_dispatches_valid_script_through_model_adapter() {
        // (a) With the feature enabled, a trivial meta-valid script dispatched
        // through the registered tool surface runs its body once and returns the
        // isolate result via the model-facing adapter.
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo workflow' };\n",
            "text('workflow-ran');",
        );

        let item = dispatch_workflow_via_handler(/*workflow_enabled=*/ true, source)
            .await
            .expect("enabled workflow tool dispatches a valid script");
        let (text, success) = custom_tool_output(item);

        assert_eq!(success, Some(true), "model-facing success flag");
        assert!(
            text.contains("workflow-ran"),
            "model-facing output should carry the isolate result, got: {text}"
        );
        assert!(
            text.contains("Script completed"),
            "model-facing output should carry the adapter status header, got: {text}"
        );
    }

    #[tokio::test]
    async fn workflow_handler_is_unreachable_when_feature_disabled() {
        // (b) With the feature disabled the tool is unreachable: even a valid
        // script is rejected at the handler surface instead of dispatching.
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo workflow' };\n",
            "text('workflow-ran');",
        );

        let err = dispatch_workflow_via_handler(/*workflow_enabled=*/ false, source)
            .await
            .expect_err("disabled workflow tool must be unreachable");
        match err {
            crate::function_tool::FunctionCallError::RespondToModel(message) => {
                assert!(
                    message.contains("workflow"),
                    "expected a feature-gate rejection, got: {message}"
                );
            }
            other => panic!("expected RespondToModel, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn workflow_handler_rejects_invalid_meta_before_isolate() {
        // (c) With the feature enabled, an invalid-meta script is rejected at the
        // handler surface before the isolate runs. If it had reached the isolate,
        // `text(...)` would have produced a successful custom-tool output.
        let err = dispatch_workflow_via_handler(
            /*workflow_enabled=*/ true,
            "text('should-not-run');",
        )
        .await
        .expect_err("invalid meta must be rejected before isolate execution");
        match err {
            crate::function_tool::FunctionCallError::RespondToModel(message) => {
                assert!(
                    message.contains("meta"),
                    "expected a meta rejection, got: {message}"
                );
            }
            other => panic!("expected RespondToModel, got {other:?}"),
        }
    }

    /// Drive the real tool-planning path (`build_code_mode_executors` via
    /// [`crate::tools::router::ToolRouter::from_context`]) so the workflow tool's
    /// registration/gating at `spec_plan.rs` is exercised — not just the handler
    /// constructed directly. Returns the names of every tool the router
    /// registered. `Feature::CodeMode` is always enabled so the code-mode
    /// executors (and hence the workflow gate) are reached at all.
    async fn registered_tool_names(workflow_enabled: bool) -> Vec<String> {
        use crate::session::step_context::StepContext;
        use crate::session::tests::make_session_and_context;
        use crate::tools::router::ToolRouter;
        use crate::tools::router::ToolRouterParams;
        use codex_features::Feature;

        let (_session, mut turn) = make_session_and_context().await;
        let mut config = (*turn.config).clone();
        config
            .features
            .enable(Feature::CodeMode)
            .expect("code_mode feature should be enableable in config");
        if workflow_enabled {
            config
                .features
                .enable(Feature::Workflow)
                .expect("workflow feature should be enableable in config");
        }
        turn.config = Arc::new(config);

        let turn = Arc::new(turn);
        let step_context = StepContext::for_test(Arc::clone(&turn));
        let router = ToolRouter::from_context(
            step_context.as_ref(),
            ToolRouterParams {
                mcp_tools: None,
                deferred_mcp_tools: None,
                tool_suggest_candidates: None,
                extension_tool_executors: Vec::new(),
                dynamic_tools: &[],
            },
            &Default::default(),
        );
        router
            .registered_tool_names_for_test()
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[tokio::test]
    async fn build_code_mode_executors_registers_workflow_tool_when_feature_enabled() {
        // (a) With `Feature::Workflow` enabled the planning path registers the
        // dispatchable `workflow` tool.
        let names = registered_tool_names(/*workflow_enabled=*/ true).await;
        assert!(
            names.iter().any(|name| name == super::WORKFLOW_TOOL_NAME),
            "workflow tool must be registered when the feature is enabled, got: {names:?}"
        );
        // Sanity: the code-mode exec tool is registered too, proving we actually
        // reached `build_code_mode_executors` (rather than the Direct path).
        assert!(
            names.iter().any(|name| name == super::PUBLIC_TOOL_NAME),
            "code-mode exec tool should be registered, got: {names:?}"
        );
    }

    #[tokio::test]
    async fn build_code_mode_executors_omits_workflow_tool_when_feature_disabled() {
        // (b) With `Feature::Workflow` disabled the planning path must NOT register
        // the workflow tool, even though code mode is on.
        let names = registered_tool_names(/*workflow_enabled=*/ false).await;
        assert!(
            !names.iter().any(|name| name == super::WORKFLOW_TOOL_NAME),
            "workflow tool must be absent when the feature is disabled, got: {names:?}"
        );
        assert!(
            names.iter().any(|name| name == super::PUBLIC_TOOL_NAME),
            "code-mode exec tool should still be registered, got: {names:?}"
        );
    }
}
