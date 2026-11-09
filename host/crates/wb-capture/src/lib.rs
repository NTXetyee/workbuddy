//! GPU-resident screen capture with DXGI Desktop Duplication (SPEC 3.3).
//!
//! Frames never leave the GPU: each acquired desktop image is copied into a
//! private BGRA8 texture from a ring of [`RING_SIZE`] and the duplication frame is
//! released immediately.

use std::fmt;
use std::sync::Once;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11DeviceContext, ID3D11Resource, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL,
    DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwareness, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, PROCESS_PER_MONITOR_DPI_AWARE,
};

pub use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

/// Number of capture textures in the ring. A texture is reused only after this many newer
/// copies, so a frame still being read by the encoder is not overwritten in practice.
pub const RING_SIZE: usize = 3;
const _: () = assert!(matches!(RING_SIZE, 2..=4));

/// Errors from capture setup and frame acquisition.
#[derive(Debug)]
pub enum CaptureError {
    /// A Win32 / DXGI / D3D11 call failed.
    Windows(windows::core::Error),
    /// A higher-level failure, e.g. no matching output.
    Message(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaptureError::Windows(e) => write!(f, "windows error: {e}"),
            CaptureError::Message(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for CaptureError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CaptureError::Windows(e) => Some(e),
            CaptureError::Message(_) => None,
        }
    }
}

impl From<windows::core::Error> for CaptureError {
    fn from(e: windows::core::Error) -> Self {
        CaptureError::Windows(e)
    }
}

/// Description of one display output across all adapters.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub index: u32,
    pub adapter_name: String,
    pub device_name: String,
    pub width: u32,
    pub height: u32,
    pub attached: bool,
}

/// Selects which output to capture.
#[derive(Debug, Clone)]
pub enum OutputSel {
    /// Attached output whose desktop coordinates start at (0,0).
    Primary,
    /// Index as reported by [`list_outputs`].
    Index(u32),
    /// Case-insensitive substring of the adapter or device name.
    NameContains(String),
}

/// Capture counters since the last [`Capturer::take_stats`] call (measurement only).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct CaptureStats {
    /// New desktop images obtained (frames returned by `next_frame`).
    pub acquired: u64,
    /// Sum of `AccumulatedFrames` over the acquired frames. An average above 1 means the
    /// duplication merged frames because the consumer was too slow.
    pub accumulated_frames_sum: u64,
    /// `AcquireNextFrame` calls that hit `DXGI_ERROR_WAIT_TIMEOUT`.
    pub timeouts: u64,
    /// Time spent inside `AcquireNextFrame`, including timeouts and mouse-only updates.
    pub wait_total: Duration,
    /// CPU submit time of `CopyResource` plus `ReleaseFrame`.
    pub copy_total: Duration,
}

impl CaptureStats {
    /// Adds the counters of `other` to `self`.
    pub fn add(&mut self, other: &CaptureStats) {
        self.acquired += other.acquired;
        self.accumulated_frames_sum += other.accumulated_frames_sum;
        self.timeouts += other.timeouts;
        self.wait_total += other.wait_total;
        self.copy_total += other.copy_total;
    }
}

/// One captured frame. `texture` is an owned BGRA8 GPU copy.
pub struct Frame {
    pub texture: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    pub pts_us: u64,
}

struct Entry {
    info: OutputInfo,
    origin: (i32, i32),
    output: IDXGIOutput,
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// Opts the process into per-monitor DPI awareness so that DXGI reports
/// physical pixels instead of DPI-virtualized sizes. Failure is ignored: the
/// awareness may already have been set by the host, or the call may be
/// unsupported on this Windows version.
fn ensure_dpi_aware() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err() {
            let _ = SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
        }
    });
}

/// Physical size of a duplicated output, as reported by the duplication itself.
fn duplication_size(dup: &IDXGIOutputDuplication) -> (u32, u32) {
    let desc = unsafe { dup.GetDesc() };
    (desc.ModeDesc.Width, desc.ModeDesc.Height)
}

fn enumerate() -> Result<Vec<Entry>, CaptureError> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut entries: Vec<Entry> = Vec::new();
    let mut a = 0u32;
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(a) } {
            Ok(x) => x,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        };
        let adapter_desc = unsafe { adapter.GetDesc1()? };
        let adapter_name = wide_to_string(&adapter_desc.Description);
        let mut o = 0u32;
        loop {
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(o) } {
                Ok(x) => x,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e.into()),
            };
            let desc = unsafe { output.GetDesc()? };
            let r = desc.DesktopCoordinates;
            let info = OutputInfo {
                index: entries.len() as u32,
                adapter_name: adapter_name.clone(),
                device_name: wide_to_string(&desc.DeviceName),
                width: (r.right - r.left).max(0) as u32,
                height: (r.bottom - r.top).max(0) as u32,
                attached: desc.AttachedToDesktop.as_bool(),
            };
            entries.push(Entry {
                info,
                origin: (r.left, r.top),
                output,
            });
            o += 1;
        }
        a += 1;
    }
    Ok(entries)
}

/// Lists every output across all adapters, in enumeration order.
pub fn list_outputs() -> Result<Vec<OutputInfo>, CaptureError> {
    ensure_dpi_aware();
    Ok(enumerate()?.into_iter().map(|e| e.info).collect())
}

fn select(entries: &[Entry], sel: &OutputSel) -> Result<usize, CaptureError> {
    let found = match sel {
        OutputSel::Primary => entries
            .iter()
            .position(|e| e.info.attached && e.origin == (0, 0)),
        OutputSel::Index(i) => entries.iter().position(|e| e.info.index == *i),
        OutputSel::NameContains(s) => {
            let needle = s.to_lowercase();
            entries.iter().position(|e| {
                e.info.adapter_name.to_lowercase().contains(&needle)
                    || e.info.device_name.to_lowercase().contains(&needle)
            })
        }
    };
    found.ok_or_else(|| CaptureError::Message(format!("no output matches {sel:?}")))
}

fn alloc_ring(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<Vec<ID3D11Texture2D>, CaptureError> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut ring = Vec::with_capacity(RING_SIZE);
    for _ in 0..RING_SIZE {
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex as *mut _))? };
        ring.push(
            tex.ok_or_else(|| CaptureError::Message("CreateTexture2D returned null".into()))?,
        );
    }
    Ok(ring)
}

fn qpc_us() -> Result<u64, CaptureError> {
    let mut count = 0i64;
    let mut freq = 0i64;
    unsafe {
        QueryPerformanceCounter(&mut count)?;
        QueryPerformanceFrequency(&mut freq)?;
    }
    if freq <= 0 {
        return Err(CaptureError::Message(
            "QueryPerformanceFrequency <= 0".into(),
        ));
    }
    Ok((count as u128 * 1_000_000 / freq as u128) as u64)
}

/// GPU-resident desktop capturer for one output.
pub struct Capturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    ring: Vec<ID3D11Texture2D>,
    next_slot: usize,
    width: u32,
    height: u32,
    produced: bool,
    stats: CaptureStats,
}

impl Capturer {
    /// Creates a D3D11 device on the selected output's adapter and starts duplication.
    pub fn new(sel: OutputSel) -> Result<Self, CaptureError> {
        ensure_dpi_aware();
        let entries = enumerate()?;
        let idx = select(&entries, &sel)?;
        let info = entries[idx].info.clone();
        if info.width == 0 || info.height == 0 {
            return Err(CaptureError::Message(format!(
                "output {} has no desktop area",
                info.index
            )));
        }
        let output = entries[idx].output.clone();
        drop(entries);

        let adapter: IDXGIAdapter = unsafe { output.GetParent()? };
        let (device, context) = create_device(&adapter)?;

        let mt: ID3D10Multithread = device.cast()?;
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }

        let output1: IDXGIOutput1 = output.cast()?;
        let dup = unsafe { output1.DuplicateOutput(&device)? };
        // Real pixels from the duplication itself, independent of DPI scaling.
        let (width, height) = duplication_size(&dup);
        if width == 0 || height == 0 {
            return Err(CaptureError::Message(format!(
                "output {} duplication reports no size",
                info.index
            )));
        }
        let ring = alloc_ring(&device, width, height)?;

        Ok(Capturer {
            device,
            context,
            output: output1,
            dup: Some(dup),
            ring,
            next_slot: 0,
            width,
            height,
            produced: false,
            stats: CaptureStats::default(),
        })
    }

    /// The D3D11 device that owns the returned frame textures.
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    /// Returns the counters accumulated since the previous call and resets them.
    pub fn take_stats(&mut self) -> CaptureStats {
        std::mem::take(&mut self.stats)
    }

    /// Current output size in pixels (width, height).
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Waits up to `timeout_ms` for a new desktop image.
    ///
    /// Returns `Ok(None)` on timeout, or when duplication was lost and is being
    /// recreated. Mouse-only updates are skipped once a frame has been produced.
    pub fn next_frame(&mut self, timeout_ms: u32) -> Result<Option<Frame>, CaptureError> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        loop {
            if self.dup.is_none() {
                if let Err(e) = self.recreate() {
                    let _ = e; // transient (mode switch, secure desktop); retry on next call
                    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                    return Ok(None);
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait_ms = remaining.as_millis().min(u32::MAX as u128) as u32;
            let dup = self.dup.clone().expect("duplication present");

            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            let wait_started = Instant::now();
            let acquired = unsafe { dup.AcquireNextFrame(wait_ms, &mut info, &mut resource) };
            self.stats.wait_total += wait_started.elapsed();
            match acquired {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    self.stats.timeouts += 1;
                    return Ok(None);
                }
                Err(e)
                    if e.code() == DXGI_ERROR_ACCESS_LOST
                        || e.code() == DXGI_ERROR_INVALID_CALL =>
                {
                    self.dup = None;
                    let _ = self.recreate();
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }

            // The frame is acquired: it must be released whatever happens below.
            let result = self.take_frame(resource, info.LastPresentTime, info.AccumulatedFrames);
            let release_started = Instant::now();
            let _ = unsafe { dup.ReleaseFrame() };
            self.stats.copy_total += release_started.elapsed();
            if let Some(frame) = result? {
                return Ok(Some(frame));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }

    fn take_frame(
        &mut self,
        resource: Option<IDXGIResource>,
        last_present_time: i64,
        accumulated_frames: u32,
    ) -> Result<Option<Frame>, CaptureError> {
        let Some(resource) = resource else {
            return Ok(None);
        };
        if last_present_time == 0 && self.produced {
            return Ok(None); // mouse-only update
        }
        let src: ID3D11Texture2D = resource.cast()?;
        drop(resource);
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut desc) };
        if desc.Width != self.width || desc.Height != self.height {
            self.resize(desc.Width, desc.Height)?;
        }

        let slot = self.next_slot;
        self.next_slot = (slot + 1) % RING_SIZE;
        let dst = self.ring[slot].clone();
        let src_res: ID3D11Resource = src.cast()?;
        let dst_res: ID3D11Resource = dst.cast()?;
        let copy_started = Instant::now();
        unsafe { self.context.CopyResource(&dst_res, &src_res) };
        self.stats.copy_total += copy_started.elapsed();
        self.stats.acquired += 1;
        self.stats.accumulated_frames_sum += u64::from(accumulated_frames);
        self.produced = true;

        Ok(Some(Frame {
            texture: dst,
            width: self.width,
            height: self.height,
            pts_us: qpc_us()?,
        }))
    }

    fn resize(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        self.ring = alloc_ring(&self.device, width, height)?;
        self.width = width;
        self.height = height;
        self.next_slot = 0;
        Ok(())
    }

    fn recreate(&mut self) -> Result<(), CaptureError> {
        let dup = unsafe { self.output.DuplicateOutput(&self.device)? };
        let (w, h) = duplication_size(&dup);
        if w > 0 && h > 0 && (w, h) != (self.width, self.height) {
            self.resize(w, h)?;
        }
        self.dup = Some(dup);
        Ok(())
    }
}

fn create_device(
    adapter: &IDXGIAdapter,
) -> Result<(ID3D11Device, ID3D11DeviceContext), CaptureError> {
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device as *mut _),
            None,
            Some(&mut context as *mut _),
        )?;
    }
    match (device, context) {
        (Some(d), Some(c)) => Ok((d, c)),
        _ => Err(CaptureError::Message(
            "D3D11CreateDevice returned no device".into(),
        )),
    }
}
