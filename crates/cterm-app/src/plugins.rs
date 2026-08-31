//! Cross-platform discovery and machine-local trust storage for command plugins.
//!
//! Packages and grants deliberately live below the platform local-data root,
//! outside cterm's Git-synchronized configuration directory. Discovery is
//! deterministic and isolates malformed packages so one bad directory cannot
//! hide otherwise valid commands.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use cterm_plugin_api::{
    ActionScope, BundleDigest, CommandId, GrantError, GrantStore, PluginBundle, PluginId,
    PluginPackageError,
};
use directories::ProjectDirs;
use tempfile::NamedTempFile;
use thiserror::Error;

const PLUGINS_DIRECTORY: &str = "plugins";
const GRANTS_FILE: &str = "plugin-grants.toml";
const MAX_GRANT_STORE_BYTES: usize = 1024 * 1024;

/// Platform-local paths used by the released plugin runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDataPaths {
    data_root: PathBuf,
    plugins_root: PathBuf,
    grants_file: PathBuf,
}

impl PluginDataPaths {
    /// Resolve the current user's native local-data directory.
    pub fn for_current_user() -> Option<Self> {
        ProjectDirs::from("com", "cterm", "cterm")
            .map(|directories| Self::from_data_root(directories.data_local_dir()))
    }

    /// Build paths below an explicit root, primarily for isolated products and tests.
    pub fn from_data_root(data_root: impl Into<PathBuf>) -> Self {
        let data_root = data_root.into();
        Self {
            plugins_root: data_root.join(PLUGINS_DIRECTORY),
            grants_file: data_root.join(GRANTS_FILE),
            data_root,
        }
    }

    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    pub fn plugins_root(&self) -> &Path {
        &self.plugins_root
    }

    pub fn grants_file(&self) -> &Path {
        &self.grants_file
    }
}

/// One verified command shown by native command discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommandDescriptor {
    package_root: PathBuf,
    plugin_id: PluginId,
    plugin_name: String,
    plugin_version: String,
    command_id: CommandId,
    command_title: String,
    action_id: String,
    digest: BundleDigest,
    requested_actions: BTreeSet<ActionScope>,
}

impl PluginCommandDescriptor {
    /// Canonical package root that was verified during discovery.
    pub fn package_root(&self) -> &Path {
        &self.package_root
    }

    pub fn plugin_id(&self) -> &PluginId {
        &self.plugin_id
    }

    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    pub fn plugin_version(&self) -> &str {
        &self.plugin_version
    }

    pub fn command_id(&self) -> &CommandId {
        &self.command_id
    }

    pub fn command_title(&self) -> &str {
        &self.command_title
    }

    pub fn action_id(&self) -> &str {
        &self.action_id
    }

    pub const fn digest(&self) -> BundleDigest {
        self.digest
    }

    pub fn requested_actions(&self) -> &BTreeSet<ActionScope> {
        &self.requested_actions
    }
}

/// A malformed package isolated during catalog discovery.
#[derive(Debug)]
pub struct PluginDiscoveryFailure {
    path: PathBuf,
    error: PluginDiscoveryError,
}

impl PluginDiscoveryFailure {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn error(&self) -> &PluginDiscoveryError {
        &self.error
    }
}

#[derive(Debug, Error)]
pub enum PluginDiscoveryError {
    #[error("plugin package directories may not be symbolic links")]
    SymlinkPackage,
    #[error(transparent)]
    InvalidPackage(#[from] PluginPackageError),
    #[error("plugin package resolves outside the configured plugin root")]
    OutsidePluginRoot,
    #[error("plugin directory `{directory}` does not match manifest ID `{manifest}`")]
    DirectoryIdMismatch {
        directory: String,
        manifest: PluginId,
    },
}

/// Deterministically ordered valid commands plus isolated package failures.
#[derive(Debug, Default)]
pub struct PluginCatalog {
    commands: Vec<PluginCommandDescriptor>,
    failures: Vec<PluginDiscoveryFailure>,
}

impl PluginCatalog {
    pub fn discover(plugins_root: impl AsRef<Path>) -> Result<Self, PluginCatalogError> {
        let supplied_root = plugins_root.as_ref();
        let canonical_root = match fs::canonicalize(supplied_root) {
            Ok(root) => root,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(PluginCatalogError::ReadRoot {
                    path: supplied_root.to_path_buf(),
                    source,
                });
            }
        };
        if !canonical_root.is_dir() {
            return Err(PluginCatalogError::NotDirectory(canonical_root));
        }

        let entries =
            fs::read_dir(&canonical_root).map_err(|source| PluginCatalogError::ReadRoot {
                path: canonical_root.clone(),
                source,
            })?;
        let mut candidates = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| PluginCatalogError::ReadEntry {
                path: canonical_root.clone(),
                source,
            })?;
            candidates.push(entry.path());
        }
        candidates.sort();

        let mut catalog = Self::default();
        for path in candidates {
            let metadata =
                fs::symlink_metadata(&path).map_err(|source| PluginCatalogError::ReadEntry {
                    path: path.clone(),
                    source,
                })?;
            if metadata.file_type().is_symlink() {
                catalog.failures.push(PluginDiscoveryFailure {
                    path,
                    error: PluginDiscoveryError::SymlinkPackage,
                });
                continue;
            }
            if !metadata.is_dir() {
                continue;
            }

            match discover_package(&canonical_root, &path) {
                Ok(mut commands) => catalog.commands.append(&mut commands),
                Err(error) => catalog
                    .failures
                    .push(PluginDiscoveryFailure { path, error }),
            }
        }
        catalog.commands.sort_by(|left, right| {
            left.plugin_id
                .cmp(&right.plugin_id)
                .then_with(|| left.command_id.cmp(&right.command_id))
        });
        catalog
            .failures
            .sort_by(|left, right| left.path.cmp(&right.path));
        Ok(catalog)
    }

    pub fn commands(&self) -> &[PluginCommandDescriptor] {
        &self.commands
    }

    pub fn failures(&self) -> &[PluginDiscoveryFailure] {
        &self.failures
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }
}

fn discover_package(
    canonical_root: &Path,
    path: &Path,
) -> Result<Vec<PluginCommandDescriptor>, PluginDiscoveryError> {
    let bundle = PluginBundle::load(path)?;
    if bundle.root().parent() != Some(canonical_root) {
        return Err(PluginDiscoveryError::OutsidePluginRoot);
    }
    let directory = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if directory != bundle.manifest().id().as_str() {
        return Err(PluginDiscoveryError::DirectoryIdMismatch {
            directory: directory.to_string(),
            manifest: bundle.manifest().id().clone(),
        });
    }

    Ok(bundle
        .manifest()
        .commands()
        .iter()
        .map(|command| PluginCommandDescriptor {
            package_root: bundle.root().to_path_buf(),
            plugin_id: bundle.manifest().id().clone(),
            plugin_name: bundle.manifest().name().to_string(),
            plugin_version: bundle.manifest().version().to_string(),
            command_id: command.id().clone(),
            command_title: command.title().to_string(),
            action_id: command.action_id(bundle.manifest().id()),
            digest: bundle.digest(),
            requested_actions: bundle.manifest().invoke_actions().clone(),
        })
        .collect())
}

#[derive(Debug, Error)]
pub enum PluginCatalogError {
    #[error("failed to read plugin root `{path}`: {source}")]
    ReadRoot {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("plugin root `{0}` is not a directory")]
    NotDirectory(PathBuf),
    #[error("failed to read plugin entry below `{path}`: {source}")]
    ReadEntry {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Bounded, atomically replaced machine-local grant storage.
#[derive(Debug, Clone)]
pub struct PluginGrantFile {
    path: PathBuf,
}

impl PluginGrantFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<GrantStore, PluginStorageError> {
        let bytes = match read_bounded_file(&self.path, MAX_GRANT_STORE_BYTES) {
            Ok(bytes) => bytes,
            Err(PluginStorageError::Io { source, .. })
                if source.kind() == io::ErrorKind::NotFound =>
            {
                return Ok(GrantStore::default());
            }
            Err(error) => return Err(error),
        };
        let text = std::str::from_utf8(&bytes).map_err(PluginStorageError::Encoding)?;
        GrantStore::from_toml(text).map_err(PluginStorageError::Grant)
    }

    pub fn save(&self, grants: &GrantStore) -> Result<(), PluginStorageError> {
        let text = grants.to_toml().map_err(PluginStorageError::Grant)?;
        if text.len() > MAX_GRANT_STORE_BYTES {
            return Err(PluginStorageError::TooLarge {
                path: self.path.clone(),
                limit: MAX_GRANT_STORE_BYTES,
            });
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| PluginStorageError::MissingParent(self.path.clone()))?;
        fs::create_dir_all(parent).map_err(|source| PluginStorageError::Io {
            path: parent.to_path_buf(),
            source,
        })?;

        let mut temporary =
            NamedTempFile::new_in(parent).map_err(|source| PluginStorageError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        temporary
            .write_all(text.as_bytes())
            .and_then(|()| temporary.as_file_mut().flush())
            .map_err(|source| PluginStorageError::Io {
                path: temporary.path().to_path_buf(),
                source,
            })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|source| PluginStorageError::Io {
                    path: temporary.path().to_path_buf(),
                    source,
                })?;
        }
        temporary
            .as_file()
            .sync_all()
            .map_err(|source| PluginStorageError::Io {
                path: temporary.path().to_path_buf(),
                source,
            })?;
        temporary
            .persist(&self.path)
            .map_err(|error| PluginStorageError::Io {
                path: self.path.clone(),
                source: error.error,
            })?;

        #[cfg(unix)]
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| PluginStorageError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        Ok(())
    }

    /// Persist an exact bundle approval without exposing a mutated in-memory
    /// store if the durable replacement fails.
    pub fn approve_and_save(
        &self,
        grants: &mut GrantStore,
        bundle: &PluginBundle,
        actions: BTreeSet<ActionScope>,
    ) -> Result<(), PluginStorageError> {
        let mut updated = grants.clone();
        updated
            .approve(bundle, actions)
            .map_err(PluginStorageError::Grant)?;
        self.save(&updated)?;
        *grants = updated;
        Ok(())
    }

    pub fn revoke_and_save(
        &self,
        grants: &mut GrantStore,
        plugin: &PluginId,
    ) -> Result<(), PluginStorageError> {
        let mut updated = grants.clone();
        updated.revoke(plugin);
        self.save(&updated)?;
        *grants = updated;
        Ok(())
    }
}

fn read_bounded_file(path: &Path, limit: usize) -> Result<Vec<u8>, PluginStorageError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| PluginStorageError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PluginStorageError::NotRegularFile(path.to_path_buf()));
    }
    let limit_u64 = u64::try_from(limit).unwrap_or(u64::MAX);
    if metadata.len() > limit_u64 {
        return Err(PluginStorageError::TooLarge {
            path: path.to_path_buf(),
            limit,
        });
    }

    let mut file = File::open(path).map_err(|source| PluginStorageError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let opened = file.metadata().map_err(|source| PluginStorageError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !opened.is_file() || opened.len() != metadata.len() {
        return Err(PluginStorageError::ChangedDuringRead(path.to_path_buf()));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(opened.len()).unwrap_or(limit).min(limit));
    Read::by_ref(&mut file)
        .take(limit_u64.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| PluginStorageError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > limit {
        return Err(PluginStorageError::TooLarge {
            path: path.to_path_buf(),
            limit,
        });
    }
    let final_length = file
        .metadata()
        .map_err(|source| PluginStorageError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if final_length != opened.len()
        || u64::try_from(bytes.len()).unwrap_or(u64::MAX) != final_length
    {
        return Err(PluginStorageError::ChangedDuringRead(path.to_path_buf()));
    }
    Ok(bytes)
}

#[derive(Debug, Error)]
pub enum PluginStorageError {
    #[error("plugin grant path `{0}` has no parent directory")]
    MissingParent(PathBuf),
    #[error("plugin grant path `{0}` is not a regular file")]
    NotRegularFile(PathBuf),
    #[error("plugin grant file `{path}` exceeds its {limit}-byte limit")]
    TooLarge { path: PathBuf, limit: usize },
    #[error("plugin grant file `{0}` changed while it was being read")]
    ChangedDuringRead(PathBuf),
    #[error("plugin grant file is not UTF-8: {0}")]
    Encoding(#[source] std::str::Utf8Error),
    #[error(transparent)]
    Grant(#[from] GrantError),
    #[error("failed to access plugin grant path `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use cterm_plugin_api::{GrantDecision, MANIFEST_FILE, MODULE_FILE};

    fn write_bundle(root: &Path, id: &str, commands: &[(&str, &str)]) -> PluginBundle {
        fs::create_dir_all(root).unwrap();
        let command_toml = commands
            .iter()
            .map(|(id, title)| format!("[[commands]]\nid = \"{id}\"\ntitle = \"{title}\"\n"))
            .collect::<String>();
        let manifest = format!(
            "manifest_version = 1\nid = \"{id}\"\nname = \"Plugin {id}\"\nversion = \"1.2.3\"\nabi = \"1.0\"\n\n{command_toml}\n[capabilities.invoke-actions]\nallow = [\"cterm:new-tab\"]\n"
        );
        fs::write(root.join(MANIFEST_FILE), manifest).unwrap();
        fs::write(root.join(MODULE_FILE), b"\0asm\x01\0\0\0").unwrap();
        PluginBundle::load(root).unwrap()
    }

    #[test]
    fn data_paths_keep_plugins_and_grants_together_in_local_data() {
        let paths = PluginDataPaths::from_data_root("local-data/cterm");
        assert_eq!(paths.plugins_root(), Path::new("local-data/cterm/plugins"));
        assert_eq!(
            paths.grants_file(),
            Path::new("local-data/cterm/plugin-grants.toml")
        );
    }

    #[test]
    fn missing_plugin_root_is_an_empty_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = PluginCatalog::discover(directory.path().join("missing")).unwrap();
        assert!(catalog.is_empty());
        assert!(catalog.failures().is_empty());
    }

    #[test]
    fn discovery_is_sorted_and_one_bad_package_does_not_hide_valid_commands() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("plugins");
        fs::create_dir(&root).unwrap();
        write_bundle(
            &root.join("org.example.zed"),
            "org.example.zed",
            &[("run", "Run")],
        );
        write_bundle(
            &root.join("org.example.alpha"),
            "org.example.alpha",
            &[("z-last", "Last"), ("a-first", "First")],
        );
        write_bundle(
            &root.join("org.example.wrong-directory"),
            "org.example.other",
            &[("run", "Run")],
        );
        fs::write(root.join("README.txt"), "ignored").unwrap();

        let canonical_root = root.canonicalize().unwrap();
        let catalog = PluginCatalog::discover(&root).unwrap();
        let actions = catalog
            .commands()
            .iter()
            .map(PluginCommandDescriptor::action_id)
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            [
                "plugin:org.example.alpha/a-first",
                "plugin:org.example.alpha/z-last",
                "plugin:org.example.zed/run",
            ]
        );
        assert_eq!(catalog.failures().len(), 1);
        assert!(catalog.commands().iter().all(|command| {
            command.package_root().is_absolute()
                && command.package_root().parent() == Some(canonical_root.as_path())
        }));
        assert!(matches!(
            catalog.failures()[0].error(),
            PluginDiscoveryError::DirectoryIdMismatch { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn discovery_rejects_symlinked_package_directories() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("plugins");
        let external = directory.path().join("external");
        fs::create_dir(&root).unwrap();
        write_bundle(&external, "org.example.link", &[("run", "Run")]);
        std::os::unix::fs::symlink(&external, root.join("org.example.link")).unwrap();

        let catalog = PluginCatalog::discover(&root).unwrap();
        assert!(catalog.is_empty());
        assert!(matches!(
            catalog.failures()[0].error(),
            PluginDiscoveryError::SymlinkPackage
        ));
    }

    #[test]
    fn grants_round_trip_through_an_atomic_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let bundle = write_bundle(
            &directory.path().join("package"),
            "org.example.grants",
            &[("run", "Run")],
        );
        let file = PluginGrantFile::new(directory.path().join(GRANTS_FILE));
        let mut grants = GrantStore::default();
        file.approve_and_save(
            &mut grants,
            &bundle,
            bundle.manifest().invoke_actions().clone(),
        )
        .unwrap();

        let loaded = file.load().unwrap();
        assert_eq!(loaded.decision(&bundle), GrantDecision::Granted);
        let entries = fs::read_dir(directory.path()).unwrap().count();
        assert_eq!(entries, 2, "the package and final grant file must remain");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(file.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn failed_approval_does_not_mutate_the_live_store() {
        let directory = tempfile::tempdir().unwrap();
        let bundle = write_bundle(
            &directory.path().join("package"),
            "org.example.grants",
            &[("run", "Run")],
        );
        let file = PluginGrantFile::new(directory.path().join(GRANTS_FILE));
        let mut grants = GrantStore::default();
        let undeclared = ActionScope::parse("cterm:close-window").unwrap();

        assert!(matches!(
            file.approve_and_save(&mut grants, &bundle, BTreeSet::from([undeclared])),
            Err(PluginStorageError::Grant(GrantError::ActionNotRequested(_)))
        ));
        assert!(!file.path().exists());
        assert!(matches!(
            grants.decision(&bundle),
            GrantDecision::ApprovalRequired { .. }
        ));
    }

    #[test]
    fn oversized_and_symlinked_grant_files_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(GRANTS_FILE);
        fs::write(&path, vec![b'x'; MAX_GRANT_STORE_BYTES + 1]).unwrap();
        assert!(matches!(
            PluginGrantFile::new(&path).load(),
            Err(PluginStorageError::TooLarge { .. })
        ));

        #[cfg(unix)]
        {
            fs::remove_file(&path).unwrap();
            let external = directory.path().join("external");
            fs::write(&external, "version = 1\n").unwrap();
            std::os::unix::fs::symlink(&external, &path).unwrap();
            assert!(matches!(
                PluginGrantFile::new(&path).load(),
                Err(PluginStorageError::NotRegularFile(_))
            ));
        }
    }
}
