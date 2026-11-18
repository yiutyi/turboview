//! 压缩包图片浏览（ZIP / 7z / RAR）
//!
//! 设计原则（地表最强速度）：
//! - 打开压缩包只读目录/中央目录（ZIP 秒开、7z 只解析头、RAR 走官方 UnRAR DLL 封装），
//!   不整包解压、不落临时文件；
//! - 浏览某一图片时只解压该条目到内存，直接喂 SIMD 解码器，
//!   与本地文件走同一条解码路径（libjpeg-turbo / libdeflate / image crate）；
//! - 条目列表按名称排序，导航 / 随机 / 幻灯片全部限制在压缩包内。

use anyhow::{Context as _, Result};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// 支持的图片扩展名（目录扫描与压缩包条目过滤共用）
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "bmp", "gif", "webp", "tif", "tiff", "ico", "tga", "pnm", "ppm", "pgm",
    "pbm", "qoi",
];

/// 支持的压缩包扩展名（目录扫描用）
pub const ARCHIVE_EXTENSIONS: &[&str] = &["zip", "7z", "rar"];

/// 判断路径是否是受支持的压缩包（按扩展名）
pub fn is_archive(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| matches!(e.as_str(), "zip" | "7z" | "rar"))
}

/// 条目名是否为受支持的图片
fn is_image_name(name: &str) -> bool {
    let ext = name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    IMAGE_EXTENSIONS.contains(&ext.as_str())
}

/// 压缩包格式
#[derive(Clone, Copy, PartialEq)]
enum Format {
    Zip,
    SevenZ,
    Rar,
}

impl Format {
    fn of(path: &Path) -> Option<Self> {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("zip") => Some(Self::Zip),
            Some("7z") => Some(Self::SevenZ),
            Some("rar") => Some(Self::Rar),
            _ => None,
        }
    }
}

/// 列出压缩包内的图片条目名（按名称排序、去重）
pub fn list_images(archive: &Path) -> Result<Vec<String>> {
    let Some(format) = Format::of(archive) else {
        anyhow::bail!("不支持的压缩包格式: {}", archive.display());
    };
    let mut names = match format {
        Format::Zip => list_zip(archive)?,
        Format::SevenZ => list_7z(archive)?,
        Format::Rar => list_rar(archive)?,
    };
    names.sort_unstable();
    names.dedup();
    Ok(names)
}

/// 把压缩包内的指定条目解压到内存（只解这一个，零临时文件）
pub fn extract_entry(archive: &Path, name: &str) -> Result<Vec<u8>> {
    let Some(format) = Format::of(archive) else {
        anyhow::bail!("不支持的压缩包格式: {}", archive.display());
    };
    match format {
        Format::Zip => extract_zip(archive, name),
        Format::SevenZ => extract_7z(archive, name),
        Format::Rar => extract_rar(archive, name),
    }
}

// ---------------------------------------------------------------- ZIP

/// 打开 ZIP 归档。
///
/// zip 9 默认的 `ArchiveOffset::Detect` 会从文件头正向**整包扫描** `PK\x01\x02`
/// 签名来定位中央目录（1KB 窗口 + 无缓冲 File ≈ 每 MB 2 次系统调用，21MB 包要 90ms）。
/// 这里先用 `Known(0)` 快速路径——只在 EOCD 声明的偏移处读 4 字节校验签名，
/// 毫秒级；失败（自解压 / 前导垃圾 zip）再回退 Detect 全量探测，保证兼容。
fn open_zip(archive: &Path) -> Result<zip::ZipArchive<std::io::BufReader<File>>> {
    let file = File::open(archive).with_context(|| format!("无法打开 {}", archive.display()))?;
    let fast = zip::ZipArchive::with_config(
        zip::read::Config {
            archive_offset: zip::read::ArchiveOffset::Known(0),
        },
        std::io::BufReader::new(file),
    );
    if let Ok(zip) = fast {
        return Ok(zip);
    }
    let file = File::open(archive).with_context(|| format!("无法打开 {}", archive.display()))?;
    zip::ZipArchive::new(std::io::BufReader::new(file)).context("ZIP 中央目录损坏或不是有效 ZIP")
}

fn list_zip(archive: &Path) -> Result<Vec<String>> {
    // 只读中央目录元数据，巨包也是毫秒级
    let zip = open_zip(archive)?;
    let mut out = Vec::new();
    for i in 0..zip.len() {
        // by_index_data 只读中央目录、不解析本地头：
        // 本地头损坏的 zip（合成/截断样本）依然能列出条目，解压时再按单图报错
        let entry = zip.by_index_data(i).context("读取 ZIP 条目失败")?;
        if entry.is_dir() {
            continue;
        }
        // name() 返回 ZipResult（编码异常的条目直接跳过）
        let Ok(name) = entry.name() else {
            continue;
        };
        let name = name.to_string();
        if is_image_name(&name) {
            out.push(name);
        }
    }
    Ok(out)
}

fn extract_zip(archive: &Path, name: &str) -> Result<Vec<u8>> {
    let mut zip = open_zip(archive)?;
    // 先按中央目录元数据确认条目存在（本地头损坏的合成样本也能给出准确提示）
    let mut exists = false;
    for i in 0..zip.len() {
        if let Ok(entry) = zip.by_index_data(i) {
            if let Ok(n) = entry.name() {
                if n == name {
                    exists = true;
                    break;
                }
            }
        }
    }
    if !exists {
        anyhow::bail!("ZIP 中不存在条目 {name}");
    }
    let mut entry = zip
        .by_name(name)
        .with_context(|| format!("解压 ZIP 条目 {name} 失败（条目数据可能损坏）"))?;
    let mut buf = Vec::with_capacity(entry.size() as usize);
    entry
        .read_to_end(&mut buf)
        .with_context(|| format!("读取 ZIP 条目 {name} 数据失败"))?;
    Ok(buf)
}

// ---------------------------------------------------------------- 7z

fn list_7z(archive: &Path) -> Result<Vec<String>> {
    // SevenZReader::open 只解析头部元数据，不解压任何数据流
    let mut reader = sevenz_rust::SevenZReader::open(archive, sevenz_rust::Password::empty())
        .with_context(|| format!("无法打开 7z {}", archive.display()))?;
    let mut out = Vec::new();
    reader
        .for_each_entries(|entry, _| {
            if !entry.is_directory && is_image_name(&entry.name) {
                out.push(entry.name.clone());
            }
            Ok(true)
        })
        .context("读取 7z 条目列表失败")?;
    Ok(out)
}

fn extract_7z(archive: &Path, name: &str) -> Result<Vec<u8>> {
    let mut reader = sevenz_rust::SevenZReader::open(archive, sevenz_rust::Password::empty())
        .with_context(|| format!("无法打开 7z {}", archive.display()))?;
    let mut found: Option<Vec<u8>> = None;
    reader
        .for_each_entries(|entry, src| {
            if entry.name == name {
                let mut buf = Vec::with_capacity(entry.size as usize);
                src.read_to_end(&mut buf)?;
                found = Some(buf);
                Ok(false) // 命中即停，后续 folder 不再解码
            } else {
                // 关键：跳过的条目也必须排空。7z 可以是 solid 包——同一 folder 内
                // 多个文件共享一条解压流，不排空当前条目，下一条目就会读到错位数据。
                let mut sink = std::io::sink();
                std::io::copy(src, &mut sink)?;
                Ok(true)
            }
        })
        .with_context(|| format!("解压 7z 条目 {name} 失败"))?;
    found.ok_or_else(|| anyhow::anyhow!("7z 中不存在条目 {name}"))
}

// ---------------------------------------------------------------- RAR

fn list_rar(archive: &Path) -> Result<Vec<String>> {
    // List 模式迭代器自动跳过数据流，只读头部，巨包也是毫秒级
    let listing = unrar::Archive::new(archive)
        .open_for_listing()
        .with_context(|| format!("无法打开 RAR {}", archive.display()))?;
    let mut out = Vec::new();
    for item in listing {
        let header = item.context("读取 RAR 条目失败")?;
        if !header.is_directory() && is_image_name(&header.filename.to_string_lossy()) {
            out.push(header.filename.to_string_lossy().into_owned());
        }
    }
    Ok(out)
}

fn extract_rar(archive: &Path, name: &str) -> Result<Vec<u8>> {
    let mut open = unrar::Archive::new(archive)
        .open_for_processing()
        .with_context(|| format!("无法打开 RAR {}", archive.display()))?;
    loop {
        match open.read_header().context("读取 RAR 条目失败")? {
            Some(entry) => {
                if entry.entry().filename.to_string_lossy() == name {
                    let (buf, next) = entry.read().context("解压 RAR 条目失败")?;
                    let _ = next;
                    return Ok(buf);
                }
                open = entry.skip().context("跳过 RAR 条目失败")?;
            }
            None => anyhow::bail!("RAR 中不存在条目 {name}"),
        }
    }
}
