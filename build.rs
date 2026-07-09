use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=packaging/PKGBUILD");
    let pkgbuild = fs::read_to_string("packaging/PKGBUILD").unwrap_or_default();
    let version = pkgbuild
        .lines()
        .find_map(|l| l.strip_prefix("pkgver="))
        .unwrap_or("0.0.0")
        .trim()
        .to_string();
    println!("cargo:rustc-env=WEBTORAPP_VERSION={version}");

    // Without this, Explorer/Start Menu/taskbar/desktop shortcuts all show
    // the generic blank exe icon - they read the .exe's own embedded PE
    // resource, not anything set at runtime through the window.
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rerun-if-changed=icons/icon.ico");
        winresource::WindowsResource::new()
            .set_icon("icons/icon.ico")
            .compile()
            .expect("embed windows icon resource");
    }
}
