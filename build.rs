fn main() {
    // libkrun is a C-ABI dylib (written in Rust). Homebrew puts it in /opt/homebrew/lib;
    // override with LIBKRUN_DIR if it lives elsewhere.
    let dir = std::env::var("LIBKRUN_DIR").unwrap_or_else(|_| {
        if cfg!(target_os = "macos") {
            "/opt/homebrew/lib".into()
        } else {
            "/usr/local/lib64".into()
        }
    });
    println!("cargo:rustc-link-search=native={dir}");
    println!("cargo:rustc-link-lib=dylib=krun");
    // Bake the library dir into the binary so it runs without DYLD_LIBRARY_PATH.
    println!("cargo:rustc-link-arg=-Wl,-rpath,{dir}");
    println!("cargo:rerun-if-env-changed=LIBKRUN_DIR");

    // The guest helper (guest/, a static Linux binary built with `make guest`) is embedded
    // when present for the target architecture.
    println!("cargo::rustc-check-cfg=cfg(guest_helper)");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let helper = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join(format!("guest/bin/agentbox-guest-{arch}"));
    println!("cargo:rerun-if-changed={}", helper.display());
    if helper.is_file() {
        println!("cargo:rustc-cfg=guest_helper");
        println!("cargo:rustc-env=AGENTBOX_GUEST_HELPER={}", helper.display());
    }
}
