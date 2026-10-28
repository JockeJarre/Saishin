// Build-time metadata and the Windows executable icon.
//
// Version handling: Cargo.toml holds the version, this script stamps the
// build identity (git sha, target, profile) into the binary through the
// SAISHIN_* environment variables that src/main.rs reads with env!().
// On CI the sha comes from GITHUB_SHA, locally from git.

use std::env;
use std::process::Command;

fn main() {
    println!("cargo::rerun-if-changed=saishin-png-icon-set/saishin.ico");
    println!("cargo::rerun-if-changed=build.rs");

    stamp_build_identity();

    #[cfg(windows)]
    embed_icon();
}

/// Publishes SAISHIN_VERSION/GIT/TARGET/PROFILE for `env!()` in main.rs.
fn stamp_build_identity() {
    println!(
        "cargo:rustc-env=SAISHIN_VERSION={}",
        env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string())
    );
    println!(
        "cargo:rustc-env=SAISHIN_TARGET={}",
        env::var("TARGET").unwrap_or_default()
    );
    println!(
        "cargo:rustc-env=SAISHIN_PROFILE={}",
        env::var("PROFILE").unwrap_or_default()
    );
    println!("cargo:rustc-env=SAISHIN_GIT={}", git_identity());
}

/// Short commit sha, suffixed with "-dirty" when the tree has uncommitted
/// changes. On GitHub Actions GITHUB_SHA is the checked-out commit, so no
/// git binary is needed there (shallow clones make `git describe` unreliable).
fn git_identity() -> String {
    if let Some(sha) = env::var("GITHUB_SHA").ok().filter(|s| !s.is_empty()) {
        let short = sha.get(..7).unwrap_or(&sha).to_string();
        return match env::var("GITHUB_REF_TYPE").as_deref() {
            Ok("tag") => short,
            _ => format!("{short}-ci"),
        };
    }

    match Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
    {
        Ok(out) if out.status.success() => {
            let short = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let dirty = Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=no"])
                .output()
                .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
                .unwrap_or(false);
            if dirty {
                format!("{short}-dirty")
            } else {
                short
            }
        }
        _ => "unknown".to_string(),
    }
}

/// Writes the .rc into OUT_DIR and lets embed-resource find rc.exe (it
/// ships a fallback compiler, so a Windows SDK is not required). The icon
/// is presentation, not function: if the resource compiler is missing, we
/// warn and build the binary without it. The "icon freshness" CI job is
/// what actually protects the artwork.
#[cfg(windows)]
fn embed_icon() {
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR");
    let icon = env::current_dir()
        .expect("current dir")
        .join("saishin-png-icon-set")
        .join("saishin.ico");

    if !icon.exists() {
        println!("cargo:warning=saishin.ico not found, building without the app icon");
        return;
    }

    // Paths in a .rc file use forward slashes; backslashes are escapes.
    let icon_path = icon.to_string_lossy().replace('\\', "/");
    let source = format!("1 ICON \"{icon_path}\"\n");
    let rc = std::path::Path::new(&out_dir).join("saishin-icon.rc");
    if let Err(ex) = std::fs::write(&rc, source) {
        println!("cargo:warning=could not write the icon resource script: {ex}");
        return;
    }

    match embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_optional()
    {
        Ok(_) => {}
        Err(ex) => println!("cargo:warning=could not embed saishin.ico: {ex}"),
    }
}
