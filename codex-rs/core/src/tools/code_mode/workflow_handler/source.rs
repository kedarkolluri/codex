//! Bounded, no-follow workflow source snapshots used by every execution entrypoint.

use std::io;
use std::path::Path;

use tokio::fs::OpenOptions;
use tokio::io::AsyncReadExt;

/// Read a raw workflow script path into one bounded, immutable source snapshot.
pub(in crate::tools::code_mode) async fn read_workflow_source_bounded(
    path: &Path,
) -> io::Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    let file = options.open(path).await?;
    let metadata = file.metadata().await?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workflow source is not a regular, non-reparse file",
        ));
    }

    let max_bytes = codex_core_workflows::WORKFLOW_SOURCE_MAX_BYTES;
    if metadata.len() > max_bytes {
        return Err(source_too_large(max_bytes));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes).await?;
    if bytes.len() as u64 > max_bytes {
        return Err(source_too_large(max_bytes));
    }
    String::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

/// Read a saved workflow and revalidate the name against the exact source snapshot.
///
/// Registry discovery and execution are separate operations. Rechecking the full
/// source closes the replacement window: a file discovered as `expected_name`
/// cannot later execute after being atomically replaced with another workflow.
pub(in crate::tools::code_mode) async fn read_named_workflow_source_bounded(
    path: &Path,
    expected_name: &str,
) -> io::Result<String> {
    let source = read_workflow_source_bounded(path).await?;
    let metadata = codex_code_mode::parse_workflow_meta(&source)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if metadata.name != expected_name {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "saved workflow source no longer declares the requested name",
        ));
    }
    Ok(source)
}

fn source_too_large(max_bytes: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("workflow source exceeds the {max_bytes}-byte execution cap"),
    )
}

#[cfg(unix)]
fn configure_no_follow(options: &mut OpenOptions) {
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
}

#[cfg(windows)]
fn configure_no_follow(options: &mut OpenOptions) {
    // Open the final reparse object itself so handle validation rejects it.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(any(unix, windows)))]
fn configure_no_follow(_options: &mut OpenOptions) {}

#[cfg(windows)]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;
