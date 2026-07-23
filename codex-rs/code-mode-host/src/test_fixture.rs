use std::ffi::OsStr;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use codex_code_mode_protocol::host::FramedReader;
use codex_code_mode_protocol::host::FramedWriter;

#[path = "test_fixture/host.rs"]
mod host;
#[path = "test_fixture/mismatched_identity.rs"]
mod mismatched_identity;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let current_exe = std::env::current_exe().context("failed to resolve fixture executable")?;
    let executable_stem = current_exe
        .file_stem()
        .context("fixture executable has no file stem")?;
    let negotiation_mode = host::NegotiationMode::from_executable_stem(executable_stem);
    let mismatched_identity_mode =
        executable_stem == OsStr::new(mismatched_identity::MISMATCHED_WORKFLOW_ID_MODE);
    if negotiation_mode.is_none() && !mismatched_identity_mode {
        bail!(
            "unknown fixture executable stem `{}`; expected `{}`, `{}`, `{}`, or `{}`",
            executable_stem.to_string_lossy(),
            host::NO_SAVED_CAPABILITIES_MODE,
            host::LEGACY_OUTPUT_ONLY_MODE,
            host::INVALID_WORKFLOW_CAPABILITIES_MODE,
            mismatched_identity::MISMATCHED_WORKFLOW_ID_MODE,
        );
    }

    let mut args = std::env::args_os().skip(1);
    let stdin_observer = match args.next() {
        None => false,
        Some(arg) if arg == OsStr::new(mismatched_identity::STDIN_OBSERVER_ARG) => true,
        Some(arg) => bail!("unknown fixture argument `{}`", arg.to_string_lossy()),
    };
    ensure!(args.next().is_none(), "fixture received too many arguments");
    if stdin_observer {
        ensure!(
            mismatched_identity_mode,
            "stdin observer is only valid for the mismatched workflow-ID fixture"
        );
    }

    if let Some(mode) = negotiation_mode {
        host::run_negotiation_fixture(
            mode,
            FramedReader::new(tokio::io::stdin()),
            FramedWriter::new(tokio::io::stdout()),
        )
        .await
    } else {
        mismatched_identity::run(current_exe, stdin_observer).await
    }
}
