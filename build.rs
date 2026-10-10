// Embeds the shared saishin.ico (generated from
// saishin-png-icon-set/saishin-master-1024.png via
// saishin-png-icon-set/make_saishin_ico.py) as the Windows
// executable icon. The winres dependency is Windows-only (see Cargo.toml),
// and this script is a no-op everywhere else.

#[cfg(windows)]
fn main() {
    // The icon lives outside the crate, so watch it explicitly.
    println!("cargo::rerun-if-changed=saishin-png-icon-set/saishin.ico");
    let icon = std::env::current_dir()
        .expect("current dir")
        .join("saishin-png-icon-set/saishin.ico");
    let icon_path = icon.to_string_lossy().to_string();
    winres::WindowsResource::new()
        .set_icon(&icon_path)
        .compile()
        .expect("embed saishin icon");
}

#[cfg(not(windows))]
fn main() {}
