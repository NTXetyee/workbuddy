//! Console output for `wb-host` (SPEC 3.5). Quiet by default: one line per event, and while the
//! iPad streams, one status line that is redrawn in place once a second. `--verbose` prints every
//! diagnostic line and the full per-second stats line instead, and no status line.
//!
//! Every line goes through this module so that a status line on screen is cleared before any
//! other line is printed.

use std::ffi::c_void;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Mutex, MutexGuard};

use crate::net::SessionEnd;
use crate::pipeline::{self, Tick};

/// At or below this many frames per second the screen counts as static.
pub const IDLE_FPS_MAX: f64 = 8.0;

const STD_ERROR_HANDLE: u32 = -12i32 as u32;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

extern "system" {
    fn GetStdHandle(std_handle: u32) -> *mut c_void;
    fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
    fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
}

static VERBOSE: AtomicBool = AtomicBool::new(false);
static CONSOLE: Mutex<Console> = Mutex::new(Console {
    live: false,
    vt: false,
    shown: false,
    width: 0,
});

struct Console {
    /// Quiet mode on a real console: the status line is drawn in place.
    live: bool,
    /// ANSI sequences work (virtual terminal processing is on).
    vt: bool,
    /// A status line is on screen and must be cleared before the next line.
    shown: bool,
    /// Length of the status line on screen, used to blank it when ANSI is unavailable.
    width: usize,
}

impl Console {
    fn clear(&mut self) {
        if !self.shown {
            return;
        }
        if self.vt {
            eprint!("\r\x1b[K");
        } else {
            eprint!("\r{:w$}\r", "", w = self.width);
        }
        self.shown = false;
        self.width = 0;
    }
}

fn console() -> MutexGuard<'static, Console> {
    CONSOLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Sets the output mode. Call once at startup, before anything is printed.
pub fn init(verbose: bool) {
    VERBOSE.store(verbose, SeqCst);
    let mut c = console();
    c.live = !verbose && std::io::stderr().is_terminal();
    c.vt = c.live && enable_virtual_terminal();
}

/// Whether `--verbose` was given.
pub fn verbose() -> bool {
    VERBOSE.load(SeqCst)
}

/// Prints one event line, after clearing the status line if one is on screen.
pub fn event(msg: &str) {
    let mut c = console();
    c.clear();
    eprintln!("{msg}");
}

/// Prints a diagnostic line, only in verbose mode.
pub fn debug(msg: &str) {
    if verbose() {
        event(msg);
    }
}

/// Redraws the status line in place. Does nothing unless stderr is a console in quiet mode.
pub fn status(line: &str) {
    let mut c = console();
    if !c.live {
        return;
    }
    if c.vt {
        eprint!("\r{line}\x1b[K");
    } else {
        let pad = c.width.saturating_sub(line.len());
        eprint!("\r{line}{:pad$}", "");
    }
    c.width = line.len();
    c.shown = true;
}

/// Removes the status line if one is on screen.
pub fn clear() {
    console().clear();
}

/// Turns on ANSI escape handling for the stderr console. Returns false when that is not possible,
/// in which case the status line is blanked with spaces instead.
fn enable_virtual_terminal() -> bool {
    // SAFETY: plain Win32 calls on this process's stderr handle, with a valid out-pointer.
    unsafe {
        let handle = GetStdHandle(STD_ERROR_HANDLE);
        if handle.is_null() || handle as usize == usize::MAX {
            return false;
        }
        let mut mode = 0u32;
        if GetConsoleMode(handle, &mut mode) == 0 {
            return false;
        }
        if mode & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0 {
            return true;
        }
        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

/// The quiet-mode status line.
///
/// `fps` is frames sent per second and `kbps` the stream bit rate. At or below [`IDLE_FPS_MAX`] the
/// screen is shown as static, and the iPad decode time is left out. `decode_ms` is shown only when
/// the iPad has reported it.
pub fn format_status(
    width: u32,
    height: u32,
    fps: f64,
    kbps: f64,
    decode_ms: Option<f64>,
) -> String {
    let mbps = format!("{:.1} Mbps", kbps / 1000.0);
    if fps <= IDLE_FPS_MAX {
        return format!("Streaming {width}x{height} | idle (static screen) | {mbps}");
    }
    match decode_ms {
        Some(ms) => {
            format!("Streaming {width}x{height} | {fps:.0} fps | {mbps} | iPad decode {ms:.0} ms")
        }
        None => format!("Streaming {width}x{height} | {fps:.0} fps | {mbps}"),
    }
}

/// The quiet-mode status line for one once-per-second tick of a stream of `size`.
pub fn streaming_line(tick: &Tick, size: (u32, u32)) -> String {
    let secs = tick.secs.max(f64::EPSILON);
    let fps = tick.sent as f64 / secs;
    let kbps = tick.bytes as f64 * 8.0 / 1000.0 / secs;
    format_status(size.0, size.1, fps, kbps, tick.ipad_decode_ms)
}

/// The once-per-second report line for a tick: in verbose mode the full stats line.
pub fn report_tick(tick: &Tick) {
    event(&pipeline::verbose_line(tick));
}

/// Quiet-mode line saying why the iPad session ended, in plain words.
pub fn disconnect_message(end: &SessionEnd) -> String {
    let reason = match end {
        SessionEnd::Bye => "the iPad app closed the connection".to_string(),
        SessionEnd::Closed => "the USB connection closed".to_string(),
        SessionEnd::Stopped => "stopped with Ctrl+C".to_string(),
        SessionEnd::Failed(e) => format!("connection problem ({e})"),
    };
    format!("iPad disconnected: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_line_with_ipad_decode_time() {
        assert_eq!(
            format_status(2266, 1488, 60.0, 18_400.0, Some(8.2)),
            "Streaming 2266x1488 | 60 fps | 18.4 Mbps | iPad decode 8 ms"
        );
    }

    #[test]
    fn streaming_line_without_ipad_decode_time() {
        assert_eq!(
            format_status(2266, 1488, 60.0, 18_400.0, None),
            "Streaming 2266x1488 | 60 fps | 18.4 Mbps"
        );
    }

    #[test]
    fn idle_line_omits_decode_time() {
        assert_eq!(
            format_status(2266, 1488, 5.0, 1_100.0, Some(3.0)),
            "Streaming 2266x1488 | idle (static screen) | 1.1 Mbps"
        );
    }

    #[test]
    fn idle_threshold_is_inclusive_at_eight_fps() {
        assert!(format_status(10, 10, 8.0, 0.0, None).contains("idle"));
        assert!(format_status(10, 10, 9.0, 0.0, None).contains("9 fps"));
    }

    #[test]
    fn streaming_line_derives_rates_from_the_tick() {
        let tick = Tick {
            secs: 2.0,
            encoded: 240,
            sent: 120,
            bytes: 4_600_000,
            enc_ms: 3.0,
            total_sent: 120,
            total_dropped: 0,
            fields: String::new(),
            ipad_decode_ms: Some(7.6),
        };
        assert_eq!(
            streaming_line(&tick, (2266, 1488)),
            "Streaming 2266x1488 | 60 fps | 18.4 Mbps | iPad decode 8 ms"
        );
        let idle = Tick {
            secs: 1.0,
            sent: 5,
            bytes: 137_500,
            ipad_decode_ms: None,
            ..tick
        };
        assert_eq!(
            streaming_line(&idle, (2266, 1488)),
            "Streaming 2266x1488 | idle (static screen) | 1.1 Mbps"
        );
    }

    #[test]
    fn disconnect_reasons_are_plain() {
        assert_eq!(
            disconnect_message(&SessionEnd::Bye),
            "iPad disconnected: the iPad app closed the connection"
        );
        assert_eq!(
            disconnect_message(&SessionEnd::Closed),
            "iPad disconnected: the USB connection closed"
        );
        assert_eq!(
            disconnect_message(&SessionEnd::Stopped),
            "iPad disconnected: stopped with Ctrl+C"
        );
        assert_eq!(
            disconnect_message(&SessionEnd::Failed("iPad stopped reading for 3 s".into())),
            "iPad disconnected: connection problem (iPad stopped reading for 3 s)"
        );
    }
}
