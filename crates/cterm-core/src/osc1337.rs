//! Bounded, streaming interception for iTerm2 OSC 1337 file transfers.
//!
//! The all-or-none prefix interception and split/recovery test structure are
//! adapted from Zellij's Kitty APC interceptor at revision
//! e839bfffa586992364309a685b2c71f3b23c247e (MIT).

use crate::iterm2::Iterm2FileParams;
use crate::streaming_file::{StreamingFileReceiver, StreamingFileResult};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_PARAMS_BYTES: usize = 64 * 1024;
const REPLAY_MEMORY_BYTES: usize = 64 * 1024;
const PREFIX_AFTER_ESC: &[u8] = b"]1337;File=";
static NEXT_REPLAY_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
enum ReplayStorage {
    Memory(Vec<u8>),
    File {
        path: PathBuf,
        writer: BufWriter<File>,
    },
}

#[derive(Debug)]
pub(crate) struct ReplayBuffer {
    storage: ReplayStorage,
    fallback: Vec<u8>,
}

impl ReplayBuffer {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            storage: ReplayStorage::Memory(bytes),
            fallback: Vec::new(),
        }
    }

    fn push(&mut self, byte: u8) -> io::Result<()> {
        if let ReplayStorage::Memory(bytes) = &mut self.storage {
            if bytes.len() < REPLAY_MEMORY_BYTES {
                bytes.push(byte);
                return Ok(());
            }
        } else if let ReplayStorage::File { writer, .. } = &mut self.storage {
            let result = writer.write_all(&[byte]);
            if result.is_err() {
                self.fallback.push(byte);
            }
            return result;
        }

        let ReplayStorage::Memory(bytes) =
            std::mem::replace(&mut self.storage, ReplayStorage::Memory(Vec::new()))
        else {
            unreachable!("file storage returned above");
        };
        let (path, mut writer) = match create_replay_file() {
            Ok(file) => file,
            Err(error) => {
                self.storage = ReplayStorage::Memory(bytes);
                self.fallback.push(byte);
                return Err(error);
            }
        };
        if let Err(error) = writer
            .write_all(&bytes)
            .and_then(|()| writer.write_all(&[byte]))
        {
            drop(writer);
            let _ = std::fs::remove_file(path);
            self.storage = ReplayStorage::Memory(bytes);
            self.fallback.push(byte);
            return Err(error);
        }
        self.storage = ReplayStorage::File { path, writer };
        Ok(())
    }

    pub(crate) fn replay(mut self, mut forward: impl FnMut(&[u8])) -> io::Result<()> {
        let storage = std::mem::replace(&mut self.storage, ReplayStorage::Memory(Vec::new()));
        match storage {
            ReplayStorage::Memory(bytes) => forward(&bytes),
            ReplayStorage::File { path, mut writer } => {
                let flush_result = writer.flush();
                drop(writer);
                if let Err(error) = flush_result {
                    let _ = std::fs::remove_file(path);
                    return Err(error);
                }
                let result: io::Result<()> = (|| {
                    let mut file = File::open(&path)?;
                    let mut buffer = [0; 16 * 1024];
                    loop {
                        let read = file.read(&mut buffer)?;
                        if read == 0 {
                            break;
                        }
                        forward(&buffer[..read]);
                    }
                    Ok(())
                })();
                let _ = std::fs::remove_file(path);
                result?;
            }
        }
        forward(&self.fallback);
        Ok(())
    }
}

impl Drop for ReplayBuffer {
    fn drop(&mut self) {
        let storage = std::mem::replace(&mut self.storage, ReplayStorage::Memory(Vec::new()));
        if let ReplayStorage::File { path, writer } = storage {
            drop(writer);
            let _ = std::fs::remove_file(path);
        }
    }
}

fn create_replay_file() -> io::Result<(PathBuf, BufWriter<File>)> {
    for _ in 0..100 {
        let id = NEXT_REPLAY_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cterm-osc1337-replay-{}-{id}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, BufWriter::new(file))),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique OSC 1337 replay file",
    ))
}

#[derive(Debug, Default)]
enum State {
    #[default]
    Ground,
    Escape,
    Prefix {
        matched: usize,
        raw: Vec<u8>,
    },
    Params {
        raw: Vec<u8>,
        params: Vec<u8>,
    },
    ParamsEscape {
        raw: Vec<u8>,
    },
    Passthrough,
    PassthroughEscape,
    Streaming {
        receiver: StreamingFileReceiver,
        replay: ReplayBuffer,
    },
    StreamingEscape {
        receiver: StreamingFileReceiver,
        replay: ReplayBuffer,
    },
}

#[derive(Debug, Default)]
pub(crate) struct Osc1337Interceptor {
    state: State,
}

pub(crate) enum ForwardBytes {
    One([u8; 1]),
    Two([u8; 2]),
    Buffered(Vec<u8>),
}

impl ForwardBytes {
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Self::One(bytes) => bytes,
            Self::Two(bytes) => bytes,
            Self::Buffered(bytes) => bytes,
        }
    }
}

pub(crate) enum InterceptorResult {
    Forward(ForwardBytes),
    Replay(ReplayBuffer),
    Swallow,
    Finished(StreamingFileResult),
}

impl Osc1337Interceptor {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn advance(&mut self, byte: u8) -> InterceptorResult {
        let state = std::mem::take(&mut self.state);
        match state {
            State::Ground => self.advance_ground(byte),
            State::Escape => self.advance_escape(byte),
            State::Prefix { matched, raw } => self.advance_prefix(byte, matched, raw),
            State::Params { raw, params } => self.advance_params(byte, raw, params),
            State::ParamsEscape { raw } => self.advance_params_escape(byte, raw),
            State::Passthrough => self.advance_passthrough(byte),
            State::PassthroughEscape => self.advance_passthrough_escape(byte),
            State::Streaming { receiver, replay } => self.advance_streaming(byte, receiver, replay),
            State::StreamingEscape { receiver, replay } => {
                self.advance_streaming_escape(byte, receiver, replay)
            }
        }
    }

    fn advance_ground(&mut self, byte: u8) -> InterceptorResult {
        if byte == 0x1b {
            self.state = State::Escape;
            InterceptorResult::Swallow
        } else {
            InterceptorResult::Forward(ForwardBytes::One([byte]))
        }
    }

    fn advance_escape(&mut self, byte: u8) -> InterceptorResult {
        match byte {
            b']' => {
                self.state = State::Prefix {
                    matched: 1,
                    raw: vec![0x1b, b']'],
                };
                InterceptorResult::Swallow
            }
            0x1b => {
                self.state = State::Escape;
                InterceptorResult::Forward(ForwardBytes::One([0x1b]))
            }
            _ => InterceptorResult::Forward(ForwardBytes::Two([0x1b, byte])),
        }
    }

    fn advance_prefix(&mut self, byte: u8, matched: usize, mut raw: Vec<u8>) -> InterceptorResult {
        raw.push(byte);
        if PREFIX_AFTER_ESC.get(matched) == Some(&byte) {
            let next = matched + 1;
            if next == PREFIX_AFTER_ESC.len() {
                self.state = State::Params {
                    raw,
                    params: Vec::new(),
                };
            } else {
                self.state = State::Prefix { matched: next, raw };
            }
            return InterceptorResult::Swallow;
        }

        self.state = state_after_osc_replay(byte);
        InterceptorResult::Forward(ForwardBytes::Buffered(raw))
    }

    fn advance_params(
        &mut self,
        byte: u8,
        mut raw: Vec<u8>,
        mut params: Vec<u8>,
    ) -> InterceptorResult {
        match byte {
            b':' => {
                raw.push(byte);
                let Ok(params) = std::str::from_utf8(&params) else {
                    self.state = State::Passthrough;
                    return InterceptorResult::Forward(ForwardBytes::Buffered(raw));
                };
                let params = Iterm2FileParams::parse(params);
                log::debug!(
                    "OSC 1337 File streaming: name={:?}, size={:?}, inline={}",
                    params.name,
                    params.size,
                    params.inline
                );
                self.state = State::Streaming {
                    receiver: StreamingFileReceiver::new(params),
                    replay: ReplayBuffer::new(raw),
                };
                InterceptorResult::Swallow
            }
            0x1b => {
                raw.push(byte);
                self.state = State::ParamsEscape { raw };
                InterceptorResult::Swallow
            }
            0x07 | 0x18 | 0x1a | 0x9c => {
                raw.push(byte);
                InterceptorResult::Forward(ForwardBytes::Buffered(raw))
            }
            _ if params.len() == MAX_PARAMS_BYTES => {
                raw.push(byte);
                self.state = State::Passthrough;
                InterceptorResult::Forward(ForwardBytes::Buffered(raw))
            }
            _ => {
                raw.push(byte);
                params.push(byte);
                self.state = State::Params { raw, params };
                InterceptorResult::Swallow
            }
        }
    }

    fn advance_params_escape(&mut self, byte: u8, mut raw: Vec<u8>) -> InterceptorResult {
        raw.push(byte);
        match byte {
            b'\\' | 0x07 | 0x18 | 0x1a | 0x9c => {}
            b']' => self.state = State::Passthrough,
            _ => {}
        }
        InterceptorResult::Forward(ForwardBytes::Buffered(raw))
    }

    fn advance_passthrough(&mut self, byte: u8) -> InterceptorResult {
        self.state = match byte {
            0x1b => State::PassthroughEscape,
            0x07 | 0x18 | 0x1a | 0x9c => State::Ground,
            _ => State::Passthrough,
        };
        InterceptorResult::Forward(ForwardBytes::One([byte]))
    }

    fn advance_passthrough_escape(&mut self, byte: u8) -> InterceptorResult {
        self.state = match byte {
            b'\\' | 0x07 | 0x18 | 0x1a | 0x9c => State::Ground,
            0x1b => State::PassthroughEscape,
            b']' => State::Passthrough,
            _ => State::Ground,
        };
        InterceptorResult::Forward(ForwardBytes::One([byte]))
    }

    fn advance_streaming(
        &mut self,
        byte: u8,
        mut receiver: StreamingFileReceiver,
        mut replay: ReplayBuffer,
    ) -> InterceptorResult {
        match byte {
            0x07 | 0x9c => self.finish_streaming(receiver, replay, byte),
            0x1b => {
                if let Err(error) = replay.push(byte) {
                    log::warn!("OSC 1337 replay buffering failed: {error}");
                    self.state = State::PassthroughEscape;
                    return InterceptorResult::Replay(replay);
                }
                self.state = State::StreamingEscape { receiver, replay };
                InterceptorResult::Swallow
            }
            0x18 | 0x1a => {
                let _ = replay.push(byte);
                InterceptorResult::Replay(replay)
            }
            _ => match replay.push(byte) {
                Err(error) => {
                    log::warn!("OSC 1337 replay buffering failed: {error}");
                    self.state = State::Passthrough;
                    InterceptorResult::Replay(replay)
                }
                Ok(()) if receiver.put(byte) => {
                    self.state = State::Streaming { receiver, replay };
                    InterceptorResult::Swallow
                }
                Ok(()) => {
                    log::warn!("OSC 1337 streaming error: {:?}", receiver.error());
                    self.state = State::Passthrough;
                    InterceptorResult::Replay(replay)
                }
            },
        }
    }

    fn advance_streaming_escape(
        &mut self,
        byte: u8,
        receiver: StreamingFileReceiver,
        mut replay: ReplayBuffer,
    ) -> InterceptorResult {
        match byte {
            b'\\' => self.finish_streaming(receiver, replay, byte),
            0x18 | 0x1a => {
                let _ = replay.push(byte);
                InterceptorResult::Replay(replay)
            }
            _ => {
                let _ = replay.push(byte);
                log::warn!("Malformed OSC 1337 File payload; replaying through VTE");
                self.state = state_after_escaped_osc_replay(byte);
                InterceptorResult::Replay(replay)
            }
        }
    }

    fn finish_streaming(
        &mut self,
        receiver: StreamingFileReceiver,
        mut replay: ReplayBuffer,
        terminator: u8,
    ) -> InterceptorResult {
        let _ = replay.push(terminator);
        match receiver.finish() {
            Ok(result) => InterceptorResult::Finished(result),
            Err(error) => {
                log::warn!("OSC 1337 File streaming failed: {error}; replaying through VTE");
                InterceptorResult::Replay(replay)
            }
        }
    }
}

fn state_after_osc_replay(byte: u8) -> State {
    match byte {
        0x07 | 0x18 | 0x1a | 0x9c => State::Ground,
        0x1b => State::PassthroughEscape,
        _ => State::Passthrough,
    }
}

fn state_after_escaped_osc_replay(byte: u8) -> State {
    match byte {
        0x1b => State::PassthroughEscape,
        b']' => State::Passthrough,
        _ => State::Ground,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::create_replay_file;
    use super::{InterceptorResult, Osc1337Interceptor};

    #[derive(Debug, Default, PartialEq, Eq)]
    struct RecordingPerformer {
        events: Vec<String>,
    }

    impl vte::Perform for RecordingPerformer {
        fn print(&mut self, c: char) {
            self.events.push(format!("print({c:?})"));
        }

        fn execute(&mut self, byte: u8) {
            self.events.push(format!("execute({byte})"));
        }

        fn hook(&mut self, params: &vte::Params, intermediates: &[u8], ignore: bool, action: char) {
            let params: Vec<Vec<u16>> = params.iter().map(|param| param.to_vec()).collect();
            self.events.push(format!(
                "hook({params:?},{intermediates:?},{ignore},{action:?})"
            ));
        }

        fn put(&mut self, byte: u8) {
            self.events.push(format!("put({byte})"));
        }

        fn unhook(&mut self) {
            self.events.push("unhook".to_string());
        }

        fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
            let params: Vec<Vec<u8>> = params.iter().map(|param| param.to_vec()).collect();
            self.events
                .push(format!("osc({params:?},{bell_terminated})"));
        }

        fn csi_dispatch(
            &mut self,
            params: &vte::Params,
            intermediates: &[u8],
            ignore: bool,
            action: char,
        ) {
            let params: Vec<Vec<u16>> = params.iter().map(|param| param.to_vec()).collect();
            self.events.push(format!(
                "csi({params:?},{intermediates:?},{ignore},{action:?})"
            ));
        }

        fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
            self.events
                .push(format!("esc({intermediates:?},{ignore},{byte})"));
        }
    }

    #[derive(Debug, Default, PartialEq, Eq)]
    struct Run {
        forwarded: Vec<u8>,
        transfers: Vec<Vec<u8>>,
    }

    fn run(input: &[u8], chunk_size: usize) -> Run {
        let mut interceptor = Osc1337Interceptor::new();
        let mut run = Run::default();

        for chunk in input.chunks(chunk_size) {
            for &byte in chunk {
                match interceptor.advance(byte) {
                    InterceptorResult::Forward(bytes) => {
                        run.forwarded.extend_from_slice(bytes.as_slice());
                    }
                    InterceptorResult::Replay(bytes) => bytes
                        .replay(|chunk| run.forwarded.extend_from_slice(chunk))
                        .expect("replay buffered sequence"),
                    InterceptorResult::Swallow => {}
                    InterceptorResult::Finished(result) => {
                        run.transfers
                            .push(result.data.take().expect("read streamed transfer"));
                    }
                }
            }
        }

        run
    }

    fn run_through_vte(input: &[u8], chunk_size: usize) -> (usize, RecordingPerformer) {
        let mut interceptor = Osc1337Interceptor::new();
        let mut parser = vte::Parser::new();
        let mut performer = RecordingPerformer::default();
        let mut transfers = 0;

        for chunk in input.chunks(chunk_size) {
            for &byte in chunk {
                match interceptor.advance(byte) {
                    InterceptorResult::Forward(bytes) => {
                        for &forwarded in bytes.as_slice() {
                            parser.advance(&mut performer, forwarded);
                        }
                    }
                    InterceptorResult::Replay(bytes) => bytes
                        .replay(|chunk| {
                            for &forwarded in chunk {
                                parser.advance(&mut performer, forwarded);
                            }
                        })
                        .expect("replay buffered sequence"),
                    InterceptorResult::Swallow => {}
                    InterceptorResult::Finished(_) => {
                        transfers += 1;
                    }
                }
            }
        }

        (transfers, performer)
    }

    fn run_bare(input: &[u8]) -> RecordingPerformer {
        let mut parser = vte::Parser::new();
        let mut performer = RecordingPerformer::default();
        for &byte in input {
            parser.advance(&mut performer, byte);
        }
        performer
    }

    #[test]
    fn split_invariance_across_arbitrary_chunk_sizes() {
        let corpus = b"hello \x1b[31mred\x1b[0m \x1b]1337;File=inline=0;size=4:AQAAAA==\x1b\\ mid \x1b]0;title\x07 tail";
        let whole = run(corpus, corpus.len());

        for chunk_size in [1, 2, 3, 7, corpus.len()] {
            assert_eq!(run(corpus, chunk_size), whole, "chunk size {chunk_size}");
        }

        assert_eq!(whole.transfers, vec![vec![1, 0, 0, 0]]);
        assert_eq!(
            whole.forwarded,
            b"hello \x1b[31mred\x1b[0m  mid \x1b]0;title\x07 tail"
        );
    }

    #[test]
    fn unhandled_and_malformed_control_strings_are_replayed_losslessly() {
        let inputs: Vec<&[u8]> = vec![
            b"\x1b]0;title\x07after",
            b"\x1b]1337;SetMark\x07after",
            b"\x1b]1337;File=inline=0\x1b\\after",
            b"\x1b]1337;File=bad=\xff:AAAA\x07after",
            b"\x1b]1337;File=inline=0:!!!!\x07after",
            b"\x1b]1337;File=inline=0:AAAA\x1b\x07after",
            b"plain \x1b[31mred\x1b[0m",
        ];

        for input in inputs {
            let result = run(input, 1);
            assert_eq!(result.forwarded, input, "input {input:?}");
            assert!(result.transfers.is_empty(), "input {input:?}");
        }
    }

    #[test]
    fn parameter_overflow_replays_and_recovers() {
        let mut input = Vec::from(&b"\x1b]1337;File="[..]);
        input.extend(std::iter::repeat_n(b'x', super::MAX_PARAMS_BYTES + 1));
        input.extend_from_slice(b":AAAA\x07OK");

        let result = run(&input, 7);
        assert_eq!(result.forwarded, input);
        assert!(result.transfers.is_empty());
    }

    #[test]
    fn large_streams_stay_streaming_and_late_errors_replay_losslessly() {
        let mut valid = Vec::from(&b"\x1b]1337;File=inline=0:"[..]);
        valid.extend(std::iter::repeat_n(b'A', super::REPLAY_MEMORY_BYTES));
        valid.extend_from_slice(b"\x07OK");
        let result = run(&valid, 4093);
        assert_eq!(result.forwarded, b"OK");
        assert_eq!(result.transfers.len(), 1);

        let mut malformed = Vec::from(&b"\x1b]1337;File=inline=0:"[..]);
        malformed.extend(std::iter::repeat_n(b'A', super::REPLAY_MEMORY_BYTES));
        malformed.push(b'!');
        malformed.extend_from_slice(b"\x07OK");
        let result = run(&malformed, 4093);
        assert_eq!(result.forwarded, malformed);
        assert!(result.transfers.is_empty());
    }

    #[test]
    fn can_and_sub_abort_candidates_losslessly_and_recover() {
        for abort in [0x18, 0x1a] {
            for prefix in [&b"\x1b]1337;Fi"[..], &b"\x1b]1337;File=inline=0"[..]] {
                let mut input = prefix.to_vec();
                input.extend_from_slice(&[abort, b'O', b'K']);
                let result = run(&input, 1);
                assert_eq!(result.forwarded, input, "abort {abort:#x}");
                assert!(result.transfers.is_empty());
            }
        }
    }

    #[test]
    fn handled_transfer_is_isolated_from_vte() {
        let input = b"AB\x1b]1337;File=inline=0;size=4:AQAAAA==\x1b\\CD";
        let (transfers, performer) = run_through_vte(input, 1);
        assert_eq!(transfers, 1);
        assert_eq!(performer, run_bare(b"ABCD"));
    }

    #[test]
    fn can_and_sub_abort_streaming_with_bare_vte_semantics() {
        for abort in [0x18, 0x1a] {
            let mut input = b"A\x1b]1337;File=inline=0;size=4:AQ".to_vec();
            input.extend_from_slice(&[abort, b'B']);
            let (transfers, performer) = run_through_vte(&input, 1);
            assert_eq!(transfers, 0, "abort {abort:#x}");
            assert_eq!(performer, run_bare(&input), "abort {abort:#x}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn replay_spill_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let (path, writer) = create_replay_file().unwrap();
        let mode = writer.get_ref().metadata().unwrap().permissions().mode();
        drop(writer);
        std::fs::remove_file(path).unwrap();

        assert_eq!(mode & 0o777, 0o600);
    }
}
