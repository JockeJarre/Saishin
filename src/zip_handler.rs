use crate::config::{ExtractOptions, RenameRule};
use crate::{glob, log};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub fn is_archive(file_name: &str) -> bool {
    let extension = Path::new(file_name)
        .extension()
        .map(|ext| ext.to_string_lossy().to_string())
        .unwrap_or_default();
    extension.eq_ignore_ascii_case("zip") || extension.eq_ignore_ascii_case("vpt")
}

pub fn extract(
    archive_path: &str,
    options: &ExtractOptions,
    force: bool,
) -> Result<(), crate::error::AppError> {
    let root = absolute_of(&options.destination);
    fs::create_dir_all(&root).map_err(|ex| crate::error::AppError::Download {
        status: 0,
        message: format!("Could not create {}: {ex}", root.display()),
    })?;

    let file = fs::File::open(archive_path).map_err(|ex| crate::error::AppError::Download {
        status: 0,
        message: format!("Could not read {archive_path}: {ex}"),
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|ex| {
        crate::error::AppError::Download {
            status: 0,
            message: format!("Could not open {archive_path}: {ex}"),
        }
    })?;

    let mut written = 0;
    let mut skipped = 0;
    // Two entries renamed to one name is a real archive layout (two platforms'
    // `app.exe` in one zip), so the collision is said out loud rather than left
    // to the last write winning in silence.
    let mut renamed_targets: std::collections::HashSet<String> = std::collections::HashSet::new();

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|ex| {
            crate::error::AppError::Download {
                status: 0,
                message: format!("Could not read {archive_path}: {ex}"),
            }
        })?;
        let raw_name = entry.name().to_string();
        if entry.is_dir() || raw_name.is_empty() {
            continue;
        }

        let relative = normalize(&raw_name);
        if has_traversal(&relative) {
            log::warn(&format!(
                "refusing to extract an entry with a parent-directory segment: {raw_name}"
            ));
            continue;
        }
        let Some(relative) = strip_prefix(&relative, options.strip_prefix) else {
            continue;
        };
        if !glob::matches_any(&relative, &options.patterns) {
            continue;
        }

        let target_rule = options
            .renames
            .iter()
            .find(|rule| glob::matches(&relative, &rule.pattern));
        let written_as = match target_rule {
            Some(rule) => rule.target.as_str(),
            None => relative.as_str(),
        };
        let target = join_inside(&root, written_as);
        let Some(target) = target else {
            log::warn(&format!(
                "refusing to extract outside the destination: {raw_name}"
            ));
            continue;
        };
        if let Some(rule) = target_rule {
            if !renamed_targets.insert(rule.target.clone()) {
                log::warn(&format!(
                    "more than one entry maps to {}, the last one extracted wins",
                    rule.target
                ));
            }
        }

        if target.exists() && !force && !options.overwrite {
            skipped += 1;
            continue;
        }
        if let Some(parent) = target.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|ex| {
                    crate::error::AppError::Download {
                        status: 0,
                        message: format!("Could not create {}: {ex}", parent.display()),
                    }
                })?;
            }
        }

        let mut contents = Vec::new();
        entry.read_to_end(&mut contents).map_err(|ex| {
            crate::error::AppError::Download {
                status: 0,
                message: format!("Could not read {raw_name}: {ex}"),
            }
        })?;
        fs::write(&target, contents).map_err(|ex| {
            crate::error::AppError::Download {
                status: 0,
                message: format!("Could not write {}: {ex}", target.display()),
            }
        })?;
        written += 1;
        match target_rule {
            Some(rule) => log::progress(&format!("extracted {relative} as {}", rule.target)),
            None => log::progress(&format!("extracted {relative}")),
        }
    }

    let summary = format!("archive: {written} extracted, {skipped} skipped");
    if written > 0 {
        log::progress(&summary);
        log::progress_done();
    } else {
        log::info(&format!("    {summary}"));
    }
    Ok(())
}

fn absolute_of(path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(candidate)
    }
}

/// Joins a `/`-separated archive path onto the destination root, returning
/// `None` when the result would escape the root.
fn join_inside(root: &Path, relative: &str) -> Option<PathBuf> {
    let mut target = root.to_path_buf();
    for segment in relative.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return None;
        }
        target.push(segment);
    }
    if target == *root {
        return None;
    }
    if !target.starts_with(root) {
        return None;
    }
    Some(target)
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches('/').to_string()
}

fn has_traversal(path: &str) -> bool {
    path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .any(|segment| segment == "..")
}

fn strip_prefix(path: &str, count: i64) -> Option<String> {
    if count <= 0 {
        return Some(path.to_string());
    }
    let segments: Vec<&str> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() as i64 <= count {
        return None;
    }
    Some(segments[count as usize..].join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn workdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("saishin-zip-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test workdir");
        dir
    }

    fn make_archive(path: &Path, entries: &[(&str, &str)]) {
        let file = std::fs::File::create(path).expect("create archive");
        let mut writer = zip::ZipWriter::new(file);
        for (name, contents) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .expect("start entry");
            writer.write_all(contents.as_bytes()).expect("write entry");
        }
        writer.finish().expect("finish archive");
    }

    fn options(destination: &Path, patterns: &[&str]) -> ExtractOptions {
        ExtractOptions {
            patterns: patterns.iter().map(|s| s.to_string()).collect(),
            destination: destination.to_string_lossy().to_string(),
            ..ExtractOptions::default()
        }
    }

    fn rules(pairs: &[(&str, &str)]) -> Vec<RenameRule> {
        pairs
            .iter()
            .map(|(pattern, target)| RenameRule {
                pattern: pattern.to_string(),
                target: target.to_string(),
            })
            .collect()
    }

    #[test]
    fn filters_patterns_and_strips_prefix() {
        let dir = workdir("filter");
        let archive = dir.join("a.zip");
        make_archive(
            &archive,
            &[
                ("wrapper/keep.txt", "payload"),
                ("wrapper/skip.log", "log"),
                ("top.txt", "top"),
            ],
        );
        let destination = dir.join("out");
        let mut opts = options(&destination, &["*.txt"]);
        opts.strip_prefix = 1;

        extract(archive.to_str().unwrap(), &opts, false).expect("extract");

        assert_eq!(
            std::fs::read_to_string(destination.join("keep.txt")).expect("kept file"),
            "payload"
        );
        assert!(!destination.join("skip.log").exists());
        // "top.txt" has no first segment to strip, so it is skipped.
        assert!(!destination.join("top.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_parent_directory_traversal() {
        let dir = workdir("traversal");
        let archive = dir.join("evil.zip");
        make_archive(
            &archive,
            &[
                ("../../evil.txt", "x"),
                ("good/ok.txt", "ok"),
            ],
        );
        let destination = dir.join("out");

        extract(
            archive.to_str().unwrap(),
            &options(&destination, &["*"]),
            false,
        )
        .expect("extract");

        assert_eq!(
            std::fs::read_to_string(destination.join("good/ok.txt")).expect("ok file"),
            "ok"
        );
        assert!(!dir.join("evil.txt").exists());
        assert!(!dir.join("out/evil.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn existing_files_are_skipped_without_overwrite_or_force() {
        let dir = workdir("overwrite");
        let archive = dir.join("a.zip");
        make_archive(&archive, &[("data.txt", "new")]);
        let destination = dir.join("out");
        std::fs::create_dir_all(&destination).expect("out dir");
        std::fs::write(destination.join("data.txt"), "old").expect("seed");

        let opts = options(&destination, &["*"]);
        extract(archive.to_str().unwrap(), &opts, false).expect("extract");
        assert_eq!(
            std::fs::read_to_string(destination.join("data.txt")).expect("file"),
            "old"
        );

        extract(archive.to_str().unwrap(), &opts, true).expect("forced extract");
        assert_eq!(
            std::fs::read_to_string(destination.join("data.txt")).expect("file"),
            "new"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_as_renames_matched_entries() {
        let dir = workdir("rename");
        let archive = dir.join("a.zip");
        make_archive(
            &archive,
            &[
                ("wrapper/tool.exe", "exe"),
                ("wrapper/tool.dll", "dll"),
                ("wrapper/docs/README.md", "readme"),
            ],
        );
        let destination = dir.join("out");
        let mut opts = options(&destination, &["*"]);
        opts.strip_prefix = 1;
        opts.renames = rules(&[
            ("*.exe", "bin/mytool.exe"),
            ("docs/README.md", "README.txt"),
        ]);

        extract(archive.to_str().unwrap(), &opts, false).expect("extract");

        assert_eq!(
            std::fs::read_to_string(destination.join("bin/mytool.exe")).expect("renamed exe"),
            "exe"
        );
        assert!(!destination.join("tool.exe").exists(), "the old name is gone");
        assert_eq!(
            std::fs::read_to_string(destination.join("README.txt")).expect("renamed readme"),
            "readme"
        );
        // An entry no rule matches keeps the path it has in the archive.
        assert_eq!(
            std::fs::read_to_string(destination.join("tool.dll")).expect("untouched dll"),
            "dll"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_first_matching_rule_wins() {
        let dir = workdir("rename-first");
        let archive = dir.join("a.zip");
        make_archive(&archive, &[("app.exe", "exe")]);
        let destination = dir.join("out");
        let mut opts = options(&destination, &["*"]);
        opts.renames = rules(&[("*", "any.exe"), ("*.exe", "never.exe")]);

        extract(archive.to_str().unwrap(), &opts, false).expect("extract");

        assert_eq!(
            std::fs::read_to_string(destination.join("any.exe")).expect("first rule wins"),
            "exe"
        );
        assert!(!destination.join("never.exe").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_archives_case_insensitively() {
        assert!(is_archive("tables.zip"));
        assert!(is_archive("TABLES.ZIP"));
        assert!(is_archive("tables.vpt"));
        assert!(is_archive("tables.VPT"));
        assert!(!is_archive("tables.txt"));
        assert!(!is_archive("noextension"));
    }
}
