use codex_code_mode_protocol::host::CapabilitySet;
use codex_code_mode_protocol::host::ClientToHost;
use codex_code_mode_protocol::host::FramedReader;
use codex_code_mode_protocol::host::FramedWriter;
use codex_code_mode_protocol::host::HostHello;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::ProtocolVersion;
use pretty_assertions::assert_eq;

use super::negotiate;

async fn negotiate_with_selected_version(selected_version: ProtocolVersion) -> Result<(), String> {
    let (client_stream, host_stream) = tokio::io::duplex(/*max_buf_size*/ 1024);
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
        writer
            .write(&HostToClient::HostHello(HostHello::new(
                selected_version,
                CapabilitySet::empty(),
            )))
            .await
            .expect("write host hello");
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
async fn client_requires_bounded_cell_id_protocol_version() {
    assert_eq!(
        negotiate_with_selected_version(ProtocolVersion::V1).await,
        Err("code-mode host selected unsupported protocol version 1".to_string())
    );
    assert_eq!(
        negotiate_with_selected_version(ProtocolVersion::V2).await,
        Ok(())
    );
}
