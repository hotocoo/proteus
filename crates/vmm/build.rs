//! Names the hosts with a hypervisor backend, a window, sound and gamepads: macOS on Apple Silicon
//! and Linux and Windows on x86_64. Code for them is gated on `cfg(host_board)`.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(host_board)");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if matches!(
        (os.as_str(), arch.as_str()),
        ("macos", "aarch64") | ("linux", "x86_64") | ("windows", "x86_64")
    ) {
        println!("cargo::rustc-cfg=host_board");
    }
    // The coin logo as the program's icon in Explorer, the Start menu and shortcuts.
    if os == "windows" {
        println!("cargo::rerun-if-changed=../../assets/proteus.ico");
        embed_resource::compile("proteus.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("the icon resource compiles");
    }
}
