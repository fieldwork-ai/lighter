//! Compiles the vmnet relay. C rather than Rust because vmnet's API is
//! blocks on dispatch queues, which clang compiles and Rust would have to
//! imitate by hand.
fn main() {
    println!("cargo:rerun-if-changed=src/relay.c");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    cc::Build::new()
        .file("src/relay.c")
        .flag("-fblocks")
        .flag("-Wall")
        .flag("-Werror")
        .compile("lighter_vmnet_relay");
    println!("cargo:rustc-link-lib=framework=vmnet");
}
