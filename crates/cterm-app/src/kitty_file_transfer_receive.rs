//! Consent-gated filesystem reads for Kitty OSC 5113 receive sessions.
//!
//! Approved paths are opened once during metadata listing and retained as file
//! handles. Later data requests can only address those listed paths, so a
//! remote client cannot substitute a new path after the native consent prompt
//! or win a pathname replacement race between listing and transmission.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use cterm_core::{
    FileTransferAction, FileTransferCommand, FileTransferCompression, FileTransferType,
    FileTransmissionType, MAX_FILE_TRANSFER_CHUNK_BYTES, MAX_FILE_TRANSFER_PATH_BYTES,
};
use flate2::{write::ZlibEncoder, Compression};
use fs_at::{read_dir, OpenOptions as OpenOptionsAt};

use crate::kitty_file_transfer::{AuthorizedTtyTransferCommand, TtyTransferDirection};
use crate::kitty_file_transfer_fs::{
    resolve_protocol_path, TtyTransferFilesystemConfigError, TtyTransferLimits,
};
use crate::kitty_rsync::{write_delta, Signature};

const MAX_RSYNC_SIGNATURE_BYTES_PER_FILE: usize = 16 * 1024 * 1024;
const MAX_RSYNC_SIGNATURE_BYTES_PER_SESSION: usize = 64 * 1024 * 1024;

/// Filesystem stage for approved local-to-remote file-tree transfers.
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
                    self.list_path(command, quiet, emit)
                } else {
                    self.transmit_source(command, quiet, emit)
                }
            }
            FileTransferAction::Data | FileTransferAction::EndData => {
                self.receive_signature(command, quiet, emit)
            }
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
        let entries = std::mem::take(&mut session.entries);
        let path_ids: HashMap<_, _> = entries
            .iter()
            .filter_map(|item| match item {
                PendingReceiveListingItem::Entry(entry) => {
                    Some((entry.path.clone(), entry.actual_id.clone()))
                }
                PendingReceiveListingItem::Error(_) => None,
            })
            .collect();
        let mut regular_ids: HashMap<FileIdentity, String> = HashMap::new();

        for item in entries {
            let entry = match item {
                PendingReceiveListingItem::Entry(entry) => entry,
                PendingReceiveListingItem::Error(bytes) => {
                    if !emit(bytes) {
                        self.sessions.remove(&session_id);
                        return false;
                    }
                    continue;
                }
            };
            let (file_type, data, source) = match entry.kind {
                ListedReceiveKind::Regular {
                    file,
                    size,
                    identity,
                } => {
                    if let Some(target_id) = regular_ids.get(&identity).cloned() {
                        (
                            FileTransferType::Link,
                            target_id.into_bytes(),
                            ReceiveSource::Link,
                        )
                    } else {
                        regular_ids.insert(identity, entry.actual_id.clone());
                        (
                            FileTransferType::Regular,
                            Vec::new(),
                            ReceiveSource::Regular { file, size },
                        )
                    }
                }
                ListedReceiveKind::Directory => (
                    FileTransferType::Directory,
                    Vec::new(),
                    ReceiveSource::Directory,
                ),
                ListedReceiveKind::Symlink {
                    target,
                    resolved_target,
                    absolute,
                } => {
                    let data = resolved_target
                        .as_ref()
                        .and_then(|target| path_ids.get(target))
                        .map(|target_id| target_id.as_bytes().to_vec())
                        .unwrap_or_default();
                    (
                        FileTransferType::Symlink,
                        data,
                        ReceiveSource::Symlink { target, absolute },
                    )
                }
            };
            session
                .sources
                .entry(entry.name.clone())
                .or_default()
                .push_back(source);
            let metadata_command = FileTransferCommand {
                action: FileTransferAction::File,
                id: session_id.clone(),
                file_id: Some(entry.spec_id),
                bypass: None,
                quiet: 0,
                mtime: entry.mtime,
                permissions: Some(entry.permissions),
                size: Some(entry.size),
                name: Some(entry.name),
                status: Some(entry.actual_id),
                parent: entry.parent,
                data,
                compression: None,
                file_type: Some(file_type),
                transmission_type: None,
            };
            if !emit_command(emit, metadata_command) {
                self.sessions.remove(&session_id);
                return false;
            }
        }
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

    fn list_path<F>(&mut self, command: FileTransferCommand, quiet: u8, emit: &mut F) -> bool
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
        let remaining_files = self
            .limits
            .max_files_per_session
            .saturating_sub(session.listed_files);
        let remaining_bytes = self
            .limits
            .max_session_bytes
            .saturating_sub(session.planned_bytes);
        let collected = match collect_receive_tree(
            &path,
            file_id,
            session.next_actual_id,
            remaining_files,
            self.limits.max_file_bytes,
            remaining_bytes,
        ) {
            Ok(collected) => collected,
            Err(error) => {
                let mut encoded = None;
                emit_status(
                    &mut |bytes| {
                        encoded = Some(bytes);
                        true
                    },
                    &command,
                    error.protocol_status(),
                    quiet,
                    true,
                    None,
                    None,
                );
                if let Some(encoded) = encoded {
                    session
                        .entries
                        .push(PendingReceiveListingItem::Error(encoded));
                }
                return true;
            }
        };
        session.next_actual_id = collected.next_actual_id;
        session.listed_files += collected.entries.len();
        session.planned_bytes += collected.planned_bytes;
        session.entries.extend(
            collected
                .entries
                .into_iter()
                .map(PendingReceiveListingItem::Entry),
        );
        true
    }

    fn transmit_source<F>(&mut self, command: FileTransferCommand, quiet: u8, emit: &mut F) -> bool
    where
        F: FnMut(Vec<u8>) -> bool,
    {
        debug_assert!(matches!(
            command.transmission_type,
            None | Some(FileTransmissionType::Simple | FileTransmissionType::Rsync)
        ));
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
        let Some(source) = sources.pop_front() else {
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
        if command.transmission_type == Some(FileTransmissionType::Rsync) {
            let ReceiveSource::Regular { file, size } = source else {
                return emit_status(
                    emit,
                    &command,
                    "EINVAL:Rsync can be requested only for regular files",
                    quiet,
                    true,
                    None,
                    None,
                );
            };
            if match file.metadata() {
                Ok(metadata) => metadata.len() != size,
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
            session.rsync.insert(
                file_id.to_string(),
                PendingRsyncSource {
                    file,
                    size,
                    compression: command.compression,
                    signature: Vec::new(),
                },
            );
            return true;
        }
        let result = match source {
            ReceiveSource::Regular { mut file, size } => {
                if match file.metadata() {
                    Ok(metadata) => metadata.len() != size,
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
                stream_file(
                    &mut file,
                    size,
                    &command.id,
                    file_id,
                    command.compression,
                    emit,
                )
            }
            ReceiveSource::Symlink { target, absolute } => {
                if absolute {
                    return emit_status(
                        emit,
                        &command,
                        "EINVAL:Absolute symlink data must not be requested",
                        quiet,
                        true,
                        None,
                        None,
                    );
                }
                stream_bytes(&target, &command.id, file_id, emit)
            }
            ReceiveSource::Directory => {
                return emit_status(
                    emit,
                    &command,
                    "EISDIR:Directory data must not be requested",
                    quiet,
                    true,
                    None,
                    None,
                );
            }
            ReceiveSource::Link => {
                return emit_status(
                    emit,
                    &command,
                    "EINVAL:Hard-link data must not be requested",
                    quiet,
                    true,
                    None,
                    None,
                );
            }
        };
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

    fn receive_signature<F>(
        &mut self,
        command: FileTransferCommand,
        quiet: u8,
        emit: &mut F,
    ) -> bool
    where
        F: FnMut(Vec<u8>) -> bool,
    {
        let Some(file_id) = command.file_id.as_deref() else {
            return true;
        };
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
        let Some(pending) = session.rsync.get_mut(file_id) else {
            return emit_status(
                emit,
                &command,
                "EINVAL:No rsync signature is pending for this file",
                quiet,
                true,
                None,
                None,
            );
        };
        let new_file_bytes = pending.signature.len().checked_add(command.data.len());
        let new_session_bytes = session
            .rsync_signature_bytes
            .checked_add(command.data.len());
        if new_file_bytes.is_none_or(|length| length > MAX_RSYNC_SIGNATURE_BYTES_PER_FILE)
            || new_session_bytes.is_none_or(|length| length > MAX_RSYNC_SIGNATURE_BYTES_PER_SESSION)
        {
            let removed = session
                .rsync
                .remove(file_id)
                .expect("pending rsync source was checked above");
            session.rsync_signature_bytes = session
                .rsync_signature_bytes
                .saturating_sub(removed.signature.len());
            return emit_status(
                emit,
                &command,
                "EFBIG:Rsync signature exceeds configured limits",
                quiet,
                true,
                None,
                None,
            );
        }
        pending.signature.extend_from_slice(&command.data);
        session.rsync_signature_bytes += command.data.len();
        if command.action != FileTransferAction::EndData {
            return true;
        }

        let pending = session
            .rsync
            .remove(file_id)
            .expect("pending rsync source was checked above");
        session.rsync_signature_bytes = session
            .rsync_signature_bytes
            .saturating_sub(pending.signature.len());
        let signature =
            match Signature::parse(&pending.signature, MAX_RSYNC_SIGNATURE_BYTES_PER_FILE) {
                Ok(signature) => signature,
                Err(_) => {
                    return emit_status(
                        emit,
                        &command,
                        "EINVAL:Invalid rsync signature",
                        quiet,
                        true,
                        None,
                        None,
                    );
                }
            };
        let result = stream_delta(
            pending.file,
            pending.size,
            &signature,
            &command.id,
            file_id,
            pending.compression,
            emit,
        );
        match result {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => false,
            Err(error) => emit_status(
                emit,
                &command,
                receive_io_status(&error, "EIO:Could not create rsync delta"),
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
    entries: Vec<PendingReceiveListingItem>,
    sources: HashMap<String, VecDeque<ReceiveSource>>,
    transmitted_ids: HashSet<String>,
    rsync: HashMap<String, PendingRsyncSource>,
    rsync_signature_bytes: usize,
    listed_files: usize,
    planned_bytes: u64,
    next_actual_id: u64,
    listing_complete: bool,
}

#[derive(Debug)]
struct PendingRsyncSource {
    file: fs::File,
    size: u64,
    compression: Option<FileTransferCompression>,
    signature: Vec<u8>,
}

#[derive(Debug)]
enum PendingReceiveListingItem {
    Entry(ListedReceiveEntry),
    Error(Vec<u8>),
}

#[derive(Debug)]
struct ListedReceiveEntry {
    spec_id: String,
    actual_id: String,
    path: PathBuf,
    name: String,
    parent: Option<String>,
    mtime: Option<i64>,
    permissions: u32,
    size: u64,
    kind: ListedReceiveKind,
}

#[derive(Debug)]
enum ListedReceiveKind {
    Regular {
        file: fs::File,
        size: u64,
        identity: FileIdentity,
    },
    Directory,
    Symlink {
        target: Vec<u8>,
        resolved_target: Option<PathBuf>,
        absolute: bool,
    },
}

#[derive(Debug)]
enum ReceiveSource {
    Regular { file: fs::File, size: u64 },
    Directory,
    Symlink { target: Vec<u8>, absolute: bool },
    Link,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileIdentity {
    volume: u64,
    node: u64,
}

#[derive(Debug)]
struct CollectedReceiveTree {
    entries: Vec<ListedReceiveEntry>,
    planned_bytes: u64,
    next_actual_id: u64,
}

#[derive(Debug)]
enum ReceiveListingError {
    Io(io::Error),
    TooManyEntries,
    FileTooLarge,
    SessionTooLarge,
    UnrepresentablePath,
    UnsupportedFileType,
}

impl ReceiveListingError {
    fn protocol_status(&self) -> &'static str {
        match self {
            Self::Io(error) => receive_io_status(error, "EIO:Could not inspect source tree"),
            Self::TooManyEntries => "ENOSPC:Too many files in transfer session",
            Self::FileTooLarge | Self::SessionTooLarge => {
                "EFBIG:Transfer exceeds configured size limits"
            }
            Self::UnrepresentablePath => "EINVAL:Source path is not representable",
            Self::UnsupportedFileType => "ENOTSUP:Source contains an unsupported file type",
        }
    }
}

impl From<io::Error> for ReceiveListingError {
    fn from(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::Unsupported {
            Self::UnsupportedFileType
        } else {
            Self::Io(error)
        }
    }
}

fn collect_receive_tree(
    path: &Path,
    spec_id: &str,
    first_actual_id: u64,
    max_entries: usize,
    max_file_bytes: u64,
    max_session_bytes: u64,
) -> Result<CollectedReceiveTree, ReceiveListingError> {
    let mut collector = ReceiveTreeCollector {
        spec_id,
        entries: Vec::new(),
        planned_bytes: 0,
        next_actual_id: first_actual_id,
        max_entries,
        max_file_bytes,
        max_session_bytes,
    };
    let opened = open_top_level_entry(path)?;
    collector.collect(path.to_path_buf(), None, opened)?;
    Ok(CollectedReceiveTree {
        entries: collector.entries,
        planned_bytes: collector.planned_bytes,
        next_actual_id: collector.next_actual_id,
    })
}

struct ReceiveTreeCollector<'a> {
    spec_id: &'a str,
    entries: Vec<ListedReceiveEntry>,
    planned_bytes: u64,
    next_actual_id: u64,
    max_entries: usize,
    max_file_bytes: u64,
    max_session_bytes: u64,
}

impl ReceiveTreeCollector<'_> {
    fn collect(
        &mut self,
        path: PathBuf,
        parent: Option<String>,
        opened: OpenedReceiveEntry,
    ) -> Result<(), ReceiveListingError> {
        if self.entries.len() >= self.max_entries {
            return Err(ReceiveListingError::TooManyEntries);
        }
        let name = protocol_absolute_path(&path)
            .filter(|name| valid_generated_protocol_path(name))
            .ok_or(ReceiveListingError::UnrepresentablePath)?;
        let actual_id = self.next_actual_id.to_string();
        self.next_actual_id = self.next_actual_id.wrapping_add(1);

        match opened {
            OpenedReceiveEntry::Regular { file, metadata } => {
                let size = metadata.len();
                self.reserve_data(size)?;
                let identity = file_identity(&file, &metadata)?;
                self.entries.push(ListedReceiveEntry {
                    spec_id: self.spec_id.to_string(),
                    actual_id,
                    path,
                    name,
                    parent,
                    mtime: modification_time_nanoseconds(&metadata),
                    permissions: protocol_permissions(&metadata),
                    size,
                    kind: ListedReceiveKind::Regular {
                        file,
                        size,
                        identity,
                    },
                });
            }
            OpenedReceiveEntry::Symlink { target, metadata } => {
                let encoded_target = protocol_symlink_target(&target)
                    .ok_or(ReceiveListingError::UnrepresentablePath)?;
                self.reserve_data(encoded_target.len() as u64)?;
                let absolute = target.is_absolute();
                let resolved_target = resolve_symlink_target(&path, &target);
                self.entries.push(ListedReceiveEntry {
                    spec_id: self.spec_id.to_string(),
                    actual_id,
                    path,
                    name,
                    parent,
                    mtime: modification_time_nanoseconds(&metadata),
                    permissions: protocol_permissions(&metadata),
                    size: metadata.len(),
                    kind: ListedReceiveKind::Symlink {
                        target: encoded_target.into_bytes(),
                        resolved_target,
                        absolute,
                    },
                });
            }
            OpenedReceiveEntry::Directory {
                mut directory,
                metadata,
            } => {
                self.entries.push(ListedReceiveEntry {
                    spec_id: self.spec_id.to_string(),
                    actual_id: actual_id.clone(),
                    path: path.clone(),
                    name,
                    parent,
                    mtime: modification_time_nanoseconds(&metadata),
                    permissions: protocol_permissions(&metadata),
                    size: metadata.len(),
                    kind: ListedReceiveKind::Directory,
                });
                let mut children = Vec::new();
                for child in read_dir(&mut directory)? {
                    let child = child?;
                    if child.name() != OsStr::new(".") && child.name() != OsStr::new("..") {
                        if children.len() >= self.max_entries - self.entries.len() {
                            return Err(ReceiveListingError::TooManyEntries);
                        }
                        children.push(child.name().to_os_string());
                    }
                }
                children.sort();
                for child in children {
                    let child_path = path.join(&child);
                    let opened = open_child_entry(&directory, &child, &child_path)?;
                    self.collect(child_path, Some(actual_id.clone()), opened)?;
                }
            }
        }
        Ok(())
    }

    fn reserve_data(&mut self, size: u64) -> Result<(), ReceiveListingError> {
        if size > self.max_file_bytes {
            return Err(ReceiveListingError::FileTooLarge);
        }
        self.planned_bytes = self
            .planned_bytes
            .checked_add(size)
            .filter(|total| *total <= self.max_session_bytes)
            .ok_or(ReceiveListingError::SessionTooLarge)?;
        Ok(())
    }
}

enum OpenedReceiveEntry {
    Regular {
        file: fs::File,
        metadata: fs::Metadata,
    },
    Directory {
        directory: fs::File,
        metadata: fs::Metadata,
    },
    Symlink {
        target: PathBuf,
        metadata: fs::Metadata,
    },
}

fn open_top_level_entry(path: &Path) -> io::Result<OpenedReceiveEntry> {
    if path.file_name().is_none() {
        let directory = open_read_directory(path)?;
        let metadata = directory.metadata()?;
        return Ok(OpenedReceiveEntry::Directory {
            directory,
            metadata,
        });
    }
    let parent_path = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no file name"))?;
    let parent = open_read_directory(parent_path)?;
    open_child_entry(&parent, name, path)
}

fn open_child_entry(
    parent: &fs::File,
    name: &OsStr,
    display_path: &Path,
) -> io::Result<OpenedReceiveEntry> {
    match entry_kind_at(parent, name, display_path)? {
        ReceiveEntryKind::Symlink => {
            let target = read_link_at(parent, name, display_path)?;
            let metadata = fs::symlink_metadata(display_path)?;
            Ok(OpenedReceiveEntry::Symlink { target, metadata })
        }
        ReceiveEntryKind::Directory => {
            let mut options = OpenOptionsAt::default();
            options.read(true).follow(false);
            let directory = options.open_dir_at(parent, name)?;
            let metadata = directory.metadata()?;
            Ok(OpenedReceiveEntry::Directory {
                directory,
                metadata,
            })
        }
        ReceiveEntryKind::Regular => {
            let file = open_regular_at(parent, name)?;
            let metadata = file.metadata()?;
            Ok(OpenedReceiveEntry::Regular { file, metadata })
        }
        ReceiveEntryKind::Unsupported => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "source is not a regular file, directory, or symbolic link",
        )),
    }
}

fn open_regular_at(parent: &fs::File, name: &OsStr) -> io::Result<fs::File> {
    let mut options = OpenOptionsAt::default();
    options.read(true).follow(false);
    let file = options.open_at(parent, name)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source is not a regular file",
        ));
    }
    Ok(file)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiveEntryKind {
    Regular,
    Directory,
    Symlink,
    Unsupported,
}

#[cfg(unix)]
fn entry_kind_at(
    parent: &fs::File,
    name: &OsStr,
    _display_path: &Path,
) -> io::Result<ReceiveEntryKind> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source name contains NUL"))?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `name` is NUL-terminated, `stat` points to writable storage, and
    // the retained directory descriptor remains alive for the call.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful `fstatat` initialized the structure.
    let mode = unsafe { stat.assume_init() }.st_mode;
    Ok(match mode & libc::S_IFMT {
        libc::S_IFREG => ReceiveEntryKind::Regular,
        libc::S_IFDIR => ReceiveEntryKind::Directory,
        libc::S_IFLNK => ReceiveEntryKind::Symlink,
        _ => ReceiveEntryKind::Unsupported,
    })
}

#[cfg(windows)]
fn entry_kind_at(
    _parent: &fs::File,
    _name: &OsStr,
    display_path: &Path,
) -> io::Result<ReceiveEntryKind> {
    let metadata = fs::symlink_metadata(display_path)?;
    Ok(if metadata.file_type().is_symlink() {
        ReceiveEntryKind::Symlink
    } else if metadata.is_dir() {
        ReceiveEntryKind::Directory
    } else if metadata.is_file() {
        ReceiveEntryKind::Regular
    } else {
        ReceiveEntryKind::Unsupported
    })
}

#[cfg(not(any(unix, windows)))]
fn entry_kind_at(
    _parent: &fs::File,
    _name: &OsStr,
    _display_path: &Path,
) -> io::Result<ReceiveEntryKind> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "handle-relative metadata is unsupported on this platform",
    ))
}

#[cfg(unix)]
fn read_link_at(parent: &fs::File, name: &OsStr, _display_path: &Path) -> io::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source name contains NUL"))?;
    let mut target = vec![0_u8; MAX_FILE_TRANSFER_PATH_BYTES + 1];
    // SAFETY: the retained directory and C string are valid, and `target`
    // exposes its full initialized allocation as a writable byte buffer.
    let size = unsafe {
        libc::readlinkat(
            parent.as_raw_fd(),
            name.as_ptr(),
            target.as_mut_ptr().cast(),
            target.len(),
        )
    };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    let size = usize::try_from(size).expect("readlinkat returned a non-negative ssize_t");
    if size > MAX_FILE_TRANSFER_PATH_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "symbolic-link target exceeds the protocol path limit",
        ));
    }
    target.truncate(size);
    Ok(PathBuf::from(std::ffi::OsString::from_vec(target)))
}

#[cfg(windows)]
fn read_link_at(_parent: &fs::File, _name: &OsStr, display_path: &Path) -> io::Result<PathBuf> {
    fs::read_link(display_path)
}

#[cfg(not(any(unix, windows)))]
fn read_link_at(_parent: &fs::File, _name: &OsStr, _display_path: &Path) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symbolic-link reads are unsupported on this platform",
    ))
}

#[cfg(unix)]
fn file_identity(_file: &fs::File, metadata: &fs::Metadata) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;

    Ok(FileIdentity {
        volume: metadata.dev(),
        node: metadata.ino(),
    })
}

#[cfg(windows)]
fn file_identity(file: &fs::File, _metadata: &fs::Metadata) -> io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use winapi::um::fileapi::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};

    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the file handle is valid for this call and `information` points
    // to writable storage of the exact structure expected by Win32.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), information.as_mut_ptr()) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful Win32 call initialized the structure.
    let information = unsafe { information.assume_init() };
    Ok(FileIdentity {
        volume: information.dwVolumeSerialNumber as u64,
        node: (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_file: &fs::File, _metadata: &fs::Metadata) -> io::Result<FileIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "stable file identities are unsupported on this platform",
    ))
}

fn resolve_symlink_target(link_path: &Path, target: &Path) -> Option<PathBuf> {
    let candidate = if target.is_absolute() {
        target.to_path_buf()
    } else {
        link_path.parent()?.join(target)
    };
    normalize_absolute_path(&candidate)
}

fn normalize_absolute_path(path: &Path) -> Option<PathBuf> {
    use std::path::Component;

    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(component) => normalized.push(component),
        }
    }
    Some(normalized)
}

#[cfg(not(windows))]
fn protocol_symlink_target(target: &Path) -> Option<String> {
    let target = target.to_str()?.to_string();
    (target.len() <= MAX_FILE_TRANSFER_PATH_BYTES).then_some(target)
}

#[cfg(windows)]
fn protocol_symlink_target(target: &Path) -> Option<String> {
    let mut target = target.to_str()?.replace('\\', "/");
    if let Some(stripped) = target.strip_prefix("//?/") {
        target = stripped.to_string();
    }
    if target.as_bytes().get(1) == Some(&b':') && !target.starts_with('/') {
        target.insert(0, '/');
    }
    (target.len() <= MAX_FILE_TRANSFER_PATH_BYTES).then_some(target)
}

fn valid_generated_protocol_path(path: &str) -> bool {
    path.len() <= MAX_FILE_TRANSFER_PATH_BYTES
        && !path.contains('\0')
        && path
            .split('/')
            .filter(|component| !component.is_empty())
            .all(|component| component.len() <= 255)
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

fn stream_delta<F>(
    file: fs::File,
    expected_size: u64,
    signature: &Signature,
    session_id: &str,
    file_id: &str,
    compression: Option<FileTransferCompression>,
    emit: &mut F,
) -> io::Result<()>
where
    F: FnMut(Vec<u8>) -> bool,
{
    let mut source = file.take(expected_size);
    let mut chunks = ProtocolChunkWriter::new(session_id, file_id, emit);
    match compression {
        Some(FileTransferCompression::Zlib) => {
            let mut encoder = ZlibEncoder::new(chunks, Compression::default());
            write_delta(&mut source, signature, &mut encoder)?;
            chunks = encoder.finish()?;
        }
        None | Some(FileTransferCompression::None) => {
            write_delta(&mut source, signature, &mut chunks)?;
        }
    }
    if source.limit() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "source was truncated during rsync delta generation",
        ));
    }
    chunks.finish()
}

fn stream_bytes<F>(data: &[u8], session_id: &str, file_id: &str, emit: &mut F) -> io::Result<()>
where
    F: FnMut(Vec<u8>) -> bool,
{
    let mut chunks = ProtocolChunkWriter::new(session_id, file_id, emit);
    chunks.write_all(data)?;
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
    use crate::kitty_rsync::write_signature;
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
        let protocol_home = protocol_absolute_path(home.path()).unwrap();
        assert_eq!(
            decode(&listing[2]).name.as_deref(),
            Some(protocol_home.as_str())
        );

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
    fn rsync_signatures_produce_bounded_plain_and_zlib_deltas() {
        let home = tempfile::tempdir().unwrap();
        let mut basis = Vec::new();
        for index in 0..256 {
            basis.extend_from_slice(
                format!("line-{index:03}-xxxxxxxxxxxxxxxxxxxxxxxx\n").as_bytes(),
            );
        }
        let mut source = basis.clone();
        source[777..791].copy_from_slice(b"changed-delta!");
        fs::write(home.path().join("rsync.bin"), &source).unwrap();
        let limits =
            TtyTransferLimits::new(8, source.len() as u64 * 2, source.len() as u64 * 4).unwrap();

        for (run, compression) in [
            ("rsync-plain", None),
            ("rsync-zlib", Some(FileTransferCompression::Zlib)),
        ] {
            let mut filesystem =
                TtyTransferReceiveFilesystem::new(home.path().to_path_buf(), limits).unwrap();
            let mut manager = TtyTransferManager::new();
            let listing = run_actions(
                &mut filesystem,
                approve_receive(&mut manager, run, &[("spec", "rsync.bin")]),
            );
            let metadata = decode(&listing[1]);
            let mut request = command(FileTransferAction::File, run);
            request.file_id = Some("out".into());
            request.name = metadata.name;
            request.transmission_type = Some(FileTransmissionType::Rsync);
            request.compression = compression;
            assert!(run_actions(&mut filesystem, manager.handle(request)).is_empty());

            let mut serialized_signature = Vec::new();
            write_signature(
                io::Cursor::new(&basis),
                basis.len() as u64,
                &mut serialized_signature,
            )
            .unwrap();
            let signature =
                Signature::parse(&serialized_signature, MAX_RSYNC_SIGNATURE_BYTES_PER_FILE)
                    .unwrap();
            let block_size = signature.block_size();
            let mut output = Vec::new();
            for (index, chunk) in serialized_signature
                .chunks(MAX_FILE_TRANSFER_CHUNK_BYTES)
                .enumerate()
            {
                let mut packet = command(
                    if (index + 1) * MAX_FILE_TRANSFER_CHUNK_BYTES >= serialized_signature.len() {
                        FileTransferAction::EndData
                    } else {
                        FileTransferAction::Data
                    },
                    run,
                );
                packet.file_id = Some("out".into());
                packet.data = chunk.to_vec();
                output.extend(run_actions(&mut filesystem, manager.handle(packet)));
            }
            let decoded: Vec<_> = output.iter().map(|packet| decode(packet)).collect();
            assert!(decoded
                .iter()
                .all(|packet| packet.data.len() <= MAX_FILE_TRANSFER_CHUNK_BYTES));
            assert_eq!(decoded.last().unwrap().action, FileTransferAction::EndData);
            let wire_delta: Vec<_> = decoded.into_iter().flat_map(|packet| packet.data).collect();
            let delta = if compression == Some(FileTransferCompression::Zlib) {
                let mut decoded = Vec::new();
                ZlibDecoder::new(wire_delta.as_slice())
                    .read_to_end(&mut decoded)
                    .unwrap();
                decoded
            } else {
                wire_delta
            };
            let mut patcher = crate::kitty_rsync::DeltaPatcher::new(
                io::Cursor::new(&basis),
                Vec::new(),
                basis.len() as u64,
                block_size,
                source.len() as u64,
            )
            .unwrap();
            for chunk in delta.chunks(11) {
                patcher.write_all(chunk).unwrap();
            }
            assert_eq!(patcher.finish().unwrap(), source);
        }
    }

    #[test]
    fn invalid_rsync_signature_fails_closed_and_releases_its_buffer() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("rsync.bin"), b"approved source").unwrap();
        let limits = TtyTransferLimits::new(4, 1024, 4096).unwrap();
        let mut filesystem =
            TtyTransferReceiveFilesystem::new(home.path().to_path_buf(), limits).unwrap();
        let mut manager = TtyTransferManager::new();
        let listing = run_actions(
            &mut filesystem,
            approve_receive(&mut manager, "bad-rsync", &[("spec", "rsync.bin")]),
        );
        let metadata = decode(&listing[1]);
        let mut request = command(FileTransferAction::File, "bad-rsync");
        request.file_id = Some("out".into());
        request.name = metadata.name;
        request.transmission_type = Some(FileTransmissionType::Rsync);
        assert!(run_actions(&mut filesystem, manager.handle(request)).is_empty());

        let mut signature = command(FileTransferAction::EndData, "bad-rsync");
        signature.file_id = Some("out".into());
        signature.data = vec![0; 12];
        let rejected = run_actions(&mut filesystem, manager.handle(signature));
        assert_eq!(rejected.len(), 1);
        assert!(decode(&rejected[0])
            .status
            .as_deref()
            .is_some_and(|status| status.starts_with("EINVAL:")));
        let session = filesystem.sessions.get("bad-rsync").unwrap();
        assert!(session.rsync.is_empty());
        assert_eq!(session.rsync_signature_bytes, 0);
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

    #[test]
    fn approved_directory_lists_nested_files_and_hardlinks() {
        let home = tempfile::tempdir().unwrap();
        let tree = home.path().join("tree");
        let sub = tree.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let original = tree.join("original.txt");
        let hardlink = sub.join("hardlink.txt");
        fs::write(&original, b"one filesystem object").unwrap();
        fs::hard_link(&original, &hardlink).unwrap();
        fs::write(sub.join("child.txt"), b"child").unwrap();

        let limits = TtyTransferLimits::new(8, 1024, 4096).unwrap();
        let mut filesystem =
            TtyTransferReceiveFilesystem::new(home.path().to_path_buf(), limits).unwrap();
        let mut manager = TtyTransferManager::new();
        let listing = run_actions(
            &mut filesystem,
            approve_receive(&mut manager, "portable-tree", &[("tree", "~/tree")]),
        );
        let listing: Vec<_> = listing.iter().map(|encoded| decode(encoded)).collect();
        let files: HashMap<_, _> = listing
            .iter()
            .filter(|command| command.action == FileTransferAction::File)
            .map(|command| (command.name.as_deref().unwrap(), command))
            .collect();
        assert_eq!(files.len(), 5);

        let tree_name = protocol_absolute_path(&tree).unwrap();
        let sub_name = protocol_absolute_path(&sub).unwrap();
        let original_name = protocol_absolute_path(&original).unwrap();
        let hardlink_name = protocol_absolute_path(&hardlink).unwrap();
        let tree_metadata = files[tree_name.as_str()];
        let sub_metadata = files[sub_name.as_str()];
        let original_metadata = files[original_name.as_str()];
        let hardlink_metadata = files[hardlink_name.as_str()];
        assert_eq!(tree_metadata.file_type, Some(FileTransferType::Directory));
        assert_eq!(sub_metadata.parent, tree_metadata.status);
        assert_eq!(hardlink_metadata.file_type, Some(FileTransferType::Link));
        assert_eq!(
            hardlink_metadata.data,
            original_metadata.status.as_deref().unwrap().as_bytes()
        );
    }

    #[test]
    fn bare_home_spec_lists_the_home_directory_itself() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("inside.txt"), b"inside").unwrap();
        let limits = TtyTransferLimits::new(4, 1024, 4096).unwrap();
        let mut filesystem =
            TtyTransferReceiveFilesystem::new(home.path().to_path_buf(), limits).unwrap();
        let mut manager = TtyTransferManager::new();
        let listing = run_actions(
            &mut filesystem,
            approve_receive(&mut manager, "home-tree", &[("home", "~")]),
        );
        let files: Vec<_> = listing
            .iter()
            .map(|encoded| decode(encoded))
            .filter(|command| command.action == FileTransferAction::File)
            .collect();
        assert_eq!(files.len(), 2);
        let protocol_home = protocol_absolute_path(home.path()).unwrap();
        assert!(files.iter().any(|command| {
            command.name.as_deref() == Some(protocol_home.as_str())
                && command.file_type == Some(FileTransferType::Directory)
                && command.parent.is_none()
        }));
    }

    #[test]
    fn oversized_directory_listing_is_rejected_without_partial_metadata() {
        let home = tempfile::tempdir().unwrap();
        let tree = home.path().join("tree");
        fs::create_dir(&tree).unwrap();
        fs::write(tree.join("one"), b"1").unwrap();
        fs::write(tree.join("two"), b"2").unwrap();
        let limits = TtyTransferLimits::new(2, 1024, 4096).unwrap();
        let mut filesystem =
            TtyTransferReceiveFilesystem::new(home.path().to_path_buf(), limits).unwrap();
        let mut manager = TtyTransferManager::new();
        let listing = run_actions(
            &mut filesystem,
            approve_receive(&mut manager, "wide-tree", &[("tree", "~/tree")]),
        );
        let listing: Vec<_> = listing.iter().map(|encoded| decode(encoded)).collect();
        assert!(!listing
            .iter()
            .any(|command| command.action == FileTransferAction::File));
        assert!(listing.iter().any(|command| command
            .status
            .as_deref()
            .is_some_and(|status| status.starts_with("ENOSPC:"))));
    }

    #[cfg(unix)]
    #[test]
    fn approved_directory_tree_preserves_links_and_retained_file_handles() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().unwrap();
        let tree = home.path().join("tree");
        let sub = tree.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let root_file = tree.join("root.txt");
        let child_file = sub.join("child.txt");
        fs::write(&root_file, b"root contents").unwrap();
        fs::write(&child_file, b"approved child").unwrap();
        fs::hard_link(&root_file, sub.join("hard.txt")).unwrap();
        symlink("../root.txt", sub.join("relative-link")).unwrap();
        symlink("/outside-the-approved-tree", sub.join("absolute-link")).unwrap();

        let limits = TtyTransferLimits::new(16, 1024, 4096).unwrap();
        let mut filesystem =
            TtyTransferReceiveFilesystem::new(home.path().to_path_buf(), limits).unwrap();
        let mut manager = TtyTransferManager::new();
        let listing = run_actions(
            &mut filesystem,
            approve_receive(&mut manager, "tree-session", &[("tree-spec", "~/tree")]),
        );
        let listing: Vec<_> = listing.iter().map(|encoded| decode(encoded)).collect();
        assert_eq!(listing.first().unwrap().status.as_deref(), Some("OK"));
        assert_eq!(listing.last().unwrap().status.as_deref(), Some("OK"));
        let files: HashMap<_, _> = listing
            .iter()
            .filter(|command| command.action == FileTransferAction::File)
            .map(|command| (command.name.as_deref().unwrap(), command))
            .collect();
        assert_eq!(files.len(), 7);

        let tree_name = protocol_absolute_path(&tree).unwrap();
        let sub_name = protocol_absolute_path(&sub).unwrap();
        let root_name = protocol_absolute_path(&root_file).unwrap();
        let child_name = protocol_absolute_path(&child_file).unwrap();
        let hard_name = protocol_absolute_path(&sub.join("hard.txt")).unwrap();
        let relative_name = protocol_absolute_path(&sub.join("relative-link")).unwrap();
        let absolute_name = protocol_absolute_path(&sub.join("absolute-link")).unwrap();

        let tree_metadata = files[tree_name.as_str()];
        let sub_metadata = files[sub_name.as_str()];
        let root_metadata = files[root_name.as_str()];
        assert_eq!(tree_metadata.file_type, Some(FileTransferType::Directory));
        assert_eq!(tree_metadata.parent, None);
        assert_eq!(sub_metadata.file_type, Some(FileTransferType::Directory));
        assert_eq!(sub_metadata.parent, tree_metadata.status);
        assert_eq!(files[child_name.as_str()].parent, sub_metadata.status);

        let hard_metadata = files[hard_name.as_str()];
        assert_eq!(hard_metadata.file_type, Some(FileTransferType::Link));
        assert_eq!(
            hard_metadata.data,
            root_metadata.status.as_deref().unwrap().as_bytes()
        );
        let relative_metadata = files[relative_name.as_str()];
        assert_eq!(relative_metadata.file_type, Some(FileTransferType::Symlink));
        assert_eq!(
            relative_metadata.data,
            root_metadata.status.as_deref().unwrap().as_bytes()
        );
        assert_eq!(
            files[absolute_name.as_str()].file_type,
            Some(FileTransferType::Symlink)
        );
        assert!(files[absolute_name.as_str()].data.is_empty());

        let retained_tree = home.path().join("retained-tree");
        fs::rename(&tree, &retained_tree).unwrap();
        fs::create_dir_all(&sub).unwrap();
        fs::write(&child_file, b"replacement secret").unwrap();

        let mut child_request = command(FileTransferAction::File, "tree-session");
        child_request.file_id = Some("client-child".into());
        child_request.name = Some(child_name);
        let child_output = run_actions(&mut filesystem, manager.handle(child_request));
        let child_data: Vec<_> = child_output
            .iter()
            .flat_map(|packet| decode(packet).data)
            .collect();
        assert_eq!(child_data, b"approved child");

        let mut link_request = command(FileTransferAction::File, "tree-session");
        link_request.file_id = Some("client-link".into());
        link_request.name = Some(relative_name);
        let link_output = run_actions(&mut filesystem, manager.handle(link_request));
        assert_eq!(decode(&link_output[0]).data, b"../root.txt");

        let mut hard_request = command(FileTransferAction::File, "tree-session");
        hard_request.file_id = Some("client-hard".into());
        hard_request.name = Some(hard_name);
        let hard_output = run_actions(&mut filesystem, manager.handle(hard_request));
        assert!(decode(&hard_output[0])
            .status
            .as_deref()
            .is_some_and(|status| status.starts_with("EINVAL:")));
    }
}
