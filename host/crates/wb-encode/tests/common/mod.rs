//! Shared test helpers: hardware D3D11 device and a synthetic animated BGRA source.
//! Included from tests/encode.rs and examples/dump.rs via `#[path]`.
#![allow(dead_code)]

use std::ffi::c_void;

use wb_encode::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11DeviceContext, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

/// Creates a hardware D3D11 device with BGRA and video support.
pub fn create_device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
    let mut dev: Option<ID3D11Device> = None;
    let mut ctx: Option<ID3D11DeviceContext> = None;
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    let r = unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE(std::ptr::null_mut()),
            flags,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut dev as *mut _),
            None,
            Some(&mut ctx as *mut _),
        )
    };
    if let Err(e) = r {
        eprintln!("D3D11CreateDevice failed: {e}");
        return None;
    }
    Some((dev?, ctx?))
}

/// A GPU BGRA8 texture whose contents are rewritten each frame with a moving pattern.
pub struct Source {
    tex: ID3D11Texture2D,
    w: u32,
    h: u32,
    buf: Vec<u8>,
}

impl Source {
    pub fn new(device: &ID3D11Device, w: u32, h: u32) -> Source {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
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
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex as *mut _)) }
            .expect("CreateTexture2D(source)");
        Source {
            tex: tex.expect("source texture"),
            w,
            h,
            buf: vec![0u8; (w * h * 4) as usize],
        }
    }

    /// Writes frame `frame` (a colour gradient plus a sweeping vertical bar) into the texture.
    pub fn upload(&mut self, ctx: &ID3D11DeviceContext, frame: u32) {
        let (w, h) = (self.w as usize, self.h as usize);
        let bar = (frame as usize * 9) % w;
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                let on_bar = x.abs_diff(bar) < 24;
                self.buf[i] = (x as u32 + frame * 4) as u8;
                self.buf[i + 1] = if on_bar {
                    255
                } else {
                    (y as u32 + frame * 2) as u8
                };
                self.buf[i + 2] = ((x + y) / 2) as u8 ^ (frame as u8).wrapping_mul(8);
                self.buf[i + 3] = 255;
            }
        }
        unsafe {
            ctx.UpdateSubresource(
                &self.tex,
                0,
                None,
                self.buf.as_ptr() as *const c_void,
                self.w * 4,
                0,
            )
        };
    }

    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.tex
    }
}
