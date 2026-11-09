//! `wb-host`: streams the PC's virtual display to the WorkBuddy iPad app over USB (SPEC 3.5).

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::thread;
use std::time::{Duration, Instant};

use wb_capture::OutputSel;
use wb_host::cli::{self, Args, Mode, OutputSpec};
use wb_host::net::{self, SessionEnd};
use wb_host::pipeline::{self, CaptureEncoder, EncodeOpts, WriteSink};
use wb_proto::Hello;
use wb_usbmux::Usbmux;

/// Set by the Ctrl+C handler.
static STOP: AtomicBool = AtomicBool::new(false);

const DEVICE_POLL_MS: u64 = 500;
const CONNECT_RETRY: Duration = Duration::from_millis(500);
const CONNECT_BUDGET: Duration = Duration::from_secs(30);
/// Static-screen repeat thresholds (SPEC 3.5): dump is less latency-sensitive than serving.
const REPEAT_DUMP: Duration = Duration::from_millis(250);
const REPEAT_SERVE: Duration = Duration::from_millis(100);

extern "system" {
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

unsafe extern "system" fn on_ctrl(_ctrl_type: u32) -> i32 {
    STOP.store(true, SeqCst);
    1
}

fn install_ctrl_handler() {
    // SAFETY: `on_ctrl` is a plain extern fn that only touches a static atomic.
    unsafe {
        SetConsoleCtrlHandler(Some(on_ctrl), 1);
    }
}

fn main() {
    let code = match cli::parse(std::env::args().skip(1)) {
        Ok(args) if args.mode == Mode::Help => {
            println!("{}", cli::USAGE);
            0
        }
        Ok(args) => {
            install_ctrl_handler();
            match &args.mode {
                Mode::ListOutputs => list_outputs(),
                Mode::Dump(path) => dump(&args, path),
                Mode::Serve => serve(&args),
                Mode::Help => 0,
            }
        }
        Err(msg) => {
            eprintln!("wb-host: {msg}\n{}", cli::USAGE);
            2
        }
    };
    std::process::exit(code);
}

fn selector(spec: &OutputSpec) -> OutputSel {
    match spec {
        OutputSpec::Primary => OutputSel::Primary,
        OutputSpec::Index(i) => OutputSel::Index(*i),
        OutputSpec::NameContains(s) => OutputSel::NameContains(s.clone()),
    }
}

fn encode_opts(args: &Args, repeat_after: Duration) -> EncodeOpts {
    EncodeOpts {
        sel: selector(&args.output),
        fps: args.fps,
        bitrate_kbps: args.bitrate_kbps,
        size: args.size,
        repeat_after,
        idle_fps: args.idle_fps,
    }
}

fn list_outputs() -> i32 {
    match wb_capture::list_outputs() {
        Ok(list) => {
            println!(
                "{:<4} {:<34} {:<18} {:>11}  ATTACHED",
                "IDX", "ADAPTER", "OUTPUT", "SIZE"
            );
            for o in list {
                println!(
                    "{:<4} {:<34} {:<18} {:>11}  {}",
                    o.index,
                    o.adapter_name,
                    o.device_name,
                    format!("{}x{}", o.width, o.height),
                    if o.attached { "yes" } else { "no" }
                );
            }
            0
        }
        Err(e) => {
            eprintln!("wb-host: {e}");
            1
        }
    }
}

/// `--dump`: capture for `--seconds`, write the Annex-B stream to `path`. No device needed.
fn dump(args: &Args, path: &Path) -> i32 {
    let mut src = match CaptureEncoder::new(encode_opts(args, REPEAT_DUMP)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wb-host: {e}");
            return 1;
        }
    };
    eprintln!("{}", src.describe());
    let file = match File::create(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("wb-host: cannot create {}: {e}", path.display());
            return 1;
        }
    };
    let mut sink = WriteSink::new(BufWriter::new(file));
    // Only the first frame is forced to an IDR in dump mode.
    let force = AtomicBool::new(true);
    let (w, h) = src.size();
    eprintln!(
        "[dump] {}x{} @ {} fps, {} kbps, {} s -> {}",
        w,
        h,
        args.fps,
        args.bitrate_kbps,
        args.seconds,
        path.display()
    );
    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.seconds);
    let mut should_stop = || Instant::now() >= deadline || STOP.load(SeqCst);
    let summary =
        match pipeline::run_loop(&mut src, &mut sink, &force, &mut should_stop, &String::new) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("wb-host: {e}");
                return 1;
            }
        };
    if let Err(e) = sink.finish() {
        eprintln!("wb-host: {e}");
        return 1;
    }
    eprintln!(
        "[dump] done: {} frames ({} keyframes), {} bytes, {:.2} s, avg encode {:.2} ms",
        summary.produced,
        summary.keyframes,
        summary.bytes,
        started.elapsed().as_secs_f64(),
        summary.avg_encode_ms()
    );
    if summary.produced == 0 {
        eprintln!("[dump] warning: no frames were captured");
    }
    0
}

/// Default mode: serve the iPad until Ctrl+C. Waits for a device, connects, streams, and
/// starts over when the session ends.
fn serve(args: &Args) -> i32 {
    let mux = Usbmux::new();
    let mut waiting = false;
    while !STOP.load(SeqCst) {
        if !waiting {
            eprintln!("waiting for iPad on USB...");
            waiting = true;
        }
        let device = match mux.wait_for_device(DEVICE_POLL_MS, &STOP) {
            Ok(Some(d)) => d,
            Ok(None) => break,
            Err(e) => {
                eprintln!("[usb] {e}; retrying");
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        waiting = false;
        eprintln!("[usb] device {} ({}) found", device.id, device.serial);

        let Some(stream) = connect_with_retry(&mux, device.id, args.port) else {
            continue;
        };
        let mut src = match CaptureEncoder::new(encode_opts(args, REPEAT_SERVE)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[capture] {e}; retrying");
                drop(stream);
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        eprintln!("{}", src.describe());
        let (cfg, size) = (src.config(), src.size());
        let end = net::run_session(
            stream,
            move |hello| {
                check_aspect(hello, size);
                cfg
            },
            &mut src,
            &STOP,
        );
        eprintln!("[session] ended: {end:?}");
        drop(src);
        if matches!(end, SessionEnd::Failed(_)) {
            thread::sleep(Duration::from_millis(500));
        }
    }
    0
}

fn connect_with_retry(mux: &Usbmux, device_id: u32, port: u16) -> Option<std::net::TcpStream> {
    let started = Instant::now();
    let mut attempt = 0u32;
    while !STOP.load(SeqCst) && started.elapsed() < CONNECT_BUDGET {
        attempt += 1;
        match mux.connect(device_id, port) {
            Ok(stream) => {
                eprintln!("[usb] connected to port {port} after {attempt} attempt(s)");
                return Some(stream);
            }
            Err(e) => {
                if attempt == 1 || attempt.is_multiple_of(10) {
                    eprintln!("[usb] connect attempt {attempt} failed: {e}");
                }
            }
        }
        thread::sleep(CONNECT_RETRY);
    }
    if !STOP.load(SeqCst) {
        eprintln!("[usb] iPad app not listening after 30 s; waiting for the device again");
    }
    None
}

/// Logs the iPad's native size and warns when the stream aspect differs. The stream is never
/// rescaled here, so a mismatch just means letterboxing on the iPad.
fn check_aspect(hello: &Hello, size: (u32, u32)) {
    eprintln!(
        "[net] iPad '{}' native {}x{}, max {} fps",
        hello.name, hello.native_width, hello.native_height, hello.max_fps
    );
    let (nw, nh) = (
        u64::from(hello.native_width),
        u64::from(hello.native_height),
    );
    let (w, h) = (u64::from(size.0), u64::from(size.1));
    if nw == 0 || nh == 0 || w == 0 || h == 0 {
        return;
    }
    // Compare w/h with nw/nh by cross-multiplying, tolerating 1 percent.
    let a = w * nh;
    let b = nw * h;
    if a.abs_diff(b) * 100 > a.max(b) {
        eprintln!(
            "[net] warning: stream {w}x{h} differs in aspect from iPad native {nw}x{nh}; not rescaling"
        );
    }
}
