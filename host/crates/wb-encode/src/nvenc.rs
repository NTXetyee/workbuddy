//! Direct NVENC backend. `nvEncodeAPI64.dll` (shipped with the NVIDIA driver) is loaded at run
//! time and fed the D3D11 BGRA texture directly through `nvEncRegisterResource`, so NVENC does
//! the colour conversion itself. No CUDA is involved.
//!
//! Synchronous mode: map -> encode -> lock bitstream (blocks) -> copy -> unlock -> unmap. The
//! stream is H.264 High, preset P1 with the ultra-low-latency tuning, CBR, no B-frames, infinite
//! GOP (IDR on request only) and SPS/PPS repeated on every IDR.
//!
//! Parameter structs are never created as zero-filled Rust values: several NVENC enums have no
//! zero variant, so an all-zero struct is invalid Rust. They are built in `MaybeUninit` storage
//! and written field by field through raw pointers. The preset config is only borrowed as a
//! Rust reference after the driver has filled it.

mod sys;

use std::ffi::{c_void, CStr};
use std::mem::{self, MaybeUninit};
use std::ptr::{self, addr_of, addr_of_mut};
use std::time::{Duration, Instant};

use windows::core::{s, w, HRESULT, PCSTR};
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::Graphics::Direct3D11::{ID3D11DeviceContext, D3D11_TEXTURE2D_DESC};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

use sys::nvEncodeAPI::*;

use crate::convert::{Converter, OutFormat};
use crate::util::{adapter_info, nal_types, validate};
use crate::{EncodeError, EncodeStats, EncodedFrame, EncoderConfig, ID3D11Device, ID3D11Texture2D};

/// Driver NVENC API version (`(major << 4) | minor`) required for the 12.1 header.
const MIN_DRIVER_API: u32 = (12 << 4) | 1;

/// Registered input textures kept before the oldest is unregistered.
const MAX_REGISTRATIONS: usize = 16;

type CreateInstanceFn = unsafe extern "system" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
type GetMaxVersionFn = unsafe extern "system" fn(*mut u32) -> NVENCSTATUS;

/// Writes one field of a parameter struct through a raw pointer, without creating a reference
/// to the whole struct.
macro_rules! put {
    ($p:expr, $($f:ident).+ = $v:expr) => {
        addr_of_mut!((*$p).$($f).+).write($v)
    };
}

/// Reads one plain (integer or pointer) field through a raw pointer.
macro_rules! take {
    ($p:expr, $($f:ident).+) => {
        addr_of!((*$p).$($f).+).read()
    };
}

/// Loaded NVENC library and its function table. Freed on drop, after the session is gone.
struct Api {
    module: HMODULE,
    fns: NV_ENCODE_API_FUNCTION_LIST,
}

impl Drop for Api {
    fn drop(&mut self) {
        let _ = unsafe { FreeLibrary(self.module) };
    }
}

impl Api {
    fn last_error(&self, session: *mut c_void) -> String {
        match self.fns.nvEncGetLastErrorString {
            Some(f) if !session.is_null() => {
                let p = unsafe { f(session) };
                if p.is_null() {
                    "no detail".to_owned()
                } else {
                    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
                }
            }
            _ => "no session".to_owned(),
        }
    }
}

/// Loads `nvEncodeAPI64.dll` and fills its function table.
fn load_api() -> Result<Api, EncodeError> {
    let module = unsafe { LoadLibraryW(w!("nvEncodeAPI64.dll")) }.map_err(|source| {
        EncodeError::Windows {
            what: "LoadLibraryW(nvEncodeAPI64.dll)",
            source,
        }
    })?;
    // From here on `api` owns the module and frees it if anything below fails.
    // The function list holds only function pointers and integers, so zero is valid for it.
    let mut api = Api {
        module,
        fns: unsafe { mem::zeroed() },
    };

    let get_max: GetMaxVersionFn =
        unsafe { proc(module, s!("NvEncodeAPIGetMaxSupportedVersion")) }?;
    let mut max = 0u32;
    nv_status(
        "NvEncodeAPIGetMaxSupportedVersion",
        unsafe { get_max(&mut max) },
        None,
    )?;
    if max < MIN_DRIVER_API {
        return Err(EncodeError::Config(format!(
            "NVENC driver API {}.{} is older than the 12.1 header",
            max >> 4,
            max & 0xf
        )));
    }

    let create: CreateInstanceFn = unsafe { proc(module, s!("NvEncodeAPICreateInstance")) }?;
    api.fns.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    nv_status(
        "NvEncodeAPICreateInstance",
        unsafe { create(&mut api.fns) },
        None,
    )?;
    Ok(api)
}

/// Resolves one export of the NVENC DLL.
///
/// # Safety
/// `T` must be the function-pointer type that the export actually has.
unsafe fn proc<T: Copy>(module: HMODULE, name: PCSTR) -> Result<T, EncodeError> {
    let f = GetProcAddress(module, name).ok_or_else(|| {
        EncodeError::Config("an NVENC export is missing from nvEncodeAPI64.dll".into())
    })?;
    Ok(mem::transmute_copy(&f))
}

fn get<T>(f: Option<T>, name: &'static str) -> Result<T, EncodeError> {
    f.ok_or_else(|| EncodeError::Config(format!("NVENC function table has no {name}")))
}

/// Maps a non-success NVENC status to an error. `session` is used only for the detail text.
fn nv_status(
    what: &'static str,
    st: NVENCSTATUS,
    session: Option<(&Api, *mut c_void)>,
) -> Result<(), EncodeError> {
    if st == NVENCSTATUS::NV_ENC_SUCCESS {
        return Ok(());
    }
    let code = st as u32;
    let detail = match session {
        Some((api, s)) => api.last_error(s),
        None => "no detail".to_owned(),
    };
    Err(EncodeError::Windows {
        what,
        source: windows::core::Error::new(
            HRESULT(code as i32),
            format!("NVENC status {code}: {detail}"),
        ),
    })
}

/// A D3D11 texture registered with NVENC.
struct Registration {
    key: usize,
    /// Keeps the texture (and so the pointer used as `key`) alive while registered.
    _tex: ID3D11Texture2D,
    handle: *mut c_void,
}

/// Direct NVENC H.264 encoder on the device's own D3D11 texture.
///
/// Field order matters: `Drop` tears the session down first, and `api` (the DLL) is declared
/// last so that it is unloaded only after the session has been destroyed.
pub(crate) struct NvencEncoder {
    cfg: EncoderConfig,
    adapter: String,
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    /// Only built on the rare path where the input size or format differs from the config.
    conv: Option<Converter>,
    session: *mut c_void,
    bitstream: *mut c_void,
    regs: Vec<Registration>,
    frame_idx: u32,
    stats: EncodeStats,
    api: Api,
}

// SAFETY: the NVENC session is only used through `&mut self`, so it is never called from two
// threads at once. The raw handles refer to driver objects, not to thread-local state.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    /// Opens an NVENC session on `device` and configures H.264 low-latency CBR output.
    pub(crate) fn new(device: &ID3D11Device, cfg: EncoderConfig) -> Result<Self, EncodeError> {
        validate(&cfg)?;
        let info = adapter_info(device)?;
        let ctx = crate::win("ID3D11Device::GetImmediateContext", unsafe {
            device.GetImmediateContext()
        })?;
        let api = load_api()?;
        let mut enc = NvencEncoder {
            cfg,
            adapter: info.name,
            device: device.clone(),
            ctx,
            conv: None,
            session: ptr::null_mut(),
            bitstream: ptr::null_mut(),
            regs: Vec::new(),
            frame_idx: 0,
            stats: EncodeStats::default(),
            api,
        };
        enc.open_session()?;
        enc.init_encoder()?;
        enc.create_bitstream()?;
        Ok(enc)
    }

    fn open_session(&mut self) -> Result<(), EncodeError> {
        let mut params = MaybeUninit::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>::zeroed();
        let p = params.as_mut_ptr();
        unsafe {
            put!(p, version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER);
            put!(
                p,
                deviceType = NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX
            );
            put!(p, device = windows::core::Interface::as_raw(&self.device));
            put!(p, apiVersion = NVENCAPI_VERSION);
        }
        let open = get(
            self.api.fns.nvEncOpenEncodeSessionEx,
            "nvEncOpenEncodeSessionEx",
        )?;
        let mut session: *mut c_void = ptr::null_mut();
        let st = unsafe { open(p, &mut session) };
        nv_status("nvEncOpenEncodeSessionEx", st, None)?;
        self.session = session;
        Ok(())
    }

    fn init_encoder(&mut self) -> Result<(), EncodeError> {
        let fps = self.cfg.fps;
        let bps = self.cfg.bitrate_kbps as u64 * 1000;
        // VBV = 1.5 frame intervals, as in the Media Foundation backend.
        let vbv_bits = ((bps / fps as u64) * 3 / 2) as u32;

        let get_preset = get(
            self.api.fns.nvEncGetEncodePresetConfigEx,
            "nvEncGetEncodePresetConfigEx",
        )?;
        let mut preset = MaybeUninit::<NV_ENC_PRESET_CONFIG>::zeroed();
        let pp = preset.as_mut_ptr();
        unsafe {
            put!(pp, version = NV_ENC_PRESET_CONFIG_VER);
            put!(pp, presetCfg.version = NV_ENC_CONFIG_VER);
        }
        let st = unsafe {
            get_preset(
                self.session,
                NV_ENC_CODEC_H264_GUID,
                NV_ENC_PRESET_P1_GUID,
                NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                pp,
            )
        };
        nv_status(
            "nvEncGetEncodePresetConfigEx",
            st,
            Some((&self.api, self.session)),
        )?;
        // The driver has now written the whole preset, so it is a valid value.
        let preset = unsafe { preset.assume_init_mut() };

        {
            let c = &mut preset.presetCfg;
            c.profileGUID = NV_ENC_H264_PROFILE_HIGH_GUID;
            c.gopLength = NVENC_INFINITE_GOPLENGTH;
            c.frameIntervalP = 1;
            c.frameFieldMode = NV_ENC_PARAMS_FRAME_FIELD_MODE::NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME;
            c.mvPrecision = NV_ENC_MV_PRECISION::NV_ENC_MV_PRECISION_DEFAULT;

            let rc = &mut c.rcParams;
            rc.version = NV_ENC_RC_PARAMS_VER;
            rc.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
            rc.averageBitRate = bps as u32;
            rc.maxBitRate = bps as u32;
            rc.vbvBufferSize = vbv_bits;
            rc.vbvInitialDelay = vbv_bits;
            rc.multiPass = NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED;
            rc.set_enableLookahead(0);
            rc.set_enableAQ(0);
            rc.set_zeroReorderDelay(1);

            let h = unsafe { &mut c.encodeCodecConfig.h264Config };
            h.set_repeatSPSPPS(1);
            h.set_outputAUD(0);
            h.set_enableIntraRefresh(0);
            h.idrPeriod = NVENC_INFINITE_GOPLENGTH;
            h.level = NV_ENC_LEVEL::NV_ENC_LEVEL_AUTOSELECT as u32;
            h.h264VUIParameters.videoSignalTypePresentFlag = 1;
            h.h264VUIParameters.videoFormat =
                NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
            h.h264VUIParameters.videoFullRangeFlag = 0; // limited range
            h.h264VUIParameters.colourDescriptionPresentFlag = 1;
            h.h264VUIParameters.colourPrimaries =
                NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
            h.h264VUIParameters.transferCharacteristics =
                NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
            h.h264VUIParameters.colourMatrix =
                NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;
        }

        let mut init = MaybeUninit::<NV_ENC_INITIALIZE_PARAMS>::zeroed();
        let ip = init.as_mut_ptr();
        unsafe {
            put!(ip, version = NV_ENC_INITIALIZE_PARAMS_VER);
            put!(ip, encodeGUID = NV_ENC_CODEC_H264_GUID);
            put!(ip, presetGUID = NV_ENC_PRESET_P1_GUID);
            put!(ip, encodeWidth = self.cfg.width);
            put!(ip, encodeHeight = self.cfg.height);
            put!(ip, darWidth = self.cfg.width);
            put!(ip, darHeight = self.cfg.height);
            put!(ip, frameRateNum = fps);
            put!(ip, frameRateDen = 1);
            put!(ip, enableEncodeAsync = 0);
            put!(ip, enablePTD = 1);
            put!(
                ip,
                encodeConfig = &mut preset.presetCfg as *mut NV_ENC_CONFIG
            );
            put!(ip, maxEncodeWidth = 0);
            put!(ip, maxEncodeHeight = 0);
            put!(
                ip,
                tuningInfo = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY
            );
            put!(
                ip,
                bufferFormat = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB
            );
        }

        let f = get(
            self.api.fns.nvEncInitializeEncoder,
            "nvEncInitializeEncoder",
        )?;
        let st = unsafe { f(self.session, ip) };
        nv_status(
            "nvEncInitializeEncoder",
            st,
            Some((&self.api, self.session)),
        )
    }

    fn create_bitstream(&mut self) -> Result<(), EncodeError> {
        let f = get(
            self.api.fns.nvEncCreateBitstreamBuffer,
            "nvEncCreateBitstreamBuffer",
        )?;
        let mut cb = MaybeUninit::<NV_ENC_CREATE_BITSTREAM_BUFFER>::zeroed();
        let p = cb.as_mut_ptr();
        unsafe { put!(p, version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER) };
        let st = unsafe { f(self.session, p) };
        nv_status(
            "nvEncCreateBitstreamBuffer",
            st,
            Some((&self.api, self.session)),
        )?;
        self.bitstream = unsafe { take!(p, bitstreamBuffer) };
        Ok(())
    }

    /// Returns a BGRA texture of the configured size holding `tex`. Only converts (through the
    /// VideoProcessor) when the input size or format differs from the configuration.
    fn prepare_input(
        &mut self,
        tex: &ID3D11Texture2D,
    ) -> Result<(ID3D11Texture2D, Duration), EncodeError> {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut d) };
        if d.Width == self.cfg.width
            && d.Height == self.cfg.height
            && d.Format == DXGI_FORMAT_B8G8R8A8_UNORM
        {
            return Ok((tex.clone(), Duration::ZERO));
        }
        if self.conv.is_none() {
            self.conv = Some(Converter::new(&self.device, &self.cfg, OutFormat::Bgra)?);
        }
        let conv = self.conv.as_mut().expect("converter created above");
        conv.convert(tex)
    }

    /// Returns the NVENC handle for `tex`, registering it on first use.
    fn register(&mut self, tex: &ID3D11Texture2D) -> Result<*mut c_void, EncodeError> {
        let key = windows::core::Interface::as_raw(tex) as usize;
        if let Some(r) = self.regs.iter().find(|r| r.key == key) {
            return Ok(r.handle);
        }
        if self.regs.len() >= MAX_REGISTRATIONS {
            let old = self.regs.remove(0);
            self.unregister(old.handle);
        }
        let f = get(self.api.fns.nvEncRegisterResource, "nvEncRegisterResource")?;
        let mut reg = MaybeUninit::<NV_ENC_REGISTER_RESOURCE>::zeroed();
        let p = reg.as_mut_ptr();
        unsafe {
            put!(p, version = NV_ENC_REGISTER_RESOURCE_VER);
            put!(
                p,
                resourceType = NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX
            );
            put!(p, width = self.cfg.width);
            put!(p, height = self.cfg.height);
            put!(p, pitch = 0);
            put!(p, subResourceIndex = 0);
            put!(p, resourceToRegister = key as *mut c_void);
            put!(
                p,
                bufferFormat = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB
            );
            put!(p, bufferUsage = NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE);
        }
        let st = unsafe { f(self.session, p) };
        nv_status("nvEncRegisterResource", st, Some((&self.api, self.session)))?;
        let handle = unsafe { take!(p, registeredResource) };
        self.regs.push(Registration {
            key,
            _tex: tex.clone(),
            handle,
        });
        Ok(handle)
    }

    /// Unregisters one handle. Failure is ignored: this runs on eviction and teardown.
    fn unregister(&self, handle: *mut c_void) {
        if let Some(f) = self.api.fns.nvEncUnregisterResource {
            let _ = unsafe { f(self.session, handle) };
        }
    }

    /// Encodes one BGRA frame. Synchronous: returns the frame for this call, or `None` if NVENC
    /// asks for more input (it should not in this mode).
    pub(crate) fn encode(
        &mut self,
        tex: &ID3D11Texture2D,
        pts_us: u64,
        force_idr: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError> {
        let started = Instant::now();
        let (input, convert) = self.prepare_input(tex)?;
        let handle = self.register(&input)?;
        // Submit the copy/convert work on the immediate context before NVENC reads the texture.
        unsafe { self.ctx.Flush() };

        let map_fn = get(self.api.fns.nvEncMapInputResource, "nvEncMapInputResource")?;
        let mut map = MaybeUninit::<NV_ENC_MAP_INPUT_RESOURCE>::zeroed();
        let mp = map.as_mut_ptr();
        unsafe {
            put!(mp, version = NV_ENC_MAP_INPUT_RESOURCE_VER);
            put!(mp, registeredResource = handle);
        }
        let st = unsafe { map_fn(self.session, mp) };
        nv_status("nvEncMapInputResource", st, Some((&self.api, self.session)))?;
        let mapped = unsafe { take!(mp, mappedResource) };

        let out = self.submit(mapped, pts_us, force_idr);
        let unmapped = self.unmap(mapped);
        let frame = out?;
        unmapped?;

        self.stats.convert_total += convert;
        self.stats.mft_total += started.elapsed();
        Ok(frame)
    }

    /// Encodes a mapped input and reads back its bitstream.
    fn submit(
        &mut self,
        mapped: NV_ENC_INPUT_PTR,
        pts_us: u64,
        force_idr: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError> {
        let encode_fn = get(self.api.fns.nvEncEncodePicture, "nvEncEncodePicture")?;
        let mut pic = MaybeUninit::<NV_ENC_PIC_PARAMS>::zeroed();
        let pp = pic.as_mut_ptr();
        let flags = if force_idr {
            NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
                | NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32
        } else {
            0
        };
        unsafe {
            put!(pp, version = NV_ENC_PIC_PARAMS_VER);
            put!(pp, inputWidth = self.cfg.width);
            put!(pp, inputHeight = self.cfg.height);
            put!(pp, inputPitch = self.cfg.width);
            put!(pp, inputBuffer = mapped);
            put!(pp, outputBitstream = self.bitstream);
            put!(
                pp,
                bufferFmt = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB
            );
            put!(
                pp,
                pictureStruct = NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME
            );
            put!(pp, pictureType = NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_UNKNOWN);
            put!(pp, inputTimeStamp = pts_us);
            put!(pp, inputDuration = 1_000_000 / self.cfg.fps as u64);
            put!(pp, frameIdx = self.frame_idx);
            put!(pp, encodePicFlags = flags);
        }
        let st = unsafe { encode_fn(self.session, pp) };
        if st == NVENCSTATUS::NV_ENC_ERR_NEED_MORE_INPUT {
            return Ok(None);
        }
        nv_status("nvEncEncodePicture", st, Some((&self.api, self.session)))?;
        self.frame_idx = self.frame_idx.wrapping_add(1);
        self.stats.frames += 1;

        let lock_fn = get(self.api.fns.nvEncLockBitstream, "nvEncLockBitstream")?;
        let mut lock = MaybeUninit::<NV_ENC_LOCK_BITSTREAM>::zeroed();
        let lp = lock.as_mut_ptr();
        unsafe {
            put!(lp, version = NV_ENC_LOCK_BITSTREAM_VER);
            put!(lp, outputBitstream = self.bitstream);
        }
        let st = unsafe { lock_fn(self.session, lp) };
        nv_status("nvEncLockBitstream", st, Some((&self.api, self.session)))?;

        let len = unsafe { take!(lp, bitstreamSizeInBytes) } as usize;
        let buf_ptr = unsafe { take!(lp, bitstreamBufferPtr) } as *const u8;
        let data = if len == 0 || buf_ptr.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(buf_ptr, len) }.to_vec()
        };

        let unlock = get(self.api.fns.nvEncUnlockBitstream, "nvEncUnlockBitstream")?;
        let st = unsafe { unlock(self.session, self.bitstream) };
        nv_status("nvEncUnlockBitstream", st, Some((&self.api, self.session)))?;
        if data.is_empty() {
            return Ok(None);
        }

        let types = nal_types(&data);
        let keyframe = types.contains(&5);
        let has_params = types.contains(&7) && types.contains(&8);
        Ok(Some(EncodedFrame {
            data,
            keyframe,
            has_params,
            pts_us,
        }))
    }

    fn unmap(&self, mapped: NV_ENC_INPUT_PTR) -> Result<(), EncodeError> {
        let f = get(
            self.api.fns.nvEncUnmapInputResource,
            "nvEncUnmapInputResource",
        )?;
        let st = unsafe { f(self.session, mapped) };
        nv_status(
            "nvEncUnmapInputResource",
            st,
            Some((&self.api, self.session)),
        )
    }

    /// Returns the counters accumulated since the previous call and resets them.
    pub(crate) fn take_stats(&mut self) -> EncodeStats {
        let mut s = mem::take(&mut self.stats);
        if let Some(c) = self.conv.as_mut() {
            c.fill_gpu_stats(&mut s);
        }
        s
    }

    /// Name of the DXGI adapter the session runs on (the device's adapter).
    pub(crate) fn adapter_name(&self) -> String {
        self.adapter.clone()
    }
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        if self.session.is_null() {
            return;
        }
        // End of stream: a picture with no input and the EOS flag. Errors are ignored.
        if let Some(f) = self.api.fns.nvEncEncodePicture {
            let mut pic = MaybeUninit::<NV_ENC_PIC_PARAMS>::zeroed();
            let pp = pic.as_mut_ptr();
            unsafe {
                put!(pp, version = NV_ENC_PIC_PARAMS_VER);
                put!(
                    pp,
                    encodePicFlags = NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_EOS as u32
                );
                let _ = f(self.session, pp);
            }
        }
        let regs = mem::take(&mut self.regs);
        for r in regs {
            self.unregister(r.handle);
        }
        if !self.bitstream.is_null() {
            if let Some(f) = self.api.fns.nvEncDestroyBitstreamBuffer {
                let _ = unsafe { f(self.session, self.bitstream) };
            }
            self.bitstream = ptr::null_mut();
        }
        if let Some(f) = self.api.fns.nvEncDestroyEncoder {
            let _ = unsafe { f(self.session) };
        }
        self.session = ptr::null_mut();
    }
}
