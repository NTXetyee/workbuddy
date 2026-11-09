//! Encodes a synthetic animated 2266x1488 source at 60 fps / 20000 kbps and writes Annex-B.
//! Usage: cargo run -p wb-encode --example dump --release -- <out.h264> [seconds]

#[path = "../tests/common/mod.rs"]
mod common;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};

use wb_encode::{Encoder, EncoderConfig};

const W: u32 = 2266;
const H: u32 = 1488;
const FPS: u32 = 60;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(out_path) = args.get(1) else {
        eprintln!("usage: dump <out.h264> [seconds]");
        std::process::exit(2);
    };
    let secs: u32 = match args.get(2) {
        Some(s) => s.parse().expect("seconds must be an integer"),
        None => 5,
    };

    let (device, ctx) = common::create_device().expect("no hardware D3D11 device");
    let cfg = EncoderConfig {
        width: W,
        height: H,
        fps: FPS,
        bitrate_kbps: 20_000,
    };
    // Honours WB_ENCODER=nvenc|mft; unset selects NVENC on NVIDIA and MFT otherwise.
    let mut enc = Encoder::new(&device, cfg).expect("Encoder::new");
    println!(
        "adapter: {}, backend: {}",
        enc.adapter_name(),
        enc.backend().name()
    );

    let mut src = common::Source::new(&device, W, H);
    let mut file = BufWriter::new(File::create(out_path).expect("create output file"));
    let frames = secs * FPS;
    let mut total = Duration::ZERO;
    let mut written = 0u32;
    let mut keyframes = 0u32;
    for i in 0..frames {
        src.upload(&ctx, i);
        let pts = i as u64 * 1_000_000 / FPS as u64;
        let t0 = Instant::now();
        let out = enc.encode(src.texture(), pts, false).expect("encode");
        total += t0.elapsed();
        if let Some(f) = out {
            if f.keyframe {
                keyframes += 1;
            }
            file.write_all(&f.data).expect("write");
            written += 1;
        }
    }
    file.flush().expect("flush");
    println!(
        "encoded {frames} input frames -> {written} output frames ({keyframes} keyframes) into {out_path}"
    );
    println!(
        "avg encode() call: {:.3} ms",
        total.as_secs_f64() * 1000.0 / frames as f64
    );
}
