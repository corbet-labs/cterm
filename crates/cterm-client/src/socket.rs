//! Platform-specific socket path management

use std::path::PathBuf;

/// Get the default socket path for ctermd.
///
/// On macOS, returns a Unix socket path under `~/Library/Application Support/com.cterm.terminal/`.
/// On freedesktop Unix systems, returns a Unix socket path under
/// `$XDG_RUNTIME_DIR/cterm/` or `/tmp/`.
/// On Windows, returns a named pipe path like `\\.\pipe\ctermd-{USERNAME}`.
pub fn default_socket_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            let mut path = PathBuf::from(home);
            path.push("Library/Application Support/com.cterm.terminal");
            std::fs::create_dir_all(&path).ok();
            path.push("ctermd.sock");
            return path;
        }
    }

    #[cfg(all(unix, not(any(target_os = "macos", target_os = "ios"))))]
    {
        // Prefer XDG_RUNTIME_DIR (per-user, tmpfs)
        if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            let path = runtime_socket_path(std::path::Path::new(&runtime_dir));
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            return path;
        }
    }

    // Fallback: /tmp/ctermd-{uid}.sock
    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/ctermd-{}.sock", uid))
    }

    #[cfg(windows)]
    {
        let username = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_string());
        PathBuf::from(format!(r"\\.\pipe\ctermd-{}", username))
    }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "ios"))))]
fn runtime_socket_path(runtime_dir: &std::path::Path) -> PathBuf {
    runtime_dir.join("cterm").join("ctermd.sock")
}

/// Get the path where the ctermd PID file is stored
pub fn pid_file_path() -> PathBuf {
    let mut path = default_socket_path();
    path.set_extension("pid");
    path
}

#[cfg(test)]
mod tests {
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "ios"))))]
    #[test]
    fn xdg_runtime_socket_path_uses_the_shared_cterm_layout() {
        let runtime_dir = std::path::Path::new("/var/run/user/1001");
        assert_eq!(
            super::runtime_socket_path(runtime_dir),
            runtime_dir.join("cterm/ctermd.sock")
        );
    }
}
