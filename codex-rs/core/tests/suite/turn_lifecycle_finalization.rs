use anyhow::Result;
use codex_protocol::AgentPath;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::strip_metadata_from_json;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn triggering_agent_mail_starts_one_fifo_turn_and_releases_the_slot() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("trigger-response"),
                ev_completed("trigger-response"),
            ]),
            sse(vec![
                ev_response_created("public-response"),
                ev_completed("public-response"),
            ]),
        ],
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;

    test.codex
        .submit(Op::InterAgentCommunication {
            communication: InterAgentCommunication::new(
                AgentPath::try_from("/root/first").expect("first author path should parse"),
                AgentPath::root(),
                Vec::new(),
                "queued first".to_string(),
                /*trigger_turn*/ false,
            ),
        })
        .await?;
    test.codex
        .submit(Op::InterAgentCommunication {
            communication: InterAgentCommunication::new(
                AgentPath::try_from("/root/second").expect("second author path should parse"),
                AgentPath::root(),
                Vec::new(),
                "trigger second".to_string(),
                /*trigger_turn*/ true,
            ),
        })
        .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let trigger_requests = responses.requests();
    assert_eq!(trigger_requests.len(), 1);
    assert_eq!(
        strip_metadata_from_json(Value::Array(
            trigger_requests[0].inputs_of_type("agent_message")
        )),
        json!([
            {
                "type": "agent_message",
                "author": "/root/first",
                "recipient": "/root",
                "content": [{"type": "input_text", "text": "queued first"}],
            },
            {
                "type": "agent_message",
                "author": "/root/second",
                "recipient": "/root",
                "content": [{"type": "input_text", "text": "trigger second"}],
            },
        ])
    );

    test.submit_turn("after triggered mail").await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1]
            .message_input_texts("user")
            .last()
            .map(String::as_str),
        Some("after triggered mail")
    );
    Ok(())
}
