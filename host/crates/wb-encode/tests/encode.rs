//! Hardware H.264 encode tests on a real D3D11 device (no wb-capture dependency).
//!
//! The same 120-frame run is made with the default selection and with each backend forced
//! through `Encoder::new_with_backend`, so both paths are checked on every run.

#[path = "common/mod.rs"]
mod common;

use std::time::{Duration, Instant};

use wb_encode::{Backend, EncodeError, EncodedFrame, Encoder, EncoderConfig};

const FRAME_US: u64 = 16_667;
const FRAMES: u32 = 120;
const FORCE_AT: u32 = 60;

fn starts_with_start_code(d: &[u8]) -> bool {
    d.starts_with(&[0, 0, 0, 1]) || d.starts_with(&[0, 0, 1])
}

fn cfg_720p() -> EncoderConfig {
    EncoderConfig {
        width: 1280,
        height: 720,
        fps: 60,
        bitrate_kbps: 8000,
    }
}

/// Encodes 120 frames from a 1920x1080 source into 1280x720 (scaled), forcing an IDR at frame
/// 60, checks the stream, and returns the average `encode()` call time in ms.
fn encode_and_check(
    device: &wb_encode::ID3D11Device,
    ctx: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    enc: &mut Encoder,
    label: &str,
) -> f64 {
    let mut src = common::Source::new(device, 1920, 1080);
    let mut outs: Vec<EncodedFrame> = Vec::new();
    let mut total = Duration::ZERO;
    for i in 0..FRAMES {
        src.upload(ctx, i);
        let pts = i as u64 * FRAME_US;
        let t0 = Instant::now();
        let out = enc
            .encode(src.texture(), pts, i == FORCE_AT)
            .unwrap_or_else(|e| panic!("[{label}] encode({i}) failed: {e}"));
        total += t0.elapsed();
        if let Some(f) = out {
            outs.push(f);
        }
    }
    let avg_ms = total.as_secs_f64() * 1000.0 / FRAMES as f64;
    println!(
        "[{label}] adapter: {}, backend: {}",
        enc.adapter_name(),
        enc.backend().name()
    );
    println!(
        "[{label}] avg encode() call: {avg_ms:.3} ms over {FRAMES} frames, {} output frames",
        outs.len()
    );

    assert!(
        outs.len() >= 100,
        "[{label}] expected >= 100 output frames, got {}",
        outs.len()
    );

    let first = &outs[0];
    assert!(
        first.keyframe,
        "[{label}] first output frame must be a keyframe"
    );
    assert!(
        first.has_params,
        "[{label}] first output frame must carry SPS+PPS"
    );

    for (n, f) in outs.iter().enumerate() {
        assert!(
            starts_with_start_code(&f.data),
            "[{label}] output {n} is not Annex-B (starts {:02x?})",
            &f.data[..f.data.len().min(4)]
        );
        if f.keyframe {
            assert!(f.has_params, "[{label}] keyframe {n} lacks SPS/PPS");
        }
    }
    for w in outs.windows(2) {
        assert!(
            w[1].pts_us > w[0].pts_us,
            "[{label}] output pts must be strictly increasing"
        );
    }

    let p_force = FORCE_AT as u64 * FRAME_US;
    let idr_after_force = outs
        .iter()
        .find(|f| f.keyframe && f.pts_us >= p_force)
        .unwrap_or_else(|| panic!("[{label}] no keyframe at or after the forced IDR"));
    assert!(
        idr_after_force.pts_us <= p_force + 3 * FRAME_US,
        "[{label}] forced keyframe came too late: pts {} vs forced at {}",
        idr_after_force.pts_us,
        p_force
    );
    avg_ms
}

#[test]
fn default_selection_120_frames_scaled() {
    let Some((device, ctx)) = common::create_device() else {
        panic!("no hardware D3D11 device available");
    };
    let mut enc = match Encoder::new(&device, cfg_720p()) {
        Ok(e) => e,
        Err(EncodeError::NoHardwareEncoder) => {
            eprintln!("SKIP default_selection_120_frames_scaled: no hardware H.264 encoder");
            return;
        }
        Err(e) => panic!("Encoder::new failed: {e}"),
    };
    assert!(
        !enc.adapter_name().is_empty(),
        "adapter name must not be empty"
    );
    let backend = enc.backend();
    assert!(
        matches!(backend, Backend::Nvenc | Backend::Mft),
        "unexpected backend"
    );
    encode_and_check(&device, &ctx, &mut enc, "default");
}

#[test]
fn mft_backend_120_frames_scaled() {
    let Some((device, ctx)) = common::create_device() else {
        panic!("no hardware D3D11 device available");
    };
    let mut enc = match Encoder::new_with_backend(&device, cfg_720p(), Backend::Mft) {
        Ok(e) => e,
        Err(EncodeError::NoHardwareEncoder) => {
            eprintln!("SKIP mft_backend_120_frames_scaled: no hardware H.264 MFT");
            return;
        }
        Err(e) => panic!("Encoder::new_with_backend(Mft) failed: {e}"),
    };
    encode_and_check(&device, &ctx, &mut enc, "mft");
}

#[test]
fn nvenc_backend_120_frames_scaled() {
    let Some((device, ctx)) = common::create_device() else {
        panic!("no hardware D3D11 device available");
    };
    let mut enc = Encoder::new_with_backend(&device, cfg_720p(), Backend::Nvenc)
        .unwrap_or_else(|e| panic!("Encoder::new_with_backend(Nvenc) failed: {e}"));
    assert_eq!(enc.backend(), Backend::Nvenc);
    encode_and_check(&device, &ctx, &mut enc, "nvenc");
}
