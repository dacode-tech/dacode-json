//! Compiles the vendored C/C++ baselines for `benches/cbaseline.rs`.
//!
//! Both are vendored under `vendor/` so this project builds without any
//! reference to the Vela tree — see `vendor/README.md`.
//!
//! Gated behind the `cbench` feature, because simdjson's single-header
//! amalgamation is 2.7 MB of C++17 and takes ~30 s to compile. A plain
//! `cargo test` should not pay for that.

fn main() {
    println!("cargo:rerun-if-changed=vendor/shim.c");
    println!("cargo:rerun-if-changed=vendor/shim_simdjson.cpp");
    println!("cargo:rerun-if-changed=vendor/yyjson/yyjson.c");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_FEATURE_CBENCH").is_err() {
        return;
    }

    // --- yyjson (C99) ---
    //
    // Built with the same optimisation settings yyjson's own CMake release
    // profile uses, so the comparison is against yyjson at its best rather
    // than a debug build.
    cc::Build::new()
        .file("vendor/yyjson/yyjson.c")
        .file("vendor/shim.c")
        .include("vendor")
        .opt_level(3)
        .flag_if_supported("-std=c99")
        .flag_if_supported("-fno-plt")
        // yyjson reads these at compile time; both default on, set
        // explicitly so the build is reproducible.
        .define("YYJSON_DISABLE_NON_STANDARD", None)
        .warnings(false)
        .compile("yyjson_shim");

    // --- simdjson (C++17) ---
    cc::Build::new()
        .cpp(true)
        .file("vendor/simdjson/simdjson.cpp")
        .file("vendor/shim_simdjson.cpp")
        .include("vendor")
        .opt_level(3)
        .flag_if_supported("-std=c++17")
        .flag_if_supported("-fno-rtti")
        .warnings(false)
        .compile("simdjson_shim");

    // simdjson needs the C++ runtime.
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("apple") {
        println!("cargo:rustc-link-lib=c++");
    } else if target.contains("linux") {
        println!("cargo:rustc-link-lib=stdc++");
    }
}
