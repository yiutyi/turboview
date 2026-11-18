//! 压缩包冒烟/基准工具：archbench <压缩包> [--all]
//!
//! - 列出压缩包内图片条目（计时）
//! - 解压首张图并解码（计时），验证端到端链路
//! - --all：逐张解压+解码全部图片，输出总耗时与吞吐

use std::path::Path;
use std::time::Instant;
use turboview::archive;

fn decode_bytes(data: &[u8], ext: &str) -> anyhow::Result<(u32, u32)> {
    let img = match ext {
        "jpg" | "jpeg" => match turboview::turbo::decode_jpeg_bytes(data, false, 4096, 0.0) {
            Ok(d) => d,
            Err(e) => match image::load_from_memory(data) {
                Ok(im) => {
                    let rgba = im.to_rgba8();
                    let (w, h) = rgba.dimensions();
                    turboview::turbo::DecodedImage {
                        width: w,
                        height: h,
                        pixels: turboview::turbo::bgra_from_rgba(rgba.as_raw()),
                        src_width: w,
                        src_height: h,
                        orientation: 1,
                        roi_x: 0,
                        roi_y: 0,
                        read_ms: 0.0,
                        decode_ms: 0.0,
                        fast: false,
                    }
                }
                Err(_) => return Err(e),
            },
        },
        "png" => match turboview::turbo::decode_png_fast_bytes(data, 0.0) {
            Some(d) => d,
            None => {
                let im = image::load_from_memory(data)?;
                let rgba = im.to_rgba8();
                let (w, h) = rgba.dimensions();
                turboview::turbo::DecodedImage {
                    width: w,
                    height: h,
                    pixels: turboview::turbo::bgra_from_rgba(rgba.as_raw()),
                    src_width: w,
                    src_height: h,
                    orientation: 1,
                    roi_x: 0,
                    roi_y: 0,
                    read_ms: 0.0,
                    decode_ms: 0.0,
                    fast: false,
                }
            }
        },
        _ => {
            let im = image::load_from_memory(data)?;
            let rgba = im.to_rgba8();
            let (w, h) = rgba.dimensions();
            turboview::turbo::DecodedImage {
                width: w,
                height: h,
                pixels: turboview::turbo::bgra_from_rgba(rgba.as_raw()),
                src_width: w,
                src_height: h,
                orientation: 1,
                roi_x: 0,
                roi_y: 0,
                read_ms: 0.0,
                decode_ms: 0.0,
                fast: false,
            }
        }
    };
    Ok((img.width, img.height))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(path) = args.get(1) else {
        eprintln!("用法: archbench <压缩包> [--all]");
        std::process::exit(2);
    };
    let all = args.iter().any(|a| a == "--all");
    let path = Path::new(path);

    if args.iter().any(|a| a == "--zipopen") {
        // 诊断：拆解 ZIP 打开耗时（File::open / ZipArchive::new / 条目循环）
        use std::fs::File;
        let t = Instant::now();
        let file = File::open(path).unwrap();
        println!("File::open:            {:8.2} ms", t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        let zip = zip::ZipArchive::new(file).unwrap();
        println!("ZipArchive::new:       {:8.2} ms ({} 条目)", t.elapsed().as_secs_f64() * 1000.0, zip.len());
        let t = Instant::now();
        let mut n = 0usize;
        for i in 0..zip.len() {
            if let Ok(e) = zip.by_index_data(i) {
                if let Ok(name) = e.name() {
                    if name.to_ascii_lowercase().ends_with(".jpg") {
                        n += 1;
                    }
                }
            }
        }
        println!("by_index_data 循环:    {:8.2} ms ({n} jpg)", t.elapsed().as_secs_f64() * 1000.0);
        return;
    }

    // 1) 列目录
    let t0 = Instant::now();
    let names = match archive::list_images(path) {
        Ok(n) => n,
        Err(e) => {
            println!("打开失败: {e:#}");
            std::process::exit(1);
        }
    };
    let list_ms = t0.elapsed().as_secs_f64() * 1000.0;
    println!(
        "列表: {} 张图片, 耗时 {:.2} ms",
        names.len(),
        list_ms
    );
    for (i, n) in names.iter().enumerate() {
        println!("  [{i}] {n}");
    }
    if names.is_empty() {
        return;
    }

    if !all {
        // 2) 首图：解压 + 解码
        let name = &names[0];
        let t1 = Instant::now();
        let bytes = match archive::extract_entry(path, name) {
            Ok(b) => b,
            Err(e) => {
                println!("解压失败: {e:#}");
                std::process::exit(1);
            }
        };
        let extract_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        let t2 = Instant::now();
        match decode_bytes(&bytes, &ext) {
            Ok((w, h)) => println!(
                "首图 {}: {}x{}, 解压 {:.2} ms ({} KB), 解码 {:.2} ms",
                name,
                w,
                h,
                extract_ms,
                bytes.len() / 1024,
                t2.elapsed().as_secs_f64() * 1000.0
            ),
            Err(e) => println!(
                "首图 {}: 解压 {:.2} ms ({} KB) 但解码失败: {e:#}",
                name,
                extract_ms,
                bytes.len() / 1024,
            ),
        }
        return;
    }

    // 3) 全量：逐张解压 + 解码
    let mut total_bytes = 0usize;
    let t_all = Instant::now();
    for (i, name) in names.iter().enumerate() {
        let bytes = match archive::extract_entry(path, name) {
            Ok(b) => b,
            Err(e) => {
                println!("  [{i}] {name}: 解压失败: {e:#}");
                continue;
            }
        };
        total_bytes += bytes.len();
        let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        match decode_bytes(&bytes, &ext) {
            Ok((w, h)) => println!("  [{i}] {name}: {w}x{h}, {} KB, ok", bytes.len() / 1024),
            Err(e) => println!(
                "  [{i}] {name}: {} KB, 解码失败: {e:#}",
                bytes.len() / 1024
            ),
        }
    }
    let ms = t_all.elapsed().as_secs_f64() * 1000.0;
    println!(
        "全量 {} 张: 总耗时 {:.1} ms, 共 {} MB, 吞吐 {:.1} MB/s",
        names.len(),
        ms,
        total_bytes / 1024 / 1024,
        (total_bytes as f64 / 1e6) / (ms / 1000.0)
    );
}
