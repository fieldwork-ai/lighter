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
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../host/out"));
    println!("cargo:rerun-if-changed={}", dir.display());
    let all = [
        "libvirglrenderer.a",
        "libvirgl.a",
        "libmesa.a",
        "libMoltenVK.a",
    ]
    .iter()
    .all(|lib| dir.join(lib).exists());
    if cfg!(target_os = "macos") && all {
        println!("cargo:rustc-link-search=native={}", dir.display());
        // MoltenVK uses `@available`, which clang lowers to a compiler-rt
        // builtin rustc does not link on its own.
        if let Ok(out) = std::process::Command::new("cc")
            .arg("-print-resource-dir")
            .output()
        {
            let res = String::from_utf8_lossy(&out.stdout).trim().to_string();
            println!("cargo:rustc-link-search=native={res}/lib/darwin");
        }
        for lib in ["virglrenderer", "virgl", "mesa", "MoltenVK", "clang_rt.osx"] {
            println!("cargo:rustc-link-lib=static={lib}");
        }
        for f in [
            "Metal",
            "Foundation",
            "IOSurface",
            "QuartzCore",
            "CoreGraphics",
            "IOKit",
            "AppKit",
        ] {
            println!("cargo:rustc-link-lib=framework={f}");
        }
        println!("cargo:rustc-link-lib=objc");
        println!("cargo:rustc-link-lib=c++");
        println!("cargo:rustc-cfg=gpu_libs");
    } else {
        println!(
            "cargo:warning=GPU renderer libraries not found in {}; building without a GPU (host/gpu/build.sh)",
            dir.display()
        );
    }

    // ONNX Runtime with its CoreML provider, as the static archives
    // host/ane/build.sh collects: every lib*.a in the directory is linked.
    println!("cargo:rustc-check-cfg=cfg(ane_libs)");
    println!("cargo:rerun-if-env-changed=LIGHTER_ANE_LIBS");
    let ane = std::env::var_os("LIGHTER_ANE_LIBS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../host/out/ort"));
    println!("cargo:rerun-if-changed={}", ane.display());
    let mut archives: Vec<String> = std::fs::read_dir(&ane)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with("lib") && n.ends_with(".a"))
                .map(|n| n[3..n.len() - 2].to_string())
                .collect()
        })
        .unwrap_or_default();
    archives.sort();
    if cfg!(target_os = "macos") && archives.iter().any(|a| a == "onnxruntime_session") {
        println!("cargo:rustc-link-search=native={}", ane.display());
        for a in &archives {
            println!("cargo:rustc-link-lib=static={a}");
        }
        for f in ["CoreML", "Foundation", "Accelerate", "Network"] {
            println!("cargo:rustc-link-lib=framework={f}");
        }
        println!("cargo:rustc-link-lib=c++");
        println!("cargo:rustc-cfg=ane_libs");
    } else {
        println!(
            "cargo:warning=ONNX Runtime archives not found in {}; building without the Neural Engine (host/ane/build.sh)",
            ane.display()
        );
    }
}
