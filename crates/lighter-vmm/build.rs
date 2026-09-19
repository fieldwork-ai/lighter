//! Links the GPU renderer when its libraries are present.
//!
//! `LIGHTER_GPU_LIBS` names a directory holding `libvirglrenderer.a`,
//! `libvirgl.a`, `libmesa.a` and `libMoltenVK.a` (built by `host/gpu/build.sh`
//! into `host/out`, the default). Without them the crate still builds, with
//! the renderer stubbed out and the GPU device refusing every context.
use std::path::PathBuf;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(gpu_libs)");
    println!("cargo:rerun-if-env-changed=LIGHTER_GPU_LIBS");
    let dir = std::env::var_os("LIGHTER_GPU_LIBS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../host/out")
        });
    println!("cargo:rerun-if-changed={}", dir.display());
    let all = ["libvirglrenderer.a", "libvirgl.a", "libmesa.a", "libMoltenVK.a"]
        .iter()
        .all(|lib| dir.join(lib).exists());
    if cfg!(target_os = "macos") && all {
        println!("cargo:rustc-link-search=native={}", dir.display());
        // MoltenVK uses `@available`, which clang lowers to a compiler-rt
        // builtin rustc does not link on its own.
        if let Ok(out) = std::process::Command::new("cc").arg("-print-resource-dir").output() {
            let res = String::from_utf8_lossy(&out.stdout).trim().to_string();
            println!("cargo:rustc-link-search=native={res}/lib/darwin");
        }
        println!("cargo:rustc-cfg=gpu_libs");
    } else {
        println!(
            "cargo:warning=GPU renderer libraries not found in {}; building without a GPU (host/gpu/build.sh)",
            dir.display()
        );
    }
}
