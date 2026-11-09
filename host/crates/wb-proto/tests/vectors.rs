//! Generates and checks `proto/vectors.json` (repo root), the shared
//! cross-language vector file that the Swift side also checks.
//!
//! Regenerate with: `WB_WRITE_VECTORS=1 cargo test -p wb-proto write_vectors`

use std::path::PathBuf;
use wb_proto::{encode, Config, FrameDecoder, Hello, Message, Stats, Video};

fn vectors_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../proto/vectors.json")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    assert!(bytes.len().is_multiple_of(2), "odd hex length");
    bytes
        .chunks(2)
        .map(|p| {
            let s = std::str::from_utf8(p).unwrap();
            u8::from_str_radix(s, 16).expect("bad hex digit")
        })
        .collect()
}

fn hello(name: &str) -> Hello {
    Hello {
        proto_version: 1,
        codecs: 1,
        orientation: 0,
        native_width: 2266,
        native_height: 1488,
        max_fps: 60,
        name: name.to_string(),
    }
}

/// (name, seq, message). Order is the order written to the file.
fn cases() -> Vec<(&'static str, u32, Message)> {
    let idr_access_unit = vec![
        0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0, 0x1E, // SPS
        0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C, 0x80, // PPS
        0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x84, 0x00, // IDR slice (fake)
    ];
    let p_access_unit = vec![0x00, 0x00, 0x00, 0x01, 0x41, 0x9A, 0x24, 0x6C];
    let long_name: String = "0123456789".repeat(25) + "ABCDE";
    assert_eq!(long_name.len(), 255);

    vec![
        ("ping_golden", 1, Message::Ping(0x0102_0304_0506_0708)),
        ("keyframe_request_golden", 2, Message::KeyframeRequest),
        (
            "config_golden",
            3,
            Message::Config(Config {
                codec: 1,
                width: 2266,
                height: 1488,
                fps: 60,
                bitrate_kbps: 20000,
            }),
        ),
        (
            "hello_sumers_ipad",
            4,
            Message::Hello(hello("Sumer's iPad")),
        ),
        ("hello_empty_name", 5, Message::Hello(hello(""))),
        ("hello_name_255", 6, Message::Hello(hello(&long_name))),
        (
            "video_keyframe_with_params",
            7,
            Message::Video(Video {
                pts_us: 1_000_000,
                flags: 0x03,
                data: idr_access_unit,
            }),
        ),
        (
            "video_nonkey",
            8,
            Message::Video(Video {
                pts_us: 1_016_667,
                flags: 0x00,
                data: p_access_unit,
            }),
        ),
        ("ping_small", 9, Message::Ping(1_234_567)),
        ("pong", 10, Message::Pong(1_234_567)),
        ("orientation_landscape", 11, Message::Orientation(0)),
        ("orientation_portrait", 12, Message::Orientation(1)),
        (
            "stats",
            13,
            Message::Stats(Stats {
                frames_decoded: 3600,
                frames_dropped: 2,
                last_decode_us: 1870,
            }),
        ),
        ("bye", 14, Message::Bye),
        (
            "unknown_type_7f",
            15,
            Message::Unknown {
                msg_type: 0x7F,
                payload: vec![0xDE, 0xAD, 0xBE, 0xEF],
            },
        ),
    ]
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn message_json(msg: &Message) -> String {
    match msg {
        Message::Hello(h) => format!(
            "{{\"type\": \"Hello\", \"proto_version\": {}, \"codecs\": {}, \"orientation\": {}, \"native_width\": {}, \"native_height\": {}, \"max_fps\": {}, \"name\": {}}}",
            h.proto_version,
            h.codecs,
            h.orientation,
            h.native_width,
            h.native_height,
            h.max_fps,
            json_str(&h.name)
        ),
        Message::Config(c) => format!(
            "{{\"type\": \"Config\", \"codec\": {}, \"width\": {}, \"height\": {}, \"fps\": {}, \"bitrate_kbps\": {}}}",
            c.codec, c.width, c.height, c.fps, c.bitrate_kbps
        ),
        Message::Video(v) => format!(
            "{{\"type\": \"Video\", \"pts_us\": {}, \"flags\": {}, \"data_hex\": \"{}\"}}",
            v.pts_us,
            v.flags,
            hex(&v.data)
        ),
        Message::KeyframeRequest => "{\"type\": \"KeyframeRequest\"}".to_string(),
        Message::Ping(ts) => format!("{{\"type\": \"Ping\", \"timestamp_us\": {ts}}}"),
        Message::Pong(ts) => format!("{{\"type\": \"Pong\", \"timestamp_us\": {ts}}}"),
        Message::Orientation(v) => format!("{{\"type\": \"Orientation\", \"value\": {v}}}"),
        Message::Stats(s) => format!(
            "{{\"type\": \"Stats\", \"frames_decoded\": {}, \"frames_dropped\": {}, \"last_decode_us\": {}}}",
            s.frames_decoded, s.frames_dropped, s.last_decode_us
        ),
        Message::Bye => "{\"type\": \"Bye\"}".to_string(),
        Message::Unknown { msg_type, payload } => format!(
            "{{\"type\": \"Unknown\", \"msg_type\": {}, \"payload_hex\": \"{}\"}}",
            msg_type,
            hex(payload)
        ),
    }
}

#[test]
fn write_vectors() {
    if std::env::var("WB_WRITE_VECTORS").as_deref() != Ok("1") {
        return;
    }
    let mut out = String::from("[\n");
    let all = cases();
    let last = all.len() - 1;
    for (i, (name, seq, msg)) in all.iter().enumerate() {
        let mut frame = Vec::new();
        encode(msg, *seq, &mut frame);
        out.push_str(&format!(
            "  {{\n    \"name\": {},\n    \"seq\": {},\n    \"hex\": \"{}\",\n    \"message\": {}\n  }}{}\n",
            json_str(name),
            seq,
            hex(&frame),
            message_json(msg),
            if i == last { "" } else { "," }
        ));
    }
    out.push_str("]\n");
    let path = vectors_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, out).expect("write proto/vectors.json");
    println!("wrote {}", path.display());
}

#[test]
fn vectors_match() {
    let path = vectors_path();
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e} (run WB_WRITE_VECTORS=1 cargo test -p wb-proto write_vectors)",
            path.display()
        )
    });
    let entries = parse_entries(&src);
    assert!(
        entries.len() >= 14,
        "expected at least 14 vectors, got {}",
        entries.len()
    );

    for e in &entries {
        let bytes = unhex(&e.hex);
        let mut dec = FrameDecoder::new();
        dec.push(&bytes);
        let (seq, msg) = dec
            .next()
            .unwrap_or_else(|err| panic!("{}: decode failed: {err}", e.name))
            .unwrap_or_else(|| panic!("{}: incomplete frame", e.name));
        assert_eq!(seq, e.seq, "{}: seq mismatch", e.name);
        assert!(
            dec.next().unwrap().is_none(),
            "{}: trailing bytes after frame",
            e.name
        );
        let mut re = Vec::new();
        encode(&msg, seq, &mut re);
        assert_eq!(hex(&re), e.hex, "{}: re-encode mismatch", e.name);
    }
}

// ---------------------------------------------------------------------------
// Minimal JSON reader: only extracts "name", "seq" and "hex" of each top-level
// array element. Every other value (including "message") is skipped.
// ---------------------------------------------------------------------------

struct Entry {
    name: String,
    seq: u32,
    hex: String,
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl Reader<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> u8 {
        self.ws();
        *self.b.get(self.i).expect("unexpected end of JSON")
    }

    fn eat(&mut self, c: u8) {
        let got = self.peek();
        assert_eq!(
            got as char, c as char,
            "JSON: expected '{}' at byte {}",
            c as char, self.i
        );
        self.i += 1;
    }

    fn string(&mut self) -> String {
        self.eat(b'"');
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = *self.b.get(self.i).expect("unterminated string");
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = self.b[self.i];
                    self.i += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b't' => out.push(b'\t'),
                        b'r' => out.push(b'\r'),
                        b'u' => {
                            let h = std::str::from_utf8(&self.b[self.i..self.i + 4]).unwrap();
                            let cp = u32::from_str_radix(h, 16).unwrap();
                            self.i += 4;
                            let ch = char::from_u32(cp).expect("bad \\u escape");
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
        String::from_utf8(out).expect("invalid UTF-8 in JSON string")
    }

    fn number(&mut self) -> u64 {
        self.ws();
        let start = self.i;
        while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
            self.i += 1;
        }
        std::str::from_utf8(&self.b[start..self.i])
            .unwrap()
            .parse()
            .expect("bad number")
    }

    /// Skips any JSON value.
    fn skip(&mut self) {
        match self.peek() {
            b'"' => {
                self.string();
            }
            b'{' => {
                self.eat(b'{');
                if self.peek() == b'}' {
                    self.i += 1;
                    return;
                }
                loop {
                    self.string();
                    self.eat(b':');
                    self.skip();
                    match self.peek() {
                        b',' => self.i += 1,
                        b'}' => {
                            self.i += 1;
                            return;
                        }
                        _ => panic!("JSON: bad object at byte {}", self.i),
                    }
                }
            }
            b'[' => {
                self.eat(b'[');
                if self.peek() == b']' {
                    self.i += 1;
                    return;
                }
                loop {
                    self.skip();
                    match self.peek() {
                        b',' => self.i += 1,
                        b']' => {
                            self.i += 1;
                            return;
                        }
                        _ => panic!("JSON: bad array at byte {}", self.i),
                    }
                }
            }
            _ => {
                // number / true / false / null
                while self.i < self.b.len() && !b",]} \n\r\t".contains(&self.b[self.i]) {
                    self.i += 1;
                }
            }
        }
    }

    fn entry(&mut self) -> Entry {
        self.eat(b'{');
        let (mut name, mut seq, mut hex) = (None, None, None);
        loop {
            let key = self.string();
            self.eat(b':');
            match key.as_str() {
                "name" => name = Some(self.string()),
                "seq" => seq = Some(self.number() as u32),
                "hex" => hex = Some(self.string()),
                _ => self.skip(),
            }
            match self.peek() {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    break;
                }
                _ => panic!("JSON: bad entry at byte {}", self.i),
            }
        }
        Entry {
            name: name.expect("entry without name"),
            seq: seq.expect("entry without seq"),
            hex: hex.expect("entry without hex"),
        }
    }
}

fn parse_entries(src: &str) -> Vec<Entry> {
    let mut r = Reader {
        b: src.as_bytes(),
        i: 0,
    };
    let mut out = Vec::new();
    r.eat(b'[');
    if r.peek() == b']' {
        return out;
    }
    loop {
        out.push(r.entry());
        match r.peek() {
            b',' => r.i += 1,
            b']' => break,
            _ => panic!("JSON: bad array at byte {}", r.i),
        }
    }
    out
}
