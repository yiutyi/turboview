//! Experimental Windows Imaging Component (WIC) decoder.
//!
//! This module is deliberately opt-in. It asks WIC to scale before copying pixels
//! into the application buffer, avoiding a full-resolution RGBA allocation for
//! formats supported by the installed Windows codecs.

#![cfg(all(target_os = "windows", feature = "wic-decode"))]

use anyhow::{Context, Result, bail};
use std::{os::windows::ffi::OsStrExt, path::Path};
use windows::{
    core::PCWSTR,
    Win32::{
        Graphics::Imaging::{
            CLSID_WICImagingFactory, IWICImagingFactory, WICBitmapDitherTypeNone,
            WICBitmapInterpolationModeFant, WICBitmapPaletteTypeCustom,
            WICDecodeMetadataCacheOnDemand, GUID_WICPixelFormat32bppBGRA,
        },
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_MULTITHREADED,
        },
    },
};

/// Decodes frame zero and scales it inside WIC before allocating the BGRA output.
/// The caller must invoke this from a worker thread, not the GPUI render thread.
pub fn decode_scaled(path: &Path, max_dim: u32) -> Result<(u32, u32, Vec<u8>)> {
    if max_dim == 0 {
        bail!("WIC max_dim must be greater than zero");
    }

    // WIC/COM objects are apartment-bound. Initialize this worker as MTA and
    // balance successful initialization on every exit path.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .context("initialize COM MTA for WIC")?;
    }
    let _com = ComApartment;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let factory: IWICImagingFactory = unsafe {
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
            .context("create WIC imaging factory")?
    };
    let decoder = unsafe {
        factory
            .CreateDecoderFromFilename(
                PCWSTR(wide.as_ptr()),
                None,
                0x8000_0000, // GENERIC_READ
                WICDecodeMetadataCacheOnDemand,
            )
            .context("open image with WIC")?
    };
    let frame = unsafe { decoder.GetFrame(0).context("get WIC frame zero")? };

    let (src_width, src_height) = unsafe {
        let mut width = 0;
        let mut height = 0;
        frame.GetSize(&mut width, &mut height).context("read WIC frame size")?;
        (width, height)
    };
    if src_width == 0 || src_height == 0 {
        bail!("WIC returned an empty image");
    }

    let longest = src_width.max(src_height);
    let (out_width, out_height) = if longest > max_dim {
        let scale_num = max_dim as u64;
        let scale_den = longest as u64;
        let width = ((src_width as u64 * scale_num + scale_den / 2) / scale_den).max(1);
        let height = ((src_height as u64 * scale_num + scale_den / 2) / scale_den).max(1);
        (width as u32, height as u32)
    } else {
        (src_width, src_height)
    };

    let scaler = unsafe { factory.CreateBitmapScaler().context("create WIC scaler")? };
    unsafe {
        scaler
            .Initialize(
                &frame,
                out_width,
                out_height,
                WICBitmapInterpolationModeFant,
            )
            .context("initialize WIC native scaler")?;
    }

    let converter = unsafe {
        factory
            .CreateFormatConverter()
            .context("create WIC format converter")?
    };
    unsafe {
        converter
            .Initialize(
                &scaler,
                &GUID_WICPixelFormat32bppBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .context("convert WIC output to BGRA")?;
    }

    let stride = out_width
        .checked_mul(4)
        .context("WIC output stride overflow")?;
    let byte_len = (stride as usize)
        .checked_mul(out_height as usize)
        .context("WIC output buffer size overflow")?;
    let mut pixels = vec![0u8; byte_len];
    unsafe {
        converter
            .CopyPixels(std::ptr::null(), stride, &mut pixels)
            .context("copy scaled WIC pixels")?;
    }

    Ok((out_width, out_height, pixels))
}

struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}
