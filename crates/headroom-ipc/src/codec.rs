//! length-prefixed json framing: 4-byte big-endian length, then that many bytes of utf-8 json.

use std::io::{Read, Write};

use serde::{de::DeserializeOwned, Serialize};

use crate::error::Error;

/// default frame payload cap.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1024 * 1024; // 1 MiB

/// floor on the frame cap; a smaller cap is misuse, not normal traffic.
pub const MIN_MAX_FRAME_BYTES: usize = 256;

/// stateless framing codec; owns no buffers, callers supply reader/writer.
#[derive(Debug, Clone, Copy)]
pub struct Codec {
    max_frame_bytes: usize,
}

impl Default for Codec {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
        }
    }
}

impl Codec {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// cap clamped up to [`MIN_MAX_FRAME_BYTES`].
    #[must_use]
    pub fn with_max_frame_size(bytes: usize) -> Self {
        Self {
            max_frame_bytes: bytes.max(MIN_MAX_FRAME_BYTES),
        }
    }

    #[must_use]
    pub fn max_frame_bytes(self) -> usize {
        self.max_frame_bytes
    }

    /// # Errors
    /// - [`Error::Json`] on serialize failure.
    /// - [`Error::FrameTooLarge`] if the serialized form exceeds the cap.
    /// - [`Error::Io`] on write failure.
    pub fn write<W: Write, T: Serialize>(self, mut w: W, msg: &T) -> Result<(), Error> {
        // serialize first to know the exact length
        let buf = serde_json::to_vec(msg)?;
        if buf.len() > self.max_frame_bytes {
            return Err(Error::FrameTooLarge {
                actual: buf.len(),
                limit: self.max_frame_bytes,
            });
        }
        let len = u32::try_from(buf.len()).expect("buf.len() <= max_frame_bytes <= u32::MAX");
        w.write_all(&len.to_be_bytes())?;
        w.write_all(&buf)?;
        w.flush()?;
        Ok(())
    }

    /// # Errors
    /// - [`Error::Closed`] on EOF before any length-prefix byte (graceful close).
    /// - [`Error::Io`] on partial reads or other i/o failure.
    /// - [`Error::FrameTooLarge`] if the announced length exceeds the cap.
    /// - [`Error::Json`] on deserialize failure.
    pub fn read<R: Read, T: DeserializeOwned>(self, mut r: R) -> Result<T, Error> {
        let mut len_buf = [0u8; 4];
        match read_full_or_eof(&mut r, &mut len_buf)? {
            ReadOutcome::Full => {}
            ReadOutcome::ZeroAtStart => return Err(Error::Closed),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > self.max_frame_bytes {
            return Err(Error::FrameTooLarge {
                actual: len,
                limit: self.max_frame_bytes,
            });
        }
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)?;
        Ok(serde_json::from_slice(&buf)?)
    }
}

enum ReadOutcome {
    Full,
    ZeroAtStart,
}

fn read_full_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<ReadOutcome, std::io::Error> {
    let mut read = 0;
    while read < buf.len() {
        match r.read(&mut buf[read..]) {
            Ok(0) => {
                if read == 0 {
                    return Ok(ReadOutcome::ZeroAtStart);
                }
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "eof mid-length-prefix",
                ));
            }
            Ok(n) => read += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(ReadOutcome::Full)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Event, Op, Request, Response, ServerFrame, Topic};
    use std::io::Cursor;

    #[test]
    fn write_read_request_roundtrip() {
        let codec = Codec::new();
        let req = Request::new(
            42,
            Op::ProfileUse {
                name: "night".into(),
            },
        );

        let mut buf = Vec::new();
        codec.write(&mut buf, &req).unwrap();

        let mut cur = Cursor::new(&buf);
        let back: Request = codec.read(&mut cur).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn server_frame_round_trip_response() {
        let codec = Codec::new();
        let resp = Response::ok(1, &serde_json::json!({"name": "default"})).unwrap();
        let frame = ServerFrame::Response(resp.clone());

        let mut buf = Vec::new();
        codec.write(&mut buf, &frame).unwrap();
        let mut cur = Cursor::new(&buf);
        let back: ServerFrame = codec.read(&mut cur).unwrap();
        match back {
            ServerFrame::Response(r) => assert_eq!(r, resp),
            ServerFrame::Event(_) => panic!("decoded as event"),
        }
    }

    #[test]
    fn server_frame_round_trip_event() {
        let codec = Codec::new();
        let ev = Event::new(Topic::Daemon, "shutdown", &serde_json::json!({})).unwrap();
        let frame = ServerFrame::Event(ev.clone());

        let mut buf = Vec::new();
        codec.write(&mut buf, &frame).unwrap();
        let mut cur = Cursor::new(&buf);
        let back: ServerFrame = codec.read(&mut cur).unwrap();
        match back {
            ServerFrame::Event(e) => assert_eq!(e, ev),
            ServerFrame::Response(_) => panic!("decoded as response"),
        }
    }

    #[test]
    fn rejects_oversized_frames_on_write() {
        let codec = Codec::with_max_frame_size(MIN_MAX_FRAME_BYTES);
        // A big string that will serialize > 256 bytes.
        let req = Request::new(
            1,
            Op::SettingSet {
                key: "x".into(),
                value: serde_json::Value::String("a".repeat(1024)),
            },
        );
        let mut buf = Vec::new();
        let err = codec.write(&mut buf, &req).unwrap_err();
        assert!(matches!(err, Error::FrameTooLarge { .. }));
    }

    #[test]
    fn rejects_oversized_frames_on_read() {
        let codec = Codec::with_max_frame_size(MIN_MAX_FRAME_BYTES);
        // Hand-craft a length prefix that exceeds the cap.
        let mut buf = Vec::new();
        let bad_len: u32 = MIN_MAX_FRAME_BYTES as u32 + 1;
        buf.extend_from_slice(&bad_len.to_be_bytes());
        // No need to follow with payload; we expect early rejection.
        let mut cur = Cursor::new(&buf);
        let err = codec.read::<_, serde_json::Value>(&mut cur).unwrap_err();
        assert!(matches!(err, Error::FrameTooLarge { .. }));
    }

    #[test]
    fn graceful_eof_at_frame_boundary() {
        let codec = Codec::new();
        let mut cur = Cursor::new(Vec::<u8>::new());
        let err = codec.read::<_, Request>(&mut cur).unwrap_err();
        assert!(matches!(err, Error::Closed));
    }

    #[test]
    fn mid_frame_eof_is_io_error() {
        let codec = Codec::new();
        // Half a length prefix.
        let mut cur = Cursor::new(vec![0u8, 0u8]);
        let err = codec.read::<_, Request>(&mut cur).unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[test]
    fn rejects_invalid_json_payload() {
        let codec = Codec::new();
        let payload = b"not-json";
        let mut buf = Vec::new();
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(payload);
        let mut cur = Cursor::new(&buf);
        let err = codec.read::<_, Request>(&mut cur).unwrap_err();
        assert!(matches!(err, Error::Json(_)));
    }

    #[test]
    fn back_to_back_frames() {
        let codec = Codec::new();
        let a = Request::new(1, Op::Status);
        let b = Request::new(2, Op::ProfileList);

        let mut buf = Vec::new();
        codec.write(&mut buf, &a).unwrap();
        codec.write(&mut buf, &b).unwrap();

        let mut cur = Cursor::new(&buf);
        let ra: Request = codec.read(&mut cur).unwrap();
        let rb: Request = codec.read(&mut cur).unwrap();
        assert_eq!(ra, a);
        assert_eq!(rb, b);
    }
}
