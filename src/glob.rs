use regex::Regex;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn cache() -> &'static Mutex<HashMap<String, Regex>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Regex>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn case_insensitive() -> bool {
    cfg!(windows) || cfg!(target_os = "macos")
}

fn translate(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 2);
    out.push('^');
    let bytes: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            '*' => {
                if i + 1 < bytes.len() && bytes[i + 1] == '*' {
                    out.push_str(".*");
                    i += 1;
                } else {
                    out.push_str("[^/]*");
                }
            }
            '?' => out.push_str("[^/]"),
            c => {
                out.push_str(&regex::escape(&c.to_string()));
            }
        }
        i += 1;
    }
    out.push('$');
    if case_insensitive() {
        format!("(?i){out}")
    } else {
        out
    }
}

pub fn compile(pattern: &str) -> Regex {
    {
        let guard = cache().lock().expect("glob cache lock");
        if let Some(existing) = guard.get(pattern) {
            return existing.clone();
        }
    }
    let compiled = Regex::new(&translate(pattern)).expect("valid glob regex");
    cache()
        .lock()
        .expect("glob cache lock")
        .insert(pattern.to_string(), compiled.clone());
    compiled
}

/// Full-path match first, then — when the pattern contains no `/` or `\` —
/// a second try against the file name alone, so `*.vpt` catches tables
/// nested at any depth.
pub fn matches(value: &str, pattern: &str) -> bool {    if compile(pattern).is_match(value) {
        return true;
    }
    if pattern.contains('/') || pattern.contains('\\') {
        return false;
    }
    match value.rfind('/') {
        Some(separator) => compile(pattern).is_match(&value[separator + 1..]),
        None => false,
    }
}

/// `!`-prefixed patterns exclude, any positive match includes, and an
/// all-negative (or empty) list matches everything.
pub fn matches_any(value: &str, patterns: &[String]) -> bool {
    let mut matched = false;
    let mut has_positive = false;
    for pattern in patterns {
        if let Some(negated) = pattern.strip_prefix('!') {
            if matches(value, negated) {
                return false;
            }
        } else {
            has_positive = true;
            if matches(value, pattern) {
                matched = true;
            }
        }
    }
    if has_positive {
        matched
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn star_does_not_cross_directories_but_star_star_does() {
        assert!(matches("tables/a.vpt", "*.vpt"));
        assert!(matches("a.vpt", "*.vpt"));
        assert!(matches("tables/a.vpt", "tables/*.vpt"));
        assert!(!matches("tables/sub/a.vpt", "tables/*.vpt"));
        assert!(matches("tables/sub/a.vpt", "tables/**"));
        assert!(matches("a.vpt", "**.vpt"));
    }

    #[test]
    fn pattern_without_slash_falls_back_to_file_name() {
        // A pattern without `/` also matches the bare file name, which is
        // what makes `*.vpt` catch nested tables.
        assert!(matches("sub/dir/table.vpt", "*.vpt"));
        assert!(!matches("sub/dir/table.vpt", "sub/*.vpt"));
        assert!(matches("sub/dir/table.vpt", "sub/**"));
    }

    #[test]
    fn question_mark_matches_single_non_separator() {
        assert!(matches("ab", "?b"));
        assert!(!matches("/b", "?b"));
        assert!(!matches("abc", "?b"));
    }

    #[test]
    fn exclusions_win_over_positive_matches() {
        let filters = patterns(&["VPinballX*.zip", "!*-dev-*"]);
        assert!(matches_any("VPinballX-windows-x64-Release.zip", &filters));
        assert!(!matches_any("VPinballX-dev-third-party-msvc.zip", &filters));
        assert!(!matches_any("readme.txt", &filters));
    }

    #[test]
    fn empty_and_all_negative_lists_match_everything() {
        assert!(matches_any("anything.bin", &[]));
        assert!(matches_any("anything.bin", &patterns(&["!*.tmp"])));
        assert!(!matches_any("a.tmp", &patterns(&["!*.tmp"])));
    }

    #[test]
    fn case_follows_platform_filesystem_conventions() {
        // Case-insensitive on Windows/macOS, case-sensitive on Linux.
        assert_eq!(matches("TABLE.VPT", "*.vpt"), case_insensitive());
    }
}
