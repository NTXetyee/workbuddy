//! Capture -> encode -> sink loop shared by `--dump` and serve mode (SPEC 3.5).
//!
//! `CaptureEncoder` is the GPU side (Desktop Duplication plus hardware H.264). Capture runs on its
//! own thread and publishes each new desktop image into a [`Mailbox`]. The encoder stays on the
//! calling thread, takes the newest image and encodes it as soon as it is available. `run_loop` is
//! generic over where frames go, so the same loop feeds the iPad writer channel and the dump file.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use wb_capture::{CaptureStats, Capturer, Frame, OutputSel};
use wb_encode::{EncodeStats, EncodedFrame, Encoder, EncoderConfig};
use wb_proto::Config;

/// Codec id used in CONFIG (SPEC 2.2).
pub const CODEC_H264: u8 = 1;
/// Timeout passed to each Desktop Duplication acquire. The capture thread polls and does not block
/// inside `AcquireNextFrame`: on this device a blocking wait holds the shared D3D11 lock and stalls
/// the encoder. Measured encode time with a 100 ms wait was about 700 ms per frame, with a 1 ms
/// wait about 90 ms, and with this polling loop about 6 ms (the single-thread baseline).
const CAPTURE_POLL_MS: u32 = 0;
/// Pause after an empty poll. It bounds how long a new desktop image waits to be noticed.
const CAPTURE_IDLE_SLEEP: Duration = Duration::from_millis(1);
/// Longest the encoder blocks for a new image before it re-checks the stop flag and idle timer.
const MAX_WAIT: Duration = Duration::from_millis(100);
/// Highest accepted `idle_fps`.
pub const MAX_IDLE_FPS: u32 = 60;
const STATS_PERIOD: Duration = Duration::from_secs(1);
/// Minimum time between two forced (keyframe) encodes. A keyframe request inside this window
/// waits for it to pass; the request stays set, so it is not lost.
pub const FORCED_IDR_MIN_INTERVAL: Duration = Duration::from_millis(100);
/// Consecutive refused frames before the producer starts to back off between attempts.
const REFUSED_BACKOFF_AFTER: u32 = 5;
/// Sleep between attempts once the sink has refused [`REFUSED_BACKOFF_AFTER`] frames in a row.
const REFUSED_BACKOFF: Duration = Duration::from_millis(20);
/// The sink must accept a frame within this long of its first refusal, or the session ends.
pub const STALL_TIMEOUT: Duration = Duration::from_secs(3);
/// Error text when the stall watchdog ends a session (`STALL_TIMEOUT` in seconds).
const STALL_ERROR: &str = "iPad stopped reading for 3 s";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Single-slot mailbox between a producer thread and a consumer. Publishing replaces a value the
/// consumer has not taken yet (newest wins) and counts it as skipped. A consumer blocked in
/// [`Mailbox::wait_take`] is woken by `publish`.
pub struct Mailbox<T> {
    slot: Mutex<Slot<T>>,
    ready: Condvar,
}

struct Slot<T> {
    latest: Option<T>,
    /// Values replaced before they were taken, since the last [`Mailbox::take_skipped`].
    skipped: u64,
}

impl<T> Mailbox<T> {
    pub fn new() -> Self {
        Self {
            slot: Mutex::new(Slot {
                latest: None,
                skipped: 0,
            }),
            ready: Condvar::new(),
        }
    }

    /// Stores `value` as the newest one. An untaken older value is dropped and counted as skipped.
    pub fn publish(&self, value: T) {
        let mut slot = lock(&self.slot);
        if slot.latest.replace(value).is_some() {
            slot.skipped += 1;
        }
        drop(slot);
        self.ready.notify_one();
    }

    /// Takes the newest value if there is one. Never blocks.
    pub fn take_latest(&self) -> Option<T> {
        lock(&self.slot).latest.take()
    }

    /// Takes the newest value, waiting up to `timeout` for one to be published.
    pub fn wait_take(&self, timeout: Duration) -> Option<T> {
        let guard = lock(&self.slot);
        let (mut slot, _) = self
            .ready
            .wait_timeout_while(guard, timeout, |s| s.latest.is_none())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slot.latest.take()
    }

    /// Returns the number of replaced values since the previous call and resets the count.
    pub fn take_skipped(&self) -> u64 {
        std::mem::take(&mut lock(&self.slot).skipped)
    }
}

impl<T> Default for Mailbox<T> {
    fn default() -> Self {
        Self::new()
    }
}

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
    /// Repeat the last frame only once the screen has been unchanged for this long.
    pub repeat_after: Duration,
    /// Repeat rate of an unchanged screen, in frames per second (1..=[`MAX_IDLE_FPS`]).
    pub idle_fps: u32,
}

/// State shared between the capture thread and the encoder.
struct CaptureShared {
    frames: Mailbox<Frame>,
    /// Capture counters since the last stats line, merged by the capture thread.
    stats: Mutex<CaptureStats>,
    stop: AtomicBool,
    /// Set when capture fails; the encoder reports it on its next call.
    error: Mutex<Option<String>>,
}

/// Body of the capture thread: waits for desktop images and publishes each one.
fn capture_loop(mut cap: Capturer, shared: &CaptureShared) {
    while !shared.stop.load(SeqCst) {
        let result = cap.next_frame(CAPTURE_POLL_MS);
        lock(&shared.stats).add(&cap.take_stats());
        match result {
            Ok(Some(frame)) => shared.frames.publish(frame),
            Ok(None) => thread::sleep(CAPTURE_IDLE_SLEEP),
            Err(e) => {
                *lock(&shared.error) = Some(format!("capture: {e}"));
                break;
            }
        }
    }
}

/// Desktop Duplication capture feeding a hardware H.264 encoder, with idle repeat for static
/// screens. Capture runs on a dedicated thread; the encoder runs on the caller's thread.
pub struct CaptureEncoder {
    // Declared before `shared` so the encoder is released before the device it was created on.
    // The capture thread holds its own device reference and is joined in `Drop`.
    enc: Encoder,
    shared: Arc<CaptureShared>,
    capture: Option<JoinHandle<()>>,
    size: (u32, u32),
    fps: u16,
    bitrate_kbps: u32,
    /// The most recent image that was taken from the mailbox.
    last: Option<Frame>,
    /// When `last` arrived from capture.
    last_new_at: Instant,
    last_encode: Option<Instant>,
    /// When the last forced (keyframe-requested) encode started.
    last_forced: Option<Instant>,
    /// Time between idle repeats of an unchanged screen (1 / idle_fps).
    idle_interval: Duration,
    repeat_after: Duration,
    epoch: Instant,
}

impl CaptureEncoder {
    pub fn new(opts: EncodeOpts) -> Result<Self, String> {
        if opts.fps == 0 || opts.fps > u32::from(u16::MAX) {
            return Err(format!("unsupported fps {}", opts.fps));
        }
        if !(1..=MAX_IDLE_FPS).contains(&opts.idle_fps) {
            return Err(format!("unsupported idle fps {}", opts.idle_fps));
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
        let device = cap.device().clone();
        let enc = Encoder::new(&device, cfg).map_err(|e| format!("encoder: {e}"))?;
        drop(device);

        let shared = Arc::new(CaptureShared {
            frames: Mailbox::new(),
            stats: Mutex::new(CaptureStats::default()),
            stop: AtomicBool::new(false),
            error: Mutex::new(None),
        });
        let capture = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("wb-capture".to_string())
                .spawn(move || capture_loop(cap, &shared))
                .map_err(|e| format!("capture thread: {e}"))?
        };
        let now = Instant::now();
        Ok(Self {
            enc,
            shared,
            capture: Some(capture),
            size: (w, h),
            fps: opts.fps as u16,
            bitrate_kbps: opts.bitrate_kbps,
            last: None,
            last_new_at: now,
            last_encode: None,
            last_forced: None,
            idle_interval: Duration::from_secs_f64(1.0 / f64::from(opts.idle_fps)),
            repeat_after: opts.repeat_after,
            epoch: now,
        })
    }

    /// Encoded stream size (what the iPad will decode).
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The one-line `[encode]` summary printed at startup: backend, adapter, size, fps, bitrate.
    pub fn describe(&self) -> String {
        format!(
            "[encode] backend={} adapter={} {}x{} @{} fps {} kbps",
            self.enc.backend().name(),
            self.enc.adapter_name(),
            self.size.0,
            self.size.1,
            self.fps,
            self.bitrate_kbps
        )
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

    /// Fails if the capture thread reported an error or has stopped.
    fn check_capture(&self) -> Result<(), String> {
        let error = lock(&self.shared.error).clone();
        if let Some(e) = error {
            return Err(e);
        }
        if self.capture.as_ref().is_some_and(JoinHandle::is_finished) {
            return Err("capture thread stopped".to_string());
        }
        Ok(())
    }

    /// How long to wait for a new image: until the next idle repeat is due, at most `MAX_WAIT`.
    /// A pending keyframe request that is held back by the forced-encode gate also caps the wait
    /// at the moment the gate opens.
    fn wait_budget(&self, now: Instant, force_requested: bool) -> Duration {
        let mut budget = MAX_WAIT;
        if self.last.is_some() {
            let stale_at = self.last_new_at + self.repeat_after;
            let due = match self.last_encode {
                Some(t) => stale_at.max(t + self.idle_interval),
                None => stale_at,
            };
            budget = due.saturating_duration_since(now).min(MAX_WAIT);
        }
        if let (true, Some(t)) = (force_requested, self.last_forced) {
            budget = budget.min((t + FORCED_IDR_MIN_INTERVAL).saturating_duration_since(now));
        }
        budget
    }
}

impl Drop for CaptureEncoder {
    fn drop(&mut self) {
        self.shared.stop.store(true, SeqCst);
        if let Some(handle) = self.capture.take() {
            // A panicked capture thread has nothing left to clean up here.
            let _ = handle.join();
        }
    }
}

/// Whether a forced (keyframe) encode may start, given the time since the last forced encode.
/// `None` means there has been no forced encode yet.
pub fn forced_gate_open(since_last_forced: Option<Duration>) -> bool {
    since_last_forced.is_none_or(|d| d >= FORCED_IDR_MIN_INTERVAL)
}

/// Pacing decision for one loop iteration.
///
/// A new image (`pending`) is always encoded, with no interval gate. Desktop Duplication already
/// delivers at most one image per monitor refresh. A keyframe request (`force`) is encoded only
/// when [`forced_gate_open`] allows it, that is at most one forced encode per
/// [`FORCED_IDR_MIN_INTERVAL`]. A request inside that window is not encoded here. The caller keeps
/// it set, so it is answered once the window passes. A static screen is repeated only when it is
/// `stale` and at least `interval` has passed since the previous encode finished
/// (`since_last_encode` is `None` before the first encode). `interval` is `1 / idle_fps`.
/// `since_last_forced` is the time since the previous forced encode, `None` before the first one.
pub fn should_encode(
    pending: bool,
    stale: bool,
    force: bool,
    since_last_encode: Option<Duration>,
    interval: Duration,
    since_last_forced: Option<Duration>,
) -> bool {
    if pending || (force && forced_gate_open(since_last_forced)) {
        return true;
    }
    let due = since_last_encode.is_none_or(|d| d >= interval);
    stale && due
}

/// Watchdog decision: true when the refusal streak that began at `refused_since` has lasted at
/// least `limit`. `None` means the sink has not refused since it last accepted a frame.
pub fn stalled(refused_since: Option<Instant>, now: Instant, limit: Duration) -> bool {
    refused_since.is_some_and(|t| now.saturating_duration_since(t) >= limit)
}

/// Sleep before the next attempt after `consecutive_refused` refused frames in a row.
pub fn refusal_backoff(consecutive_refused: u32) -> Duration {
    if consecutive_refused >= REFUSED_BACKOFF_AFTER {
        REFUSED_BACKOFF
    } else {
        Duration::ZERO
    }
}

impl FrameSource for CaptureEncoder {
    fn next_frame(&mut self, force_idr: &AtomicBool) -> Result<Option<Produced>, String> {
        self.check_capture()?;
        let mut fresh = self.shared.frames.take_latest();
        // Nothing new and no keyframe request that may go now: sleep until a new image arrives,
        // an idle repeat is due, or the forced-encode gate opens. A keyframe request that may go
        // now skips the wait and is answered at once.
        let now0 = Instant::now();
        let force_requested = force_idr.load(SeqCst);
        let forced_now =
            force_requested && forced_gate_open(self.last_forced.map(|t| now0.duration_since(t)));
        if fresh.is_none() && !forced_now {
            fresh = self
                .shared
                .frames
                .wait_take(self.wait_budget(now0, force_requested));
        }
        let new = fresh.is_some();
        if let Some(frame) = fresh {
            self.last = Some(frame);
            self.last_new_at = Instant::now();
        }
        let Some(last) = self.last.as_ref() else {
            return Ok(None);
        };
        let force_requested = force_idr.load(SeqCst);
        let now = Instant::now();
        let stale = now.duration_since(self.last_new_at) >= self.repeat_after;
        let since_last_encode = self.last_encode.map(|t| now.duration_since(t));
        let since_last_forced = self.last_forced.map(|t| now.duration_since(t));
        if !should_encode(
            new,
            stale,
            force_requested,
            since_last_encode,
            self.idle_interval,
            since_last_forced,
        ) {
            return Ok(None);
        }
        // Only a request inside the gate is answered as a keyframe; otherwise the request stays
        // set for a later encode.
        let force =
            forced_gate_open(since_last_forced) && force_requested && force_idr.swap(false, SeqCst);
        let pts_us = self.epoch.elapsed().as_micros() as u64;
        let started = Instant::now();
        if force {
            self.last_forced = Some(started);
        }
        let out = self
            .enc
            .encode(&last.texture, pts_us, force)
            .map_err(|e| format!("encode: {e}"))?;
        let encode_time = started.elapsed();
        self.last_encode = Some(Instant::now());
        Ok(out.map(|frame| Produced { frame, encode_time }))
    }

    fn stats_fields(&mut self, window_secs: f64) -> String {
        let cap = std::mem::take(&mut *lock(&self.shared.stats));
        let skipped = self.shared.frames.take_skipped();
        let enc = self.enc.take_stats();
        stage_fields(&cap, &enc, skipped, window_secs)
    }
}

/// Formats the capture and encoder stage fields of the once-per-second line. Every average is
/// zero when its count is zero, so an idle window prints zeros instead of NaN. `skipped` is the
/// number of captured images replaced before the encoder took them.
fn stage_fields(cap: &CaptureStats, enc: &EncodeStats, skipped: u64, window_secs: f64) -> String {
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
        " cap_fps={:.1} cap_acc={:.1} cap_wait_ms={:.2} cap_to={:.1} conv_ms={:.2} mft_ms={:.2} in_wait_ms={:.2} gpu_conv_ms={:.2} skipped={:.1}",
        acq / secs,
        cap_acc,
        per_acq(cap.wait_total),
        cap.timeouts as f64 / secs,
        per_frame(enc.convert_total),
        per_frame(enc.mft_total),
        per_frame(enc.input_wait_total),
        gpu_ms,
        skipped as f64 / secs,
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
/// the next encode is a keyframe. After [`REFUSED_BACKOFF_AFTER`] refusals in a row the loop
/// sleeps between attempts. If the sink refuses for [`STALL_TIMEOUT`] without accepting a frame,
/// the loop returns an error. `extra` adds text to the once-per-second stats line.
pub fn run_loop<S: FrameSource, K: FrameSink>(
    src: &mut S,
    sink: &mut K,
    force_idr: &AtomicBool,
    should_stop: &mut dyn FnMut() -> bool,
    extra: &dyn Fn() -> String,
) -> Result<LoopSummary, String> {
    run_loop_with_stall(src, sink, force_idr, should_stop, extra, STALL_TIMEOUT)
}

/// [`run_loop`] with the stall limit passed in, so tests can use a short one.
fn run_loop_with_stall<S: FrameSource, K: FrameSink>(
    src: &mut S,
    sink: &mut K,
    force_idr: &AtomicBool,
    should_stop: &mut dyn FnMut() -> bool,
    extra: &dyn Fn() -> String,
    stall_limit: Duration,
) -> Result<LoopSummary, String> {
    let mut sum = LoopSummary::default();
    let mut win = Window::new();
    // Refusals since the last accepted frame, and when that streak began.
    let mut refused_run: u32 = 0;
    let mut refused_since: Option<Instant> = None;
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
                refused_run = 0;
                refused_since = None;
            } else {
                sum.dropped += 1;
                force_idr.store(true, SeqCst);
                refused_run = refused_run.saturating_add(1);
                let since = *refused_since.get_or_insert_with(Instant::now);
                if stalled(Some(since), Instant::now(), stall_limit) {
                    return Err(STALL_ERROR.to_string());
                }
                let backoff = refusal_backoff(refused_run);
                if !backoff.is_zero() {
                    thread::sleep(backoff);
                }
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
            stage_fields(&cap, &enc, 30, 1.0),
            " cap_fps=50.0 cap_acc=1.5 cap_wait_ms=10.00 cap_to=10.0 conv_ms=2.00 mft_ms=5.00 in_wait_ms=0.50 gpu_conv_ms=1.00 skipped=30.0"
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
            INTERVAL,
            None
        ));
        assert!(should_encode(true, true, false, None, INTERVAL, None));
        // A new frame is not held back by the forced-encode gate either.
        assert!(should_encode(
            true,
            false,
            true,
            Some(Duration::ZERO),
            INTERVAL,
            Some(Duration::ZERO)
        ));
    }

    #[test]
    fn no_new_frame_and_not_stale_does_not_encode() {
        assert!(!should_encode(false, false, false, None, INTERVAL, None));
        assert!(!should_encode(
            false,
            false,
            false,
            Some(INTERVAL * 10),
            INTERVAL,
            None
        ));
    }

    #[test]
    fn stale_and_due_repeats() {
        assert!(should_encode(
            false,
            true,
            false,
            Some(INTERVAL),
            INTERVAL,
            None
        ));
        assert!(should_encode(false, true, false, None, INTERVAL, None));
    }

    #[test]
    fn stale_but_not_due_does_not_encode() {
        assert!(!should_encode(
            false,
            true,
            false,
            Some(INTERVAL - Duration::from_micros(1)),
            INTERVAL,
            None
        ));
    }

    #[test]
    fn force_always_encodes() {
        assert!(should_encode(
            false,
            false,
            true,
            Some(Duration::ZERO),
            INTERVAL,
            None
        ));
        assert!(should_encode(false, false, true, None, INTERVAL, None));
    }

    #[test]
    fn forced_within_window_does_not_encode() {
        // Forced 99 ms after the last forced encode: held back, with no new frame or stale screen.
        assert!(!should_encode(
            false,
            false,
            true,
            None,
            INTERVAL,
            Some(FORCED_IDR_MIN_INTERVAL - Duration::from_millis(1))
        ));
        assert!(!should_encode(
            false,
            false,
            true,
            Some(Duration::ZERO),
            INTERVAL,
            Some(Duration::ZERO)
        ));
    }

    #[test]
    fn forced_after_window_encodes() {
        assert!(should_encode(
            false,
            false,
            true,
            None,
            INTERVAL,
            Some(FORCED_IDR_MIN_INTERVAL)
        ));
        assert!(should_encode(
            false,
            false,
            true,
            None,
            INTERVAL,
            Some(FORCED_IDR_MIN_INTERVAL * 5)
        ));
    }

    #[test]
    fn gated_force_still_allows_idle_repeat() {
        // The idle repeat itself is unchanged; the caller then encodes it without the keyframe.
        assert!(should_encode(
            false,
            true,
            true,
            Some(INTERVAL),
            INTERVAL,
            Some(Duration::ZERO)
        ));
    }

    #[test]
    fn forced_gate_boundary() {
        assert!(!forced_gate_open(Some(Duration::from_millis(99))));
        assert!(forced_gate_open(Some(Duration::from_millis(100))));
        assert!(forced_gate_open(None));
    }

    #[test]
    fn watchdog_only_fires_after_limit_of_refusals() {
        let now = Instant::now();
        let limit = Duration::from_secs(3);
        assert!(!stalled(None, now, limit));
        let recent = now - Duration::from_secs(2);
        assert!(!stalled(Some(recent), now, limit));
        let old = now - Duration::from_secs(3);
        assert!(stalled(Some(old), now, limit));
    }

    #[test]
    fn refusal_backoff_starts_after_five_refusals() {
        assert_eq!(refusal_backoff(4), Duration::ZERO);
        assert_eq!(refusal_backoff(5), REFUSED_BACKOFF);
        assert_eq!(refusal_backoff(50), REFUSED_BACKOFF);
    }

    #[test]
    fn always_refusing_sink_ends_with_stall_error() {
        let force = AtomicBool::new(false);
        let err = run_loop_with_stall(
            &mut Synthetic { n: 0 },
            &mut Refuse,
            &force,
            &mut || false,
            &String::new,
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert_eq!(err, STALL_ERROR);
        assert!(force.load(SeqCst));
    }

    /// Refuses every second frame, so the refusal streak never reaches the stall limit.
    struct Alternating {
        n: u64,
    }

    impl FrameSink for Alternating {
        fn offer(&mut self, _frame: EncodedFrame) -> Result<bool, String> {
            self.n += 1;
            Ok(self.n.is_multiple_of(2))
        }
    }

    #[test]
    fn accepted_frame_resets_the_stall_watchdog() {
        let force = AtomicBool::new(false);
        let mut n = 0;
        let mut stop = move || {
            n += 1;
            n > 10
        };
        // Every refusal is followed by an accept, so the streak never reaches the limit and the
        // loop ends normally when `stop` fires after 10 iterations.
        let sum = run_loop_with_stall(
            &mut Synthetic { n: 0 },
            &mut Alternating { n: 0 },
            &force,
            &mut stop,
            &String::new,
            Duration::from_millis(50),
        )
        .unwrap();
        assert_eq!(sum.sent, 5);
        assert_eq!(sum.dropped, 5);
    }

    #[test]
    fn stage_fields_idle_window_has_no_nan() {
        let line = stage_fields(&CaptureStats::default(), &EncodeStats::default(), 0, 1.0);
        assert!(!line.contains("NaN") && !line.contains("inf"), "{line}");
        assert!(line.starts_with(" cap_fps=0.0 cap_acc=0.0"), "{line}");
        assert!(line.ends_with(" skipped=0.0"), "{line}");
    }

    #[test]
    fn mailbox_newest_wins_and_counts_skipped() {
        let mb: Mailbox<u32> = Mailbox::new();
        mb.publish(1);
        mb.publish(2);
        mb.publish(3);
        assert_eq!(mb.take_latest(), Some(3));
        assert_eq!(mb.take_latest(), None);
        assert_eq!(mb.take_skipped(), 2);
        assert_eq!(mb.take_skipped(), 0);
    }

    #[test]
    fn mailbox_taken_value_is_not_counted_as_skipped() {
        let mb: Mailbox<u32> = Mailbox::new();
        mb.publish(1);
        assert_eq!(mb.take_latest(), Some(1));
        mb.publish(2);
        assert_eq!(mb.take_latest(), Some(2));
        assert_eq!(mb.take_skipped(), 0);
    }

    #[test]
    fn mailbox_wait_times_out_when_empty() {
        let mb: Mailbox<u32> = Mailbox::new();
        let started = Instant::now();
        assert_eq!(mb.wait_take(Duration::from_millis(20)), None);
        assert!(started.elapsed() >= Duration::from_millis(15));
    }

    #[test]
    fn mailbox_wait_returns_at_once_when_full() {
        let mb: Mailbox<u32> = Mailbox::new();
        mb.publish(5);
        assert_eq!(mb.wait_take(Duration::from_secs(60)), Some(5));
    }

    #[test]
    fn mailbox_publish_wakes_the_waiter() {
        let mb = Arc::new(Mailbox::<u32>::new());
        let publisher = {
            let mb = Arc::clone(&mb);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(20));
                mb.publish(7);
            })
        };
        let started = Instant::now();
        assert_eq!(mb.wait_take(Duration::from_secs(5)), Some(7));
        assert!(started.elapsed() < Duration::from_secs(5));
        publisher.join().unwrap();
    }
}
