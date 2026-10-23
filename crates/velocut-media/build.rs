fn main() {
    // The QSV dispatch layer in a static FFmpeg build (libvpl's registry key and
    // driver-store objects) calls the Win32 registry API. The release binary
    // happens to link because its symbol set never pulls those objects in, but
    // any target that reaches further into FFmpeg's QSV encoders — the
    // integration tests do — fails with "undefined reference to
    // `__imp_RegOpenKeyExW`" without advapi32.
    //
    // Position matters. rustc emits the crate's native libraries before some of
    // the repeats of `-lvpl` that the FFmpeg bindings contribute, and ld
    // resolves archives left to right, so a plain link-lib leaves those objects
    // unresolved. The trailing link arg places advapi32 after them.
    //
    // advapi32 is a Windows system import library like kernel32, so the
    // executable stays self-contained: nothing ships beside the binary.
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        println!("cargo:rustc-link-lib=advapi32");
        println!("cargo:rustc-link-arg-tests=-ladvapi32");
    }
}
