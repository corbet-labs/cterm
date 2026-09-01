//! Consent-gated filesystem execution for Kitty OSC 5113 send sessions.
//!
//! Regular files are written to bounded temporary files relative to retained
//! destination-directory handles and become visible only when the client
//! finishes the session. The race-resistant filesystem operations, random
//! temporary names, and compression come from the audited `fs_at`, `tempfile`,
//! and `flate2` crates instead of bespoke implementations.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use cterm_core::{
    FileTransferAction, FileTransferCommand, FileTransferCompression, FileTransferType,
    FileTransmissionType,
};
use filetime::{set_file_handle_times, FileTime};
use flate2::{Decompress, FlushDecompress, Status};
use fs_at::{OpenOptions as OpenOptionsAt, OpenOptionsWriteMode};
use tempfile::Builder;
use thiserror::Error;

use crate::kitty_file_transfer::{
    AuthorizedTtyTransferCommand, TtyTransferAction, TtyTransferDirection,
};

/// Explicit resource policy for transfers accepted by the local frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtyTransferLimits {
    pub max_files_per_session: usize,
    pub max_file_bytes: u64,
    pub max_session_bytes: u64,
}

impl TtyTransferLimits {
    pub fn new(
        max_files_per_session: usize,
        max_file_bytes: u64,
        max_session_bytes: u64,
    ) -> Result<Self, TtyTransferFilesystemConfigError> {
        if max_files_per_session == 0 || max_file_bytes == 0 || max_session_bytes == 0 {
            return Err(TtyTransferFilesystemConfigError::ZeroLimit);
        }
        if max_file_bytes > max_session_bytes {
            return Err(TtyTransferFilesystemConfigError::FileLimitExceedsSessionLimit);
        }
        Ok(Self {
            max_files_per_session,
            max_file_bytes,
            max_session_bytes,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TtyTransferFilesystemConfigError {
    #[error("TTY file-transfer limits must be non-zero")]
    ZeroLimit,
    #[error("TTY per-file limit cannot exceed the per-session limit")]
    FileLimitExceedsSessionLimit,
    #[error("TTY file-transfer home directory must be absolute")]
    HomeIsNotAbsolute,
}

/// Filesystem stage for approved remote-to-local regular-file transfers.
#[derive(Debug)]
pub struct TtyTransferSendFilesystem {
    home: PathBuf,
    limits: TtyTransferLimits,
    sessions: HashMap<String, SendSession>,
}

impl TtyTransferSendFilesystem {
    pub fn new(
        home: PathBuf,
        limits: TtyTransferLimits,
    ) -> Result<Self, TtyTransferFilesystemConfigError> {
        if !home.is_absolute() {
            return Err(TtyTransferFilesystemConfigError::HomeIsNotAbsolute);
        }
        Ok(Self {
            home,
            limits,
            sessions: HashMap::new(),
        })
    }

    /// Consume authorization actions while leaving prompts, PTY writes, and
    /// receive-side work available to the frontend or a later pipeline stage.
    pub fn handle_action(&mut self, action: TtyTransferAction) -> Vec<TtyTransferAction> {
        match action {
            TtyTransferAction::Execute(authorized)
                if authorized.direction() == TtyTransferDirection::Send =>
            {
                self.execute(authorized)
                    .into_iter()
                    .map(TtyTransferAction::Write)
                    .collect()
            }
            TtyTransferAction::Abort { session_id } => {
                self.sessions.remove(&session_id);
                Vec::new()
            }
            action => vec![action],
        }
    }

    pub fn active_sessions(&self) -> usize {
        self.sessions.len()
    }

    fn execute(&mut self, authorized: AuthorizedTtyTransferCommand) -> Vec<Vec<u8>> {
        let (_, quiet, command) = authorized.into_parts();
        match command.action {
            FileTransferAction::File => self.stage_file(command, quiet),
            FileTransferAction::Data => self.write_file_data(command, quiet, false),
            FileTransferAction::EndData => self.write_file_data(command, quiet, true),
            FileTransferAction::Finish => self.commit_session(command, quiet),
            _ => Vec::new(),
        }
    }

    fn stage_file(&mut self, command: FileTransferCommand, quiet: u8) -> Vec<Vec<u8>> {
        let file_id = command
            .file_id
            .as_deref()
            .expect("authorization validates file ids")
            .to_string();
        let session = self.sessions.entry(command.id.clone()).or_default();

        if session.files.contains_key(&file_id) || session.rejected.contains(&file_id) {
            return response(&command, "EEXIST:Duplicate file id", quiet, true, None);
        }
        if session.files.len() + session.rejected.len() >= self.limits.max_files_per_session {
            return response(
                &command,
                "ENOSPC:Too many files in transfer session",
                quiet,
                true,
                None,
            );
        }
        if !matches!(command.file_type, None | Some(FileTransferType::Regular)) {
            session.rejected.insert(file_id);
            return response(
                &command,
                "ENOTSUP:File type is not implemented",
                quiet,
                true,
                None,
            );
        }
        if !matches!(
            command.transmission_type,
            None | Some(FileTransmissionType::Simple)
        ) {
            session.rejected.insert(file_id);
            return response(
                &command,
                "ENOTSUP:Transmission type is not implemented",
                quiet,
                true,
                None,
            );
        }

        let reservation = command.size.unwrap_or(self.limits.max_file_bytes);
        if reservation > self.limits.max_file_bytes
            || session
                .reserved_bytes
                .checked_add(reservation)
                .is_none_or(|total| total > self.limits.max_session_bytes)
        {
            session.rejected.insert(file_id);
            return response(
                &command,
                "EFBIG:Transfer exceeds configured size limits",
                quiet,
                true,
                None,
            );
        }

        let name = command
            .name
            .as_deref()
            .expect("authorization validates file paths");
        let Some(destination) = resolve_protocol_path(&self.home, name) else {
            session.rejected.insert(file_id);
            return response(
                &command,
                "EINVAL:Destination path is not representable",
                quiet,
                true,
                None,
            );
        };
        if fs::symlink_metadata(&destination).is_ok_and(|metadata| metadata.is_dir()) {
            session.rejected.insert(file_id);
            return response(
                &command,
                "EISDIR:Destination is a directory",
                quiet,
                true,
                None,
            );
        }
        let temporary = match StagedTempFile::new(&destination) {
            Ok(temporary) => temporary,
            Err(error) => {
                session.rejected.insert(file_id);
                return response(
                    &command,
                    io_status(&error, "Could not stage destination file"),
                    quiet,
                    true,
                    None,
                );
            }
        };
        let output_limit = reservation.min(self.limits.max_file_bytes);
        let limited = LimitedTempFile::new(temporary, output_limit);
        let writer = match command.compression {
            Some(FileTransferCompression::Zlib) => {
                StagedWriter::Zlib(StrictZlibDecoder::new(limited))
            }
            None | Some(FileTransferCompression::None) => StagedWriter::Plain(limited),
        };
        session.reserved_bytes += reservation;
        session.files.insert(
            file_id,
            StagedFile {
                expected_size: command.size,
                permissions: command.permissions,
                mtime: command.mtime,
                writer: Some(writer),
                temporary: None,
                bytes_written: 0,
            },
        );
        response(&command, "STARTED", quiet, false, None)
    }

    fn write_file_data(
        &mut self,
        command: FileTransferCommand,
        quiet: u8,
        completes_file: bool,
    ) -> Vec<Vec<u8>> {
        let file_id = command
            .file_id
            .as_deref()
            .expect("authorization validates file ids");
        let Some(session) = self.sessions.get_mut(&command.id) else {
            return Vec::new();
        };
        if session.rejected.contains(file_id) {
            return Vec::new();
        }
        let Some(file) = session.files.get_mut(file_id) else {
            return Vec::new();
        };
        let Some(writer) = file.writer.as_mut() else {
            session.files.remove(file_id);
            session.rejected.insert(file_id.to_string());
            return response(
                &command,
                "EINVAL:File data was already completed",
                quiet,
                true,
                None,
            );
        };

        if let Err(error) = writer.write_all(&command.data) {
            session.files.remove(file_id);
            session.rejected.insert(file_id.to_string());
            return response(&command, stage_write_status(&error), quiet, true, None);
        }
        file.bytes_written = writer.bytes_written();

        if !completes_file {
            return response(&command, "PROGRESS", quiet, false, Some(file.bytes_written));
        }

        let writer = file.writer.take().expect("writer was checked above");
        let temporary = match writer.finish() {
            Ok(temporary) => temporary,
            Err(error) => {
                session.files.remove(file_id);
                session.rejected.insert(file_id.to_string());
                return response(&command, stage_write_status(&error), quiet, true, None);
            }
        };
        file.bytes_written = temporary.bytes_written;
        if file
            .expected_size
            .is_some_and(|expected| expected != file.bytes_written)
        {
            session.files.remove(file_id);
            session.rejected.insert(file_id.to_string());
            return response(
                &command,
                "EINVAL:Received size does not match file metadata",
                quiet,
                true,
                None,
            );
        }
        if let Err(error) = temporary.file.as_file().sync_all() {
            session.files.remove(file_id);
            session.rejected.insert(file_id.to_string());
            return response(
                &command,
                io_status(&error, "Could not synchronize staged file"),
                quiet,
                true,
                None,
            );
        }
        file.temporary = Some(temporary.file);
        response(&command, "OK", quiet, false, Some(file.bytes_written))
    }

    fn commit_session(&mut self, command: FileTransferCommand, quiet: u8) -> Vec<Vec<u8>> {
        let Some(session) = self.sessions.remove(&command.id) else {
            return Vec::new();
        };
        if session.files.values().any(|file| file.temporary.is_none()) {
            return response(
                &command,
                "EINVAL:Transfer contains incomplete files",
                quiet,
                true,
                None,
            );
        }

        let mut files: Vec<_> = session.files.into_iter().collect();
        files.sort_by(|left, right| left.0.cmp(&right.0));
        for (_, mut file) in files {
            let temporary = file.temporary.take().expect("completion was checked above");
            if let Err(error) = apply_metadata(temporary.as_file(), file.permissions, file.mtime) {
                return response(
                    &command,
                    io_status(&error, "Could not apply file metadata"),
                    quiet,
                    true,
                    None,
                );
            }
            if let Err(error) = temporary.as_file().sync_all() {
                return response(
                    &command,
                    io_status(&error, "Could not synchronize staged metadata"),
                    quiet,
                    true,
                    None,
                );
            }
            if let Err(error) = temporary.commit() {
                return response(
                    &command,
                    io_status(&error, "Could not commit staged file"),
                    quiet,
                    true,
                    None,
                );
            }
        }
        Vec::new()
    }
}

#[derive(Debug, Default)]
struct SendSession {
    files: HashMap<String, StagedFile>,
    rejected: HashSet<String>,
    reserved_bytes: u64,
}

#[derive(Debug)]
struct StagedFile {
    expected_size: Option<u64>,
    permissions: Option<u32>,
    mtime: Option<i64>,
    writer: Option<StagedWriter>,
    temporary: Option<StagedTempFile>,
    bytes_written: u64,
}

#[derive(Debug)]
enum StagedWriter {
    Plain(LimitedTempFile),
    Zlib(StrictZlibDecoder<LimitedTempFile>),
}

impl StagedWriter {
    fn bytes_written(&self) -> u64 {
        match self {
            Self::Plain(file) => file.bytes_written,
            Self::Zlib(decoder) => decoder.get_ref().bytes_written,
        }
    }

    fn finish(self) -> io::Result<LimitedTempFile> {
        match self {
            Self::Plain(mut file) => {
                file.flush()?;
                Ok(file)
            }
            Self::Zlib(decoder) => decoder.finish(),
        }
    }
}

impl Write for StagedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(file) => file.write(buffer),
            Self::Zlib(decoder) => decoder.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(file) => file.flush(),
            Self::Zlib(decoder) => decoder.flush(),
        }
    }
}

#[derive(Debug)]
struct StrictZlibDecoder<W> {
    inner: W,
    decompressor: Decompress,
    complete: bool,
}

impl<W: Write> StrictZlibDecoder<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            decompressor: Decompress::new(true),
            complete: false,
        }
    }

    fn get_ref(&self) -> &W {
        &self.inner
    }

    fn pump(&mut self, mut input: &[u8], flush: FlushDecompress) -> io::Result<()> {
        const OUTPUT_CHUNK_BYTES: usize = 32 * 1024;

        loop {
            let before_in = self.decompressor.total_in();
            let before_out = self.decompressor.total_out();
            let mut output = [0_u8; OUTPUT_CHUNK_BYTES];
            let state = self
                .decompressor
                .decompress(input, &mut output, flush)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let consumed = usize::try_from(self.decompressor.total_in() - before_in)
                .expect("decompressor cannot consume more than a usize input slice");
            let produced = usize::try_from(self.decompressor.total_out() - before_out)
                .expect("decompressor output is bounded by a usize output slice");
            self.inner.write_all(&output[..produced])?;
            input = &input[consumed..];

            if state == Status::StreamEnd {
                if !input.is_empty() {
                    return Err(invalid_zlib("trailing bytes after zlib member"));
                }
                self.complete = true;
                return Ok(());
            }
            if consumed == 0 && produced == 0 {
                if input.is_empty() {
                    return Ok(());
                }
                return Err(invalid_zlib("zlib decoder made no progress"));
            }
        }
    }

    fn finish(mut self) -> io::Result<W> {
        if !self.complete {
            self.pump(&[], FlushDecompress::Finish)?;
        }
        if !self.complete {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated zlib member",
            ));
        }
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for StrictZlibDecoder<W> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if input.is_empty() {
            return Ok(0);
        }
        if self.complete {
            return Err(invalid_zlib("trailing bytes after zlib member"));
        }
        self.pump(input, FlushDecompress::None)?;
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn invalid_zlib(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[derive(Debug)]
struct LimitedTempFile {
    file: StagedTempFile,
    bytes_written: u64,
    limit: u64,
}

impl LimitedTempFile {
    fn new(file: StagedTempFile, limit: u64) -> Self {
        Self {
            file,
            bytes_written: 0,
            limit,
        }
    }
}

impl Write for LimitedTempFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let remaining = self.limit.saturating_sub(self.bytes_written);
        if remaining == 0 {
            return Err(io::Error::other(OutputLimitExceeded));
        }
        let allowed = usize::try_from(remaining.min(buffer.len() as u64))
            .expect("allowed write is bounded by a usize buffer length");
        let written = self.file.write(&buffer[..allowed])?;
        self.bytes_written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[derive(Debug)]
struct StagedTempFile {
    parent: fs::File,
    source_directory: fs::File,
    staging_directory_name: Option<OsString>,
    temporary_name: OsString,
    destination_name: OsString,
    file: Option<fs::File>,
    committed: bool,
}

impl StagedTempFile {
    fn new(destination: &Path) -> io::Result<Self> {
        let parent_path = destination
            .parent()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing parent"))?;
        let destination_name = destination
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing file name"))?
            .to_os_string();
        let parent = open_parent_directory(parent_path)?;
        let staged = create_staged_file(parent_path, &parent)?;

        Ok(Self {
            parent,
            source_directory: staged.source_directory,
            staging_directory_name: staged.staging_directory_name,
            temporary_name: staged.temporary_name,
            destination_name,
            file: Some(staged.file),
            committed: false,
        })
    }

    fn as_file(&self) -> &fs::File {
        self.file.as_ref().expect("staged file handle is present")
    }

    fn commit(mut self) -> io::Result<()> {
        verify_staged_file(self.as_file(), &self.source_directory, &self.temporary_name)?;
        prepare_destination_security(self.as_file(), &self.parent)?;
        replace_staged_file(
            self.as_file(),
            &self.source_directory,
            &self.parent,
            &self.temporary_name,
            &self.destination_name,
        )?;
        self.committed = true;
        let _ = self.remove_staging_directory();
        sync_parent_directory(&self.parent)
    }

    fn remove_staging_directory(&mut self) -> io::Result<()> {
        let Some(name) = self.staging_directory_name.as_ref() else {
            return Ok(());
        };
        remove_empty_staging_directory(&self.parent, &self.source_directory, name)?;
        self.staging_directory_name = None;
        Ok(())
    }
}

impl Write for StagedTempFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file
            .as_mut()
            .expect("staged file handle is present")
            .write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file
            .as_mut()
            .expect("staged file handle is present")
            .flush()
    }
}

impl Drop for StagedTempFile {
    fn drop(&mut self) {
        if !self.committed {
            remove_staged_file(&mut self.file, &self.source_directory, &self.temporary_name);
        }
        let _ = self.remove_staging_directory();
    }
}

#[derive(Debug)]
struct StagedFileContext {
    source_directory: fs::File,
    staging_directory_name: Option<OsString>,
    temporary_name: OsString,
    file: fs::File,
}

#[cfg(unix)]
fn create_relative_file(parent: &fs::File, name: &OsStr) -> io::Result<fs::File> {
    use fs_at::os::unix::OpenOptionsExt;

    let mut options = OpenOptionsAt::default();
    options
        .write(OpenOptionsWriteMode::Write)
        .create_new(true)
        .follow(false)
        .mode(0o600);
    options.open_at(parent, name)
}

#[cfg(windows)]
fn windows_handle(file: &fs::File) -> winapi::shared::ntdef::HANDLE {
    use std::os::windows::io::AsRawHandle;

    file.as_raw_handle().cast()
}

#[cfg(windows)]
fn create_relative_file(parent: &fs::File, name: &OsStr) -> io::Result<fs::File> {
    use fs_at::os::windows::OpenOptionsExt;
    use std::os::windows::io::FromRawHandle;
    use winapi::um::handleapi::INVALID_HANDLE_VALUE;
    use winapi::um::winbase::ReOpenFile;
    use winapi::um::winnt::{
        DELETE, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, WRITE_DAC,
    };

    let security = WindowsPrivateSecurityDescriptor::new()?;
    let mut options = OpenOptionsAt::default();
    options
        .write(OpenOptionsWriteMode::Write)
        .create_new(true)
        .follow(false)
        // fs_at currently shares every handle. Create with only SYNCHRONIZE,
        // then reopen with the complete access mask and only delete sharing
        // while the protected owner-only DACL makes that transition private.
        .desired_access(0)
        .security_descriptor(security.descriptor);
    let shared_file = options.open_at(parent, name)?;
    // SAFETY: shared_file is a live synchronous file handle. The requested
    // rights are granted by the owner-only creation DACL. Delete sharing is
    // compatible with the original handle and required for handle-relative
    // rename; read and write sharing remain disabled. The returned handle is
    // independently owned.
    let exclusive = unsafe {
        ReOpenFile(
            windows_handle(&shared_file),
            FILE_GENERIC_WRITE | FILE_READ_ATTRIBUTES | WRITE_DAC | DELETE,
            FILE_SHARE_DELETE,
            0,
        )
    };
    if exclusive == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        drop(shared_file);
        let _ = OpenOptionsAt::default().unlink_at(parent, name);
        return Err(error);
    }
    drop(shared_file);
    // SAFETY: ReOpenFile returned a new owned handle not managed elsewhere.
    Ok(unsafe { fs::File::from_raw_handle(exclusive.cast()) })
}

#[cfg(not(any(unix, windows)))]
fn create_relative_file(_parent: &fs::File, _name: &OsStr) -> io::Result<fs::File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "handle-relative staging is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn create_staged_file(parent_path: &Path, parent: &fs::File) -> io::Result<StagedFileContext> {
    use fs_at::os::unix::OpenOptionsExt;

    validate_shared_parent(parent)?;

    // The private directory prevents another OS user from substituting the
    // staged source while preserving a same-filesystem atomic rename.
    let mut builder = Builder::new();
    builder
        .prefix(".cterm-transfer-")
        .rand_bytes(16)
        .disable_cleanup(true);
    let temporary_directory = builder.make_in(parent_path, |candidate| {
        let name = candidate
            .file_name()
            .expect("tempfile builder always supplies a file name");
        let mut options = OpenOptionsAt::default();
        options.create_new(true).mode(0o700);
        options.mkdir_at(parent, name)
    })?;
    let staging_directory_name = temporary_directory
        .path()
        .file_name()
        .expect("tempfile builder always supplies a file name")
        .to_os_string();
    let source_directory = temporary_directory.into_file();
    let temporary_name = OsString::from("payload");
    let file = match create_relative_file(&source_directory, &temporary_name) {
        Ok(file) => file,
        Err(error) => {
            let _ = OpenOptionsAt::default().rmdir_at(parent, &staging_directory_name);
            return Err(error);
        }
    };

    Ok(StagedFileContext {
        source_directory,
        staging_directory_name: Some(staging_directory_name),
        temporary_name,
        file,
    })
}

#[cfg(windows)]
fn create_staged_file(parent_path: &Path, parent: &fs::File) -> io::Result<StagedFileContext> {
    use fs_at::os::windows::OpenOptionsExt;

    // Create the empty directory atomically with an owner-only, protected DACL
    // before placing any transfer bytes inside it.
    let security = WindowsPrivateSecurityDescriptor::new()?;
    let mut builder = Builder::new();
    builder
        .prefix(".cterm-transfer-")
        .rand_bytes(16)
        .disable_cleanup(true);
    let temporary_directory = builder.make_in(parent_path, |candidate| {
        let name = candidate
            .file_name()
            .expect("tempfile builder always supplies a file name");
        let mut options = OpenOptionsAt::default();
        options
            .create_new(true)
            .security_descriptor(security.descriptor);
        options.mkdir_at(parent, name)
    })?;
    let staging_directory_name = temporary_directory
        .path()
        .file_name()
        .expect("tempfile builder always supplies a file name")
        .to_os_string();
    let source_directory = temporary_directory.into_file();
    let temporary_name = OsString::from("payload");
    let file = match create_relative_file(&source_directory, &temporary_name) {
        Ok(file) => file,
        Err(error) => {
            let _ =
                remove_empty_staging_directory(parent, &source_directory, &staging_directory_name);
            return Err(error);
        }
    };

    Ok(StagedFileContext {
        source_directory,
        staging_directory_name: Some(staging_directory_name),
        temporary_name,
        file,
    })
}

#[cfg(not(any(unix, windows)))]
fn create_staged_file(_parent_path: &Path, _parent: &fs::File) -> io::Result<StagedFileContext> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "handle-relative staging is unsupported on this platform",
    ))
}

#[cfg(windows)]
struct WindowsPrivateSecurityDescriptor {
    descriptor: fs_at::os::windows::SECURITY_DESCRIPTOR,
    _user_storage: Vec<usize>,
    _acl_storage: Vec<usize>,
}

#[cfg(windows)]
struct WindowsTokenHandle(winapi::shared::ntdef::HANDLE);

#[cfg(windows)]
impl Drop for WindowsTokenHandle {
    fn drop(&mut self) {
        // SAFETY: OpenProcessToken returned this owned handle.
        unsafe { winapi::um::handleapi::CloseHandle(self.0) };
    }
}

#[cfg(windows)]
fn current_process_token() -> io::Result<WindowsTokenHandle> {
    use winapi::um::processthreadsapi::{GetCurrentProcess, OpenProcessToken};
    use winapi::um::winnt::TOKEN_QUERY;

    let mut raw_token = std::ptr::null_mut();
    // SAFETY: raw_token is a valid out pointer and GetCurrentProcess supplies
    // a pseudo-handle valid for OpenProcessToken.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(WindowsTokenHandle(raw_token))
    }
}

#[cfg(windows)]
impl WindowsPrivateSecurityDescriptor {
    fn new() -> io::Result<Self> {
        use std::mem::size_of;
        use winapi::shared::minwindef::{DWORD, FALSE, TRUE};
        use winapi::um::securitybaseapi::{
            AddAccessAllowedAceEx, GetLengthSid, GetTokenInformation, InitializeAcl,
            InitializeSecurityDescriptor, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
            SetSecurityDescriptorOwner,
        };
        use winapi::um::winnt::{
            TokenUser, ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, FILE_ALL_ACCESS,
            SECURITY_DESCRIPTOR_REVISION, SE_DACL_PROTECTED, TOKEN_USER,
        };

        let token = current_process_token()?;
        let mut required = 0_u32;
        // SAFETY: this call intentionally queries the required buffer size.
        unsafe {
            GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut required);
        }
        if required < size_of::<TOKEN_USER>() as u32 {
            return Err(io::Error::last_os_error());
        }
        let words = (required as usize).div_ceil(size_of::<usize>());
        let mut user_storage = vec![0_usize; words];
        // SAFETY: usize storage is aligned and contains `required` writable
        // bytes for the TOKEN_USER and its SID.
        if unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                user_storage.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: GetTokenInformation initialized TOKEN_USER in user_storage.
        let user = unsafe { &*user_storage.as_ptr().cast::<TOKEN_USER>() };
        if user.User.Sid.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "current process token has no user SID",
            ));
        }
        // SAFETY: the SID belongs to the live user_storage allocation.
        let sid_bytes = unsafe { GetLengthSid(user.User.Sid) } as usize;
        if sid_bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let acl_bytes = size_of::<ACL>()
            .checked_add(size_of::<ACCESS_ALLOWED_ACE>() - size_of::<DWORD>())
            .and_then(|bytes| bytes.checked_add(sid_bytes))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "user SID is too large"))?;
        let acl_size = DWORD::try_from(acl_bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "user SID is too large"))?;
        let acl_words = acl_bytes.div_ceil(size_of::<usize>());
        let mut acl_storage = vec![0_usize; acl_words];
        let acl = acl_storage.as_mut_ptr().cast::<ACL>();
        // SAFETY: acl_storage is aligned and contains acl_size writable bytes.
        if unsafe { InitializeAcl(acl, acl_size, ACL_REVISION as DWORD) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the initialized ACL has capacity for exactly this ACE and the
        // current-user SID remains live in user_storage.
        if unsafe {
            AddAccessAllowedAceEx(
                acl,
                ACL_REVISION as DWORD,
                0,
                FILE_ALL_ACCESS,
                user.User.Sid,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }

        let mut descriptor = fs_at::os::windows::SECURITY_DESCRIPTOR {
            Revision: 0,
            Sbz1: 0,
            Control: 0,
            Owner: std::ptr::null_mut(),
            Group: std::ptr::null_mut(),
            Sacl: std::ptr::null_mut(),
            Dacl: std::ptr::null_mut(),
        };
        // SAFETY: descriptor is a writable absolute security descriptor; its
        // owner SID and ACL allocations outlive every use of the descriptor.
        if unsafe {
            InitializeSecurityDescriptor(
                (&mut descriptor as *mut fs_at::os::windows::SECURITY_DESCRIPTOR).cast(),
                SECURITY_DESCRIPTOR_REVISION,
            )
        } == 0
            || unsafe {
                SetSecurityDescriptorOwner(
                    (&mut descriptor as *mut fs_at::os::windows::SECURITY_DESCRIPTOR).cast(),
                    user.User.Sid,
                    FALSE,
                )
            } == 0
            || unsafe {
                SetSecurityDescriptorDacl(
                    (&mut descriptor as *mut fs_at::os::windows::SECURITY_DESCRIPTOR).cast(),
                    TRUE,
                    acl,
                    FALSE,
                )
            } == 0
            || unsafe {
                SetSecurityDescriptorControl(
                    (&mut descriptor as *mut fs_at::os::windows::SECURITY_DESCRIPTOR).cast(),
                    SE_DACL_PROTECTED,
                    SE_DACL_PROTECTED,
                )
            } == 0
        {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            descriptor,
            _user_storage: user_storage,
            _acl_storage: acl_storage,
        })
    }
}

#[cfg(unix)]
fn open_parent_directory(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(windows)]
fn open_parent_directory(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use winapi::um::winbase::FILE_FLAG_BACKUP_SEMANTICS;
    use winapi::um::winnt::{
        FILE_ADD_FILE, FILE_GENERIC_READ, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        // FILE_RENAME_INFORMATION resolves the final name relative to this retained
        // handle, and Windows requires FILE_ADD_FILE on that target directory.
        .access_mode(FILE_GENERIC_READ | FILE_ADD_FILE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_parent_directory(_path: &Path) -> io::Result<fs::File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "handle-relative staging is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn validate_shared_parent(parent: &fs::File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    const STICKY_BIT: u32 = 0o1000;
    let mode = parent.metadata()?.mode();
    if mode & 0o022 != 0 && mode & STICKY_BIT == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "group/world-writable destination directory lacks the sticky bit",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn verify_staged_file(
    file: &fs::File,
    source_directory: &fs::File,
    temporary_name: &OsStr,
) -> io::Result<()> {
    use nix::fcntl::AtFlags;
    use nix::sys::stat::{fstat, fstatat};

    // `fstatat` inspects the directory entry without opening the payload, so
    // protocol-requested modes such as 0000 cannot break identity checking.
    let expected = fstat(file).map_err(io::Error::from)?;
    let named = fstatat(
        source_directory,
        temporary_name,
        AtFlags::AT_SYMLINK_NOFOLLOW,
    )
    .map_err(io::Error::from)?;
    if expected.st_dev != named.st_dev || expected.st_ino != named.st_ino {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "staged file entry was replaced",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_staged_file(
    _file: &fs::File,
    _source_directory: &fs::File,
    _temporary_name: &OsStr,
) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn remove_staged_file(
    file: &mut Option<fs::File>,
    source_directory: &fs::File,
    temporary_name: &OsStr,
) {
    let matches = file
        .as_ref()
        .is_some_and(|file| verify_staged_file(file, source_directory, temporary_name).is_ok());
    if matches {
        let _ = OpenOptionsAt::default().unlink_at(source_directory, temporary_name);
    }
    *file = None;
}

#[cfg(windows)]
fn remove_staged_file(
    file: &mut Option<fs::File>,
    _source_directory: &fs::File,
    _temporary_name: &OsStr,
) {
    use fs_at::os::windows::FileExt;

    if let Some(file) = file.take() {
        let _ = file.delete_by_handle();
    }
}

#[cfg(not(any(unix, windows)))]
fn remove_staged_file(
    file: &mut Option<fs::File>,
    _source_directory: &fs::File,
    _temporary_name: &OsStr,
) {
    *file = None;
}

#[cfg(unix)]
fn remove_empty_staging_directory(
    parent: &fs::File,
    _source_directory: &fs::File,
    name: &OsStr,
) -> io::Result<()> {
    OpenOptionsAt::default().rmdir_at(parent, name)
}

#[cfg(windows)]
fn remove_empty_staging_directory(
    _parent: &fs::File,
    source_directory: &fs::File,
    _name: &OsStr,
) -> io::Result<()> {
    use fs_at::os::windows::FileExt;

    source_directory
        .try_clone()?
        .delete_by_handle()
        .map_err(|(_, error)| error)
}

#[cfg(not(any(unix, windows)))]
fn remove_empty_staging_directory(
    _parent: &fs::File,
    _source_directory: &fs::File,
    _name: &OsStr,
) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn replace_staged_file(
    _file: &fs::File,
    source_directory: &fs::File,
    parent: &fs::File,
    temporary_name: &OsStr,
    destination_name: &OsStr,
) -> io::Result<()> {
    nix::fcntl::renameat(source_directory, temporary_name, parent, destination_name)
        .map_err(io::Error::from)
}

#[cfg(windows)]
fn replace_staged_file(
    file: &fs::File,
    _source_directory: &fs::File,
    parent: &fs::File,
    _temporary_name: &OsStr,
    destination_name: &OsStr,
) -> io::Result<()> {
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use windows_sys::Wdk::Storage::FileSystem::{
        FileRenameInformation, NtSetInformationFile, FILE_RENAME_INFORMATION,
    };
    use windows_sys::Win32::Foundation::RtlNtStatusToDosError;
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    let name: Vec<u16> = destination_name.encode_wide().collect();
    let name_bytes = name
        .len()
        .checked_mul(size_of::<u16>())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file name is too long"))?;
    let buffer_bytes = size_of::<FILE_RENAME_INFORMATION>()
        .checked_add(name_bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file name is too long"))?;
    let buffer_size = u32::try_from(buffer_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file name is too long"))?;
    let words = buffer_bytes.div_ceil(size_of::<usize>());
    let mut storage = vec![0_usize; words];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();

    // Adapted from OpenVMM's tested handle-relative Windows rename. `storage`
    // is pointer-aligned and sized for the fixed structure plus the complete
    // UTF-16 component. Both handles stay open for the call, and the source
    // handle was opened with DELETE access.
    let status = unsafe {
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).RootDirectory = windows_handle(parent).cast();
        (*info).FileNameLength = name_bytes as u32;
        ptr::copy_nonoverlapping(
            name.as_ptr(),
            ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name.len(),
        );
        let mut io_status = IO_STATUS_BLOCK::default();
        NtSetInformationFile(
            windows_handle(file).cast(),
            &mut io_status,
            info.cast(),
            buffer_size,
            FileRenameInformation,
        )
    };
    if status < 0 {
        // SAFETY: the status came directly from `NtSetInformationFile`.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn replace_staged_file(
    _file: &fs::File,
    _source_directory: &fs::File,
    _parent: &fs::File,
    _temporary_name: &OsStr,
    _destination_name: &OsStr,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "handle-relative commit is unsupported on this platform",
    ))
}

#[cfg(windows)]
fn prepare_destination_security(file: &fs::File, parent: &fs::File) -> io::Result<()> {
    use winapi::shared::minwindef::{FALSE, TRUE};
    use winapi::um::accctrl::SE_FILE_OBJECT;
    use winapi::um::aclapi::{GetSecurityInfo, SetSecurityInfo};
    use winapi::um::securitybaseapi::{
        CreatePrivateObjectSecurityEx, DestroyPrivateObjectSecurity, GetSecurityDescriptorDacl,
    };
    use winapi::um::winbase::LocalFree;
    use winapi::um::winnt::{
        DACL_SECURITY_INFORMATION, FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, GENERIC_MAPPING, GROUP_SECURITY_INFORMATION,
        OWNER_SECURITY_INFORMATION, PACL, PSECURITY_DESCRIPTOR, SEF_DACL_AUTO_INHERIT,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };

    struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);
    impl Drop for LocalSecurityDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: GetSecurityInfo allocated this descriptor with
                // LocalAlloc.
                unsafe { LocalFree(self.0.cast::<winapi::ctypes::c_void>()) };
            }
        }
    }

    struct PrivateSecurityDescriptor(PSECURITY_DESCRIPTOR);
    impl Drop for PrivateSecurityDescriptor {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: CreatePrivateObjectSecurityEx allocated this
                // descriptor, and it has not otherwise been released.
                unsafe { DestroyPrivateObjectSecurity(&mut self.0) };
            }
        }
    }

    let mut raw_parent_descriptor = std::ptr::null_mut();
    // SAFETY: parent is a live directory handle with READ_CONTROL from its
    // generic read access, and the descriptor output pointer is valid.
    let status = unsafe {
        GetSecurityInfo(
            windows_handle(parent),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut raw_parent_descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let parent_descriptor = LocalSecurityDescriptor(raw_parent_descriptor);
    let token = current_process_token()?;
    let mut mapping = GENERIC_MAPPING {
        GenericRead: FILE_GENERIC_READ,
        GenericWrite: FILE_GENERIC_WRITE,
        GenericExecute: FILE_GENERIC_EXECUTE,
        GenericAll: FILE_ALL_ACCESS,
    };
    let mut raw_child_descriptor = std::ptr::null_mut();
    // SAFETY: the parent descriptor and token are live, the output pointer and
    // file generic mapping are valid, and a null creator descriptor requests
    // the same token defaults plus inherited ACL entries as a new child file.
    if unsafe {
        CreatePrivateObjectSecurityEx(
            parent_descriptor.0,
            std::ptr::null_mut(),
            &mut raw_child_descriptor,
            std::ptr::null_mut(),
            FALSE,
            SEF_DACL_AUTO_INHERIT,
            token.0,
            &mut mapping,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let child_descriptor = PrivateSecurityDescriptor(raw_child_descriptor);
    let mut dacl_present = FALSE;
    let mut dacl_defaulted = FALSE;
    let mut dacl: PACL = std::ptr::null_mut();
    // SAFETY: child_descriptor owns a live security descriptor and all output
    // pointers are valid for the duration of the call.
    if unsafe {
        GetSecurityDescriptorDacl(
            child_descriptor.0,
            &mut dacl_present,
            &mut dacl,
            &mut dacl_defaulted,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if dacl_present != TRUE || dacl.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "destination parent produced no bounded child DACL",
        ));
    }

    // Apply the destination parent's derived child DACL while the payload is
    // still private. If this fails, the old destination has not been replaced
    // and Drop removes only the staging entry.
    let status = unsafe {
        SetSecurityInfo(
            windows_handle(file),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null_mut(),
        )
    };
    // Destroy before reporting SetSecurityInfo so the DACL pointer remains
    // live through the synchronous call.
    drop(child_descriptor);
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status as i32))
    }
}

#[cfg(not(windows))]
fn prepare_destination_security(_file: &fs::File, _parent: &fs::File) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(parent: &fs::File) -> io::Result<()> {
    parent.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &fs::File) -> io::Result<()> {
    Ok(())
}

#[derive(Debug, Error)]
#[error("decompressed output exceeds configured transfer limit")]
struct OutputLimitExceeded;

fn stage_write_status(error: &io::Error) -> &'static str {
    if error
        .get_ref()
        .and_then(|source| source.downcast_ref::<OutputLimitExceeded>())
        .is_some()
    {
        "EFBIG:File exceeds configured size limit"
    } else if matches!(
        error.kind(),
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
    ) {
        "EINVAL:Invalid zlib stream"
    } else {
        "EIO:Could not write staged file"
    }
}

fn response(
    request: &FileTransferCommand,
    status: &str,
    quiet: u8,
    is_error: bool,
    size: Option<u64>,
) -> Vec<Vec<u8>> {
    if quiet >= 2 || quiet == 1 && !is_error {
        return Vec::new();
    }
    let command = FileTransferCommand {
        action: FileTransferAction::Status,
        id: request.id.clone(),
        file_id: request.file_id.clone(),
        bypass: None,
        quiet: 0,
        mtime: None,
        permissions: None,
        size,
        name: None,
        status: Some(status.to_string()),
        parent: None,
        data: Vec::new(),
        compression: None,
        file_type: None,
        transmission_type: None,
    };
    command.encode().ok().into_iter().collect()
}

fn io_status(error: &io::Error, fallback: &'static str) -> &'static str {
    match error.kind() {
        io::ErrorKind::NotFound => "ENOENT:Destination does not exist",
        io::ErrorKind::PermissionDenied => "EPERM:Permission denied",
        io::ErrorKind::AlreadyExists => "EEXIST:Destination already exists",
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => {
            "EINVAL:Invalid filesystem operation"
        }
        _ => fallback,
    }
}

fn apply_metadata(file: &fs::File, permissions: Option<u32>, mtime: Option<i64>) -> io::Result<()> {
    if let Some(permissions) = permissions {
        apply_permissions(file, permissions)?;
    }
    if let Some(nanoseconds) = mtime {
        let seconds = nanoseconds.div_euclid(1_000_000_000);
        let nanos = nanoseconds.rem_euclid(1_000_000_000) as u32;
        set_file_handle_times(file, None, Some(FileTime::from_unix_time(seconds, nanos)))?;
    }
    Ok(())
}

#[cfg(unix)]
fn apply_permissions(file: &fs::File, permissions: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(permissions & 0o7777))
}

#[cfg(windows)]
fn apply_permissions(file: &fs::File, permissions: u32) -> io::Result<()> {
    let mut local = file.metadata()?.permissions();
    local.set_readonly(permissions & 0o200 == 0);
    file.set_permissions(local)
}

#[cfg(not(any(unix, windows)))]
fn apply_permissions(_file: &fs::File, _permissions: u32) -> io::Result<()> {
    Ok(())
}

fn resolve_protocol_path(home: &Path, path: &str) -> Option<PathBuf> {
    // OSC 5113 paths always use POSIX separators, including on Windows.
    // Rejecting backslashes also prevents a component from becoming an
    // unexpected Windows path prefix when it is appended below.
    if path.contains('\\') {
        return None;
    }
    if let Some(relative) = path.strip_prefix("~/") {
        return append_normal_components(home.to_path_buf(), Path::new(relative));
    }
    resolve_absolute_protocol_path(path)
}

#[cfg(not(windows))]
fn resolve_absolute_protocol_path(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return None;
    }
    let mut destination = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => destination.push(Path::new("/")),
            Component::Normal(component) => destination.push(component),
            Component::Prefix(_) | Component::CurDir | Component::ParentDir => return None,
        }
    }
    Some(destination)
}

#[cfg(windows)]
fn resolve_absolute_protocol_path(path: &str) -> Option<PathBuf> {
    let bytes = path.as_bytes();
    if bytes.len() < 3
        || bytes[0] != b'/'
        || !bytes[1].is_ascii_alphabetic()
        || bytes[2] != b':'
        || bytes.get(3).is_some_and(|separator| *separator != b'/')
    {
        return None;
    }
    let mut destination = PathBuf::from(format!("{}:\\", bytes[1] as char));
    for component in path[3..].split('/') {
        if component.is_empty() {
            continue;
        }
        if !valid_windows_component(component) {
            return None;
        }
        destination.push(component);
    }
    Some(destination)
}

fn append_normal_components(mut destination: PathBuf, relative: &Path) -> Option<PathBuf> {
    for component in relative.components() {
        match component {
            Component::Normal(component) => {
                #[cfg(windows)]
                if !valid_windows_component(component.to_str()?) {
                    return None;
                }
                destination.push(component);
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => return None,
        }
    }
    Some(destination)
}

#[cfg(windows)]
fn valid_windows_component(component: &str) -> bool {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.ends_with('.')
        || component.ends_with(' ')
        || component.chars().any(|character| {
            character <= '\u{1f}'
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        })
    {
        return false;
    }
    let stem = component.split('.').next().unwrap_or(component);
    !matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "COM¹"
            | "COM²"
            | "COM³"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
            | "LPT¹"
            | "LPT²"
            | "LPT³"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty_file_transfer::{TtyTransferApprovalRequest, TtyTransferManager};
    use flate2::{write::ZlibEncoder, Compression};

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

    fn limits(max_file_bytes: u64) -> TtyTransferLimits {
        TtyTransferLimits::new(8, max_file_bytes, max_file_bytes * 4).unwrap()
    }

    fn approve_send(manager: &mut TtyTransferManager, session_id: &str) {
        let actions = manager.handle(command(FileTransferAction::Send, session_id));
        let [TtyTransferAction::RequestApproval(TtyTransferApprovalRequest { request_id, .. })] =
            actions.as_slice()
        else {
            panic!("expected an approval request")
        };
        manager.approve(*request_id);
    }

    fn run(
        manager: &mut TtyTransferManager,
        filesystem: &mut TtyTransferSendFilesystem,
        command: FileTransferCommand,
    ) -> Vec<TtyTransferAction> {
        manager
            .handle(command)
            .into_iter()
            .flat_map(|action| filesystem.handle_action(action))
            .collect()
    }

    fn status(actions: &[TtyTransferAction]) -> FileTransferCommand {
        let [TtyTransferAction::Write(encoded)] = actions else {
            panic!("expected one status response, got {actions:?}")
        };
        let body = encoded
            .strip_prefix(b"\x1b]5113;")
            .and_then(|body| body.strip_suffix(b"\x1b\\"))
            .expect("encoded OSC 5113 response");
        let mut fields = vec![&b"5113"[..]];
        fields.extend(body.split(|byte| *byte == b';'));
        cterm_core::parse_file_transfer_command(&fields).expect("valid response")
    }

    fn zlib_bytes(contents: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(contents).unwrap();
        encoder.finish().unwrap()
    }

    fn start_file(
        manager: &mut TtyTransferManager,
        filesystem: &mut TtyTransferSendFilesystem,
        session_id: &str,
        name: &str,
        size: u64,
    ) -> Vec<TtyTransferAction> {
        let mut file = command(FileTransferAction::File, session_id);
        file.file_id = Some("f1".into());
        file.name = Some(name.into());
        file.size = Some(size);
        run(manager, filesystem, file)
    }

    #[test]
    fn simple_file_is_invisible_until_finish_then_atomically_replaces_destination() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("received.txt");
        fs::write(&destination, b"old").unwrap();
        let mut manager = TtyTransferManager::new();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits(1024)).unwrap();
        approve_send(&mut manager, "simple");

        let started = start_file(&mut manager, &mut filesystem, "simple", "~/received.txt", 6);
        assert_eq!(status(&started).status.as_deref(), Some("STARTED"));

        let mut data = command(FileTransferAction::Data, "simple");
        data.file_id = Some("f1".into());
        data.data = b"abc".to_vec();
        let progress = run(&mut manager, &mut filesystem, data);
        assert_eq!(status(&progress).status.as_deref(), Some("PROGRESS"));
        assert_eq!(status(&progress).size, Some(3));
        assert_eq!(fs::read(&destination).unwrap(), b"old");

        let mut end = command(FileTransferAction::EndData, "simple");
        end.file_id = Some("f1".into());
        end.data = b"def".to_vec();
        let completed = run(&mut manager, &mut filesystem, end);
        assert_eq!(status(&completed).status.as_deref(), Some("OK"));
        assert_eq!(status(&completed).size, Some(6));
        assert_eq!(fs::read(&destination).unwrap(), b"old");

        assert!(run(
            &mut manager,
            &mut filesystem,
            command(FileTransferAction::Finish, "simple")
        )
        .is_empty());
        assert_eq!(fs::read(&destination).unwrap(), b"abcdef");
        assert_eq!(filesystem.active_sessions(), 0);
    }

    #[test]
    fn cancel_discards_staging_without_touching_destination() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("cancelled.txt");
        let mut manager = TtyTransferManager::new();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits(1024)).unwrap();
        approve_send(&mut manager, "cancel");
        start_file(
            &mut manager,
            &mut filesystem,
            "cancel",
            "~/cancelled.txt",
            3,
        );

        let mut data = command(FileTransferAction::Data, "cancel");
        data.file_id = Some("f1".into());
        data.data = b"abc".to_vec();
        run(&mut manager, &mut filesystem, data);
        let canceled = run(
            &mut manager,
            &mut filesystem,
            command(FileTransferAction::Cancel, "cancel"),
        );

        assert_eq!(status(&canceled).status.as_deref(), Some("CANCELED"));
        assert!(!destination.exists());
        assert_eq!(filesystem.active_sessions(), 0);
        assert!(fs::read_dir(home.path()).unwrap().next().is_none());
    }

    #[test]
    fn zlib_is_bounded_by_decompressed_size_and_preserves_metadata() {
        use std::time::{Duration, UNIX_EPOCH};

        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("compressed.txt");
        let mut manager = TtyTransferManager::new();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits(64)).unwrap();
        approve_send(&mut manager, "zlib");

        let mut file = command(FileTransferAction::File, "zlib");
        file.file_id = Some("f1".into());
        file.name = Some("~/compressed.txt".into());
        file.size = Some(11);
        file.compression = Some(FileTransferCompression::Zlib);
        file.permissions = Some(0o640);
        // Windows FILETIME has 100 ns precision, so use an exactly
        // representable value for the cross-platform assertion.
        file.mtime = Some(1_700_000_000_123_456_700);
        run(&mut manager, &mut filesystem, file);

        let mut end = command(FileTransferAction::EndData, "zlib");
        end.file_id = Some("f1".into());
        end.data = zlib_bytes(b"hello zlib!");
        assert_eq!(
            status(&run(&mut manager, &mut filesystem, end))
                .status
                .as_deref(),
            Some("OK")
        );
        run(
            &mut manager,
            &mut filesystem,
            command(FileTransferAction::Finish, "zlib"),
        );

        assert_eq!(fs::read(&destination).unwrap(), b"hello zlib!");
        let modified = destination.metadata().unwrap().modified().unwrap();
        assert_eq!(
            modified.duration_since(UNIX_EPOCH).unwrap(),
            Duration::new(1_700_000_000, 123_456_700)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                destination.metadata().unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
    }

    #[test]
    fn strict_zlib_requires_one_complete_member() {
        let compressed = zlib_bytes(b"complete payload");

        let mut split = StrictZlibDecoder::new(Vec::new());
        for byte in &compressed {
            split.write_all(std::slice::from_ref(byte)).unwrap();
        }
        split.write_all(&[]).unwrap();
        assert_eq!(split.finish().unwrap(), b"complete payload");

        let empty = StrictZlibDecoder::new(Vec::new());
        assert_eq!(
            empty.finish().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let mut encoded_empty = StrictZlibDecoder::new(Vec::new());
        encoded_empty.write_all(&zlib_bytes(b"")).unwrap();
        assert!(encoded_empty.finish().unwrap().is_empty());

        for removed in 1..=4 {
            let mut truncated = StrictZlibDecoder::new(Vec::new());
            truncated
                .write_all(&compressed[..compressed.len() - removed])
                .unwrap();
            assert!(matches!(
                truncated.finish().unwrap_err().kind(),
                io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
            ));
        }

        for trailing in [b"junk".to_vec(), compressed.clone()] {
            let mut bytes = compressed.clone();
            bytes.extend_from_slice(&trailing);
            let mut decoder = StrictZlibDecoder::new(Vec::new());
            assert_eq!(
                decoder.write_all(&bytes).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        let mut later_trailing = StrictZlibDecoder::new(Vec::new());
        later_trailing.write_all(&compressed).unwrap();
        assert_eq!(
            later_trailing.write_all(b"junk").unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        for end in [1, compressed.len() / 2] {
            let mut decoder = StrictZlibDecoder::new(Vec::new());
            let write_result = decoder.write_all(&compressed[..end]);
            assert!(write_result.is_err() || decoder.finish().is_err());
        }

        let mut corrupt = compressed;
        corrupt[0] ^= 0xff;
        let mut decoder = StrictZlibDecoder::new(Vec::new());
        assert_eq!(
            decoder.write_all(&corrupt).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn declared_and_decompressed_limits_fail_closed() {
        let home = tempfile::tempdir().unwrap();
        let mut manager = TtyTransferManager::new();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits(8)).unwrap();
        approve_send(&mut manager, "declared");
        let too_large = start_file(&mut manager, &mut filesystem, "declared", "~/too-large", 9);
        assert!(status(&too_large)
            .status
            .as_deref()
            .is_some_and(|value| value.starts_with("EFBIG:")));
        assert!(!home.path().join("too-large").exists());

        approve_send(&mut manager, "compressed-limit");
        let mut file = command(FileTransferAction::File, "compressed-limit");
        file.file_id = Some("f1".into());
        file.name = Some("~/bomb".into());
        file.compression = Some(FileTransferCompression::Zlib);
        run(&mut manager, &mut filesystem, file);
        let mut end = command(FileTransferAction::EndData, "compressed-limit");
        end.file_id = Some("f1".into());
        end.data = zlib_bytes(&[b'x'; 32]);
        let rejected = run(&mut manager, &mut filesystem, end);
        assert!(status(&rejected)
            .status
            .as_deref()
            .is_some_and(|value| value.starts_with("EFBIG:")));
        run(
            &mut manager,
            &mut filesystem,
            command(FileTransferAction::Finish, "compressed-limit"),
        );
        assert!(!home.path().join("bomb").exists());
    }

    #[test]
    fn zlib_output_exactly_at_the_limit_succeeds() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("exact-limit");
        let mut manager = TtyTransferManager::new();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits(8)).unwrap();
        approve_send(&mut manager, "exact-limit");

        let mut file = command(FileTransferAction::File, "exact-limit");
        file.file_id = Some("f1".into());
        file.name = Some("~/exact-limit".into());
        file.compression = Some(FileTransferCompression::Zlib);
        run(&mut manager, &mut filesystem, file);
        let mut end = command(FileTransferAction::EndData, "exact-limit");
        end.file_id = Some("f1".into());
        end.data = zlib_bytes(b"12345678");
        assert_eq!(
            status(&run(&mut manager, &mut filesystem, end))
                .status
                .as_deref(),
            Some("OK")
        );
        assert!(run(
            &mut manager,
            &mut filesystem,
            command(FileTransferAction::Finish, "exact-limit")
        )
        .is_empty());
        assert_eq!(fs::read(destination).unwrap(), b"12345678");
    }

    #[test]
    fn truncated_zlib_never_reaches_the_destination() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("truncated.txt");
        let mut manager = TtyTransferManager::new();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits(1024)).unwrap();
        approve_send(&mut manager, "truncated");

        let mut file = command(FileTransferAction::File, "truncated");
        file.file_id = Some("f1".into());
        file.name = Some("~/truncated.txt".into());
        file.compression = Some(FileTransferCompression::Zlib);
        run(&mut manager, &mut filesystem, file);

        let mut compressed = zlib_bytes(b"all output is already available");
        compressed.truncate(compressed.len() - 1);
        let mut end = command(FileTransferAction::EndData, "truncated");
        end.file_id = Some("f1".into());
        end.data = compressed;
        assert!(status(&run(&mut manager, &mut filesystem, end))
            .status
            .as_deref()
            .is_some_and(|value| value.starts_with("EINVAL:")));

        run(
            &mut manager,
            &mut filesystem,
            command(FileTransferAction::Finish, "truncated"),
        );
        assert!(!destination.exists());
    }

    #[test]
    fn unsafe_relative_components_and_unsupported_types_never_stage() {
        let home = tempfile::tempdir().unwrap();
        let mut manager = TtyTransferManager::new();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits(1024)).unwrap();
        approve_send(&mut manager, "paths");
        let rejected = start_file(&mut manager, &mut filesystem, "paths", "~/../escape", 1);
        assert!(status(&rejected)
            .status
            .as_deref()
            .is_some_and(|value| value.starts_with("EINVAL:")));

        let mut backslash = command(FileTransferAction::File, "paths");
        backslash.file_id = Some("f-backslash".into());
        backslash.name = Some("~/safe/..\\escape".into());
        let rejected = run(&mut manager, &mut filesystem, backslash);
        assert!(status(&rejected)
            .status
            .as_deref()
            .is_some_and(|value| value.starts_with("EINVAL:")));

        let mut directory = command(FileTransferAction::File, "paths");
        directory.file_id = Some("f2".into());
        directory.name = Some("~/directory".into());
        directory.file_type = Some(FileTransferType::Directory);
        let unsupported = run(&mut manager, &mut filesystem, directory);
        assert!(status(&unsupported)
            .status
            .as_deref()
            .is_some_and(|value| value.starts_with("ENOTSUP:")));
        assert!(fs::read_dir(home.path()).unwrap().next().is_none());
    }

    #[test]
    fn file_limit_rejections_do_not_grow_unbounded_state() {
        let home = tempfile::tempdir().unwrap();
        let mut manager = TtyTransferManager::new();
        let limits = TtyTransferLimits::new(1, 1024, 1024).unwrap();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits).unwrap();
        approve_send(&mut manager, "bounded-files");
        assert_eq!(
            status(&start_file(
                &mut manager,
                &mut filesystem,
                "bounded-files",
                "~/first",
                1,
            ))
            .status
            .as_deref(),
            Some("STARTED")
        );

        for index in 0..128 {
            let mut file = command(FileTransferAction::File, "bounded-files");
            file.file_id = Some(format!("rejected-{index}"));
            file.name = Some(format!("~/rejected-{index}"));
            file.size = Some(1);
            assert!(status(&run(&mut manager, &mut filesystem, file))
                .status
                .as_deref()
                .is_some_and(|value| value.starts_with("ENOSPC:")));
        }

        let session = filesystem.sessions.get("bounded-files").unwrap();
        assert_eq!(session.files.len(), 1);
        assert!(session.rejected.is_empty());
    }

    #[test]
    fn parent_rename_cannot_redirect_staging_or_commit() {
        let home = tempfile::tempdir().unwrap();
        let original_parent = home.path().join("destination");
        let retained_parent = home.path().join("retained");
        fs::create_dir(&original_parent).unwrap();
        let mut manager = TtyTransferManager::new();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits(1024)).unwrap();
        approve_send(&mut manager, "parent-race");
        assert_eq!(
            status(&start_file(
                &mut manager,
                &mut filesystem,
                "parent-race",
                "~/destination/file.txt",
                4,
            ))
            .status
            .as_deref(),
            Some("STARTED")
        );

        #[cfg(windows)]
        let parent_moved = match fs::rename(&original_parent, &retained_parent) {
            Ok(()) => true,
            Err(error) => {
                assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
                false
            }
        };
        #[cfg(not(windows))]
        let parent_moved = {
            fs::rename(&original_parent, &retained_parent).unwrap();
            true
        };
        if parent_moved {
            fs::create_dir(&original_parent).unwrap();
        }
        let mut end = command(FileTransferAction::EndData, "parent-race");
        end.file_id = Some("f1".into());
        end.data = b"safe".to_vec();
        assert_eq!(
            status(&run(&mut manager, &mut filesystem, end))
                .status
                .as_deref(),
            Some("OK")
        );
        assert!(run(
            &mut manager,
            &mut filesystem,
            command(FileTransferAction::Finish, "parent-race")
        )
        .is_empty());

        let committed_parent = if parent_moved {
            &retained_parent
        } else {
            &original_parent
        };
        assert_eq!(
            fs::read(committed_parent.join("file.txt")).unwrap(),
            b"safe"
        );
        if parent_moved {
            assert!(!original_parent.join("file.txt").exists());
        } else {
            assert!(!retained_parent.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn substituted_staged_entry_is_never_committed() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("substituted.txt");
        let mut manager = TtyTransferManager::new();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits(1024)).unwrap();
        approve_send(&mut manager, "substitution");
        start_file(
            &mut manager,
            &mut filesystem,
            "substitution",
            "~/substituted.txt",
            4,
        );

        let mut end = command(FileTransferAction::EndData, "substitution");
        end.file_id = Some("f1".into());
        end.data = b"safe".to_vec();
        assert_eq!(
            status(&run(&mut manager, &mut filesystem, end))
                .status
                .as_deref(),
            Some("OK")
        );

        let staged = filesystem
            .sessions
            .get_mut("substitution")
            .unwrap()
            .files
            .get_mut("f1")
            .unwrap()
            .temporary
            .as_mut()
            .unwrap();
        OpenOptionsAt::default()
            .unlink_at(&staged.source_directory, &staged.temporary_name)
            .unwrap();
        let mut replacement =
            create_relative_file(&staged.source_directory, &staged.temporary_name).unwrap();
        replacement.write_all(b"evil").unwrap();
        replacement.sync_all().unwrap();

        let rejected = run(
            &mut manager,
            &mut filesystem,
            command(FileTransferAction::Finish, "substitution"),
        );
        assert!(status(&rejected)
            .status
            .as_deref()
            .is_some_and(|value| value.starts_with("EPERM:")));
        assert!(!destination.exists());
    }

    #[cfg(unix)]
    #[test]
    fn zero_permission_payload_still_commits_by_identity() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("mode-zero");
        let mut manager = TtyTransferManager::new();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits(1024)).unwrap();
        approve_send(&mut manager, "mode-zero");

        let mut file = command(FileTransferAction::File, "mode-zero");
        file.file_id = Some("f1".into());
        file.name = Some("~/mode-zero".into());
        file.size = Some(1);
        file.permissions = Some(0);
        run(&mut manager, &mut filesystem, file);
        let mut end = command(FileTransferAction::EndData, "mode-zero");
        end.file_id = Some("f1".into());
        end.data = b"x".to_vec();
        run(&mut manager, &mut filesystem, end);
        assert!(run(
            &mut manager,
            &mut filesystem,
            command(FileTransferAction::Finish, "mode-zero")
        )
        .is_empty());
        assert_eq!(
            destination.metadata().unwrap().permissions().mode() & 0o777,
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_shared_parent_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let shared = home.path().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).unwrap();
        let mut manager = TtyTransferManager::new();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits(1024)).unwrap();
        approve_send(&mut manager, "shared-parent");

        let rejected = start_file(
            &mut manager,
            &mut filesystem,
            "shared-parent",
            "~/shared/file.txt",
            1,
        );
        assert!(status(&rejected)
            .status
            .as_deref()
            .is_some_and(|value| value.starts_with("EPERM:")));
        assert!(fs::read_dir(&shared).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn staging_and_default_commit_are_private_under_umask_zero() {
        use std::process::Command;

        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "kitty_file_transfer_fs::tests::umask_zero_staging_helper",
                "--nocapture",
            ])
            .env("CTERM_TEST_UMASK_ZERO", "1")
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[test]
    fn umask_zero_staging_helper() {
        use std::os::unix::fs::PermissionsExt;

        if std::env::var_os("CTERM_TEST_UMASK_ZERO").is_none() {
            return;
        }

        // SAFETY: the helper is run as the only selected test in a dedicated
        // child process, so changing its process-global umask cannot race.
        let previous_umask = unsafe { libc::umask(0) };
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let destination = home.path().join("private.txt");
        let mut manager = TtyTransferManager::new();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits(1024)).unwrap();
        approve_send(&mut manager, "private-modes");
        let started = start_file(
            &mut manager,
            &mut filesystem,
            "private-modes",
            "~/private.txt",
            3,
        );
        assert_eq!(status(&started).status.as_deref(), Some("STARTED"));

        let staged = match filesystem
            .sessions
            .get("private-modes")
            .unwrap()
            .files
            .get("f1")
            .unwrap()
            .writer
            .as_ref()
            .unwrap()
        {
            StagedWriter::Plain(file) => &file.file,
            StagedWriter::Zlib(_) => panic!("expected plain staged file"),
        };
        assert_eq!(
            staged
                .source_directory
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            staged.as_file().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );

        let mut end = command(FileTransferAction::EndData, "private-modes");
        end.file_id = Some("f1".into());
        end.data = b"abc".to_vec();
        run(&mut manager, &mut filesystem, end);
        assert!(run(
            &mut manager,
            &mut filesystem,
            command(FileTransferAction::Finish, "private-modes")
        )
        .is_empty());
        assert_eq!(
            destination.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );

        // SAFETY: restore the child process's original umask before exit.
        unsafe { libc::umask(previous_umask) };
    }

    #[cfg(windows)]
    #[test]
    fn windows_commit_primitives_preserve_the_prepared_handle() {
        let home = tempfile::tempdir().unwrap();
        let destination = home.path().join("primitive.txt");
        let mut staged = StagedTempFile::new(&destination).unwrap();
        staged.write_all(b"x").unwrap();
        staged.as_file().sync_all().unwrap();

        verify_staged_file(
            staged.as_file(),
            &staged.source_directory,
            &staged.temporary_name,
        )
        .unwrap();
        prepare_destination_security(staged.as_file(), &staged.parent).unwrap();
        replace_staged_file(
            staged.as_file(),
            &staged.source_directory,
            &staged.parent,
            &staged.temporary_name,
            &staged.destination_name,
        )
        .unwrap();
        staged.committed = true;
        staged.remove_staging_directory().unwrap();
        drop(staged);

        assert_eq!(fs::read(destination).unwrap(), b"x");
    }

    #[cfg(windows)]
    #[test]
    fn windows_stages_inside_a_private_directory() {
        let home = tempfile::tempdir().unwrap();
        let mut manager = TtyTransferManager::new();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits(1024)).unwrap();
        approve_send(&mut manager, "windows-private");
        assert_eq!(
            status(&start_file(
                &mut manager,
                &mut filesystem,
                "windows-private",
                "~/private.txt",
                1,
            ))
            .status
            .as_deref(),
            Some("STARTED")
        );

        let entries: Vec<_> = fs::read_dir(home.path())
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].file_type().unwrap().is_dir());
        let staging_path = entries[0].path();
        let staged_entries: Vec<_> = fs::read_dir(&staging_path)
            .unwrap()
            .map(|entry| entry.unwrap())
            .collect();
        assert_eq!(staged_entries.len(), 1);
        assert_eq!(staged_entries[0].file_name(), OsStr::new("payload"));
        assert!(staged_entries[0].file_type().unwrap().is_file());
        let open_error = fs::File::open(staging_path.join("payload")).unwrap_err();
        assert_eq!(
            open_error.raw_os_error(),
            Some(winapi::shared::winerror::ERROR_SHARING_VIOLATION as i32)
        );

        run(
            &mut manager,
            &mut filesystem,
            command(FileTransferAction::Cancel, "windows-private"),
        );
        assert!(fs::read_dir(home.path()).unwrap().next().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_staging_descriptor_is_owner_only_and_protected() {
        use winapi::shared::minwindef::LPVOID;
        use winapi::um::securitybaseapi::{EqualSid, GetAce};
        use winapi::um::winnt::{
            ACCESS_ALLOWED_ACE, ACCESS_ALLOWED_ACE_TYPE, FILE_ALL_ACCESS, SE_DACL_PROTECTED,
            TOKEN_USER,
        };

        let security = WindowsPrivateSecurityDescriptor::new().unwrap();
        assert_ne!(security.descriptor.Control & SE_DACL_PROTECTED, 0);
        assert!(!security.descriptor.Owner.is_null());
        assert!(!security.descriptor.Dacl.is_null());
        // SAFETY: the descriptor owns a live initialized ACL and TOKEN_USER
        // allocation for the entire test.
        unsafe {
            assert_eq!((*security.descriptor.Dacl).AceCount, 1);
            let mut raw_ace: LPVOID = std::ptr::null_mut();
            assert_ne!(GetAce(security.descriptor.Dacl.cast(), 0, &mut raw_ace), 0);
            let ace = &*raw_ace.cast::<ACCESS_ALLOWED_ACE>();
            assert_eq!(ace.Header.AceType, ACCESS_ALLOWED_ACE_TYPE);
            assert_eq!(ace.Mask, FILE_ALL_ACCESS);
            let user = &*security._user_storage.as_ptr().cast::<TOKEN_USER>();
            assert_ne!(EqualSid(security.descriptor.Owner.cast(), user.User.Sid), 0);
            assert_ne!(
                EqualSid(
                    std::ptr::addr_of!(ace.SidStart).cast_mut().cast(),
                    user.User.Sid,
                ),
                0
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_committed_file_inherits_the_destination_dacl() {
        use std::os::windows::fs::OpenOptionsExt;
        use winapi::shared::minwindef::LPVOID;
        use winapi::um::accctrl::SE_FILE_OBJECT;
        use winapi::um::aclapi::{GetSecurityInfo, SetSecurityInfo};
        use winapi::um::securitybaseapi::{GetAce, GetSecurityDescriptorControl};
        use winapi::um::winbase::FILE_FLAG_BACKUP_SEMANTICS;
        use winapi::um::winnt::{
            ACCESS_ALLOWED_ACE, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION,
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, INHERITED_ACE,
            OBJECT_INHERIT_ACE, PACL, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
            READ_CONTROL, SECURITY_DESCRIPTOR_CONTROL, SE_DACL_PROTECTED, WRITE_DAC,
        };

        struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);
        impl Drop for LocalSecurityDescriptor {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    // SAFETY: GetSecurityInfo allocated this descriptor with
                    // LocalAlloc.
                    unsafe { winapi::um::winbase::LocalFree(self.0.cast()) };
                }
            }
        }

        let home = tempfile::tempdir().unwrap();
        let security = WindowsPrivateSecurityDescriptor::new().unwrap();
        let mut raw_ace: LPVOID = std::ptr::null_mut();
        // SAFETY: the security helper owns a live initialized one-ACE DACL.
        assert_ne!(
            unsafe { GetAce(security.descriptor.Dacl.cast(), 0, &mut raw_ace) },
            0
        );
        // SAFETY: GetAce returned the single writable ACCESS_ALLOWED_ACE.
        unsafe {
            (*raw_ace.cast::<ACCESS_ALLOWED_ACE>()).Header.AceFlags =
                OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE;
        }
        let parent = fs::OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(home.path())
            .unwrap();
        // SAFETY: the parent handle has WRITE_DAC and the helper's ACL remains
        // live through this synchronous call.
        let status = unsafe {
            SetSecurityInfo(
                windows_handle(&parent),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                security.descriptor.Dacl.cast(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(status, 0);

        let destination = home.path().join("inherited.txt");
        let mut manager = TtyTransferManager::new();
        let mut filesystem =
            TtyTransferSendFilesystem::new(home.path().to_path_buf(), limits(1024)).unwrap();
        approve_send(&mut manager, "windows-inheritance");
        start_file(
            &mut manager,
            &mut filesystem,
            "windows-inheritance",
            "~/inherited.txt",
            1,
        );
        let mut end = command(FileTransferAction::EndData, "windows-inheritance");
        end.file_id = Some("f1".into());
        end.data = b"x".to_vec();
        run(&mut manager, &mut filesystem, end);
        assert!(run(
            &mut manager,
            &mut filesystem,
            command(FileTransferAction::Finish, "windows-inheritance")
        )
        .is_empty());

        let committed = fs::File::open(destination).unwrap();
        let mut dacl: PACL = std::ptr::null_mut();
        let mut raw_descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: the committed handle is live and the output pointers are
        // valid for a DACL query.
        let status = unsafe {
            GetSecurityInfo(
                windows_handle(&committed),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut raw_descriptor,
            )
        };
        assert_eq!(status, 0);
        let descriptor = LocalSecurityDescriptor(raw_descriptor);
        let mut control: SECURITY_DESCRIPTOR_CONTROL = 0;
        let mut revision = 0_u32;
        // SAFETY: descriptor owns the queried security descriptor.
        assert_ne!(
            unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) },
            0
        );
        assert_eq!(control & SE_DACL_PROTECTED, 0);
        assert!(!dacl.is_null());
        let ace_count = unsafe { (*dacl).AceCount };
        let mut inherited = 0;
        let mut explicit = 0;
        for index in 0..ace_count {
            let mut raw_ace: LPVOID = std::ptr::null_mut();
            // SAFETY: index is within the live ACL's reported ACE count.
            assert_ne!(unsafe { GetAce(dacl, index as u32, &mut raw_ace) }, 0);
            let header = unsafe { &*raw_ace.cast::<winapi::um::winnt::ACE_HEADER>() };
            if header.AceFlags & INHERITED_ACE != 0 {
                inherited += 1;
            } else {
                explicit += 1;
            }
        }
        assert!(inherited > 0);
        assert_eq!(explicit, 0);
    }

    #[cfg(windows)]
    #[test]
    fn windows_protocol_paths_reject_devices_ads_and_ambiguous_names() {
        assert!(resolve_absolute_protocol_path("/C:/safe/file.txt").is_some());
        for path in [
            "/C:/CON",
            "/C:/aux.txt",
            "/C:/COM¹.txt",
            "/C:/lpt³",
            "/C:/file.txt:stream",
            "/C:/trailing.",
            "/C:/trailing ",
            "/C:/question?",
        ] {
            assert!(resolve_absolute_protocol_path(path).is_none(), "{path}");
        }
    }
}
