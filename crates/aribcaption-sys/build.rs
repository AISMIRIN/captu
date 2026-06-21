use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // libaribcaption C source is vendored inside this crate at vendor/libaribcaption/.
    let src_dir = manifest_dir
        .join("vendor/libaribcaption")
        .canonicalize()
        .expect("vendor/libaribcaption not found — run: git submodule update --init --recursive");

    // Build libaribcaption as a static library via cmake.
    // cmake crate sets CMAKE_INSTALL_PREFIX to OUT_DIR; `dst` == that prefix.
    let dst = cmake::Config::new(&src_dir)
        .define("CMAKE_BUILD_TYPE", "Release")
        .define("ARIBCC_BUILD_TESTS", "OFF")
        .define("ARIBCC_SHARED_LIBRARY", "OFF")
        .build();

    // cmake installs to: dst/lib/libaribcaption.a, dst/include/aribcaption/*.h
    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-lib=static=aribcaption");

    // libaribcaption is C++17 — link the C++ runtime.
    println!("cargo:rustc-link-lib=stdc++");

    // External renderer dependencies on Linux (freetype may be embedded in the
    // static archive if cmake didn't find the system one, but fontconfig is always
    // external and must be linked explicitly).
    println!("cargo:rustc-link-lib=fontconfig");
    println!("cargo:rustc-link-lib=freetype");

    // Use the installed include dir so bindgen sees the cmake-generated aribcc_config.h
    // (not aribcc_config.h.in from the source tree).
    let include_dir = dst.join("include");
    let header = include_dir.join("aribcaption/aribcaption.h");

    let bindings = bindgen::Builder::default()
        .header(header.to_str().unwrap())
        .clang_arg(format!("-I{}", include_dir.display()))
        // Only pull in aribcc_* symbols to keep the generated file small.
        .allowlist_function("aribcc_.*")
        .allowlist_type("aribcc_.*")
        .allowlist_var("ARIBCC_.*")
        // C++ std:: types appear in .hpp headers; block them.
        .blocklist_item("std.*")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen failed to generate bindings");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out.join("bindings.rs"))
        .expect("failed to write bindings.rs");
}
