//! Usage: cargo run -p wb-capture --example bench --release -- [output_index|primary] [seconds]

use std::time::{Duration, Instant};

use wb_capture::{list_outputs, Capturer, OutputSel};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sel = match args.first().map(String::as_str) {
        None | Some("primary") => OutputSel::Primary,
        Some(s) => OutputSel::Index(
            s.parse()
                .expect("output_index must be a number or 'primary'"),
        ),
    };
    let secs: f64 = args
        .get(1)
        .map(|s| s.parse().expect("seconds must be a number"))
        .unwrap_or(3.0);

    println!("outputs:");
    for o in list_outputs().expect("list_outputs failed") {
        println!(
            "  [{}] {} / {} {}x{} attached={}",
            o.index, o.adapter_name, o.device_name, o.width, o.height, o.attached
        );
    }

    let mut cap = Capturer::new(sel).expect("Capturer::new failed");
    let (w, h) = cap.size();
    println!("capturing {w}x{h} for {secs} s");

    let start = Instant::now();
    let end = start + Duration::from_secs_f64(secs);
    let mut calls = 0u64;
    let mut frames = 0u64;
    let mut call_time = Duration::ZERO;
    let mut frame_call_time = Duration::ZERO;
    while Instant::now() < end {
        let t = Instant::now();
        let got = cap.next_frame(100).expect("next_frame failed");
        let dt = t.elapsed();
        calls += 1;
        call_time += dt;
        if got.is_some() {
            frames += 1;
            frame_call_time += dt;
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    println!(
        "frames: {frames} in {elapsed:.2} s = {:.1} fps",
        frames as f64 / elapsed
    );
    println!(
        "next_frame: {calls} calls, avg {:.3} ms/call, avg {:.3} ms/returned frame",
        call_time.as_secs_f64() * 1000.0 / calls.max(1) as f64,
        frame_call_time.as_secs_f64() * 1000.0 / frames.max(1) as f64
    );
}
