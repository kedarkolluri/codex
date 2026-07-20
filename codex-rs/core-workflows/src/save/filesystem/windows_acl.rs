use std::mem::offset_of;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::GetAce;
use windows_sys::Win32::Security::GetLengthSid;
use windows_sys::Win32::Security::GetSecurityDescriptorControl;
use windows_sys::Win32::Security::GetTokenInformation;
use windows_sys::Win32::Security::IsValidSid;
use windows_sys::Win32::Security::IsWellKnownSid;
use windows_sys::Win32::Security::OWNER_SECURITY_INFORMATION;
use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;
use windows_sys::Win32::Security::PSID;
use windows_sys::Win32::Security::SE_DACL_PROTECTED;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Security::TOKEN_USER;
use windows_sys::Win32::Security::TokenUser;
use windows_sys::Win32::Security::WinBuiltinAdministratorsSid;
use windows_sys::Win32::Security::WinCreatorOwnerRightsSid;
use windows_sys::Win32::Security::WinCreatorOwnerSid;
use windows_sys::Win32::Security::WinLocalSystemSid;
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
use windows_sys::Win32::System::SystemServices::ACCESS_DENIED_ACE_TYPE;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::OpenProcessToken;

/// Owns the self-relative descriptor installed atomically when a private
/// workflow directory is created by `NtCreateFile`.
pub(in super::super) struct PrivateWindowsSecurityDescriptor(LocalSecurityDescriptor);

impl PrivateWindowsSecurityDescriptor {
    pub(in super::super) fn new() -> std::io::Result<Self> {
        let current_user = CurrentProcessUser::new()?;
        let mut sid_string = std::ptr::null_mut();
        // SAFETY: the token-user buffer remains live and the output receives
        // one LocalAlloc-owned NUL-terminated SID string.
        if unsafe { ConvertSidToStringSidW(current_user.sid(), &mut sid_string) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let sid_string = LocalWideString(sid_string);
        let sid = sid_string.to_string()?;
        // Protect the DACL from inheritance and grant access only to the
        // creating user, Local System, and built-in Administrators.
        let sddl = format!("O:{sid}D:P(A;;GA;;;{sid})(A;;GA;;;SY)(A;;GA;;;BA)");
        let mut wide_sddl = sddl.encode_utf16().collect::<Vec<_>>();
        wide_sddl.push(0);
        let mut descriptor = std::ptr::null_mut();
        // SAFETY: `wide_sddl` is NUL-terminated and remains live for the call;
        // the output receives one LocalAlloc-owned self-relative descriptor.
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide_sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self(LocalSecurityDescriptor(descriptor)))
    }

    pub(in super::super) fn as_ptr(
        &self,
    ) -> *const windows_sys::Win32::Security::SECURITY_DESCRIPTOR {
        self.0.0.cast()
    }
}

pub(in super::super) fn validate_created_private_windows_object(
    handle: HANDLE,
) -> std::io::Result<()> {
    validate_private_windows_object(handle)
}

pub(in super::super) fn validate_existing_private_windows_object(
    handle: HANDLE,
) -> std::io::Result<()> {
    validate_private_windows_object(handle)
}

fn validate_private_windows_object(handle: HANDLE) -> std::io::Result<()> {
    let mut owner = std::ptr::null_mut();
    let mut acl = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: the handle carries READ_CONTROL and every unrequested output is
    // null. All returned component pointers remain owned by `descriptor`.
    let error = unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut acl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != ERROR_SUCCESS {
        return Err(win32_error(error));
    }
    let descriptor = LocalSecurityDescriptor(descriptor);
    if owner.is_null() || unsafe { IsValidSid(owner) } == 0 {
        return Err(std::io::Error::other(
            "private workflow object has no valid owner",
        ));
    }
    if acl.is_null() {
        return Err(std::io::Error::other("private workflow object has no DACL"));
    }

    let current_user = CurrentProcessUser::new()?;
    if !sid_equals(owner, current_user.sid()) {
        return Err(std::io::Error::other(
            "private workflow object is not owned by the current user",
        ));
    }
    // Existing roots must be protected too. Otherwise a later inherited parent
    // ACE could grant DELETE_CHILD and permit replacement of a protected target.
    if !dacl_is_protected(descriptor.0)? {
        return Err(std::io::Error::other(
            "private workflow DACL is not protected from inheritance",
        ));
    }

    // This checks the policy semantically instead of comparing ACL bytes:
    // Windows may canonicalize generic rights while preserving their meaning.
    for index in 0..unsafe { (*acl).AceCount } {
        let mut raw_ace = std::ptr::null_mut();
        // SAFETY: `index` is bounded by the ACL's kernel-returned ACE count and
        // the descriptor allocation remains live.
        if unsafe { GetAce(acl, u32::from(index), &mut raw_ace) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let header = raw_ace.cast::<windows_sys::Win32::Security::ACE_HEADER>();
        // SAFETY: `GetAce` returned a pointer to at least one complete header.
        match unsafe { (*header).AceType } as u32 {
            ACCESS_DENIED_ACE_TYPE => continue,
            ACCESS_ALLOWED_ACE_TYPE => {}
            _ => {
                return Err(std::io::Error::other(
                    "private workflow DACL contains an unsupported ACE type",
                ));
            }
        }

        let ace = raw_ace.cast::<ACCESS_ALLOWED_ACE>();
        let sid_offset = offset_of!(ACCESS_ALLOWED_ACE, SidStart);
        // A SID is at least its 8-byte fixed header. Bound it inside this ACE
        // before asking Windows to validate or compare it.
        let ace_size = usize::from(unsafe { (*header).AceSize });
        if ace_size < sid_offset + 8 {
            return Err(std::io::Error::other(
                "private workflow DACL contains a malformed allow ACE",
            ));
        }
        // SAFETY: the fixed portion was bounded above; `SidStart` is the first
        // byte of the variable-length SID in both basic allow and deny ACEs.
        let sid: PSID = unsafe { std::ptr::addr_of_mut!((*ace).SidStart).cast() };
        // The second SID byte is its sub-authority count. Bound the complete
        // binary SID inside the ACE before calling APIs that inspect it.
        let sub_authority_count = usize::from(unsafe { *sid.cast::<u8>().add(1) });
        let encoded_sid_size = 8_usize
            .checked_add(
                sub_authority_count
                    .checked_mul(size_of::<u32>())
                    .ok_or_else(|| {
                        std::io::Error::other("private workflow trustee SID is too large")
                    })?,
            )
            .ok_or_else(|| std::io::Error::other("private workflow trustee SID is too large"))?;
        if sid_offset
            .checked_add(encoded_sid_size)
            .is_none_or(|end| end > ace_size)
        {
            return Err(std::io::Error::other(
                "private workflow DACL trustee exceeds its ACE",
            ));
        }
        if unsafe { IsValidSid(sid) } == 0 {
            return Err(std::io::Error::other(
                "private workflow DACL contains an invalid trustee SID",
            ));
        }
        let sid_size = usize::try_from(unsafe { GetLengthSid(sid) })
            .map_err(|_| std::io::Error::other("private workflow trustee SID is too large"))?;
        if sid_size != encoded_sid_size {
            return Err(std::io::Error::other(
                "private workflow DACL trustee has an inconsistent SID length",
            ));
        }
        if !sid_equals(sid, current_user.sid())
            && !is_well_known_sid(sid, WinLocalSystemSid)
            && !is_well_known_sid(sid, WinBuiltinAdministratorsSid)
            && !is_well_known_sid(sid, WinCreatorOwnerRightsSid)
            && !is_well_known_sid(sid, WinCreatorOwnerSid)
        {
            return Err(std::io::Error::other(
                "private workflow DACL grants access to an unapproved principal",
            ));
        }
    }
    Ok(())
}

fn dacl_is_protected(descriptor: PSECURITY_DESCRIPTOR) -> std::io::Result<bool> {
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: the queried descriptor remains owned by its live guard and both
    // output pointers refer to initialized stack values.
    if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(control & SE_DACL_PROTECTED != 0)
}

fn sid_equals(first: PSID, second: PSID) -> bool {
    // SAFETY: every caller has validated both live SIDs.
    unsafe { EqualSid(first, second) != 0 }
}

fn is_well_known_sid(sid: PSID, kind: windows_sys::Win32::Security::WELL_KNOWN_SID_TYPE) -> bool {
    // SAFETY: every caller has validated the live SID.
    unsafe { IsWellKnownSid(sid, kind) != 0 }
}

struct CurrentProcessUser {
    storage: Vec<usize>,
}

impl CurrentProcessUser {
    fn new() -> std::io::Result<Self> {
        let mut token = std::ptr::null_mut();
        // SAFETY: the current-process pseudo handle is always valid and the
        // output receives one newly owned token handle.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `OpenProcessToken` returned one newly owned handle.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut byte_len = 0;
        // SAFETY: a null, zero-length first query obtains the required size.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &mut byte_len,
            );
        }
        if usize::try_from(byte_len).map_or(true, |length| length < size_of::<TOKEN_USER>()) {
            return Err(std::io::Error::last_os_error());
        }
        let byte_len_usize = usize::try_from(byte_len)
            .map_err(|_| std::io::Error::other("Windows token user is too large"))?;
        let mut storage = vec![0_usize; byte_len_usize.div_ceil(size_of::<usize>())];
        // SAFETY: the storage is pointer-aligned and at least `byte_len` bytes;
        // the token remains open for the complete synchronous query.
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                storage.as_mut_ptr().cast(),
                byte_len,
                &mut byte_len,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let user = Self { storage };
        if unsafe { IsValidSid(user.sid()) } == 0 {
            return Err(std::io::Error::other(
                "Windows process token has no valid user SID",
            ));
        }
        Ok(user)
    }

    fn sid(&self) -> PSID {
        // SAFETY: successful `GetTokenInformation(TokenUser)` initialized the
        // front of the aligned allocation as `TOKEN_USER`; its SID points into
        // the same live allocation.
        unsafe { (*self.storage.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: the conversion/query APIs return one LocalAlloc-owned
        // descriptor, and this guard owns it exactly once.
        unsafe {
            let _ = LocalFree(self.0.cast());
        }
    }
}

struct LocalWideString(windows_sys::core::PWSTR);

impl LocalWideString {
    fn to_string(&self) -> std::io::Result<String> {
        if self.0.is_null() {
            return Err(std::io::Error::other(
                "Windows SID conversion returned no string",
            ));
        }
        let mut length = 0;
        // SAFETY: the conversion API returned a live NUL-terminated string.
        while unsafe { *self.0.add(length) } != 0 {
            length += 1;
        }
        // SAFETY: the scan above found the terminator within the API-owned
        // string; the slice excludes that terminator.
        String::from_utf16(unsafe { std::slice::from_raw_parts(self.0, length) })
            .map_err(|_| std::io::Error::other("Windows SID string is not valid UTF-16"))
    }
}

impl Drop for LocalWideString {
    fn drop(&mut self) {
        // SAFETY: `ConvertSidToStringSidW` returned one LocalAlloc-owned string.
        unsafe {
            let _ = LocalFree(self.0.cast());
        }
    }
}

fn win32_error(error: u32) -> std::io::Error {
    match i32::try_from(error) {
        Ok(error) => std::io::Error::from_raw_os_error(error),
        Err(_) => std::io::Error::other("Windows security operation failed"),
    }
}
