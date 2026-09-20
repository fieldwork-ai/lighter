fn main() {
    let inc = std::env::var("ORT_INCLUDE").expect("ORT_INCLUDE");
    let bindings = bindgen::Builder::default()
        .header(format!("{inc}/onnxruntime_c_api.h"))
        .clang_arg(format!("-I{inc}"))
        .allowlist_type("Ort.*")
        .allowlist_var("ORT_API_VERSION")
        .allowlist_function("OrtGetApiBase")
        .layout_tests(false)
        .generate()
        .expect("bindgen");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    bindings.write_to_file(out.join("ort.rs")).unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
