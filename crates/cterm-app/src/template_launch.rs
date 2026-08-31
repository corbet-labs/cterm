//! Shared launch planning for configured tab templates.

use std::path::{Path, PathBuf};

use cterm_client::{CreateSessionOpts, SshParams};

use crate::cli::merge_environment;
use crate::config::{Config, StickyTabConfig};

/// Where the daemon that creates the terminal session runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateDaemonTarget {
    /// Use the local ctermd instance.
    Local,
    /// Connect to a configured remote ctermd instance.
    Named(TemplateNamedRemote),
}

/// Resolved connection intent for a configured named remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateNamedRemote {
    /// Configuration key used by remote connection managers.
    pub name: String,
    /// SSH destination used to reach the remote daemon.
    pub host: String,
    /// Whether the daemon tunnel requests SSH compression.
    pub ssh_compression: bool,
}

/// The mutually exclusive process type described by a template.
#[derive(Debug, Clone, PartialEq)]
pub enum TemplateLaunchTarget {
    /// Run a normal process or the selected daemon's default shell.
    Local {
        /// Executable, or `None` for the selected daemon's default shell.
        command: Option<String>,
        /// Exact argv entries, never a joined shell command line.
        args: Vec<String>,
    },
    /// Ask ctermd to open a native SSH session.
    Ssh {
        /// Wire-level native SSH parameters accepted by ctermd.
        parameters: SshParams,
    },
    /// Run an argv-safe Docker command on the selected daemon host.
    Docker {
        /// Docker executable selected by the existing command builder.
        command: String,
        /// Exact Docker argv entries.
        args: Vec<String>,
    },
}

/// Whether opening a template should reuse an existing named session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateInstancePolicy {
    /// Opening the template always creates another session.
    AlwaysCreate,
    /// Focus an existing tab carrying this plan's template name when present.
    ReuseExisting,
}

/// Template-level visual overrides that a native adapter applies to a tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateAppearance {
    /// Optional native tab indicator color.
    pub tab_color: Option<String>,
    /// Optional named theme override.
    pub theme: Option<String>,
    /// Optional locked terminal background color.
    pub background_color: Option<String>,
}

/// Which machine owns a configured template working directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateWorkspaceLocation {
    /// The path belongs to the frontend host and may be prepared locally.
    Local,
    /// The path belongs to the selected named daemon host.
    NamedRemote,
}

/// Which configuration layer supplied a template working directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateWorkspaceSource {
    /// The template explicitly requested this directory and optional clone.
    Template,
    /// The application-wide working-directory default filled an omitted value.
    GeneralDefault,
}

/// Working-directory and clone intent for a template session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateWorkspace {
    /// Working directory passed to ctermd.
    pub directory: PathBuf,
    /// Repository to clone when the directory does not yet exist.
    pub git_remote: Option<String>,
    /// Host on which the directory and clone intent belong.
    pub location: TemplateWorkspaceLocation,
    /// Configuration layer that supplied the directory.
    pub source: TemplateWorkspaceSource,
}

/// A frontend-independent, fully resolved template launch description.
///
/// Frontends add runtime geometry, palette, and window state to the returned
/// [`CreateSessionOpts`], then connect to [`Self::daemon`]. No process is
/// started and no working directory is prepared while building the plan.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateLaunchPlan {
    /// Stable template identity stored on the resulting session.
    pub template_name: String,
    /// Process kind and argv-safe launch data.
    pub target: TemplateLaunchTarget,
    /// Daemon endpoint that owns the resulting session.
    pub daemon: TemplateDaemonTarget,
    /// Applicable working-directory and clone intent.
    pub workspace: Option<TemplateWorkspace>,
    /// Merged configured and template-specific child environment.
    pub environment: Vec<(String, String)>,
    /// Terminal type passed to ctermd.
    pub term: Option<String>,
    /// Native presentation overrides.
    pub appearance: TemplateAppearance,
    /// Whether the UI keeps the tab after its process exits.
    pub keep_open: bool,
    /// Whether the UI creates or focuses by template identity.
    pub instance_policy: TemplateInstancePolicy,
}

/// Why a configured template cannot be normalized into one launch plan.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateLaunchPlanError {
    #[error("template `{template}` configures both Docker and SSH launch targets")]
    ConflictingTargets { template: String },
    #[error("template `{template}` did not produce a Docker command")]
    MissingDockerCommand { template: String },
    #[error("template `{template}` references unknown remote `{remote}`")]
    UnknownRemote { template: String, remote: String },
}

impl TemplateLaunchPlan {
    /// Resolve a stored template against application defaults and named remotes.
    pub fn build(
        template: &StickyTabConfig,
        config: &Config,
    ) -> Result<Self, TemplateLaunchPlanError> {
        if template.docker.is_some() && template.ssh.is_some() {
            return Err(TemplateLaunchPlanError::ConflictingTargets {
                template: template.name.clone(),
            });
        }

        let daemon = match template.remote.as_deref() {
            None => TemplateDaemonTarget::Local,
            Some(name) => {
                let remote = config.find_remote(name).ok_or_else(|| {
                    TemplateLaunchPlanError::UnknownRemote {
                        template: template.name.clone(),
                        remote: name.to_string(),
                    }
                })?;
                TemplateDaemonTarget::Named(TemplateNamedRemote {
                    name: remote.name.clone(),
                    host: remote.host.clone(),
                    ssh_compression: remote.ssh_compression,
                })
            }
        };
        let local_daemon = daemon == TemplateDaemonTarget::Local;

        let target = if template.docker.is_some() {
            let (command, args) = template.get_command_args();
            TemplateLaunchTarget::Docker {
                command: command.ok_or_else(|| TemplateLaunchPlanError::MissingDockerCommand {
                    template: template.name.clone(),
                })?,
                args,
            }
        } else if let Some(ssh) = template.ssh.as_ref() {
            TemplateLaunchTarget::Ssh {
                parameters: ssh.to_ssh_params(),
            }
        } else {
            let command = template.command.clone().or_else(|| {
                local_daemon
                    .then(|| config.general.default_shell.clone())
                    .flatten()
            });
            let args = if local_daemon && template.command.is_none() && template.args.is_empty() {
                config.general.shell_args.clone()
            } else {
                template.args.clone()
            };
            TemplateLaunchTarget::Local { command, args }
        };

        let workspace = if matches!(&target, TemplateLaunchTarget::Ssh { .. }) {
            None
        } else if let Some(directory) = template.working_directory.as_ref() {
            Some(TemplateWorkspace {
                directory: directory.clone(),
                git_remote: template.git_remote.clone(),
                location: if local_daemon {
                    TemplateWorkspaceLocation::Local
                } else {
                    TemplateWorkspaceLocation::NamedRemote
                },
                source: TemplateWorkspaceSource::Template,
            })
        } else if local_daemon {
            config
                .general
                .working_directory
                .as_ref()
                .map(|directory| TemplateWorkspace {
                    directory: directory.clone(),
                    git_remote: None,
                    location: TemplateWorkspaceLocation::Local,
                    source: TemplateWorkspaceSource::GeneralDefault,
                })
        } else {
            None
        };

        Ok(Self {
            template_name: template.name.clone(),
            target,
            daemon,
            workspace,
            environment: merge_environment(
                config
                    .general
                    .env
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone())),
                template
                    .env
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone())),
                cfg!(windows),
            ),
            term: config.general.term.clone(),
            appearance: TemplateAppearance {
                tab_color: template.color.clone(),
                theme: template.theme.clone(),
                background_color: template.background_color.clone(),
            },
            keep_open: template.keep_open,
            instance_policy: if template.unique {
                TemplateInstancePolicy::ReuseExisting
            } else {
                TemplateInstancePolicy::AlwaysCreate
            },
        })
    }

    /// Produce daemon session options for runtime terminal dimensions.
    pub fn session_options(&self, cols: u32, rows: u32) -> CreateSessionOpts {
        let (shell, args, ssh) = match &self.target {
            TemplateLaunchTarget::Local { command, args } => (command.clone(), args.clone(), None),
            TemplateLaunchTarget::Ssh { parameters } => {
                (None, Vec::new(), Some(parameters.clone()))
            }
            TemplateLaunchTarget::Docker { command, args } => {
                (Some(command.clone()), args.clone(), None)
            }
        };

        CreateSessionOpts {
            cols,
            rows,
            shell,
            args,
            cwd: self
                .workspace
                .as_ref()
                .map(|workspace| workspace.directory.to_string_lossy().into_owned()),
            env: self.environment.clone(),
            term: self.term.clone(),
            ssh,
            ..Default::default()
        }
    }

    /// Return explicit local workspace preparation intent.
    ///
    /// General working-directory defaults are passed to ctermd but are not
    /// created or cloned by template launchers. Named-remote paths must be
    /// prepared by their owning daemon host, never by the local frontend.
    pub fn local_workspace_preparation(&self) -> Option<(&Path, Option<&str>)> {
        self.workspace
            .as_ref()
            .filter(|workspace| {
                workspace.location == TemplateWorkspaceLocation::Local
                    && workspace.source == TemplateWorkspaceSource::Template
            })
            .map(|workspace| {
                (
                    workspace.directory.as_path(),
                    workspace.git_remote.as_deref(),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::*;
    use crate::config::{
        Config, DockerMode, DockerTabConfig, RemoteConfig, SshTabConfig, StickyTabConfig,
    };

    #[test]
    fn local_plan_resolves_defaults_and_preserves_template_metadata() {
        let mut config = Config::default();
        config.general.default_shell = Some("/bin/fish".into());
        config.general.shell_args = vec!["--login".into()];
        config.general.working_directory = Some(PathBuf::from("/srv/default"));
        config
            .general
            .env
            .insert("SHARED".into(), "configured".into());
        config
            .general
            .env
            .insert("CONFIG_ONLY".into(), "yes".into());
        config.general.term = Some("xterm-direct".into());

        let template = StickyTabConfig {
            name: "Editor".into(),
            color: Some("#112233".into()),
            theme: Some("Solarized Dark".into()),
            background_color: Some("#001122".into()),
            keep_open: true,
            unique: true,
            env: HashMap::from([
                ("SHARED".into(), "template".into()),
                ("TEMPLATE_ONLY".into(), "yes".into()),
            ]),
            ..Default::default()
        };

        let plan = TemplateLaunchPlan::build(&template, &config).unwrap();

        assert_eq!(plan.template_name, "Editor");
        assert_eq!(plan.daemon, TemplateDaemonTarget::Local);
        assert_eq!(plan.instance_policy, TemplateInstancePolicy::ReuseExisting);
        assert_eq!(
            plan.appearance,
            TemplateAppearance {
                tab_color: Some("#112233".into()),
                theme: Some("Solarized Dark".into()),
                background_color: Some("#001122".into()),
            }
        );
        assert!(plan.keep_open);
        assert!(matches!(
            &plan.target,
            TemplateLaunchTarget::Local {
                command: Some(command),
                args,
            } if command == "/bin/fish" && args == &["--login"]
        ));
        assert_eq!(
            plan.workspace,
            Some(TemplateWorkspace {
                directory: PathBuf::from("/srv/default"),
                git_remote: None,
                location: TemplateWorkspaceLocation::Local,
                source: TemplateWorkspaceSource::GeneralDefault,
            })
        );
        assert!(plan.local_workspace_preparation().is_none());
        assert_eq!(
            plan.environment,
            [
                ("CONFIG_ONLY".into(), "yes".into()),
                ("SHARED".into(), "template".into()),
                ("TEMPLATE_ONLY".into(), "yes".into()),
            ]
        );

        let options = plan.session_options(132, 43);
        assert_eq!((options.cols, options.rows), (132, 43));
        assert_eq!(options.shell.as_deref(), Some("/bin/fish"));
        assert_eq!(options.args, ["--login"]);
        assert_eq!(options.cwd.as_deref(), Some("/srv/default"));
        assert_eq!(options.term.as_deref(), Some("xterm-direct"));
        assert!(options.ssh.is_none());
    }

    #[test]
    fn explicit_local_process_and_workspace_override_defaults() {
        let mut config = Config::default();
        config.general.default_shell = Some("/bin/default".into());
        config.general.shell_args = vec!["--default".into()];
        config.general.working_directory = Some(PathBuf::from("/default"));
        let template = StickyTabConfig {
            command: Some("just".into()),
            args: vec!["serve".into(), "--fast".into()],
            working_directory: Some(PathBuf::from("/work/project")),
            git_remote: Some("https://example.test/project.git".into()),
            ..Default::default()
        };

        let plan = TemplateLaunchPlan::build(&template, &config).unwrap();

        assert!(matches!(
            &plan.target,
            TemplateLaunchTarget::Local {
                command: Some(command),
                args,
            } if command == "just" && args == &["serve", "--fast"]
        ));
        assert_eq!(
            plan.local_workspace_preparation(),
            Some((
                PathBuf::from("/work/project").as_path(),
                Some("https://example.test/project.git")
            ))
        );
    }

    #[test]
    fn docker_plan_uses_existing_argv_safe_command_builder() {
        let template = StickyTabConfig {
            docker: Some(DockerTabConfig {
                mode: DockerMode::Run,
                image: Some("alpine:latest".into()),
                shell: Some("/bin/ash".into()),
                docker_args: vec!["--pull=never".into()],
                auto_remove: true,
                ..Default::default()
            }),
            ..Default::default()
        };

        let plan = TemplateLaunchPlan::build(&template, &Config::default()).unwrap();

        assert!(matches!(
            &plan.target,
            TemplateLaunchTarget::Docker { command, args }
                if command == "docker"
                    && args == &["run", "-it", "--rm", "--pull=never", "alpine:latest", "/bin/ash"]
        ));
        let options = plan.session_options(80, 24);
        assert_eq!(options.shell.as_deref(), Some("docker"));
        assert_eq!(
            options.args,
            [
                "run",
                "-it",
                "--rm",
                "--pull=never",
                "alpine:latest",
                "/bin/ash"
            ]
        );
        assert!(options.ssh.is_none());
    }

    #[test]
    fn ssh_plan_uses_native_parameters_and_omits_process_cwd() {
        let mut config = Config::default();
        config.general.default_shell = Some("/bin/must-not-run".into());
        config.general.working_directory = Some(PathBuf::from("/must-not-cross-ssh"));
        config.general.term = Some("xterm-256color".into());
        let template = StickyTabConfig {
            working_directory: Some(PathBuf::from("/also-not-applicable")),
            ssh: Some(SshTabConfig {
                host: "server.example".into(),
                port: Some(2222),
                username: Some("deploy".into()),
                remote_command: Some("tmux attach".into()),
                agent_forward: true,
                ..Default::default()
            }),
            ..Default::default()
        };

        let plan = TemplateLaunchPlan::build(&template, &config).unwrap();

        assert!(matches!(
            &plan.target,
            TemplateLaunchTarget::Ssh { parameters }
                if parameters.host == "server.example"
                    && parameters.port == 2222
                    && parameters.username.as_deref() == Some("deploy")
                    && parameters.remote_command.as_deref() == Some("tmux attach")
                    && parameters.agent_forward
        ));
        assert!(plan.workspace.is_none());
        let options = plan.session_options(100, 30);
        assert!(options.shell.is_none());
        assert!(options.args.is_empty());
        assert!(options.cwd.is_none());
        assert_eq!(options.term.as_deref(), Some("xterm-256color"));
        assert!(options.ssh.is_some());
    }

    #[test]
    fn named_remote_keeps_explicit_intent_without_leaking_local_defaults() {
        let mut config = Config::default();
        config.general.default_shell = Some("/bin/local-only".into());
        config.general.shell_args = vec!["--local-only".into()];
        config.general.working_directory = Some(PathBuf::from("/local-only"));
        config.remotes.push(RemoteConfig {
            name: "build".into(),
            host: "dev@build.example".into(),
            ssh_compression: false,
        });
        let template = StickyTabConfig {
            remote: Some("build".into()),
            working_directory: Some(PathBuf::from("/srv/project")),
            git_remote: Some("https://example.test/project.git".into()),
            ..Default::default()
        };

        let plan = TemplateLaunchPlan::build(&template, &config).unwrap();

        assert_eq!(
            plan.daemon,
            TemplateDaemonTarget::Named(TemplateNamedRemote {
                name: "build".into(),
                host: "dev@build.example".into(),
                ssh_compression: false,
            })
        );
        assert!(matches!(
            &plan.target,
            TemplateLaunchTarget::Local {
                command: None,
                args,
            } if args.is_empty()
        ));
        assert_eq!(
            plan.workspace,
            Some(TemplateWorkspace {
                directory: PathBuf::from("/srv/project"),
                git_remote: Some("https://example.test/project.git".into()),
                location: TemplateWorkspaceLocation::NamedRemote,
                source: TemplateWorkspaceSource::Template,
            })
        );
        assert!(plan.local_workspace_preparation().is_none());
        let options = plan.session_options(80, 24);
        assert!(options.shell.is_none());
        assert!(options.args.is_empty());
        assert_eq!(options.cwd.as_deref(), Some("/srv/project"));
    }

    #[test]
    fn unknown_named_remote_is_a_typed_error() {
        let template = StickyTabConfig {
            name: "Missing remote".into(),
            remote: Some("nowhere".into()),
            ..Default::default()
        };

        assert_eq!(
            TemplateLaunchPlan::build(&template, &Config::default()),
            Err(TemplateLaunchPlanError::UnknownRemote {
                template: "Missing remote".into(),
                remote: "nowhere".into(),
            })
        );
    }

    #[test]
    fn conflicting_ssh_and_docker_targets_are_rejected() {
        let template = StickyTabConfig {
            name: "Ambiguous".into(),
            docker: Some(DockerTabConfig::default()),
            ssh: Some(SshTabConfig::default()),
            ..Default::default()
        };

        assert_eq!(
            TemplateLaunchPlan::build(&template, &Config::default()),
            Err(TemplateLaunchPlanError::ConflictingTargets {
                template: "Ambiguous".into(),
            })
        );
    }
}
