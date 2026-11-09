//! Session test against a local fake iPad (no GPU, no usbmuxd).
//!
//! The fake iPad connects over 127.0.0.1 and checks the wire behaviour of `net::run_session`:
//! HELLO, then CONFIG, then VIDEO with increasing pts. A KEYFRAME_REQUEST must reach the frame
//! source as `force_idr`, and PING must get PONG. BYE ends the session.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::thread;
use std::time::Duration;

use wb_encode::EncodedFrame;
use wb_host::net::{run_session, SessionEnd};
use wb_host::pipeline::{FrameSource, Produced};
use wb_proto::{Config, FrameDecoder, Hello, Message};

/// Produces synthetic Annex-B frames at roughly 200 fps. Keyframes are the forced ones.
struct FakeSource {
    n: u64,
    saw_force: bool,
}

impl FrameSource for FakeSource {
    fn next_frame(&mut self, force_idr: &AtomicBool) -> Result<Option<Produced>, String> {
        thread::sleep(Duration::from_millis(5));
        let forced = force_idr.swap(false, SeqCst);
        self.saw_force |= forced;
        self.n += 1;
        let mut data = Vec::new();
        if forced {
            // Fake SPS and PPS, then the IDR slice.
            data.extend_from_slice(&[0, 0, 0, 1, 0x67, 0x42, 0xC0, 0x1F]);
            data.extend_from_slice(&[0, 0, 0, 1, 0x68, 0xCE, 0x3C, 0x80]);
            data.extend_from_slice(&[0, 0, 0, 1, 0x65, self.n as u8]);
        } else {
            data.extend_from_slice(&[0, 0, 0, 1, 0x41, self.n as u8]);
        }
        Ok(Some(Produced {
            frame: EncodedFrame {
                data,
                keyframe: forced,
                has_params: forced,
                pts_us: self.n * 16_667,
            },
            encode_time: Duration::from_micros(200),
        }))
    }
}

struct IpadReport {
    config: Config,
    videos: usize,
    pts_increasing: bool,
    keyframe_after_request: bool,
    keyframe_flags_ok: bool,
    pong: u64,
}

const PING_TOKEN: u64 = 42;

fn send(s: &mut TcpStream, seq: &mut u32, msg: &Message) -> Result<(), String> {
    let mut out = Vec::new();
    wb_proto::encode(msg, *seq, &mut out);
    *seq += 1;
    s.write_all(&out).map_err(|e| format!("write: {e}"))
}

fn recv(s: &mut TcpStream, dec: &mut FrameDecoder) -> Result<Message, String> {
    loop {
        if let Some((_, msg)) = dec.next().map_err(|e| e.to_string())? {
            return Ok(msg);
        }
        let mut buf = [0u8; 16 * 1024];
        let n = s.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("host closed the connection early".to_string());
        }
        dec.push(&buf[..n]);
    }
}

/// Counts a VIDEO frame and checks that its pts is larger than the previous one.
fn check_video(report: &mut IpadReport, last_pts: &mut Option<u64>, pts: u64) {
    report.videos += 1;
    if last_pts.is_some_and(|p| pts <= p) {
        report.pts_increasing = false;
    }
    *last_pts = Some(pts);
}

fn run_fake_ipad(mut s: TcpStream) -> Result<IpadReport, String> {
    s.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let mut seq = 0u32;
    let mut dec = FrameDecoder::new();

    let hello = Hello {
        proto_version: 1,
        codecs: 0x01,
        orientation: 0,
        native_width: 2266,
        native_height: 1488,
        max_fps: 60,
        name: "fake iPad".to_string(),
    };
    send(&mut s, &mut seq, &Message::Hello(hello))?;
    let config = match recv(&mut s, &mut dec)? {
        Message::Config(c) => c,
        other => return Err(format!("expected CONFIG, got {other:?}")),
    };

    let mut report = IpadReport {
        config,
        videos: 0,
        pts_increasing: true,
        keyframe_after_request: false,
        keyframe_flags_ok: false,
        pong: 0,
    };
    let mut last_pts: Option<u64> = None;

    // 1. The first VIDEO frames must be increasing and the first one an IDR.
    let mut first = true;
    while report.videos < 20 {
        match recv(&mut s, &mut dec)? {
            Message::Video(v) => {
                if first {
                    report.keyframe_flags_ok = v.flags & 0b11 == 0b11;
                    first = false;
                }
                check_video(&mut report, &mut last_pts, v.pts_us);
            }
            other => return Err(format!("unexpected message before request: {other:?}")),
        }
    }

    // 2. KEYFRAME_REQUEST must produce a keyframe with SPS/PPS later than the request.
    let pts_at_request = last_pts.unwrap_or(0);
    send(&mut s, &mut seq, &Message::KeyframeRequest)?;
    loop {
        match recv(&mut s, &mut dec)? {
            Message::Video(v) => {
                let key = v.flags & 0b01 == 0b01;
                check_video(&mut report, &mut last_pts, v.pts_us);
                if key && v.pts_us > pts_at_request {
                    report.keyframe_after_request = true;
                    report.keyframe_flags_ok &= v.flags & 0b11 == 0b11;
                    break;
                }
            }
            other => return Err(format!("unexpected message after request: {other:?}")),
        }
    }

    // 3. PING must come back as PONG with the same token.
    send(&mut s, &mut seq, &Message::Ping(PING_TOKEN))?;
    loop {
        match recv(&mut s, &mut dec)? {
            Message::Pong(t) => {
                report.pong = t;
                break;
            }
            Message::Video(v) => check_video(&mut report, &mut last_pts, v.pts_us),
            other => return Err(format!("unexpected message waiting for PONG: {other:?}")),
        }
    }

    // 4. Ending the session with BYE.
    send(&mut s, &mut seq, &Message::Bye)?;
    Ok(report)
}

#[test]
fn handshake_video_keyframe_request_and_ping() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let ipad = thread::spawn(move || {
        let (stream, _) = listener.accept().map_err(|e| e.to_string())?;
        run_fake_ipad(stream)
    });

    let host = TcpStream::connect(addr).expect("connect to fake iPad");
    let expected = Config {
        codec: 1,
        width: 1132,
        height: 744,
        fps: 60,
        bitrate_kbps: 20_000,
    };
    let mut source = FakeSource {
        n: 0,
        saw_force: false,
    };
    let stop = AtomicBool::new(false);
    let to_send = expected.clone();
    let end = run_session(host, move |_hello| to_send, &mut source, &stop);

    let report = ipad
        .join()
        .expect("fake iPad thread panicked")
        .expect("fake iPad failed");

    assert_eq!(end, SessionEnd::Bye);
    assert_eq!(
        report.config, expected,
        "CONFIG must match what make_config returned"
    );
    assert!(report.videos >= 20, "expected at least 20 VIDEO frames");
    assert!(report.pts_increasing, "VIDEO pts must strictly increase");
    assert!(
        report.keyframe_flags_ok,
        "keyframes must carry flags bit0 and bit1"
    );
    assert!(
        source.saw_force,
        "KEYFRAME_REQUEST must set force_idr on the source"
    );
    assert!(
        report.keyframe_after_request,
        "a keyframe must follow the request"
    );
    assert_eq!(
        report.pong, PING_TOKEN,
        "PING must be answered with the same token"
    );
}
