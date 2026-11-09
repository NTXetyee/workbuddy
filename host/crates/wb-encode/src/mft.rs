//! Media Foundation backend: a hardware H.264 MFT (AMD, Intel, or NVIDIA when NVENC is not
//! used) fed by the D3D11 VideoProcessor (BGRA -> NV12 with scaling).

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::ptr;
use std::thread::sleep;
use std::time::{Duration, Instant};

use windows::core::{IUnknown, Interface, GUID};
use windows::Win32::Foundation::{VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::ID3D11Multithread;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

use crate::convert::{Converter, OutFormat};
use crate::util::{adapter_info, nal_types, pack2, validate, START_CODE};
use crate::{
    win, EncodeError, EncodeStats, EncodedFrame, EncoderConfig, ID3D11Device, ID3D11Texture2D,
};

const INPUT_WAIT: Duration = Duration::from_millis(50);
const OUTPUT_WAIT: Duration = Duration::from_millis(50);
const DRAIN_WAIT: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(1);
const NV12_FOURCC: u32 = u32::from_le_bytes(*b"NV12");

// Event type values (MediaEventType) used by the transform.
const EVT_NEED_INPUT: u32 = 601;
const EVT_HAVE_OUTPUT: u32 = 602;
const EVT_DRAIN_COMPLETE: u32 = 604;

/// Keeps MF started for the lifetime of an encoder. Declared last in `MftEncoder`
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
pub(crate) struct MftEncoder {
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
    stats: EncodeStats,
    /// Kept alive for the transform's lifetime (the transform references it).
    _mgr: IMFDXGIDeviceManager,
    _mf: MfGuard,
}

impl MftEncoder {
    /// Creates a hardware H.264 encoder on the same adapter as `device`.
    pub(crate) fn new(device: &ID3D11Device, cfg: EncoderConfig) -> Result<Self, EncodeError> {
        validate(&cfg)?;
        let mf = MfGuard::start()?;

        // The DXGI device manager requires a multithread-protected device.
        let mt: ID3D11Multithread = win("QI ID3D11Multithread", device.cast())?;
        let _ = unsafe { mt.SetMultithreadProtected(true) };

        let info = adapter_info(device)?;

        let mut token = 0u32;
        let mut mgr_opt: Option<IMFDXGIDeviceManager> = None;
        win("MFCreateDXGIDeviceManager", unsafe {
            MFCreateDXGIDeviceManager(&mut token, &mut mgr_opt)
        })?;
        let mgr = mgr_opt.ok_or_else(|| EncodeError::Config("no device manager".into()))?;
        win("IMFDXGIDeviceManager::ResetDevice", unsafe {
            mgr.ResetDevice(device, token)
        })?;

        let (mft, in_id, out_id) = create_transform(info.luid, &mgr, &cfg)?;

        // Settings via ICodecAPI; individual failures are non-fatal.
        let codec: Option<ICodecAPI> = mft.cast().ok();
        if let Some(api) = &codec {
            apply_codec_settings(api, &cfg);
        } else {
            log_diag!("wb-encode: transform exposes no ICodecAPI; using defaults");
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

        let conv = Converter::new(device, &cfg, OutFormat::Nv12)?;
        let mut enc = MftEncoder {
            cfg,
            adapter: info.name,
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
            stats: EncodeStats::default(),
            _mgr: mgr,
            _mf: mf,
        };
        let _ = enc.refresh_params();
        Ok(enc)
    }

    /// Encodes one BGRA frame (any size; scaled to the configured size).
    /// Returns `Ok(None)` when no output is ready within the internal time budget.
    pub(crate) fn encode(
        &mut self,
        tex: &ID3D11Texture2D,
        pts_us: u64,
        force_idr: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError> {
        if force_idr {
            self.force_idr();
        }
        let wait_started = Instant::now();
        let ready = self.pump(INPUT_WAIT, |s| s.need_input > 0)?;
        self.stats.input_wait_total += wait_started.elapsed();
        if !ready {
            return Ok(self.out_queue.pop_front());
        }

        let (nv12, blt) = self.conv.convert(tex)?;
        self.stats.convert_total += blt;
        let sample = make_sample(
            &nv12,
            self.cfg.width,
            self.cfg.height,
            pts_us,
            self.frame_dur_100ns,
        )?;
        let mft_started = Instant::now();
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
        self.stats.frames += 1;
        self.need_input = self.need_input.saturating_sub(1);
        self.pts_queue.push_back(pts_us);

        self.pump(OUTPUT_WAIT, |s| !s.out_queue.is_empty())?;
        self.stats.mft_total += mft_started.elapsed();
        Ok(self.out_queue.pop_front())
    }

    /// Returns the counters accumulated since the previous call and resets them.
    pub(crate) fn take_stats(&mut self) -> EncodeStats {
        let mut s = std::mem::take(&mut self.stats);
        self.conv.fill_gpu_stats(&mut s);
        s
    }

    /// Name of the DXGI adapter the encoder runs on (the device's adapter).
    pub(crate) fn adapter_name(&self) -> String {
        self.adapter.clone()
    }

    fn force_idr(&self) {
        if let Some(api) = &self.codec {
            let v = variant_u32(1);
            if let Err(e) = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &v) } {
                log_diag!("wb-encode: force keyframe failed: {e}");
            }
        }
    }

    /// Processes events until `done` holds or `budget` elapses. Returns whether `done` held.
    fn pump(
        &mut self,
        budget: Duration,
        done: impl Fn(&MftEncoder) -> bool,
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

impl Drop for MftEncoder {
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
                log_diag!("wb-encode: candidate encoder rejected: {e}");
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
        log_diag!("wb-encode: MF_LOW_LATENCY not accepted: {e}");
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
            log_diag!("wb-encode: {name} not applied: {e}");
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
            log_diag!("wb-encode: {name} not applied: {e}");
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
