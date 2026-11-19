//! Real decoding: libjpeg-turbo (SIMD, runtime CPU dispatch) for JPEG,
//! the `image` crate for everything else.
//!
//! This is the same libjpeg-turbo source the Windows build uses, so the
//! SIMD path and the 1/2 · 1/4 · 1/8 scaled IDCT behaviour are identical
//! across platforms.

use anyhow::{bail, Context, Result};
use std::ffi::{c_char, c_int, c_ulong, c_void, CStr};
use std::path::Path;

pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    /// RGBA, row-major, 4 bytes per pixel.
    pub rgba: Vec<u8>,
}

impl Bitmap {
    pub fn to_u32(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity((self.width * self.height) as usize);
        for px in self.rgba.chunks_exact(4) {
            out.push(((px[0] as u32) << 16) | ((px[1] as u32) << 8) | (px[2] as u32));
        }
        out
    }
}

#[allow(non_snake_case)]
mod tj {
    use super::*;

    pub const TJPF_RGBA: c_int = 7;
    pub const TJFLAG_FASTDCT: c_int = 2048;
    pub const TJFLAG_FASTUPSAMPLE: c_int = 256;

    extern "C" {
        pub fn tjInitDecompress() -> *mut c_void;
        pub fn tjDestroy(handle: *mut c_void) -> c_int;
        pub fn tjDecompressHeader3(
            handle: *mut c_void,
            jpeg_buf: *const u8,
            jpeg_size: c_ulong,
            width: *mut c_int,
            height: *mut c_int,
            subsamp: *mut c_int,
            colorspace: *mut c_int,
        ) -> c_int;
        pub fn tjDecompress2(
            handle: *mut c_void,
            jpeg_buf: *const u8,
            jpeg_size: c_ulong,
            dst_buf: *mut u8,
            width: c_int,
            pitch: c_int,
            height: c_int,
            pixel_format: c_int,
            flags: c_int,
        ) -> c_int;
        pub fn tjGetErrorStr2(handle: *mut c_void) -> *const c_char;
    }
}

struct Handle(*mut c_void);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { tj::tjDestroy(self.0) };
        }
    }
}

fn last_error(handle: *mut c_void) -> String {
    unsafe {
        let p = tj::tjGetErrorStr2(handle);
        if p.is_null() {
            "unknown".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().to_string()
        }
    }
}

pub fn is_jpeg(data: &[u8], hint: Option<&str>) -> bool {
    if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
        return true;
    }
    matches!(
        hint.map(|h| h.to_ascii_lowercase()).as_deref(),
        Some("jpg") | Some("jpeg")
    )
}

/// Header-only read: lets the UI know whether a sharper decode is worth
/// starting in the background without touching pixels.
pub fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let handle = unsafe { tj::tjInitDecompress() };
    if handle.is_null() {
        return None;
    }
    let guard = Handle(handle);
    let _ = &guard;
    let (mut w, mut h, mut subsamp, mut colorspace) = (0, 0, 0, 0);
    let rc = unsafe {
        tj::tjDecompressHeader3(
            handle,
            data.as_ptr(),
            data.len() as c_ulong,
            &mut w,
            &mut h,
            &mut subsamp,
            &mut colorspace,
        )
    };
    if rc != 0 || w <= 0 || h <= 0 {
        return None;
    }
    Some((w as u32, h as u32))
}

/// Picks a scaled-IDCT factor that libjpeg-turbo accelerates with SIMD.
/// 3/8, 5/8 … fall back to the C path and are slower than a full decode,
/// so we prefer dropping a whole step.
fn pick_scale(width: u32, height: u32, max_dim: u32) -> u32 {
    let longest = width.max(height);
    if longest <= max_dim {
        return 8;
    }
    for m in [4u32, 2, 1] {
        if (longest * m).div_ceil(8) <= max_dim {
            return m;
        }
    }
    1
}

pub fn decode_jpeg(jpeg: &[u8], max_dim: u32, fast: bool) -> Result<Bitmap> {
    if jpeg.is_empty() {
        bail!("空 JPEG 数据");
    }
    let handle = unsafe { tj::tjInitDecompress() };
    if handle.is_null() {
        bail!("tjInitDecompress 失败");
    }
    let guard = Handle(handle);
    let _ = &guard;

    let (mut w, mut h, mut subsamp, mut colorspace) = (0 as c_int, 0 as c_int, 0 as c_int, 0 as c_int);
    let rc = unsafe {
        tj::tjDecompressHeader3(
            handle,
            jpeg.as_ptr(),
            jpeg.len() as c_ulong,
            &mut w,
            &mut h,
            &mut subsamp,
            &mut colorspace,
        )
    };
    if rc != 0 || w <= 0 || h <= 0 {
        bail!("不是有效的 JPEG ({})", last_error(handle));
    }

    let m = pick_scale(w as u32, h as u32, max_dim);
    let sw = (w as u32 * m).div_ceil(8);
    let sh = (h as u32 * m).div_ceil(8);

    let mut pixels = vec![0u8; (sw * sh * 4) as usize];
    let mut flags = 0;
    if fast {
        flags |= tj::TJFLAG_FASTDCT | tj::TJFLAG_FASTUPSAMPLE;
    }
    let rc = unsafe {
        tj::tjDecompress2(
            handle,
            jpeg.as_ptr(),
            jpeg.len() as c_ulong,
            pixels.as_mut_ptr(),
            sw as c_int,
            0,
            sh as c_int,
            tj::TJPF_RGBA,
            flags,
        )
    };
    if rc != 0 {
        bail!("JPEG 解码失败 ({})", last_error(handle));
    }

    // EXIF orientation: phone shots must not show up sideways.
    let orientation = jpeg_orientation(jpeg);
    let mut bmp = Bitmap {
        width: sw,
        height: sh,
        rgba: pixels,
    };
    if orientation > 1 {
        bmp = apply_orientation(bmp, orientation);
    }
    Ok(bmp)
}

/// Reads the EXIF orientation tag (0x0112) out of APP1, defaulting to 1.
pub fn jpeg_orientation(data: &[u8]) -> u32 {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return 1;
    }
    let mut pos = 2usize;
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            pos += 1;
            continue;
        }
        let marker = data[pos + 1];
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            pos += 2;
            continue;
        }
        if marker == 0xDA || marker == 0xD9 {
            break;
        }
        let len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        if len < 2 || pos + 2 + len > data.len() {
            break;
        }
        if marker == 0xE1 {
            let seg = &data[pos + 4..pos + 2 + len];
            if seg.len() > 6 && &seg[..6] == b"Exif\0\0" {
                if let Some(o) = parse_tiff_orientation(&seg[6..]) {
                    return o;
                }
            }
        }
        pos += 2 + len;
    }
    1
}

fn parse_tiff_orientation(tiff: &[u8]) -> Option<u32> {
    if tiff.len() < 8 {
        return None;
    }
    let big = match &tiff[..2] {
        b"II" => false,
        b"MM" => true,
        _ => return None,
    };
    let rd16 = |p: usize| -> Option<u16> {
        let b = tiff.get(p..p + 2)?;
        Some(if big {
            u16::from_be_bytes([b[0], b[1]])
        } else {
            u16::from_le_bytes([b[0], b[1]])
        })
    };
    let rd32 = |p: usize| -> Option<u32> {
        let b = tiff.get(p..p + 4)?;
        Some(if big {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        })
    };
    let ifd = rd32(4)? as usize;
    let count = rd16(ifd)? as usize;
    for i in 0..count {
        let entry = ifd + 2 + i * 12;
        let tag = rd16(entry)?;
        if tag == 0x0112 {
            let value = rd16(entry + 8)?;
            return Some(value as u32);
        }
    }
    None
}

pub fn apply_orientation(bmp: Bitmap, o: u32) -> Bitmap {
    let (w, h) = (bmp.width as usize, bmp.height as usize);
    if o < 2 || o > 8 {
        return bmp;
    }
    let swap = matches!(o, 5..=8);
    let (dw, dh) = if swap { (h, w) } else { (w, h) };
    let src = bmp.rgba;
    let mut out = vec![0u8; src.len()];
    for y in 0..dh {
        for x in 0..dw {
            let (sx, sy) = match o {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (y, h - 1 - x),
                7 => (w - 1 - y, h - 1 - x),
                _ => (w - 1 - y, x),
            };
            let si = (sy * w + sx) * 4;
            let di = (y * dw + x) * 4;
            out[di..di + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    Bitmap {
        width: dw as u32,
        height: dh as u32,
        rgba: out,
    }
}

pub fn decode_other(data: &[u8], max_dim: u32) -> Result<Bitmap> {
    let img = image::load_from_memory(data).context("图片解码失败")?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let longest = w.max(h);
    if longest > max_dim {
        let scale = max_dim as f32 / longest as f32;
        let nw = ((w as f32 * scale).round() as u32).max(1);
        let nh = ((h as f32 * scale).round() as u32).max(1);
        let small = image::imageops::resize(&rgba, nw, nh, image::imageops::FilterType::Triangle);
        return Ok(Bitmap {
            width: nw,
            height: nh,
            rgba: small.into_raw(),
        });
    }
    Ok(Bitmap {
        width: w,
        height: h,
        rgba: rgba.into_raw(),
    })
}

pub fn decode_bytes(data: &[u8], hint: Option<&str>, max_dim: u32, fast: bool) -> Result<Bitmap> {
    if is_jpeg(data, hint) {
        decode_jpeg(data, max_dim, fast)
    } else {
        decode_other(data, max_dim)
    }
}

pub fn decode_file(path: &Path, max_dim: u32, fast: bool) -> Result<Bitmap> {
    let data = std::fs::read(path)
        .with_context(|| format!("读取失败: {}", path.display()))?;
    let hint = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase());
    decode_bytes(&data, hint.as_deref(), max_dim, fast)
        .with_context(|| format!("解码失败: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const JPG64: &[u8] = include_bytes!("../assets/test-64.jpg");

    #[test]
    fn decodes_real_jpeg() {
        let bmp = decode_jpeg(JPG64, 64, false).expect("decode");
        assert_eq!((bmp.width, bmp.height), (64, 64));
        assert_eq!(bmp.rgba.len(), 64 * 64 * 4);
        // opaque alpha everywhere
        assert!(bmp.rgba.chunks_exact(4).all(|p| p[3] == 255));
    }

    #[test]
    fn reads_jpeg_dimensions_without_decoding() {
        assert_eq!(jpeg_dimensions(JPG64), Some((64, 64)));
        assert_eq!(jpeg_dimensions(b"not a jpeg"), None);
    }

    #[test]
    fn picks_scaled_idct_when_too_large() {
        let bmp = decode_jpeg(JPG64, 32, true).expect("decode");
        assert!(bmp.width <= 32 && bmp.height <= 32);
        assert!(bmp.width > 0 && bmp.height > 0);
    }

    fn with_orientation(jpg: &[u8], value: u16) -> Vec<u8> {
        // Minimal little-endian TIFF with a single IFD0 entry: tag 0x0112.
        let mut tiff = Vec::new();
        tiff.extend(b"II");
        tiff.extend(42u16.to_le_bytes());
        tiff.extend(8u32.to_le_bytes());
        tiff.extend(1u16.to_le_bytes()); // one entry
        tiff.extend(0x0112u16.to_le_bytes());
        tiff.extend(3u16.to_le_bytes()); // SHORT
        tiff.extend(1u32.to_le_bytes()); // count
        tiff.extend(value.to_le_bytes());
        tiff.extend(0u16.to_le_bytes());
        tiff.extend(0u32.to_le_bytes()); // next IFD

        let mut payload = b"Exif\0\0".to_vec();
        payload.extend(tiff);
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend(((payload.len() + 2) as u16).to_be_bytes());
        out.extend(payload);
        out.extend(&jpg[2..]);
        out
    }

    #[test]
    fn reads_exif_orientation() {
        assert_eq!(jpeg_orientation(JPG64), 1);
        let tagged = with_orientation(JPG64, 6);
        assert_eq!(jpeg_orientation(&tagged), 6);
        let tagged = with_orientation(JPG64, 3);
        assert_eq!(jpeg_orientation(&tagged), 3);
    }

    #[test]
    fn applies_exif_rotation() {
        // 2x1: red, green -> rotate CW -> 1x2
        let bmp = Bitmap {
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
        };
        let out = apply_orientation(bmp, 6);
        assert_eq!((out.width, out.height), (1, 2));
        assert_eq!(&out.rgba[0..4], &[255, 0, 0, 255]);
        assert_eq!(&out.rgba[4..8], &[0, 255, 0, 255]);
    }

    #[test]
    fn orientation_one_is_identity() {
        let bmp = Bitmap {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 255],
        };
        let out = apply_orientation(bmp, 1);
        assert_eq!(out.rgba, vec![1, 2, 3, 255]);
    }
}
