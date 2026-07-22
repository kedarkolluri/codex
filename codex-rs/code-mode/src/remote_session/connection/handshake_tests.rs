use codex_code_mode_protocol::host::Capability;
use codex_code_mode_protocol::host::CapabilitySet;
use codex_code_mode_protocol::host::ClientToHost;
use codex_code_mode_protocol::host::FramedReader;
use codex_code_mode_protocol::host::FramedWriter;
use codex_code_mode_protocol::host::HandshakeRejectReason;
use codex_code_mode_protocol::host::HostHello;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::ProtocolVersion;
use codex_code_mode_protocol::host::SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY;
use codex_code_mode_protocol::host::SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireCellId;
use pretty_assertions::assert_eq;

use super::NegotiatedCapabilities;
use super::negotiate;

async fn negotiate_with_response(response: HostToClient) -> Result<NegotiatedCapabilities, String> {
    let (client_stream, host_stream) = tokio::io::duplex(/*max_buf_size*/ 4096);
    let (client_reader, client_writer) = tokio::io::split(client_stream);
    let (host_reader, host_writer) = tokio::io::split(host_stream);
    let host = tokio::spawn(async move {
        let mut reader = FramedReader::new(host_reader);
        let mut writer = FramedWriter::new(host_writer);
        let message = reader
            .read::<ClientToHost>()
            .await
            .expect("read client hello")
            .expect("client hello");
        let ClientToHost::ClientHello(hello) = message else {
            panic!("first client message should be a hello");
        };
        assert!(hello.supported_versions().contains(ProtocolVersion::V2));
        assert!(!hello.supported_versions().contains(ProtocolVersion::V1));
        assert_eq!(hello.required_capabilities(), &CapabilitySet::empty());
        assert_eq!(
            hello.optional_capabilities(),
            &CapabilitySet::try_new([
                Capability::new(SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY)
                    .expect("saved output capability"),
                Capability::new(SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY)
                    .expect("workflow cell identity capability"),
            ])
            .expect("optional capabilities")
        );
        writer.write(&response).await.expect("write host response");
    });
    let result = negotiate(
        &mut FramedReader::new(client_reader),
        &mut FramedWriter::new(client_writer),
    )
    .await;
    host.await.expect("host task");
    result
}

#[tokio::test]
async fn client_requires_v2_and_accepts_only_offered_capabilities() {
    assert_eq!(
        negotiate_with_response(HostToClient::HostHello(HostHello::new(
            ProtocolVersion::V1,
            CapabilitySet::empty(),
        )))
        .await,
        Err("code-mode host selected an unsupported protocol version".to_string())
    );
    assert_eq!(
        negotiate_with_response(HostToClient::HostHello(HostHello::new(
            ProtocolVersion::V2,
            CapabilitySet::empty(),
        )))
        .await,
        Ok(NegotiatedCapabilities::default())
    );
    let selected_capabilities = CapabilitySet::try_new([
        Capability::new(SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY).expect("saved output capability"),
        Capability::new(SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY)
            .expect("workflow cell identity capability"),
    ])
    .expect("selected capabilities");
    let result = negotiate_with_response(HostToClient::HostHello(HostHello::new(
        ProtocolVersion::V2,
        selected_capabilities.clone(),
    )))
    .await
    .expect("paired capabilities");
    assert_eq!(
        result,
        NegotiatedCapabilities {
            selected: selected_capabilities,
        }
    );
}

#[tokio::test]
async fn client_accepts_legacy_saved_output_selection() {
    let selected = CapabilitySet::try_new([
        Capability::new(SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY).expect("saved output capability")
    ])
    .expect("selected capabilities");
    assert_eq!(
        negotiate_with_response(HostToClient::HostHello(HostHello::new(
            ProtocolVersion::V2,
            selected.clone(),
        )))
        .await,
        Ok(NegotiatedCapabilities { selected })
    );
}

#[tokio::test]
async fn client_rejects_workflow_identity_without_saved_output() {
    assert_eq!(
        negotiate_with_response(HostToClient::HostHello(HostHello::new(
            ProtocolVersion::V2,
            CapabilitySet::try_new([Capability::new(SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY)
                .expect("workflow cell identity capability"),])
            .expect("selected capabilities"),
        )))
        .await,
        Err("code-mode host selected an invalid workflow capability set".to_string())
    );
}

#[tokio::test]
async fn client_rejects_unoffered_host_capability_without_reflecting_it() {
    assert_eq!(
        negotiate_with_response(HostToClient::HostHello(HostHello::new(
            ProtocolVersion::V2,
            CapabilitySet::try_new([
                Capability::new("private_host_capability").expect("host capability"),
            ])
            .expect("host capabilities"),
        )))
        .await,
        Err("code-mode host selected an unoffered capability".to_string())
    );
}

#[tokio::test]
async fn client_does_not_reflect_host_handshake_failures() {
    for (response, expected_error) in [
        (
            HostToClient::HandshakeRejected {
                reason: HandshakeRejectReason::InvalidHello {
                    message: "private rejection detail".to_string(),
                },
            },
            "code-mode host rejected the handshake",
        ),
        (
            HostToClient::CellClosed {
                session_id: SessionId::new("private-session").expect("session ID"),
                cell_id: WireCellId::try_new("private-cell").expect("cell ID"),
            },
            "code-mode host returned an invalid handshake response",
        ),
    ] {
        assert_eq!(
            negotiate_with_response(response).await,
            Err(expected_error.to_string())
        );
    }
}
