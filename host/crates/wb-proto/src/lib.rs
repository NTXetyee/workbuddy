//! WorkBuddy wire protocol v1 (SPEC.md section 2). Pure std, little-endian.
//!
//! A frame is a 12-byte header (`'W' 'B' version type seq:u32 len:u32`)
//! followed by `len` payload bytes.

use std::fmt;

pub const MAGIC: [u8; 2] = *b"WB";
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 12;
pub const MAX_PAYLOAD: u32 = 4 * 1024 * 1024;

const T_HELLO: u8 = 0x01;
const T_CONFIG: u8 = 0x02;
const T_VIDEO: u8 = 0x03;
const T_KEYFRAME_REQUEST: u8 = 0x04;
const T_PING: u8 = 0x05;
const T_PONG: u8 = 0x06;
const T_ORIENTATION: u8 = 0x07;
const T_STATS: u8 = 0x08;
const T_BYE: u8 = 0x09;

const HELLO_FIXED_LEN: usize = 11;
const CONFIG_LEN: usize = 12;
const VIDEO_HEADER_LEN: usize = 12;
const STATS_LEN: usize = 12;
const MAX_NAME_LEN: usize = 255;

#[derive(Debug, Clone, PartialEq)]
pub struct Hello {
    pub proto_version: u8,
    pub codecs: u8,
    pub orientation: u8,
    pub native_width: u16,
    pub native_height: u16,
    pub max_fps: u16,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub codec: u8,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
    pub bitrate_kbps: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Video {
    pub pts_us: u64,
    pub flags: u8,
    /// Annex-B access unit.
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
    pub frames_decoded: u32,
    pub frames_dropped: u32,
    pub last_decode_us: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Hello(Hello),
    Config(Config),
    Video(Video),
    KeyframeRequest,
    Ping(u64),
    Pong(u64),
    Orientation(u8),
    Stats(Stats),
    Bye,
    Unknown { msg_type: u8, payload: Vec<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    BadMagic,
    BadVersion(u8),
    TooLarge(u32),
    BadPayload { msg_type: u8 },
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::BadMagic => write!(f, "bad frame magic"),
            ProtoError::BadVersion(v) => write!(f, "unsupported protocol version {v}"),
            ProtoError::TooLarge(n) => write!(f, "payload length {n} exceeds maximum"),
            ProtoError::BadPayload { msg_type } => {
                write!(f, "malformed payload for message type 0x{msg_type:02x}")
            }
        }
    }
}

impl std::error::Error for ProtoError {}

/// Hello names are truncated to 255 bytes, on a char boundary.
fn hello_name_bytes(name: &str) -> &[u8] {
    let mut end = name.len().min(MAX_NAME_LEN);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name.as_bytes()[..end]
}

/// Appends one complete frame (header + payload) for `msg` to `out`.
pub fn encode(msg: &Message, seq: u32, out: &mut Vec<u8>) {
    let (msg_type, len) = match msg {
        Message::Hello(h) => (T_HELLO, HELLO_FIXED_LEN + hello_name_bytes(&h.name).len()),
        Message::Config(_) => (T_CONFIG, CONFIG_LEN),
        Message::Video(v) => (T_VIDEO, VIDEO_HEADER_LEN + v.data.len()),
        Message::KeyframeRequest => (T_KEYFRAME_REQUEST, 0),
        Message::Ping(_) => (T_PING, 8),
        Message::Pong(_) => (T_PONG, 8),
        Message::Orientation(_) => (T_ORIENTATION, 1),
        Message::Stats(_) => (T_STATS, STATS_LEN),
        Message::Bye => (T_BYE, 0),
        Message::Unknown { msg_type, payload } => (*msg_type, payload.len()),
    };

    out.reserve(HEADER_LEN + len);
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(msg_type);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&(len as u32).to_le_bytes());

    match msg {
        Message::Hello(h) => {
            let name = hello_name_bytes(&h.name);
            out.push(h.proto_version);
            out.push(h.codecs);
            out.push(h.orientation);
            out.push(0);
            out.extend_from_slice(&h.native_width.to_le_bytes());
            out.extend_from_slice(&h.native_height.to_le_bytes());
            out.extend_from_slice(&h.max_fps.to_le_bytes());
            out.push(name.len() as u8);
            out.extend_from_slice(name);
        }
        Message::Config(c) => {
            out.push(c.codec);
            out.push(0);
            out.extend_from_slice(&c.width.to_le_bytes());
            out.extend_from_slice(&c.height.to_le_bytes());
            out.extend_from_slice(&c.fps.to_le_bytes());
            out.extend_from_slice(&c.bitrate_kbps.to_le_bytes());
        }
        Message::Video(v) => {
            out.extend_from_slice(&v.pts_us.to_le_bytes());
            out.push(v.flags);
            out.extend_from_slice(&[0, 0, 0]);
            out.extend_from_slice(&v.data);
        }
        Message::KeyframeRequest | Message::Bye => {}
        Message::Ping(ts) | Message::Pong(ts) => out.extend_from_slice(&ts.to_le_bytes()),
        Message::Orientation(o) => out.push(*o),
        Message::Stats(s) => {
            out.extend_from_slice(&s.frames_decoded.to_le_bytes());
            out.extend_from_slice(&s.frames_dropped.to_le_bytes());
            out.extend_from_slice(&s.last_decode_us.to_le_bytes());
        }
        Message::Unknown { payload, .. } => out.extend_from_slice(payload),
    }
}

/// Incremental frame decoder. Feed bytes with `push`, then call `next`
/// until it returns `Ok(None)`. Works with any chunking.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Returns the next complete message and its sequence number, or
    /// `Ok(None)` when more bytes are needed.
    #[allow(clippy::should_implement_trait)] // name and signature fixed by SPEC 3.1
    pub fn next(&mut self) -> Result<Option<(u32, Message)>, ProtoError> {
        let buf = &self.buf;
        if !buf.is_empty() && buf[0] != MAGIC[0] {
            return Err(ProtoError::BadMagic);
        }
        if buf.len() >= 2 && buf[1] != MAGIC[1] {
            return Err(ProtoError::BadMagic);
        }
        if buf.len() >= 3 && buf[2] != VERSION {
            return Err(ProtoError::BadVersion(buf[2]));
        }
        if buf.len() < HEADER_LEN {
            return Ok(None);
        }
        let msg_type = buf[3];
        let seq = le32(&buf[4..8]);
        let len = le32(&buf[8..12]);
        if len > MAX_PAYLOAD {
            return Err(ProtoError::TooLarge(len));
        }
        let total = HEADER_LEN + len as usize;
        if buf.len() < total {
            return Ok(None);
        }
        let payload = buf[HEADER_LEN..total].to_vec();
        self.buf.drain(..total);
        let msg = parse_payload(msg_type, payload)?;
        Ok(Some((seq, msg)))
    }
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn le64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

fn parse_payload(msg_type: u8, p: Vec<u8>) -> Result<Message, ProtoError> {
    let bad = ProtoError::BadPayload { msg_type };
    let msg = match msg_type {
        T_HELLO => {
            if p.len() < HELLO_FIXED_LEN || p[10] as usize != p.len() - HELLO_FIXED_LEN {
                return Err(bad);
            }
            let name = std::str::from_utf8(&p[HELLO_FIXED_LEN..]).map_err(|_| bad.clone())?;
            Message::Hello(Hello {
                proto_version: p[0],
                codecs: p[1],
                orientation: p[2],
                native_width: le16(&p[4..6]),
                native_height: le16(&p[6..8]),
                max_fps: le16(&p[8..10]),
                name: name.to_owned(),
            })
        }
        T_CONFIG => {
            if p.len() != CONFIG_LEN {
                return Err(bad);
            }
            Message::Config(Config {
                codec: p[0],
                width: le16(&p[2..4]),
                height: le16(&p[4..6]),
                fps: le16(&p[6..8]),
                bitrate_kbps: le32(&p[8..12]),
            })
        }
        T_VIDEO => {
            if p.len() < VIDEO_HEADER_LEN {
                return Err(bad);
            }
            Message::Video(Video {
                pts_us: le64(&p[0..8]),
                flags: p[8],
                data: p[VIDEO_HEADER_LEN..].to_vec(),
            })
        }
        T_KEYFRAME_REQUEST => {
            if !p.is_empty() {
                return Err(bad);
            }
            Message::KeyframeRequest
        }
        T_PING | T_PONG => {
            if p.len() != 8 {
                return Err(bad);
            }
            let ts = le64(&p);
            if msg_type == T_PING {
                Message::Ping(ts)
            } else {
                Message::Pong(ts)
            }
        }
        T_ORIENTATION => {
            if p.len() != 1 {
                return Err(bad);
            }
            Message::Orientation(p[0])
        }
        T_STATS => {
            if p.len() != STATS_LEN {
                return Err(bad);
            }
            Message::Stats(Stats {
                frames_decoded: le32(&p[0..4]),
                frames_dropped: le32(&p[4..8]),
                last_decode_us: le32(&p[8..12]),
            })
        }
        T_BYE => {
            if !p.is_empty() {
                return Err(bad);
            }
            Message::Bye
        }
        _ => Message::Unknown {
            msg_type,
            payload: p,
        },
    };
    Ok(msg)
}
