//! `wb-encode`: D3D11 VideoProcessor (BGRA -> NV12, with scaling) feeding a
//! hardware H.264 Media Foundation transform. Low latency, Annex-B output.
//! Windows only (the whole crate compiles to nothing elsewhere).
#![cfg(windows)]

use std::collections::VecDeque;
use std::error::Error as StdError;
use std::fmt;
use std::mem::ManuallyDrop;
use std::ptr;
use std::thread::sleep;
use std::time::{Duration, Instant};

pub use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

use windows::core::{IUnknown, Interface, GUID};
use windows::Win32::Foundation::{RECT, TRUE, VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11DeviceContext, ID3D11Multithread, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

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

/// Errors from the encoder. `Windows` carries the failing call name and the HRESULT.
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

const RING_LEN: usize = 3;
const INPUT_WAIT: Duration = Duration::from_millis(50);
const OUTPUT_WAIT: Duration = Duration::from_millis(50);
const DRAIN_WAIT: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(1);
const START_CODE: [u8; 4] = [0, 0, 0, 1];
const NV12_FOURCC: u32 = u32::from_le_bytes(*b"NV12");
const MAX_INPUT_VIEWS: usize = 8;

// Event type values (MediaEventType) used by the transform.
const EVT_NEED_INPUT: u32 = 601;
const EVT_HAVE_OUTPUT: u32 = 602;
const EVT_DRAIN_COMPLETE: u32 = 604;

fn win<T>(what: &'static str, r: windows::core::Result<T>) -> Result<T, EncodeError> {
    r.map_err(|source| EncodeError::Windows { what, source })
}

fn pack2(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

/// Keeps MF started for the lifetime of an encoder. Declared last in `Encoder`
/// so that every MF/COM object is released before `MFShutdown`.
struct MfGuard;

impl MfGuard {
    fn start() -> Result<Self, EncodeError> {
        // Harmless if COM is already initialised on this thread (RPC_E_CHANGED_MODE).
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        win("MFStartup", unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL)
        })?;
        Ok(MfGuard)
    }
}

impl Drop for MfGuard {
    fn drop(&mut self) {
        let _ = unsafe { MFShutdown() };
    }
}

/// Hardware H.264 encoder fed by a D3D11 VideoProcessor (BGRA -> NV12 + scale).
///
/// Field order matters: MF objects are declared before `_mf` so that they are
/// released before `MFShutdown` runs.
pub struct Encoder {
    cfg: EncoderConfig,
    adapter: String,
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    conv: Converter,
    in_id: u32,
    out_id: u32,
    out_size: u32,
    provides_samples: bool,
    need_input: u32,
    drained: bool,
    pts_queue: VecDeque<u64>,
    out_queue: VecDeque<EncodedFrame>,
    /// Annex-B SPS+PPS, taken from MF_MT_MPEG_SEQUENCE_HEADER.
    params: Vec<u8>,
    /// NAL length size used when the transform emits AVCC.
    nal_len: usize,
    frame_dur_100ns: i64,
    /// Kept alive for the transform's lifetime (the transform references it).
    _mgr: IMFDXGIDeviceManager,
    _mf: MfGuard,
}

impl Encoder {
    /// Creates a hardware H.264 encoder on the same adapter as `device`.
    pub fn new(device: &ID3D11Device, cfg: EncoderConfig) -> Result<Self, EncodeError> {
        validate(&cfg)?;
        let mf = MfGuard::start()?;

        // The DXGI device manager requires a multithread-protected device.
        let mt: ID3D11Multithread = win("QI ID3D11Multithread", device.cast())?;
        let _ = unsafe { mt.SetMultithreadProtected(true) };

        let dxgi: IDXGIDevice = win("QI IDXGIDevice", device.cast())?;
        let adapter_obj = win("IDXGIDevice::GetAdapter", unsafe { dxgi.GetAdapter() })?;
        let desc = win("IDXGIAdapter::GetDesc", unsafe { adapter_obj.GetDesc() })?;
        let adapter = wide_to_string(&desc.Description);
        let luid = pack2(desc.AdapterLuid.HighPart as u32, desc.AdapterLuid.LowPart);

        let mut token = 0u32;
        let mut mgr_opt: Option<IMFDXGIDeviceManager> = None;
        win("MFCreateDXGIDeviceManager", unsafe {
            MFCreateDXGIDeviceManager(&mut token, &mut mgr_opt)
        })?;
        let mgr = mgr_opt.ok_or_else(|| EncodeError::Config("no device manager".into()))?;
        win("IMFDXGIDeviceManager::ResetDevice", unsafe {
            mgr.ResetDevice(device, token)
        })?;

        let (mft, in_id, out_id) = create_transform(luid, &mgr, &cfg)?;

        // Settings via ICodecAPI; individual failures are non-fatal.
        let codec: Option<ICodecAPI> = mft.cast().ok();
        if let Some(api) = &codec {
            apply_codec_settings(api, &cfg);
        } else {
            eprintln!("wb-encode: transform exposes no ICodecAPI; using defaults");
        }
        let start_stream = || -> windows::core::Result<()> {
            unsafe {
                mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
                mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
                mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
            }
        };
        win("MFT streaming start", start_stream())?;

        let events: IMFMediaEventGenerator = win("QI IMFMediaEventGenerator", mft.cast())?;
        let out_info = win("GetOutputStreamInfo", unsafe {
            mft.GetOutputStreamInfo(out_id)
        })?;
        let provides_samples =
            out_info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
        let out_size = out_info.cbSize.max(cfg.width * cfg.height);

        let conv = Converter::new(device, &cfg)?;
        let mut enc = Encoder {
            cfg,
            adapter,
            mft,
            events,
            codec,
            conv,
            in_id,
            out_id,
            out_size,
            provides_samples,
            need_input: 0,
            drained: false,
            pts_queue: VecDeque::new(),
            out_queue: VecDeque::new(),
            params: Vec::new(),
            nal_len: 4,
            frame_dur_100ns: 10_000_000 / cfg.fps as i64,
            _mgr: mgr,
            _mf: mf,
        };
        let _ = enc.refresh_params();
        Ok(enc)
    }

    /// Encodes one BGRA frame (any size; scaled to the configured size).
    /// Returns `Ok(None)` when no output is ready within the internal time budget.
    pub fn encode(
        &mut self,
        tex: &ID3D11Texture2D,
        pts_us: u64,
        force_idr: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError> {
        if force_idr {
            self.force_idr();
        }
        if !self.pump(INPUT_WAIT, |s| s.need_input > 0)? {
            return Ok(self.out_queue.pop_front());
        }

        let nv12 = self.conv.convert(tex)?;
        let sample = make_sample(
            &nv12,
            self.cfg.width,
            self.cfg.height,
            pts_us,
            self.frame_dur_100ns,
        )?;
        let r = unsafe { self.mft.ProcessInput(self.in_id, &sample, 0) };
        match r {
            Ok(()) => {}
            Err(source) if source.code() == MF_E_NOTACCEPTING => {
                self.need_input = 0;
                return Ok(self.out_queue.pop_front());
            }
            Err(source) => {
                return Err(EncodeError::Windows {
                    what: "IMFTransform::ProcessInput",
                    source,
                })
            }
        }
        self.need_input = self.need_input.saturating_sub(1);
        self.pts_queue.push_back(pts_us);

        self.pump(OUTPUT_WAIT, |s| !s.out_queue.is_empty())?;
        Ok(self.out_queue.pop_front())
    }

    /// Name of the DXGI adapter the encoder runs on (the device's adapter).
    pub fn adapter_name(&self) -> String {
        self.adapter.clone()
    }

    fn force_idr(&self) {
        if let Some(api) = &self.codec {
            let v = variant_u32(1);
            if let Err(e) = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &v) } {
                eprintln!("wb-encode: force keyframe failed: {e}");
            }
        }
    }

    /// Processes events until `done` holds or `budget` elapses. Returns whether `done` held.
    fn pump(
        &mut self,
        budget: Duration,
        done: impl Fn(&Encoder) -> bool,
    ) -> Result<bool, EncodeError> {
        let deadline = Instant::now() + budget;
        loop {
            if done(self) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(done(self));
            }
            match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => {
                    let ty = win("IMFMediaEvent::GetType", unsafe { ev.GetType() })?;
                    if ty == EVT_NEED_INPUT {
                        self.need_input += 1;
                    } else if ty == EVT_HAVE_OUTPUT {
                        self.collect_output()?;
                    } else if ty == EVT_DRAIN_COMPLETE {
                        self.drained = true;
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => sleep(POLL),
                Err(source) => {
                    return Err(EncodeError::Windows {
                        what: "IMFMediaEventGenerator::GetEvent",
                        source,
                    })
                }
            }
        }
    }

    /// One ProcessOutput call (answers one HaveOutput event).
    fn collect_output(&mut self) -> Result<(), EncodeError> {
        for _ in 0..2 {
            let mut buf = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.out_id,
                pSample: ManuallyDrop::new(None),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            if !self.provides_samples {
                let s = win("MFCreateSample", unsafe { MFCreateSample() })?;
                let b = win("MFCreateMemoryBuffer", unsafe {
                    MFCreateMemoryBuffer(self.out_size)
                })?;
                win("IMFSample::AddBuffer", unsafe { s.AddBuffer(&b) })?;
                buf.pSample = ManuallyDrop::new(Some(s));
            }
            let mut status = 0u32;
            let r = unsafe {
                self.mft
                    .ProcessOutput(0, std::slice::from_mut(&mut buf), &mut status)
            };
            let sample = unsafe { ManuallyDrop::take(&mut buf.pSample) };
            drop(unsafe { ManuallyDrop::take(&mut buf.pEvents) });
            match r {
                Ok(()) => {
                    if let Some(s) = sample {
                        self.consume(&s)?;
                    }
                    return Ok(());
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let _ = self.refresh_params();
                    continue;
                }
                Err(source) => {
                    return Err(EncodeError::Windows {
                        what: "IMFTransform::ProcessOutput",
                        source,
                    })
                }
            }
        }
        Ok(())
    }

    /// Converts an output sample into an `EncodedFrame` and queues it.
    fn consume(&mut self, sample: &IMFSample) -> Result<(), EncodeError> {
        let from_sample = unsafe { sample.GetSampleTime() }
            .ok()
            .map(|t| (t / 10).max(0) as u64);
        let from_queue = self.pts_queue.pop_front();
        let pts_us = from_sample.or(from_queue).unwrap_or(0);

        let buf = win("IMFSample::ConvertToContiguousBuffer", unsafe {
            sample.ConvertToContiguousBuffer()
        })?;
        let mut ptr_out: *mut u8 = ptr::null_mut();
        let mut cur = 0u32;
        win("IMFMediaBuffer::Lock", unsafe {
            buf.Lock(&mut ptr_out, None, Some(&mut cur))
        })?;
        let raw = if cur == 0 || ptr_out.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(ptr_out, cur as usize) }.to_vec()
        };
        let _ = unsafe { buf.Unlock() };
        if raw.is_empty() {
            return Ok(());
        }

        let mut data = if raw.starts_with(&START_CODE) || raw.starts_with(&[0, 0, 1]) {
            raw
        } else {
            avcc_to_annexb(&raw, self.nal_len)
        };
        let types = nal_types(&data);
        let idr = types.contains(&5);
        let mut has_params = types.contains(&7) && types.contains(&8);
        if idr && !has_params {
            if self.params.is_empty() {
                let _ = self.refresh_params();
            }
            if !self.params.is_empty() {
                let mut v = self.params.clone();
                v.append(&mut data);
                data = v;
                has_params = true;
            }
        }
        self.out_queue.push_back(EncodedFrame {
            data,
            keyframe: idr,
            has_params,
            pts_us,
        });
        Ok(())
    }

    /// Re-reads SPS/PPS from the transform's current output type.
    fn refresh_params(&mut self) -> Result<(), EncodeError> {
        let t = win("GetOutputCurrentType", unsafe {
            self.mft.GetOutputCurrentType(self.out_id)
        })?;
        let n = match unsafe { t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) } {
            Ok(n) if n > 0 => n,
            _ => return Ok(()),
        };
        let mut blob = vec![0u8; n as usize];
        win("GetBlob(MF_MT_MPEG_SEQUENCE_HEADER)", unsafe {
            t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)
        })?;
        let (params, nal_len) = parse_sequence_header(&blob);
        if !params.is_empty() {
            self.params = params;
            self.nal_len = nal_len;
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }
        // Best effort: flush whatever the transform still holds.
        let _ = self.pump(DRAIN_WAIT, |s| s.drained);
        self.out_queue.clear();
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

fn validate(cfg: &EncoderConfig) -> Result<(), EncodeError> {
    if cfg.width == 0 || cfg.height == 0 || cfg.fps == 0 || cfg.bitrate_kbps == 0 {
        return Err(EncodeError::Config("all fields must be non-zero".into()));
    }
    if !cfg.width.is_multiple_of(2) || !cfg.height.is_multiple_of(2) {
        return Err(EncodeError::Config(
            "NV12 needs even width and height".into(),
        ));
    }
    Ok(())
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// Enumerates hardware H.264 encoders, preferring those bound to `luid`.
/// Returns the first candidate whose transform accepts our media types.
fn create_transform(
    luid: u64,
    mgr: &IMFDXGIDeviceManager,
    cfg: &EncoderConfig,
) -> Result<(IMFTransform, u32, u32), EncodeError> {
    let activates = enum_encoders(luid)?;
    if activates.is_empty() {
        return Err(EncodeError::NoHardwareEncoder);
    }
    let mut last = EncodeError::NoHardwareEncoder;
    for act in activates {
        match setup_transform(&act, mgr, cfg) {
            Ok(v) => return Ok(v),
            Err(e) => {
                eprintln!("wb-encode: candidate encoder rejected: {e}");
                last = e;
            }
        }
    }
    Err(last)
}

fn enum_encoders(luid: u64) -> Result<Vec<IMFActivate>, EncodeError> {
    let out_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;

    let attrs = {
        let mut a: Option<IMFAttributes> = None;
        win("MFCreateAttributes", unsafe {
            MFCreateAttributes(&mut a, 1)
        })?;
        a.ok_or_else(|| EncodeError::Config("MFCreateAttributes returned nothing".into()))?
    };
    win("IMFAttributes::SetUINT64(MFT_ENUM_ADAPTER_LUID)", unsafe {
        attrs.SetUINT64(&MFT_ENUM_ADAPTER_LUID, luid)
    })?;

    let mut list = take_activates(|pp, n| unsafe {
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&out_type),
            &attrs,
            pp,
            n,
        )
    })?;
    if list.is_empty() {
        list = take_activates(|pp, n| unsafe {
            MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                None,
                Some(&out_type),
                pp,
                n,
            )
        })?;
    }
    Ok(list)
}

/// Runs an MFTEnum-style call that returns a CoTaskMem array of activates.
fn take_activates(
    f: impl FnOnce(*mut *mut Option<IMFActivate>, *mut u32) -> windows::core::Result<()>,
) -> Result<Vec<IMFActivate>, EncodeError> {
    let mut arr: *mut Option<IMFActivate> = ptr::null_mut();
    let mut n = 0u32;
    win("MFTEnum", f(&mut arr, &mut n))?;
    let mut out = Vec::new();
    if !arr.is_null() {
        let slice = unsafe { std::slice::from_raw_parts_mut(arr, n as usize) };
        for slot in slice.iter_mut() {
            if let Some(a) = slot.take() {
                out.push(a);
            }
        }
        unsafe { CoTaskMemFree(Some(arr as *const _)) };
    }
    Ok(out)
}

fn setup_transform(
    act: &IMFActivate,
    mgr: &IMFDXGIDeviceManager,
    cfg: &EncoderConfig,
) -> Result<(IMFTransform, u32, u32), EncodeError> {
    let mft: IMFTransform = win("IMFActivate::ActivateObject", unsafe {
        act.ActivateObject()
    })?;
    let attrs = win("IMFTransform::GetAttributes", unsafe {
        mft.GetAttributes()
    })?;
    win("MF_TRANSFORM_ASYNC_UNLOCK", unsafe {
        attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
    })?;
    if let Err(e) = unsafe { attrs.SetUINT32(&MF_LOW_LATENCY, 1) } {
        eprintln!("wb-encode: MF_LOW_LATENCY not accepted: {e}");
    }

    let mgr_ptr = mgr.as_raw() as usize;
    win("MFT_MESSAGE_SET_D3D_MANAGER", unsafe {
        mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr_ptr)
    })?;

    // Fixed-stream transforms return E_NOTIMPL here; stream 0 is then correct.
    let (mut ins, mut outs) = ([0u32], [0u32]);
    let (in_id, out_id) = match unsafe { mft.GetStreamIDs(&mut ins, &mut outs) } {
        Ok(()) => (ins[0], outs[0]),
        Err(_) => (0, 0),
    };

    // Output first (H.264 Main, falling back to Constrained/Base), then input NV12.
    let profiles = [
        eAVEncH264VProfile_Main.0 as u32,
        eAVEncH264VProfile_Base.0 as u32,
    ];
    let mut set_out = Err(EncodeError::Config("no output profile accepted".into()));
    for profile in profiles {
        let t = make_output_type(cfg, profile)?;
        match unsafe { mft.SetOutputType(out_id, &t, 0) } {
            Ok(()) => {
                set_out = Ok(());
                break;
            }
            Err(source) => {
                set_out = Err(EncodeError::Windows {
                    what: "IMFTransform::SetOutputType(H264)",
                    source,
                })
            }
        }
    }
    set_out?;

    let in_type = make_input_type(cfg)?;
    win("IMFTransform::SetInputType(NV12)", unsafe {
        mft.SetInputType(in_id, &in_type, 0)
    })?;
    Ok((mft, in_id, out_id))
}

fn make_output_type(cfg: &EncoderConfig, profile: u32) -> Result<IMFMediaType, EncodeError> {
    let t = win("MFCreateMediaType", unsafe { MFCreateMediaType() })?;
    let set = || -> windows::core::Result<()> {
        unsafe {
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            t.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_kbps.saturating_mul(1000))?;
            t.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)?;
            t.SetUINT64(&MF_MT_FRAME_SIZE, pack2(cfg.width, cfg.height))?;
            t.SetUINT64(&MF_MT_FRAME_RATE, pack2(cfg.fps, 1))?;
            t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1))?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        }
        Ok(())
    };
    win("output media type", set())?;
    Ok(t)
}

fn make_input_type(cfg: &EncoderConfig) -> Result<IMFMediaType, EncodeError> {
    let t = win("MFCreateMediaType", unsafe { MFCreateMediaType() })?;
    let set = || -> windows::core::Result<()> {
        unsafe {
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            t.SetUINT64(&MF_MT_FRAME_SIZE, pack2(cfg.width, cfg.height))?;
            t.SetUINT64(&MF_MT_FRAME_RATE, pack2(cfg.fps, 1))?;
            t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1))?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        }
        Ok(())
    };
    win("input media type", set())?;
    Ok(t)
}

fn variant_u32(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = v;
    }
    var
}

fn variant_bool(b: bool) -> VARIANT {
    let mut var = VARIANT::default();
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = if b { VARIANT_TRUE } else { VARIANT_FALSE };
    }
    var
}

/// Applies the low-latency / CBR / small-VBV settings. Each failure is logged, never fatal.
fn apply_codec_settings(api: &ICodecAPI, cfg: &EncoderConfig) {
    let bps = cfg.bitrate_kbps as u64 * 1000;
    let vbv_bits = (bps / cfg.fps as u64) * 3 / 2;
    let flags: [(&str, GUID, bool); 4] = [
        ("CODECAPI_AVLowLatencyMode", CODECAPI_AVLowLatencyMode, true),
        (
            "CODECAPI_AVEncCommonLowLatency",
            CODECAPI_AVEncCommonLowLatency,
            true,
        ),
        (
            "CODECAPI_AVEncCommonRealTime",
            CODECAPI_AVEncCommonRealTime,
            true,
        ),
        (
            "CODECAPI_AVEncMPVDefaultBPictureCount",
            CODECAPI_AVEncMPVDefaultBPictureCount,
            false,
        ),
    ];
    for (name, guid, on) in flags {
        // Some encoders want VT_UI4 for these, others VT_BOOL: try BOOL, then UI4.
        let as_bool = unsafe { api.SetValue(&guid, &variant_bool(on)) };
        if as_bool.is_err() && unsafe { api.SetValue(&guid, &variant_u32(on as u32)) }.is_err() {
            let e = as_bool.err().unwrap_or_else(windows::core::Error::empty);
            eprintln!("wb-encode: {name} not applied: {e}");
        }
    }
    let values: [(&str, GUID, u32); 4] = [
        (
            "CODECAPI_AVEncCommonRateControlMode",
            CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_CBR.0 as u32,
        ),
        (
            "CODECAPI_AVEncCommonMeanBitRate",
            CODECAPI_AVEncCommonMeanBitRate,
            bps as u32,
        ),
        (
            "CODECAPI_AVEncCommonBufferSize",
            CODECAPI_AVEncCommonBufferSize,
            vbv_bits as u32,
        ),
        (
            "CODECAPI_AVEncMPVGOPSize",
            CODECAPI_AVEncMPVGOPSize,
            1_000_000,
        ),
    ];
    for (name, guid, v) in values {
        if let Err(e) = unsafe { api.SetValue(&guid, &variant_u32(v)) } {
            eprintln!("wb-encode: {name} not applied: {e}");
        }
    }
}

/// Builds an IMFSample wrapping an NV12 texture, with time and duration in 100 ns units.
fn make_sample(
    tex: &ID3D11Texture2D,
    w: u32,
    h: u32,
    pts_us: u64,
    dur_100ns: i64,
) -> Result<IMFSample, EncodeError> {
    let unk: IUnknown = win("QI IUnknown", tex.cast())?;
    let buf = win("MFCreateDXGISurfaceBuffer", unsafe {
        MFCreateDXGISurfaceBuffer(&<ID3D11Texture2D as Interface>::IID, &unk, 0, false)
    })?;
    let len = win("MFGetPlaneSize", unsafe {
        MFGetPlaneSize(NV12_FOURCC, w, h)
    })?;
    win("IMFMediaBuffer::SetCurrentLength", unsafe {
        buf.SetCurrentLength(len)
    })?;
    let s = win("MFCreateSample", unsafe { MFCreateSample() })?;
    win("IMFSample::AddBuffer", unsafe { s.AddBuffer(&buf) })?;
    win("IMFSample::SetSampleTime", unsafe {
        s.SetSampleTime(pts_us as i64 * 10)
    })?;
    win("IMFSample::SetSampleDuration", unsafe {
        s.SetSampleDuration(dur_100ns)
    })?;
    Ok(s)
}

/// BGRA -> NV12 conversion and scaling with the D3D11 VideoProcessor.
struct Converter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    out_w: u32,
    out_h: u32,
    fps: u32,
    in_size: (u32, u32),
    enumerator: Option<ID3D11VideoProcessorEnumerator>,
    processor: Option<ID3D11VideoProcessor>,
    /// Input views keyed by texture identity; holding the texture keeps the pointer unique.
    in_views: Vec<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
    ring_tex: Vec<ID3D11Texture2D>,
    ring_views: Vec<ID3D11VideoProcessorOutputView>,
    next: usize,
}

impl Converter {
    fn new(device: &ID3D11Device, cfg: &EncoderConfig) -> Result<Self, EncodeError> {
        let vdev: ID3D11VideoDevice = win("QI ID3D11VideoDevice", device.cast())?;
        let ctx: ID3D11DeviceContext = win("ID3D11Device::GetImmediateContext", unsafe {
            device.GetImmediateContext()
        })?;
        let vctx: ID3D11VideoContext = win("QI ID3D11VideoContext", ctx.cast())?;

        let mut ring_tex = Vec::with_capacity(RING_LEN);
        for _ in 0..RING_LEN {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: cfg.width,
                Height: cfg.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            win("CreateTexture2D(NV12 ring)", unsafe {
                device.CreateTexture2D(&desc, None, Some(&mut tex))
            })?;
            ring_tex
                .push(tex.ok_or_else(|| EncodeError::Config("NV12 texture not created".into()))?);
        }

        Ok(Converter {
            vdev,
            vctx,
            out_w: cfg.width,
            out_h: cfg.height,
            fps: cfg.fps,
            in_size: (0, 0),
            enumerator: None,
            processor: None,
            in_views: Vec::new(),
            ring_tex,
            ring_views: Vec::new(),
            next: 0,
        })
    }

    /// (Re)creates the processor for an input of `w x h`.
    fn rebuild(&mut self, w: u32, h: u32) -> Result<(), EncodeError> {
        let rate = DXGI_RATIONAL {
            Numerator: self.fps,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: w,
            InputHeight: h,
            OutputFrameRate: rate,
            OutputWidth: self.out_w,
            OutputHeight: self.out_h,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let en = win("CreateVideoProcessorEnumerator", unsafe {
            self.vdev.CreateVideoProcessorEnumerator(&desc)
        })?;
        let proc = win("CreateVideoProcessor", unsafe {
            self.vdev.CreateVideoProcessor(&en, 0)
        })?;

        let mut views = Vec::with_capacity(RING_LEN);
        for tex in &self.ring_tex {
            let vdesc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut v: Option<ID3D11VideoProcessorOutputView> = None;
            win("CreateVideoProcessorOutputView", unsafe {
                self.vdev
                    .CreateVideoProcessorOutputView(tex, &en, &vdesc, Some(&mut v))
            })?;
            views.push(v.ok_or_else(|| EncodeError::Config("output view not created".into()))?);
        }

        // Input: full-range RGB. Output: BT.709, limited range (NV12 for the encoder).
        let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: 1 << 4, // Nominal_Range = 0-255, RGB_Range = full
        };
        let out_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: (1 << 2) | (2 << 4), // YCbCr_Matrix = BT.709, Nominal_Range = 16-235
        };
        let src = RECT {
            left: 0,
            top: 0,
            right: w as i32,
            bottom: h as i32,
        };
        let dst = RECT {
            left: 0,
            top: 0,
            right: self.out_w as i32,
            bottom: self.out_h as i32,
        };
        unsafe {
            self.vctx
                .VideoProcessorSetStreamColorSpace(&proc, 0, &in_cs);
            self.vctx.VideoProcessorSetOutputColorSpace(&proc, &out_cs);
            self.vctx
                .VideoProcessorSetStreamSourceRect(&proc, 0, true, Some(&src));
            self.vctx
                .VideoProcessorSetStreamDestRect(&proc, 0, true, Some(&dst));
            self.vctx
                .VideoProcessorSetOutputTargetRect(&proc, true, Some(&dst));
        }

        self.enumerator = Some(en);
        self.processor = Some(proc);
        self.ring_views = views;
        self.in_views.clear();
        self.in_size = (w, h);
        Ok(())
    }

    fn input_view(
        &mut self,
        src: &ID3D11Texture2D,
    ) -> Result<ID3D11VideoProcessorInputView, EncodeError> {
        let raw = src.as_raw();
        if let Some((_, v)) = self.in_views.iter().find(|(t, _)| t.as_raw() == raw) {
            return Ok(v.clone());
        }
        let en = self
            .enumerator
            .as_ref()
            .ok_or_else(|| EncodeError::Config("processor not built".into()))?;
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let mut v: Option<ID3D11VideoProcessorInputView> = None;
        win("CreateVideoProcessorInputView", unsafe {
            self.vdev
                .CreateVideoProcessorInputView(src, en, &desc, Some(&mut v))
        })?;
        let v = v.ok_or_else(|| EncodeError::Config("input view not created".into()))?;
        if self.in_views.len() >= MAX_INPUT_VIEWS {
            self.in_views.remove(0);
        }
        self.in_views.push((src.clone(), v.clone()));
        Ok(v)
    }

    /// Converts and scales `src` into the next ring NV12 texture and returns it.
    fn convert(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11Texture2D, EncodeError> {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut d) };
        let (w, h) = (d.Width, d.Height);
        if self.processor.is_none() || self.in_size != (w, h) {
            self.rebuild(w, h)?;
        }
        let view = self.input_view(src)?;
        let idx = self.next;
        self.next = (self.next + 1) % RING_LEN;

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(view)),
            ppFutureSurfaces: ptr::null_mut(),
            ppPastSurfacesRight: ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: ptr::null_mut(),
        };
        let proc = self
            .processor
            .as_ref()
            .ok_or_else(|| EncodeError::Config("processor not built".into()))?;
        let r = unsafe {
            self.vctx.VideoProcessorBlt(
                proc,
                &self.ring_views[idx],
                0,
                std::slice::from_ref(&stream),
            )
        };
        // The stream holds a manually managed reference to the input view.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        win("ID3D11VideoContext::VideoProcessorBlt", r)?;
        Ok(self.ring_tex[idx].clone())
    }
}

/// Converts AVCC (length-prefixed) NAL units into Annex-B.
fn avcc_to_annexb(data: &[u8], nal_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    let mut pos = 0usize;
    while pos + nal_len <= data.len() {
        let mut l = 0usize;
        for &b in &data[pos..pos + nal_len] {
            l = (l << 8) | b as usize;
        }
        pos += nal_len;
        if l == 0 || pos + l > data.len() {
            break;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&data[pos..pos + l]);
        pos += l;
    }
    out
}

/// Parses an avcC box (or passes through Annex-B) into Annex-B parameter sets.
fn parse_sequence_header(blob: &[u8]) -> (Vec<u8>, usize) {
    if blob.starts_with(&START_CODE) || blob.starts_with(&[0, 0, 1]) {
        return (blob.to_vec(), 4);
    }
    if blob.len() < 7 || blob[0] != 1 {
        return (Vec::new(), 4);
    }
    let nal_len = (blob[4] & 3) as usize + 1;
    let mut out = Vec::new();
    let mut pos = 5usize;
    let mut count = (blob[5] & 0x1f) as usize;
    pos += 1;
    for pass in 0..2 {
        for _ in 0..count {
            if pos + 2 > blob.len() {
                return (Vec::new(), 4);
            }
            let l = u16::from_be_bytes([blob[pos], blob[pos + 1]]) as usize;
            pos += 2;
            if pos + l > blob.len() {
                return (Vec::new(), 4);
            }
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(&blob[pos..pos + l]);
            pos += l;
        }
        if pass == 0 {
            if pos >= blob.len() {
                return (Vec::new(), 4);
            }
            count = blob[pos] as usize;
            pos += 1;
        }
    }
    (out, nal_len)
}

/// NAL unit types (`nal_unit_type`) of an Annex-B buffer.
fn nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let s = i + 3;
            if s < data.len() {
                types.push(data[s] & 0x1f);
            }
            i = s;
        } else {
            i += 1;
        }
    }
    types
}
