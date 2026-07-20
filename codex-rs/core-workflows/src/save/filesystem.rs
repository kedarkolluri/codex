use std::path::Path;
use std::path::PathBuf;

use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::fs::OpenOptions;

use super::WorkflowSaveError;

#[cfg(windows)]
#[path = "filesystem/windows_acl.rs"]
mod windows_acl;
#[cfg(windows)]
pub(super) use windows_acl::PrivateWindowsSecurityDescriptor;
#[cfg(windows)]
pub(super) use windows_acl::validate_created_private_windows_object;
#[cfg(windows)]
pub(super) use windows_acl::validate_existing_private_windows_object;

pub(super) async fn canonicalize_local_path(
    path: &Path,
    action: &'static str,
) -> Result<PathBuf, WorkflowSaveError> {
    let canonical =
        tokio::fs::canonicalize(path)
            .await
            .map_err(|source| WorkflowSaveError::Io {
                action,
                path: path.to_path_buf(),
                source,
            })?;
    AbsolutePathBuf::from_absolute_path(canonical)
        .map(AbsolutePathBuf::into_path_buf)
        .map_err(|source| WorkflowSaveError::Io {
            action: "normalize canonical local path",
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(unix)]
pub(super) fn configure_no_follow(options: &mut OpenOptions) {
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
}

#[cfg(windows)]
pub(super) fn configure_no_follow(options: &mut OpenOptions) {
    // FILE_FLAG_OPEN_REPARSE_POINT opens the final reparse object itself so
    // handle validation can reject it instead of following its target.
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(any(unix, windows)))]
pub(super) fn configure_no_follow(_options: &mut OpenOptions) {}

#[cfg(windows)]
pub(super) fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub(super) fn is_link_or_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
pub(super) fn nt_status_to_io_error(
    status: windows_sys::Win32::Foundation::NTSTATUS,
) -> std::io::Error {
    use windows_sys::Win32::Foundation::RtlNtStatusToDosError;

    // SAFETY: this conversion routine accepts every NTSTATUS value and has no
    // pointer or lifetime preconditions.
    let error = unsafe { RtlNtStatusToDosError(status) };
    match i32::try_from(error) {
        Ok(error) => std::io::Error::from_raw_os_error(error),
        Err(_) => std::io::Error::other("native workflow filesystem operation failed"),
    }
}
