fn main() {
    // macOS: the agent of the sign-in screen is this program. WindowServer lets a
    // program work there as a "pre-login app" only when its binary has this section
    // (Apple's CGPreLoginApp sample): without it the screen was captured, but the
    // mouse and keyboard did nothing.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-Wl,-sectcreate,__CGPreLoginApp,__cgpreloginapp,/dev/null");
    }
    // Windows: the DLLs this program imports are loaded from System32 only, never
    // from the program's folder first (APP-M1). The copy started from Downloads
    // (zero-config file names), also as administrator, otherwise loaded a
    // version.dll, dwmapi.dll, winmm.dll... put next to it. Windows 10 1607 and
    // later honour the flag (LOAD_LIBRARY_SEARCH_SYSTEM32); older ones ignore it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    }
    tauri_build::build()
}
