//! Shared command-line contract for all native UI backends.

use clap::Parser;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use crate::config::Config;

fn merge_environment(
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
    pub fn env_pairs(&self) -> Vec<(String, String)> {
        self.child_env
            .iter()
            .map(|entry| (entry.name.clone(), entry.value.clone()))
            .collect()
    }

    /// Whether this invocation explicitly requests a new child rather than a
    /// reconnect to an existing daemon session.
    pub fn requests_fresh_session(&self) -> bool {
        self.command.is_some()
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
