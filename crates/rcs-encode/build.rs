//! Builds native/amf_shim against the pinned AMF headers in third_party/amf.

use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let shim = root.join("native/amf_shim");
    println!("cargo:rerun-if-changed={}", shim.display());
    println!(
        "cargo:rerun-if-changed={}",
        root.join("third_party/amf/include").display()
    );

    cc::Build::new()
        .cpp(true)
        .file(shim.join("amf_shim.cpp"))
        .file(shim.join("amf_encoder.cpp"))
        .include(&shim)
        .include(root.join("third_party/amf/include"))
        .define("UNICODE", None)
        .define("_UNICODE", None)
        .define("NOMINMAX", None)
        .flag("/std:c++17")
        .flag("/EHsc")
        .flag("/W4")
        .compile("amf_shim");
}
