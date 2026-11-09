//! One iPad session over an established tunnel (SPEC 2.4 and 3.5).
//!
//! The handshake runs on the calling thread: HELLO, then CONFIG. After that the caller runs the
//! frame producer and feeds a `sync_channel(1)`. A writer thread drains that channel to the
//! socket. A reader thread handles KEYFRAME_REQUEST, PING, STATS and BYE. Control replies share
//! the socket through a mutex so a PONG never interleaves with a VIDEO frame.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use wb_encode::EncodedFrame;
use wb_proto::{Config, FrameDecoder, Hello, Message, Stats, Video};

use crate::pipeline::{self, FrameSink, FrameSource};

/// How long the iPad has to send HELLO after connecting.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnd {
    /// The iPad sent BYE.
    Bye,
    /// The socket reached EOF.
    Closed,
    /// The host was asked to stop (Ctrl+C).
    Stopped,
    /// Protocol, I/O or pipeline failure.
    Failed(String),
}

/// Host->iPad direction: serialised writes plus the outgoing sequence counter.
struct Link {
    stream: Mutex<TcpStream>,
    seq: AtomicU32,
}

impl Link {
    fn send(&self, msg: &Message, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.clear();
        wb_proto::encode(msg, self.seq.fetch_add(1, SeqCst), buf);
        lock(&self.stream).write_all(buf)
    }
}

#[derive(Default)]
struct Shared {
    force_idr: AtomicBool,
    session_over: AtomicBool,
    ipad_stats: Mutex<Option<Stats>>,
    writer_error: Mutex<Option<String>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Runs one session to completion on the calling thread.
///
/// `make_config` receives the iPad's HELLO and returns the CONFIG to send. `source` produces the
/// frames. The session ends on BYE, EOF, an I/O or protocol error, or when `stop` is set.
pub fn run_session<S: FrameSource>(
    mut stream: TcpStream,
    make_config: impl FnOnce(&Hello) -> Config,
    source: &mut S,
    stop: &AtomicBool,
) -> SessionEnd {
    let (hello, dec) = match read_hello(&mut stream) {
        Ok(v) => v,
        Err(end) => return end,
    };
    let cfg = make_config(&hello);
    eprintln!(
        "[net] iPad '{}' connected, native {}x{}, max {} fps",
        hello.name, hello.native_width, hello.native_height, hello.max_fps
    );

    if let Err(e) = stream.set_read_timeout(None) {
        return SessionEnd::Failed(format!("socket setup: {e}"));
    }
    let writer_half = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => return SessionEnd::Failed(format!("socket clone: {e}")),
    };
    // Used only for shutdown, so it never waits behind a writer blocked in write_all.
    let shutdown_half = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => return SessionEnd::Failed(format!("socket clone: {e}")),
    };
    let link = Arc::new(Link {
        stream: Mutex::new(writer_half),
        seq: AtomicU32::new(0),
    });
    let shared = Arc::new(Shared::default());
    // The first frame of a session is always an IDR (SPEC 2.4).
    shared.force_idr.store(true, SeqCst);

    let mut buf = Vec::new();
    if let Err(e) = link.send(&Message::Config(cfg), &mut buf) {
        return SessionEnd::Failed(format!("send CONFIG: {e}"));
    }

    let (tx, rx) = mpsc::sync_channel::<Message>(1);
    let writer = {
        let (link, shared) = (Arc::clone(&link), Arc::clone(&shared));
        thread::spawn(move || writer_loop(rx, &link, &shared))
    };
    let reader = {
        let (link, shared) = (Arc::clone(&link), Arc::clone(&shared));
        thread::spawn(move || {
            let end = reader_loop(stream, dec, &link, &shared);
            shared.session_over.store(true, SeqCst);
            end
        })
    };

    let mut sink = ChannelSink { tx };
    let rate_base = Mutex::new(None);
    let extra = || {
        let stats = lock(&shared.ipad_stats).clone();
        ipad_suffix(stats.as_ref(), &mut lock(&rate_base), Instant::now())
    };
    let mut should_stop = || stop.load(SeqCst) || shared.session_over.load(SeqCst);
    let run = pipeline::run_loop(
        source,
        &mut sink,
        &shared.force_idr,
        &mut should_stop,
        &extra,
    );

    let loop_err = run.err();
    let host_stop = stop.load(SeqCst);
    shared.session_over.store(true, SeqCst);
    drop(sink);
    if host_stop {
        let _ = link.send(&Message::Bye, &mut buf);
    }
    let _ = shutdown_half.shutdown(Shutdown::Both);
    let _ = writer.join();
    let reader_end = reader
        .join()
        .unwrap_or_else(|_| SessionEnd::Failed("reader thread panicked".to_string()));

    let writer_err = lock(&shared.writer_error).take();
    match (writer_err, loop_err, host_stop) {
        (Some(e), _, _) => SessionEnd::Failed(e),
        (None, _, true) => SessionEnd::Stopped,
        (None, Some(e), false) => SessionEnd::Failed(e),
        (None, None, false) => reader_end,
    }
}

/// Counters from the previous stats line: when it was printed and the iPad's decoded and
/// dropped totals at that time.
type RateBase = Option<(Instant, u32, u32)>;

/// The iPad part of the once-per-second stats line. Empty until a STATS message has arrived.
/// `ipad_dec_fps` and `ipad_drop_ps` are the iPad counter deltas since the previous stats line,
/// per second. The first line that has stats has no previous line, so both are 0.0 there.
fn ipad_suffix(stats: Option<&Stats>, base: &mut RateBase, now: Instant) -> String {
    let Some(s) = stats else {
        return String::new();
    };
    let (dec_ps, drop_ps) = match *base {
        Some((t0, dec0, drop0)) => {
            let secs = now.duration_since(t0).as_secs_f64().max(f64::EPSILON);
            (
                f64::from(s.frames_decoded.saturating_sub(dec0)) / secs,
                f64::from(s.frames_dropped.saturating_sub(drop0)) / secs,
            )
        }
        None => (0.0, 0.0),
    };
    *base = Some((now, s.frames_decoded, s.frames_dropped));
    format!(
        " ipad_dec={} ipad_drop={} ipad_dec_ms={:.1} ipad_dec_fps={:.1} ipad_drop_ps={:.1}",
        s.frames_decoded,
        s.frames_dropped,
        f64::from(s.last_decode_us) / 1000.0,
        dec_ps,
        drop_ps
    )
}

/// Reads until HELLO arrives. Anything before it is ignored. The decoder keeps any bytes that
/// were read past HELLO, so the reader thread sees them.
fn read_hello(stream: &mut TcpStream) -> Result<(Hello, FrameDecoder), SessionEnd> {
    let deadline = Instant::now() + HELLO_TIMEOUT;
    let mut dec = FrameDecoder::new();
    let mut chunk = vec![0u8; 16 * 1024];
    loop {
        loop {
            match dec.next() {
                Ok(Some((_, Message::Hello(hello)))) => return Ok((hello, dec)),
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) => return Err(SessionEnd::Failed(format!("protocol error: {e}"))),
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(SessionEnd::Failed("no HELLO within 5 s".to_string()));
        }
        if let Err(e) = stream.set_read_timeout(Some(deadline - now)) {
            return Err(SessionEnd::Failed(format!("socket setup: {e}")));
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(SessionEnd::Closed),
            Ok(n) => dec.push(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if is_timeout(&e) => {
                return Err(SessionEnd::Failed("no HELLO within 5 s".to_string()))
            }
            Err(e) => return Err(SessionEnd::Failed(format!("read: {e}"))),
        }
    }
}

fn writer_loop(rx: Receiver<Message>, link: &Link, shared: &Shared) {
    let mut buf = Vec::new();
    for msg in rx {
        if let Err(e) = link.send(&msg, &mut buf) {
            if !shared.session_over.load(SeqCst) {
                *lock(&shared.writer_error) = Some(format!("write to iPad failed: {e}"));
            }
            shared.session_over.store(true, SeqCst);
            // Returning drops the receiver, so the producer's try_send fails and the loop ends.
            break;
        }
    }
}

fn reader_loop(
    mut stream: TcpStream,
    mut dec: FrameDecoder,
    link: &Link,
    shared: &Shared,
) -> SessionEnd {
    let mut chunk = vec![0u8; 64 * 1024];
    let mut buf = Vec::new();
    loop {
        loop {
            match dec.next() {
                Ok(Some((_, msg))) => match msg {
                    Message::KeyframeRequest => shared.force_idr.store(true, SeqCst),
                    Message::Ping(t) => {
                        if let Err(e) = link.send(&Message::Pong(t), &mut buf) {
                            return SessionEnd::Failed(format!("send PONG: {e}"));
                        }
                    }
                    Message::Stats(s) => *lock(&shared.ipad_stats) = Some(s),
                    Message::Bye => return SessionEnd::Bye,
                    // Repeated HELLO, reserved ORIENTATION/TOUCH and unknown types are ignored.
                    _ => {}
                },
                Ok(None) => break,
                Err(e) => return SessionEnd::Failed(format!("protocol error: {e}")),
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) => return SessionEnd::Closed,
            Ok(n) => dec.push(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return SessionEnd::Failed(format!("read: {e}")),
        }
    }
}

/// Hands encoded frames to the writer thread without ever blocking (SPEC 2.4 rule 5).
struct ChannelSink {
    tx: SyncSender<Message>,
}

impl FrameSink for ChannelSink {
    fn offer(&mut self, frame: EncodedFrame) -> Result<bool, String> {
        let flags = u8::from(frame.keyframe) | (u8::from(frame.has_params) << 1);
        let msg = Message::Video(Video {
            pts_us: frame.pts_us,
            flags,
            data: frame.data,
        });
        match self.tx.try_send(msg) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => Err("writer thread stopped".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(decoded: u32, dropped: u32, decode_us: u32) -> Stats {
        Stats {
            frames_decoded: decoded,
            frames_dropped: dropped,
            last_decode_us: decode_us,
        }
    }

    #[test]
    fn no_suffix_before_first_stats() {
        let mut base = None;
        assert_eq!(ipad_suffix(None, &mut base, Instant::now()), "");
        assert!(base.is_none());
    }

    #[test]
    fn first_stats_line_has_zero_rates() {
        let mut base = None;
        let s = stats(100, 3, 12_345);
        assert_eq!(
            ipad_suffix(Some(&s), &mut base, Instant::now()),
            " ipad_dec=100 ipad_drop=3 ipad_dec_ms=12.3 ipad_dec_fps=0.0 ipad_drop_ps=0.0"
        );
    }

    #[test]
    fn rates_are_deltas_per_second() {
        let mut base = None;
        let t0 = Instant::now();
        ipad_suffix(Some(&stats(100, 3, 1_000)), &mut base, t0);
        let s = stats(160, 5, 2_500);
        let line = ipad_suffix(Some(&s), &mut base, t0 + Duration::from_secs(2));
        assert_eq!(
            line,
            " ipad_dec=160 ipad_drop=5 ipad_dec_ms=2.5 ipad_dec_fps=30.0 ipad_drop_ps=1.0"
        );
    }

    #[test]
    fn counter_reset_does_not_underflow() {
        let mut base = None;
        let t0 = Instant::now();
        ipad_suffix(Some(&stats(500, 9, 1_000)), &mut base, t0);
        let line = ipad_suffix(
            Some(&stats(10, 0, 1_000)),
            &mut base,
            t0 + Duration::from_secs(1),
        );
        assert!(
            line.ends_with("ipad_dec_fps=0.0 ipad_drop_ps=0.0"),
            "{line}"
        );
    }
}
