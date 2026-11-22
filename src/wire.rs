//! Wayland wire protocol primitives.
//!
//! Every message on a Wayland socket looks like:
//!
//! ```text
//! u32   object id
//! u16   request/event opcode
//! u16   total size in bytes (including this 8-byte header)
//! ...   payload (request/event arguments)
//! ```
//!
//! Scalars are in host byte order (libwayland uses native endianness; every
//! platform that runs a Wayland desktop today is little-endian, which this
//! crate assumes). Strings are a `u32` length *including* the terminating
//! NUL, followed by the bytes and the NUL, padded to a 4-byte boundary
//! measured from the start of the payload.

/// Size of the fixed message header in bytes.
pub const HEADER_LEN: usize = 8;

/// An incoming message with its payload already split off.
#[derive(Debug)]
pub struct Message {
    pub object_id: u32,
    pub opcode: u16,
    pub payload: Vec<u8>,
}

/// Encode a full message (header + payload) into `out`.
pub fn encode(out: &mut Vec<u8>, object_id: u32, opcode: u16, payload: &[u8]) {
    let size = (HEADER_LEN + payload.len()) as u16;
    out.reserve(HEADER_LEN + payload.len());
    out.extend_from_slice(&object_id.to_ne_bytes());
    out.extend_from_slice(&opcode.to_ne_bytes());
    out.extend_from_slice(&size.to_ne_bytes());
    out.extend_from_slice(payload);
}

/// Builder for a request payload (the part after the 8-byte header).
#[derive(Default)]
pub struct Args {
    buf: Vec<u8>,
}

impl Args {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn u32(mut self, v: u32) -> Self {
        self.buf.extend_from_slice(&v.to_ne_bytes());
        self
    }

    pub fn i32(mut self, v: i32) -> Self {
        self.buf.extend_from_slice(&v.to_ne_bytes());
        self
    }

    /// Object id argument (including `new_id` ids we allocate).
    pub fn object(self, id: u32) -> Self {
        self.u32(id)
    }

    /// String argument: u32 length (incl. NUL) + bytes + NUL, padded to 4.
    pub fn string(mut self, s: &str) -> Self {
        let len = s.len() + 1;
        self.buf.extend_from_slice(&(len as u32).to_ne_bytes());
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
        while self.buf.len() % 4 != 0 {
            self.buf.push(0);
        }
        self
    }

    pub fn into_payload(self) -> Vec<u8> {
        self.buf
    }
}

/// Sequential reader over a message payload.
pub struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.pos + n > self.data.len() {
            return None;
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Some(s)
    }

    pub fn u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn i32(&mut self) -> Option<i32> {
        self.u32().map(|v| v as i32)
    }

    /// Read a string argument (u32 length incl. NUL + bytes + padding).
    pub fn string(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        if len == 0 {
            return None;
        }
        let bytes = self.take(len)?;
        let s = String::from_utf8_lossy(&bytes[..len - 1]).into_owned();
        // Re-align to a 4-byte boundary relative to the payload start.
        self.pos = (self.pos + 3) & !3;
        Some(s)
    }

    pub fn at_end(&self) -> bool {
        self.pos >= self.data.len()
    }
}

/// Split all complete messages currently buffered in `buf`.
///
/// Truncates `buf` down to the leftover partial message (if any).
pub fn parse_messages(buf: &mut Vec<u8>) -> Vec<Message> {
    let mut msgs = Vec::new();
    let mut off = 0usize;
    while buf.len() - off >= HEADER_LEN {
        let size = u16::from_ne_bytes([buf[off + 6], buf[off + 7]]) as usize;
        if size < HEADER_LEN {
            // Garbage / protocol violation; stop parsing.
            break;
        }
        if buf.len() - off < size {
            break; // incomplete message, wait for more bytes
        }
        let object_id =
            u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
        let opcode = u16::from_ne_bytes([buf[off + 4], buf[off + 5]]);
        msgs.push(Message {
            object_id,
            opcode,
            payload: buf[off + HEADER_LEN..off + size].to_vec(),
        });
        off += size;
    }
    buf.drain(..off);
    msgs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let mut buf = Vec::new();
        encode(&mut buf, 0xdead_beef, 7, &[]);
        assert_eq!(buf.len(), 8);
        let msgs = parse_messages(&mut buf);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].object_id, 0xdead_beef);
        assert_eq!(msgs[0].opcode, 7);
        assert!(msgs[0].payload.is_empty());
        assert!(buf.is_empty());
    }

    #[test]
    fn string_padding_is_message_relative() {
        // 1-char string: 4 (len) + 2 ("a\0") + 2 padding = 8
        let args = Args::new().string("a").into_payload();
        assert_eq!(args.len(), 8);
        let mut p = Parser::new(&args);
        assert_eq!(p.string().unwrap(), "a");
        assert!(p.at_end());
    }

    #[test]
    fn mixed_args_roundtrip() {
        let args = Args::new()
            .u32(42)
            .string("hello")
            .i32(-1)
            .object(0x1234)
            .into_payload();
        // 4 + (4 + 6 + 2) + 4 + 4 = 24
        assert_eq!(args.len(), 24);
        let mut p = Parser::new(&args);
        assert_eq!(p.u32(), Some(42));
        assert_eq!(p.string().unwrap(), "hello");
        assert_eq!(p.i32(), Some(-1));
        assert_eq!(p.u32(), Some(0x1234));
        assert!(p.at_end());
    }

    #[test]
    fn partial_message_is_kept() {
        let mut full = Vec::new();
        encode(&mut full, 9, 0, &[1, 2, 3, 4]);
        let mut buf = full[..5].to_vec();
        assert!(parse_messages(&mut buf).is_empty());
        assert_eq!(buf.len(), 5);
        buf.extend_from_slice(&full[5..]);
        let msgs = parse_messages(&mut buf);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].payload, vec![1, 2, 3, 4]);
        assert!(buf.is_empty());
    }

    #[test]
    fn two_messages_in_one_read() {
        let mut buf = Vec::new();
        encode(&mut buf, 5, 1, &[]);
        encode(&mut buf, 6, 0, &7u32.to_ne_bytes());
        let msgs = parse_messages(&mut buf);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].payload, vec![7, 0, 0, 0]);
        assert!(buf.is_empty());
    }
}
