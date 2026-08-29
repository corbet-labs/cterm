use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, GENERIC_ALL, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    GetSecurityInfo, SetEntriesInAclW, SetSecurityInfo, EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE,
    SET_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_IS_WELL_KNOWN_GROUP,
    TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    AclSizeInformation, CreateWellKnownSid, EqualSid, GetAce, GetAclInformation,
    GetSecurityDescriptorControl, GetTokenInformation, TokenUser, WinBuiltinAdministratorsSid,
    WinLocalSystemSid, ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION,
    NO_INHERITANCE, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_MAX_SID_SIZE, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
const FILE_ALL_ACCESS: u32 = 0x001f_01ff;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

struct TokenHandle(HANDLE);

impl Drop for TokenHandle {
    fn drop(&mut self) {
        // SAFETY: OpenProcessToken returned this owned handle.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

struct LocalAcl(*mut ACL);

impl Drop for LocalAcl {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: SetEntriesInAclW allocated this ACL with LocalAlloc.
            unsafe {
                LocalFree(self.0.cast());
            }
        }
    }
}

fn permission_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn current_user_sid_storage() -> io::Result<Vec<usize>> {
    let mut raw_token = std::ptr::null_mut();
    // SAFETY: raw_token is a valid out pointer and GetCurrentProcess supplies a
    // process pseudo-handle valid for OpenProcessToken.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = TokenHandle(raw_token);

    let mut required = 0_u32;
    // This call intentionally queries the required allocation size.
    unsafe {
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut required);
    }
    if required < std::mem::size_of::<TOKEN_USER>() as u32 {
        return Err(io::Error::last_os_error());
    }
    let words = (required as usize).div_ceil(std::mem::size_of::<usize>());
    let mut storage = vec![0_usize; words];
    // SAFETY: usize storage is aligned and contains at least `required`
    // writable bytes.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            storage.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(storage)
}

fn well_known_sid(kind: i32) -> io::Result<Vec<usize>> {
    let mut size = SECURITY_MAX_SID_SIZE;
    let words = (size as usize).div_ceil(std::mem::size_of::<usize>());
    let mut storage = vec![0_usize; words];
    // SAFETY: aligned storage contains SECURITY_MAX_SID_SIZE writable bytes;
    // no domain SID is required for these well-known SIDs.
    if unsafe {
        CreateWellKnownSid(
            kind,
            std::ptr::null_mut(),
            storage.as_mut_ptr().cast(),
            &mut size,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(storage)
}

fn sid_matches(left: PSID, right: PSID) -> bool {
    // SAFETY: both pointers reference SIDs returned by Windows security APIs
    // whose backing allocations remain alive during this call.
    unsafe { EqualSid(left, right) != 0 }
}

fn explicit_full_access(sid: PSID, trustee_type: i32) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: NO_INHERITANCE,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: trustee_type,
            ptstrName: sid.cast(),
        },
    }
}

pub(super) fn set_private_auth_file_acl(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || metadata.file_size() != 0
    {
        return Err(permission_error(
            "private ACL setup requires a new, empty, non-reparse regular file",
        ));
    }

    let current_user_storage = current_user_sid_storage()?;
    // SAFETY: current_user_sid_storage contains a TOKEN_USER for this scope.
    let current_user = unsafe { &*current_user_storage.as_ptr().cast::<TOKEN_USER>() };
    if current_user.User.Sid.is_null() {
        return Err(permission_error("current process token has no user SID"));
    }
    let mut system_storage = well_known_sid(WinLocalSystemSid)?;
    let mut administrators_storage = well_known_sid(WinBuiltinAdministratorsSid)?;
    let entries = [
        explicit_full_access(current_user.User.Sid, TRUSTEE_IS_USER),
        explicit_full_access(
            system_storage.as_mut_ptr().cast(),
            TRUSTEE_IS_WELL_KNOWN_GROUP,
        ),
        explicit_full_access(
            administrators_storage.as_mut_ptr().cast(),
            TRUSTEE_IS_WELL_KNOWN_GROUP,
        ),
    ];
    let mut raw_acl = std::ptr::null_mut();
    // SAFETY: every trustee points at a live, valid SID and raw_acl is a valid
    // output pointer for the LocalAlloc allocation.
    let status = unsafe {
        SetEntriesInAclW(
            entries.len() as u32,
            entries.as_ptr(),
            std::ptr::null(),
            &mut raw_acl,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let acl = LocalAcl(raw_acl);
    // SAFETY: the file handle was opened by the caller with WRITE_OWNER and
    // WRITE_DAC access; the owner SID and ACL remain live for this call.
    let status = unsafe {
        SetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            current_user.User.Sid,
            std::ptr::null_mut(),
            acl.0,
            std::ptr::null(),
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    validate_private_auth_file(file)
}

/// Require the exact persistent-secret ACL contract shared with the launcher:
/// a current-user-owned, non-reparse regular file with a protected DACL and
/// full-access allow entries only for the current user, LocalSystem, and the
/// built-in Administrators group.
pub(super) fn validate_private_auth_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(permission_error(
            "daemon authentication file must be a non-reparse regular file",
        ));
    }

    let current_user_storage = current_user_sid_storage()?;
    // SAFETY: current_user_sid_storage contains a TOKEN_USER for this scope.
    let current_user = unsafe { &*current_user_storage.as_ptr().cast::<TOKEN_USER>() };
    if current_user.User.Sid.is_null() {
        return Err(permission_error("current process token has no user SID"));
    }
    let system_storage = well_known_sid(WinLocalSystemSid)?;
    let administrators_storage = well_known_sid(WinBuiltinAdministratorsSid)?;
    let system_sid = system_storage.as_ptr().cast::<c_void>() as PSID;
    let administrators_sid = administrators_storage.as_ptr().cast::<c_void>() as PSID;

    let mut owner = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut raw_descriptor = std::ptr::null_mut();
    // SAFETY: the file handle is live and all requested output pointers are
    // valid. GetSecurityInfo owns the returned descriptor allocation.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut raw_descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let descriptor = LocalSecurityDescriptor(raw_descriptor);
    if owner.is_null()
        || !sid_matches(owner, current_user.User.Sid)
        || dacl.is_null()
        || descriptor.0.is_null()
    {
        return Err(permission_error(
            "daemon authentication file owner or DACL is invalid",
        ));
    }

    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: descriptor is a live self-relative security descriptor.
    if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Err(permission_error(
            "daemon authentication file DACL must be protected from inheritance",
        ));
    }

    let mut information = ACL_SIZE_INFORMATION::default();
    // SAFETY: dacl belongs to the live descriptor and information is writable.
    if unsafe {
        GetAclInformation(
            dacl,
            (&mut information as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if information.AceCount != 3 {
        return Err(permission_error(
            "daemon authentication file DACL has unexpected principals",
        ));
    }

    let mut seen_user = false;
    let mut seen_system = false;
    let mut seen_administrators = false;
    for index in 0..information.AceCount {
        let mut raw_ace = std::ptr::null_mut();
        // SAFETY: index is below the ACE count reported for this live ACL.
        if unsafe { GetAce(dacl, index, &mut raw_ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: GetAce returned a valid ACE pointer. We reject all layouts
        // except ACCESS_ALLOWED_ACE before reading its SID.
        let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
        if ace.Header.AceType != ACCESS_ALLOWED_ACE_TYPE
            || ace.Header.AceFlags != 0
            || (ace.Mask != GENERIC_ALL && ace.Mask != FILE_ALL_ACCESS)
        {
            return Err(permission_error(
                "daemon authentication file DACL contains a non-private ACE",
            ));
        }
        let sid = (&ace.SidStart as *const u32).cast_mut().cast::<c_void>() as PSID;
        let seen = if sid_matches(sid, current_user.User.Sid) {
            &mut seen_user
        } else if sid_matches(sid, system_sid) {
            &mut seen_system
        } else if sid_matches(sid, administrators_sid) {
            &mut seen_administrators
        } else {
            return Err(permission_error(
                "daemon authentication file DACL grants an unexpected principal",
            ));
        };
        if *seen {
            return Err(permission_error(
                "daemon authentication file DACL contains duplicate principals",
            ));
        }
        *seen = true;
    }

    if !(seen_user && seen_system && seen_administrators) {
        return Err(permission_error(
            "daemon authentication file DACL is incomplete",
        ));
    }
    Ok(())
}
