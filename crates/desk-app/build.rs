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
    }
}
