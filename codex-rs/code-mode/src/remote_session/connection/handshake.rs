use codex_code_mode_protocol::host::CapabilitySet;
use codex_code_mode_protocol::host::ClientHello;
use codex_code_mode_protocol::host::ClientToHost;
use codex_code_mode_protocol::host::FramedReader;
use codex_code_mode_protocol::host::FramedWriter;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::ProtocolVersion;
use codex_code_mode_protocol::host::SupportedProtocolVersions;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;

pub(super) async fn negotiate<R, W>(
    reader: &mut FramedReader<R>,
    writer: &mut FramedWriter<W>,
) -> Result<(), String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let hello = ClientHello::new(
        SupportedProtocolVersions::try_new([ProtocolVersion::V2]).map_err(|err| err.to_string())?,
        CapabilitySet::empty(),
        CapabilitySet::empty(),
    )
    .map_err(|err| err.to_string())?;
    writer
        .write(&ClientToHost::ClientHello(hello))
        .await
        .map_err(|err| format!("failed to write code-mode host hello: {err}"))?;
    match reader
        .read::<HostToClient>()
        .await
        .map_err(|err| format!("failed to read code-mode host hello: {err}"))?
    {
        Some(HostToClient::HostHello(hello)) if hello.selected_version() == ProtocolVersion::V2 => {
            Ok(())
        }
        Some(HostToClient::HostHello(hello)) => Err(format!(
            "code-mode host selected unsupported protocol version {}",
            hello.selected_version().get()
        )),
        Some(HostToClient::HandshakeRejected { reason }) => {
            Err(format!("code-mode host rejected the handshake: {reason:?}"))
        }
        Some(message) => Err(format!(
            "code-mode host returned an invalid handshake response: {message:?}"
        )),
        None => Err("code-mode host exited during handshake".to_string()),
    }
}

#[cfg(test)]
#[path = "handshake_tests.rs"]
mod tests;
