use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_code_mode_protocol::host::FramedReader;
use codex_code_mode_protocol::host::FramedWriter;

#[path = "test_fixture/host.rs"]
mod host;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let current_exe = std::env::current_exe().context("failed to resolve fixture executable")?;
    let executable_stem = current_exe
        .file_stem()
        .context("fixture executable has no file stem")?;
    let mode = match host::NegotiationMode::from_executable_stem(executable_stem) {
        Some(mode) => mode,
        None => bail!(
            "unknown fixture executable stem `{}`; expected `{}`, `{}`, or `{}`",
            executable_stem.to_string_lossy(),
            host::NO_SAVED_CAPABILITIES_MODE,
            host::LEGACY_OUTPUT_ONLY_MODE,
            host::INVALID_WORKFLOW_CAPABILITIES_MODE,
        ),
    };
    if let Some(arg) = std::env::args_os().nth(1) {
        bail!("unknown fixture argument `{}`", arg.to_string_lossy());
    }

    host::run_negotiation_fixture(
        mode,
        FramedReader::new(tokio::io::stdin()),
        FramedWriter::new(tokio::io::stdout()),
    )
    .await
}
