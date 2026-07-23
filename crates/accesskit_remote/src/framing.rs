//! Length-prefixed framing over chunked byte transports.
//!
//! A frame is a 4-byte little-endian payload length followed by the payload.
//! [`FrameReader`] reassembles frames from arbitrarily split chunks, matching
//! DVC delivery semantics where message and chunk boundaries are unrelated.

pub const FRAME_HEADER_LEN: usize = 4;
pub const DEFAULT_MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    Empty,
    TooLarge { len: usize, max: usize },
}

impl core::fmt::Display for FrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => write!(f, "zero-length frame"),
            Self::TooLarge { len, max } => {
                write!(f, "frame of {len} bytes exceeds maximum of {max}")
            }
        }
    }
}

impl std::error::Error for FrameError {}

/// Appends a payload's frame (length header + payload) to `out`, leaving
/// `out` untouched on error.
pub fn frame_into(payload: &[u8], out: &mut Vec<u8>) -> Result<(), FrameError> {
    if payload.is_empty() {
        return Err(FrameError::Empty);
    }
    if payload.len() > DEFAULT_MAX_FRAME_LEN {
        return Err(FrameError::TooLarge {
            len: payload.len(),
            max: DEFAULT_MAX_FRAME_LEN,
        });
    }
    out.reserve(FRAME_HEADER_LEN + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// Wraps a payload in a frame header.
pub fn frame(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let mut out = Vec::new();
    frame_into(payload, &mut out)?;
    Ok(out)
}

/// Reassembles frames from a stream of chunks.
///
/// Feed chunks with [`push`](Self::push), then drain complete frames with
/// [`next_frame`](Self::next_frame) until it returns `Ok(None)`.
#[derive(Debug)]
pub struct FrameReader {
    buf: Vec<u8>,
    pos: usize,
    max_len: usize,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::with_max_len(DEFAULT_MAX_FRAME_LEN)
    }

    pub fn with_max_len(max_len: usize) -> Self {
        Self {
            buf: Vec::new(),
            pos: 0,
            max_len,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let avail = self.buf.len() - self.pos;
        if avail < FRAME_HEADER_LEN {
            self.compact();
            return Ok(None);
        }
        let header: [u8; FRAME_HEADER_LEN] = self.buf[self.pos..self.pos + FRAME_HEADER_LEN]
            .try_into()
            .unwrap();
        let len = u32::from_le_bytes(header) as usize;
        if len == 0 {
            return Err(FrameError::Empty);
        }
        if len > self.max_len {
            return Err(FrameError::TooLarge {
                len,
                max: self.max_len,
            });
        }
        if avail < FRAME_HEADER_LEN + len {
            self.compact();
            return Ok(None);
        }
        let start = self.pos + FRAME_HEADER_LEN;
        let payload = self.buf[start..start + len].to_vec();
        self.pos = start + len;
        Ok(Some(payload))
    }

    fn compact(&mut self) {
        if self.pos > 0 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
    }
}

impl Default for FrameReader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_single_chunk() {
        let framed = frame(b"hello").unwrap();
        let mut reader = FrameReader::new();
        reader.push(&framed);
        assert_eq!(reader.next_frame().unwrap(), Some(b"hello".to_vec()));
        assert_eq!(reader.next_frame().unwrap(), None);
    }

    #[test]
    fn reassembles_from_single_byte_chunks() {
        let framed = frame(b"chunked payload").unwrap();
        let mut reader = FrameReader::new();
        for byte in &framed[..framed.len() - 1] {
            reader.push(std::slice::from_ref(byte));
            assert_eq!(reader.next_frame().unwrap(), None);
        }
        reader.push(&framed[framed.len() - 1..]);
        assert_eq!(reader.next_frame().unwrap(), Some(b"chunked payload".to_vec()));
    }

    #[test]
    fn multiple_frames_in_one_chunk() {
        let mut bytes = frame(b"first").unwrap();
        bytes.extend_from_slice(&frame(b"second").unwrap());
        let mut reader = FrameReader::new();
        reader.push(&bytes);
        assert_eq!(reader.next_frame().unwrap(), Some(b"first".to_vec()));
        assert_eq!(reader.next_frame().unwrap(), Some(b"second".to_vec()));
        assert_eq!(reader.next_frame().unwrap(), None);
    }

    #[test]
    fn frame_and_partial_next_in_one_chunk() {
        let mut bytes = frame(b"complete").unwrap();
        let second = frame(b"incomplete").unwrap();
        bytes.extend_from_slice(&second[..6]);
        let mut reader = FrameReader::new();
        reader.push(&bytes);
        assert_eq!(reader.next_frame().unwrap(), Some(b"complete".to_vec()));
        assert_eq!(reader.next_frame().unwrap(), None);
        reader.push(&second[6..]);
        assert_eq!(reader.next_frame().unwrap(), Some(b"incomplete".to_vec()));
    }

    #[test]
    fn rejects_oversized_frame() {
        let mut reader = FrameReader::with_max_len(8);
        reader.push(&9u32.to_le_bytes());
        reader.push(&[0; 9]);
        assert_eq!(
            reader.next_frame(),
            Err(FrameError::TooLarge { len: 9, max: 8 })
        );
    }

    #[test]
    fn rejects_empty_frame() {
        let mut reader = FrameReader::new();
        reader.push(&0u32.to_le_bytes());
        assert_eq!(reader.next_frame(), Err(FrameError::Empty));
    }

    #[test]
    fn frame_rejects_empty_payload() {
        assert_eq!(frame(b""), Err(FrameError::Empty));
    }
}
