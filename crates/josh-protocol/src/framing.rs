use std::io::{BufReader, Read, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

use serde_json::Value;

use crate::{ProtocolError, WireMessage, decode_message, decode_value, encode_message};

pub const DEFAULT_MAX_FRAME_BYTES: usize = 4_194_304;
const MAX_HEADER_LINE_BYTES: usize = 1_024;
const CONTENT_TYPE_LINE: &[u8] = b"Content-Type: application/josh+json; charset=utf-8";

pub struct FrameReader<R> {
    reader: BufReader<R>,
    max_frame_bytes: AtomicUsize,
}

impl<R: Read> FrameReader<R> {
    #[must_use]
    pub fn new(reader: R, max_frame_bytes: usize) -> Self {
        Self {
            reader: BufReader::new(reader),
            max_frame_bytes: AtomicUsize::new(max_frame_bytes),
        }
    }

    /// Replaces the frame bound after capability negotiation.
    pub fn set_max_frame_bytes(&self, max_frame_bytes: usize) {
        self.max_frame_bytes
            .store(max_frame_bytes, Ordering::Release);
    }

    /// Reads one complete message. A clean EOF before a new frame returns `None`.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O failure or any invalid frame, JSON, or message.
    pub fn read_message(&mut self) -> Result<Option<WireMessage>, ProtocolError> {
        self.read_body()?
            .map(|body| decode_message(&body))
            .transpose()
    }

    /// Reads one complete frame as strict JSON without imposing a message shape.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O failure or any invalid frame or strict JSON value.
    pub fn read_value(&mut self) -> Result<Option<Value>, ProtocolError> {
        self.read_body()?
            .map(|body| decode_value(&body))
            .transpose()
    }

    fn read_body(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        let Some(first) = read_crlf_line(&mut self.reader, true)? else {
            return Ok(None);
        };
        let prefix = b"Content-Length: ";
        let digits = first
            .strip_prefix(prefix)
            .ok_or(ProtocolError::InvalidHeader)?;
        if digits.is_empty()
            || (digits.len() > 1 && digits[0] == b'0')
            || !digits.iter().all(u8::is_ascii_digit)
        {
            return Err(ProtocolError::InvalidLength);
        }
        let length = std::str::from_utf8(digits)
            .ok()
            .and_then(|text| text.parse::<usize>().ok())
            .filter(|length| *length > 0)
            .ok_or(ProtocolError::InvalidLength)?;
        if length > self.max_frame_bytes.load(Ordering::Acquire) {
            return Err(ProtocolError::FrameTooLarge);
        }
        let content_type =
            read_crlf_line(&mut self.reader, false)?.ok_or(ProtocolError::UnexpectedEof)?;
        if content_type != CONTENT_TYPE_LINE {
            return Err(ProtocolError::InvalidHeader);
        }
        let empty = read_crlf_line(&mut self.reader, false)?.ok_or(ProtocolError::UnexpectedEof)?;
        if !empty.is_empty() {
            return Err(ProtocolError::InvalidHeader);
        }
        let mut body = vec![0; length];
        self.reader
            .read_exact(&mut body)
            .map_err(map_body_read_error)?;
        Ok(Some(body))
    }
}

fn read_crlf_line<R: Read>(
    reader: &mut R,
    clean_eof: bool,
) -> Result<Option<Vec<u8>>, ProtocolError> {
    let mut line = Vec::new();
    loop {
        let mut byte = [0];
        match reader.read(&mut byte) {
            Ok(0) if line.is_empty() && clean_eof => return Ok(None),
            Ok(0) => return Err(ProtocolError::UnexpectedEof),
            Ok(_) => {
                if byte[0] == b'\n' {
                    if line.last() != Some(&b'\r') {
                        return Err(ProtocolError::InvalidHeader);
                    }
                    line.pop();
                    return Ok(Some(line));
                }
                line.push(byte[0]);
                if line.len() > MAX_HEADER_LINE_BYTES + 1 {
                    return Err(ProtocolError::HeaderLineTooLong);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn map_body_read_error(error: std::io::Error) -> ProtocolError {
    if error.kind() == std::io::ErrorKind::UnexpectedEof {
        ProtocolError::UnexpectedEof
    } else {
        error.into()
    }
}

/// Encodes one validated wire message in the exact two-header frame.
///
/// # Errors
///
/// Returns an error when the message is invalid or exceeds `max_frame_bytes`.
pub fn encode_frame(
    message: &WireMessage,
    max_frame_bytes: usize,
) -> Result<Vec<u8>, ProtocolError> {
    let body = encode_message(message)?;
    if body.is_empty() || body.len() > max_frame_bytes {
        return Err(ProtocolError::FrameTooLarge);
    }
    let header = format!(
        "Content-Length: {}\r\nContent-Type: application/josh+json; charset=utf-8\r\n\r\n",
        body.len()
    );
    let mut frame = Vec::with_capacity(header.len() + body.len());
    frame.extend_from_slice(header.as_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

struct WriterState<W> {
    writer: W,
    serving: u64,
}

/// A ticketed writer that commits every frame atomically in call order.
pub struct SerializedWriter<W> {
    next_ticket: AtomicU64,
    state: Mutex<WriterState<W>>,
    ready: Condvar,
    max_frame_bytes: AtomicUsize,
}

impl<W: Write> SerializedWriter<W> {
    #[must_use]
    pub fn new(writer: W, max_frame_bytes: usize) -> Self {
        Self {
            next_ticket: AtomicU64::new(0),
            state: Mutex::new(WriterState { writer, serving: 0 }),
            ready: Condvar::new(),
            max_frame_bytes: AtomicUsize::new(max_frame_bytes),
        }
    }

    /// Replaces the frame bound after capability negotiation.
    pub fn set_max_frame_bytes(&self, max_frame_bytes: usize) {
        self.max_frame_bytes
            .store(max_frame_bytes, Ordering::Release);
    }

    /// Encodes and writes one complete message frame.
    ///
    /// # Errors
    ///
    /// Returns an error when encoding or writing fails.
    pub fn write_message(&self, message: &WireMessage) -> Result<(), ProtocolError> {
        let frame = encode_frame(message, self.max_frame_bytes.load(Ordering::Acquire))?;
        self.write_frame(&frame)
    }

    /// Writes caller-validated frame bytes as one serialized unit.
    ///
    /// # Errors
    ///
    /// Returns an error when the writer lock is poisoned or the write fails.
    pub fn write_frame(&self, frame: &[u8]) -> Result<(), ProtocolError> {
        let ticket = self.next_ticket.fetch_add(1, Ordering::Relaxed);
        let mut state = self
            .state
            .lock()
            .map_err(|_| ProtocolError::Io("serialized writer lock is poisoned".into()))?;
        while state.serving != ticket {
            state = self
                .ready
                .wait(state)
                .map_err(|_| ProtocolError::Io("serialized writer lock is poisoned".into()))?;
        }
        let result = state
            .writer
            .write_all(frame)
            .and_then(|()| state.writer.flush());
        state.serving = state.serving.wrapping_add(1);
        self.ready.notify_all();
        result.map_err(Into::into)
    }

    /// Returns the underlying writer.
    ///
    /// # Errors
    ///
    /// Returns an error when the writer lock is poisoned.
    pub fn into_inner(self) -> Result<W, ProtocolError> {
        self.state
            .into_inner()
            .map(|state| state.writer)
            .map_err(|_| ProtocolError::Io("serialized writer lock is poisoned".into()))
    }
}
