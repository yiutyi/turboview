//! Compare the three image-path representations before wiring PathStore into ImageViewer.
//!
//! Run on Windows with:
//!   cargo run --release --bin pathbench -- 100000 flat
//!   cargo run --release --bin pathbench -- 100000 deep
//!   cargo run --release --bin pathbench -- 1000000 deep
//!
//! This is a repeatable synthetic benchmark, not a substitute for measuring a real library.

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use turboview::path_store::PathStore;
use turboview::prefix_path_store::PrefixPathStore;

fn make_path(i: usize, count: usize, deep: bool) -> PathBuf {
    let file = (i * 7919) % count.max(1);
    if deep {
        // Eight entries per leaf directory; six numeric components create a
        // high-cardinality, deeply nested directory distribution.
        let dir = i / 8;
        PathBuf::from(format!(
            r"D:\Archive\G{:03}\Y{:03}\M{:03}\C{:03}\B{:03}\S{:03}\IMG_{file:08}.jpg",
            dir / 1_000_000_000,
            (dir / 1_000_000) % 1_000,
            (dir / 1_000) % 1_000,
            (dir / 100) % 10,
            (dir / 10) % 10,
            dir % 10,
        ))
    } else {
        let dir = (i * 37) % 4096;
        PathBuf::from(format!(r"D:\Photos\{dir:04}\IMG_{file:08}.jpg"))
    }
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    let mut args = std::env::args().skip(1);
    let count = args
        .next()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100_000);
    let profile = args.next().unwrap_or_else(|| "flat".to_string());
    let deep = match profile.as_str() {
        "flat" => false,
        "deep" => true,
        _ => {
            eprintln!("profile must be 'flat' or 'deep'");
            std::process::exit(2);
        }
    };
    if count == 0 || count > u32::MAX as usize {
        eprintln!("entry count must be in 1..=u32::MAX");
        std::process::exit(2);
    }
    let directory_capacity = if deep { count.div_ceil(8) } else { 4096 };

    println!("TurboView path storage benchmark · {count} entries");
    println!("Synthetic Windows paths · profile={profile} · estimated unique directories={directory_capacity} · deterministic order");
    println!("Times are wall-clock; memory is an approximate owned-heap estimate.\n");

    // Vec<PathBuf>
    let start = Instant::now();
    let mut paths: Vec<PathBuf> = (0..count).map(|i| make_path(i, count, deep)).collect();
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
    let start = Instant::now();
    let mut forward_checksum = 0usize;
    for path in &paths {
        forward_checksum = forward_checksum.wrapping_add(black_box(path.as_os_str().len()));
    }
    for path in paths.iter().rev() {
        forward_checksum = forward_checksum.wrapping_add(black_box(path.as_os_str().len()));
    }
    let traversal = elapsed_ms(start);
    println!("Vec<PathBuf>       build {build:9.2} ms · sort {sort:9.2} ms · indexed {nav:9.2} ms · fwd+rev {traversal:9.2} ms · payload~{:.2} MiB",
        bytes as f64 / 1_048_576.0);
    black_box(checksum);
    black_box(forward_checksum);
    drop(paths);

    // Vec<Arc<Path>>
    let start = Instant::now();
    let mut paths: Vec<Arc<Path>> = (0..count)
        .map(|i| Arc::<Path>::from(make_path(i, count, deep)))
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
    let start = Instant::now();
    let mut forward_checksum = 0usize;
    for path in &paths {
        forward_checksum = forward_checksum.wrapping_add(black_box(path.as_os_str().len()));
    }
    for path in paths.iter().rev() {
        forward_checksum = forward_checksum.wrapping_add(black_box(path.as_os_str().len()));
    }
    let traversal = elapsed_ms(start);
    println!("Vec<Arc<Path>>     build {build:9.2} ms · sort {sort:9.2} ms · indexed {nav:9.2} ms · fwd+rev {traversal:9.2} ms · payload~{:.2} MiB",
        bytes as f64 / 1_048_576.0);
    black_box(checksum);
    black_box(forward_checksum);
    drop(paths);

    // PathStore + compact integer IDs
    let start = Instant::now();
    // Reserve directory metadata for the selected distribution; filename bytes are shared by both profiles.
    let mut store = PathStore::with_capacity(count, count * 20, directory_capacity);
    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        ids.push(store.insert(Path::new(&make_path(i, count, deep))));
    }
    let build = elapsed_ms(start);
    let bytes = store.storage_bytes() + ids.capacity() * std::mem::size_of_val(&ids[0]);
    let start = Instant::now();
    store.sort_ids(&mut ids);
    let sort = elapsed_ms(start);
    let start = Instant::now();
    let mut index_checksum = 0usize;
    // `ids` is the sorted navigation order. `id_at(i)` would only return the
    // insertion-order ID and would not benchmark navigation through this list.
    for i in (0..count).step_by(97) {
        index_checksum = index_checksum.wrapping_add(black_box(ids[i].index()));
    }
    let index_ms = elapsed_ms(start);

    let start = Instant::now();
    let mut traversal_checksum = 0usize;
    for id in &ids {
        traversal_checksum = traversal_checksum.wrapping_add(black_box(id.index()));
    }
    for id in ids.iter().rev() {
        traversal_checksum = traversal_checksum.wrapping_add(black_box(id.index()));
    }
    let traversal_ms = elapsed_ms(start);

    let start = Instant::now();
    let mut checksum = 0usize;
    for i in (0..count).step_by(97) {
        if let Some(path) = store.get(ids[i]) {
            checksum = checksum.wrapping_add(black_box(path.as_os_str().len()));
        }
    }
    let materialize_ms = elapsed_ms(start);

    let start = Instant::now();
    let mut full_materialize_checksum = 0usize;
    for id in &ids {
        if let Some(path) = store.get(*id) {
            full_materialize_checksum = full_materialize_checksum
                .wrapping_add(black_box(path.as_os_str().len()));
        }
    }
    for id in ids.iter().rev() {
        if let Some(path) = store.get(*id) {
            full_materialize_checksum = full_materialize_checksum
                .wrapping_add(black_box(path.as_os_str().len()));
        }
    }
    let full_materialize_ms = elapsed_ms(start);
    println!("PathStore + ID     build {build:9.2} ms · sort {sort:9.2} ms · indexed {index_ms:9.2} ms · fwd+rev-id {traversal_ms:9.2} ms · sampled-get {materialize_ms:9.2} ms · full-get-fwd+rev {full_materialize_ms:9.2} ms · estimated~{:.2} MiB · dirs {}",
        bytes as f64 / 1_048_576.0, store.directory_count());
    black_box(index_checksum);
    black_box(traversal_checksum);
    black_box(checksum);
    black_box(full_materialize_checksum);

    // PrefixPathStore v2: intern every directory component under its parent.
    let prefix_directory_capacity = if deep { count.div_ceil(8) * 6 } else { 4096 * 4 };
    let start = Instant::now();
    let mut prefix_store = PrefixPathStore::with_capacity(count, count * 20, prefix_directory_capacity);
    let mut prefix_ids = Vec::with_capacity(count);
    for i in 0..count {
        prefix_ids.push(prefix_store.insert(Path::new(&make_path(i, count, deep))));
    }
    let prefix_build = elapsed_ms(start);
    let prefix_bytes = prefix_store.storage_bytes()
        + prefix_ids.capacity() * std::mem::size_of_val(&prefix_ids[0]);
    let start = Instant::now();
    prefix_store.sort_ids(&mut prefix_ids);
    let prefix_sort = elapsed_ms(start);
    let start = Instant::now();
    let mut prefix_checksum = 0usize;
    for i in (0..count).step_by(97) {
        prefix_checksum = prefix_checksum.wrapping_add(black_box(prefix_ids[i].index()));
    }
    for id in &prefix_ids {
        prefix_checksum = prefix_checksum.wrapping_add(black_box(id.index()));
    }
    for id in prefix_ids.iter().rev() {
        prefix_checksum = prefix_checksum.wrapping_add(black_box(id.index()));
    }
    let prefix_traversal = elapsed_ms(start);
    println!("PrefixPathStore v2 build {prefix_build:9.2} ms · sort {prefix_sort:9.2} ms · index+fwd+rev {prefix_traversal:9.2} ms · estimated~{:.2} MiB · dirs {}",
        prefix_bytes as f64 / 1_048_576.0, prefix_store.directory_count());
    black_box(prefix_checksum);
    drop(prefix_store);
    drop(prefix_ids);

    println!("\nNote: PathStore sort compares precomputed byte keys (no PathBuf materialization).");
    println!("Profiles: flat = 4,096 shallow shared directories; deep = six nested components and up to one leaf directory per eight entries.");
    println!("Validate against a real library before integrating; synthetic profiles are not a substitute for real paths.");
}
