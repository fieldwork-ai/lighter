fn main() {
    let v = "/Users/admin/spike/virglrenderer/build-dload/src";
    println!("cargo:rustc-link-search=native={v}");
    println!("cargo:rustc-link-search=native={v}/mesa");
    println!("cargo:rustc-link-lib=static=virglrenderer");
    println!("cargo:rustc-link-lib=static=virgl");
    println!("cargo:rustc-link-lib=static=mesa");
    for f in ["Metal", "Foundation", "IOSurface", "QuartzCore", "CoreGraphics", "IOKit"] { println!("cargo:rustc-link-lib=framework={f}"); }
    println!("cargo:rustc-link-lib=objc");
}
