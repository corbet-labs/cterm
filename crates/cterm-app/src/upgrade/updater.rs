//! GitHub release update checker and downloader
//!
//! This module provides functionality to check for updates from GitHub releases,
//! download new versions, and verify their integrity.

use semver::Version;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Canonical GitHub repository used for cterm releases and updates.
pub const CTERM_GITHUB_REPOSITORY: &str = "corbet-labs/cterm";

/// Errors that can occur during update operations
#[derive(Error, Debug)]
pub enum UpdateError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] rsurl::Error),

    #[error("Failed to parse version: {0}")]
    Version(String),

    #[error("Failed to parse JSON: {0}")]
    Json(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Checksum verification failed")]
    ChecksumMismatch,

    #[error("No suitable release asset found for this platform")]
    NoAssetFound,

    #[error("Unsupported update platform: {os}/{arch}")]
    UnsupportedPlatform { os: String, arch: String },

    #[error("GitHub API rate limit exceeded")]
    RateLimited,

    #[error("Release not found")]
    NotFound,
}

/// Information about an available update
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// Version string (e.g., "1.2.3")
    pub version: String,
    /// Parsed semantic version
    pub semver: Version,
    /// URL to download the binary
    pub download_url: String,
    /// Exact release asset selected for this platform
    pub asset_name: String,
    /// URL to download the SHA256 checksum file (if available)
    pub checksum_url: Option<String>,
    /// Size of the download in bytes
    pub size: u64,
    /// Release notes / changelog
    pub release_notes: String,
    /// Release name/title
    pub name: String,
    /// Whether this is a prerelease
    pub prerelease: bool,
}

/// Update checker for GitHub releases
pub struct Updater {
    /// GitHub repository in "owner/repo" format
    repo: String,
    /// Current version of the application
    current_version: Version,
    /// User-Agent string sent with every request
    user_agent: String,
}

impl Updater {
    /// Create a new updater
    ///
    /// # Arguments
    /// * `repo` - GitHub repository in "owner/repo" format
    /// * `current_version` - Current version string
    pub fn new(repo: &str, current_version: &str) -> Result<Self, UpdateError> {
        let version =
            Version::parse(current_version).map_err(|e| UpdateError::Version(e.to_string()))?;

        Ok(Self {
            repo: repo.to_string(),
            current_version: version,
            user_agent: format!("cterm/{}", current_version),
        })
    }

    /// Build a GET request carrying our User-Agent header.
    fn get(&self, url: &str) -> Result<rsurl::Request, UpdateError> {
        Ok(rsurl::Request::get(url)?.header("User-Agent", &self.user_agent))
    }

    /// Check for available updates
    ///
    /// Returns `Some(UpdateInfo)` if a newer version is available,
    /// `None` if already on the latest version.
    pub fn check_for_update(&self) -> Result<Option<UpdateInfo>, UpdateError> {
        let url = format!("https://api.github.com/repos/{}/releases/latest", self.repo);

        let response = self
            .get(&url)?
            .header("Accept", "application/vnd.github.v3+json")
            .send()?;

        if response.status == 404 {
            return Err(UpdateError::NotFound);
        }

        if response.status == 403 {
            // Check if it's rate limiting
            if response
                .header("X-RateLimit-Remaining")
                .map(|v| v == "0")
                .unwrap_or(false)
            {
                return Err(UpdateError::RateLimited);
            }
        }

        let release: Value =
            serde_json::from_slice(&response.body).map_err(|e| UpdateError::Json(e.to_string()))?;

        self.parse_release(&release)
    }

    /// Parse a GitHub release response
    fn parse_release(&self, release: &Value) -> Result<Option<UpdateInfo>, UpdateError> {
        self.parse_release_for_target(release, std::env::consts::OS, std::env::consts::ARCH)
    }

    /// Parse a release for an explicit platform target.
    ///
    /// Keeping the target explicit makes the published artifact contract
    /// testable on any CI host instead of relying on substring heuristics for
    /// the host that happens to run the test.
    fn parse_release_for_target(
        &self,
        release: &Value,
        target_os: &str,
        target_arch: &str,
    ) -> Result<Option<UpdateInfo>, UpdateError> {
        let tag_name = release["tag_name"]
            .as_str()
            .ok_or_else(|| UpdateError::Json("Missing tag_name".to_string()))?;

        let version_str = tag_name.strip_prefix('v').unwrap_or(tag_name);
        let version =
            Version::parse(version_str).map_err(|e| UpdateError::Version(e.to_string()))?;

        if version <= self.current_version {
            return Ok(None);
        }

        let asset_name = Self::client_asset_name_for(target_os, target_arch).ok_or_else(|| {
            UpdateError::UnsupportedPlatform {
                os: target_os.to_string(),
                arch: target_arch.to_string(),
            }
        })?;
        let (download_url, size) = Self::find_asset(release, asset_name)?;

        // Look for checksum file
        let checksum_url = Self::find_checksum_asset(release, asset_name);

        let release_notes = release["body"].as_str().unwrap_or("").to_string();

        let name = release["name"].as_str().unwrap_or(tag_name).to_string();

        let prerelease = release["prerelease"].as_bool().unwrap_or(false);

        Ok(Some(UpdateInfo {
            version: version_str.to_string(),
            semver: version,
            download_url,
            asset_name: asset_name.to_string(),
            checksum_url,
            size,
            release_notes,
            name,
            prerelease,
        }))
    }

    /// Find the appropriate release asset for the current platform
    fn find_asset(release: &Value, expected_name: &str) -> Result<(String, u64), UpdateError> {
        let assets = release["assets"]
            .as_array()
            .ok_or(UpdateError::NoAssetFound)?;

        for asset in assets {
            let name = asset["name"].as_str().unwrap_or("");

            if name == expected_name {
                let url = asset["browser_download_url"]
                    .as_str()
                    .ok_or(UpdateError::NoAssetFound)?
                    .to_string();

                let size = asset["size"].as_u64().unwrap_or(0);

                return Ok((url, size));
            }
        }

        Err(UpdateError::NoAssetFound)
    }

    /// Find the checksum asset if available
    fn find_checksum_asset(release: &Value, asset_name: &str) -> Option<String> {
        let assets = release["assets"].as_array()?;
        let expected_name = format!("{asset_name}.sha256");

        for asset in assets {
            let name = asset["name"].as_str().unwrap_or("");

            if name == expected_name {
                return asset["browser_download_url"]
                    .as_str()
                    .map(|s| s.to_string());
            }
        }

        None
    }

    /// Exact client archive name published by the release workflow.
    fn client_asset_name_for(target_os: &str, target_arch: &str) -> Option<&'static str> {
        match (target_os, target_arch) {
            ("linux", "x86_64") => Some("cterm-linux-x86_64.tar.gz"),
            ("linux", "aarch64") => Some("cterm-linux-arm64.tar.gz"),
            ("macos", "x86_64" | "aarch64") => Some("cterm-macos-universal.tar.gz"),
            ("windows", "x86_64") => Some("cterm-windows-x86_64.zip"),
            _ => None,
        }
    }

    fn client_asset_name() -> Result<&'static str, UpdateError> {
        Self::client_asset_name_for(std::env::consts::OS, std::env::consts::ARCH).ok_or_else(|| {
            UpdateError::UnsupportedPlatform {
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
            }
        })
    }

    /// Download the update to a temporary file
    ///
    /// # Arguments
    /// * `info` - Update information from `check_for_update`
    /// * `on_progress` - Callback for progress updates (bytes_downloaded, total_bytes)
    ///
    /// # Returns
    /// Path to the downloaded file
    pub fn download<F>(&self, info: &UpdateInfo, mut on_progress: F) -> Result<PathBuf, UpdateError>
    where
        F: FnMut(u64, u64),
    {
        let expected_asset = Self::client_asset_name()?;
        if info.asset_name != expected_asset {
            return Err(UpdateError::NoAssetFound);
        }

        // `send_reader` yields a `Read` over the raw (undecoded) body and exposes
        // the response head immediately, so we can read `content-length` before
        // streaming the bytes to disk.
        let mut reader = self.get(&info.download_url)?.send_reader()?;

        if reader.status() >= 400 {
            return Err(UpdateError::Http(rsurl::Error::Status {
                code: reader.status(),
                reason: String::new(),
            }));
        }

        let total_size = reader
            .header("content-length")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(info.size);

        // Preserve the archive suffix so installers can select the correct
        // extraction path without trusting the download URL.
        let temp_dir = std::env::temp_dir();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let file_name = format!(
            "cterm-update-{}-{nonce}-{}",
            std::process::id(),
            info.asset_name
        );
        let file_path = temp_dir.join(&file_name);

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&file_path)?;
        let mut downloaded: u64 = 0;

        // Stream the download
        let mut buf = [0u8; 65536];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            downloaded += n as u64;
            on_progress(downloaded, total_size);
        }

        file.flush()?;

        Ok(file_path)
    }

    /// Extract a Linux client release and return the actual UI binary.
    ///
    /// The daemon remains next to the UI binary so the relaunched process can
    /// resolve the matching release daemon before consulting `PATH`.
    #[cfg(target_os = "linux")]
    pub fn prepare_linux_update(archive_path: &Path) -> Result<PathBuf, UpdateError> {
        let asset_name = Self::client_asset_name()?;
        Self::prepare_linux_update_for_asset(archive_path, asset_name)
    }

    #[cfg(any(target_os = "linux", test))]
    fn prepare_linux_update_for_asset(
        archive_path: &Path,
        asset_name: &str,
    ) -> Result<PathBuf, UpdateError> {
        let bundle_name = asset_name.strip_suffix(".tar.gz").ok_or_else(|| {
            UpdateError::Json(format!(
                "Linux update asset is not a tar.gz archive: {asset_name}"
            ))
        })?;
        let extracted_dir = Self::extract_archive(archive_path)?;
        let bundle_dir = extracted_dir.join(bundle_name);
        let cterm = bundle_dir.join("cterm");
        let ctermd = bundle_dir.join("ctermd");

        Self::require_regular_file(&cterm, "cterm")?;
        Self::require_regular_file(&ctermd, "ctermd")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for binary in [&cterm, &ctermd] {
                let mut permissions = std::fs::metadata(binary)?.permissions();
                permissions.set_mode(permissions.mode() | 0o700);
                std::fs::set_permissions(binary, permissions)?;
            }
        }

        Ok(cterm)
    }

    #[cfg(any(target_os = "linux", test))]
    fn require_regular_file(path: &Path, name: &str) -> Result<(), UpdateError> {
        let metadata = std::fs::symlink_metadata(path).map_err(|error| {
            UpdateError::Io(std::io::Error::new(
                error.kind(),
                format!(
                    "{name} missing from update archive at {}: {error}",
                    path.display()
                ),
            ))
        })?;
        if !metadata.file_type().is_file() {
            return Err(UpdateError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{name} in update archive is not a regular file"),
            )));
        }
        Ok(())
    }

    /// Extract a downloaded tar.gz archive
    ///
    /// # Arguments
    /// * `archive_path` - Path to the downloaded tar.gz file
    ///
    /// # Returns
    /// Path to the extracted directory
    pub fn extract_archive(archive_path: &Path) -> Result<PathBuf, UpdateError> {
        use flate2::read::GzDecoder;
        use std::fs::File;
        use tar::Archive;

        let file = File::open(archive_path)?;
        let decoder = GzDecoder::new(file);
        let mut archive = Archive::new(decoder);

        // Extract to a temp directory next to the archive
        let extract_dir = archive_path.with_extension("extracted");
        if extract_dir.exists() {
            std::fs::remove_dir_all(&extract_dir)?;
        }
        std::fs::create_dir_all(&extract_dir)?;

        archive.unpack(&extract_dir)?;

        Ok(extract_dir)
    }

    /// Install the update on macOS by replacing the app bundle
    ///
    /// # Arguments
    /// * `extracted_dir` - Path to the extracted update directory containing cterm.app
    ///
    /// # Returns
    /// Path to the installed app bundle's binary
    #[cfg(target_os = "macos")]
    pub fn install_macos_update(extracted_dir: &Path) -> Result<PathBuf, UpdateError> {
        let new_app = extracted_dir.join("cterm.app");
        if !new_app.exists() {
            return Err(UpdateError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cterm.app not found in extracted archive",
            )));
        }

        // Get the current app bundle location
        let current_exe = std::env::current_exe()?;

        // Check if we're running from an app bundle
        // The path would be like: /Applications/cterm.app/Contents/MacOS/cterm
        let current_app = if let Some(macos_dir) = current_exe.parent() {
            if macos_dir.ends_with("MacOS") {
                if let Some(contents_dir) = macos_dir.parent() {
                    if contents_dir.ends_with("Contents") {
                        contents_dir.parent().map(|p| p.to_path_buf())
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        let target_app = match current_app {
            Some(app) => app,
            None => {
                // Not running from an app bundle, install to /Applications
                PathBuf::from("/Applications/cterm.app")
            }
        };

        log::info!(
            "Installing update: {} -> {}",
            new_app.display(),
            target_app.display()
        );

        // Remove old app bundle if it exists
        if target_app.exists() {
            // Move to trash or backup instead of deleting
            let backup_path = target_app.with_extension("app.backup");
            if backup_path.exists() {
                // Check that backup path is not a symlink to prevent symlink attacks
                let meta = std::fs::symlink_metadata(&backup_path)?;
                if meta.is_symlink() {
                    std::fs::remove_file(&backup_path)?;
                } else {
                    std::fs::remove_dir_all(&backup_path)?;
                }
            }
            std::fs::rename(&target_app, &backup_path)?;
        }

        // Move new app bundle into place
        std::fs::rename(&new_app, &target_app)?;

        // Return path to the new binary
        let new_binary = target_app.join("Contents/MacOS/cterm");
        Ok(new_binary)
    }

    /// Verify the downloaded file against its SHA256 checksum
    ///
    /// # Arguments
    /// * `file_path` - Path to the downloaded file
    /// * `info` - Update information containing checksum URL
    ///
    /// # Returns
    /// `Ok(true)` if verification passed, `Ok(false)` if no checksum available,
    /// `Err` on verification failure
    pub fn verify(&self, file_path: &Path, info: &UpdateInfo) -> Result<bool, UpdateError> {
        let checksum_url = match &info.checksum_url {
            Some(url) => url,
            None => return Ok(false), // No checksum available
        };

        // Download checksum file
        let response = self.get(checksum_url)?.send()?;

        if response.status >= 400 {
            return Err(UpdateError::Http(rsurl::Error::Status {
                code: response.status,
                reason: String::new(),
            }));
        }

        let checksum_text = response.text()?;

        // Parse expected checksum (format: "hash  filename" or just "hash")
        let expected_hash = checksum_text
            .split_whitespace()
            .next()
            .ok_or_else(|| UpdateError::Json("Invalid checksum format".to_string()))?
            .to_lowercase();

        // Calculate actual hash
        let file_data = std::fs::read(file_path)?;
        let mut hasher = Sha256::new();
        hasher.update(&file_data);
        let actual_hash = format!("{:x}", hasher.finalize());

        if actual_hash != expected_hash {
            return Err(UpdateError::ChecksumMismatch);
        }

        Ok(true)
    }

    /// Verify a file against an expected SHA256 hash
    pub fn verify_hash(file_path: &Path, expected_hash: &str) -> Result<bool, UpdateError> {
        let file_data = std::fs::read(file_path)?;
        let mut hasher = Sha256::new();
        hasher.update(&file_data);
        let actual_hash = format!("{:x}", hasher.finalize());

        Ok(actual_hash.to_lowercase() == expected_hash.to_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RELEASE_TARGETS: [(&str, &str, &str); 5] = [
        ("linux", "x86_64", "cterm-linux-x86_64.tar.gz"),
        ("linux", "aarch64", "cterm-linux-arm64.tar.gz"),
        ("macos", "x86_64", "cterm-macos-universal.tar.gz"),
        ("macos", "aarch64", "cterm-macos-universal.tar.gz"),
        ("windows", "x86_64", "cterm-windows-x86_64.zip"),
    ];

    fn synthetic_release(client_asset: &str, include_client: bool) -> Value {
        let daemon_asset = client_asset.replacen("cterm-", "ctermd-", 1);
        let daemon_checksum = format!("{daemon_asset}.sha256");
        let client_checksum = format!("{client_asset}.sha256");
        let mut assets = vec![
            json!({
                "name": daemon_asset,
                "browser_download_url": "https://example.invalid/daemon",
                "size": 11
            }),
            json!({
                "name": daemon_checksum,
                "browser_download_url": "https://example.invalid/daemon.sha256",
                "size": 64
            }),
        ];
        if include_client {
            assets.extend([
                json!({
                    "name": client_checksum,
                    "browser_download_url": "https://example.invalid/client.sha256",
                    "size": 64
                }),
                json!({
                    "name": client_asset,
                    "browser_download_url": "https://example.invalid/client",
                    "size": 42
                }),
            ]);
        }

        json!({
            "tag_name": "v0.0.20",
            "name": "cterm 0.0.20",
            "body": "release notes",
            "prerelease": false,
            "assets": assets
        })
    }

    #[test]
    fn exact_client_assets_are_selected_for_every_release_target() {
        let updater = Updater::new("example/cterm", "0.0.19").unwrap();

        for (target_os, target_arch, expected_asset) in RELEASE_TARGETS {
            assert_eq!(
                Updater::client_asset_name_for(target_os, target_arch),
                Some(expected_asset)
            );
            let release = synthetic_release(expected_asset, true);
            let info = updater
                .parse_release_for_target(&release, target_os, target_arch)
                .unwrap()
                .unwrap();
            assert_eq!(info.asset_name, expected_asset);
            assert_eq!(info.download_url, "https://example.invalid/client");
            assert_eq!(
                info.checksum_url.as_deref(),
                Some("https://example.invalid/client.sha256")
            );
            assert_eq!(info.size, 42);
        }
    }

    #[test]
    fn daemon_assets_never_satisfy_the_client_update_contract() {
        let updater = Updater::new("example/cterm", "0.0.19").unwrap();

        for (target_os, target_arch, expected_asset) in RELEASE_TARGETS {
            let release = synthetic_release(expected_asset, false);
            assert!(matches!(
                updater.parse_release_for_target(&release, target_os, target_arch),
                Err(UpdateError::NoAssetFound)
            ));
        }
    }

    #[test]
    fn unsupported_release_targets_fail_explicitly() {
        let updater = Updater::new("example/cterm", "0.0.19").unwrap();
        let release = synthetic_release("cterm-freebsd-x86_64.tar.gz", true);
        assert!(matches!(
            updater.parse_release_for_target(&release, "freebsd", "x86_64"),
            Err(UpdateError::UnsupportedPlatform { .. })
        ));
    }

    #[test]
    fn unsupported_release_targets_can_still_report_the_current_version() {
        let updater = Updater::new("example/cterm", "0.0.20").unwrap();
        let release = synthetic_release("cterm-freebsd-x86_64.tar.gz", true);
        assert!(updater
            .parse_release_for_target(&release, "freebsd", "x86_64")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_version_comparison() {
        let v1 = Version::parse("1.0.0").unwrap();
        let v2 = Version::parse("1.0.1").unwrap();
        let v3 = Version::parse("2.0.0").unwrap();

        assert!(v2 > v1);
        assert!(v3 > v2);
        assert!(v3 > v1);
    }

    #[test]
    fn test_verify_hash() {
        use std::io::Write;

        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test_hash_file");
        let mut file = std::fs::File::create(&file_path).unwrap();
        file.write_all(b"test content").unwrap();
        file.flush().unwrap();

        // Known SHA256 of "test content"
        let expected_hash = "6ae8a75555209fd6c44157c0aed8016e763ff435a19cf186f76863140143ff72";

        let result = Updater::verify_hash(&file_path, expected_hash).unwrap();
        assert!(result);

        // Wrong hash should return false
        let result = Updater::verify_hash(&file_path, "wronghash").unwrap();
        assert!(!result);
    }

    fn write_linux_update_archive(path: &Path, include_daemon: bool) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::{Builder, Header};

        fn append(builder: &mut Builder<GzEncoder<std::fs::File>>, path: &str, contents: &[u8]) {
            let mut header = Header::new_gnu();
            header.set_mode(0o755);
            header.set_size(contents.len() as u64);
            header.set_cksum();
            builder.append_data(&mut header, path, contents).unwrap();
        }

        let file = std::fs::File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);
        append(&mut builder, "cterm-linux-x86_64/cterm", b"client binary");
        if include_daemon {
            append(&mut builder, "cterm-linux-x86_64/ctermd", b"daemon binary");
        }
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
    }

    #[test]
    fn linux_update_preparation_returns_client_with_daemon_beside_it() {
        let temp_dir = tempfile::tempdir().unwrap();
        let archive = temp_dir.path().join("cterm-linux-x86_64.tar.gz");
        write_linux_update_archive(&archive, true);

        let binary =
            Updater::prepare_linux_update_for_asset(&archive, "cterm-linux-x86_64.tar.gz").unwrap();
        assert_eq!(std::fs::read(&binary).unwrap(), b"client binary");
        assert_eq!(
            std::fs::read(binary.with_file_name("ctermd")).unwrap(),
            b"daemon binary"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_ne!(
                std::fs::metadata(&binary).unwrap().permissions().mode() & 0o100,
                0
            );
            assert_ne!(
                std::fs::metadata(binary.with_file_name("ctermd"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o100,
                0
            );
        }
    }

    #[test]
    fn linux_update_preparation_rejects_archive_without_daemon() {
        let temp_dir = tempfile::tempdir().unwrap();
        let archive = temp_dir.path().join("cterm-linux-x86_64.tar.gz");
        write_linux_update_archive(&archive, false);

        let error = Updater::prepare_linux_update_for_asset(&archive, "cterm-linux-x86_64.tar.gz")
            .unwrap_err();
        assert!(error.to_string().contains("ctermd missing"));
    }
}
