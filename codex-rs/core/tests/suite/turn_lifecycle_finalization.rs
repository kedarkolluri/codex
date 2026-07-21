use anyhow::Result;
use codex_protocol::protocol::EventMsg;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successive_public_turns_release_the_lifecycle_slot() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("response-1"),
                ev_completed("response-1"),
            ]),
            sse(vec![
                ev_response_created("response-2"),
                ev_completed("response-2"),
            ]),
        ],
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;

    test.codex
        .try_start_turn_if_idle(vec![responses::user_message_item("automatic turn")])
        .await
        .expect("idle automatic turn should start");
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.submit_turn("second turn").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let first_user_messages = requests[0].message_input_texts("user");
    assert_eq!(
        first_user_messages.last().map(String::as_str),
        Some("automatic turn")
    );
    Ok(())
}
