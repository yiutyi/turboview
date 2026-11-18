//! 基准测试：turboview <jpeg 文件> [轮数]
//!
//! 对比 libjpeg-turbo SIMD（快速/标准两种模式）与 image crate 的纯软件解码，
//! 输出平均/最快耗时与吞吐（MB/s、像素/s）。

use std::path::Path;
use std::time::Instant;
use turboview::turbo;

fn bench(name: &str, iters: usize, mut f: impl FnMut() -> usize) -> (f64, f64, usize) {
    // 预热
    let bytes = f();
    let mut best = f64::MAX;
    let mut total = 0.0;
    for _ in 0..iters {
        let start = Instant::now();
        let b = f();
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        best = best.min(ms);
        total += ms;
        assert_eq!(b, bytes);
    }
    let avg = total / iters as f64;
    println!(
        "  {:<28} 平均 {:7.2} ms · 最快 {:7.2} ms · {:8.1} MB/s · {:6.1} Mpx/s",
        name,
        avg,
        best,
        (bytes as f64 / 1e6) / (avg / 1000.0),
        (bytes as f64 / 4.0 / 1e6) / (avg / 1000.0),
    );
    (avg, best, bytes)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "../testimages/testorig.jpg".to_string());
    let iters: usize = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let path = Path::new(&path);
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as usize;
    println!("文件: {} ({} KB)", path.display(), file_size / 1024);

    // PNG：快速通道（libdeflate + 自研 unfilter）vs image crate
    if path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("png"))
        .unwrap_or(false)
    {
        let (avg_png, best_png, bytes) = bench("PNG 快速通道", iters, || {
            let d = turbo::decode_png_fast(path).expect("fast png failed");
            d.pixels.len()
        });
        let (avg_img_png, _, _) = bench("PNG image crate", iters, || {
            let img = image::open(path).expect("image decode failed");
            img.to_rgba8().as_raw().len()
        });
        let _ = (bytes, best_png);
        println!(
            "\nPNG 结论: 快速通道平均 {:.2} ms，比 image crate 快 {:.1}x。",
            avg_png,
            avg_img_png / avg_png
        );
        return;
    }

    // 读一次文件头，报告尺寸
    let (avg_fast, best_fast, bytes) = bench("libjpeg-turbo 快速", iters, || {
        let d = turbo::decode_jpeg(path, true, u32::MAX).expect("decode failed");
        assert_eq!(d.pixels.len(), d.width as usize * d.height as usize * 4);
        d.pixels.len()
    });
    println!(
        "    └ 尺寸 {}x{} · 解码输出 {:.1} MB",
        turbo::decode_jpeg(path, true, u32::MAX).unwrap().width,
        turbo::decode_jpeg(path, true, u32::MAX).unwrap().height,
        bytes as f64 / 1e6
    );

    let (avg_norm, _best_norm, _) = bench("libjpeg-turbo 标准", iters, || {
        let d = turbo::decode_jpeg(path, false, u32::MAX).expect("decode failed");
        d.pixels.len()
    });

    let (avg_image, _, _) = bench("image crate (纯软件)", iters, || {
        let img = image::open(path).expect("image decode failed");
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        (w * h * 4) as usize
    });

    println!(
        "\n结论: SIMD 快速模式平均 {:.2} ms（最快 {:.2} ms），比标准模式快 {:.1}x；\
         相比 image crate 快 {:.1}x。",
        avg_fast,
        best_fast,
        avg_norm / avg_fast,
        avg_image / avg_fast
    );
}
