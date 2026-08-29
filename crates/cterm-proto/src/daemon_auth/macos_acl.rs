use std::ffi::{c_char, c_int, c_void, CString};
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

type Acl = *mut c_void;

const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
const ACL_FIRST_ENTRY: c_int = 0;

unsafe extern "C" {
    fn acl_get_fd_np(fd: c_int, acl_type: c_int) -> Acl;
    fn acl_get_link_np(path: *const c_char, acl_type: c_int) -> Acl;
    fn acl_get_entry(acl: Acl, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
    fn acl_free(object: *mut c_void) -> c_int;
}

struct OwnedAcl(Acl);

impl Drop for OwnedAcl {
    fn drop(&mut self) {
        // SAFETY: the ACL was returned by an acl_get_* function and has not
        // otherwise been freed.
        unsafe {
            acl_free(self.0);
        }
    }
}

fn validate_acl_result(raw_acl: Acl) -> io::Result<()> {
    if raw_acl.is_null() {
        // Darwin reports a missing FILESEC_ACL property as ENOENT. Every other
        // failure is fail-closed; unsupported ACL inspection does not prove
        // that another principal lacks access.
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(())
        } else {
            Err(error)
        };
    }
    let acl = OwnedAcl(raw_acl);
    let mut entry = std::ptr::null_mut();
    // Darwin's acl_get_entry returns zero when the requested entry exists and
    // -1/EINVAL for an empty ACL. Any ACE, allow or deny, violates the private
    // managed-object contract.
    unsafe {
        *libc::__error() = 0;
    }
    let result = unsafe { acl_get_entry(acl.0, ACL_FIRST_ENTRY, &mut entry) };
    if result == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "managed daemon object must not have a macOS extended ACL",
        ));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EINVAL) {
        Ok(())
    } else {
        Err(error)
    }
}

pub(super) fn validate_fd_has_no_extended_acl(file: &File) -> io::Result<()> {
    // Clear stale errno so a null/no-ACL result cannot inherit an unrelated
    // error from earlier work in this thread.
    unsafe {
        *libc::__error() = 0;
    }
    // SAFETY: the file descriptor is live for this call.
    let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
    validate_acl_result(acl)
}

pub(super) fn validate_path_has_no_extended_acl(path: &Path) -> io::Result<()> {
    let encoded = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed daemon path contains a NUL byte",
        )
    })?;
    unsafe {
        *libc::__error() = 0;
    }
    // SAFETY: encoded is NUL-terminated and acl_get_link_np does not follow a
    // final symlink.
    let acl = unsafe { acl_get_link_np(encoded.as_ptr(), ACL_TYPE_EXTENDED) };
    validate_acl_result(acl)
}
