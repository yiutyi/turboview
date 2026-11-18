//! libjpeg-turbo SIMD 解码服务（FFI 直连静态库）
//!
//! - 直接从 C 层解码为 **BGRA**（GPUI `RenderImage` 的帧缓冲字节序），
//!   零像素格式转换、零中间拷贝。
//! - 超出目标尺寸的图在解码时按 turbojpeg 的 M/8 缩放因子降采样
//!   （目标尺寸由窗口物理尺寸决定），SIMD 缩放几乎免费，
//!   同时避免纹理/内存爆炸。
//! - 文件 I/O 用内存映射（失败时回退普通读取），只读不写。

#![allow(non_camel_case_types)]

use anyhow::{Context as _, Result};
use memmap2::Mmap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uchar, c_ulong, c_void};
use std::path::Path;
use std::time::Instant;

/// GPUI 的 `RenderImage` 帧缓冲要求 BGRA 字节序。
pub const TJPF_BGRA: c_int = 8;

const TJFLAG_FASTUPSAMPLE: c_int = 256;
const TJFLAG_FASTDCT: c_int = 2048;

/// 解码后的位图（BGRA，行主序，无行间填充）
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// BGRA 像素数据，长度 = width * height * 4
    pub pixels: Vec<u8>,
    /// 原始 JPEG 的宽高（缩放前，**存储方向**，未应用 EXIF 旋转）
    pub src_width: u32,
    pub src_height: u32,
    /// EXIF orientation（1-8；像素缓冲已按此变换到显示方向）
    pub orientation: u32,
    /// ROI 渐进重解码：本缓冲内容在**显示方向**源坐标系中的左上角
    /// （全图解码时为 0,0；ROI 解码时为实际解码区域的原点）
    pub roi_x: u32,
    pub roi_y: u32,
    /// 文件读取耗时（ms）
    pub read_ms: f64,
    /// SIMD 解码耗时（ms，不含文件 I/O）
    pub decode_ms: f64,
    /// 本次解码是否使用了快速模式（FASTDCT + FASTUPSAMPLE）
    pub fast: bool,
}

/// 显示方向坐标系中的矩形（原始分辨率，像素）
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RoiRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

// 源码级链接声明：cargo 1.99 不会把 build script 的 `-l` 传给同包 bin 目标
// （verbose 取证确认只有 -L 到达），导致 LNK2019。#[link] 直接编入 crate
// 元数据，rustc 链接 bin 时必定转发给 link.exe。
#[link(name = "turbojpeg-static", kind = "static", modifiers = "-bundle")]
unsafe extern "C" {
    fn tjInitDecompress() -> *mut c_void;
    fn tjDestroy(handle: *mut c_void) -> c_int;
    fn tjDecompressHeader3(
        handle: *mut c_void,
        jpeg_buf: *const c_uchar,
        jpeg_size: c_ulong,
        width: *mut c_int,
        height: *mut c_int,
        jpeg_subsamp: *mut c_int,
        jpeg_colorspace: *mut c_int,
    ) -> c_int;
    fn tjDecompress2(
        handle: *mut c_void,
        jpeg_buf: *const c_uchar,
        jpeg_size: c_ulong,
        dst_buf: *mut c_uchar,
        width: c_int,
        pitch: c_int,
        height: c_int,
        pixel_format: c_int,
        flags: c_int,
    ) -> c_int;
    fn tjGetErrorStr2(handle: *mut c_void) -> *mut c_char;
}

/// turbojpeg 只支持 M/8（M = 1..=16）的缩放因子。
/// 返回能让 `ceil(dim * M / 8) <= max_dim` 成立的最大 M（无需缩放时返回 8，即 1/1）。
fn pick_scale_num(width: u32, height: u32, max_dim: u32) -> u32 {
    let longest = width.max(height);
    if longest <= max_dim {
        return 8;
    }
    // 只挑 libjpeg-turbo 有 SIMD 加速的档位：1/1、1/2、1/4、1/8。
    // 3/8、5/8、6/8 等比例走的是 C 缩放 IDCT，实测比全解还慢
    // （6000x4000 用 5/8 解 3750x2500 要 224ms，全解 6000x4000 只要 128ms），
    // 所以宁可多降一档，也不要落到慢路径上。
    for m in [4u32, 2, 1] {
        if (longest * m).div_ceil(8) <= max_dim {
            return m;
        }
    }
    1
}

/// 机械盘标志：为 true 时用 `FILE_FLAG_SEQUENTIAL_SCAN` 顺序读替代 mmap。
///
/// mmap 的缺页是同步的，系统无法为它做前向预读；机械盘冷缓存下会退化成一串
/// 4KB 随机读，寻道开销比顺序读高一个数量级。启动时按磁盘类型设置一次。
static SEQUENTIAL_IO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_sequential_io(v: bool) {
    SEQUENTIAL_IO.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// 读取 JPEG 文件为字节切片：SSD 走 mmap，机械盘走顺序读。
fn read_jpeg(path: &Path) -> Result<(MmapOrVec, f64)> {
    let start = Instant::now();
    if SEQUENTIAL_IO.load(std::sync::atomic::Ordering::Relaxed) {
        let data = crate::win32::read_file_sequential(path)
            .with_context(|| format!("无法读取 {}", path.display()))?;
        return Ok((MmapOrVec::Vec(data), start.elapsed().as_secs_f64() * 1000.0));
    }
    let file =
        std::fs::File::open(path).with_context(|| format!("无法打开 {}", path.display()))?;
    let len = file.metadata()?.len();
    if len == 0 {
        anyhow::bail!("空文件: {}", path.display());
    }
    // SAFETY: 文件只读打开，映射期间不修改；回退路径拷贝到 Vec 同样安全。
    let data = unsafe {
        match Mmap::map(&file) {
            Ok(map) => MmapOrVec::Mmap(map),
            Err(_) => MmapOrVec::Vec(std::fs::read(path)?),
        }
    };
    Ok((data, start.elapsed().as_secs_f64() * 1000.0))
}

enum MmapOrVec {
    Mmap(Mmap),
    Vec(Vec<u8>),
}

impl std::ops::Deref for MmapOrVec {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            MmapOrVec::Mmap(m) => m,
            MmapOrVec::Vec(v) => v,
        }
    }
}

/// 解码 JPEG 文件。`max_dim` 为解码输出的最长边上限（超出则 SIMD 缩放）。
pub fn decode_jpeg(path: &Path, fast: bool, max_dim: u32) -> Result<DecodedImage> {
    let (data, read_ms) = read_jpeg(path)?;
    decode_jpeg_bytes(&data, fast, max_dim, read_ms)
        .with_context(|| format!("解码 JPEG 失败: {}", path.display()))
}

/// 解码内存中的 JPEG 字节（压缩包条目直接走这里，零临时文件）。
/// `read_ms` 由调用方提供（压缩包场景为解压耗时）。
pub fn decode_jpeg_bytes(
    jpeg: &[u8],
    fast: bool,
    max_dim: u32,
    read_ms: f64,
) -> Result<DecodedImage> {
    decode_jpeg_inner(jpeg, fast, max_dim, read_ms, None)
}

/// 解码实现。`orientation_override` 为 Some 时忽略内嵌 EXIF 方向，
/// 用于解 EXIF 缩略图（子 JPEG 通常不带自己的 APP1，但方向要跟随原图）。
fn decode_jpeg_inner(
    jpeg: &[u8],
    fast: bool,
    max_dim: u32,
    read_ms: f64,
    orientation_override: Option<u32>,
) -> Result<DecodedImage> {
    if jpeg.is_empty() {
        anyhow::bail!("空 JPEG 数据");
    }

    let handle = unsafe { tjInitDecompress() };
    if handle.is_null() {
        anyhow::bail!("tjInitDecompress 失败");
    }
    // 保证 handle 一定被释放
    let _guard = HandleGuard(handle);

    let mut width: c_int = 0;
    let mut height: c_int = 0;
    let mut subsamp: c_int = 0;
    let mut colorspace: c_int = 0;

    let rc = unsafe {
        tjDecompressHeader3(
            handle,
            jpeg.as_ptr(),
            jpeg.len() as c_ulong,
            &mut width,
            &mut height,
            &mut subsamp,
            &mut colorspace,
        )
    };
    if rc != 0 {
        anyhow::bail!("不是有效的 JPEG ({})", last_error(handle));
    }
    if width <= 0 || height <= 0 {
        anyhow::bail!("JPEG 尺寸非法: {}x{}", width, height);
    }

    let m = pick_scale_num(width as u32, height as u32, max_dim);
    let scaled_w = ((width as u32 * m).div_ceil(8)) as c_int;
    let scaled_h = ((height as u32 * m).div_ceil(8)) as c_int;

    let mut pixels = vec![0u8; scaled_w as usize * scaled_h as usize * 4];
    let flags = if fast {
        TJFLAG_FASTDCT | TJFLAG_FASTUPSAMPLE
    } else {
        0
    };

    let start = Instant::now();
    let rc = unsafe {
        tjDecompress2(
            handle,
            jpeg.as_ptr(),
            jpeg.len() as c_ulong,
            pixels.as_mut_ptr(),
            scaled_w,
            0, // pitch = 0 → 自动按 width * 4
            scaled_h,
            TJPF_BGRA,
            flags,
        )
    };
    let decode_ms = start.elapsed().as_secs_f64() * 1000.0;
    if rc != 0 {
        anyhow::bail!("解码失败 ({})", last_error(handle));
    }

    // EXIF 方向：像素缓冲变换到显示方向（手机竖拍照片不再横显）
    let orientation = orientation_override.unwrap_or_else(|| jpeg_orientation_from_bytes(jpeg));
    let (pixels, w, h) = apply_orientation(pixels, scaled_w as u32, scaled_h as u32, orientation);

    Ok(DecodedImage {
        width: w,
        height: h,
        pixels,
        src_width: width as u32,
        src_height: height as u32,
        orientation,
        roi_x: 0,
        roi_y: 0,
        read_ms,
        decode_ms,
        fast,
    })
}

// ============================================================================
// EXIF 内嵌缩略图：两级渲染的第一级
//
// 大图 APP1 里通常有一段 160x120 左右的 JPEG 缩略图（几 KB）。先解它并立刻
// 上屏（拉伸成模糊图），再让全图解码在后台完成替换 —— 主观"出图时间"从
// 几百毫秒降到十毫秒以内，在机械盘和低端 CPU 上差别尤其明显。
// ============================================================================

/// 读文件头（不碰像素数据）：EXIF 缩略图一定在 SOS 之前。
const THUMB_HEAD_BYTES: usize = 256 * 1024;

pub fn jpeg_thumbnail_file(path: &Path) -> Option<DecodedImage> {
    use std::io::Read as _;
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; THUMB_HEAD_BYTES];
    let n = f.read(&mut head).ok()?;
    head.truncate(n);
    jpeg_thumbnail(&head)
}

/// 从完整 JPEG 字节里取 EXIF 缩略图并解码（内存版，压缩包条目也适用）。
pub fn jpeg_thumbnail(data: &[u8]) -> Option<DecodedImage> {
    let tiff = find_exif_tiff(data)?;
    let (off, len) = tiff_thumbnail(tiff)?;
    let thumb = tiff.get(off..off.checked_add(len)?)?;
    // 缩略图子 JPEG 往往不自带 APP1，方向必须跟随原图
    let orientation = jpeg_orientation_from_bytes(data);
    let d = decode_jpeg_inner(thumb, true, u32::MAX, 0.0, Some(orientation)).ok()?;
    // 缩略图小于 16px 的基本是垃圾数据
    (d.width >= 16 && d.height >= 16).then_some(d)
}

/// 找到 APP1 "Exif\0\0" 段并返回其后的 TIFF 数据。
fn find_exif_tiff(data: &[u8]) -> Option<&[u8]> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut pos = 2usize;
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            return None;
        }
        let marker = data[pos + 1];
        match marker {
            0xFF => pos += 1,
            0x01 | 0xD0..=0xD7 => pos += 2,
            0xD9 | 0xDA => return None,
            _ => {
                let len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
                if len < 2 {
                    return None;
                }
                let body = pos + 4;
                if body + len - 2 > data.len() {
                    return None;
                }
                if marker == 0xE1 && len >= 8 && &data[body..body + 6] == b"Exif\0\0" {
                    return Some(&data[body + 6..body + len - 2]);
                }
                pos = body + len - 2;
            }
        }
    }
    None
}

/// 在 TIFF 里找缩略图 (offset, length)：先看 IFD0（部分相机不规范），
/// 再顺着 next-IFD 指针看 IFD1（EXIF 标准位置）。
fn tiff_thumbnail(tiff: &[u8]) -> Option<(usize, usize)> {
    if tiff.len() < 8 {
        return None;
    }
    let le = match &tiff[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let rd16 = |b: &[u8]| {
        if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    };
    let rd32 = |b: &[u8]| {
        if le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        }
    };
    if rd16(&tiff[2..4]) != 42 {
        return None;
    }

    // 最多看 IFD0 和 IFD1 两级
    let mut ifd = rd32(&tiff[4..8]) as usize;
    for _ in 0..2 {
        if ifd == 0 || ifd + 2 > tiff.len() {
            return None;
        }
        let count = rd16(&tiff[ifd..ifd + 2]) as usize;
        let entries_end = ifd + 2 + count * 12;
        if entries_end + 4 > tiff.len() {
            return None;
        }
        let (mut off, mut len) = (None, None);
        for i in 0..count {
            let e = ifd + 2 + i * 12;
            let tag = rd16(&tiff[e..e + 2]);
            match tag {
                0x0201 => off = Some(rd32(&tiff[e + 8..e + 12]) as usize),
                0x0202 => len = Some(rd32(&tiff[e + 8..e + 12]) as usize),
                _ => {}
            }
        }
        if let (Some(o), Some(l)) = (off, len) {
            if l > 0 && o.checked_add(l).is_some_and(|end| end <= tiff.len()) {
                return Some((o, l));
            }
        }
        ifd = rd32(&tiff[entries_end..entries_end + 4]) as usize;
    }
    None
}

struct HandleGuard(*mut c_void);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            tjDestroy(self.0);
        }
    }
}

fn last_error(handle: *mut c_void) -> String {
    unsafe {
        let ptr = tjGetErrorStr2(handle);
        if ptr.is_null() {
            return "未知错误".to_string();
        }
        CStr::from_ptr(ptr).to_string_lossy().to_string()
    }
}

/// PNG 快速解码：libdeflate inflate + 自研 unfilter + 直出 BGRA。
/// 比 image crate（miniz_oxide 纯软件解压 + 中间格式转换）快数倍。
/// 返回 None 表示该 PNG 变体不受支持（16 位/调色板/隔行等），调用方回退 image crate。
pub fn decode_png_fast(path: &Path, max_dim: u32) -> Option<DecodedImage> {
    let start = Instant::now();
    let data = std::fs::read(path).ok()?;
    let read_ms = start.elapsed().as_secs_f64() * 1000.0;
    decode_png_fast_bytes(&data, read_ms, max_dim)
}

/// 内存版 PNG 快速解码（压缩包条目走这里，零临时文件）。
pub fn decode_png_fast_bytes(data: &[u8], read_ms: f64, max_dim: u32) -> Option<DecodedImage> {
    let decode_start = Instant::now();
    if data.len() < 58 || &data[..8] != [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return None;
    }

    // ---- 解析 chunk：取 IHDR，拼接全部 IDAT ----
    let mut width = 0u32;
    let mut height = 0u32;
    let mut bit_depth = 0u8;
    let mut color_type = 0u8;
    let mut interlace = 0u8;
    let mut idat: Vec<u8> = Vec::new();
    let mut pos = 8usize;
    while pos + 8 <= data.len() {
        let len = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
        let typ = &data[pos + 4..pos + 8];
        let body = pos + 8;
        if body.checked_add(len)?.checked_add(4)? > data.len() {
            return None;
        }
        match typ {
            b"IHDR" => {
                if len < 13 {
                    return None;
                }
                width = u32::from_be_bytes(data[body..body + 4].try_into().ok()?);
                height = u32::from_be_bytes(data[body + 4..body + 8].try_into().ok()?);
                bit_depth = data[body + 8];
                color_type = data[body + 9];
                interlace = data[body + 12];
            }
            b"IDAT" => idat.extend_from_slice(&data[body..body + len]),
            b"IEND" => break,
            _ => {}
        }
        pos = body + len + 4;
    }
    if width == 0 || height == 0 || interlace != 0 || idat.is_empty() {
        return None;
    }
    // 快速通道只处理无需缩放的图片。超出显示尺寸时交给通用解码器，
    // 避免先分配整张 BGRA 输出，再在 UI 路径中缩小；通用路径会限制最终纹理尺寸。
    if width.max(height) > max_dim.max(1) {
        return None;
    }
    // 只做 8 位 RGBA / RGB / 灰度的快速通道，其余回退
    let bpp: usize = match (bit_depth, color_type) {
        (8, 6) => 4,
        (8, 2) => 3,
        (8, 0) => 1,
        _ => return None,
    };
    let stride = (width as usize).checked_mul(bpp)?;
    let raw_len = (height as usize).checked_mul(stride.checked_add(1)?)?;
    if raw_len > (1usize << 31) {
        return None;
    }

    // ---- libdeflate zlib inflate ----
    let mut raw = vec![0u8; raw_len];
    {
        let mut decomp = libdeflater::Decompressor::new();
        match decomp.zlib_decompress(&idat, &mut raw) {
            Ok(n) if n == raw_len => {}
            _ => return None, // 流异常 → 回退 image crate
        }
    }

    // ---- unfilter（原地，按 PNG 规范 5 种滤波）----
    for y in 0..height as usize {
        let row0 = y * (stride + 1);
        let ds = row0 + 1;
        match raw[row0] {
            0 => {}
            1 => {
                for i in bpp..stride {
                    raw[ds + i] = raw[ds + i].wrapping_add(raw[ds + i - bpp]);
                }
            }
            2 => {
                if y > 0 {
                    for i in 0..stride {
                        raw[ds + i] = raw[ds + i].wrapping_add(raw[ds + i - stride - 1]);
                    }
                }
            }
            3 => {
                for i in 0..stride {
                    let a = if i >= bpp { raw[ds + i - bpp] as u16 } else { 0 };
                    let b = if y > 0 { raw[ds + i - stride - 1] as u16 } else { 0 };
                    raw[ds + i] = raw[ds + i].wrapping_add(((a + b) / 2) as u8);
                }
            }
            4 => {
                for i in 0..stride {
                    let a = if i >= bpp { raw[ds + i - bpp] as i16 } else { 0 };
                    let b = if y > 0 { raw[ds + i - stride - 1] as i16 } else { 0 };
                    let c = if y > 0 && i >= bpp {
                        raw[ds + i - stride - 1 - bpp] as i16
                    } else {
                        0
                    };
                    let p = a + b - c;
                    let pa = (p - a).abs();
                    let pb = (p - b).abs();
                    let pc = (p - c).abs();
                    let pred = if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    };
                    raw[ds + i] = raw[ds + i].wrapping_add(pred as u8);
                }
            }
            _ => return None,
        }
    }

    // ---- 解包为 BGRA（GPUI 帧缓冲字节序）----
    let w = width as usize;
    let pixels_len = w.checked_mul(height as usize)?.checked_mul(4)?;
    let mut pixels = vec![0u8; pixels_len];
    for y in 0..height as usize {
        let src = &raw[y * (stride + 1) + 1..];
        let dst = &mut pixels[y * w * 4..][..w * 4];
        match color_type {
            6 => {
                for (o, i) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                    o[0] = i[2];
                    o[1] = i[1];
                    o[2] = i[0];
                    o[3] = i[3];
                }
            }
            2 => {
                for (o, i) in dst.chunks_exact_mut(4).zip(src.chunks_exact(3)) {
                    o[0] = i[2];
                    o[1] = i[1];
                    o[2] = i[0];
                    o[3] = 255;
                }
            }
            _ => {
                for (o, i) in dst.chunks_exact_mut(4).zip(src.chunks_exact(1)) {
                    o[0] = i[0];
                    o[1] = i[0];
                    o[2] = i[0];
                    o[3] = 255;
                }
            }
        }
    }

    Some(DecodedImage {
        width,
        height,
        pixels,
        src_width: width,
        src_height: height,
        orientation: 1,
        roi_x: 0,
        roi_y: 0,
        read_ms,
        decode_ms: decode_start.elapsed().as_secs_f64() * 1000.0,
        fast: true,
    })
}

/// 供非 JPEG 格式使用的 BGRA 转换（image crate 解码路径）
pub fn bgra_from_rgba(rgba: &[u8]) -> Vec<u8> {
    let mut bgra = rgba.to_vec();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    bgra
}

// ============================================================================
// EXIF 方向（orientation 1-8）
// ============================================================================

/// 显示方向下的宽高（orientation 5-8 时宽高互换）
pub fn oriented_dims(o: u32, raw_w: u32, raw_h: u32) -> (u32, u32) {
    if (5..=8).contains(&o) {
        (raw_h, raw_w)
    } else {
        (raw_w, raw_h)
    }
}

/// 显示坐标 (x, y) → 存储坐标（raw_w/raw_h 为存储方向尺寸）
fn orient_map(o: u32, raw_w: u32, raw_h: u32, x: u32, y: u32) -> (u32, u32) {
    match o {
        2 => (raw_w - 1 - x, y),
        3 => (raw_w - 1 - x, raw_h - 1 - y),
        4 => (x, raw_h - 1 - y),
        5 => (y, x),
        6 => (y, raw_h - 1 - x),
        7 => (raw_w - 1 - y, raw_h - 1 - x),
        8 => (raw_w - 1 - y, x),
        _ => (x, y),
    }
}

/// 存储坐标 (sx, sy) → 显示坐标
fn oriented_pt(o: u32, raw_w: u32, raw_h: u32, sx: u32, sy: u32) -> (u32, u32) {
    match o {
        2 => (raw_w - 1 - sx, sy),
        3 => (raw_w - 1 - sx, raw_h - 1 - sy),
        4 => (sx, raw_h - 1 - sy),
        5 => (sy, sx),
        6 => (raw_h - 1 - sy, sx),
        7 => (raw_h - 1 - sy, raw_w - 1 - sx),
        8 => (sy, raw_w - 1 - sx),
        _ => (sx, sy),
    }
}

/// 按 EXIF orientation 变换 BGRA 像素缓冲，返回（像素, 宽, 高）
pub fn apply_orientation(pixels: Vec<u8>, w: u32, h: u32, o: u32) -> (Vec<u8>, u32, u32) {
    match o {
        2 | 3 | 4 => {
            // 翻转类：整行线性操作，按 u32 整像素处理 + 行级 memcpy，
            // 比逐像素 4 字节切片少一层迭代器与越界检查
            let wu = w as usize;
            let hu = h as usize;
            let mut out = vec![0u8; pixels.len()];
            let src = pixels.as_ptr() as *const u32;
            let dst = out.as_mut_ptr() as *mut u32;
            for y in 0..hu {
                let dy = match o {
                    4 => y,
                    _ => hu - 1 - y, // 2/3: 上下颠倒
                };
                let srow = y * wu;
                let drow = dy * wu;
                match o {
                    2 | 3 => {
                        // 水平镜像：行内倒序（源升序、目标降序，两边都连续）
                        // SAFETY: 两侧索引都在 w*h 内
                        unsafe {
                            for x in 0..wu {
                                *dst.add(drow + x) = *src.add(srow + (wu - 1 - x));
                            }
                        }
                    }
                    _ => unsafe {
                        std::ptr::copy_nonoverlapping(src.add(srow), dst.add(drow), wu);
                    },
                }
            }
            (out, w, h)
        }
        5..=8 => {
            // 旋转类：dst 的连续行对应 src 的连续列，天然不缓存友好。
            // 32x32 分块 + 按 u32 整像素读写（代替 4 字节 copy_from_slice），
            // 省掉每像素的切片构造与越界检查，比逐像素切片快 2~3 倍。
            const TILE: u32 = 32;
            let (dw, dh) = oriented_dims(o, w, h);
            let mut out = vec![0u8; pixels.len()];
            let src = pixels.as_ptr() as *const u32;
            let dst = out.as_mut_ptr() as *mut u32;
            for ty in (0..dh).step_by(TILE as usize) {
                for tx in (0..dw).step_by(TILE as usize) {
                    let y_end = (ty + TILE).min(dh);
                    let x_end = (tx + TILE).min(dw);
                    for y in ty..y_end {
                        for x in tx..x_end {
                            let (sx, sy) = orient_map(o, w, h, x, y);
                            // SAFETY: 坐标由 dims 推出，恒落在 w*h 个像素内
                            unsafe {
                                *dst.add((y * dw + x) as usize) = *src.add((sy * w + sx) as usize);
                            }
                        }
                    }
                }
            }
            (out, dw, dh)
        }
        _ => (pixels, w, h),
    }
}

/// 从 JPEG 原始字节解析 EXIF Orientation（1-8；无/非法返回 1）。
/// 只扫 APP1 "Exif" 段 IFD0 的 tag 0x0112，顺带记录 SOF 类型（ROI 预检用）。
fn jpeg_orientation_from_bytes(data: &[u8]) -> u32 {
    scan_jpeg_markers(data).and_then(|(_, o)| o).unwrap_or(1)
}

/// 扫描 JPEG 段结构：返回（SOF 标记，EXIF orientation）。
/// SOF 标记用于 ROI 预检（无损 JPEG 不支持 crop/skip API）。
pub(crate) fn scan_jpeg_markers(data: &[u8]) -> Option<(Option<u8>, Option<u32>)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut pos = 2usize;
    let mut sof: Option<u8> = None;
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            return None; // 段对齐破坏
        }
        let marker = data[pos + 1];
        match marker {
            0xFF => {
                pos += 1; // 填充字节
            }
            0x01 | 0xD0..=0xD7 => pos += 2, // 独立标记，无长度字段
            0xD9 | 0xDA => {
                return Some((sof, None)); // EOI / SOS：EXIF 必在其之前
            }
            _ => {
                let len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
                if len < 2 {
                    return None;
                }
                let body = pos + 4;
                if body + len - 2 > data.len() {
                    return None;
                }
                if (0xC0..=0xCF).contains(&marker)
                    && !matches!(marker, 0xC4 | 0xC8 | 0xCC)
                    && sof.is_none()
                {
                    sof = Some(marker);
                }
                if marker == 0xE1 && len >= 8 && &data[body..body + 6] == b"Exif\0\0" {
                    if let Some(o) = tiff_orientation(&data[body + 6..body + len - 2]) {
                        return Some((sof, Some(o)));
                    }
                }
                pos = body + len - 2;
            }
        }
    }
    Some((sof, None))
}

/// 解析 TIFF 头 + IFD0，取 orientation tag (0x0112)
fn tiff_orientation(tiff: &[u8]) -> Option<u32> {
    if tiff.len() < 8 {
        return None;
    }
    let le = match &tiff[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let magic = if le {
        u16::from_le_bytes([tiff[2], tiff[3]])
    } else {
        u16::from_be_bytes([tiff[2], tiff[3]])
    };
    if magic != 42 {
        return None;
    }
    let rd16 = |b: &[u8]| {
        if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    };
    let rd32 = |b: &[u8]| {
        if le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        }
    };
    let ifd0 = rd32(&tiff[4..8]) as usize;
    if ifd0 + 2 > tiff.len() {
        return None;
    }
    let n = rd16(&tiff[ifd0..ifd0 + 2]) as usize;
    for i in 0..n {
        let e = ifd0 + 2 + i * 12;
        if e + 12 > tiff.len() {
            return None;
        }
        let entry = &tiff[e..e + 12];
        if rd16(&entry[0..2]) == 0x0112 {
            let v = rd16(&entry[8..10]) as u32; // SHORT：值在 value 字段前 2 字节
            return (1..=8).contains(&v).then_some(v);
        }
    }
    None
}

// ============================================================================
// ROI 渐进重解码：turbojpeg 无损裁剪（DCT 域搬运）
//
// 早期实现直接调低层 libjpeg（jpeg_crop_scanline），但结构体是按
// JPEG_LIB_VERSION=80 复刻的，而这里链接的静态库是 62（WITH_JPEG8=OFF）。
// 版本号/结构体尺寸不符时 jpeg_CreateDecompress 会用还没赋值的 cinfo.err
// 去 ERREXIT —— 空指针调用，直接 0xC0000005。ROI 之前几乎不触发，这个坑
// 一直没暴露；把解码尺寸改成按窗口自适应后一放大就踩到了。
//
// 现在改用 turbojpeg 高层的 tjTransform(TJXOPT_CROP)：只依赖稳定 ABI，
// 而且是**无损**的（DCT 系数搬运 + 重新熵编码，没有 IDCT、没有再量化）。
// ============================================================================

#[repr(C)]
#[derive(Clone, Copy)]
struct tjregion {
    x: c_int,
    y: c_int,
    w: c_int,
    h: c_int,
}

type TjCustomFilter = Option<
    unsafe extern "C" fn(
        coeffs: *mut i16,
        array_region: tjregion,
        plane_region: tjregion,
        component_id: c_int,
        transform_id: c_int,
        transform: *mut tjtransform,
    ) -> c_int,
>;

#[repr(C)]
struct tjtransform {
    r: tjregion,
    op: c_int,
    options: c_int,
    data: *mut c_void,
    custom_filter: TjCustomFilter,
}

const TJXOP_NONE: c_int = 0;
/// 无损裁剪：按 r 指定的矩形从 DCT 域裁出
const TJXOPT_CROP: c_int = 4;

unsafe extern "C" {
    fn tjInitTransform() -> *mut c_void;
    fn tjTransform(
        handle: *mut c_void,
        jpeg_buf: *const c_uchar,
        jpeg_size: c_ulong,
        n: c_int,
        dst_bufs: *mut *mut c_uchar,
        dst_sizes: *mut c_ulong,
        transforms: *mut tjtransform,
        flags: c_int,
    ) -> c_int;
    fn tjFree(buffer: *mut c_uchar);
}

/// iMCU 尺寸表（turbojpeg.h 里是 static const 数组，不是导出符号）。
/// 下标即 TJSAMP_*：444,422,420,GRAY,440,411,441,24, 然后 unknown=-1。
const TJ_MCU_WIDTH: [u32; 9] = [8, 16, 16, 8, 8, 32, 8, 32, 16];
const TJ_MCU_HEIGHT: [u32; 9] = [8, 8, 16, 8, 16, 8, 32, 16, 32];

/// 把显示坐标系的期望 ROI 映射到存储坐标系（对角两点映射取包围盒）
fn roi_to_raw(o: u32, raw_w: u32, raw_h: u32, r: RoiRect) -> Option<RoiRect> {
    let (dw, dh) = oriented_dims(o, raw_w, raw_h);
    if dw == 0 || dh == 0 {
        return None;
    }
    let x0 = r.x.min(dw.saturating_sub(1));
    let y0 = r.y.min(dh.saturating_sub(1));
    let x1 = (r.x + r.w.saturating_sub(1)).min(dw - 1);
    let y1 = (r.y + r.h.saturating_sub(1)).min(dh - 1);
    if x1 < x0 || y1 < y0 {
        return None;
    }
    let (sx0, sy0) = orient_map(o, raw_w, raw_h, x0, y0);
    let (sx1, sy1) = orient_map(o, raw_w, raw_h, x1, y1);
    let (rx, rx1) = (sx0.min(sx1), sx0.max(sx1));
    let (ry, ry1) = (sy0.min(sy1), sy0.max(sy1));
    Some(RoiRect {
        x: rx,
        y: ry,
        w: rx1 - rx + 1,
        h: ry1 - ry + 1,
    })
}

/// 存储坐标系的实际 ROI → 显示坐标系矩形
fn roi_to_oriented(o: u32, raw_w: u32, raw_h: u32, r: RoiRect) -> RoiRect {
    let (x0, y0) = oriented_pt(o, raw_w, raw_h, r.x, r.y);
    let (x1, y1) = oriented_pt(o, raw_w, raw_h, r.x + r.w - 1, r.y + r.h - 1);
    let (x, x1) = (x0.min(x1), x0.max(x1));
    let (y, y1) = (y0.min(y1), y0.max(y1));
    RoiRect {
        x,
        y,
        w: x1 - x + 1,
        h: y1 - y + 1,
    }
}

/// ROI 重解码：把显示坐标系的一块区域按原始分辨率解出来。
///
/// 两步：tjTransform 在 DCT 域无损裁出对齐到 iMCU 的区域 → 再正常解码这张
/// 小图。裁剪区比请求最多大 2×iMCU（对齐代价），绘制时按实际 rect 映射。
pub fn decode_jpeg_roi(
    path: &Path,
    orientation: u32,
    raw_w: u32,
    raw_h: u32,
    rect: RoiRect,
    fast: bool,
) -> Result<DecodedImage> {
    let (data, read_ms) = read_jpeg(path)?;
    let jpeg = &*data;

    // 预检 1：JPEG 结构 + EXIF 方向（以文件为准，缓存里的 orientation 可能过期）
    let (_, exif_o) = scan_jpeg_markers(jpeg)
        .ok_or_else(|| anyhow::anyhow!("ROI 预检失败: JPEG 结构异常"))?;
    let orientation = exif_o.unwrap_or(orientation);

    let raw_rect = roi_to_raw(orientation, raw_w, raw_h, rect)
        .ok_or_else(|| anyhow::anyhow!("ROI 空区域"))?;
    if raw_rect.w == 0 || raw_rect.h == 0 {
        anyhow::bail!("ROI 空区域");
    }

    // 预检 2：header 校验（挡住文件被替换/损坏）+ 取子采样比（iMCU 尺寸）
    let dec = unsafe { tjInitDecompress() };
    if dec.is_null() {
        anyhow::bail!("tjInitDecompress 失败");
    }
    let dec_guard = HandleGuard(dec);
    let mut hw: c_int = 0;
    let mut hh: c_int = 0;
    let mut subsamp: c_int = 0;
    let mut cs: c_int = 0;
    let rc = unsafe {
        tjDecompressHeader3(
            dec,
            jpeg.as_ptr(),
            jpeg.len() as c_ulong,
            &mut hw,
            &mut hh,
            &mut subsamp,
            &mut cs,
        )
    };
    if rc != 0 || hw as u32 != raw_w || hh as u32 != raw_h {
        anyhow::bail!("ROI 预检失败: 文件已变化或损坏");
    }
    // 未知子采样不支持无损裁剪（头文件明确说明），直接放弃 ROI
    let mcu_w = *TJ_MCU_WIDTH
        .get(subsamp as usize)
        .ok_or_else(|| anyhow::anyhow!("ROI 不支持未知子采样"))?;
    let mcu_h = *TJ_MCU_HEIGHT
        .get(subsamp as usize)
        .ok_or_else(|| anyhow::anyhow!("ROI 不支持未知子采样"))?;
    drop(dec_guard);

    // 裁剪区对齐到 iMCU 并夹在图内：向下取整保证不越界，
    // 最多丢掉右/下边缘不足一个 iMCU 的几像素（补细节场景无感）。
    let mx0 = raw_rect.x / mcu_w * mcu_w;
    let my0 = raw_rect.y / mcu_h * mcu_h;
    let mx1 = (raw_rect.x + raw_rect.w).min(raw_w) / mcu_w * mcu_w;
    let my1 = (raw_rect.y + raw_rect.h).min(raw_h) / mcu_h * mcu_h;
    let cw = mx1.saturating_sub(mx0);
    let ch = my1.saturating_sub(my0);
    if cw < mcu_w || ch < mcu_h {
        anyhow::bail!("ROI 区域过小");
    }

    // 无损裁剪
    let th = unsafe { tjInitTransform() };
    if th.is_null() {
        anyhow::bail!("tjInitTransform 失败");
    }
    let tr_guard = HandleGuard(th);
    let mut t = tjtransform {
        r: tjregion {
            x: mx0 as c_int,
            y: my0 as c_int,
            w: cw as c_int,
            h: ch as c_int,
        },
        op: TJXOP_NONE,
        options: TJXOPT_CROP,
        data: std::ptr::null_mut(),
        custom_filter: None,
    };
    let mut dst: *mut c_uchar = std::ptr::null_mut();
    let mut dst_size: c_ulong = 0;
    let decode_start = Instant::now();
    let rc = unsafe {
        tjTransform(
            th,
            jpeg.as_ptr(),
            jpeg.len() as c_ulong,
            1,
            &mut dst,
            &mut dst_size,
            &mut t,
            0,
        )
    };
    if rc != 0 || dst.is_null() || dst_size == 0 {
        anyhow::bail!("ROI 裁剪失败");
    }

    let (decoded, cw2, ch2) = unsafe {
        let cropped = std::slice::from_raw_parts(dst, dst_size as usize);
        // 裁剪块还没有方向信息，先按 orientation=1 解出来
        let r = decode_jpeg_inner(cropped, fast, u32::MAX, read_ms, Some(1))
            .map(|d| {
                let (w, h) = (d.width, d.height);
                (d, w, h)
            });
        tjFree(dst);
        r?
    };
    drop(tr_guard);

    // 裁剪块仍是存储方向，按 EXIF 方向转成显示方向
    let (pixels, ow, oh) = apply_orientation(decoded.pixels, cw2, ch2, orientation);
    let oriented = roi_to_oriented(
        orientation,
        raw_w,
        raw_h,
        RoiRect {
            x: mx0,
            y: my0,
            w: cw2,
            h: ch2,
        },
    );
    Ok(DecodedImage {
        width: ow,
        height: oh,
        pixels,
        src_width: raw_w,
        src_height: raw_h,
        orientation,
        roi_x: oriented.x,
        roi_y: oriented.y,
        read_ms,
        decode_ms: decode_start.elapsed().as_secs_f64() * 1000.0,
        fast,
    })
}
