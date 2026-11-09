//! `wb-encode`: hardware H.264 encoding of D3D11 BGRA textures into Annex-B access units.
//! Low latency. Windows only (the whole crate compiles to nothing elsewhere).
//!
//! Two backends:
//! - [`Backend::Nvenc`]: direct NVENC (`nvEncodeAPI64.dll`, no CUDA). The BGRA texture is
//!   registered as-is and NVENC does the colour conversion.
//! - [`Backend::Mft`]: Media Foundation hardware H.264 MFT fed by a D3D11 VideoProcessor
//!   (BGRA -> NV12 with scaling). Used on AMD and Intel, and as the fallback.
//!
//! [`Encoder::new`] picks NVENC for NVIDIA adapters and falls back to MFT. The environment
//! variable `WB_ENCODER=mft|nvenc` forces one backend.
#![cfg(windows)]

use std::error::Error as StdError;
use std::fmt;
use std::time::Duration;

pub use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

/// Whether diagnostic warnings are printed. Set by `WB_VERBOSE=1` (`wb-host --verbose` sets it).
pub(crate) fn verbose_diagnostics() -> bool {
    std::env::var_os("WB_VERBOSE").is_some_and(|v| v == "1")
}

/// Diagnostic warning on stderr, printed only in verbose mode. Errors and the NVENC fallback
/// notice use `eprintln!` directly.
macro_rules! log_diag {
    ($($arg:tt)*) => {
        if $crate::verbose_diagnostics() {
            eprintln!($($arg)*);
        }
    };
}

mod convert;
mod mft;
mod nvenc;
mod util;

use util::{adapter_info, validate, NVIDIA_VENDOR_ID};

/// Encoder configuration. `width`/`height` are the output (encoded) size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

/// One encoded access unit, Annex-B formatted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub has_params: bool,
    pub pts_us: u64,
}

/// Encoder counters since the last [`Encoder::take_stats`] call (measurement only).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct EncodeStats {
    /// Frames accepted by the encoder. The averages below divide by this.
    pub frames: u64,
    /// CPU time spent converting the input (MFT: VideoProcessorBlt submit; NVENC: 0 unless a
    /// resize/format conversion was needed).
    pub convert_total: Duration,
    /// Encoder call time: MFT from `ProcessInput` through the output wait; NVENC from
    /// `nvEncEncodePicture` through the bitstream copy.
    pub mft_total: Duration,
    /// Time spent waiting for METransformNeedInput (MFT only).
    pub input_wait_total: Duration,
    /// GPU time of the VideoProcessorBlt, from timestamp queries. Only samples whose results
    /// were ready are included, so this can lag the other totals by a frame or two.
    /// Caveat: on the development machine's GPU driver the begin and end timestamps came back
    /// equal or tens of nanoseconds apart, so this does NOT measure the Blt there. Treat it as
    /// unreliable until it is validated on another driver or with GPU tracing.
    pub gpu_convert_total: Duration,
    /// Number of GPU samples in `gpu_convert_total`.
    pub gpu_samples: u64,
}

/// Errors from the encoder. `Windows` carries the failing call name and the HRESULT (for NVENC
/// the HRESULT field holds the NVENC status code).
#[derive(Debug)]
pub enum EncodeError {
    Windows {
        what: &'static str,
        source: windows::core::Error,
    },
    NoHardwareEncoder,
    Config(String),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::Windows { what, source } => write!(f, "{what} failed: {source}"),
            EncodeError::NoHardwareEncoder => {
                write!(f, "no hardware H.264 encoder MFT found for this device")
            }
            EncodeError::Config(msg) => write!(f, "invalid encoder config: {msg}"),
        }
    }
}

impl StdError for EncodeError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            EncodeError::Windows { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Wraps a windows-crate result with the name of the failing call.
pub(crate) fn win<T>(what: &'static str, r: windows::core::Result<T>) -> Result<T, EncodeError> {
    r.map_err(|source| EncodeError::Windows { what, source })
}

/// Which hardware encoder is in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Direct NVENC on the NVIDIA driver (no CUDA).
    Nvenc,
    /// Media Foundation hardware H.264 MFT.
    Mft,
}

impl Backend {
    /// Short name as used by `WB_ENCODER`.
    pub fn name(self) -> &'static str {
        match self {
            Backend::Nvenc => "nvenc",
            Backend::Mft => "mft",
        }
    }
}

enum Inner {
    Nvenc(Box<nvenc::NvencEncoder>),
    Mft(Box<mft::MftEncoder>),
}

/// Hardware H.264 encoder for D3D11 BGRA textures (either backend).
pub struct Encoder {
    inner: Inner,
}

impl Encoder {
    /// Creates a hardware H.264 encoder on the same adapter as `device`.
    ///
    /// Selection: `WB_ENCODER=nvenc` or `WB_ENCODER=mft` forces that backend (NVENC failure is
    /// an error). Otherwise NVENC is used on NVIDIA adapters, falling back to MFT with one line
    /// on stderr when NVENC is unavailable, and MFT is used on every other adapter.
    pub fn new(device: &ID3D11Device, cfg: EncoderConfig) -> Result<Self, EncodeError> {
        let forced = std::env::var("WB_ENCODER").unwrap_or_default();
        match forced.trim().to_ascii_lowercase().as_str() {
            "" => Self::auto(device, cfg),
            "nvenc" => Self::new_with_backend(device, cfg, Backend::Nvenc),
            "mft" => Self::new_with_backend(device, cfg, Backend::Mft),
            other => Err(EncodeError::Config(format!(
                "WB_ENCODER must be 'nvenc' or 'mft', got '{other}'"
            ))),
        }
    }

    /// Creates an encoder with an explicit backend (no environment lookup).
    pub fn new_with_backend(
        device: &ID3D11Device,
        cfg: EncoderConfig,
        backend: Backend,
    ) -> Result<Self, EncodeError> {
        let inner = match backend {
            Backend::Nvenc => Inner::Nvenc(Box::new(nvenc::NvencEncoder::new(device, cfg)?)),
            Backend::Mft => Inner::Mft(Box::new(mft::MftEncoder::new(device, cfg)?)),
        };
        Ok(Encoder { inner })
    }

    fn auto(device: &ID3D11Device, cfg: EncoderConfig) -> Result<Self, EncodeError> {
        validate(&cfg)?;
        let info = adapter_info(device)?;
        if info.vendor == NVIDIA_VENDOR_ID {
            match Self::new_with_backend(device, cfg, Backend::Nvenc) {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    eprintln!(
                        "wb-encode: NVENC unavailable ({e}); falling back to Media Foundation"
                    )
                }
            }
        } else {
            log_diag!(
                "wb-encode: adapter vendor 0x{:04x} is not NVIDIA; using Media Foundation",
                info.vendor
            );
        }
        Self::new_with_backend(device, cfg, Backend::Mft)
    }

    /// The backend in use.
    pub fn backend(&self) -> Backend {
        match self.inner {
            Inner::Nvenc(_) => Backend::Nvenc,
            Inner::Mft(_) => Backend::Mft,
        }
    }

    /// Encodes one BGRA frame (any size; scaled to the configured size).
    ///
    /// The NVENC backend always returns the frame for this call. The MFT backend returns
    /// `Ok(None)` when no output is ready within its internal time budget.
    pub fn encode(
        &mut self,
        tex: &ID3D11Texture2D,
        pts_us: u64,
        force_idr: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError> {
        match &mut self.inner {
            Inner::Nvenc(e) => e.encode(tex, pts_us, force_idr),
            Inner::Mft(e) => e.encode(tex, pts_us, force_idr),
        }
    }

    /// Returns the counters accumulated since the previous call and resets them.
    pub fn take_stats(&mut self) -> EncodeStats {
        match &mut self.inner {
            Inner::Nvenc(e) => e.take_stats(),
            Inner::Mft(e) => e.take_stats(),
        }
    }

    /// Name of the DXGI adapter the encoder runs on (the device's adapter).
    pub fn adapter_name(&self) -> String {
        match &self.inner {
            Inner::Nvenc(e) => e.adapter_name(),
            Inner::Mft(e) => e.adapter_name(),
        }
    }
}
