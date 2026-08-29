use std::fmt;
use std::io::{self, Read};
use std::path::Path;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::proto::{HandshakeRequest, HandshakeResponse};

#[cfg(windows)]
mod windows_acl;

#[cfg(target_os = "macos")]
mod macos_acl;

/// Apply the Windows auth-file ACL contract to a newly created, still-empty
/// file before secret bytes are written. Existing secrets must be validated,
/// never retroactively made private by this function.
#[cfg(windows)]
pub fn set_private_daemon_auth_file_acl(file: &std::fs::File) -> io::Result<()> {
    windows_acl::set_private_auth_file_acl(file)
}

pub const DAEMON_AUTH_SECRET_BYTES: usize = 32;
pub const DAEMON_AUTH_CHALLENGE_BYTES: usize = 32;
const AUTH_DOMAIN: &[u8] = b"cterm-managed-daemon-auth-v1\0";

/// A managed-daemon authentication secret whose debug output is always
/// redacted. The bytes are loaded from a private file and are never serialized
/// into a process command line or handshake response.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct DaemonAuthSecret([u8; DAEMON_AUTH_SECRET_BYTES]);

impl fmt::Debug for DaemonAuthSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DaemonAuthSecret([REDACTED])")
    }
}

impl DaemonAuthSecret {
    pub fn from_bytes(bytes: [u8; DAEMON_AUTH_SECRET_BYTES]) -> Self {
        Self(bytes)
    }

    fn as_bytes(&self) -> &[u8; DAEMON_AUTH_SECRET_BYTES] {
        &self.0
    }
}

/// Load an exact 32-byte binary secret or its exact 64-character lowercase-hex
/// representation from a private, absolute regular file.
pub fn load_daemon_auth_secret(path: &Path) -> io::Result<DaemonAuthSecret> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "daemon authentication file must be absolute",
        ));
    }

    let symlink_metadata = std::fs::symlink_metadata(path)?;
    if symlink_metadata.file_type().is_symlink() || !symlink_metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "daemon authentication path must be a regular non-symlink file",
        ));
    }

    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.permissions().mode() & 0o7777 != 0o600
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "daemon authentication file must be owned by the current user and private",
            ));
        }
        #[cfg(target_os = "macos")]
        macos_acl::validate_fd_has_no_extended_acl(&file)?;
        file
    };

    #[cfg(not(unix))]
    #[cfg(not(windows))]
    let file = std::fs::File::open(path)?;

    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        windows_acl::validate_private_auth_file(&file)?;
        file
    };

    let mut encoded = Zeroizing::new(Vec::new());
    file.take(DAEMON_AUTH_SECRET_BYTES as u64 * 2 + 1)
        .read_to_end(&mut encoded)?;
    let mut bytes = Zeroizing::new([0_u8; DAEMON_AUTH_SECRET_BYTES]);
    if encoded.len() == DAEMON_AUTH_SECRET_BYTES {
        bytes.copy_from_slice(&encoded);
    } else if encoded.len() == DAEMON_AUTH_SECRET_BYTES * 2
        && encoded
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        for (index, output) in bytes.iter_mut().enumerate() {
            let offset = index * 2;
            *output = (hex_nibble(encoded[offset]) << 4) | hex_nibble(encoded[offset + 1]);
        }
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon authentication file must contain exactly 32 bytes or 64 lowercase hex characters",
        ));
    }

    Ok(DaemonAuthSecret::from_bytes(*bytes))
}

/// Validate the private parent directory used for an authenticated local
/// daemon endpoint. The opened directory, not a pathname lookup result, is the
/// security decision.
#[cfg(unix)]
pub fn validate_private_daemon_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed daemon directory must be absolute",
        ));
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed daemon directory must be a non-symlink directory",
        ));
    }
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir()
        || metadata.permissions().mode() & 0o7777 != 0o700
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "managed daemon directory must be owned by the current user and private",
        ));
    }
    #[cfg(target_os = "macos")]
    macos_acl::validate_fd_has_no_extended_acl(&directory)?;
    Ok(())
}

/// Validate the bound filesystem socket after its mode has been set. Parent
/// directory validation prevents a cross-account replacement race.
#[cfg(unix)]
pub fn validate_private_daemon_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_socket()
        || metadata.permissions().mode() & 0o7777 != 0o700
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "managed daemon socket must be owned by the current user and private",
        ));
    }
    #[cfg(target_os = "macos")]
    macos_acl::validate_path_has_no_extended_acl(path)?;
    Ok(())
}

fn hex_nibble(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        _ => unreachable!("hex input was validated"),
    }
}

fn update_field(mac: &mut Hmac<Sha256>, value: &[u8]) {
    mac.update(&(value.len() as u64).to_be_bytes());
    mac.update(value);
}

fn authentication_mac(
    secret: &DaemonAuthSecret,
    request: &HandshakeRequest,
    response: &HandshakeResponse,
) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC-SHA256 accepts a 32-byte key");
    mac.update(AUTH_DOMAIN);
    mac.update(&request.protocol_version.to_be_bytes());
    update_field(&mut mac, request.client_id.as_bytes());
    update_field(&mut mac, request.client_version.as_bytes());
    update_field(&mut mac, &request.daemon_auth_challenge);
    mac.update(&response.protocol_version.to_be_bytes());
    update_field(&mut mac, response.daemon_id.as_bytes());
    update_field(&mut mac, response.daemon_version.as_bytes());
    update_field(&mut mac, response.daemon_identity.as_bytes());
    mac.update(&[u8::from(response.is_local)]);
    update_field(&mut mac, response.hostname.as_bytes());
    mac
}

pub fn managed_daemon_auth_proof(
    secret: &DaemonAuthSecret,
    request: &HandshakeRequest,
    response: &HandshakeResponse,
) -> Vec<u8> {
    authentication_mac(secret, request, response)
        .finalize()
        .into_bytes()
        .to_vec()
}

/// Verify the proof with the constant-time comparison implemented by the HMAC
/// crate. The proof field itself is not part of the authenticated context.
pub fn verify_managed_daemon_auth_proof(
    secret: &DaemonAuthSecret,
    request: &HandshakeRequest,
    response: &HandshakeResponse,
) -> bool {
    authentication_mac(secret, request, response)
        .verify_slice(&response.daemon_auth_proof)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(challenge: u8) -> HandshakeRequest {
        HandshakeRequest {
            client_id: "client".to_string(),
            client_version: "1.0.0".to_string(),
            protocol_version: crate::PROTOCOL_VERSION,
            daemon_auth_challenge: vec![challenge; DAEMON_AUTH_CHALLENGE_BYTES],
        }
    }

    fn response() -> HandshakeResponse {
        HandshakeResponse {
            daemon_id: "instance".to_string(),
            daemon_version: "1.0.0".to_string(),
            is_local: true,
            hostname: "localhost".to_string(),
            protocol_version: crate::PROTOCOL_VERSION,
            daemon_identity: "managed-product".to_string(),
            daemon_auth_proof: Vec::new(),
        }
    }

    #[test]
    fn proof_is_bound_to_every_handshake_context_field() {
        let secret = DaemonAuthSecret::from_bytes([0x42; DAEMON_AUTH_SECRET_BYTES]);
        let baseline_request = request(1);
        let mut baseline_response = response();
        baseline_response.daemon_auth_proof =
            managed_daemon_auth_proof(&secret, &baseline_request, &baseline_response);

        assert!(verify_managed_daemon_auth_proof(
            &secret,
            &baseline_request,
            &baseline_response
        ));
        assert_ne!(
            baseline_response.daemon_auth_proof,
            secret.as_bytes().as_slice()
        );

        #[derive(Clone, Copy, Debug)]
        enum Mutation {
            ClientId,
            ClientVersion,
            RequestProtocol,
            Challenge,
            ResponseProtocol,
            DaemonId,
            DaemonVersion,
            DaemonIdentity,
            IsLocal,
            Hostname,
            ProofLength,
        }

        for mutation in [
            Mutation::ClientId,
            Mutation::ClientVersion,
            Mutation::RequestProtocol,
            Mutation::Challenge,
            Mutation::ResponseProtocol,
            Mutation::DaemonId,
            Mutation::DaemonVersion,
            Mutation::DaemonIdentity,
            Mutation::IsLocal,
            Mutation::Hostname,
            Mutation::ProofLength,
        ] {
            let mut changed_request = baseline_request.clone();
            let mut changed_response = baseline_response.clone();
            match mutation {
                Mutation::ClientId => changed_request.client_id.push('x'),
                Mutation::ClientVersion => changed_request.client_version.push('x'),
                Mutation::RequestProtocol => changed_request.protocol_version += 1,
                Mutation::Challenge => changed_request.daemon_auth_challenge[0] ^= 1,
                Mutation::ResponseProtocol => changed_response.protocol_version += 1,
                Mutation::DaemonId => changed_response.daemon_id.push('x'),
                Mutation::DaemonVersion => changed_response.daemon_version.push('x'),
                Mutation::DaemonIdentity => changed_response.daemon_identity.push('x'),
                Mutation::IsLocal => changed_response.is_local = !changed_response.is_local,
                Mutation::Hostname => changed_response.hostname.push('x'),
                Mutation::ProofLength => {
                    changed_response.daemon_auth_proof.pop().unwrap();
                }
            }
            assert!(
                !verify_managed_daemon_auth_proof(&secret, &changed_request, &changed_response),
                "context mutation {mutation:?} retained a valid proof"
            );
        }

        let wrong_secret = DaemonAuthSecret::from_bytes([0x24; DAEMON_AUTH_SECRET_BYTES]);
        assert!(!verify_managed_daemon_auth_proof(
            &wrong_secret,
            &baseline_request,
            &baseline_response
        ));
    }

    #[cfg(unix)]
    #[test]
    fn secret_loader_rejects_non_private_files() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("secret");
        std::fs::write(&path, "00".repeat(DAEMON_AUTH_SECRET_BYTES)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_daemon_auth_secret(&path).is_err());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_daemon_auth_secret(&path).is_ok());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(load_daemon_auth_secret(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn managed_directory_validator_rejects_public_mode() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_private_daemon_directory(directory.path()).is_err());

        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(validate_private_daemon_directory(directory.path()).is_ok());

        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        assert!(validate_private_daemon_directory(directory.path()).is_err());
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn secret_and_directory_validators_reject_macos_extended_acls() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("secret");
        std::fs::write(&path, "42".repeat(DAEMON_AUTH_SECRET_BYTES)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let status = std::process::Command::new("/bin/chmod")
            .arg("+a")
            .arg("everyone allow read")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(load_daemon_auth_secret(&path).is_err());

        let status = std::process::Command::new("/bin/chmod")
            .arg("+a")
            .arg("everyone allow search")
            .arg(directory.path())
            .status()
            .unwrap();
        assert!(status.success());
        assert!(validate_private_daemon_directory(directory.path()).is_err());
    }
}
