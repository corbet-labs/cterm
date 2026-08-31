//! Shared command-line contract for all native UI backends.

use clap::Parser;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use crate::config::Config;

pub(crate) fn merge_environment(
    configured: impl IntoIterator<Item = (String, String)>,
    explicit: impl IntoIterator<Item = (String, String)>,
    case_insensitive: bool,
) -> Vec<(String, String)> {
    let mut env = BTreeMap::new();
    for (name, value) in configured.into_iter().chain(explicit) {
        let key = if case_insensitive {
            name.to_ascii_uppercase()
        } else {
            name.clone()
        };
        env.insert(key, (name, value));
    }
    env.into_values().collect()
}

/// An explicit environment value for the initial child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildEnv {
    pub name: String,
    pub value: String,
}

impl FromStr for ChildEnv {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (name, value) = value
            .split_once('=')
            .ok_or_else(|| "environment values must use NAME=VALUE".to_string())?;
        if name.is_empty() {
            return Err("environment variable name cannot be empty".to_string());
        }
        if name.contains('\0') || value.contains('\0') {
            return Err("environment values cannot contain NUL bytes".to_string());
        }
        Ok(Self {
            name: name.to_string(),
            value: value.to_string(),
        })
    }
}

/// Command-line arguments for cterm.
#[derive(Parser, Debug)]
#[command(
    name = "cterm",
    version,
    about = "A high-performance terminal emulator"
)]
pub struct Args {
    /// Enable the fail-closed embedding contract for a managed product.
    #[arg(
        long,
        requires_all = [
            "config_dir",
            "daemon_socket",
            "daemon_identity",
            "daemon_executable",
            "daemon_auth_file",
            "command"
        ],
        conflicts_with = "upgrade_state"
    )]
    pub managed: bool,

    /// Absolute, isolated configuration directory (managed mode only).
    #[arg(long, value_name = "PATH", requires = "managed")]
    pub config_dir: Option<PathBuf>,

    /// Exact Unix socket or Windows named-pipe endpoint (managed mode only).
    #[arg(long, value_name = "ENDPOINT", requires = "managed")]
    pub daemon_socket: Option<PathBuf>,

    /// Stable daemon identity checked during the handshake (managed mode only).
    #[arg(long, value_name = "IDENTITY", requires = "managed")]
    pub daemon_identity: Option<String>,

    /// Daemon path relative to the UI executable (managed mode only).
    #[arg(long, value_name = "RELATIVE_PATH", requires = "managed")]
    pub daemon_executable: Option<PathBuf>,

    /// Absolute private file containing the managed daemon authentication key.
    #[arg(long, value_name = "PATH", requires = "managed")]
    pub daemon_auth_file: Option<PathBuf>,

    /// Execute a command instead of the default shell.
    #[arg(short = 'e', long = "execute", value_name = "COMMAND")]
    pub command: Option<String>,

    /// Arguments passed verbatim to COMMAND. Once these begin, remaining
    /// hyphenated values are child arguments; `--` is accepted as an explicit
    /// boundary.
    #[arg(
        value_name = "ARG",
        trailing_var_arg = true,
        allow_hyphen_values = true,
        requires = "command"
    )]
    pub command_args: Vec<String>,

    /// Set an environment value for the initial child (repeatable).
    #[arg(long = "env", value_name = "NAME=VALUE")]
    pub child_env: Vec<ChildEnv>,

    /// Set the working directory.
    #[arg(short = 'd', long = "directory")]
    pub directory: Option<PathBuf>,

    /// Start in fullscreen mode.
    #[arg(long)]
    pub fullscreen: bool,

    /// Start maximized.
    #[arg(long)]
    pub maximized: bool,

    /// Set the window title.
    #[arg(short = 't', long = "title")]
    pub title: Option<String>,

    /// Path to upgrade state file (internal use).
    #[arg(long, hide = true)]
    pub upgrade_state: Option<String>,
}

impl Args {
    /// Validate and resolve all managed-product paths without consulting PATH
    /// or the current working directory.
    pub fn managed_runtime(
        &self,
        ui_executable: &std::path::Path,
    ) -> Result<Option<ManagedRuntime>, String> {
        if !self.managed {
            return Ok(None);
        }

        let config_dir = self
            .config_dir
            .clone()
            .ok_or_else(|| "--managed requires --config-dir".to_string())?;
        if !config_dir.is_absolute() {
            return Err("--config-dir must be absolute in managed mode".to_string());
        }

        let relative_daemon = self
            .daemon_executable
            .as_ref()
            .ok_or_else(|| "--managed requires --daemon-executable".to_string())?;
        if relative_daemon.as_os_str().is_empty()
            || relative_daemon.is_absolute()
            || relative_daemon.components().any(|component| {
                !matches!(
                    component,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            })
        {
            return Err(
                "--daemon-executable must stay relative to the UI executable directory".to_string(),
            );
        }
        let executable_dir = ui_executable
            .parent()
            .ok_or_else(|| "UI executable has no package directory".to_string())?;
        let package_dir = executable_dir
            .canonicalize()
            .map_err(|error| format!("cannot resolve UI package directory: {error}"))?;
        let daemon_executable = executable_dir
            .join(relative_daemon)
            .canonicalize()
            .map_err(|error| format!("cannot resolve managed daemon executable: {error}"))?;
        if !daemon_executable.starts_with(&package_dir) {
            return Err("--daemon-executable must resolve inside the UI package directory".into());
        }

        let daemon = cterm_client::ManagedDaemonConfig::new(
            self.daemon_socket
                .clone()
                .ok_or_else(|| "--managed requires --daemon-socket".to_string())?,
            daemon_executable,
            self.daemon_identity
                .clone()
                .ok_or_else(|| "--managed requires --daemon-identity".to_string())?,
            self.daemon_auth_file
                .clone()
                .ok_or_else(|| "--managed requires --daemon-auth-file".to_string())?,
        )?;

        Ok(Some(ManagedRuntime { config_dir, daemon }))
    }

    /// Install the managed runtime before any configuration or daemon access.
    pub fn initialize_runtime(&self, ui_executable: &std::path::Path) -> Result<(), String> {
        let Some(runtime) = self.managed_runtime(ui_executable)? else {
            return Ok(());
        };
        crate::config::set_config_dir_override(runtime.config_dir)?;
        cterm_client::configure_managed_daemon(runtime.daemon)
    }

    /// Start and authenticate the configured managed daemon before opening a
    /// window. This turns missing, mismatched, or occupied product endpoints
    /// into startup errors instead of leaving an empty terminal window.
    pub fn preflight_managed_daemon(&self) -> Result<(), String> {
        if !self.managed {
            return Ok(());
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("cannot create daemon preflight runtime: {error}"))?;
        runtime
            .block_on(cterm_client::DaemonConnection::connect_local())
            .map(|_| ())
            .map_err(|error| format!("managed daemon preflight failed: {error}"))
    }

    /// Managed products own their update pipeline and never expose cterm's
    /// upstream updater.
    pub fn updater_enabled(&self) -> bool {
        !self.managed
    }

    pub fn env_pairs(&self) -> Vec<(String, String)> {
        self.child_env
            .iter()
            .map(|entry| (entry.name.clone(), entry.value.clone()))
            .collect()
    }

    /// Whether this invocation explicitly requests a new child rather than a
    /// reconnect to an existing daemon session.
    pub fn requests_fresh_session(&self) -> bool {
        self.managed
            || self.command.is_some()
            || self.directory.is_some()
            || !self.child_env.is_empty()
            || self.title.is_some()
    }

    /// Whether GTK must avoid forwarding this launch to an existing process,
    /// which cannot observe the new invocation's parsed window flags.
    pub fn requires_non_unique_instance(&self) -> bool {
        self.requests_fresh_session() || self.maximized || self.fullscreen
    }

    /// Build the initial daemon session without interpreting child arguments
    /// as a shell command line. Command-line environment entries override the
    /// configured values, with the last repeated entry winning.
    pub fn initial_session_options(
        &self,
        config: &Config,
        cols: u32,
        rows: u32,
    ) -> cterm_client::CreateSessionOpts {
        let explicit_command = self.command.is_some();
        let env = merge_environment(
            config
                .general
                .env
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
            self.env_pairs(),
            cfg!(windows),
        );

        cterm_client::CreateSessionOpts {
            cols,
            rows,
            shell: self
                .command
                .clone()
                .or_else(|| config.general.default_shell.clone()),
            args: if explicit_command {
                self.command_args.clone()
            } else {
                config.general.shell_args.clone()
            },
            cwd: self
                .directory
                .as_ref()
                .or(config.general.working_directory.as_ref())
                .map(|path| path.to_string_lossy().into_owned()),
            env,
            term: config.general.term.clone(),
            ..Default::default()
        }
    }

    /// Initial title for an explicitly launched child.
    pub fn initial_title(&self, config: &Config) -> String {
        self.title.clone().unwrap_or_else(|| {
            self.command
                .as_deref()
                .or(config.general.default_shell.as_deref())
                .and_then(|program| {
                    PathBuf::from(program)
                        .file_name()?
                        .to_str()
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| "Terminal".to_string())
        })
    }
}

/// Fully resolved runtime contract for an embedded cterm product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRuntime {
    pub config_dir: PathBuf,
    pub daemon: cterm_client::ManagedDaemonConfig,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_auth_file(directory: &std::path::Path) -> PathBuf {
        let path = directory.join("daemon-auth");
        let secret = "42".repeat(32);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            std::fs::write(&path, &secret).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        #[cfg(windows)]
        {
            use std::io::Write;
            use std::os::windows::fs::OpenOptionsExt;

            const FILE_ALL_ACCESS: u32 = 0x001f_01ff;
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .access_mode(FILE_ALL_ACCESS)
                .open(&path)
                .unwrap();
            cterm_proto::set_private_daemon_auth_file_acl(&file).unwrap();
            file.write_all(secret.as_bytes()).unwrap();
        }
        path
    }

    #[test]
    fn parses_command_argv_without_shell_joining() {
        let args = Args::try_parse_from([
            "cterm",
            "--env",
            "FIRST=one",
            "--env",
            "SECOND=two=parts",
            "--execute=/path/with space/tool",
            "--",
            "--literal-flag",
            "two words",
        ])
        .unwrap();

        assert_eq!(args.command.as_deref(), Some("/path/with space/tool"));
        assert_eq!(args.command_args, ["--literal-flag", "two words"]);
        assert_eq!(
            args.env_pairs(),
            [
                ("FIRST".to_string(), "one".to_string()),
                ("SECOND".to_string(), "two=parts".to_string()),
            ]
        );
    }

    #[test]
    fn accepts_hyphenated_trailing_args_without_boundary() {
        let args = Args::try_parse_from(["cterm", "-e", "tool", "--child-flag"]).unwrap();
        assert_eq!(args.command.as_deref(), Some("tool"));
        assert_eq!(args.command_args, ["--child-flag"]);
    }

    #[test]
    fn rejects_trailing_args_without_execute() {
        assert!(Args::try_parse_from(["cterm", "orphan"]).is_err());
    }

    #[test]
    fn rejects_invalid_environment_values() {
        assert!(Args::try_parse_from(["cterm", "--env", "MISSING_EQUALS"]).is_err());
        assert!(Args::try_parse_from(["cterm", "--env", "=empty-name"]).is_err());
    }

    #[test]
    fn explicit_title_requests_a_fresh_titled_session() {
        let args = Args::try_parse_from(["cterm", "--title", "Review"]).unwrap();
        assert!(args.requests_fresh_session());

        let args = Args::try_parse_from(["cterm", "--maximized"]).unwrap();
        assert!(!args.requests_fresh_session());
        assert!(args.requires_non_unique_instance());
    }

    #[test]
    fn builds_initial_session_with_cli_precedence_and_exact_argv() {
        let mut config = Config::default();
        config.general.default_shell = Some("configured-shell".to_string());
        config.general.shell_args = vec!["configured-arg".to_string()];
        config
            .general
            .env
            .insert("SHARED".to_string(), "config".to_string());
        config
            .general
            .env
            .insert("CONFIG_ONLY".to_string(), "yes".to_string());

        let args = Args::try_parse_from([
            "cterm",
            "--env",
            "SHARED=first",
            "--env",
            "SHARED=last",
            "-e",
            "child",
            "--",
            "two words",
            "--literal",
        ])
        .unwrap();
        let opts = args.initial_session_options(&config, 90, 30);

        assert_eq!(opts.shell.as_deref(), Some("child"));
        assert_eq!(opts.args, ["two words", "--literal"]);
        assert_eq!(
            opts.env,
            [
                ("CONFIG_ONLY".to_string(), "yes".to_string()),
                ("SHARED".to_string(), "last".to_string()),
            ]
        );
        assert!(args.requests_fresh_session());
    }

    #[test]
    fn windows_environment_precedence_is_case_insensitive_and_last_wins() {
        let env = merge_environment(
            [
                ("Path".to_string(), "configured".to_string()),
                ("CONFIG_ONLY".to_string(), "yes".to_string()),
            ],
            [
                ("PATH".to_string(), "first".to_string()),
                ("path".to_string(), "last".to_string()),
            ],
            true,
        );

        assert_eq!(
            env,
            [
                ("CONFIG_ONLY".to_string(), "yes".to_string()),
                ("path".to_string(), "last".to_string()),
            ]
        );
    }

    #[test]
    fn managed_mode_resolves_daemon_next_to_ui_and_preserves_child_argv() {
        let dir = tempfile::tempdir().unwrap();
        let ui = dir
            .path()
            .join(if cfg!(windows) { "cterm.exe" } else { "cterm" });
        let daemon_name = if cfg!(windows) {
            "ctermd.exe"
        } else {
            "ctermd"
        };
        let daemon = dir.path().join(daemon_name);
        std::fs::write(&daemon, b"daemon").unwrap();
        let auth_file = write_auth_file(dir.path());
        let config_dir = dir.path().join("isolated-config");
        let socket = if cfg!(windows) {
            PathBuf::from(r"\\.\pipe\cterm-managed-test")
        } else {
            dir.path().join("managed.sock")
        };

        let args = Args::try_parse_from(vec![
            std::ffi::OsString::from("cterm"),
            "--managed".into(),
            "--config-dir".into(),
            config_dir.as_os_str().to_owned(),
            "--daemon-socket".into(),
            socket.as_os_str().to_owned(),
            "--daemon-identity".into(),
            "product-alpha".into(),
            "--daemon-executable".into(),
            daemon_name.into(),
            "--daemon-auth-file".into(),
            auth_file.as_os_str().to_owned(),
            "--execute".into(),
            "tool".into(),
            "--".into(),
            "two words".into(),
            "--literal".into(),
        ])
        .unwrap();

        let runtime = args.managed_runtime(&ui).unwrap().unwrap();
        assert_eq!(runtime.config_dir, config_dir);
        assert_eq!(runtime.daemon.socket_path, socket);
        assert_eq!(runtime.daemon.executable, daemon.canonicalize().unwrap());
        assert_eq!(runtime.daemon.identity, "product-alpha");
        assert_eq!(args.command_args, ["two words", "--literal"]);
        assert!(args.requests_fresh_session());
        assert!(args.requires_non_unique_instance());
        assert!(!args.updater_enabled());

        #[cfg(unix)]
        {
            let mut args = args;
            let outside = tempfile::tempdir().unwrap();
            let outside_daemon = outside.path().join("ctermd");
            std::fs::write(&outside_daemon, b"daemon").unwrap();
            std::os::unix::fs::symlink(&outside_daemon, dir.path().join("escaped-ctermd")).unwrap();
            args.daemon_executable = Some("escaped-ctermd".into());
            assert!(args.managed_runtime(&ui).is_err());
        }
    }

    #[test]
    fn managed_mode_is_complete_and_package_relative_or_rejected() {
        assert!(Args::try_parse_from(["cterm", "--managed"]).is_err());
        assert!(Args::try_parse_from(["cterm", "--config-dir", "/tmp/config"]).is_err());

        let dir = tempfile::tempdir().unwrap();
        let ui = dir.path().join("cterm");
        let auth_file = write_auth_file(dir.path());
        let socket = if cfg!(windows) {
            r"\\.\pipe\cterm-managed-test"
        } else {
            "/tmp/cterm-managed-test.sock"
        };
        let args = Args::try_parse_from(vec![
            std::ffi::OsString::from("cterm"),
            "--managed".into(),
            "--config-dir".into(),
            dir.path().join("config").into_os_string(),
            "--daemon-socket".into(),
            socket.into(),
            "--daemon-identity".into(),
            "product-alpha".into(),
            "--daemon-executable".into(),
            "../ctermd".into(),
            "--daemon-auth-file".into(),
            auth_file.into_os_string(),
            "--execute".into(),
            "product-command".into(),
        ])
        .unwrap();
        assert!(args.managed_runtime(&ui).is_err());
    }
}
