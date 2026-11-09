use wb_proto::{
    encode, Config, FrameDecoder, Hello, Message, ProtoError, Stats, Video, HEADER_LEN, MAX_PAYLOAD,
};

fn unhex(s: &str) -> Vec<u8> {
    let clean: Vec<u8> = s.bytes().filter(|c| !c.is_ascii_whitespace()).collect();
    clean
        .chunks(2)
        .map(|p| {
            let s = std::str::from_utf8(p).unwrap();
            u8::from_str_radix(s, 16).unwrap()
        })
        .collect()
}

fn enc(msg: &Message, seq: u32) -> Vec<u8> {
    let mut out = Vec::new();
    encode(msg, seq, &mut out);
    out
}

fn decode_all(bytes: &[u8]) -> Vec<(u32, Message)> {
    let mut dec = FrameDecoder::new();
    dec.push(bytes);
    let mut out = Vec::new();
    while let Some(item) = dec.next().expect("decode must succeed") {
        out.push(item);
    }
    out
}

fn decode_one(bytes: &[u8]) -> Result<Option<(u32, Message)>, ProtoError> {
    let mut dec = FrameDecoder::new();
    dec.push(bytes);
    dec.next()
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

fn sample_messages() -> Vec<Message> {
    vec![
        Message::Hello(hello("Sumer's iPad")),
        Message::Hello(hello("")),
        Message::Hello(hello(&"N".repeat(255))),
        Message::Config(Config {
            codec: 1,
            width: 2266,
            height: 1488,
            fps: 60,
            bitrate_kbps: 20000,
        }),
        Message::Video(Video {
            pts_us: 123_456_789,
            flags: 0x03,
            data: vec![0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1E, 0, 0, 0, 1, 0x65, 0x88],
        }),
        Message::Video(Video {
            pts_us: u64::MAX,
            flags: 0,
            data: Vec::new(),
        }),
        Message::KeyframeRequest,
        Message::Ping(0x0102_0304_0506_0708),
        Message::Pong(0),
        Message::Orientation(1),
        Message::Stats(Stats {
            frames_decoded: 3600,
            frames_dropped: 2,
            last_decode_us: 1870,
        }),
        Message::Bye,
        Message::Unknown {
            msg_type: 0x7F,
            payload: vec![0xDE, 0xAD, 0xBE, 0xEF],
        },
        Message::Unknown {
            msg_type: 0x10,
            payload: Vec::new(),
        },
    ]
}

#[test]
fn golden_ping() {
    let bytes = unhex("57 42 01 05 01 00 00 00 08 00 00 00 08 07 06 05 04 03 02 01");
    let msg = Message::Ping(0x0102_0304_0506_0708);
    assert_eq!(enc(&msg, 1), bytes);
    assert_eq!(decode_all(&bytes), vec![(1, msg)]);
}

#[test]
fn golden_keyframe_request() {
    let bytes = unhex("57 42 01 04 02 00 00 00 00 00 00 00");
    let msg = Message::KeyframeRequest;
    assert_eq!(enc(&msg, 2), bytes);
    assert_eq!(decode_all(&bytes), vec![(2, msg)]);
}

#[test]
fn golden_config() {
    let bytes = unhex("57 42 01 02 03 00 00 00 0C 00 00 00 01 00 DA 08 D0 05 3C 00 20 4E 00 00");
    let msg = Message::Config(Config {
        codec: 1,
        width: 2266,
        height: 1488,
        fps: 60,
        bitrate_kbps: 20000,
    });
    assert_eq!(enc(&msg, 3), bytes);
    assert_eq!(decode_all(&bytes), vec![(3, msg)]);
}

#[test]
fn roundtrip_every_type() {
    for (i, msg) in sample_messages().into_iter().enumerate() {
        let seq = i as u32 * 7 + 1;
        let bytes = enc(&msg, seq);
        assert_eq!(bytes.len(), HEADER_LEN + bytes_payload_len(&bytes));
        assert_eq!(decode_all(&bytes), vec![(seq, msg)], "message index {i}");
    }
}

fn bytes_payload_len(frame: &[u8]) -> usize {
    u32::from_le_bytes([frame[8], frame[9], frame[10], frame[11]]) as usize
}

#[test]
fn roundtrip_one_mib_video() {
    let data: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let msg = Message::Video(Video {
        pts_us: 987_654_321,
        flags: 0x01,
        data,
    });
    let bytes = enc(&msg, 42);
    assert_eq!(bytes.len(), HEADER_LEN + 12 + 1024 * 1024);
    assert_eq!(decode_all(&bytes), vec![(42, msg)]);
}

#[test]
fn hello_name_truncated_to_255_on_char_boundary() {
    // 200 two-byte chars = 400 bytes; 255 bytes cut lands mid-char, so 254 bytes survive.
    let long = "é".repeat(200);
    let bytes = enc(&Message::Hello(hello(&long)), 5);
    let decoded = decode_all(&bytes);
    let expected = "é".repeat(127);
    assert_eq!(decoded, vec![(5, Message::Hello(hello(&expected)))]);
    assert_eq!(bytes_payload_len(&bytes), 11 + 254);
}

#[test]
fn byte_at_a_time_stream() {
    let mut msgs = Vec::new();
    let mut stream = Vec::new();
    for i in 0..300u32 {
        let msg = match i % 4 {
            0 => Message::Video(Video {
                pts_us: i as u64 * 16_667,
                flags: (i % 2) as u8,
                data: (0..(i % 37)).map(|b| b as u8).collect(),
            }),
            1 => Message::Ping(i as u64),
            2 => Message::Stats(Stats {
                frames_decoded: i,
                frames_dropped: 0,
                last_decode_us: 5,
            }),
            _ => Message::Unknown {
                msg_type: 0x42,
                payload: vec![i as u8; (i % 5) as usize],
            },
        };
        encode(&msg, i, &mut stream);
        msgs.push((i, msg));
    }
    // Also include a 1 MiB video in the middle of the byte-at-a-time stream.
    let big = Message::Video(Video {
        pts_us: 1,
        flags: 1,
        data: vec![0xAB; 1024 * 1024],
    });
    encode(&big, 1000, &mut stream);
    msgs.push((1000, big));

    let mut dec = FrameDecoder::new();
    let mut got = Vec::new();
    for b in &stream {
        dec.push(std::slice::from_ref(b));
        while let Some(item) = dec.next().expect("decode must succeed") {
            got.push(item);
        }
    }
    assert_eq!(got, msgs);
}

#[test]
fn partial_header_needs_more_bytes() {
    let bytes = enc(&Message::Ping(9), 1);
    for n in 0..bytes.len() {
        assert_eq!(decode_one(&bytes[..n]), Ok(None), "prefix of {n} bytes");
    }
}

#[test]
fn bad_magic() {
    let mut bytes = enc(&Message::Bye, 0);
    bytes[0] = b'X';
    assert_eq!(decode_one(&bytes), Err(ProtoError::BadMagic));
    // Detected as soon as the first bytes arrive.
    assert_eq!(decode_one(&bytes[..1]), Err(ProtoError::BadMagic));
}

#[test]
fn bad_magic_second_byte() {
    let mut bytes = enc(&Message::Bye, 0);
    bytes[1] = b'Z';
    assert_eq!(decode_one(&bytes[..2]), Err(ProtoError::BadMagic));
}

#[test]
fn bad_version() {
    let mut bytes = enc(&Message::Bye, 0);
    bytes[2] = 2;
    assert_eq!(decode_one(&bytes), Err(ProtoError::BadVersion(2)));
    assert_eq!(decode_one(&bytes[..3]), Err(ProtoError::BadVersion(2)));
}

#[test]
fn too_large_payload() {
    let mut bytes = enc(&Message::Bye, 0);
    let len = MAX_PAYLOAD + 1;
    bytes[8..12].copy_from_slice(&len.to_le_bytes());
    assert_eq!(decode_one(&bytes), Err(ProtoError::TooLarge(len)));
}

#[test]
fn max_payload_is_accepted_as_length() {
    // Exactly MAX_PAYLOAD is allowed; the decoder just waits for the bytes.
    let mut bytes = enc(&Message::Bye, 0);
    bytes[8..12].copy_from_slice(&MAX_PAYLOAD.to_le_bytes());
    assert_eq!(decode_one(&bytes), Ok(None));
}

#[test]
fn bad_payloads() {
    // Config with 11 bytes instead of 12.
    let mut f = vec![b'W', b'B', 1, 0x02, 0, 0, 0, 0, 11, 0, 0, 0];
    f.extend_from_slice(&[0; 11]);
    assert_eq!(
        decode_one(&f),
        Err(ProtoError::BadPayload { msg_type: 0x02 })
    );

    // Ping with 4 bytes.
    let mut f = vec![b'W', b'B', 1, 0x05, 0, 0, 0, 0, 4, 0, 0, 0];
    f.extend_from_slice(&[1, 2, 3, 4]);
    assert_eq!(
        decode_one(&f),
        Err(ProtoError::BadPayload { msg_type: 0x05 })
    );

    // KeyframeRequest with a body.
    let f = vec![b'W', b'B', 1, 0x04, 0, 0, 0, 0, 1, 0, 0, 0, 0];
    assert_eq!(
        decode_one(&f),
        Err(ProtoError::BadPayload { msg_type: 0x04 })
    );

    // Video shorter than its 12-byte header.
    let mut f = vec![b'W', b'B', 1, 0x03, 0, 0, 0, 0, 11, 0, 0, 0];
    f.extend_from_slice(&[0; 11]);
    assert_eq!(
        decode_one(&f),
        Err(ProtoError::BadPayload { msg_type: 0x03 })
    );

    // Stats with 11 bytes.
    let mut f = vec![b'W', b'B', 1, 0x08, 0, 0, 0, 0, 11, 0, 0, 0];
    f.extend_from_slice(&[0; 11]);
    assert_eq!(
        decode_one(&f),
        Err(ProtoError::BadPayload { msg_type: 0x08 })
    );

    // Orientation with 2 bytes, Bye with 1 byte.
    let mut f = vec![b'W', b'B', 1, 0x07, 0, 0, 0, 0, 2, 0, 0, 0, 1, 2];
    assert_eq!(
        decode_one(&f),
        Err(ProtoError::BadPayload { msg_type: 0x07 })
    );
    f = vec![b'W', b'B', 1, 0x09, 0, 0, 0, 0, 1, 0, 0, 0, 1];
    assert_eq!(
        decode_one(&f),
        Err(ProtoError::BadPayload { msg_type: 0x09 })
    );
}

#[test]
fn hello_bad_name_length_and_utf8() {
    // name_len says 3 but 2 bytes follow.
    let mut payload = vec![1, 1, 0, 0, 0xEA, 0x08, 0xD0, 0x05, 0x3C, 0, 3];
    payload.extend_from_slice(b"ab");
    let mut f = vec![b'W', b'B', 1, 0x01, 0, 0, 0, 0];
    f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    f.extend_from_slice(&payload);
    assert_eq!(
        decode_one(&f),
        Err(ProtoError::BadPayload { msg_type: 0x01 })
    );

    // Invalid UTF-8 name.
    let mut payload = vec![1, 1, 0, 0, 0xEA, 0x08, 0xD0, 0x05, 0x3C, 0, 2];
    payload.extend_from_slice(&[0xFF, 0xFE]);
    let mut f = vec![b'W', b'B', 1, 0x01, 0, 0, 0, 0];
    f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    f.extend_from_slice(&payload);
    assert_eq!(
        decode_one(&f),
        Err(ProtoError::BadPayload { msg_type: 0x01 })
    );
}

#[test]
fn unknown_type_is_surfaced() {
    let bytes = enc(
        &Message::Unknown {
            msg_type: 0x7F,
            payload: vec![9, 8, 7],
        },
        77,
    );
    assert_eq!(
        decode_one(&bytes),
        Ok(Some((
            77,
            Message::Unknown {
                msg_type: 0x7F,
                payload: vec![9, 8, 7]
            }
        )))
    );
}

#[test]
fn errors_display() {
    assert_eq!(ProtoError::BadMagic.to_string(), "bad frame magic");
    assert!(ProtoError::BadVersion(9).to_string().contains('9'));
    assert!(ProtoError::TooLarge(5_000_000)
        .to_string()
        .contains("5000000"));
    assert!(ProtoError::BadPayload { msg_type: 2 }
        .to_string()
        .contains("0x02"));
    let _: &dyn std::error::Error = &ProtoError::BadMagic;
}
