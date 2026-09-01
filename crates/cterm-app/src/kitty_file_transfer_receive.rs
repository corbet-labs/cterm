//! Consent-gated filesystem reads for Kitty OSC 5113 receive sessions.
//!
//! Approved paths are opened once during metadata listing and retained as file
//! handles. Later data requests can only address those listed paths, so a
//! remote client cannot substitute a new path after the native consent prompt
//! or win a pathname replacement race between listing and transmission.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use cterm_core::{
    FileTransferAction, FileTransferCommand, FileTransferCompression, FileTransferType,
    FileTransmissionType, MAX_FILE_TRANSFER_CHUNK_BYTES,
};
use flate2::{write::ZlibEncoder, Compression};
use fs_at::OpenOptions as OpenOptionsAt;

use crate::kitty_file_transfer::{AuthorizedTtyTransferCommand, TtyTransferDirection};
use crate::kitty_file_transfer_fs::{
    resolve_protocol_path, TtyTransferFilesystemConfigError, TtyTransferLimits,
};

/// Filesystem stage for approved local-to-remote regular-file transfers.
#[derive(Debug)]
pub struct TtyTransferReceiveFilesystem {
    home: PathBuf,
    protocol_home: String,
    limits: TtyTransferLimits,
    sessions: HashMap<String, ReceiveSession>,
}

impl TtyTransferReceiveFilesystem {
    pub fn new(
        home: PathBuf,
        limits: TtyTransferLimits,
    ) -> Result<Self, TtyTransferFilesystemConfigError> {
        if !home.is_absolute() {
            return Err(TtyTransferFilesystemConfigError::HomeIsNotAbsolute);
        }
        let protocol_home = protocol_absolute_path(&home)
            .ok_or(TtyTransferFilesystemConfigError::HomeIsNotRepresentable)?;
        Ok(Self {
            home,
            protocol_home,
            limits,
            sessions: HashMap::new(),
        })
    }

    pub fn active_sessions(&self) -> usize {
        self.sessions.len()
    }

    pub(crate) fn handle_authorized<F>(
        &mut self,
        authorized: AuthorizedTtyTransferCommand,
        emit: &mut F,
    ) -> bool
    where
        F: FnMut(Vec<u8>) -> bool,
    {
        debug_assert_eq!(authorized.direction(), TtyTransferDirection::Receive);
        let (_, quiet, command) = authorized.into_parts();
        match command.action {
            FileTransferAction::File => {
                let listing = self
                    .sessions
                    .get(&command.id)
                    .is_none_or(|session| !session.listing_complete);
                if listing {
                    self.list_regular_file(command, quiet, emit)
                } else {
                    self.transmit_regular_file(command, quiet, emit)
                }
            }
            FileTransferAction::Data | FileTransferAction::EndData => emit_status(
                emit,
                &command,
                "ENOTSUP:Rsync signatures are not implemented",
                quiet,
                true,
                None,
                None,
            ),
            FileTransferAction::Finished => {
                self.sessions.remove(&command.id);
                true
            }
            _ => true,
        }
    }

    pub(crate) fn complete_listing<F>(
        &mut self,
        session_id: String,
        _quiet: u8,
        emit: &mut F,
    ) -> bool
    where
        F: FnMut(Vec<u8>) -> bool,
    {
        let session = self.sessions.entry(session_id.clone()).or_default();
        session.listing_complete = true;
        let request = FileTransferCommand {
            action: FileTransferAction::Status,
            id: session_id,
            file_id: None,
            bypass: None,
            quiet: 0,
            mtime: None,
            permissions: None,
            size: None,
            name: None,
            status: None,
            parent: None,
            data: Vec::new(),
            compression: None,
            file_type: None,
            transmission_type: None,
        };
        // Kitty's receive-side metadata terminator is required even when
        // acknowledgement chatter is otherwise quieted: it tells the client
        // that the listing is complete and supplies the remote home path.
        emit_status(
            emit,
            &request,
            "OK",
            0,
            false,
            None,
            Some(self.protocol_home.clone()),
        )
    }

    pub(crate) fn abort(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
    }

    fn list_regular_file<F>(
        &mut self,
        command: FileTransferCommand,
        quiet: u8,
        emit: &mut F,
    ) -> bool
    where
        F: FnMut(Vec<u8>) -> bool,
    {
        let file_id = command
            .file_id
            .as_deref()
            .expect("authorization validates receive file ids");
        let requested_name = command
            .name
            .as_deref()
            .expect("authorization validates receive paths");
        let Some(path) = resolve_protocol_path(&self.home, requested_name) else {
            return emit_status(
                emit,
                &command,
                "EINVAL:Source path is not representable",
                quiet,
                true,
                None,
                None,
            );
        };
        let listed_name = match protocol_absolute_path(&path) {
            Some(name) => name,
            None => {
                return emit_status(
                    emit,
                    &command,
                    "EINVAL:Source path is not representable",
                    quiet,
                    true,
                    None,
                    None,
                );
            }
        };
        let file = match open_regular_nofollow(&path) {
            Ok(file) => file,
            Err(error) => {
                return emit_status(
                    emit,
                    &command,
                    receive_io_status(&error, "EIO:Could not open source file"),
                    quiet,
                    true,
                    None,
                    None,
                );
            }
        };
        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                return emit_status(
                    emit,
                    &command,
                    receive_io_status(&error, "EIO:Could not inspect source file"),
                    quiet,
                    true,
                    None,
                    None,
                );
            }
        };
        if !metadata.is_file() {
            return emit_status(
                emit,
                &command,
                "ENOTSUP:Only regular-file receive is implemented",
                quiet,
                true,
                None,
                None,
            );
        }

        let session = self.sessions.entry(command.id.clone()).or_default();
        if session.listing_complete {
            return emit_status(
                emit,
                &command,
                "EINVAL:Receive metadata listing is already complete",
                quiet,
                true,
                None,
                None,
            );
        }
        if session.listed_files >= self.limits.max_files_per_session {
            return emit_status(
                emit,
                &command,
                "ENOSPC:Too many files in transfer session",
                quiet,
                true,
                None,
                None,
            );
        }
        let size = metadata.len();
        if size > self.limits.max_file_bytes
            || session
                .planned_bytes
                .checked_add(size)
                .is_none_or(|total| total > self.limits.max_session_bytes)
        {
            return emit_status(
                emit,
                &command,
                "EFBIG:Transfer exceeds configured size limits",
                quiet,
                true,
                None,
                None,
            );
        }

        let actual_id = session.next_actual_id.to_string();
        session.next_actual_id = session.next_actual_id.wrapping_add(1);
        session.listed_files += 1;
        session.planned_bytes += size;
        session
            .sources
            .entry(listed_name.clone())
            .or_default()
            .push_back(ReceiveSource { file, size });

        let metadata_command = FileTransferCommand {
            action: FileTransferAction::File,
            id: command.id,
            file_id: Some(file_id.to_string()),
            bypass: None,
            quiet: 0,
            mtime: modification_time_nanoseconds(&metadata),
            permissions: Some(protocol_permissions(&metadata)),
            size: Some(size),
            name: Some(listed_name),
            status: Some(actual_id),
            parent: None,
            data: Vec::new(),
            compression: None,
            file_type: Some(FileTransferType::Regular),
            transmission_type: None,
        };
        emit_command(emit, metadata_command)
    }

    fn transmit_regular_file<F>(
        &mut self,
        command: FileTransferCommand,
        quiet: u8,
        emit: &mut F,
    ) -> bool
    where
        F: FnMut(Vec<u8>) -> bool,
    {
        if !matches!(
            command.transmission_type,
            None | Some(FileTransmissionType::Simple)
        ) {
            return emit_status(
                emit,
                &command,
                "ENOTSUP:Rsync transmission is not implemented",
                quiet,
                true,
                None,
                None,
            );
        }
        let file_id = command
            .file_id
            .as_deref()
            .expect("authorization validates receive file ids");
        let requested_name = command
            .name
            .as_deref()
            .expect("authorization validates receive paths");
        let Some(session) = self.sessions.get_mut(&command.id) else {
            return emit_status(
                emit,
                &command,
                "ENOENT:Unknown receive filesystem session",
                quiet,
                true,
                None,
                None,
            );
        };
        if session.transmitted_ids.contains(file_id) {
            return emit_status(
                emit,
                &command,
                "EEXIST:Duplicate receive file id",
                quiet,
                true,
                None,
                None,
            );
        }
        let Some(sources) = session.sources.get_mut(requested_name) else {
            return emit_status(
                emit,
                &command,
                "EPERM:Source path was not approved",
                quiet,
                true,
                None,
                None,
            );
        };
        session.transmitted_ids.insert(file_id.to_string());
        let Some(mut source) = sources.pop_front() else {
            return emit_status(
                emit,
                &command,
                "ENOENT:Source file was already transmitted",
                quiet,
                true,
                None,
                None,
            );
        };
        if sources.is_empty() {
            session.sources.remove(requested_name);
        }
        if match source.file.metadata() {
            Ok(metadata) => metadata.len() != source.size,
            Err(_) => true,
        } {
            return emit_status(
                emit,
                &command,
                "ESTALE:Source file changed after approval",
                quiet,
                true,
                None,
                None,
            );
        }

        let result = stream_file(
            &mut source.file,
            source.size,
            &command.id,
            file_id,
            command.compression,
            emit,
        );
        match result {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => false,
            Err(error) => emit_status(
                emit,
                &command,
                receive_io_status(&error, "EIO:Could not read source file"),
                quiet,
                true,
                None,
                None,
            ),
        }
    }
}

#[derive(Debug, Default)]
struct ReceiveSession {
    sources: HashMap<String, VecDeque<ReceiveSource>>,
    transmitted_ids: HashSet<String>,
    listed_files: usize,
    planned_bytes: u64,
    next_actual_id: u64,
    listing_complete: bool,
}

#[derive(Debug)]
struct ReceiveSource {
    file: fs::File,
    size: u64,
}

fn open_regular_nofollow(path: &Path) -> io::Result<fs::File> {
    let parent_path = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no file name"))?;
    let parent = open_read_directory(parent_path)?;
    let mut options = OpenOptionsAt::default();
    options.read(true).follow(false);
    let file = options.open_at(&parent, name)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_read_directory(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(windows)]
fn open_read_directory(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use winapi::um::winbase::FILE_FLAG_BACKUP_SEMANTICS;
    use winapi::um::winnt::{
        FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    fs::OpenOptions::new()
        .access_mode(FILE_GENERIC_READ)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_read_directory(_path: &Path) -> io::Result<fs::File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "handle-relative reads are unsupported on this platform",
    ))
}

fn stream_file<F>(
    file: &mut fs::File,
    expected_size: u64,
    session_id: &str,
    file_id: &str,
    compression: Option<FileTransferCompression>,
    emit: &mut F,
) -> io::Result<()>
where
    F: FnMut(Vec<u8>) -> bool,
{
    const READ_BUFFER_BYTES: usize = 32 * 1024;

    let mut remaining = expected_size;
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    let mut chunks = ProtocolChunkWriter::new(session_id, file_id, emit);
    match compression {
        Some(FileTransferCompression::Zlib) => {
            let mut encoder = ZlibEncoder::new(chunks, Compression::default());
            while remaining != 0 {
                let limit = usize::try_from(remaining.min(READ_BUFFER_BYTES as u64))
                    .expect("read limit is bounded by the fixed buffer");
                let read = file.read(&mut buffer[..limit])?;
                if read == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "source was truncated during transmission",
                    ));
                }
                encoder.write_all(&buffer[..read])?;
                remaining -= read as u64;
            }
            chunks = encoder.finish()?;
        }
        None | Some(FileTransferCompression::None) => {
            while remaining != 0 {
                let limit = usize::try_from(remaining.min(READ_BUFFER_BYTES as u64))
                    .expect("read limit is bounded by the fixed buffer");
                let read = file.read(&mut buffer[..limit])?;
                if read == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "source was truncated during transmission",
                    ));
                }
                chunks.write_all(&buffer[..read])?;
                remaining -= read as u64;
            }
        }
    }
    chunks.finish()
}

struct ProtocolChunkWriter<'a, F> {
    session_id: &'a str,
    file_id: &'a str,
    emit: &'a mut F,
    pending: Vec<u8>,
}

impl<'a, F> ProtocolChunkWriter<'a, F>
where
    F: FnMut(Vec<u8>) -> bool,
{
    fn new(session_id: &'a str, file_id: &'a str, emit: &'a mut F) -> Self {
        Self {
            session_id,
            file_id,
            emit,
            pending: Vec::with_capacity(MAX_FILE_TRANSFER_CHUNK_BYTES),
        }
    }

    fn emit_pending(&mut self, action: FileTransferAction) -> io::Result<()> {
        let data = std::mem::take(&mut self.pending);
        self.pending = Vec::with_capacity(MAX_FILE_TRANSFER_CHUNK_BYTES);
        let command = FileTransferCommand {
            action,
            id: self.session_id.to_string(),
            file_id: Some(self.file_id.to_string()),
            bypass: None,
            quiet: 0,
            mtime: None,
            permissions: None,
            size: None,
            name: None,
            status: None,
            parent: None,
            data,
            compression: None,
            file_type: None,
            transmission_type: None,
        };
        let encoded = command
            .encode()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if (self.emit)(encoded) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "PTY response sink closed",
            ))
        }
    }

    fn finish(mut self) -> io::Result<()> {
        self.emit_pending(FileTransferAction::EndData)
    }
}

impl<F> Write for ProtocolChunkWriter<'_, F>
where
    F: FnMut(Vec<u8>) -> bool,
{
    fn write(&mut self, mut buffer: &[u8]) -> io::Result<usize> {
        let original = buffer.len();
        while !buffer.is_empty() {
            if self.pending.len() == MAX_FILE_TRANSFER_CHUNK_BYTES {
                self.emit_pending(FileTransferAction::Data)?;
            }
            let available = MAX_FILE_TRANSFER_CHUNK_BYTES - self.pending.len();
            let take = available.min(buffer.len());
            self.pending.extend_from_slice(&buffer[..take]);
            buffer = &buffer[take..];
        }
        Ok(original)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn emit_status<F>(
    emit: &mut F,
    request: &FileTransferCommand,
    status: &str,
    quiet: u8,
    is_error: bool,
    size: Option<u64>,
    name: Option<String>,
) -> bool
where
    F: FnMut(Vec<u8>) -> bool,
{
    if quiet >= 2 || quiet == 1 && !is_error {
        return true;
    }
    emit_command(
        emit,
        FileTransferCommand {
            action: FileTransferAction::Status,
            id: request.id.clone(),
            file_id: request.file_id.clone(),
            bypass: None,
            quiet: 0,
            mtime: None,
            permissions: None,
            size,
            name,
            status: Some(status.to_string()),
            parent: None,
            data: Vec::new(),
            compression: None,
            file_type: None,
            transmission_type: None,
        },
    )
}

fn emit_command<F>(emit: &mut F, command: FileTransferCommand) -> bool
where
    F: FnMut(Vec<u8>) -> bool,
{
    command.encode().is_ok_and(emit)
}

fn receive_io_status(error: &io::Error, fallback: &'static str) -> &'static str {
    match error.kind() {
        io::ErrorKind::NotFound => "ENOENT:Source does not exist",
        io::ErrorKind::PermissionDenied => "EPERM:Permission denied",
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => {
            "EINVAL:Source is not a regular file"
        }
        io::ErrorKind::UnexpectedEof => "ESTALE:Source changed during transmission",
        _ => fallback,
    }
}

fn modification_time_nanoseconds(metadata: &fs::Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).ok(),
        Err(error) => i64::try_from(error.duration().as_nanos())
            .ok()
            .and_then(i64::checked_neg),
    }
}

#[cfg(unix)]
fn protocol_permissions(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o7777
}

#[cfg(not(unix))]
fn protocol_permissions(metadata: &fs::Metadata) -> u32 {
    if metadata.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

#[cfg(not(windows))]
fn protocol_absolute_path(path: &Path) -> Option<String> {
    path.is_absolute()
        .then(|| path.to_str().map(str::to_string))?
}

#[cfg(windows)]
fn protocol_absolute_path(path: &Path) -> Option<String> {
    use std::path::Component;

    let mut components = path.components();
    let Component::Prefix(prefix) = components.next()? else {
        return None;
    };
    let drive = prefix.as_os_str().to_str()?;
    if drive.len() != 2 || !drive.as_bytes()[0].is_ascii_alphabetic() || !drive.ends_with(':') {
        return None;
    }
    if !matches!(components.next(), Some(Component::RootDir)) {
        return None;
    }
    let mut output = format!("/{drive}");
    for component in components {
        let Component::Normal(component) = component else {
            return None;
        };
        output.push('/');
        output.push_str(component.to_str()?);
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty_file_transfer::{TtyTransferAction, TtyTransferManager};
    use flate2::read::ZlibDecoder;

    fn command(action: FileTransferAction, id: &str) -> FileTransferCommand {
        FileTransferCommand {
            action,
            id: id.to_string(),
            file_id: None,
            bypass: None,
            quiet: 0,
            mtime: None,
            permissions: None,
            size: None,
            name: None,
            status: None,
            parent: None,
            data: Vec::new(),
            compression: None,
            file_type: None,
            transmission_type: None,
        }
    }

    fn decode(encoded: &[u8]) -> FileTransferCommand {
        let body = encoded
            .strip_prefix(b"\x1b]5113;")
            .and_then(|body| body.strip_suffix(b"\x1b\\"))
            .expect("encoded OSC 5113 response");
        let mut fields = vec![&b"5113"[..]];
        fields.extend(body.split(|byte| *byte == b';'));
        cterm_core::parse_file_transfer_command(&fields).expect("valid response")
    }

    fn approve_receive(
        manager: &mut TtyTransferManager,
        id: &str,
        requested: &[(&str, &str)],
    ) -> Vec<TtyTransferAction> {
        let mut start = command(FileTransferAction::Receive, id);
        start.size = Some(requested.len() as u64);
        assert!(manager.handle(start).is_empty());
        let mut prompt = None;
        for (file_id, name) in requested {
            let mut request = command(FileTransferAction::File, id);
            request.file_id = Some((*file_id).to_string());
            request.name = Some((*name).to_string());
            let actions = manager.handle(request);
            if let [TtyTransferAction::RequestApproval(request)] = actions.as_slice() {
                prompt = Some(request.request_id);
            } else {
                assert!(actions.is_empty());
            }
        }
        manager.approve(prompt.expect("receive approval request"))
    }

    fn run_actions(
        filesystem: &mut TtyTransferReceiveFilesystem,
        actions: Vec<TtyTransferAction>,
    ) -> Vec<Vec<u8>> {
        let mut output = Vec::new();
        for action in actions {
            match action {
                TtyTransferAction::Write(bytes) => output.push(bytes),
                TtyTransferAction::Execute(command) => {
                    let mut emit = |bytes| {
                        output.push(bytes);
                        true
                    };
                    assert!(filesystem.handle_authorized(command, &mut emit));
                }
                TtyTransferAction::CompleteReceiveListing { session_id, quiet } => {
                    let mut emit = |bytes| {
                        output.push(bytes);
                        true
                    };
                    assert!(filesystem.complete_listing(session_id, quiet, &mut emit));
                }
                TtyTransferAction::Abort { session_id } => filesystem.abort(&session_id),
                TtyTransferAction::RequestApproval(_) => panic!("unexpected prompt"),
            }
        }
        output
    }

    #[test]
    fn approved_regular_file_is_listed_then_streamed_from_retained_handle() {
        let home = tempfile::tempdir().unwrap();
        let original = home.path().join("report.txt");
        let retained = home.path().join("retained.txt");
        fs::write(&original, b"approved contents").unwrap();
        let limits = TtyTransferLimits::new(8, 1024, 4096).unwrap();
        let mut filesystem =
            TtyTransferReceiveFilesystem::new(home.path().to_path_buf(), limits).unwrap();
        let mut manager = TtyTransferManager::new();

        let listing = run_actions(
            &mut filesystem,
            approve_receive(&mut manager, "receive", &[("spec-0", "~/report.txt")]),
        );
        assert_eq!(decode(&listing[0]).status.as_deref(), Some("OK"));
        let metadata = decode(&listing[1]);
        assert_eq!(metadata.action, FileTransferAction::File);
        assert_eq!(metadata.file_id.as_deref(), Some("spec-0"));
        assert_eq!(metadata.status.as_deref(), Some("0"));
        assert_eq!(metadata.size, Some(17));
        assert_eq!(metadata.file_type, Some(FileTransferType::Regular));
        assert_eq!(decode(&listing[2]).name.as_deref(), home.path().to_str());

        fs::rename(&original, &retained).unwrap();
        fs::write(&original, b"replacement secret").unwrap();
        let mut request = command(FileTransferAction::File, "receive");
        request.file_id = Some("local-1".into());
        request.name = metadata.name;
        let output = run_actions(&mut filesystem, manager.handle(request));
        assert_eq!(output.len(), 1);
        let end = decode(&output[0]);
        assert_eq!(end.action, FileTransferAction::EndData);
        assert_eq!(end.file_id.as_deref(), Some("local-1"));
        assert_eq!(end.data, b"approved contents");
    }

    #[test]
    fn large_and_zlib_sources_stream_in_bounded_protocol_chunks() {
        let home = tempfile::tempdir().unwrap();
        let contents = vec![b'x'; MAX_FILE_TRANSFER_CHUNK_BYTES * 3 + 17];
        fs::write(home.path().join("large.bin"), &contents).unwrap();
        let limits = TtyTransferLimits::new(8, 64 * 1024, 128 * 1024).unwrap();

        for compression in [None, Some(FileTransferCompression::Zlib)] {
            let mut filesystem =
                TtyTransferReceiveFilesystem::new(home.path().to_path_buf(), limits).unwrap();
            let mut manager = TtyTransferManager::new();
            let listing = run_actions(
                &mut filesystem,
                approve_receive(&mut manager, "large", &[("0", "large.bin")]),
            );
            let metadata = decode(&listing[1]);
            let mut request = command(FileTransferAction::File, "large");
            request.file_id = Some("out".into());
            request.name = metadata.name;
            request.compression = compression;
            let output = run_actions(&mut filesystem, manager.handle(request));
            let decoded: Vec<_> = output.iter().map(|packet| decode(packet)).collect();
            assert!(decoded
                .iter()
                .all(|packet| packet.data.len() <= MAX_FILE_TRANSFER_CHUNK_BYTES));
            assert_eq!(decoded.last().unwrap().action, FileTransferAction::EndData);
            let payload: Vec<_> = decoded.into_iter().flat_map(|packet| packet.data).collect();
            let received = if compression == Some(FileTransferCompression::Zlib) {
                let mut decoded = Vec::new();
                ZlibDecoder::new(payload.as_slice())
                    .read_to_end(&mut decoded)
                    .unwrap();
                decoded
            } else {
                payload
            };
            assert_eq!(received, contents);
        }
    }

    #[test]
    fn unlisted_and_changed_sources_are_not_read() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("listed"), b"safe").unwrap();
        fs::write(home.path().join("unlisted"), b"secret").unwrap();
        let limits = TtyTransferLimits::new(8, 1024, 4096).unwrap();
        let mut filesystem =
            TtyTransferReceiveFilesystem::new(home.path().to_path_buf(), limits).unwrap();
        let mut manager = TtyTransferManager::new();
        let listing = run_actions(
            &mut filesystem,
            approve_receive(&mut manager, "guard", &[("0", "listed")]),
        );
        let metadata = decode(&listing[1]);

        let mut unlisted = command(FileTransferAction::File, "guard");
        unlisted.file_id = Some("bad".into());
        unlisted.name = Some(protocol_absolute_path(&home.path().join("unlisted")).unwrap());
        let rejected = run_actions(&mut filesystem, manager.handle(unlisted));
        assert!(decode(&rejected[0])
            .status
            .as_deref()
            .is_some_and(|status| status.starts_with("EPERM:")));

        fs::OpenOptions::new()
            .append(true)
            .open(home.path().join("listed"))
            .unwrap()
            .write_all(b" changed")
            .unwrap();
        let mut changed = command(FileTransferAction::File, "guard");
        changed.file_id = Some("changed".into());
        changed.name = metadata.name;
        let rejected = run_actions(&mut filesystem, manager.handle(changed));
        assert!(decode(&rejected[0])
            .status
            .as_deref()
            .is_some_and(|status| status.starts_with("ESTALE:")));
    }

    #[test]
    fn listing_enforces_file_and_session_limits() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("one"), b"1234").unwrap();
        fs::write(home.path().join("two"), b"5678").unwrap();
        let limits = TtyTransferLimits::new(1, 4, 4).unwrap();
        let mut filesystem =
            TtyTransferReceiveFilesystem::new(home.path().to_path_buf(), limits).unwrap();
        let mut manager = TtyTransferManager::new();
        let output = run_actions(
            &mut filesystem,
            approve_receive(&mut manager, "limits", &[("0", "one"), ("1", "two")]),
        );
        assert_eq!(decode(&output[1]).action, FileTransferAction::File);
        assert!(decode(&output[2])
            .status
            .as_deref()
            .is_some_and(|status| status.starts_with("ENOSPC:")));
    }
}
