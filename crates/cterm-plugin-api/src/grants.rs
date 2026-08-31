use std::collections::{BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ActionScope, BundleDigest, PluginBundle, PluginId, PluginPackageError};

const GRANT_STORE_VERSION: u32 = 1;

/// Result of matching a verified bundle against the machine-local grant store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantDecision {
    Granted,
    ApprovalRequired {
        missing: BTreeSet<ActionScope>,
        /// True when this plugin ID was approved before, but its exact bytes changed.
        content_changed: bool,
    },
}

#[derive(Debug, Clone, Default)]
pub struct GrantStore {
    records: Vec<GrantRecord>,
}

#[derive(Debug, Clone)]
struct GrantRecord {
    plugin: PluginId,
    digest: BundleDigest,
    invoke_actions: BTreeSet<ActionScope>,
}

impl GrantStore {
    pub fn from_toml(text: &str) -> Result<Self, GrantError> {
        let raw: RawGrantStore = toml::from_str(text).map_err(GrantError::Parse)?;
        if raw.version != GRANT_STORE_VERSION {
            return Err(GrantError::UnsupportedVersion(raw.version));
        }

        let mut keys = HashSet::new();
        let mut records = Vec::with_capacity(raw.grants.len());
        for raw_record in raw.grants {
            let digest = raw_record
                .digest
                .parse::<BundleDigest>()
                .map_err(GrantError::InvalidDigest)?;
            let key = (raw_record.plugin.clone(), digest);
            if !keys.insert(key) {
                return Err(GrantError::DuplicateRecord {
                    plugin: raw_record.plugin,
                    digest,
                });
            }

            let mut invoke_actions = BTreeSet::new();
            for scope in raw_record.invoke_actions {
                if !invoke_actions.insert(scope.clone()) {
                    return Err(GrantError::DuplicateActionScope(scope));
                }
            }
            records.push(GrantRecord {
                plugin: raw_record.plugin,
                digest,
                invoke_actions,
            });
        }

        Ok(Self { records })
    }

    pub fn to_toml(&self) -> Result<String, GrantError> {
        let mut records = self.records.clone();
        records.sort_by(|left, right| {
            left.plugin
                .cmp(&right.plugin)
                .then_with(|| left.digest.cmp(&right.digest))
        });
        let raw = RawGrantStore {
            version: GRANT_STORE_VERSION,
            grants: records
                .into_iter()
                .map(|record| RawGrantRecord {
                    plugin: record.plugin,
                    digest: record.digest.to_string(),
                    invoke_actions: record.invoke_actions.into_iter().collect(),
                })
                .collect(),
        };
        toml::to_string_pretty(&raw).map_err(GrantError::Serialize)
    }

    pub fn decision(&self, bundle: &PluginBundle) -> GrantDecision {
        let requested = bundle.manifest().invoke_actions();
        if requested.is_empty() {
            return GrantDecision::Granted;
        }

        let exact = self.records.iter().find(|record| {
            record.plugin == *bundle.manifest().id() && record.digest == bundle.digest()
        });
        if let Some(record) = exact {
            let missing = requested
                .difference(&record.invoke_actions)
                .cloned()
                .collect::<BTreeSet<_>>();
            if missing.is_empty() {
                GrantDecision::Granted
            } else {
                GrantDecision::ApprovalRequired {
                    missing,
                    content_changed: false,
                }
            }
        } else {
            GrantDecision::ApprovalRequired {
                missing: requested.clone(),
                content_changed: self
                    .records
                    .iter()
                    .any(|record| record.plugin == *bundle.manifest().id()),
            }
        }
    }

    /// Replace all earlier grants for this plugin ID with scopes for this exact
    /// verified bundle. A subset supports least-privilege approval.
    pub fn approve(
        &mut self,
        bundle: &PluginBundle,
        invoke_actions: BTreeSet<ActionScope>,
    ) -> Result<(), GrantError> {
        if let Some(scope) = invoke_actions
            .difference(bundle.manifest().invoke_actions())
            .next()
        {
            return Err(GrantError::ActionNotRequested(scope.clone()));
        }

        self.records
            .retain(|record| record.plugin != *bundle.manifest().id());
        self.records.push(GrantRecord {
            plugin: bundle.manifest().id().clone(),
            digest: bundle.digest(),
            invoke_actions,
        });
        Ok(())
    }

    pub fn revoke(&mut self, plugin: &PluginId) {
        self.records.retain(|record| &record.plugin != plugin);
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGrantStore {
    version: u32,
    #[serde(default)]
    grants: Vec<RawGrantRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGrantRecord {
    plugin: PluginId,
    digest: String,
    #[serde(default, rename = "invoke-actions")]
    invoke_actions: Vec<ActionScope>,
}

#[derive(Debug, Error)]
pub enum GrantError {
    #[error("invalid plugin grant store: {0}")]
    Parse(#[source] toml::de::Error),
    #[error("failed to serialize plugin grant store: {0}")]
    Serialize(#[source] toml::ser::Error),
    #[error("plugin grant store version {0} is unsupported")]
    UnsupportedVersion(u32),
    #[error("invalid digest in plugin grant store: {0}")]
    InvalidDigest(#[source] PluginPackageError),
    #[error("duplicate grant for `{plugin}` at `{digest}`")]
    DuplicateRecord {
        plugin: PluginId,
        digest: BundleDigest,
    },
    #[error("duplicate granted action scope `{0}`")]
    DuplicateActionScope(ActionScope),
    #[error("cannot grant action `{0}` because the plugin did not request it")]
    ActionNotRequested(ActionScope),
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{MANIFEST_FILE, MODULE_FILE};

    const MANIFEST: &str = r#"
manifest_version = 1
id = "org.example.tools"
name = "Tools"
version = "1.0.0"
abi = "1.0"

[[commands]]
id = "run"
title = "Run Tool"

[capabilities.invoke-actions]
allow = ["cterm:new-tab", "cterm:split-pane"]
"#;

    fn bundle() -> (tempfile::TempDir, PluginBundle) {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(MANIFEST_FILE), MANIFEST).unwrap();
        fs::write(directory.path().join(MODULE_FILE), b"\0asm\x01\0\0\0").unwrap();
        let bundle = PluginBundle::load(directory.path()).unwrap();
        (directory, bundle)
    }

    fn scope(value: &str) -> ActionScope {
        ActionScope::parse(value).unwrap()
    }

    #[test]
    fn approval_is_exact_digest_and_least_privilege() {
        let (_directory, bundle) = bundle();
        let mut grants = GrantStore::default();
        assert_eq!(
            grants.decision(&bundle),
            GrantDecision::ApprovalRequired {
                missing: bundle.manifest().invoke_actions().clone(),
                content_changed: false,
            }
        );

        grants
            .approve(&bundle, BTreeSet::from([scope("cterm:new-tab")]))
            .unwrap();
        assert_eq!(
            grants.decision(&bundle),
            GrantDecision::ApprovalRequired {
                missing: BTreeSet::from([scope("cterm:split-pane")]),
                content_changed: false,
            }
        );

        grants
            .approve(&bundle, bundle.manifest().invoke_actions().clone())
            .unwrap();
        assert_eq!(grants.decision(&bundle), GrantDecision::Granted);
    }

    #[test]
    fn changed_code_never_inherits_old_grants() {
        let (directory, initial) = bundle();
        let mut grants = GrantStore::default();
        grants
            .approve(&initial, initial.manifest().invoke_actions().clone())
            .unwrap();

        fs::write(
            directory.path().join(MODULE_FILE),
            b"\0asm\x01\0\0\0changed",
        )
        .unwrap();
        let changed = PluginBundle::load(directory.path()).unwrap();
        assert_eq!(
            grants.decision(&changed),
            GrantDecision::ApprovalRequired {
                missing: changed.manifest().invoke_actions().clone(),
                content_changed: true,
            }
        );
    }

    #[test]
    fn grant_store_round_trips_deterministically() {
        let (_directory, bundle) = bundle();
        let mut grants = GrantStore::default();
        grants
            .approve(&bundle, bundle.manifest().invoke_actions().clone())
            .unwrap();

        let encoded = grants.to_toml().unwrap();
        let decoded = GrantStore::from_toml(&encoded).unwrap();
        assert_eq!(decoded.decision(&bundle), GrantDecision::Granted);
        assert_eq!(decoded.to_toml().unwrap(), encoded);
    }

    #[test]
    fn malformed_ambiguous_or_unknown_grants_fail_closed() {
        let (_directory, bundle) = bundle();
        let digest = bundle.digest();
        let cases = [
            "version = 2\n".to_string(),
            "version = 1\nunknown = true\n".to_string(),
            format!(
                "version = 1\n[[grants]]\nplugin = \"org.example.tools\"\ndigest = \"{digest}\"\ninvoke-actions = [\"cterm:new-tab\", \"cterm:new-tab\"]\n"
            ),
            format!(
                "version = 1\n[[grants]]\nplugin = \"org.example.tools\"\ndigest = \"{digest}\"\n[[grants]]\nplugin = \"org.example.tools\"\ndigest = \"{digest}\"\n"
            ),
            format!(
                "version = 1\n[[grants]]\nplugin = \"org.example.tools\"\ndigest = \"{digest}\"\ninvoke-actions = [\"*\"]\n"
            ),
        ];

        for text in cases {
            assert!(GrantStore::from_toml(&text).is_err(), "accepted {text}");
        }
    }

    #[test]
    fn host_cannot_grant_an_undeclared_action() {
        let (_directory, bundle) = bundle();
        let mut grants = GrantStore::default();
        assert!(matches!(
            grants.approve(&bundle, BTreeSet::from([scope("cterm:close-window")])),
            Err(GrantError::ActionNotRequested(action))
                if action == scope("cterm:close-window")
        ));
    }
}
