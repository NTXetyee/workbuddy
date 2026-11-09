//! BGRA -> NV12 (or BGRA) conversion and scaling with the D3D11 VideoProcessor, plus the
//! optional GPU timestamp measurement used by `take_stats`.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::{size_of, ManuallyDrop};
use std::ptr;
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Foundation::{RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11DeviceContext, ID3D11Query, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_QUERY,
    D3D11_QUERY_DATA_TIMESTAMP_DISJOINT, D3D11_QUERY_DESC, D3D11_QUERY_TIMESTAMP,
    D3D11_QUERY_TIMESTAMP_DISJOINT, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

use crate::{win, EncodeError, EncodeStats, EncoderConfig, ID3D11Device, ID3D11Texture2D};

const RING_LEN: usize = 3;
const MAX_INPUT_VIEWS: usize = 8;

/// Format of the converter's output textures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutFormat {
    /// NV12, BT.709 limited range (Media Foundation encoder input).
    Nv12,
    /// BGRA8, full range RGB (NVENC input after a resize).
    Bgra,
}

/// Query sets in flight for the GPU timer. Each Blt uses one set until its results are read.
const GPU_QUERY_SETS: usize = 8;

/// Three timestamp queries: the disjoint clock-frequency query and the begin/end timestamps.
struct QuerySet {
    disjoint: ID3D11Query,
    begin: ID3D11Query,
    end: ID3D11Query,
}

/// Measures the GPU time of the VideoProcessorBlt with D3D11 timestamp queries.
///
/// Results are read without blocking (`GetData` with no flags). A set that is not ready yet
/// stays pending and is read on a later call. When every set is pending, the Blt is simply not
/// timed, so measurement never stalls the GPU or the encode loop.
struct GpuTimer {
    ctx: ID3D11DeviceContext,
    free: Vec<QuerySet>,
    pending: VecDeque<QuerySet>,
    total: Duration,
    samples: u64,
}

fn make_query(device: &ID3D11Device, kind: D3D11_QUERY) -> Option<ID3D11Query> {
    let desc = D3D11_QUERY_DESC {
        Query: kind,
        MiscFlags: 0,
    };
    let mut q: Option<ID3D11Query> = None;
    unsafe { device.CreateQuery(&desc, Some(&mut q as *mut _)) }.ok()?;
    q
}

/// Reads one timestamp. Zero means "not ready" (a real counter value is never zero here).
fn read_timestamp(ctx: &ID3D11DeviceContext, q: &ID3D11Query) -> Option<u64> {
    let mut v = 0u64;
    unsafe {
        ctx.GetData(
            q,
            Some(&mut v as *mut u64 as *mut c_void),
            size_of::<u64>() as u32,
            0,
        )
    }
    .ok()?;
    (v != 0).then_some(v)
}

impl GpuTimer {
    /// Creates the query pool. If queries cannot be created, timing is simply disabled.
    fn new(device: &ID3D11Device, ctx: ID3D11DeviceContext) -> Self {
        let mut free = Vec::with_capacity(GPU_QUERY_SETS);
        for _ in 0..GPU_QUERY_SETS {
            let disjoint = make_query(device, D3D11_QUERY_TIMESTAMP_DISJOINT);
            let begin = make_query(device, D3D11_QUERY_TIMESTAMP);
            let end = make_query(device, D3D11_QUERY_TIMESTAMP);
            match (disjoint, begin, end) {
                (Some(disjoint), Some(begin), Some(end)) => free.push(QuerySet {
                    disjoint,
                    begin,
                    end,
                }),
                _ => break,
            }
        }
        GpuTimer {
            ctx,
            free,
            pending: VecDeque::new(),
            total: Duration::ZERO,
            samples: 0,
        }
    }

    /// Starts a timed region. Returns `None` when no query set is free.
    fn begin(&mut self) -> Option<QuerySet> {
        let set = self.free.pop()?;
        unsafe {
            self.ctx.Begin(&set.disjoint);
            self.ctx.End(&set.begin);
        }
        Some(set)
    }

    /// Ends a timed region started by [`GpuTimer::begin`].
    fn end(&mut self, set: QuerySet) {
        unsafe {
            self.ctx.End(&set.end);
            self.ctx.End(&set.disjoint);
        }
        self.pending.push_back(set);
    }

    /// Collects every pending sample whose results are ready, in submission order.
    fn poll(&mut self) {
        while let Some(front) = self.pending.front() {
            let mut dj = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
            let got = unsafe {
                self.ctx.GetData(
                    &front.disjoint,
                    Some(&mut dj as *mut _ as *mut c_void),
                    size_of::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>() as u32,
                    0,
                )
            };
            if got.is_err() {
                // Device trouble: drop this sample and keep the set for reuse.
                if let Some(set) = self.pending.pop_front() {
                    self.free.push(set);
                }
                continue;
            }
            if dj.Frequency == 0 {
                break; // not ready; queries complete in order, so stop here
            }
            let front = self.pending.front().expect("checked above");
            let begin = read_timestamp(&self.ctx, &front.begin);
            let end = read_timestamp(&self.ctx, &front.end);
            let set = self.pending.pop_front().expect("checked above");
            if let (Some(b), Some(e), false) = (begin, end, dj.Disjoint.as_bool()) {
                if e >= b {
                    self.total += Duration::from_secs_f64((e - b) as f64 / dj.Frequency as f64);
                    self.samples += 1;
                }
            }
            self.free.push(set);
        }
    }

    /// Returns the GPU time and sample count since the previous call and resets them.
    fn take(&mut self) -> (Duration, u64) {
        let out = (self.total, self.samples);
        self.total = Duration::ZERO;
        self.samples = 0;
        out
    }
}

/// BGRA -> NV12 (or BGRA) conversion and scaling with the D3D11 VideoProcessor.
pub(crate) struct Converter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext,
    gpu: GpuTimer,
    out_w: u32,
    out_h: u32,
    fps: u32,
    out_format: OutFormat,
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
    pub(crate) fn new(
        device: &ID3D11Device,
        cfg: &EncoderConfig,
        out_format: OutFormat,
    ) -> Result<Self, EncodeError> {
        let vdev: ID3D11VideoDevice = win("QI ID3D11VideoDevice", device.cast())?;
        let ctx: ID3D11DeviceContext = win("ID3D11Device::GetImmediateContext", unsafe {
            device.GetImmediateContext()
        })?;
        let vctx: ID3D11VideoContext = win("QI ID3D11VideoContext", ctx.cast())?;

        let (format, bind) = match out_format {
            OutFormat::Nv12 => (DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET.0 as u32),
            OutFormat::Bgra => (
                DXGI_FORMAT_B8G8R8A8_UNORM,
                (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ),
        };
        let mut ring_tex = Vec::with_capacity(RING_LEN);
        for _ in 0..RING_LEN {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: cfg.width,
                Height: cfg.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: bind,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            win("CreateTexture2D(converter ring)", unsafe {
                device.CreateTexture2D(&desc, None, Some(&mut tex))
            })?;
            ring_tex
                .push(tex.ok_or_else(|| EncodeError::Config("ring texture not created".into()))?);
        }

        let gpu = GpuTimer::new(device, ctx);
        Ok(Converter {
            vdev,
            vctx,
            gpu,
            out_w: cfg.width,
            out_h: cfg.height,
            fps: cfg.fps,
            out_format,
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

        // Input: full-range RGB. Output: BT.709 limited-range NV12, or full-range RGB for BGRA.
        let in_cs = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: 1 << 4, // Nominal_Range = 0-255, RGB_Range = full
        };
        let out_cs = match self.out_format {
            OutFormat::Nv12 => D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: (1 << 2) | (2 << 4), // YCbCr_Matrix = BT.709, Nominal_Range = 16-235
            },
            OutFormat::Bgra => in_cs,
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

    /// Converts and scales `src` into the next ring texture. Returns the texture and the CPU
    /// submit time of the VideoProcessorBlt.
    pub(crate) fn convert(
        &mut self,
        src: &ID3D11Texture2D,
    ) -> Result<(ID3D11Texture2D, Duration), EncodeError> {
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
        self.gpu.poll();
        let timing = self.gpu.begin();
        let blt_started = Instant::now();
        let r = unsafe {
            self.vctx.VideoProcessorBlt(
                proc,
                &self.ring_views[idx],
                0,
                std::slice::from_ref(&stream),
            )
        };
        let blt = blt_started.elapsed();
        if let Some(set) = timing {
            self.gpu.end(set);
        }
        // The stream holds a manually managed reference to the input view.
        unsafe { ManuallyDrop::drop(&mut stream.pInputSurface) };
        win("ID3D11VideoContext::VideoProcessorBlt", r)?;
        Ok((self.ring_tex[idx].clone(), blt))
    }

    /// Fills the GPU-time fields of `stats` from the timestamp queries.
    pub(crate) fn fill_gpu_stats(&mut self, stats: &mut EncodeStats) {
        self.gpu.poll();
        let (gpu_total, gpu_samples) = self.gpu.take();
        stats.gpu_convert_total = gpu_total;
        stats.gpu_samples = gpu_samples;
    }
}
