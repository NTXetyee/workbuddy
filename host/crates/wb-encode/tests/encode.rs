//! Hardware H.264 encode test on a real D3D11 device (no wb-capture dependency).

#[path = "common/mod.rs"]
mod common;

use std::time::{Duration, Instant};

use wb_encode::{EncodeError, EncodedFrame, Encoder, EncoderConfig};

const FRAME_US: u64 = 16_667;

fn starts_with_start_code(d: &[u8]) -> bool {
    d.starts_with(&[0, 0, 0, 1]) || d.starts_with(&[0, 0, 1])
}

#[test]
fn hardware_encode_120_frames_scaled() {
    let Some((device, ctx)) = common::create_device() else {
        panic!("no hardware D3D11 device available");
    };
    let cfg = EncoderConfig {
        width: 1280,
        height: 720,
        fps: 60,
        bitrate_kbps: 8000,
    };
    let mut enc = match Encoder::new(&device, cfg) {
        Ok(e) => e,
        Err(EncodeError::NoHardwareEncoder) => {
            eprintln!(
                "SKIP hardware_encode_120_frames_scaled: no hardware H.264 MFT on this machine"
            );
            return;
        }
        Err(e) => panic!("Encoder::new failed: {e}"),
    };
    let adapter = enc.adapter_name();
    println!("adapter: {adapter}");
    assert!(!adapter.is_empty(), "adapter name must not be empty");

    let mut src = common::Source::new(&device, 1920, 1080);
    let mut outs: Vec<EncodedFrame> = Vec::new();
    let mut total = Duration::ZERO;
    let frames = 120u32;
    let force_at = 60u32;
    for i in 0..frames {
        src.upload(&ctx, i);
        let pts = i as u64 * FRAME_US;
        let t0 = Instant::now();
        let out = enc
            .encode(src.texture(), pts, i == force_at)
            .unwrap_or_else(|e| panic!("encode({i}) failed: {e}"));
        total += t0.elapsed();
        if let Some(f) = out {
            outs.push(f);
        }
    }
    println!(
        "avg encode() call: {:.3} ms over {frames} frames, {} output frames",
        total.as_secs_f64() * 1000.0 / frames as f64,
        outs.len()
    );

    assert!(
        outs.len() >= 100,
        "expected >= 100 output frames, got {}",
        outs.len()
    );

    let first = &outs[0];
    assert!(first.keyframe, "first output frame must be a keyframe");
    assert!(first.has_params, "first output frame must carry SPS+PPS");

    for (n, f) in outs.iter().enumerate() {
        assert!(
            starts_with_start_code(&f.data),
            "output {n} is not Annex-B (starts {:02x?})",
            &f.data[..f.data.len().min(4)]
        );
        if f.keyframe {
            assert!(f.has_params, "keyframe {n} lacks SPS/PPS");
        }
    }
    for w in outs.windows(2) {
        assert!(
            w[1].pts_us > w[0].pts_us,
            "output pts must be strictly increasing"
        );
    }

    let p_force = force_at as u64 * FRAME_US;
    let idr_after_force = outs
        .iter()
        .find(|f| f.keyframe && f.pts_us >= p_force)
        .expect("no keyframe at or after the forced IDR");
    assert!(
        idr_after_force.pts_us <= p_force + 3 * FRAME_US,
        "forced keyframe came too late: pts {} vs forced at {}",
        idr_after_force.pts_us,
        p_force
    );
}
