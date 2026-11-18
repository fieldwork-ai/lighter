//! The peer check is C: it is Security framework calls on an audit token.
//! Only the daemon checks its peers, so only the binary links it: the
//! library is the client's too, and lighter's machine has no use for it.
fn main() {
    println!("cargo:rerun-if-changed=src/peer.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    cc::Build::new()
        .file("src/peer.c")
        .flag("-Wall")
        .flag("-Werror")
        .cargo_metadata(false)
        .compile("lighter_helper_peer");
    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    println!("cargo:rustc-link-arg-bins={out}/liblighter_helper_peer.a");
    for framework in ["Security", "CoreFoundation"] {
        println!("cargo:rustc-link-arg-bins=-framework");
        println!("cargo:rustc-link-arg-bins={framework}");
    }
}
