use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use semver::Version;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const MANIFEST_VERSION: u32 = 1;
pub const ABI_MAJOR: u32 = 1;
pub const ABI_MINOR: u32 = 0;
pub const MANIFEST_FILE: &str = "cterm-plugin.toml";
pub const MODULE_FILE: &str = "plugin.wasm";

const DIGEST_DOMAIN: &[u8] = b"cterm-plugin-package-v1\0";
const WASM_HEADER: &[u8] = b"\0asm\x01\0\0\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundleLimits {
    pub manifest_bytes: usize,
    pub module_bytes: usize,
}

impl Default for BundleLimits {
    fn default() -> Self {
        Self {
            manifest_bytes: 64 * 1024,
            module_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginId(String);

impl PluginId {
    pub fn parse(value: impl Into<String>) -> Result<Self, PluginPackageError> {
        let value = value.into();
        if !valid_dotted_id(&value) {
            return Err(PluginPackageError::InvalidPluginId(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PluginId {
    type Err = PluginPackageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl Serialize for PluginId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PluginId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandId(String);

impl CommandId {
    pub fn parse(value: impl Into<String>) -> Result<Self, PluginPackageError> {
        let value = value.into();
        if !valid_segment(&value, 64) {
            return Err(PluginPackageError::InvalidCommandId(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CommandId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActionScope(String);

impl ActionScope {
    pub fn parse(value: impl Into<String>) -> Result<Self, PluginPackageError> {
        let value = value.into();
        let Some(slug) = value.strip_prefix("cterm:") else {
            return Err(PluginPackageError::InvalidActionScope(value));
        };
        if !valid_segment(slug, 96) {
            return Err(PluginPackageError::InvalidActionScope(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ActionScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for ActionScope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ActionScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommand {
    id: CommandId,
    title: String,
}

impl PluginCommand {
    pub fn id(&self) -> &CommandId {
        &self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn action_id(&self, plugin_id: &PluginId) -> String {
        format!("plugin:{plugin_id}/{}", self.id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    id: PluginId,
    name: String,
    version: Version,
    invoke_actions: BTreeSet<ActionScope>,
    commands: Vec<PluginCommand>,
}

impl PluginManifest {
    pub fn id(&self) -> &PluginId {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> &Version {
        &self.version
    }

    pub fn invoke_actions(&self) -> &BTreeSet<ActionScope> {
        &self.invoke_actions
    }

    pub fn commands(&self) -> &[PluginCommand] {
        &self.commands
    }

    pub fn command(&self, id: &CommandId) -> Option<&PluginCommand> {
        self.commands.iter().find(|command| command.id() == id)
    }

    fn parse(bytes: &[u8]) -> Result<Self, PluginPackageError> {
        let text = std::str::from_utf8(bytes).map_err(PluginPackageError::ManifestEncoding)?;
        let raw: RawManifest = toml::from_str(text).map_err(PluginPackageError::ManifestParse)?;

        if raw.manifest_version != MANIFEST_VERSION {
            return Err(PluginPackageError::UnsupportedManifestVersion {
                found: raw.manifest_version,
                supported: MANIFEST_VERSION,
            });
        }
        if raw.abi != format!("{ABI_MAJOR}.{ABI_MINOR}") {
            return Err(PluginPackageError::UnsupportedAbi {
                found: raw.abi,
                supported: format!("{ABI_MAJOR}.{ABI_MINOR}"),
            });
        }

        let id = PluginId::parse(raw.id)?;
        validate_label("plugin name", &raw.name, 128)?;
        let version = Version::parse(&raw.version).map_err(|source| {
            PluginPackageError::InvalidPluginVersion {
                version: raw.version,
                source,
            }
        })?;

        let mut invoke_actions = BTreeSet::new();
        if let Some(capability) = raw.capabilities.invoke_actions {
            for raw_scope in capability.allow {
                let scope = ActionScope::parse(raw_scope)?;
                if !invoke_actions.insert(scope.clone()) {
                    return Err(PluginPackageError::DuplicateActionScope(scope));
                }
            }
            if invoke_actions.is_empty() {
                return Err(PluginPackageError::EmptyActionCapability);
            }
        }

        let mut command_ids = HashSet::new();
        let mut commands = Vec::with_capacity(raw.commands.len());
        for raw_command in raw.commands {
            let command_id = CommandId::parse(raw_command.id)?;
            if !command_ids.insert(command_id.clone()) {
                return Err(PluginPackageError::DuplicateCommand(command_id));
            }
            validate_label("command title", &raw_command.title, 128)?;
            commands.push(PluginCommand {
                id: command_id,
                title: raw_command.title,
            });
        }
        if commands.is_empty() {
            return Err(PluginPackageError::NoCommands);
        }

        Ok(Self {
            id,
            name: raw.name,
            version,
            invoke_actions,
            commands,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    manifest_version: u32,
    id: String,
    name: String,
    version: String,
    abi: String,
    #[serde(default)]
    capabilities: RawCapabilities,
    #[serde(default)]
    commands: Vec<RawCommand>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCapabilities {
    #[serde(rename = "invoke-actions")]
    invoke_actions: Option<RawInvokeActions>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInvokeActions {
    #[serde(default)]
    allow: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    id: String,
    title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BundleDigest([u8; 32]);

impl BundleDigest {
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for BundleDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("sha256:")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for BundleDigest {
    type Err = PluginPackageError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some(hex) = value.strip_prefix("sha256:") else {
            return Err(PluginPackageError::InvalidDigest(value.to_string()));
        };
        if hex.len() != 64
            || hex
                .bytes()
                .any(|byte| !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte))
        {
            return Err(PluginPackageError::InvalidDigest(value.to_string()));
        }

        let mut digest = [0u8; 32];
        for (index, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
                .map_err(|_| PluginPackageError::InvalidDigest(value.to_string()))?;
        }
        Ok(Self(digest))
    }
}

/// A verified package held in memory so its module cannot change between
/// authorization and runner launch.
#[derive(Debug, Clone)]
pub struct PluginBundle {
    root: PathBuf,
    manifest: PluginManifest,
    module: Arc<[u8]>,
    digest: BundleDigest,
}

impl PluginBundle {
    pub fn load(root: impl AsRef<Path>) -> Result<Self, PluginPackageError> {
        Self::load_with_limits(root, BundleLimits::default())
    }

    pub fn load_with_limits(
        root: impl AsRef<Path>,
        limits: BundleLimits,
    ) -> Result<Self, PluginPackageError> {
        let supplied_root = root.as_ref();
        let root = canonicalize(supplied_root)?;
        if !root.is_dir() {
            return Err(PluginPackageError::NotDirectory(root));
        }

        let manifest_path = checked_file(&root, &root.join(MANIFEST_FILE))?;
        let manifest_bytes = read_limited(&manifest_path, limits.manifest_bytes, "manifest")?;
        let manifest = PluginManifest::parse(&manifest_bytes)?;

        let module_path = checked_file(&root, &root.join(MODULE_FILE))?;
        let module = read_limited(&module_path, limits.module_bytes, "WebAssembly module")?;
        if !module.starts_with(WASM_HEADER) {
            return Err(PluginPackageError::InvalidWasmModule);
        }

        let digest = bundle_digest(&manifest_bytes, &module);
        Ok(Self {
            root,
            manifest,
            module: module.into(),
            digest,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    pub fn module(&self) -> &[u8] {
        &self.module
    }

    pub const fn digest(&self) -> BundleDigest {
        self.digest
    }
}

#[derive(Debug, Error)]
pub enum PluginPackageError {
    #[error("failed to access plugin path `{path}`: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("plugin root `{0}` is not a directory")]
    NotDirectory(PathBuf),
    #[error("plugin path `{0}` escapes its package root")]
    PathEscapesBundle(PathBuf),
    #[error("plugin {kind} exceeds its {limit}-byte limit")]
    FileTooLarge { kind: &'static str, limit: usize },
    #[error("plugin file `{0}` changed length while it was being read")]
    FileChangedDuringRead(PathBuf),
    #[error("plugin manifest is not UTF-8: {0}")]
    ManifestEncoding(#[source] std::str::Utf8Error),
    #[error("invalid plugin manifest: {0}")]
    ManifestParse(#[source] toml::de::Error),
    #[error("plugin manifest version {found} is unsupported; this build supports {supported}")]
    UnsupportedManifestVersion { found: u32, supported: u32 },
    #[error("plugin ABI `{found}` is unsupported; this build supports `{supported}`")]
    UnsupportedAbi { found: String, supported: String },
    #[error("invalid plugin identifier `{0}`")]
    InvalidPluginId(String),
    #[error("invalid plugin version `{version}`: {source}")]
    InvalidPluginVersion {
        version: String,
        #[source]
        source: semver::Error,
    },
    #[error("invalid plugin command identifier `{0}`")]
    InvalidCommandId(String),
    #[error("duplicate plugin command `{0}`")]
    DuplicateCommand(CommandId),
    #[error("invalid action capability scope `{0}`")]
    InvalidActionScope(String),
    #[error("duplicate action capability scope `{0}`")]
    DuplicateActionScope(ActionScope),
    #[error("invoke-actions capability must allow at least one exact action")]
    EmptyActionCapability,
    #[error("plugin manifest must declare at least one command")]
    NoCommands,
    #[error("{kind} must be non-empty, printable, and no longer than {limit} characters")]
    InvalidLabel { kind: &'static str, limit: usize },
    #[error("plugin WebAssembly module has an invalid header")]
    InvalidWasmModule,
    #[error("invalid bundle digest `{0}`")]
    InvalidDigest(String),
}

fn canonicalize(path: &Path) -> Result<PathBuf, PluginPackageError> {
    path.canonicalize()
        .map_err(|source| PluginPackageError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn checked_file(root: &Path, path: &Path) -> Result<PathBuf, PluginPackageError> {
    let canonical = canonicalize(path)?;
    if !canonical.starts_with(root) {
        return Err(PluginPackageError::PathEscapesBundle(path.to_path_buf()));
    }
    if !canonical.is_file() {
        return Err(PluginPackageError::Io {
            path: canonical,
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"),
        });
    }
    Ok(canonical)
}

fn read_limited(
    path: &Path,
    limit: usize,
    kind: &'static str,
) -> Result<Vec<u8>, PluginPackageError> {
    let map_io = |source| PluginPackageError::Io {
        path: path.to_path_buf(),
        source,
    };
    let limit_u64 = u64::try_from(limit).unwrap_or(u64::MAX);

    let advertised = fs::metadata(path).map_err(map_io)?;
    if advertised.len() > limit_u64 {
        return Err(PluginPackageError::FileTooLarge { kind, limit });
    }

    let mut file = fs::File::open(path).map_err(map_io)?;
    let opened = file.metadata().map_err(map_io)?;
    if !opened.is_file() {
        return Err(map_io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        )));
    }
    if opened.len() > limit_u64 {
        return Err(PluginPackageError::FileTooLarge { kind, limit });
    }
    if advertised.len() != opened.len() {
        return Err(PluginPackageError::FileChangedDuringRead(
            path.to_path_buf(),
        ));
    }

    let capacity = usize::try_from(opened.len()).unwrap_or(limit).min(limit);
    let mut bytes = Vec::with_capacity(capacity);
    file.by_ref()
        .take(limit_u64.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(map_io)?;
    if bytes.len() > limit {
        return Err(PluginPackageError::FileTooLarge { kind, limit });
    }

    let final_length = file.metadata().map_err(map_io)?.len();
    if final_length != opened.len()
        || u64::try_from(bytes.len()).unwrap_or(u64::MAX) != final_length
    {
        return Err(PluginPackageError::FileChangedDuringRead(
            path.to_path_buf(),
        ));
    }
    Ok(bytes)
}

fn bundle_digest(manifest: &[u8], module: &[u8]) -> BundleDigest {
    let mut hasher = Sha256::new();
    hasher.update(DIGEST_DOMAIN);
    hash_part(&mut hasher, manifest);
    hash_part(&mut hasher, module);
    BundleDigest(hasher.finalize().into())
}

fn hash_part(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn validate_label(kind: &'static str, value: &str, limit: usize) -> Result<(), PluginPackageError> {
    if value.trim().is_empty()
        || value.chars().count() > limit
        || value.chars().any(char::is_control)
    {
        return Err(PluginPackageError::InvalidLabel { kind, limit });
    }
    Ok(())
}

fn valid_dotted_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.split('.').count() >= 2
        && value.split('.').all(|segment| valid_segment(segment, 63))
}

fn valid_segment(value: &str, limit: usize) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= limit
        && bytes
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    const VALID_MANIFEST: &str = r#"
manifest_version = 1
id = "org.example.search"
name = "Search"
version = "1.2.3"
abi = "1.0"

[[commands]]
id = "open"
title = "Open Search"

[capabilities.invoke-actions]
allow = ["cterm:find-text", "cterm:new-tab"]
"#;

    fn valid_bundle(manifest: &str) -> TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join(MANIFEST_FILE), manifest).unwrap();
        fs::write(directory.path().join(MODULE_FILE), WASM_HEADER).unwrap();
        directory
    }

    #[test]
    fn valid_bundle_has_stable_namespaces_scopes_and_digest() {
        let directory = valid_bundle(VALID_MANIFEST);
        let first = PluginBundle::load(directory.path()).unwrap();
        let second = PluginBundle::load(directory.path()).unwrap();

        assert_eq!(first.digest(), second.digest());
        assert_eq!(first.module(), WASM_HEADER);
        assert_eq!(
            first.manifest().commands()[0].action_id(first.manifest().id()),
            "plugin:org.example.search/open"
        );
        assert_eq!(
            first.manifest().invoke_actions(),
            &BTreeSet::from([
                ActionScope::parse("cterm:find-text").unwrap(),
                ActionScope::parse("cterm:new-tab").unwrap(),
            ])
        );
        assert_eq!(
            first.digest().to_string().parse::<BundleDigest>().unwrap(),
            first.digest()
        );
        assert_eq!(
            first.digest().to_string(),
            "sha256:9cce2ae0529d4ad79d565b1b5ba1c90bcd5972c611c61c1cd6fa349f906fc873"
        );
    }

    #[test]
    fn any_manifest_or_module_change_invalidates_the_digest() {
        let directory = valid_bundle(VALID_MANIFEST);
        let initial = PluginBundle::load(directory.path()).unwrap().digest();

        let changed_manifest = VALID_MANIFEST.replace("Search\"", "Search Tools\"");
        fs::write(directory.path().join(MANIFEST_FILE), changed_manifest).unwrap();
        let manifest_changed = PluginBundle::load(directory.path()).unwrap().digest();
        assert_ne!(manifest_changed, initial);

        fs::write(
            directory.path().join(MODULE_FILE),
            [WASM_HEADER, b"custom"].concat(),
        )
        .unwrap();
        let module_changed = PluginBundle::load(directory.path()).unwrap().digest();
        assert_ne!(module_changed, manifest_changed);
    }

    #[test]
    fn unknown_fields_scopes_and_versions_fail_closed() {
        for manifest in [
            VALID_MANIFEST.replace("manifest_version = 1", "manifest_version = 2"),
            VALID_MANIFEST.replace("abi = \"1.0\"", "abi = \"2.0\""),
            VALID_MANIFEST.replace("cterm:new-tab", "network:*"),
            format!("{VALID_MANIFEST}\nunknown = true\n"),
        ] {
            let directory = valid_bundle(&manifest);
            assert!(PluginBundle::load(directory.path()).is_err());
        }
    }

    #[test]
    fn duplicate_commands_and_scopes_are_rejected() {
        let duplicate_command =
            format!("{VALID_MANIFEST}\n[[commands]]\nid = \"open\"\ntitle = \"Again\"\n");
        let duplicate_scope = VALID_MANIFEST.replace("\"cterm:new-tab\"]", "\"cterm:find-text\"]");

        for manifest in [duplicate_command, duplicate_scope] {
            let directory = valid_bundle(&manifest);
            assert!(PluginBundle::load(directory.path()).is_err());
        }
    }

    #[test]
    fn identifiers_are_strictly_portable() {
        for manifest in [
            VALID_MANIFEST.replace("org.example.search", "Org.Example"),
            VALID_MANIFEST.replace("org.example.search", "example"),
            VALID_MANIFEST.replace("id = \"open\"", "id = \"../open\""),
        ] {
            let directory = valid_bundle(&manifest);
            assert!(PluginBundle::load(directory.path()).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn package_files_cannot_be_symlinks_outside_the_root() {
        use std::os::unix::fs::symlink;

        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join(MODULE_FILE), WASM_HEADER).unwrap();
        let directory = valid_bundle(VALID_MANIFEST);
        fs::remove_file(directory.path().join(MODULE_FILE)).unwrap();
        symlink(
            outside.path().join(MODULE_FILE),
            directory.path().join(MODULE_FILE),
        )
        .unwrap();

        assert!(matches!(
            PluginBundle::load(directory.path()),
            Err(PluginPackageError::PathEscapesBundle(_))
        ));
    }

    #[test]
    fn size_limits_and_wasm_header_are_enforced() {
        let directory = valid_bundle(VALID_MANIFEST);
        assert!(matches!(
            PluginBundle::load_with_limits(
                directory.path(),
                BundleLimits {
                    manifest_bytes: 4,
                    module_bytes: 32,
                }
            ),
            Err(PluginPackageError::FileTooLarge {
                kind: "manifest",
                ..
            })
        ));

        fs::write(directory.path().join(MODULE_FILE), b"not wasm").unwrap();
        assert!(matches!(
            PluginBundle::load(directory.path()),
            Err(PluginPackageError::InvalidWasmModule)
        ));
    }

    #[test]
    fn oversized_sparse_module_is_rejected_before_allocation() {
        let directory = valid_bundle(VALID_MANIFEST);
        let module = fs::File::create(directory.path().join(MODULE_FILE)).unwrap();
        let advertised_size = u64::try_from(BundleLimits::default().module_bytes).unwrap() + 1;
        module.set_len(advertised_size).unwrap();

        assert!(matches!(
            PluginBundle::load(directory.path()),
            Err(PluginPackageError::FileTooLarge {
                kind: "WebAssembly module",
                ..
            })
        ));
    }

    #[test]
    fn digest_parser_rejects_ambiguous_spellings() {
        for digest in [
            "",
            "sha256:00",
            "sha512:0000000000000000000000000000000000000000000000000000000000000000",
            "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert!(digest.parse::<BundleDigest>().is_err());
        }
    }
}
