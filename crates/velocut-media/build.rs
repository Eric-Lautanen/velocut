fn main() {
    // The QSV dispatch layer in a static FFmpeg build (libvpl's driver-store
    // loader) calls the Win32 registry API. The release binary happens to link
    // because its symbol set never pulls that object in, but any target that
    // reaches further into FFmpeg's QSV encoders — the integration tests do —
    // hits "undefined reference to `__imp_RegOpenKeyExW`" without this.
    //
    // advapi32 is a Windows system import library like kernel32, so declaring it
    // keeps the executable self-contained: nothing ships beside the binary.
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        println!("cargo:rustc-link-lib=advapi32");
    }
}
