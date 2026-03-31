use std::io::{Read, Write};

use anyhow::Result;

/// Maximum message size (64 MB). Prevents OOM from malformed length headers.
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Read a length-prefixed message from a stream.
///
/// Wire format: `[4-byte big-endian length][payload bytes]`
pub fn read_message(stream: &mut impl Read) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let msg_len = u32::from_be_bytes(len_buf) as usize;

    if msg_len > MAX_MESSAGE_SIZE {
        anyhow::bail!("Message too large: {msg_len} bytes (max {MAX_MESSAGE_SIZE})");
    }

    let mut buf = vec![0u8; msg_len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

/// Write a length-prefixed message to a stream.
///
/// Wire format: `[4-byte big-endian length][payload bytes]`
pub fn write_message(stream: &mut impl Write, data: &[u8]) -> Result<()> {
    let len_bytes = (data.len() as u32).to_be_bytes();
    stream.write_all(&len_bytes)?;
    stream.write_all(data)?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_ascii() {
        let original = b"hello world";
        let mut buf = Vec::new();
        write_message(&mut buf, original).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded = read_message(&mut cursor).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_utf8() {
        let original = "こんにちは世界".as_bytes();
        let mut buf = Vec::new();
        write_message(&mut buf, original).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded = read_message(&mut cursor).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn roundtrip_empty() {
        let original = b"";
        let mut buf = Vec::new();
        write_message(&mut buf, original).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded = read_message(&mut cursor).unwrap();
        assert_eq!(decoded, original.to_vec());
    }

    #[test]
    fn roundtrip_json() {
        let json = serde_json::json!({"texts": ["hello", "world"]});
        let data = serde_json::to_vec(&json).unwrap();
        let mut buf = Vec::new();
        write_message(&mut buf, &data).unwrap();

        let mut cursor = Cursor::new(buf);
        let decoded = read_message(&mut cursor).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(parsed, json);
    }

    #[test]
    fn multiple_messages() {
        let mut buf = Vec::new();
        write_message(&mut buf, b"first").unwrap();
        write_message(&mut buf, b"second").unwrap();
        write_message(&mut buf, b"third").unwrap();

        let mut cursor = Cursor::new(buf);
        assert_eq!(read_message(&mut cursor).unwrap(), b"first");
        assert_eq!(read_message(&mut cursor).unwrap(), b"second");
        assert_eq!(read_message(&mut cursor).unwrap(), b"third");
    }

    #[test]
    fn read_truncated_length_fails() {
        let buf = vec![0u8; 2]; // Only 2 bytes, need 4
        let mut cursor = Cursor::new(buf);
        assert!(read_message(&mut cursor).is_err());
    }

    #[test]
    fn read_truncated_payload_fails() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&10u32.to_be_bytes()); // Says 10 bytes
        buf.extend_from_slice(b"short"); // Only 5 bytes
        let mut cursor = Cursor::new(buf);
        assert!(read_message(&mut cursor).is_err());
    }

    #[test]
    fn read_oversized_message_fails() {
        let huge_len = (super::MAX_MESSAGE_SIZE as u32) + 1;
        let mut buf = Vec::new();
        buf.extend_from_slice(&huge_len.to_be_bytes());
        let mut cursor = Cursor::new(buf);
        let err = read_message(&mut cursor).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }
}
