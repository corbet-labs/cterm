use std::io::{self, Read, Write};

use thiserror::Error;

/// Read an entire stream without ever retaining more than `limit` bytes.
pub fn read_bounded(reader: &mut impl Read, limit: usize) -> Result<Vec<u8>, BoundedReadError> {
    let take_limit = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    reader
        .take(take_limit)
        .read_to_end(&mut bytes)
        .map_err(BoundedReadError::Io)?;
    if bytes.len() > limit {
        return Err(BoundedReadError::LimitExceeded { limit });
    }
    Ok(bytes)
}

#[derive(Debug, Error)]
pub enum BoundedReadError {
    #[error("failed to read runner input: {0}")]
    Io(#[source] io::Error),
    #[error("runner input exceeds its {limit}-byte limit")]
    LimitExceeded { limit: usize },
}

#[derive(Debug)]
pub(crate) struct BoundedOutput {
    bytes: Vec<u8>,
    limit: usize,
    overflowed: bool,
}

impl BoundedOutput {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            overflowed: false,
        }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) const fn overflowed(&self) -> bool {
        self.overflowed
    }
}

impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.overflowed = true;
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "plugin output limit exceeded",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_reader_never_accepts_a_limit_plus_one_byte() {
        assert_eq!(read_bounded(&mut &b"abcd"[..], 4).unwrap(), b"abcd");
        assert!(matches!(
            read_bounded(&mut &b"abcde"[..], 4),
            Err(BoundedReadError::LimitExceeded { limit: 4 })
        ));
    }

    #[test]
    fn bounded_writer_retains_no_bytes_beyond_the_limit() {
        let mut output = BoundedOutput::new(4);
        output.write_all(b"abcd").unwrap();
        assert!(output.write_all(b"e").is_err());
        assert_eq!(output.bytes(), b"abcd");
        assert!(output.overflowed());
    }
}
