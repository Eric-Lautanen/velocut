fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    // Only compile Windows resources when the `winresource` feature is enabled
    // AND we're actually targeting Windows. Non-Windows builds skip this so
    // they don't pull in winresource (which only targets Windows).
    #[cfg(feature = "winresource")]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
            let mut res = winresource::WindowsResource::new();
            res.set_icon("../../assets/icon.ico");
            res.compile().expect("Failed to compile Windows resources");
        }
    }
}
