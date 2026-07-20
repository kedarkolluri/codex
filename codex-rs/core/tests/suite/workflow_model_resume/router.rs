use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

use super::WORKFLOW_NAME;

const CHILD_PROMPT_PREFIX: &str = "MODEL_RESUME_CHILD_";
pub(super) const RAW_CHILD_COMPLETION: &str = "MODEL_RESUME_RAW_CHILD_COMPLETION";

struct ToolPlan {
    args: Value,
    resume_from_run_id: Option<String>,
}

#[derive(Default)]
struct RouterState {
    plans: VecDeque<ToolPlan>,
    active_call_id: Option<String>,
    sequence: usize,
    tool_outputs: Vec<String>,
    parent_requests: Vec<Value>,
    child_prompts: Vec<String>,
}

#[derive(Clone, Default)]
pub(super) struct ModelRouter {
    state: Arc<Mutex<RouterState>>,
}

impl ModelRouter {
    pub(super) fn enqueue(&self, args: Value, resume_from_run_id: Option<&str>) -> usize {
        let mut state = self.state.lock().unwrap();
        assert!(
            state.plans.is_empty(),
            "only one parent turn is scripted at a time"
        );
        assert!(
            state.active_call_id.is_none(),
            "the prior tool call must be settled"
        );
        let output_index = state.tool_outputs.len();
        state.plans.push_back(ToolPlan {
            args,
            resume_from_run_id: resume_from_run_id.map(str::to_string),
        });
        output_index
    }

    pub(super) fn output(&self, index: usize) -> String {
        self.state.lock().unwrap().tool_outputs[index].clone()
    }

    pub(super) fn child_prompts(&self) -> Vec<String> {
        self.state.lock().unwrap().child_prompts.clone()
    }

    pub(super) fn parent_requests(&self) -> Vec<Value> {
        self.state.lock().unwrap().parent_requests.clone()
    }

    pub(super) fn tool_outputs(&self) -> Vec<String> {
        self.state.lock().unwrap().tool_outputs.clone()
    }
}

impl Respond for ModelRouter {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body = request_body_json(request);
        let mut state = self.state.lock().unwrap();
        state.sequence += 1;
        let sequence = state.sequence;

        if let Some(prompt) = child_prompt(&body) {
            state.child_prompts.push(prompt.clone());
            drop(state);
            let completion = format!("{RAW_CHILD_COMPLETION}:{prompt}:{}", "x".repeat(12_000));
            return sse_response(sse(vec![
                ev_response_created(&format!("resp-child-{sequence}")),
                ev_assistant_message(&format!("msg-child-{sequence}"), &completion),
                ev_completed(&format!("resp-child-{sequence}")),
            ]));
        }

        state.parent_requests.push(body.clone());
        if let Some(call_id) = state.active_call_id.clone()
            && let Some(output) = function_output_text(&body, &call_id)
        {
            state.tool_outputs.push(output);
            state.active_call_id = None;
            drop(state);
            return sse_response(sse(vec![
                ev_response_created(&format!("resp-parent-close-{sequence}")),
                ev_assistant_message(
                    &format!("msg-parent-close-{sequence}"),
                    "workflow tool observed",
                ),
                ev_completed(&format!("resp-parent-close-{sequence}")),
            ]));
        }

        let plan = state
            .plans
            .pop_front()
            .expect("unexpected unscripted parent model request");
        let call_id = format!("call-workflow-{sequence}");
        state.active_call_id = Some(call_id.clone());
        drop(state);

        let mut arguments = json!({ "name": WORKFLOW_NAME, "args": plan.args });
        if let Some(run_id) = plan.resume_from_run_id {
            arguments["resumeFromRunId"] = json!(run_id);
        }
        sse_response(sse(vec![
            ev_response_created(&format!("resp-parent-open-{sequence}")),
            ev_function_call(&call_id, "workflow_run", &arguments.to_string()),
            ev_completed(&format!("resp-parent-open-{sequence}")),
        ]))
    }
}

fn request_body_json(request: &Request) -> Value {
    let zstd_encoded = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|encoding| encoding.trim().eq_ignore_ascii_case("zstd"))
        });
    let body = if zstd_encoded {
        zstd::stream::decode_all(std::io::Cursor::new(request.body.as_slice()))
            .expect("decode zstd request")
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&body).expect("request body is JSON")
}

fn child_prompt(body: &Value) -> Option<String> {
    body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("message")
                && item.get("role").and_then(Value::as_str) == Some("user")
        })
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .find_map(|text| {
            text.find(CHILD_PROMPT_PREFIX)
                .map(|start| text[start..].lines().next().unwrap_or_default().to_string())
        })
}

fn function_output_text(body: &Value, call_id: &str) -> Option<String> {
    let output = body["input"].as_array()?.iter().find_map(|item| {
        (item.get("type").and_then(Value::as_str) == Some("function_call_output")
            && item.get("call_id").and_then(Value::as_str) == Some(call_id))
        .then(|| item.get("output"))
        .flatten()
    })?;
    match output {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        other => Some(other.to_string()),
    }
}

pub(super) async fn mount_router(server: &MockServer) -> ModelRouter {
    let router = ModelRouter::default();
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(router.clone())
        .mount(server)
        .await;
    router
}
