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
}
