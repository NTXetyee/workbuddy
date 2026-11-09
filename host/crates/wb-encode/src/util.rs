//! Helpers shared by the encoder backends.

use windows::core::Interface;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

use crate::{win, EncodeError, EncoderConfig, ID3D11Device};

/// Annex-B start code.
pub(crate) const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// PCI vendor id of NVIDIA.
pub(crate) const NVIDIA_VENDOR_ID: u32 = 0x10DE;

/// Packs two 32-bit values into one 64-bit value (`hi` in the upper half).
pub(crate) fn pack2(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

pub(crate) fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// Rejects configurations the encoders cannot handle.
pub(crate) fn validate(cfg: &EncoderConfig) -> Result<(), EncodeError> {
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

/// Identity of the DXGI adapter a device was created on.
pub(crate) struct AdapterInfo {
    pub name: String,
    pub luid: u64,
    pub vendor: u32,
}

pub(crate) fn adapter_info(device: &ID3D11Device) -> Result<AdapterInfo, EncodeError> {
    let dxgi: IDXGIDevice = win("QI IDXGIDevice", device.cast())?;
    let adapter = win("IDXGIDevice::GetAdapter", unsafe { dxgi.GetAdapter() })?;
    let desc = win("IDXGIAdapter::GetDesc", unsafe { adapter.GetDesc() })?;
    Ok(AdapterInfo {
        name: wide_to_string(&desc.Description),
        luid: pack2(desc.AdapterLuid.HighPart as u32, desc.AdapterLuid.LowPart),
        vendor: desc.VendorId,
    })
}

/// NAL unit types (`nal_unit_type`) of an Annex-B buffer.
pub(crate) fn nal_types(data: &[u8]) -> Vec<u8> {
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
