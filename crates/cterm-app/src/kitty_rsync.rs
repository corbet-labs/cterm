//! Bounded implementation of Kitty's rsync/XXH3 wire format.
//!
//! The rolling matcher and queued-copy structure follow the mature Rust
//! implementation in Dropbox's Apache-2.0 `fast_rsync`; Kitty's distinct
//! little-endian records and XXH3 hashes follow the published protocol. Input
//! and output remain streaming so the transfer file-size limit is not also a
//! memory-allocation limit.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};

use twox_hash::{XxHash3_128, XxHash3_64};

const SIGNATURE_HEADER_BYTES: usize = 12;
const BLOCK_HASH_BYTES: usize = 20;
const MAX_BLOCK_BYTES: usize = 1024 * 1024;
const DEFAULT_BLOCK_BYTES: usize = 6 * 1024;
const HASH_BLOCK_BYTES: usize = 64;
const MAX_LITERAL_BYTES: usize = 64 * 1024;
const OUTPUT_HASH_BYTES: usize = 16;

const OP_BLOCK: u8 = 0;
const OP_DATA: u8 = 1;
const OP_HASH: u8 = 2;
const OP_BLOCK_RANGE: u8 = 3;

/// Parsed, indexed Kitty signature. Duplicate content hashes retain the first
/// block index because either identical block reconstructs the same bytes.
#[derive(Debug)]
pub(crate) struct Signature {
    block_size: usize,
    blocks: HashMap<u32, HashMap<u64, u64>>,
}

impl Signature {
    pub(crate) fn parse(data: &[u8], max_bytes: usize) -> io::Result<Self> {
        if data.len() > max_bytes {
            return Err(invalid_data("rsync signature exceeds its configured limit"));
        }
        if data.len() < SIGNATURE_HEADER_BYTES {
            return Err(unexpected_eof("rsync signature header is truncated"));
        }
        if !(data.len() - SIGNATURE_HEADER_BYTES).is_multiple_of(BLOCK_HASH_BYTES) {
            return Err(unexpected_eof("rsync signature block is truncated"));
        }
        for (offset, name) in [
            (0, "version"),
            (2, "checksum type"),
            (4, "strong hash type"),
            (6, "weak hash type"),
        ] {
            if u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap()) != 0 {
                return Err(invalid_data(format!("unsupported rsync {name}")));
            }
        }
        let block_size = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
        if block_size == 0 || block_size > MAX_BLOCK_BYTES {
            return Err(invalid_data(
                "rsync block size is outside the supported range",
            ));
        }

        let (records, remainder) = data[SIGNATURE_HEADER_BYTES..].as_chunks::<BLOCK_HASH_BYTES>();
        debug_assert!(remainder.is_empty());
        let mut blocks: HashMap<u32, HashMap<u64, u64>> = HashMap::new();
        for record in records {
            let index = u64::from_le_bytes(record[0..8].try_into().unwrap());
            let weak = u32::from_le_bytes(record[8..12].try_into().unwrap());
            let strong = u64::from_le_bytes(record[12..20].try_into().unwrap());
            blocks
                .entry(weak)
                .or_default()
                .entry(strong)
                .or_insert(index);
        }
        Ok(Self { block_size, blocks })
    }

    fn find(&self, weak: u32, window: &[u8]) -> Option<u64> {
        let candidates = self.blocks.get(&weak)?;
        candidates.get(&XxHash3_64::oneshot(window)).copied()
    }

    pub(crate) fn block_size(&self) -> usize {
        self.block_size
    }
}

/// Generate Kitty's serialized signature without buffering the source file.
pub(crate) fn write_signature(
    mut source: impl Read,
    expected_size: u64,
    mut output: impl Write,
) -> io::Result<u64> {
    let block_size = signature_block_size(expected_size);
    let mut header = [0_u8; SIGNATURE_HEADER_BYTES];
    header[8..].copy_from_slice(&(block_size as u32).to_le_bytes());
    output.write_all(&header)?;
    let mut written = header.len() as u64;
    let mut consumed = 0_u64;
    let mut index = 0_u64;
    let mut block = vec![0_u8; block_size];
    loop {
        let length = read_up_to(&mut source, &mut block)?;
        if length == 0 {
            break;
        }
        consumed = consumed
            .checked_add(length as u64)
            .ok_or_else(|| invalid_data("rsync source size overflow"))?;
        let contents = &block[..length];
        let mut record = [0_u8; BLOCK_HASH_BYTES];
        record[..8].copy_from_slice(&index.to_le_bytes());
        record[8..12].copy_from_slice(&RollingChecksum::new(contents).digest().to_le_bytes());
        record[12..].copy_from_slice(&XxHash3_64::oneshot(contents).to_le_bytes());
        output.write_all(&record)?;
        written += record.len() as u64;
        index = index
            .checked_add(1)
            .ok_or_else(|| invalid_data("rsync block index overflow"))?;
    }
    if consumed != expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rsync source changed while its signature was generated",
        ));
    }
    Ok(written)
}

/// Create a Kitty delta from a source stream and a previously supplied
/// signature. At most one block, one literal chunk, and the reader's own
/// buffer are live at a time.
pub(crate) fn write_delta(
    source: impl Read,
    signature: &Signature,
    mut output: impl Write,
) -> io::Result<u64> {
    let mut source = io::BufReader::with_capacity(128 * 1024, source);
    let mut emitter = DeltaEmitter::new(&mut output);
    let mut checksum = XxHash3_128::new();
    let mut window = vec![0_u8; signature.block_size];
    let mut window_len = read_up_to(&mut source, &mut window)?;
    checksum.write(&window[..window_len]);
    let mut literal = Vec::with_capacity(MAX_LITERAL_BYTES + signature.block_size);

    if window_len == signature.block_size {
        let mut head = 0_usize;
        let mut weak = RollingChecksum::new(&window);
        let mut contiguous = vec![0_u8; signature.block_size];
        loop {
            let candidate = if head == 0 {
                signature.find(weak.digest(), &window)
            } else {
                let split = signature.block_size - head;
                contiguous[..split].copy_from_slice(&window[head..]);
                contiguous[split..].copy_from_slice(&window[..head]);
                signature.find(weak.digest(), &contiguous)
            };
            if let Some(index) = candidate {
                emitter.data(&mut literal)?;
                emitter.block(index)?;
                window_len = read_up_to(&mut source, &mut window)?;
                checksum.write(&window[..window_len]);
                head = 0;
                if window_len < signature.block_size {
                    literal.extend_from_slice(&window[..window_len]);
                    break;
                }
                weak = RollingChecksum::new(&window);
                continue;
            }

            let mut incoming = [0_u8; 1];
            if read_up_to(&mut source, &mut incoming)? == 0 {
                append_ring(&mut literal, &window, head);
                break;
            }
            checksum.write(&incoming);
            let outgoing = window[head];
            literal.push(outgoing);
            window[head] = incoming[0];
            head = (head + 1) % signature.block_size;
            weak.roll(outgoing, incoming[0]);
            if literal.len() >= MAX_LITERAL_BYTES {
                emitter.data(&mut literal)?;
            }
        }
    } else {
        literal.extend_from_slice(&window[..window_len]);
    }

    emitter.data(&mut literal)?;
    emitter.hash(checksum.finish_128().to_be_bytes())?;
    emitter.finish()
}

/// Streaming, bounded delta applier used by incoming transfers.
pub(crate) struct DeltaPatcher<R, W> {
    base: R,
    output: W,
    base_size: u64,
    block_size: u64,
    output_limit: u64,
    bytes_written: u64,
    checksum: XxHash3_128,
    pending: Vec<u8>,
    data_remaining: u64,
    checksum_seen: bool,
}

impl<R, W> fmt::Debug for DeltaPatcher<R, W> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeltaPatcher")
            .field("base_size", &self.base_size)
            .field("block_size", &self.block_size)
            .field("output_limit", &self.output_limit)
            .field("bytes_written", &self.bytes_written)
            .field("pending_bytes", &self.pending.len())
            .field("data_remaining", &self.data_remaining)
            .field("checksum_seen", &self.checksum_seen)
            .finish_non_exhaustive()
    }
}

impl<R: Read + Seek, W: Write> DeltaPatcher<R, W> {
    pub(crate) fn new(
        base: R,
        output: W,
        base_size: u64,
        block_size: usize,
        output_limit: u64,
    ) -> io::Result<Self> {
        if block_size == 0 || block_size > MAX_BLOCK_BYTES {
            return Err(invalid_data(
                "rsync block size is outside the supported range",
            ));
        }
        Ok(Self {
            base,
            output,
            base_size,
            block_size: block_size as u64,
            output_limit,
            bytes_written: 0,
            checksum: XxHash3_128::new(),
            pending: Vec::with_capacity(3 + OUTPUT_HASH_BYTES),
            data_remaining: 0,
            checksum_seen: false,
        })
    }

    pub(crate) fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    pub(crate) fn set_output_limit(&mut self, limit: u64) {
        self.output_limit = limit;
    }

    pub(crate) fn finish(mut self) -> io::Result<W> {
        if self.data_remaining != 0 || !self.pending.is_empty() {
            return Err(unexpected_eof("rsync delta operation is truncated"));
        }
        if !self.checksum_seen {
            return Err(unexpected_eof("rsync delta has no final checksum"));
        }
        self.output.flush()?;
        Ok(self.output)
    }

    fn consume(&mut self, mut input: &[u8]) -> io::Result<()> {
        if self.checksum_seen && !input.is_empty() {
            return Err(invalid_data("rsync delta has data after its checksum"));
        }
        while !input.is_empty() {
            if self.data_remaining != 0 {
                let length = usize::try_from(self.data_remaining.min(input.len() as u64))
                    .expect("length is bounded by the input slice");
                self.write_output(&input[..length])?;
                self.data_remaining -= length as u64;
                input = &input[length..];
                continue;
            }

            self.pending.push(input[0]);
            input = &input[1..];
            let needed = operation_bytes_needed(&self.pending)?;
            if self.pending.len() < needed {
                continue;
            }
            let operation = std::mem::take(&mut self.pending);
            match operation[0] {
                OP_BLOCK => {
                    let index = u64::from_le_bytes(operation[1..9].try_into().unwrap());
                    self.copy_blocks(index, 0)?;
                }
                OP_BLOCK_RANGE => {
                    let index = u64::from_le_bytes(operation[1..9].try_into().unwrap());
                    let additional = u32::from_le_bytes(operation[9..13].try_into().unwrap());
                    self.copy_blocks(index, additional)?;
                }
                OP_DATA => {
                    self.data_remaining =
                        u32::from_le_bytes(operation[1..5].try_into().unwrap()) as u64;
                    if self.data_remaining > self.output_limit.saturating_sub(self.bytes_written) {
                        return Err(output_too_large());
                    }
                }
                OP_HASH => {
                    let length = u16::from_le_bytes(operation[1..3].try_into().unwrap()) as usize;
                    if length != OUTPUT_HASH_BYTES {
                        return Err(invalid_data("rsync delta checksum must contain 16 bytes"));
                    }
                    let expected: [u8; OUTPUT_HASH_BYTES] = operation[3..].try_into().unwrap();
                    if self.checksum.finish_128().to_be_bytes() != expected {
                        return Err(invalid_data(
                            "rsync delta checksum does not match its output",
                        ));
                    }
                    self.checksum_seen = true;
                    if !input.is_empty() {
                        return Err(invalid_data("rsync delta has data after its checksum"));
                    }
                }
                _ => unreachable!("operation type was validated"),
            }
        }
        Ok(())
    }

    fn copy_blocks(&mut self, index: u64, additional: u32) -> io::Result<()> {
        let end_index = index
            .checked_add(additional as u64)
            .ok_or_else(|| invalid_data("rsync block range overflows"))?;
        let offset = index
            .checked_mul(self.block_size)
            .ok_or_else(|| invalid_data("rsync block offset overflows"))?;
        let block_count = self.base_size.div_ceil(self.block_size);
        if end_index >= block_count {
            return Err(invalid_data(
                "rsync delta references a block outside the base file",
            ));
        }
        let end = end_index
            .checked_add(1)
            .and_then(|value| value.checked_mul(self.block_size))
            .ok_or_else(|| invalid_data("rsync block range overflows"))?
            .min(self.base_size);
        if offset >= self.base_size || end <= offset {
            return Err(invalid_data(
                "rsync delta references a block outside the base file",
            ));
        }
        let length = end - offset;
        if length > self.output_limit.saturating_sub(self.bytes_written) {
            return Err(output_too_large());
        }
        self.base.seek(SeekFrom::Start(offset))?;
        let mut remaining = length;
        let mut buffer = [0_u8; 32 * 1024];
        while remaining != 0 {
            let requested = usize::try_from(remaining.min(buffer.len() as u64))
                .expect("copy length is bounded by the buffer");
            self.base.read_exact(&mut buffer[..requested])?;
            self.write_output(&buffer[..requested])?;
            remaining -= requested as u64;
        }
        Ok(())
    }

    fn write_output(&mut self, data: &[u8]) -> io::Result<()> {
        let next = self
            .bytes_written
            .checked_add(data.len() as u64)
            .filter(|value| *value <= self.output_limit)
            .ok_or_else(output_too_large)?;
        self.output.write_all(data)?;
        self.checksum.write(data);
        self.bytes_written = next;
        Ok(())
    }
}

impl<R: Read + Seek, W: Write> Write for DeltaPatcher<R, W> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.consume(input)?;
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

#[derive(Debug, Clone, Copy)]
struct RollingChecksum {
    alpha: u16,
    beta: u16,
    length: u16,
}

impl RollingChecksum {
    fn new(data: &[u8]) -> Self {
        let length = data.len() as u16;
        let mut alpha = 0_u16;
        let mut beta = 0_u16;
        for (index, byte) in data.iter().copied().enumerate() {
            alpha = alpha.wrapping_add(byte as u16);
            beta = beta.wrapping_add(length.wrapping_sub(index as u16).wrapping_mul(byte as u16));
        }
        Self {
            alpha,
            beta,
            length,
        }
    }

    fn roll(&mut self, outgoing: u8, incoming: u8) {
        self.alpha = self
            .alpha
            .wrapping_sub(outgoing as u16)
            .wrapping_add(incoming as u16);
        self.beta = self
            .beta
            .wrapping_sub(self.length.wrapping_mul(outgoing as u16))
            .wrapping_add(self.alpha);
    }

    fn digest(self) -> u32 {
        self.alpha as u32 | (self.beta as u32) << 16
    }
}

struct DeltaEmitter<W> {
    output: W,
    bytes_written: u64,
    pending_blocks: Option<(u64, u64)>,
}

impl<W: Write> DeltaEmitter<W> {
    fn new(output: W) -> Self {
        Self {
            output,
            bytes_written: 0,
            pending_blocks: None,
        }
    }

    fn block(&mut self, index: u64) -> io::Result<()> {
        match &mut self.pending_blocks {
            Some((_, end)) if end.checked_add(1) == Some(index) => *end = index,
            Some(_) => {
                self.flush_blocks()?;
                self.pending_blocks = Some((index, index));
            }
            None => self.pending_blocks = Some((index, index)),
        }
        Ok(())
    }

    fn data(&mut self, data: &mut Vec<u8>) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.flush_blocks()?;
        for chunk in data.chunks(MAX_LITERAL_BYTES) {
            self.write_all(&[OP_DATA])?;
            self.write_all(&(chunk.len() as u32).to_le_bytes())?;
            self.write_all(chunk)?;
        }
        data.clear();
        Ok(())
    }

    fn hash(&mut self, checksum: [u8; OUTPUT_HASH_BYTES]) -> io::Result<()> {
        self.flush_blocks()?;
        self.write_all(&[OP_HASH])?;
        self.write_all(&(OUTPUT_HASH_BYTES as u16).to_le_bytes())?;
        self.write_all(&checksum)
    }

    fn finish(mut self) -> io::Result<u64> {
        self.flush_blocks()?;
        self.output.flush()?;
        Ok(self.bytes_written)
    }

    fn flush_blocks(&mut self) -> io::Result<()> {
        let Some((start, end)) = self.pending_blocks.take() else {
            return Ok(());
        };
        if start == end {
            self.write_all(&[OP_BLOCK])?;
            self.write_all(&start.to_le_bytes())
        } else {
            let additional = u32::try_from(end - start)
                .map_err(|_| invalid_data("rsync block range is too large"))?;
            self.write_all(&[OP_BLOCK_RANGE])?;
            self.write_all(&start.to_le_bytes())?;
            self.write_all(&additional.to_le_bytes())
        }
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.output.write_all(data)?;
        self.bytes_written = self
            .bytes_written
            .checked_add(data.len() as u64)
            .ok_or_else(|| invalid_data("rsync delta size overflow"))?;
        Ok(())
    }
}

fn signature_block_size(size: u64) -> usize {
    if size == 0 {
        return DEFAULT_BLOCK_BYTES;
    }
    let root = (size as f64).sqrt().round() as usize;
    root.clamp(HASH_BLOCK_BYTES, MAX_BLOCK_BYTES) / HASH_BLOCK_BYTES * HASH_BLOCK_BYTES
}

fn operation_bytes_needed(pending: &[u8]) -> io::Result<usize> {
    let Some(operation) = pending.first().copied() else {
        return Ok(1);
    };
    match operation {
        OP_BLOCK => Ok(9),
        OP_DATA => Ok(5),
        OP_BLOCK_RANGE => Ok(13),
        OP_HASH if pending.len() < 3 => Ok(3),
        OP_HASH => {
            let length = u16::from_le_bytes(pending[1..3].try_into().unwrap()) as usize;
            if length != OUTPUT_HASH_BYTES {
                return Err(invalid_data("rsync delta checksum must contain 16 bytes"));
            }
            Ok(3 + length)
        }
        _ => Err(invalid_data("rsync delta has an unknown operation")),
    }
}

fn read_up_to(mut source: impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match source.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(length) => filled += length,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

fn append_ring(output: &mut Vec<u8>, ring: &[u8], head: usize) {
    output.extend_from_slice(&ring[head..]);
    output.extend_from_slice(&ring[..head]);
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn unexpected_eof(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, message)
}

fn output_too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        "rsync delta exceeds the output limit",
    )
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn signature(data: &[u8], block_size: usize) -> Signature {
        let mut serialized = vec![0_u8; SIGNATURE_HEADER_BYTES];
        serialized[8..12].copy_from_slice(&(block_size as u32).to_le_bytes());
        for (index, block) in data.chunks(block_size).enumerate() {
            serialized.extend_from_slice(&(index as u64).to_le_bytes());
            serialized.extend_from_slice(&RollingChecksum::new(block).digest().to_le_bytes());
            serialized.extend_from_slice(&XxHash3_64::oneshot(block).to_le_bytes());
        }
        Signature::parse(&serialized, serialized.len()).unwrap()
    }

    fn roundtrip(base: &[u8], source: &[u8], block_size: usize) -> Vec<u8> {
        let signature = signature(base, block_size);
        let mut delta = Vec::new();
        write_delta(Cursor::new(source), &signature, &mut delta).unwrap();
        let mut patcher = DeltaPatcher::new(
            Cursor::new(base),
            Vec::new(),
            base.len() as u64,
            block_size,
            source.len() as u64,
        )
        .unwrap();
        for chunk in delta.chunks(7) {
            patcher.write_all(chunk).unwrap();
        }
        patcher.finish().unwrap()
    }

    #[test]
    fn hashes_match_kitty_reference_vectors() {
        assert_eq!(XxHash3_64::oneshot(b"abcd"), 7_248_448_420_886_124_688);
        assert_eq!(
            XxHash3_128::oneshot(b"abcd").to_be_bytes(),
            hex("8d6b60383dfa90c21be79eecd1b1353d")
        );
    }

    #[test]
    fn signature_is_little_endian_and_strictly_bounded() {
        let mut serialized = Vec::new();
        write_signature(Cursor::new(b"abcdefgh"), 8, &mut serialized).unwrap();
        assert_eq!(&serialized[..8], &[0; 8]);
        assert_eq!(
            u32::from_le_bytes(serialized[8..12].try_into().unwrap()),
            64
        );
        assert_eq!(
            u64::from_le_bytes(serialized[12..20].try_into().unwrap()),
            0
        );
        assert!(Signature::parse(&serialized, serialized.len() - 1).is_err());
        serialized.push(0);
        assert!(Signature::parse(&serialized, serialized.len()).is_err());
    }

    #[test]
    fn kitty_style_roundtrips_cover_shifts_patches_and_trailers() {
        let block_size = 16;
        let mut base = Vec::new();
        for index in 0..16 {
            let mut block = vec![b'_'; block_size];
            let label = index.to_string();
            block[..label.len()].copy_from_slice(label.as_bytes());
            base.extend(block);
        }
        let mut changed = base.clone();
        changed[3..9].copy_from_slice(b"patch1");
        changed[130..135].copy_from_slice(b"ptch3");
        assert_eq!(roundtrip(&base, &base, block_size), base);
        assert_eq!(roundtrip(&base, &changed, block_size), changed);
        assert_eq!(roundtrip(&base[block_size..], &base, block_size), base);
        assert_eq!(
            roundtrip(&base, &base[..base.len() - 3], block_size),
            base[..base.len() - 3]
        );
        let mut extended = changed.clone();
        extended.extend_from_slice(b"trailer");
        assert_eq!(roundtrip(&base, &extended, block_size), extended);
        assert!(roundtrip(&[], &[], block_size).is_empty());
    }

    #[test]
    fn delta_rejects_truncation_bad_ranges_hashes_and_output_overflow() {
        let base = b"abcdefgh";
        for delta in [
            vec![OP_BLOCK],
            [vec![OP_BLOCK], u64::MAX.to_le_bytes().to_vec()].concat(),
            [
                vec![OP_BLOCK_RANGE],
                1_u64.to_le_bytes().to_vec(),
                1_u32.to_le_bytes().to_vec(),
            ]
            .concat(),
            vec![OP_DATA, 4, 0, 0, 0, b'a'],
            [vec![OP_HASH, 16, 0], vec![0; 16]].concat(),
        ] {
            let mut patcher = DeltaPatcher::new(Cursor::new(base), Vec::new(), 8, 4, 8).unwrap();
            let result = patcher.write_all(&delta).and_then(|()| patcher.finish());
            assert!(result.is_err(), "accepted malformed delta: {delta:?}");
        }

        let valid_signature = signature(base, 4);
        let mut delta = Vec::new();
        write_delta(Cursor::new(b"abcdefgh!"), &valid_signature, &mut delta).unwrap();
        let mut patcher = DeltaPatcher::new(Cursor::new(base), Vec::new(), 8, 4, 8).unwrap();
        assert!(patcher.write_all(&delta).is_err());
    }

    fn hex(value: &str) -> [u8; 16] {
        let mut output = [0_u8; 16];
        let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
        assert!(remainder.is_empty());
        for (index, pair) in pairs.iter().enumerate() {
            output[index] = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
        }
        output
    }
}
