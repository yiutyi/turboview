use std::path::PathBuf;

fn main() {
    // libjpeg-turbo 3.2.0 SIMD 静态库由 scripts/build-turbojpeg.bat 构建到
    // <repo>/build-turbo/turbojpeg-static.lib（NASM AVX2/SSE2 汇编全量编入，
    // WITH_CRT_DLL=ON 动态 CRT，与 Rust 的 /MD 匹配）。
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let libjpeg_root = manifest_dir
        .parent()
        .expect("turboview must live inside the libjpeg-turbo checkout");
    let lib_dir = libjpeg_root.join("build-turbo");
    let lib_file = lib_dir.join("turbojpeg-static.lib");

    if !lib_file.exists() {
        panic!(
            "turbojpeg static library not found at {}. \
             Run scripts\\build-turbojpeg.bat first.",
            lib_file.display()
        );
    }

    println!("cargo:rerun-if-changed={}", lib_file.display());
    println!("cargo:rustc-link-search=native={}", lib_dir.display());

    // GPUI's Windows backend loads RT_ICON resource ID 1 from the executable
    // (LoadImageW(module, MAKEINTRESOURCE(1), IMAGE_ICON, ...)). Without this
    // resource the window class receives a null HICON, so the taskbar is blank.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        use std::process::Command;

        let icon = manifest_dir.join("assets").join("turboview.ico");
        if !icon.exists() {
            panic!("Windows application icon is missing: {}", icon.display());
        }
        let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set"));
        let rc_file = out_dir.join("turboview-icon.rc");
        let res_file = out_dir.join("turboview-icon.res");
        let icon_path = icon.to_string_lossy().replace('\\\\', "\\\\\\\\").replace('"', "\\\\"");
        std::fs::write(&rc_file, format!("1 ICON \"{}\"\\n", icon_path))
            .expect("write Windows icon resource script");
        let status = Command::new("rc.exe")
            .arg("/nologo")
            .arg("/fo")
            .arg(&res_file)
            .arg(&rc_file)
            .status()
            .expect("run rc.exe from the MSVC toolchain");
        if !status.success() {
            panic!("rc.exe failed to compile the TurboView icon resource");
        }
        println!("cargo:rerun-if-changed={}", icon.display());
        println!("cargo:rustc-link-arg-bin=turboview={}", res_file.display());
    }
    // 注意：链接指令由 src/turbo.rs 的 #[link(..., modifiers = "-bundle")]
    // 提供，不在这里发 rustc-link-lib —— 与命令行 -l 的修饰符冲突会让 rustc
    // 报 "overriding linking modifiers from command line is not supported"。
    // -bundle 防止静态库被捆绑进 rlib（MSVC link.exe 无法从 rlib 内的嵌套
    // 归档提取符号，会报 LNK2019），改为直接出现在最终链接命令行。
}
