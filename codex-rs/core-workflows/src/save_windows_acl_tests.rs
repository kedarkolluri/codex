use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::path::Path;
use std::path::PathBuf;

use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
use windows_sys::Win32::Security::Authorization::SetSecurityInfo;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::GetSecurityDescriptorControl;
use windows_sys::Win32::Security::GetSecurityDescriptorDacl;
use windows_sys::Win32::Security::GetTokenInformation;
use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::Security::PSID;
use windows_sys::Win32::Security::SE_DACL_PROTECTED;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Security::TOKEN_USER;
use windows_sys::Win32::Security::TokenUser;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
use windows_sys::Win32::Storage::FileSystem::WRITE_DAC;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::OpenProcessToken;

use super::*;

const SAFE_INHERITABLE_DACL: &str = "D:P(A;OICI;GA;;;OW)(A;OICI;GA;;;SY)(A;OICI;GA;;;BA)";

#[derive(Debug, Eq, PartialEq)]
struct DaclSnapshot {
    bytes: Vec<u8>,
    protected: bool,
    ace_count: u16,
    current_user_owner: bool,
}

fn workflow_source() -> Vec<u8> {
    b"export const meta = { name: 'private', description: 'saved' };\n\
      export default async function main() { return 'secret'; }\n"
        .to_vec()
}

fn source_identity(source: &[u8]) -> WorkflowSaveSourceIdentity {
    WorkflowSaveSourceIdentity::new(
        "private",
        codex_workflow_journal::prompt_hash(std::str::from_utf8(source).unwrap()),
    )
}

fn write_run_script(temp: &TempDir, source: &[u8]) -> PathBuf {
    let run_directory = temp.path().join("runs").join("run-1");
    std::fs::create_dir_all(&run_directory).unwrap();
    std::fs::write(run_directory.join(RUN_SCRIPT_FILE_NAME), source).unwrap();
    run_directory
}

fn personal_root(home: &Path) -> WorkflowSaveRoot {
    let canonical_home = std::fs::canonicalize(home).unwrap();
    WorkflowSaveRoot::personal(
        AbsolutePathBuf::from_absolute_path(canonical_home).expect("canonical path is absolute"),
    )
}

fn open_security_handle(path: &Path, directory: bool, write_dacl: bool) -> std::fs::File {
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .access_mode(READ_CONTROL | if write_dacl { WRITE_DAC } else { 0 })
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT
                | if directory {
                    FILE_FLAG_BACKUP_SEMANTICS
                } else {
                    0
                },
        );
    options.open(path).unwrap()
}

fn set_protected_directory_dacl(path: &Path, sddl: &str) {
    let descriptor = LocalDescriptor::from_sddl(sddl);
    let acl = descriptor.dacl();
    let directory = open_security_handle(path, /*directory*/ true, /*write_dacl*/ true);
    // SAFETY: the handle carries WRITE_DAC and the ACL remains owned by the
    // live descriptor for the complete call.
    let error = unsafe {
        SetSecurityInfo(
            directory.as_raw_handle(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null(),
        )
    };
    assert_eq!(error, ERROR_SUCCESS);
}

fn dacl_snapshot(path: &Path, directory: bool) -> DaclSnapshot {
    let object = open_security_handle(path, directory, /*write_dacl*/ false);
    let mut owner = std::ptr::null_mut();
    let mut acl = std::ptr::null_mut::<ACL>();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: the handle carries READ_CONTROL; all unrequested outputs are
    // null, and `LocalDescriptor` releases the returned allocation.
    let error = unsafe {
        GetSecurityInfo(
            object.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut acl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(error, ERROR_SUCCESS);
    assert!(!owner.is_null());
    assert!(!acl.is_null());
    let descriptor = LocalDescriptor(descriptor);
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: the descriptor and its ACL remain live through this function.
    assert_ne!(
        unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) },
        0
    );
    // SAFETY: the kernel-returned ACL header is within the live descriptor,
    // and `AclSize` bounds the complete allocation-backed ACL image.
    let (ace_count, bytes) = unsafe {
        let acl_size = usize::from((*acl).AclSize);
        (
            (*acl).AceCount,
            std::slice::from_raw_parts(acl.cast::<u8>(), acl_size).to_vec(),
        )
    };
    DaclSnapshot {
        bytes,
        protected: control & SE_DACL_PROTECTED != 0,
        ace_count,
        current_user_owner: CurrentUser::new().equals(owner),
    }
}

#[tokio::test]
async fn new_personal_objects_have_creation_time_protected_private_dacls() {
    let temp = TempDir::new().unwrap();
    let source = workflow_source();
    let run_directory = write_run_script(&temp, &source);
    let home = temp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let save_root = personal_root(&home);

    save_run_workflow(
        &run_directory,
        &save_root,
        &source_identity(&source),
        WorkflowSaveMode::Create,
    )
    .await
    .unwrap();

    let agents = home.join(".agents");
    let workflows = agents.join("workflows");
    let target = workflows.join("private.js");
    assert_eq!(std::fs::read(&target).unwrap(), source);
    for (path, directory) in [(&agents, true), (&workflows, true), (&target, false)] {
        let snapshot = dacl_snapshot(path, directory);
        assert!(snapshot.protected);
        assert_eq!(snapshot.ace_count, 3);
        assert!(snapshot.current_user_owner);
        let object = open_security_handle(path, directory, /*write_dacl*/ false);
        filesystem::validate_created_private_windows_object(object.as_raw_handle()).unwrap();
    }
}

#[tokio::test]
async fn safe_protected_personal_directory_dacls_are_preserved_byte_for_byte() {
    let temp = TempDir::new().unwrap();
    let source = workflow_source();
    let run_directory = write_run_script(&temp, &source);
    let home = temp.path().join("home");
    let agents = home.join(".agents");
    let workflows = agents.join("workflows");
    std::fs::create_dir_all(&workflows).unwrap();
    set_protected_directory_dacl(&agents, SAFE_INHERITABLE_DACL);
    set_protected_directory_dacl(&workflows, SAFE_INHERITABLE_DACL);
    let before_agents = dacl_snapshot(&agents, /*directory*/ true);
    let before_workflows = dacl_snapshot(&workflows, /*directory*/ true);
    assert!(before_agents.protected);
    assert!(before_workflows.protected);

    save_run_workflow(
        &run_directory,
        &personal_root(&home),
        &source_identity(&source),
        WorkflowSaveMode::Create,
    )
    .await
    .unwrap();

    assert_eq!(dacl_snapshot(&agents, /*directory*/ true), before_agents);
    assert_eq!(
        dacl_snapshot(&workflows, /*directory*/ true),
        before_workflows
    );
    assert_eq!(std::fs::read(workflows.join("private.js")).unwrap(), source);
}

#[tokio::test]
async fn safe_but_inherited_personal_directory_dacls_fail_closed_without_mutation() {
    let temp = TempDir::new().unwrap();
    let source = workflow_source();
    let run_directory = write_run_script(&temp, &source);
    let home = temp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    set_protected_directory_dacl(&home, SAFE_INHERITABLE_DACL);
    let agents = home.join(".agents");
    let workflows = agents.join("workflows");
    std::fs::create_dir_all(&workflows).unwrap();
    let before_agents = dacl_snapshot(&agents, /*directory*/ true);
    let before_workflows = dacl_snapshot(&workflows, /*directory*/ true);
    assert!(!before_agents.protected);
    assert!(!before_workflows.protected);

    let result = save_run_workflow(
        &run_directory,
        &personal_root(&home),
        &source_identity(&source),
        WorkflowSaveMode::Create,
    )
    .await;

    assert!(matches!(result, Err(WorkflowSaveError::Io { .. })));
    assert_eq!(dacl_snapshot(&agents, /*directory*/ true), before_agents);
    assert_eq!(
        dacl_snapshot(&workflows, /*directory*/ true),
        before_workflows
    );
    assert!(!workflows.join("private.js").exists());
}

#[tokio::test]
async fn existing_personal_dacl_rejects_everyone_and_builtin_users_read_access() {
    for broad_trustee in ["WD", "BU"] {
        let temp = TempDir::new().unwrap();
        let source = workflow_source();
        let run_directory = write_run_script(&temp, &source);
        let home = temp.path().join("home");
        let agents = home.join(".agents");
        let workflows = agents.join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        set_protected_directory_dacl(
            &agents,
            &format!("D:P(A;;GA;;;OW)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GR;;;{broad_trustee})"),
        );

        let result = save_run_workflow(
            &run_directory,
            &personal_root(&home),
            &source_identity(&source),
            WorkflowSaveMode::Create,
        )
        .await;

        assert!(matches!(result, Err(WorkflowSaveError::Io { .. })));
        assert!(!workflows.join("private.js").exists());
    }
}

struct LocalDescriptor(PSECURITY_DESCRIPTOR);

impl LocalDescriptor {
    fn from_sddl(sddl: &str) -> Self {
        let mut wide_sddl = sddl.encode_utf16().collect::<Vec<_>>();
        wide_sddl.push(0);
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: `wide_sddl` is NUL-terminated and the output receives one
        // LocalAlloc-owned descriptor.
        assert_ne!(
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide_sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    std::ptr::null_mut(),
                )
            },
            0
        );
        Self(descriptor)
    }

    fn dacl(&self) -> *mut ACL {
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = std::ptr::null_mut();
        // SAFETY: the descriptor is live and all outputs point to stack values.
        assert_ne!(
            unsafe { GetSecurityDescriptorDacl(self.0, &mut present, &mut acl, &mut defaulted) },
            0
        );
        assert_ne!(present, 0);
        assert!(!acl.is_null());
        acl
    }
}

impl Drop for LocalDescriptor {
    fn drop(&mut self) {
        // SAFETY: the security APIs returned one LocalAlloc-owned descriptor.
        unsafe {
            let _ = LocalFree(self.0.cast());
        }
    }
}

struct CurrentUser {
    storage: Vec<usize>,
}

impl CurrentUser {
    fn new() -> Self {
        let mut token = std::ptr::null_mut();
        // SAFETY: the current process pseudo-handle is valid and the output
        // receives one newly owned token handle.
        assert_ne!(
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
            0
        );
        // SAFETY: `OpenProcessToken` returned one newly owned handle.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut byte_len = 0;
        // SAFETY: a null first query obtains the required byte count.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &mut byte_len,
            );
        }
        assert!(usize::try_from(byte_len).unwrap() >= std::mem::size_of::<TOKEN_USER>());
        let mut storage = vec![
            0_usize;
            usize::try_from(byte_len)
                .unwrap()
                .div_ceil(std::mem::size_of::<usize>())
        ];
        // SAFETY: the aligned allocation is at least `byte_len` bytes.
        assert_ne!(
            unsafe {
                GetTokenInformation(
                    token.as_raw_handle(),
                    TokenUser,
                    storage.as_mut_ptr().cast(),
                    byte_len,
                    &mut byte_len,
                )
            },
            0
        );
        Self { storage }
    }

    fn equals(&self, owner: PSID) -> bool {
        // SAFETY: the token query initialized `TOKEN_USER` at the front of the
        // aligned allocation, and the object query returned a live owner SID.
        let current = unsafe { (*self.storage.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        unsafe { EqualSid(current, owner) != 0 }
    }
}
