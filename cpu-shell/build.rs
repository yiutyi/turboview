use std::path::PathBuf;

fn main() {
    // Link the libjpeg-turbo static library built from this repository, so
    // Windows, macOS and Linux run the exact same SIMD decoder.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest
        .parent()
        .expect("cpu-shell must live inside the libjpeg-turbo checkout");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // Ninja (single-config) puts the lib directly in the build dir; the
    // Visual Studio generator puts it in Release/ or Debug/.
    let candidates: Vec<&str> = if target_os == "windows" {
        vec![
            "build-turbo/turbojpeg-static.lib",
            "build-turbo/Release/turbojpeg-static.lib",
            "build-turbo/Debug/turbojpeg-static.lib",
        ]
    } else {
        vec![
            "build-turbo-linux/libturbojpeg.a",
            "build-turbo-linux/Release/libturbojpeg.a",
            "build-turbo/libturbojpeg.a",
        ]
    };

    let mut found = None;
    for rel in &candidates {
        let lib = repo.join(rel);
        if lib.is_file() {
            found = Some(lib);
            break;
        }
    }

    let Some(lib) = found else {
        panic!(
            "libjpeg-turbo static library not found under {}. Build it first:\n  \
             cmake -S . -B build-turbo -G Ninja -DENABLE_SHARED=OFF -DENABLE_STATIC=ON \
             -DWITH_SIMD=ON -DWITH_TURBOJPEG=ON\n  cmake --build build-turbo --target turbojpeg-static",
            repo.display()
        );
    };

    println!("cargo:rustc-link-search=native={}", lib.parent().unwrap().display());
    let stem = if target_os == "windows" { "turbojpeg-static" } else { "turbojpeg" };
    println!("cargo:rustc-link-lib=static={stem}");
    if target_os != "windows" {
        println!("cargo:rustc-link-lib=dylib=m");
    }
    println!("cargo:rerun-if-changed={}", lib.display());
}
