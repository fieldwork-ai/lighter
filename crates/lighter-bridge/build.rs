//! The peer check is C: it is Security framework calls on an audit token.
fn main() {
    println!("cargo:rerun-if-changed=src/peer.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    cc::Build::new()
        .file("src/peer.c")
        .flag("-Wall")
        .flag("-Werror")
        .compile("lighter_bridge_peer");
    println!("cargo:rustc-link-lib=framework=Security");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
}
