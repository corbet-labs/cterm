//! Frontend-neutral authorization and execution flow for command plugins.
//!
//! Native frontends receive only catalog descriptors, explicit approval
//! requests, and ordinary [`cterm_ui::Action`] values. Package revalidation,
//! grant persistence, protobuf details, and broker launch policy remain here.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::PathBuf;

use cterm_plugin_api::{
    proto, ActionScope, CommandId, GrantDecision, GrantStore, PluginBundle, PluginId,
    PluginPackageError,
};
use cterm_ui::{
    Action, ActionInvocation, ActionInvocationError, ActionParameter, PaneDirection, SplitDirection,
};
use thiserror::Error;

use crate::plugin_broker::{PluginBroker, PluginBrokerError, PluginBrokerOutput};
use crate::plugins::{
    PluginCatalog, PluginCatalogError, PluginCommandDescriptor, PluginDataPaths, PluginGrantFile,
    PluginStorageError,
};

/// Loaded machine-local plugin state shared by the native frontends.
#[derive(Debug)]
pub struct PluginRuntime {
    paths: PluginDataPaths,
    grant_file: PluginGrantFile,
    catalog: PluginCatalog,
    grants: GrantStore,
}

impl PluginRuntime {
    /// Load the current user's catalog and grant store.
    pub fn for_current_user() -> Result<Self, PluginRuntimeError> {
        let paths = PluginDataPaths::for_current_user()
            .ok_or(PluginRuntimeError::LocalDataDirectoryUnavailable)?;
        Self::load(paths)
    }

    /// Load plugin state below explicit local-data paths.
    pub fn load(paths: PluginDataPaths) -> Result<Self, PluginRuntimeError> {
        let grant_file = PluginGrantFile::new(paths.grants_file());
        let catalog = PluginCatalog::discover(paths.plugins_root())?;
        let grants = grant_file.load()?;
        Ok(Self {
            paths,
            grant_file,
            catalog,
            grants,
        })
    }

    pub fn paths(&self) -> &PluginDataPaths {
        &self.paths
    }

    pub fn catalog(&self) -> &PluginCatalog {
        &self.catalog
    }

    /// Transactionally reload catalog and grant state from disk.
    pub fn refresh(&mut self) -> Result<(), PluginRuntimeError> {
        let catalog = PluginCatalog::discover(self.paths.plugins_root())?;
        let grants = self.grant_file.load()?;
        self.catalog = catalog;
        self.grants = grants;
        Ok(())
    }

    /// Revalidate a discovered command and determine whether native approval
    /// is required before it may run.
    pub fn authorize(
        &self,
        command: &PluginCommandDescriptor,
    ) -> Result<PluginAuthorization, PluginRuntimeError> {
        let bundle = self.load_unchanged_bundle(command)?;
        match self.grants.decision(&bundle) {
            GrantDecision::Granted => Ok(PluginAuthorization::Granted(
                self.invocation(command.plugin_id().clone(), command.command_id().clone()),
            )),
            GrantDecision::ApprovalRequired {
                missing,
                content_changed,
            } => Ok(PluginAuthorization::ApprovalRequired(
                PluginApprovalPrompt {
                    command: command.clone(),
                    missing,
                    content_changed,
                },
            )),
        }
    }

    /// Persist approval for the exact package shown by a native prompt and
    /// return a worker-thread-safe invocation snapshot.
    pub fn approve(
        &mut self,
        prompt: PluginApprovalPrompt,
    ) -> Result<PluginInvocation, PluginRuntimeError> {
        let bundle = self.load_unchanged_bundle(&prompt.command)?;
        self.grant_file.approve_and_save(
            &mut self.grants,
            &bundle,
            bundle.manifest().invoke_actions().clone(),
        )?;
        Ok(self.invocation(
            prompt.command.plugin_id().clone(),
            prompt.command.command_id().clone(),
        ))
    }

    /// Atomically revoke every saved digest for one plugin ID.
    pub fn revoke(&mut self, plugin: &PluginId) -> Result<(), PluginRuntimeError> {
        self.grant_file.revoke_and_save(&mut self.grants, plugin)?;
        Ok(())
    }

    fn invocation(&self, plugin: PluginId, command: CommandId) -> PluginInvocation {
        PluginInvocation {
            plugins_root: self.paths.plugins_root().to_path_buf(),
            grants: self.grants.clone(),
            plugin,
            command,
        }
    }

    fn load_unchanged_bundle(
        &self,
        command: &PluginCommandDescriptor,
    ) -> Result<PluginBundle, PluginRuntimeError> {
        let canonical_root = fs::canonicalize(self.paths.plugins_root()).map_err(|source| {
            PluginRuntimeError::PluginRoot {
                path: self.paths.plugins_root().to_path_buf(),
                source,
            }
        })?;
        let bundle = PluginBundle::load(command.package_root())?;
        let declared_command = bundle.manifest().command(command.command_id());
        if bundle.root().parent() != Some(canonical_root.as_path())
            || bundle.root() != command.package_root()
            || bundle.manifest().id() != command.plugin_id()
            || bundle.digest() != command.digest()
            || declared_command.is_none()
        {
            return Err(PluginRuntimeError::CatalogChanged(
                command.action_id().to_string(),
            ));
        }
        Ok(bundle)
    }
}

/// Result of checking one selected command against exact local grants.
#[derive(Debug)]
pub enum PluginAuthorization {
    Granted(PluginInvocation),
    ApprovalRequired(PluginApprovalPrompt),
}

/// Information a native frontend must show before granting authority.
#[derive(Debug)]
pub struct PluginApprovalPrompt {
    command: PluginCommandDescriptor,
    missing: BTreeSet<ActionScope>,
    content_changed: bool,
}

impl PluginApprovalPrompt {
    pub fn command(&self) -> &PluginCommandDescriptor {
        &self.command
    }

    pub fn missing_actions(&self) -> &BTreeSet<ActionScope> {
        &self.missing
    }

    pub const fn content_changed(&self) -> bool {
        self.content_changed
    }
}

/// Immutable invocation data that can be moved off a native UI thread.
#[derive(Debug, Clone)]
pub struct PluginInvocation {
    plugins_root: PathBuf,
    grants: GrantStore,
    plugin: PluginId,
    command: CommandId,
}

impl PluginInvocation {
    pub fn plugin_id(&self) -> &PluginId {
        &self.plugin
    }

    pub fn command_id(&self) -> &CommandId {
        &self.command
    }

    /// Launch the package-relative isolated runner and convert its bounded
    /// response into the same typed actions used by native shortcuts.
    pub async fn execute(self) -> Result<PluginExecution, PluginRuntimeError> {
        let broker = PluginBroker::discover(&self.plugins_root)?;
        let output = broker
            .invoke(&self.grants, &self.plugin, &self.command)
            .await?;
        PluginExecution::from_broker_output(output)
    }
}

/// Fully validated result ready for native action-policy dispatch.
#[derive(Debug)]
pub struct PluginExecution {
    actions: Vec<Action>,
    diagnostics: Vec<String>,
    host_stderr: Vec<u8>,
}

impl PluginExecution {
    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }

    pub fn host_stderr(&self) -> &[u8] {
        &self.host_stderr
    }

    fn from_broker_output(output: PluginBrokerOutput) -> Result<Self, PluginRuntimeError> {
        let (response, host_stderr) = output.into_parts();
        let actions = response
            .actions
            .iter()
            .map(convert_action)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            actions,
            diagnostics: response.diagnostics,
            host_stderr,
        })
    }
}

fn convert_action(action: &proto::ActionInvocation) -> Result<Action, PluginRuntimeError> {
    use proto::action_invocation::Parameter;

    let parameter = match action.parameter {
        None => None,
        Some(Parameter::Tab(tab)) => {
            let tab = u8::try_from(tab).map_err(|_| PluginRuntimeError::InvalidWireParameter {
                kind: "tab",
                value: i64::from(tab),
            })?;
            Some(ActionParameter::Tab(tab))
        }
        Some(Parameter::SplitDirection(value)) => {
            let direction = match proto::SplitDirection::try_from(value) {
                Ok(proto::SplitDirection::Horizontal) => SplitDirection::Horizontal,
                Ok(proto::SplitDirection::Vertical) => SplitDirection::Vertical,
                Ok(proto::SplitDirection::Unspecified) | Err(_) => {
                    return Err(PluginRuntimeError::InvalidWireParameter {
                        kind: "split direction",
                        value: i64::from(value),
                    });
                }
            };
            Some(ActionParameter::SplitDirection(direction))
        }
        Some(Parameter::PaneDirection(value)) => {
            let direction = match proto::PaneDirection::try_from(value) {
                Ok(proto::PaneDirection::Left) => PaneDirection::Left,
                Ok(proto::PaneDirection::Right) => PaneDirection::Right,
                Ok(proto::PaneDirection::Up) => PaneDirection::Up,
                Ok(proto::PaneDirection::Down) => PaneDirection::Down,
                Ok(proto::PaneDirection::Unspecified) | Err(_) => {
                    return Err(PluginRuntimeError::InvalidWireParameter {
                        kind: "pane direction",
                        value: i64::from(value),
                    });
                }
            };
            Some(ActionParameter::PaneDirection(direction))
        }
    };
    let invocation = ActionInvocation::new(&action.id, parameter);
    Action::try_from(invocation).map_err(PluginRuntimeError::Action)
}

#[derive(Debug, Error)]
pub enum PluginRuntimeError {
    #[error("the platform local-data directory is unavailable")]
    LocalDataDirectoryUnavailable,
    #[error(transparent)]
    Catalog(#[from] PluginCatalogError),
    #[error(transparent)]
    Storage(#[from] PluginStorageError),
    #[error(transparent)]
    Package(#[from] PluginPackageError),
    #[error("failed to resolve plugin root `{path}`: {source}")]
    PluginRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("plugin command `{0}` changed after catalog discovery; refresh is required")]
    CatalogChanged(String),
    #[error(transparent)]
    Broker(#[from] PluginBrokerError),
    #[error("plugin returned an invalid {kind} parameter value {value}")]
    InvalidWireParameter { kind: &'static str, value: i64 },
    #[error("plugin action cannot be dispatched: {0}")]
    Action(#[from] ActionInvocationError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use cterm_plugin_api::{ABI_MAJOR, ABI_MINOR, MANIFEST_FILE, MODULE_FILE};
    use cterm_ui::action_ids;
    use std::path::Path;

    fn write_bundle(root: &Path, id: &str) -> PluginBundle {
        fs::create_dir_all(root).unwrap();
        let manifest = format!(
            "manifest_version = 1\nid = \"{id}\"\nname = \"Runtime Test\"\nversion = \"1.0.0\"\nabi = \"1.0\"\n\n[[commands]]\nid = \"run\"\ntitle = \"Run\"\n\n[capabilities.invoke-actions]\nallow = [\"cterm:new-tab\", \"cterm:split-pane\"]\n"
        );
        fs::write(root.join(MANIFEST_FILE), manifest).unwrap();
        fs::write(root.join(MODULE_FILE), b"\0asm\x01\0\0\0").unwrap();
        PluginBundle::load(root).unwrap()
    }

    fn test_runtime() -> (tempfile::TempDir, PluginRuntime, PluginCommandDescriptor) {
        let directory = tempfile::tempdir().unwrap();
        let paths = PluginDataPaths::from_data_root(directory.path().join("data"));
        write_bundle(
            &paths.plugins_root().join("org.example.runtime"),
            "org.example.runtime",
        );
        let runtime = PluginRuntime::load(paths).unwrap();
        let command = runtime.catalog().commands()[0].clone();
        (directory, runtime, command)
    }

    #[test]
    fn authorization_requires_explicit_approval_then_uses_saved_exact_grant() {
        let (_directory, mut runtime, command) = test_runtime();
        let prompt = match runtime.authorize(&command).unwrap() {
            PluginAuthorization::ApprovalRequired(prompt) => prompt,
            PluginAuthorization::Granted(_) => panic!("unapproved plugin was granted"),
        };
        assert_eq!(prompt.command().action_id(), command.action_id());
        assert_eq!(prompt.missing_actions(), command.requested_actions());
        assert!(!prompt.content_changed());

        let invocation = runtime.approve(prompt).unwrap();
        assert_eq!(invocation.plugin_id(), command.plugin_id());
        assert_eq!(invocation.command_id(), command.command_id());
        assert!(matches!(
            runtime.authorize(&command).unwrap(),
            PluginAuthorization::Granted(_)
        ));

        runtime.revoke(command.plugin_id()).unwrap();
        assert!(matches!(
            runtime.authorize(&command).unwrap(),
            PluginAuthorization::ApprovalRequired(_)
        ));
    }

    #[test]
    fn changed_package_cannot_use_a_stale_catalog_descriptor() {
        let (_directory, runtime, command) = test_runtime();
        fs::write(
            command.package_root().join(MODULE_FILE),
            b"\0asm\x01\0\0\0changed",
        )
        .unwrap();
        assert!(matches!(
            runtime.authorize(&command),
            Err(PluginRuntimeError::CatalogChanged(_))
        ));
    }

    #[test]
    fn package_change_between_prompt_and_acceptance_is_not_granted() {
        let (_directory, mut runtime, command) = test_runtime();
        let prompt = match runtime.authorize(&command).unwrap() {
            PluginAuthorization::ApprovalRequired(prompt) => prompt,
            PluginAuthorization::Granted(_) => panic!("unapproved plugin was granted"),
        };
        fs::write(
            command.package_root().join(MODULE_FILE),
            b"\0asm\x01\0\0\0changed-after-prompt",
        )
        .unwrap();

        assert!(matches!(
            runtime.approve(prompt),
            Err(PluginRuntimeError::CatalogChanged(_))
        ));
        assert!(!runtime.paths().grants_file().exists());
    }

    #[test]
    fn refreshed_changed_content_reports_that_prior_approval_is_stale() {
        let (_directory, mut runtime, command) = test_runtime();
        let prompt = match runtime.authorize(&command).unwrap() {
            PluginAuthorization::ApprovalRequired(prompt) => prompt,
            PluginAuthorization::Granted(_) => panic!("unapproved plugin was granted"),
        };
        runtime.approve(prompt).unwrap();
        fs::write(
            command.package_root().join(MODULE_FILE),
            b"\0asm\x01\0\0\0updated",
        )
        .unwrap();
        runtime.refresh().unwrap();
        let changed_command = runtime.catalog().commands()[0].clone();

        let prompt = match runtime.authorize(&changed_command).unwrap() {
            PluginAuthorization::ApprovalRequired(prompt) => prompt,
            PluginAuthorization::Granted(_) => panic!("changed plugin inherited a stale grant"),
        };
        assert!(prompt.content_changed());
        assert_eq!(
            prompt.missing_actions(),
            changed_command.requested_actions()
        );
    }

    #[test]
    fn wire_actions_become_typed_native_actions() {
        let response = proto::PluginResponse {
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            actions: vec![
                proto::ActionInvocation {
                    id: action_ids::NEW_TAB.to_string(),
                    parameter: None,
                },
                proto::ActionInvocation {
                    id: action_ids::SPLIT_PANE.to_string(),
                    parameter: Some(proto::action_invocation::Parameter::SplitDirection(
                        proto::SplitDirection::Vertical.into(),
                    )),
                },
            ],
            diagnostics: vec!["done".to_string()],
        };
        let actions = response
            .actions
            .iter()
            .map(convert_action)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            actions,
            [Action::NewTab, Action::SplitPane(SplitDirection::Vertical)]
        );
    }

    #[test]
    fn semantic_action_parameter_mismatches_fail_before_native_dispatch() {
        let action = proto::ActionInvocation {
            id: action_ids::NEW_TAB.to_string(),
            parameter: Some(proto::action_invocation::Parameter::Tab(4)),
        };
        assert!(matches!(
            convert_action(&action),
            Err(PluginRuntimeError::Action(
                ActionInvocationError::UnexpectedParameter { .. }
            ))
        ));
    }
}
