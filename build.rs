//! Compiles the vendored C/C++ baselines for `benches/cbaseline.rs`.
//!
//! Both are vendored under `vendor/` so this project builds without any
//! reference to the Vela tree — see `vendor/README.md`.
//!
//! Gated behind the `cbench` feature, because simdjson's single-header
//! amalgamation is 2.7 MB of C++17 and takes ~30 s to compile. A plain
//! `cargo test` should not pay for that. The gate is a `cfg`, not the
//! runtime env check it replaces, because `cc` is an *optional*
//! build-dependency behind the same feature: with `cbench` off the crate
//! is not merely unused but absent, and it stays out of every consumer's
//! resolved dependency graph — which is what graph-auditing gates read.

fn main() {
    println!("cargo:rerun-if-changed=vendor/shim.c");
    println!("cargo:rerun-if-changed=vendor/shim_simdjson.cpp");
    println!("cargo:rerun-if-changed=vendor/yyjson/yyjson.c");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(feature = "cbench")]
    compile_baselines();
}

#[cfg(feature = "cbench")]
fn compile_baselines() {
    // `vendor/` is excluded from the published crate: it is 11 MB of C and
    // C++ that only the baseline benchmarks need, and shipping it would
    // make every consumer download it. Fail with an explanation rather
    // than a wall of "no such file" from the C compiler.
    if !std::path::Path::new("vendor/yyjson/yyjson.c").exists() {
        println!(
            "cargo:warning=the `cbench` feature needs the vendored yyjson \
             and simdjson sources, which are not shipped in the published \
             crate. Clone the repository to run the C baselines."
        );
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
