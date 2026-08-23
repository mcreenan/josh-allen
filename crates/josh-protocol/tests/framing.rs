use std::io::{Cursor, Read, Write};
use std::sync::{Arc, Mutex};

use josh_protocol::{
    DEFAULT_MAX_FRAME_BYTES, FrameReader, ProtocolError, SerializedWriter, WireMessage,
    decode_message, encode_frame,
};
use serde_json::json;

fn request() -> WireMessage {
    WireMessage::Request {
        id: "h-1".into(),
        method: "initialize".into(),
        params: json!({}),
    }
}

#[test]
fn exact_frame_round_trips_and_clean_eof_is_distinct() {
    let frame = encode_frame(&request(), DEFAULT_MAX_FRAME_BYTES).unwrap();
    let body =
        br#"{"protocol":"josh/1","kind":"request","id":"h-1","method":"initialize","params":{}}"#;
    let expected_header = format!(
        "Content-Length: {}\r\nContent-Type: application/josh+json; charset=utf-8\r\n\r\n",
        body.len()
    );
    assert_eq!(&frame[..expected_header.len()], expected_header.as_bytes());
    assert_eq!(&frame[expected_header.len()..], body);

    let mut reader = FrameReader::new(Cursor::new(frame), DEFAULT_MAX_FRAME_BYTES);
    assert_eq!(reader.read_message().unwrap(), Some(request()));
    assert_eq!(reader.read_message().unwrap(), None);
}

struct OneByte<R>(R);

impl<R: Read> Read for OneByte<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let length = buffer.len().min(1);
        self.0.read(&mut buffer[..length])
    }
}

#[test]
fn accepts_a_frame_split_at_every_byte_boundary() {
    let frame = encode_frame(&request(), DEFAULT_MAX_FRAME_BYTES).unwrap();
    let mut reader = FrameReader::new(OneByte(Cursor::new(frame)), DEFAULT_MAX_FRAME_BYTES);
    assert_eq!(reader.read_message().unwrap(), Some(request()));
}

fn framed(header: &[u8], body: &[u8]) -> Vec<u8> {
    let mut bytes = header.to_vec();
    bytes.extend_from_slice(body);
    bytes
}

#[test]
fn rejects_malformed_headers_lengths_and_bodies() {
    let body = br"{}";
    let cases: Vec<(Vec<u8>, ProtocolError)> = vec![
        (framed(b"Content-Length: 2\nContent-Type: application/josh+json; charset=utf-8\n\n", body), ProtocolError::InvalidHeader),
        (framed(b"Content-Type: application/josh+json; charset=utf-8\r\nContent-Length: 2\r\n\r\n", body), ProtocolError::InvalidHeader),
        (framed(b"Content-Length: 02\r\nContent-Type: application/josh+json; charset=utf-8\r\n\r\n", body), ProtocolError::InvalidLength),
        (framed(b"Content-Length: 0\r\nContent-Type: application/josh+json; charset=utf-8\r\n\r\n", b""), ProtocolError::InvalidLength),
        (framed(b"Content-Length: +2\r\nContent-Type: application/josh+json; charset=utf-8\r\n\r\n", body), ProtocolError::InvalidLength),
        (framed(b"Content-Length: 2\r\nX: y\r\nContent-Type: application/josh+json; charset=utf-8\r\n\r\n", body), ProtocolError::InvalidHeader),
        (framed(b"Content-Length: 2\r\nContent-Type: application/json\r\n\r\n", body), ProtocolError::InvalidHeader),
        (framed(b"Content-Length: 3\r\nContent-Type: application/josh+json; charset=utf-8\r\n\r\n", body), ProtocolError::UnexpectedEof),
        (framed(b"Content-Length: 2\r\nContent-Type: application/josh+json; charset=utf-8\r\n\r\n", &[0xff, 0xff]), ProtocolError::InvalidUtf8),
    ];
    for (bytes, expected) in cases {
        let error = FrameReader::new(Cursor::new(bytes), DEFAULT_MAX_FRAME_BYTES)
            .read_message()
            .unwrap_err();
        assert_eq!(error, expected);
    }

    let over_limit = framed(
        b"Content-Length: 3\r\nContent-Type: application/josh+json; charset=utf-8\r\n\r\n",
        b"123",
    );
    assert_eq!(
        FrameReader::new(Cursor::new(over_limit), 2)
            .read_message()
            .unwrap_err(),
        ProtocolError::FrameTooLarge
    );

    let long = format!("Content-Length: {}\r\n", "1".repeat(1_025));
    assert_eq!(
        FrameReader::new(Cursor::new(long.into_bytes()), DEFAULT_MAX_FRAME_BYTES)
            .read_message()
            .unwrap_err(),
        ProtocolError::HeaderLineTooLong
    );
}

#[test]
fn partial_header_eof_is_fatal() {
    for bytes in [b"C".as_slice(), b"Content-Length: 2\r\n".as_slice()] {
        assert_eq!(
            FrameReader::new(Cursor::new(bytes), DEFAULT_MAX_FRAME_BYTES)
                .read_message()
                .unwrap_err(),
            ProtocolError::UnexpectedEof
        );
    }
}

#[test]
fn raw_value_path_is_bounded_and_strict() {
    let body = br#"{"outer":{"x":1,"x":2}}"#;
    let header = format!(
        "Content-Length: {}\r\nContent-Type: application/josh+json; charset=utf-8\r\n\r\n",
        body.len()
    );
    let error = FrameReader::new(
        Cursor::new(framed(header.as_bytes(), body)),
        DEFAULT_MAX_FRAME_BYTES,
    )
    .read_value()
    .unwrap_err();
    assert!(matches!(error, ProtocolError::InvalidJson(_)));

    assert!(decode_message(br"{} trailing").is_err());
}

#[derive(Clone, Default)]
struct SharedBytes(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn concurrent_writer_emits_only_whole_frames() {
    let bytes = SharedBytes::default();
    let output = bytes.clone();
    let writer = Arc::new(SerializedWriter::new(bytes, DEFAULT_MAX_FRAME_BYTES));
    let mut threads = Vec::new();
    for index in 0..16 {
        let writer = Arc::clone(&writer);
        threads.push(std::thread::spawn(move || {
            writer
                .write_message(&WireMessage::Notification {
                    method: "execution/event".into(),
                    params: json!({"index": index}),
                })
                .unwrap();
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }
    let snapshot = output.0.lock().unwrap().clone();
    let mut reader = FrameReader::new(Cursor::new(snapshot), DEFAULT_MAX_FRAME_BYTES);
    let mut count = 0;
    while reader.read_message().unwrap().is_some() {
        count += 1;
    }
    assert_eq!(count, 16);
}
