fn main() {
    // macOS: the agent of the sign-in screen is this program. WindowServer lets a
    // program work there as a "pre-login app" only when its binary has this section
    // (Apple's CGPreLoginApp sample): without it the screen was captured, but the
    // mouse and keyboard did nothing.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-Wl,-sectcreate,__CGPreLoginApp,__cgpreloginapp,/dev/null");
    }
    tauri_build::build()
}
