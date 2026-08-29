//! CLI argument parsing for ctermd

use crate::server::ServerConfig;
use clap::Parser;
use std::path::PathBuf;

/// ctermd - Headless terminal daemon with gRPC API
#[derive(Parser, Debug)]
#[command(name = "ctermd")]
#[command(about = "Headless terminal daemon with gRPC API")]
#[command(version)]
pub struct Cli {
    /// Unix socket path (default: platform-specific per-user path)
    #[arg(short = 'l', long = "listen")]
    pub socket_path: Option<String>,

    /// Use TCP instead of Unix socket
    #[arg(long = "tcp")]
    pub use_tcp: bool,

    /// TCP port (only used with --tcp)
    #[arg(short = 'p', long = "port", default_value = "50051")]
    pub port: u16,

    /// TCP bind address (only used with --tcp)
    #[arg(long = "bind", default_value = "127.0.0.1")]
    pub bind_addr: String,

    /// Log level
    #[arg(long = "log-level", default_value = "info")]
    pub log_level: String,

    /// Run in foreground (don't daemonize)
    #[arg(short = 'f', long = "foreground")]
    pub foreground: bool,

    /// Print the default socket path and exit
    #[arg(long = "print-socket-path")]
    pub print_socket_path: bool,

    /// Stable logical identity reported during the client handshake.
    #[arg(long, default_value = "cterm")]
    pub identity: String,

    /// Absolute private file containing the managed daemon authentication key.
    #[arg(long, value_name = "PATH")]
    pub daemon_auth_file: Option<PathBuf>,

    /// Default scrollback lines for new sessions (0 = no scrollback)
    #[arg(long = "scrollback", default_value = "10000")]
    pub scrollback_lines: usize,

    /// Path to relaunch state file (internal, used during daemon relaunch)
    #[arg(long = "relaunch-state", hide = true)]
    pub relaunch_state: Option<String>,
}

impl Cli {
    /// Parse command-line arguments
    pub fn parse_args() -> Self {
        Cli::parse()
    }

    /// Convert CLI arguments to ServerConfig
    pub fn to_server_config(&self) -> Result<ServerConfig, String> {
        let socket_path = self
            .socket_path
            .clone()
            .unwrap_or_else(|| default_socket_path().to_string_lossy().to_string());

        if self.daemon_auth_file.is_some() && self.use_tcp {
            return Err(
                "--daemon-auth-file is restricted to the protected local socket/pipe transport"
                    .to_string(),
            );
        }
        if self.daemon_auth_file.is_some() && self.relaunch_state.is_some() {
            return Err("daemon relaunch state is disabled with managed authentication".into());
        }
        #[cfg(unix)]
        if self.daemon_auth_file.is_some() && !std::path::Path::new(&socket_path).is_absolute() {
            return Err(
                "an authenticated daemon requires an absolute local socket path".to_string(),
            );
        }

        let auth_secret = self
            .daemon_auth_file
            .as_deref()
            .map(cterm_proto::load_daemon_auth_secret)
            .transpose()
            .map_err(|error| format!("invalid daemon authentication file: {error}"))?;

        Ok(ServerConfig {
            use_tcp: self.use_tcp,
            bind_addr: self.bind_addr.clone(),
            port: self.port,
            socket_path,
            identity: self.identity.clone(),
            auth_file: self.daemon_auth_file.clone(),
            auth_secret,
            scrollback_lines: self.scrollback_lines,
            foreground: self.foreground,
        })
    }
}

/// Get the cterm config directory path.
pub fn config_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            let mut path = PathBuf::from(home);
            path.push("Library/Application Support/com.cterm.terminal");
            std::fs::create_dir_all(&path).ok();
            return path;
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(config_dir) = std::env::var("XDG_CONFIG_HOME") {
            let path = PathBuf::from(config_dir).join("cterm");
            std::fs::create_dir_all(&path).ok();
            return path;
        }
        if let Some(home) = std::env::var_os("HOME") {
            let path = PathBuf::from(home).join(".config/cterm");
            std::fs::create_dir_all(&path).ok();
            return path;
        }
    }

    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            let path = PathBuf::from(appdata).join("cterm");
            std::fs::create_dir_all(&path).ok();
            return path;
        }
    }

    PathBuf::from("/tmp")
}

/// Get the default Unix socket path for ctermd.
///
/// This matches the path used by cterm-client for auto-discovery.
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

    #[cfg(target_os = "linux")]
    {
        if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            let mut path = PathBuf::from(runtime_dir);
            path.push("cterm");
            std::fs::create_dir_all(&path).ok();
            path.push("ctermd.sock");
            return path;
        }
    }

    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/ctermd-{}.sock", uid))
    }

    #[cfg(not(unix))]
    {
        let username = std::env::var("USERNAME").unwrap_or_else(|_| "default".to_string());
        PathBuf::from(format!(r"\\.\pipe\ctermd-{}", username))
    }
}

/// Get the path where the ctermd PID file is stored
pub fn pid_file_path() -> PathBuf {
    let mut path = default_socket_path();
    path.set_extension("pid");
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_values() {
        let cli = Cli::parse_from(["ctermd"]);
        assert!(cli.socket_path.is_none());
        assert!(!cli.use_tcp);
        assert_eq!(cli.port, 50051);
        assert_eq!(cli.bind_addr, "127.0.0.1");
        assert_eq!(cli.log_level, "info");
        assert_eq!(cli.identity, "cterm");
        assert!(!cli.foreground);
        assert_eq!(cli.scrollback_lines, 10000);
    }

    #[test]
    fn test_tcp_mode() {
        let cli = Cli::parse_from(["ctermd", "--tcp", "-p", "8080"]);
        assert!(cli.use_tcp);
        assert_eq!(cli.port, 8080);
    }

    #[test]
    fn test_custom_socket() {
        let cli = Cli::parse_from(["ctermd", "-l", "/var/run/ctermd.sock"]);
        assert_eq!(cli.socket_path, Some("/var/run/ctermd.sock".to_string()));
    }

    #[test]
    fn test_custom_identity() {
        let cli = Cli::parse_from(["ctermd", "--identity", "managed-product"]);
        assert_eq!(cli.identity, "managed-product");
        assert_eq!(cli.to_server_config().unwrap().identity, "managed-product");
    }

    #[test]
    fn authenticated_tcp_transport_is_rejected_before_secret_loading() {
        let cli = Cli::parse_from([
            "ctermd",
            "--tcp",
            "--daemon-auth-file",
            "/private/daemon-auth",
        ]);
        assert!(cli.to_server_config().is_err());
    }

    #[test]
    fn authenticated_relaunch_state_is_rejected_before_secret_loading() {
        let cli = Cli::parse_from([
            "ctermd",
            "--daemon-auth-file",
            "/private/daemon-auth",
            "--relaunch-state",
            "/tmp/untrusted-state",
        ]);
        assert!(cli.to_server_config().is_err());
    }

    #[test]
    fn test_custom_scrollback() {
        let cli = Cli::parse_from(["ctermd", "--scrollback", "5000"]);
        assert_eq!(cli.scrollback_lines, 5000);
    }

    #[test]
    fn test_no_scrollback() {
        let cli = Cli::parse_from(["ctermd", "--scrollback", "0"]);
        assert_eq!(cli.scrollback_lines, 0);
    }

    #[test]
    fn test_default_socket_path() {
        let path = default_socket_path();
        assert!(path.to_string_lossy().contains("ctermd"));
    }
}
