//! Pre-generated NVENC FFI bindings for `nvEncodeAPI.h` (NVENC API 12.1).
//!
//! Vendored from the `nvidia-video-codec-sdk` 0.4.0 crate, `src/sys/windows_sys` plus its
//! `guid.rs` and `version.rs` (MIT, see `LICENSE-nvidia-video-codec-sdk.txt`). Only the
//! declarations are used: the NVENC entry points are loaded at run time from
//! `nvEncodeAPI64.dll`, so nothing is linked and no CUDA toolkit is needed.

#[allow(dead_code)]
#[rustfmt::skip]
pub mod guid;
#[allow(dead_code)]
#[rustfmt::skip]
pub mod version;
#[allow(warnings)]
#[rustfmt::skip]
pub mod nvEncodeAPI;
