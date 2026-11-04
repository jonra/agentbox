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
}
