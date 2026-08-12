fn main() {
    // Embed the app icon as a Windows resource so Explorer/taskbar show it.
    // Applies to both binaries in this crate (pterm_hook harmlessly included).
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.compile().expect("embedding assets/icon.ico failed");
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
