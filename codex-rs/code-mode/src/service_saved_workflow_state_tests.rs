use super::InProcessCodeModeSession;
use super::RuntimeResponse;
use super::protocol_cell_id;
use super::runtime_request;
use super::runtime_response;
use super::tests::cell_id;
use super::tests::execute;
use super::tests::execute_request;
use crate::ExecuteOutputPolicy;
use crate::ExecuteRequest;
use crate::FunctionCallOutputContentItem;
use crate::session_runtime as runtime;
use codex_code_mode_protocol::SAVED_WORKFLOW_OUTPUT_POLICY_UNAVAILABLE;
use pretty_assertions::assert_eq;

async fn execute_saved(
    service: &InProcessCodeModeSession,
    request: ExecuteRequest,
) -> RuntimeResponse {
    let started = service
        .runtime
        .execute(
            runtime_request(request),
            runtime::ObserveMode::PendingFrontier,
        )
        .await
        .expect("start saved cell");
    let cell_id = protocol_cell_id(&started.cell_id);
    let event = started.initial_event().await.expect("saved cell event");
    runtime_response(&cell_id, event).expect("saved cell response")
}

#[tokio::test]
async fn saved_workflow_stored_values_are_cell_local() {
    let service = InProcessCodeModeSession::new();

    let seed = execute(
        &service,
        ExecuteRequest {
            source: r#"store("shared", "ordinary");"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;
    let saved = execute_saved(
        &service,
        ExecuteRequest {
            source: r#"
store("private", "inside");
text(String(load("private")));
text(String(load("shared")));
"#
            .to_string(),
            output_policy: ExecuteOutputPolicy::SavedWorkflow,
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;
    let ordinary = execute(
        &service,
        ExecuteRequest {
            source: r#"text(`${String(load("shared"))}/${String(load("private"))}`);"#.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        seed,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: Vec::new(),
            error_text: None,
        }
    );
    assert_eq!(
        saved,
        RuntimeResponse::Result {
            cell_id: cell_id("2"),
            content_items: vec![
                FunctionCallOutputContentItem::InputText {
                    text: "inside".to_string(),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "undefined".to_string(),
                },
            ],
            error_text: None,
        }
    );
    assert_eq!(
        ordinary,
        RuntimeResponse::Result {
            cell_id: cell_id("3"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "ordinary/undefined".to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn saved_workflow_omits_wall_clock_timer_globals() {
    let service = InProcessCodeModeSession::new();
    let source = r#"text(JSON.stringify([typeof setTimeout, typeof clearTimeout]));"#;

    let ordinary = execute(
        &service,
        ExecuteRequest {
            source: source.to_string(),
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;
    let saved = execute_saved(
        &service,
        ExecuteRequest {
            source: source.to_string(),
            output_policy: ExecuteOutputPolicy::SavedWorkflow,
            yield_time_ms: None,
            ..execute_request("")
        },
    )
    .await;

    assert_eq!(
        ordinary,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: r#"["function","function"]"#.to_string(),
            }],
            error_text: None,
        }
    );
    assert_eq!(
        saved,
        RuntimeResponse::Result {
            cell_id: cell_id("2"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: r#"["undefined","undefined"]"#.to_string(),
            }],
            error_text: None,
        }
    );
}

#[tokio::test]
async fn public_execute_entrypoints_reject_saved_workflow_before_starting_a_cell() {
    let service = InProcessCodeModeSession::new();
    let saved_request = || ExecuteRequest {
        source: r#"text("unreachable");"#.to_string(),
        output_policy: ExecuteOutputPolicy::SavedWorkflow,
        yield_time_ms: None,
        ..execute_request("")
    };
    let execute_error = service.execute(saved_request()).await.err();
    let pending_error = service.execute_to_pending(saved_request()).await.err();

    assert_eq!(
        [execute_error.as_deref(), pending_error.as_deref()],
        [Some(SAVED_WORKFLOW_OUTPUT_POLICY_UNAVAILABLE); 2]
    );
    assert_eq!(
        execute(
            &service,
            ExecuteRequest {
                source: r#"text("ordinary");"#.to_string(),
                yield_time_ms: None,
                ..execute_request("")
            },
        )
        .await,
        RuntimeResponse::Result {
            cell_id: cell_id("1"),
            content_items: vec![FunctionCallOutputContentItem::InputText {
                text: "ordinary".to_string(),
            }],
            error_text: None,
        }
    );
}
