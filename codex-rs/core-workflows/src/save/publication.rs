use std::io;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

#[cfg(not(windows))]
use cap_std::fs::OpenOptions;

use self::root::PreparedWorkflowRoot;
use self::root::is_link_or_reparse_point;
use self::root::prepare_workflow_root;
use super::WorkflowSaveError;
use super::WorkflowSaveMode;
use super::WorkflowSaveOutcome;
use super::WorkflowSaveRoot;
#[cfg(windows)]
use super::filesystem::nt_status_to_io_error;

mod root;
#[cfg(windows)]
mod windows;

#[cfg(test)]
#[path = "publication_tests.rs"]
mod tests;

const MAX_TEMP_CREATE_ATTEMPTS: usize = 128;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
enum TargetState {
    Missing,
    Regular,
}

struct PreparedTempFile {
    name: String,
    file: cap_std::fs::File,
}

pub(super) fn publish_workflow(
    requested_root: &WorkflowSaveRoot,
    file_name: &str,
    source: &[u8],
    mode: WorkflowSaveMode,
) -> Result<WorkflowSaveOutcome, WorkflowSaveError> {
    let root = prepare_workflow_root(requested_root)?;
    match mode {
        WorkflowSaveMode::Create => create_workflow(&root, file_name, source),
        WorkflowSaveMode::Overwrite => overwrite_workflow(&root, file_name, source),
    }
}

fn inspect_target(
    root: &PreparedWorkflowRoot,
    file_name: &str,
) -> Result<TargetState, WorkflowSaveError> {
    let target = root.path.join(file_name);
    match root.dir.symlink_metadata(file_name) {
        Ok(metadata) if is_link_or_reparse_point(&metadata) => {
            Err(WorkflowSaveError::InvalidTarget {
                path: target,
                reason: "target is a symlink or reparse point".to_string(),
            })
        }
        Ok(metadata) if metadata.is_file() => Ok(TargetState::Regular),
        Ok(_) => Err(WorkflowSaveError::InvalidTarget {
            path: target,
            reason: "target exists but is not a regular file".to_string(),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(TargetState::Missing),
        Err(source) => Err(WorkflowSaveError::Io {
            action: "inspect workflow target",
            path: target,
            source,
        }),
    }
}

fn create_workflow(
    root: &PreparedWorkflowRoot,
    file_name: &str,
    source: &[u8],
) -> Result<WorkflowSaveOutcome, WorkflowSaveError> {
    let target = root.path.join(file_name);
    if matches!(inspect_target(root, file_name)?, TargetState::Regular) {
        return Ok(WorkflowSaveOutcome::Conflict { path: target });
    }
    let temp = prepare_temp_file(root, source)?;
    match atomically_create_file(root, &temp, file_name) {
        Ok(()) => {
            finish_published_create(root, &temp);
            sync_workflow_directory(root)?;
            Ok(WorkflowSaveOutcome::Created { path: target })
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            remove_abandoned_temp(root, &temp);
            match inspect_target(root, file_name)? {
                TargetState::Regular => Ok(WorkflowSaveOutcome::Conflict { path: target }),
                TargetState::Missing => Err(WorkflowSaveError::InvalidTarget {
                    path: target,
                    reason: "target changed while create-only save was being admitted".to_string(),
                }),
            }
        }
        Err(source) => {
            remove_abandoned_temp(root, &temp);
            Err(WorkflowSaveError::Io {
                action: "atomically create workflow target",
                path: target,
                source,
            })
        }
    }
}

fn overwrite_workflow(
    root: &PreparedWorkflowRoot,
    file_name: &str,
    source: &[u8],
) -> Result<WorkflowSaveOutcome, WorkflowSaveError> {
    inspect_target(root, file_name)?;
    let temp = prepare_temp_file(root, source)?;
    let observed_target = match inspect_target(root, file_name) {
        Ok(state) => state,
        Err(error) => {
            remove_abandoned_temp(root, &temp);
            return Err(error);
        }
    };
    if let Err(source) = atomically_replace_file(root, &temp, file_name) {
        remove_abandoned_temp(root, &temp);
        return match inspect_target(root, file_name) {
            Err(error @ WorkflowSaveError::InvalidTarget { .. }) => Err(error),
            Ok(TargetState::Missing | TargetState::Regular) | Err(_) => {
                Err(WorkflowSaveError::Io {
                    action: "atomically replace workflow target",
                    path: root.path.join(file_name),
                    source,
                })
            }
        };
    }
    sync_workflow_directory(root)?;
    let path = root.path.join(file_name);
    Ok(match observed_target {
        TargetState::Missing => WorkflowSaveOutcome::Created { path },
        TargetState::Regular => WorkflowSaveOutcome::Overwritten { path },
    })
}

fn prepare_temp_file(
    root: &PreparedWorkflowRoot,
    source: &[u8],
) -> Result<PreparedTempFile, WorkflowSaveError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    for _attempt in 0..MAX_TEMP_CREATE_ATTEMPTS {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temp_name = format!(".codex-workflow-save-{pid}-{timestamp}-{id}.tmp");
        let temp_path = root.path.join(&temp_name);
        #[cfg(not(windows))]
        let opened = {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt;

                options.mode(/*mode*/ 0o600).custom_flags(libc::O_NOFOLLOW);
            }
            root.dir.open_with(&temp_name, &options)
        };
        #[cfg(windows)]
        let opened = windows::create_temp_file(root, &temp_name);
        match opened {
            Ok(file) => {
                let mut temp = PreparedTempFile {
                    name: temp_name,
                    file,
                };
                let metadata = match temp.file.metadata() {
                    Ok(metadata) => metadata,
                    Err(source) => {
                        remove_abandoned_temp(root, &temp);
                        return Err(WorkflowSaveError::Io {
                            action: "inspect opened workflow publication temp file",
                            path: temp_path,
                            source,
                        });
                    }
                };
                if is_link_or_reparse_point(&metadata) || !metadata.is_file() {
                    remove_abandoned_temp(root, &temp);
                    return Err(WorkflowSaveError::InvalidTarget {
                        path: temp_path,
                        reason: "opened publication handle is not a regular file".to_string(),
                    });
                }
                if let Err(error) = set_private_file_permissions(root, &temp.file, &temp_path) {
                    remove_abandoned_temp(root, &temp);
                    return Err(error);
                }
                if let Err(source) = temp
                    .file
                    .write_all(source)
                    .and_then(|()| temp.file.sync_all())
                {
                    remove_abandoned_temp(root, &temp);
                    return Err(WorkflowSaveError::Io {
                        action: "write and sync workflow publication temp file",
                        path: temp_path,
                        source,
                    });
                }
                return Ok(temp);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(WorkflowSaveError::Io {
                    action: "create workflow publication temp file",
                    path: temp_path,
                    source,
                });
            }
        }
    }
    Err(WorkflowSaveError::InvalidTarget {
        path: root.path.clone(),
        reason: format!(
            "could not allocate a publication temp file after {MAX_TEMP_CREATE_ATTEMPTS} attempts"
        ),
    })
}

#[cfg(not(windows))]
fn remove_abandoned_temp(root: &PreparedWorkflowRoot, temp: &PreparedTempFile) {
    if let Err(error) = root.dir.remove_file(&temp.name)
        && error.kind() != io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %root.path.join(&temp.name).display(),
            %error,
            "failed to remove workflow publication temp file"
        );
    }
}

#[cfg(windows)]
fn remove_abandoned_temp(root: &PreparedWorkflowRoot, temp: &PreparedTempFile) {
    if let Err(error) = mark_file_for_deletion(&temp.file) {
        tracing::warn!(
            path = %root.path.join(&temp.name).display(),
            %error,
            "failed to mark workflow publication temp handle for deletion"
        );
    }
}

#[cfg(not(windows))]
fn finish_published_create(root: &PreparedWorkflowRoot, temp: &PreparedTempFile) {
    remove_abandoned_temp(root, temp);
}

#[cfg(windows)]
fn finish_published_create(_root: &PreparedWorkflowRoot, _temp: &PreparedTempFile) {}

#[cfg(not(windows))]
fn atomically_create_file(
    root: &PreparedWorkflowRoot,
    temp: &PreparedTempFile,
    target: &str,
) -> io::Result<()> {
    root.dir.hard_link(&temp.name, &root.dir, target)
}

#[cfg(windows)]
fn atomically_create_file(
    root: &PreparedWorkflowRoot,
    temp: &PreparedTempFile,
    target: &str,
) -> io::Result<()> {
    rename_temp_handle(root, temp, target, RenameMode::NoReplace)
}

#[cfg(not(windows))]
fn atomically_replace_file(
    root: &PreparedWorkflowRoot,
    source: &PreparedTempFile,
    target: &str,
) -> io::Result<()> {
    root.dir.rename(&source.name, &root.dir, target)
}

#[cfg(windows)]
fn atomically_replace_file(
    root: &PreparedWorkflowRoot,
    source: &PreparedTempFile,
    target: &str,
) -> io::Result<()> {
    rename_temp_handle(root, source, target, RenameMode::Replace)
}

#[cfg(windows)]
#[derive(Clone, Copy)]
enum RenameMode {
    NoReplace,
    Replace,
}

#[cfg(windows)]
fn rename_temp_handle(
    root: &PreparedWorkflowRoot,
    temp: &PreparedTempFile,
    target: &str,
    mode: RenameMode,
) -> io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Wdk::Storage::FileSystem::FILE_RENAME_INFORMATION;
    use windows_sys::Wdk::Storage::FileSystem::FileRenameInformation;
    use windows_sys::Wdk::Storage::FileSystem::NtSetInformationFile;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    let target = std::ffi::OsStr::new(target)
        .encode_wide()
        .collect::<Vec<_>>();
    let file_name_bytes = target
        .len()
        .checked_mul(size_of::<u16>())
        .and_then(|length| u32::try_from(length).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target name is too long"))?;
    let file_name_offset = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    // The native API contract requires at least the complete fixed structure
    // plus the byte length of the flexible UTF-16 name.
    let buffer_size = size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(
            usize::try_from(file_name_bytes).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "target name is too long")
            })?,
        )
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target name is too long"))?;
    let buffer_size_u32 = u32::try_from(buffer_size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target name is too long"))?;
    let mut storage = vec![0_usize; buffer_size.div_ceil(size_of::<usize>())];
    let rename_info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut io_status = IO_STATUS_BLOCK::default();

    // SAFETY: `storage` is pointer-aligned and large enough for the fixed
    // structure plus the exact UTF-16 target name. The native
    // `FileRenameInformation` contract resolves this simple relative name
    // against `RootDirectory`. Both the source and destination handles remain
    // alive for the complete synchronous call.
    let status = unsafe {
        (*rename_info).Anonymous.ReplaceIfExists = match mode {
            RenameMode::NoReplace => false,
            RenameMode::Replace => true,
        };
        (*rename_info).RootDirectory = root.dir.as_raw_handle();
        (*rename_info).FileNameLength = file_name_bytes;
        std::ptr::copy_nonoverlapping(
            target.as_ptr(),
            storage
                .as_mut_ptr()
                .cast::<u8>()
                .add(file_name_offset)
                .cast(),
            target.len(),
        );
        NtSetInformationFile(
            temp.file.as_raw_handle(),
            &mut io_status,
            rename_info.cast(),
            buffer_size_u32,
            FileRenameInformation,
        )
    };
    if status >= 0 {
        Ok(())
    } else {
        Err(nt_status_to_io_error(status))
    }
}

#[cfg(windows)]
fn mark_file_for_deletion(file: &cap_std::fs::File) -> io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::FILE_DISPOSITION_INFO;
    use windows_sys::Win32::Storage::FileSystem::FileDispositionInfo;
    use windows_sys::Win32::Storage::FileSystem::SetFileInformationByHandle;

    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    let buffer_size = u32::try_from(size_of::<FILE_DISPOSITION_INFO>()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "file disposition structure is too large",
        )
    })?;
    // SAFETY: `disposition` is the structure required by
    // `FileDispositionInfo`, and the handle remains alive for the call.
    let deleted = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            std::ptr::addr_of!(disposition).cast(),
            buffer_size,
        )
    };
    if deleted == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn set_private_file_permissions(
    _root: &PreparedWorkflowRoot,
    file: &cap_std::fs::File,
    path: &Path,
) -> Result<(), WorkflowSaveError> {
    use cap_std::fs::PermissionsExt;

    file.set_permissions(cap_std::fs::Permissions::from_mode(/*mode*/ 0o600))
        .map_err(|source| WorkflowSaveError::Io {
            action: "set private workflow-file permissions",
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(windows)]
fn set_private_file_permissions(
    root: &PreparedWorkflowRoot,
    file: &cap_std::fs::File,
    path: &Path,
) -> Result<(), WorkflowSaveError> {
    if root.private_acl {
        use std::os::windows::io::AsRawHandle;

        super::filesystem::validate_created_private_windows_object(file.as_raw_handle()).map_err(
            |source| WorkflowSaveError::Io {
                action: "verify creation-time private workflow-file DACL",
                path: path.to_path_buf(),
                source,
            },
        )?;
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn set_private_file_permissions(
    _root: &PreparedWorkflowRoot,
    _file: &cap_std::fs::File,
    _path: &Path,
) -> Result<(), WorkflowSaveError> {
    Ok(())
}

#[cfg(unix)]
fn sync_workflow_directory(root: &PreparedWorkflowRoot) -> Result<(), WorkflowSaveError> {
    root.dir
        .try_clone()
        .and_then(|dir| dir.into_std_file().sync_all())
        .map_err(|source| WorkflowSaveError::Io {
            action: "sync workflow directory",
            path: root.path.clone(),
            source,
        })
}

#[cfg(not(unix))]
fn sync_workflow_directory(_root: &PreparedWorkflowRoot) -> Result<(), WorkflowSaveError> {
    Ok(())
}
