// Build-time metadata and the Windows executable icon.
//
// Version handling: Cargo.toml holds the version, this script stamps the
// build identity (git sha, target, profile) into the binary through the
// SAISHIN_* environment variables that src/main.rs reads with env!().
// On CI the sha comes from GITHUB_SHA, locally from git.

use std::env;
use std::process::Command;

/// The square artwork the GUI header draws, and how far it is shrunk on the
/// way in: the 1024 px PNG becomes a 256 px texture, which is what a 120 pt
/// logo takes on a 2x display.
const LOGO_PNG: &str = "saishin-png-icon-set/saishin-master-1024.png";
const LOGO_SHRINK: usize = 4;

fn main() {
    println!("cargo::rerun-if-changed=saishin-png-icon-set/saishin.ico");
    println!("cargo::rerun-if-changed=build.rs");

    watch_git_identity();

    stamp_build_identity();
    embed_logo();

    #[cfg(windows)]
    embed_icon();
}

/// Makes this script run again when the commit the tree sits on changes, which
/// is what keeps a local `-V` telling the truth: a commit is not a file Cargo
/// watches, and a checkout that only moves HEAD is not a file change either.
/// The paths are asked of git, because "where HEAD is" differs between a normal
/// checkout, a worktree and a detached HEAD, and a watched path that does not
/// exist reruns the script on every build, so each one is checked first.
fn watch_git_identity() {
    emit_watch(git_path("HEAD"));

    // HEAD names the branch; the branch's ref file is what a commit moves. A
    // detached HEAD holds a sha and names nothing, and a branch packed into
    // packed-refs has no loose file: both cases leave a path that is missing.
    let head = git_path("HEAD");
    let loose = head
        .as_ref()
        .and_then(|head| std::fs::read_to_string(head).ok())
        .and_then(|text| {
            let name = text.trim().strip_prefix("ref: ")?.trim().to_string();
            git_path(&name)
        });
    emit_watch(loose);
    emit_watch(git_path("packed-refs"));
}

/// Where git keeps a path, relative to the package root this script runs in.
fn git_path(path: &str) -> Option<std::path::PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-path", path])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then(|| std::path::PathBuf::from(text))
}

fn emit_watch(path: Option<std::path::PathBuf>) {
    if let Some(path) = path.filter(|path| path.exists()) {
        println!("cargo::rerun-if-changed={}", path.display());
    }
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
/// is presentation, not function: a missing .ico, or a missing resource
/// compiler, only warns and the binary builds without it. The promise is
/// kept in CI instead: the jobs that ship a binary regenerate the .ico from
/// the masters, and scripts/check-icon.ps1 fails the run when the shipped
/// executable carries no icon resource.
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

/// Decodes the header logo and writes it into OUT_DIR as raw RGBA8, which
/// src/gui.rs embeds with include_bytes!(). Raw pixels rather than the PNG
/// bytes, so that the texture is uploaded from the embedded bytes as they
/// are: showing the logo needs no image decoder in the binary. The PNG is
/// 285 KB, the shrunk raw texture 256 KB and ready to hand to the GPU.
///
/// Unlike the icon, a missing logo stops the build: a binary without a header
/// logo is broken, not merely unbranded.
fn embed_logo() {
    println!("cargo::rerun-if-changed={LOGO_PNG}");

    let bytes = match std::fs::read(LOGO_PNG) {
        Ok(bytes) => bytes,
        Err(ex) => panic!("{LOGO_PNG} is missing, and the GUI header needs it: {ex}"),
    };
    let (width, height, rgba) =
        decode_rgba8(&bytes).unwrap_or_else(|ex| panic!("{LOGO_PNG} cannot be decoded: {ex}"));

    if width != height {
        panic!("{LOGO_PNG} is {width}x{height}; the header logo needs a square image");
    }
    if width % LOGO_SHRINK != 0 {
        panic!("{LOGO_PNG} is {width} px wide, which does not divide by {LOGO_SHRINK}");
    }

    let size = width / LOGO_SHRINK;
    println!("cargo:rustc-env=SAISHIN_LOGO_SIZE={size}");

    let pixels = box_shrink(&rgba, width, LOGO_SHRINK);
    let dest = std::path::Path::new(&env::var("OUT_DIR").expect("OUT_DIR")).join("saishin-logo.rgba");
    if let Err(ex) = std::fs::write(&dest, pixels) {
        panic!("{} cannot be written: {ex}", dest.display());
    }
}

/// Decodes a PNG into 8-bit RGBA. The `png` crate is a build dependency, so
/// this is the only place a PNG decoder exists in the project.
fn decode_rgba8(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // EXPAND takes care of palettes, grayscale and sub-8-bit samples, ALPHA
    // guarantees an alpha channel, STRIP_16 keeps the sample depth at 8 bits.
    decoder.set_transformations(png::Transformations::ALPHA | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|ex| ex.to_string())?;
    let (width, height) = {
        let info = reader.info();
        (info.width as usize, info.height as usize)
    };
    let mut buffer = vec![0u8; reader.output_buffer_size().ok_or("the size of the PNG is unknown")?];
    let frame = reader.next_frame(&mut buffer).map_err(|ex| ex.to_string())?;

    if frame.color_type != png::ColorType::Rgba || frame.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "the logo decodes to colour type {} at {} bit, which the header cannot use",
            frame.color_type as u8, frame.bit_depth as u8
        ));
    }

    Ok((width, height, buffer))
}

/// Averages `shrink` x `shrink` blocks of RGBA pixels into one pixel each.
///
/// A plain box filter on straight (not premultiplied) alpha: cheap, and the
/// header draws the logo at a fraction of this size, so the slight darkening
/// of the edges is not visible there.
fn box_shrink(rgba: &[u8], width: usize, shrink: usize) -> Vec<u8> {
    let side = width / shrink;
    let mut out = vec![0u8; side * side * 4];

    for y in 0..side {
        for x in 0..side {
            let (mut red, mut green, mut blue, mut alpha, mut count) = (0u32, 0u32, 0u32, 0u32, 0u32);

            for source_y in y * shrink..(y + 1) * shrink {
                for source_x in x * shrink..(x + 1) * shrink {
                    let at = (source_y * width + source_x) * 4;
                    red += u32::from(rgba[at]);
                    green += u32::from(rgba[at + 1]);
                    blue += u32::from(rgba[at + 2]);
                    alpha += u32::from(rgba[at + 3]);
                    count += 1;
                }
            }

            let at = (y * side + x) * 4;
            out[at] = (red / count) as u8;
            out[at + 1] = (green / count) as u8;
            out[at + 2] = (blue / count) as u8;
            out[at + 3] = (alpha / count) as u8;
        }
    }

    out
}
