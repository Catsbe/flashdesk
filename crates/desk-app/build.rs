fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=FLASHDESK_MINGW_DIR");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "gnu" {
        // Toolchain root is D:\Rust\mingw64 by convention (AGENTS.md). Machines without a D: data
        // drive can point FLASHDESK_MINGW_DIR at the same WinLibs 16.2.0 tree elsewhere.
        let mingw =
            std::env::var("FLASHDESK_MINGW_DIR").unwrap_or_else(|_| "D:\\Rust\\mingw64".to_string());
        println!("cargo:rustc-link-search=native={mingw}\\lib");
        println!("cargo:rustc-link-search=native={mingw}\\x86_64-w64-mingw32\\lib");
        println!("cargo:rustc-link-search=native={mingw}\\lib\\gcc\\x86_64-w64-mingw32\\16.2.0");
        println!("cargo:rustc-link-arg=-Wl,--start-group");
        println!("cargo:rustc-link-arg=-l:libstdc++.a");
        println!("cargo:rustc-link-arg=-l:libwinpthread.a");
        println!("cargo:rustc-link-arg=-l:libgcc.a");
        println!("cargo:rustc-link-arg=-l:libgcc_eh.a");
        println!("cargo:rustc-link-arg=-lmingwex");
        println!("cargo:rustc-link-arg=-lmsvcrt");
        println!("cargo:rustc-link-arg=-lkernel32");
        println!("cargo:rustc-link-arg=-Wl,--end-group");
        embed_windows_icon(&mingw);
    }
}

fn embed_windows_icon(mingw: &str) {
    println!("cargo:rerun-if-changed=../../assets/flashdesk.ico");

    let manifest_dir =
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let src_ico = manifest_dir
        .join("..")
        .join("..")
        .join("assets")
        .join("flashdesk.ico");
    let dst_ico = out_dir.join("flashdesk.ico");
    std::fs::copy(&src_ico, &dst_ico).unwrap_or_else(|e| {
        panic!(
            "failed to copy {} -> {}: {e}",
            src_ico.display(),
            dst_ico.display()
        )
    });

    let rc_path = out_dir.join("flashdesk.rc");
    std::fs::write(&rc_path, "1 ICON \"flashdesk.ico\"\n").expect("write flashdesk.rc");

    let mingw_bin = std::path::Path::new(mingw).join("bin");
    let windres = mingw_bin.join("windres.exe");
    let cur_path = std::env::var_os("PATH").unwrap_or_default();
    let mut joined_paths = vec![mingw_bin];
    joined_paths.extend(std::env::split_paths(&cur_path));
    let child_path = std::env::join_paths(joined_paths).expect("join PATH");

    let status = std::process::Command::new(&windres)
        .current_dir(&out_dir)
        .env("PATH", child_path)
        .args(["-O", "coff", "-i", "flashdesk.rc", "-o", "flashdesk_res.o"])
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke {}: {e}", windres.display()));
    assert!(
        status.success(),
        "{} failed with status {status}",
        windres.display()
    );

    let res_obj = out_dir.join("flashdesk_res.o");
    println!("cargo:rustc-link-arg-bins={}", res_obj.display());
}
