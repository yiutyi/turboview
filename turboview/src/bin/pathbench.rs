//! Compare the three image-path representations before wiring PathStore into ImageViewer.
//!
//! Run on Windows with:
//!   cargo run --release --bin pathbench -- 100000
//!   cargo run --release --bin pathbench -- 1000000
//!
//! This is a repeatable synthetic benchmark, not a substitute for measuring a real library.

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use turboview::path_store::PathStore;

fn make_path(i: usize, count: usize) -> PathBuf {
    let dir = (i * 37) % 4096;
    let file = (i * 7919) % count.max(1);
    PathBuf::from(format!(r"D:\Photos\{dir:04}\IMG_{file:08}.jpg"))
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    let count = std::env::args()
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100_000);
    if count == 0 || count > u32::MAX as usize {
        eprintln!("entry count must be in 1..=u32::MAX");
        std::process::exit(2);
    }

    println!("TurboView path storage benchmark · {count} entries");
    println!("Synthetic Windows paths, 4096 shared directories, deterministic order");
    println!("Times are wall-clock; memory is an approximate owned-heap estimate.\n");

    // Vec<PathBuf>
    let start = Instant::now();
    let mut paths: Vec<PathBuf> = (0..count).map(|i| make_path(i, count)).collect();
    let build = elapsed_ms(start);
    let bytes: usize = paths.iter().map(|p| std::mem::size_of::<PathBuf>() + p.as_os_str().len()).sum();
    let start = Instant::now();
    paths.sort_unstable();
    let sort = elapsed_ms(start);
    let start = Instant::now();
    let mut checksum = 0usize;
    for i in (0..count).step_by(97) {
        checksum = checksum.wrapping_add(black_box(paths[i].as_os_str().len()));
    }
    let nav = elapsed_ms(start);
    println!("Vec<PathBuf>       build {build:9.2} ms · sort {sort:9.2} ms · indexed {nav:9.2} ms · payload~{:.2} MiB",
        bytes as f64 / 1_048_576.0);
    black_box(checksum);
    drop(paths);

    // Vec<Arc<Path>>
    let start = Instant::now();
    let mut paths: Vec<Arc<Path>> = (0..count)
        .map(|i| Arc::<Path>::from(make_path(i, count)))
        .collect();
    let build = elapsed_ms(start);
    let bytes: usize = paths.iter().map(|p| std::mem::size_of::<Arc<Path>>() + p.as_os_str().len()).sum();
    let start = Instant::now();
    paths.sort_unstable_by(|a, b| a.as_ref().cmp(b.as_ref()));
    let sort = elapsed_ms(start);
    let start = Instant::now();
    let mut checksum = 0usize;
    for i in (0..count).step_by(97) {
        let shared = Arc::clone(&paths[i]);
        checksum = checksum.wrapping_add(black_box(shared.as_os_str().len()));
    }
    let nav = elapsed_ms(start);
    println!("Vec<Arc<Path>>     build {build:9.2} ms · sort {sort:9.2} ms · indexed {nav:9.2} ms · payload~{:.2} MiB",
        bytes as f64 / 1_048_576.0);
    black_box(checksum);
    drop(paths);

    // PathStore + compact integer IDs
    let start = Instant::now();
    let mut store = PathStore::with_capacity(count, count * 16, 4096);
    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        ids.push(store.insert(Path::new(&make_path(i, count))));
    }
    let build = elapsed_ms(start);
    let bytes = store.storage_bytes() + ids.capacity() * std::mem::size_of_val(&ids[0]);
    let start = Instant::now();
    store.sort_ids(&mut ids);
    let sort = elapsed_ms(start);
    let start = Instant::now();
    let mut index_checksum = 0usize;
    for i in (0..count).step_by(97) {
        if let Some(id) = store.id_at(i) {
            index_checksum = index_checksum.wrapping_add(black_box(id.index()));
        }
    }
    let index_ms = elapsed_ms(start);

    let start = Instant::now();
    let mut checksum = 0usize;
    for i in (0..count).step_by(97) {
        if let Some(path) = store.get(ids[i]) {
            checksum = checksum.wrapping_add(black_box(path.as_os_str().len()));
        }
    }
    let materialize_ms = elapsed_ms(start);
    println!("PathStore + ID     build {build:9.2} ms · sort {sort:9.2} ms · index {index_ms:9.2} ms · materialize {materialize_ms:9.2} ms · estimated~{:.2} MiB · dirs {}",
        bytes as f64 / 1_048_576.0, store.directory_count());
    black_box(index_checksum);
    black_box(checksum);

    println!("\nNote: PathStore sort compares precomputed byte keys (no PathBuf materialization).");
    println!("Validate against a real library before integrating; synthetic paths share short roots.");
}
