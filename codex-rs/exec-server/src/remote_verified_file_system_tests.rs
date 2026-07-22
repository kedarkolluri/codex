#![allow(clippy::expect_used)]

use std::io;

use bytes::Bytes;
use codex_exec_server_protocol::JSONRPCError;
use codex_exec_server_protocol::JSONRPCErrorError;
use codex_exec_server_protocol::JSONRPCMessage;
use codex_exec_server_protocol::JSONRPCRequest;
use codex_exec_server_protocol::JSONRPCResponse;
use codex_exec_server_protocol::RequestId;
use codex_utils_path_uri::PathUri;
use futures::SinkExt;
use futures::StreamExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use super::*;
use crate::FILE_READ_CHUNK_SIZE;
use crate::client_api::DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT;
use crate::client_api::ExecServerTransportParams;
use crate::protocol::FS_CLOSE_METHOD;
use crate::protocol::FS_OPEN_VERIFIED_METHOD;
use crate::protocol::FS_READ_BLOCK_METHOD;
use crate::protocol::FsCloseParams;
use crate::protocol::FsCloseResponse;
use crate::protocol::FsOpenVerifiedParams;
use crate::protocol::FsOpenVerifiedResponse;
use crate::protocol::FsReadBlockParams;
use crate::protocol::FsReadBlockResponse;
use crate::protocol::INITIALIZE_METHOD;
use crate::protocol::INITIALIZED_METHOD;
use crate::protocol::InitializeResponse;

#[tokio::test]
async fn verified_read_uses_the_distinct_bounded_stream_protocol() {
    let (websocket_url, listener) = listen().await;
    let path = PathUri::parse("file:///C:/Users/Alice/workflow.js").expect("workflow URI");
    let expected_path = path.clone();
    let capture_size = FILE_READ_CHUNK_SIZE as u64 + 1;
    let server = tokio::spawn(async move {
        let mut websocket = accept_initialized(listener).await;
        let handle_id =
            respond_to_open(&mut websocket, expected_path, capture_size, capture_size).await;
        respond_to_read(
            &mut websocket,
            &handle_id,
            /*offset*/ 0,
            FILE_READ_CHUNK_SIZE,
            vec![b'a'; FILE_READ_CHUNK_SIZE],
            /*eof*/ false,
        )
        .await;
        respond_to_read(
            &mut websocket,
            &handle_id,
            FILE_READ_CHUNK_SIZE as u64,
            /*len*/ 1,
            b"z".to_vec(),
            /*eof*/ false,
        )
        .await;
        respond_to_close(&mut websocket, handle_id).await;
    });
    let file_system = remote_file_system(&websocket_url);

    let capture = file_system
        .read_file_verified(
            &path,
            VerifiedFileReadOptions {
                max_bytes: capture_size,
            },
            /*sandbox*/ None,
        )
        .await
        .expect("verified read should open");
    assert_eq!(capture.size, capture_size);
    let chunks = capture
        .stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .expect("verified stream should complete");
    assert_eq!(
        chunks.iter().map(Bytes::len).collect::<Vec<_>>(),
        vec![FILE_READ_CHUNK_SIZE, 1]
    );
    assert_eq!(chunks[1], Bytes::from_static(b"z"));
    server.await.expect("recording server should succeed");
}

#[tokio::test]
async fn missing_verified_read_support_fails_closed_without_fallback() {
    let (websocket_url, listener) = listen().await;
    let path = PathUri::parse("file://server/share/workflow.js").expect("workflow URI");
    let expected_path = path.clone();
    let server = tokio::spawn(async move {
        let mut websocket = accept_initialized(listener).await;
        let (request_id, params) =
            read_request_params::<FsOpenVerifiedParams>(&mut websocket, FS_OPEN_VERIFIED_METHOD)
                .await;
        assert_eq!(params.path, expected_path);
        assert_eq!(params.max_bytes, 1_024);
        write_message(
            &mut websocket,
            JSONRPCMessage::Error(JSONRPCError {
                id: request_id,
                error: JSONRPCErrorError {
                    code: -32601,
                    message: "method not found".to_string(),
                    data: None,
                },
            }),
        )
        .await;

        assert!(
            timeout(Duration::from_millis(100), read_message(&mut websocket))
                .await
                .is_err(),
            "unsupported verified read must not issue cleanup or ordinary-read fallback requests"
        );
    });
    let file_system = remote_file_system(&websocket_url);

    let result = file_system
        .read_file_verified(
            &path,
            VerifiedFileReadOptions { max_bytes: 1_024 },
            /*sandbox*/ None,
        )
        .await;
    let Err(error) = result else {
        panic!("missing verified-read support should fail");
    };
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert_eq!(
        error.to_string(),
        "verified file reads are not supported by this executor"
    );
    server.await.expect("recording server should succeed");
}

#[tokio::test]
async fn malformed_verified_open_response_releases_the_requested_handle() {
    let (websocket_url, listener) = listen().await;
    let path = PathUri::parse("file:///workspace/malformed.js").expect("workflow URI");
    let server = tokio::spawn(async move {
        let mut websocket = accept_initialized(listener).await;
        let (request_id, params) =
            read_request_params::<FsOpenVerifiedParams>(&mut websocket, FS_OPEN_VERIFIED_METHOD)
                .await;
        write_response(
            &mut websocket,
            request_id,
            serde_json::json!({
                "handleId": params.handle_id.clone(),
                "size": "not-a-number",
            }),
        )
        .await;
        respond_to_close(&mut websocket, params.handle_id).await;
    });
    let file_system = remote_file_system(&websocket_url);

    let result = file_system
        .read_file_verified(
            &path,
            VerifiedFileReadOptions { max_bytes: 64 },
            /*sandbox*/ None,
        )
        .await;
    let Err(error) = result else {
        panic!("malformed verified-open response should fail");
    };
    assert_eq!(error.kind(), io::ErrorKind::Other);
    server.await.expect("recording server should succeed");
}

#[tokio::test]
async fn verified_stream_rejects_size_drift_and_releases_every_handle() {
    let (websocket_url, listener) = listen().await;
    let path = PathUri::parse("file:///workspace/drift.js").expect("workflow URI");
    let expected_path = path.clone();
    let (cleanup_tx, mut cleanup_rx) = mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let mut websocket = accept_initialized(listener).await;

        let handle_id = respond_to_open(
            &mut websocket,
            expected_path.clone(),
            /*max_bytes*/ 0,
            /*size*/ 0,
        )
        .await;
        respond_to_close(&mut websocket, handle_id).await;
        cleanup_tx.send(()).expect("cleanup receiver should remain");

        let handle_id = respond_to_open(
            &mut websocket,
            expected_path.clone(),
            /*max_bytes*/ 3,
            /*size*/ 3,
        )
        .await;
        respond_to_read(
            &mut websocket,
            &handle_id,
            /*offset*/ 0,
            /*len*/ 3,
            b"ab".to_vec(),
            /*eof*/ true,
        )
        .await;
        respond_to_close(&mut websocket, handle_id).await;
        cleanup_tx.send(()).expect("cleanup receiver should remain");

        let handle_id = respond_to_open(
            &mut websocket,
            expected_path.clone(),
            /*max_bytes*/ 2,
            /*size*/ 2,
        )
        .await;
        respond_to_read(
            &mut websocket,
            &handle_id,
            /*offset*/ 0,
            /*len*/ 2,
            b"abc".to_vec(),
            /*eof*/ false,
        )
        .await;
        respond_to_close(&mut websocket, handle_id).await;
        cleanup_tx.send(()).expect("cleanup receiver should remain");

        let handle_id = respond_to_open(
            &mut websocket,
            expected_path,
            /*max_bytes*/ 3,
            /*size*/ 3,
        )
        .await;
        respond_to_close(&mut websocket, handle_id).await;
        cleanup_tx.send(()).expect("cleanup receiver should remain");
    });
    let file_system = remote_file_system(&websocket_url);

    let empty = file_system
        .read_file_verified(
            &path,
            VerifiedFileReadOptions { max_bytes: 0 },
            /*sandbox*/ None,
        )
        .await
        .expect("empty verified read should open");
    assert_eq!(empty.size, 0);
    assert!(empty.stream.collect::<Vec<_>>().await.is_empty());
    cleanup_rx.recv().await.expect("empty capture should close");

    let mut truncated = file_system
        .read_file_verified(
            &path,
            VerifiedFileReadOptions { max_bytes: 3 },
            /*sandbox*/ None,
        )
        .await
        .expect("truncated verified read should open")
        .stream;
    let error = truncated
        .next()
        .await
        .expect("truncated stream should return an error")
        .expect_err("truncated stream should fail");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    cleanup_rx
        .recv()
        .await
        .expect("truncated capture should close");

    let mut overrun = file_system
        .read_file_verified(
            &path,
            VerifiedFileReadOptions { max_bytes: 2 },
            /*sandbox*/ None,
        )
        .await
        .expect("overrun verified read should open")
        .stream;
    let error = overrun
        .next()
        .await
        .expect("overrun stream should return an error")
        .expect_err("overrun stream should fail");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    cleanup_rx
        .recv()
        .await
        .expect("overrun capture should close");

    let unconsumed = file_system
        .read_file_verified(
            &path,
            VerifiedFileReadOptions { max_bytes: 3 },
            /*sandbox*/ None,
        )
        .await
        .expect("unconsumed verified read should open");
    drop(unconsumed);
    cleanup_rx
        .recv()
        .await
        .expect("unconsumed capture should close");
    server.await.expect("recording server should succeed");
}

async fn listen() -> (String, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let websocket_url = format!("ws://{}", listener.local_addr().expect("listener address"));
    (websocket_url, listener)
}

fn remote_file_system(websocket_url: &str) -> RemoteFileSystem {
    RemoteFileSystem::new(LazyRemoteExecServerClient::new(
        ExecServerTransportParams::websocket_url(
            websocket_url.to_string(),
            DEFAULT_REMOTE_EXEC_SERVER_CONNECT_TIMEOUT,
        ),
    ))
}

async fn respond_to_open(
    websocket: &mut WebSocketStream<TcpStream>,
    expected_path: PathUri,
    max_bytes: u64,
    size: u64,
) -> String {
    let (request_id, params) =
        read_request_params::<FsOpenVerifiedParams>(websocket, FS_OPEN_VERIFIED_METHOD).await;
    assert_eq!(
        params,
        FsOpenVerifiedParams {
            handle_id: params.handle_id.clone(),
            path: expected_path,
            max_bytes,
            sandbox: None,
        }
    );
    let handle_id = params.handle_id;
    write_response(
        websocket,
        request_id,
        FsOpenVerifiedResponse {
            handle_id: handle_id.clone(),
            size,
        },
    )
    .await;
    handle_id
}

async fn respond_to_read(
    websocket: &mut WebSocketStream<TcpStream>,
    handle_id: &str,
    offset: u64,
    len: usize,
    chunk: Vec<u8>,
    eof: bool,
) {
    let (request_id, params) =
        read_request_params::<FsReadBlockParams>(websocket, FS_READ_BLOCK_METHOD).await;
    assert_eq!(
        params,
        FsReadBlockParams {
            handle_id: handle_id.to_string(),
            offset,
            len,
        }
    );
    write_response(
        websocket,
        request_id,
        FsReadBlockResponse {
            chunk: chunk.into(),
            eof,
        },
    )
    .await;
}

async fn respond_to_close(websocket: &mut WebSocketStream<TcpStream>, handle_id: String) {
    let (request_id, params) =
        read_request_params::<FsCloseParams>(websocket, FS_CLOSE_METHOD).await;
    assert_eq!(params, FsCloseParams { handle_id });
    write_response(websocket, request_id, FsCloseResponse {}).await;
}

async fn accept_initialized(listener: TcpListener) -> WebSocketStream<TcpStream> {
    let (stream, _) = listener.accept().await.expect("listener should accept");
    let mut websocket = accept_async(stream)
        .await
        .expect("websocket handshake should succeed");
    let request = read_request(&mut websocket, INITIALIZE_METHOD).await;
    write_response(
        &mut websocket,
        request.id,
        InitializeResponse {
            session_id: "session-1".to_string(),
        },
    )
    .await;
    match read_message(&mut websocket).await {
        JSONRPCMessage::Notification(notification) if notification.method == INITIALIZED_METHOD => {
        }
        other => panic!("expected initialized notification, got {other:?}"),
    }
    websocket
}

async fn read_request_params<T: DeserializeOwned>(
    websocket: &mut WebSocketStream<TcpStream>,
    expected_method: &str,
) -> (RequestId, T) {
    let request = read_request(websocket, expected_method).await;
    let params = serde_json::from_value(
        request
            .params
            .expect("JSON-RPC request params should exist"),
    )
    .expect("JSON-RPC request params should deserialize");
    (request.id, params)
}

async fn read_request(
    websocket: &mut WebSocketStream<TcpStream>,
    expected_method: &str,
) -> JSONRPCRequest {
    match read_message(websocket).await {
        JSONRPCMessage::Request(request) if request.method == expected_method => request,
        other => panic!("expected {expected_method} request, got {other:?}"),
    }
}

async fn write_response<T: Serialize>(
    websocket: &mut WebSocketStream<TcpStream>,
    request_id: RequestId,
    result: T,
) {
    write_message(
        websocket,
        JSONRPCMessage::Response(JSONRPCResponse {
            id: request_id,
            result: serde_json::to_value(result).expect("JSON-RPC response should serialize"),
        }),
    )
    .await;
}

async fn read_message(websocket: &mut WebSocketStream<TcpStream>) -> JSONRPCMessage {
    loop {
        match timeout(Duration::from_secs(1), websocket.next())
            .await
            .expect("JSON-RPC websocket read should not time out")
            .expect("websocket should stay open")
            .expect("websocket frame should read")
        {
            Message::Text(text) => {
                return serde_json::from_str(text.as_ref())
                    .expect("JSON-RPC text frame should parse");
            }
            Message::Binary(bytes) => {
                return serde_json::from_slice(bytes.as_ref())
                    .expect("JSON-RPC binary frame should parse");
            }
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("expected JSON-RPC websocket frame, got {other:?}"),
        }
    }
}

async fn write_message(websocket: &mut WebSocketStream<TcpStream>, message: JSONRPCMessage) {
    let encoded = serde_json::to_string(&message).expect("JSON-RPC should serialize");
    websocket
        .send(Message::Text(encoded.into()))
        .await
        .expect("JSON-RPC websocket frame should write");
}
