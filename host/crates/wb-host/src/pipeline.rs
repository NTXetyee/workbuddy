//! Capture -> encode -> sink loop shared by `--dump` and serve mode (SPEC 3.5).
//!
//! `CaptureEncoder` is the GPU side (Desktop Duplication plus hardware H.264) and only runs on
//! the thread that owns the D3D11 device. `run_loop` is generic over where frames go, so the
//! same loop feeds the iPad writer channel and the dump file.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::{Duration, Instant};

use wb_capture::{CaptureStats, Capturer, Frame, OutputSel};
use wb_encode::{EncodeStats, EncodedFrame, Encoder, EncoderConfig};
use wb_proto::Config;

/// Codec id used in CONFIG (SPEC 2.2).
pub const CODEC_H264: u8 = 1;
/// How long one Desktop Duplication poll may block.
const CAPTURE_WAIT_MS: u32 = 8;
const STATS_PERIOD: Duration = Duration::from_secs(1);

/// One encoded frame and how long the encoder call took.
pub struct Produced {
    pub frame: EncodedFrame,
    pub encode_time: Duration,
}

/// Produces encoded frames. `force_idr` is set when a keyframe was requested. The source clears
/// it only when it actually encodes with the request applied.
pub trait FrameSource {
    fn next_frame(&mut self, force_idr: &AtomicBool) -> Result<Option<Produced>, String>;

    /// Extra fields for the once-per-second line, covering the last `window_secs` seconds.
    /// Resets the source's counters. Empty by default.
    fn stats_fields(&mut self, _window_secs: f64) -> String {
        String::new()
    }
}

/// Consumes encoded frames. `Ok(false)` means the sink refused the frame (back-pressure).
pub trait FrameSink {
    fn offer(&mut self, frame: EncodedFrame) -> Result<bool, String>;
}

/// Settings for [`CaptureEncoder::new`].
pub struct EncodeOpts {
    pub sel: OutputSel,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Encode size; `None` means the capture size. Odd sizes are rounded down to even.
    pub size: Option<(u32, u32)>,
    /// Re-encode the last frame once the screen has been unchanged for this long.
    pub repeat_after: Duration,
}

/// Desktop Duplication capture feeding a hardware H.264 encoder, with frame repeat for static
/// screens and fps pacing.
pub struct CaptureEncoder {
    // Declared before `cap` so the encoder is released before the device it was created on.
    enc: Encoder,
    cap: Capturer,
    size: (u32, u32),
    fps: u16,
    bitrate_kbps: u32,
    last: Option<Frame>,
    /// A new capture has not been encoded yet.
    pending: bool,
    last_new_at: Instant,
    last_encode: Option<Instant>,
    interval: Duration,
    repeat_after: Duration,
    epoch: Instant,
}

impl CaptureEncoder {
    pub fn new(opts: EncodeOpts) -> Result<Self, String> {
        if opts.fps == 0 || opts.fps > u32::from(u16::MAX) {
            return Err(format!("unsupported fps {}", opts.fps));
        }
        let cap = Capturer::new(opts.sel).map_err(|e| format!("capture: {e}"))?;
        let (cap_w, cap_h) = cap.size();
        let (w, h) = opts.size.unwrap_or((cap_w, cap_h));
        let (w, h) = (w & !1, h & !1);
        if w == 0 || h == 0 || w > u32::from(u16::MAX) || h > u32::from(u16::MAX) {
            return Err(format!("unsupported encode size {w}x{h}"));
        }
        let cfg = EncoderConfig {
            width: w,
            height: h,
            fps: opts.fps,
            bitrate_kbps: opts.bitrate_kbps,
        };
        let enc = Encoder::new(cap.device(), cfg).map_err(|e| format!("encoder: {e}"))?;
        let now = Instant::now();
        Ok(Self {
            enc,
            cap,
            size: (w, h),
            fps: opts.fps as u16,
            bitrate_kbps: opts.bitrate_kbps,
            last: None,
            pending: false,
            last_new_at: now,
            last_encode: None,
            interval: Duration::from_secs_f64(1.0 / f64::from(opts.fps)),
            repeat_after: opts.repeat_after,
            epoch: now,
        })
    }

    /// Encoded stream size (what the iPad will decode).
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The CONFIG message describing this stream.
    pub fn config(&self) -> Config {
        Config {
            codec: CODEC_H264,
            width: self.size.0 as u16,
            height: self.size.1 as u16,
            fps: self.fps,
            bitrate_kbps: self.bitrate_kbps,
        }
    }
}

/// Pacing decision for one loop iteration.
///
/// A new capture (`pending`) and a keyframe request (`force`) are always encoded: Desktop
/// Duplication already delivers at most one image per monitor refresh, so new frames self-limit.
/// A static screen is repeated only when it is `stale` and at least `interval` has passed since
/// the previous encode finished (`since_last_encode` is `None` before the first encode).
pub fn should_encode(
    pending: bool,
    stale: bool,
    force: bool,
    since_last_encode: Option<Duration>,
    interval: Duration,
) -> bool {
    if pending || force {
        return true;
    }
    let due = since_last_encode.is_none_or(|d| d >= interval);
    stale && due
}

impl FrameSource for CaptureEncoder {
    fn next_frame(&mut self, force_idr: &AtomicBool) -> Result<Option<Produced>, String> {
        if let Some(frame) = self
            .cap
            .next_frame(CAPTURE_WAIT_MS)
            .map_err(|e| format!("capture: {e}"))?
        {
            self.last = Some(frame);
            self.pending = true;
            self.last_new_at = Instant::now();
        }
        let Some(last) = self.last.as_ref() else {
            return Ok(None);
        };
        let now = Instant::now();
        let stale = now.duration_since(self.last_new_at) >= self.repeat_after;
        let since_last_encode = self.last_encode.map(|t| now.duration_since(t));
        if !should_encode(
            self.pending,
            stale,
            force_idr.load(SeqCst),
            since_last_encode,
            self.interval,
        ) {
            return Ok(None);
        }
        let force = force_idr.swap(false, SeqCst);
        let pts_us = self.epoch.elapsed().as_micros() as u64;
        let started = Instant::now();
        let out = self
            .enc
            .encode(&last.texture, pts_us, force)
            .map_err(|e| format!("encode: {e}"))?;
        let encode_time = started.elapsed();
        self.pending = false;
        self.last_encode = Some(Instant::now());
        Ok(out.map(|frame| Produced { frame, encode_time }))
    }

    fn stats_fields(&mut self, window_secs: f64) -> String {
        let cap = self.cap.take_stats();
        let enc = self.enc.take_stats();
        stage_fields(&cap, &enc, window_secs)
    }
}

/// Formats the capture and encoder stage fields of the once-per-second line. Every average is
/// zero when its count is zero, so an idle window prints zeros instead of NaN.
fn stage_fields(cap: &CaptureStats, enc: &EncodeStats, window_secs: f64) -> String {
    let secs = window_secs.max(f64::EPSILON);
    let acq = cap.acquired as f64;
    let per_acq = |total: std::time::Duration| {
        if cap.acquired == 0 {
            0.0
        } else {
            total.as_secs_f64() * 1000.0 / acq
        }
    };
    let frames = enc.frames as f64;
    let per_frame = |total: std::time::Duration| {
        if enc.frames == 0 {
            0.0
        } else {
            total.as_secs_f64() * 1000.0 / frames
        }
    };
    let cap_acc = if cap.acquired == 0 {
        0.0
    } else {
        cap.accumulated_frames_sum as f64 / acq
    };
    let gpu_ms = if enc.gpu_samples == 0 {
        0.0
    } else {
        enc.gpu_convert_total.as_secs_f64() * 1000.0 / enc.gpu_samples as f64
    };
    format!(
        " cap_fps={:.1} cap_acc={:.1} cap_wait_ms={:.2} cap_to={:.1} conv_ms={:.2} mft_ms={:.2} in_wait_ms={:.2} gpu_conv_ms={:.2}",
        acq / secs,
        cap_acc,
        per_acq(cap.wait_total),
        cap.timeouts as f64 / secs,
        per_frame(enc.convert_total),
        per_frame(enc.mft_total),
        per_frame(enc.input_wait_total),
        gpu_ms,
    )
}

/// Totals for one `run_loop` call.
#[derive(Debug, Default, Clone)]
pub struct LoopSummary {
    pub produced: u64,
    pub keyframes: u64,
    pub bytes: u64,
    pub encode_total: Duration,
    /// Frames the sink accepted.
    pub sent: u64,
    /// Frames the sink refused (a keyframe is then forced).
    pub dropped: u64,
}

impl LoopSummary {
    pub fn avg_encode_ms(&self) -> f64 {
        if self.produced == 0 {
            0.0
        } else {
            self.encode_total.as_secs_f64() * 1000.0 / self.produced as f64
        }
    }
}

/// Counters for the once-per-second log line.
struct Window {
    start: Instant,
    frames: u64,
    bytes: u64,
    encode: Duration,
}

impl Window {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            frames: 0,
            bytes: 0,
            encode: Duration::ZERO,
        }
    }

    fn line(&self, sum: &LoopSummary, extra: &str) -> String {
        let secs = self.start.elapsed().as_secs_f64().max(f64::EPSILON);
        let enc_ms = if self.frames == 0 {
            0.0
        } else {
            self.encode.as_secs_f64() * 1000.0 / self.frames as f64
        };
        format!(
            "fps={:.1} enc_ms={:.2} kbps={:.0} sent={} dropped={}{}",
            self.frames as f64 / secs,
            enc_ms,
            self.bytes as f64 * 8.0 / 1000.0 / secs,
            sum.sent,
            sum.dropped,
            extra
        )
    }
}

/// Runs source -> sink until `should_stop` returns true. A refused frame sets `force_idr` so
/// the next encode is a keyframe. `extra` adds text to the once-per-second stats line.
pub fn run_loop<S: FrameSource, K: FrameSink>(
    src: &mut S,
    sink: &mut K,
    force_idr: &AtomicBool,
    should_stop: &mut dyn FnMut() -> bool,
    extra: &dyn Fn() -> String,
) -> Result<LoopSummary, String> {
    let mut sum = LoopSummary::default();
    let mut win = Window::new();
    while !should_stop() {
        if let Some(p) = src.next_frame(force_idr)? {
            let bytes = p.frame.data.len() as u64;
            sum.produced += 1;
            sum.bytes += bytes;
            sum.keyframes += u64::from(p.frame.keyframe);
            sum.encode_total += p.encode_time;
            win.frames += 1;
            win.bytes += bytes;
            win.encode += p.encode_time;
            if sink.offer(p.frame)? {
                sum.sent += 1;
            } else {
                sum.dropped += 1;
                force_idr.store(true, SeqCst);
            }
        }
        if win.start.elapsed() >= STATS_PERIOD {
            let secs = win.start.elapsed().as_secs_f64();
            let fields = format!("{}{}", src.stats_fields(secs), extra());
            eprintln!("{}", win.line(&sum, &fields));
            win = Window::new();
        }
    }
    Ok(sum)
}

/// Writes every frame's Annex-B data to `out` (used by `--dump`).
pub struct WriteSink<W: Write> {
    out: W,
}

impl<W: Write> WriteSink<W> {
    pub fn new(out: W) -> Self {
        Self { out }
    }

    pub fn finish(&mut self) -> Result<(), String> {
        self.out.flush().map_err(|e| format!("flush: {e}"))
    }
}

impl<W: Write> FrameSink for WriteSink<W> {
    fn offer(&mut self, frame: EncodedFrame) -> Result<bool, String> {
        self.out
            .write_all(&frame.data)
            .map_err(|e| format!("write: {e}"))?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Yields a small Annex-B frame every call.
    struct Synthetic {
        n: u64,
    }

    impl FrameSource for Synthetic {
        fn next_frame(&mut self, force_idr: &AtomicBool) -> Result<Option<Produced>, String> {
            self.n += 1;
            let keyframe = force_idr.swap(false, SeqCst);
            Ok(Some(Produced {
                frame: EncodedFrame {
                    data: vec![0, 0, 0, 1, 0x41, self.n as u8],
                    keyframe,
                    has_params: keyframe,
                    pts_us: self.n,
                },
                encode_time: Duration::from_micros(100),
            }))
        }
    }

    struct Refuse;

    impl FrameSink for Refuse {
        fn offer(&mut self, _frame: EncodedFrame) -> Result<bool, String> {
            Ok(false)
        }
    }

    #[test]
    fn refused_frame_forces_next_keyframe() {
        let force = AtomicBool::new(false);
        let mut n = 0;
        let mut stop = move || {
            n += 1;
            n > 2
        };
        let sum = run_loop(
            &mut Synthetic { n: 0 },
            &mut Refuse,
            &force,
            &mut stop,
            &String::new,
        )
        .unwrap();
        assert_eq!(sum.produced, 2);
        assert_eq!(sum.dropped, 2);
        assert_eq!(sum.sent, 0);
        assert!(force.load(SeqCst));
    }

    #[test]
    fn write_sink_appends_every_frame() {
        let mut sink = WriteSink::new(Vec::new());
        let frame = EncodedFrame {
            data: vec![1, 2, 3],
            keyframe: true,
            has_params: true,
            pts_us: 0,
        };
        assert!(sink.offer(frame).unwrap());
        assert_eq!(sink.out, vec![1, 2, 3]);
    }

    #[test]
    fn stage_fields_average_over_counts() {
        let cap = CaptureStats {
            acquired: 50,
            accumulated_frames_sum: 75,
            timeouts: 10,
            wait_total: Duration::from_millis(500),
            copy_total: Duration::from_millis(5),
        };
        let enc = EncodeStats {
            frames: 50,
            convert_total: Duration::from_millis(100),
            mft_total: Duration::from_millis(250),
            input_wait_total: Duration::from_millis(25),
            gpu_convert_total: Duration::from_millis(40),
            gpu_samples: 40,
        };
        assert_eq!(
            stage_fields(&cap, &enc, 1.0),
            " cap_fps=50.0 cap_acc=1.5 cap_wait_ms=10.00 cap_to=10.0 conv_ms=2.00 mft_ms=5.00 in_wait_ms=0.50 gpu_conv_ms=1.00"
        );
    }

    const INTERVAL: Duration = Duration::from_micros(16_667);

    #[test]
    fn new_frame_always_encodes() {
        // Even right after an encode, with no interval elapsed.
        assert!(should_encode(
            true,
            false,
            false,
            Some(Duration::ZERO),
            INTERVAL
        ));
        assert!(should_encode(true, true, false, None, INTERVAL));
    }

    #[test]
    fn no_new_frame_and_not_stale_does_not_encode() {
        assert!(!should_encode(false, false, false, None, INTERVAL));
        assert!(!should_encode(
            false,
            false,
            false,
            Some(INTERVAL * 10),
            INTERVAL
        ));
    }

    #[test]
    fn stale_and_due_repeats() {
        assert!(should_encode(false, true, false, Some(INTERVAL), INTERVAL));
        assert!(should_encode(false, true, false, None, INTERVAL));
    }

    #[test]
    fn stale_but_not_due_does_not_encode() {
        assert!(!should_encode(
            false,
            true,
            false,
            Some(INTERVAL - Duration::from_micros(1)),
            INTERVAL
        ));
    }

    #[test]
    fn force_always_encodes() {
        assert!(should_encode(
            false,
            false,
            true,
            Some(Duration::ZERO),
            INTERVAL
        ));
        assert!(should_encode(false, false, true, None, INTERVAL));
    }

    #[test]
    fn stage_fields_idle_window_has_no_nan() {
        let line = stage_fields(&CaptureStats::default(), &EncodeStats::default(), 1.0);
        assert!(!line.contains("NaN") && !line.contains("inf"), "{line}");
        assert!(line.starts_with(" cap_fps=0.0 cap_acc=0.0"), "{line}");
    }
}
