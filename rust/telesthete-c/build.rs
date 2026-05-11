use std::env;
use std::path::PathBuf;

fn main() {
    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    // Header lives under the spec crate's include/:
    //   rust/telesthete/include/telesthete.h
    let out_path = PathBuf::from(&crate_dir)
        .join("..")
        .join("telesthete")
        .join("include")
        .join("telesthete.h");
    std::fs::create_dir_all(out_path.parent().unwrap()).unwrap();
    cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_language(cbindgen::Language::C)
        .with_pragma_once(true)
        .with_include_guard("TELESTHETE_H")
        .with_documentation(true)
        .with_no_includes()
        .with_sys_include("stdint.h")
        .with_sys_include("stddef.h")
        .generate()
        .map(|b| b.write_to_file(&out_path))
        .ok();
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=build.rs");
}
