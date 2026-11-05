// `wayland_x11`: the Wayland and X11 desktops, which is every Unix but
// macOS (Linux, the BSDs). Cargo.toml's target tables can't use it, so they
// spell it out: cfg(all(unix, not(target_os = "macos"))).
fn main() {
    println!("cargo::rustc-check-cfg=cfg(wayland_x11)");
    let unix = std::env::var("CARGO_CFG_TARGET_FAMILY").is_ok_and(|f| f.split(',').any(|f| f == "unix"));
    let macos = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos");
    if unix && !macos {
        println!("cargo::rustc-cfg=wayland_x11");
    }
    // ScreenCaptureKit (recording) arrived in macOS 12.3, but clipt starts on
    // 12.0: weakly linked, a missing framework doesn't stop dyld, and
    // recording checks for it before use. CI checks the load command.
    if macos {
        println!("cargo::rustc-link-arg=-Wl,-weak_framework,ScreenCaptureKit");
    }
}
