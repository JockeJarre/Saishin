use crate::error::AppError;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const GENERAL_SECTION: &str = "general";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    GitHub,
    Url,
    Actions,
}

#[derive(Debug, Clone, Default)]
pub struct ExtractOptions {
    pub patterns: Vec<String>,
    pub destination: String,
    pub strip_prefix: i64,
    pub overwrite: bool,
    pub keep_zip: bool,
}

#[derive(Debug, Clone)]
pub struct SourceConfig {
    pub name: String,
    pub kind: SourceKind,
    pub owner: String,
    pub repo: String,
    pub tag: String,
    pub private: bool,
    pub workflow: String,
    pub max_runs: i64,
    pub releases: i64,
    pub keep: i64,
    pub changelog: bool,
    pub version_suffix: bool,
    pub urls: Vec<String>,
    pub filters: Vec<String>,
    pub out_dir: String,
    pub extract: Option<ExtractOptions>,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    // Base directory of the INI file; currently informational only.
    #[allow(dead_code)]
    pub base_directory: String,
    pub default_out_dir: String,
    pub github_api_url: String,
    pub keyring_service: String,
    pub window_centered: bool,
    /// What the window does when a run ends. False keeps it open, on the end
    /// state with a Close button, so the log strip stays readable.
    pub close_on_finish: bool,
    /// Open the GUI by default (`[general] gui`). `--gui` and `--no-gui` on the
    /// command line win over this.
    pub gui: bool,
    pub releases: i64,
    pub sources: Vec<SourceConfig>,
}

type Sections = HashMap<String, HashMap<String, String>>;

pub fn load(path: &str) -> Result<AppConfig, AppError> {
    if !Path::new(path).exists() {
        return Err(AppError::Config(format!(
            "Configuration file not found: {path}"
        )));
    }
    // The no-escape reader on purpose: rust-ini's default reads '\' as an
    // escape character and drops it, which ate the separators of every Windows
    // path (`out = C:\Saishin\tables` arrived as `C:Saishinstables`). A
    // backslash in a value means a path, and the file is read as written.
    let ini = ini::Ini::load_from_file_noescape(path)
        .map_err(|ex| AppError::Config(format!("Could not parse {path}: {ex}")))?;

    let mut sections: Sections = HashMap::new();
    for (section, properties) in ini.iter() {
        let name = section.unwrap_or(GENERAL_SECTION).to_lowercase();
        let entry = sections.entry(name).or_default();
        for (key, value) in properties.iter() {
            entry.insert(key.to_lowercase(), value.to_string());
        }
    }

    let full = to_full_path(path);
    let base_dir = parent_of(&full);
    let github_api_url = get_string(&sections, GENERAL_SECTION, "github_api_url", "https://api.github.com")
        .trim_end_matches('/')
        .to_string();

    let mut config = AppConfig {
        base_directory: base_dir.clone(),
        default_out_dir: get_string(&sections, GENERAL_SECTION, "out_dir", "downloads"),
        github_api_url,
        keyring_service: get_string(&sections, GENERAL_SECTION, "keyring_service", "saishin"),
        window_centered: get_bool(&sections, GENERAL_SECTION, "window_centered", true)?,
            close_on_finish: get_bool(&sections, GENERAL_SECTION, "close_on_finish", false)?,
        gui: get_bool(&sections, GENERAL_SECTION, "gui", false)?,
        releases: get_int(&sections, GENERAL_SECTION, "releases", 10)?,
        sources: Vec::new(),
    };

    if config.releases < 1 {
        return Err(AppError::Config(
            "Section [general]: 'releases' must be at least 1.".to_string(),
        ));
    }

    // Preserve file order for sources.
    let mut ordered: Vec<String> = Vec::new();
    for (section, _) in ini.iter() {
        let name = section.unwrap_or(GENERAL_SECTION).to_string();
        if name.eq_ignore_ascii_case(GENERAL_SECTION) {
            continue;
        }
        if !ordered.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            ordered.push(name);
        }
    }

    for name in ordered {
        if let Some(source) = read_source(&sections, &name, &config.default_out_dir, &base_dir, config.releases)? {
            config.sources.push(source);
        }
    }

    if config.sources.is_empty() {
        return Err(AppError::Config(format!(
            "No sources defined in {path}. Every section other than [{GENERAL_SECTION}] needs a 'type' of github or url."
        )));
    }

    Ok(config)
}

fn read_source(
    sections: &Sections,
    section: &str,
    default_out: &str,
    base_dir: &str,
    default_releases: i64,
) -> Result<Option<SourceConfig>, AppError> {
    let raw_type = get_string(sections, section, "type", "");
    let source_type = raw_type.to_lowercase();
    if source_type.is_empty() {
        return Err(AppError::Config(format!(
            "Section [{section}] is missing the required 'type' key."
        )));
    }

    let out_dir = resolve_output_directory(sections, section, default_out, base_dir);
    let default_destination = Path::new(&out_dir).join(section).to_string_lossy().to_string();

    let mut source = match source_type.as_str() {
        "github" => {
            let owner = get_string(sections, section, "owner", "");
            let repo = get_string(sections, section, "repo", "");
            if owner.is_empty() || repo.is_empty() {
                return Err(AppError::Config(format!(
                    "Section [{section}] of type github requires both 'owner' and 'repo'."
                )));
            }
            SourceConfig {
                name: section.to_string(),
                kind: SourceKind::GitHub,
                owner,
                repo,
                tag: get_string(sections, section, "tag", "latest"),
                private: get_bool(sections, section, "private", false)?,
                workflow: String::new(),
                max_runs: 10,
                releases: get_int(sections, section, "releases", default_releases)?,
                keep: 0,
                changelog: false,
                version_suffix: false,
                urls: Vec::new(),
                filters: Vec::new(),
                out_dir,
                extract: read_extract(sections, section, base_dir, &default_destination)?,
            }
        }
        "url" => SourceConfig {
            name: section.to_string(),
            kind: SourceKind::Url,
            owner: String::new(),
            repo: String::new(),
            tag: "latest".to_string(),
            private: false,
            workflow: String::new(),
            max_runs: 10,
            releases: get_int(sections, section, "releases", default_releases)?,
            keep: 0,
            changelog: false,
            version_suffix: false,
            urls: Vec::new(),
            filters: Vec::new(),
            out_dir,
            extract: read_extract(sections, section, base_dir, &default_destination)?,
        },
        "actions" => {
            let owner = get_string(sections, section, "owner", "");
            let repo = get_string(sections, section, "repo", "");
            if owner.is_empty() || repo.is_empty() {
                return Err(AppError::Config(format!(
                    "Section [{section}] of type actions requires both 'owner' and 'repo'."
                )));
            }
            SourceConfig {
                name: section.to_string(),
                kind: SourceKind::Actions,
                owner,
                repo,
                tag: "latest".to_string(),
                private: false,
                workflow: get_string(sections, section, "workflow", ""),
                max_runs: get_int(sections, section, "runs", 10)?,
                releases: get_int(sections, section, "releases", default_releases)?,
                keep: 0,
                changelog: false,
                version_suffix: false,
                urls: Vec::new(),
                filters: Vec::new(),
                out_dir,
                extract: read_extract(sections, section, base_dir, &default_destination)?,
            }
        }
        _ => {
            return Err(AppError::Config(format!(
                "Section [{section}] has unknown type '{source_type}'. Use 'github' or 'url'."
            )));
        }
    };

    for value in get_list(sections, section, "filters") {
        source.filters.push(value);
    }

    source.keep = get_int(sections, section, "keep", 0)?;
    source.changelog = get_bool(sections, section, "changelog", false)?;
    source.version_suffix = get_bool(sections, section, "version_suffix", false)?;
    if source.keep < 0 {
        return Err(AppError::Config(format!(
            "Section [{section}]: 'keep' cannot be negative."
        )));
    }
    if source.max_runs < 1 {
        return Err(AppError::Config(format!(
            "Section [{section}]: 'runs' must be at least 1."
        )));
    }
    if source.releases < 1 {
        return Err(AppError::Config(format!(
            "Section [{section}]: 'releases' must be at least 1."
        )));
    }

    if source.kind == SourceKind::Url {
        let urls = get_list(sections, section, "urls");
        if urls.is_empty() {
            return Err(AppError::Config(format!(
                "Section [{section}] of type url requires at least one entry in 'urls'."
            )));
        }
        source.urls.extend(urls);
    }

    Ok(Some(source))
}

fn read_extract(
    sections: &Sections,
    section: &str,
    base_dir: &str,
    default_destination: &str,
) -> Result<Option<ExtractOptions>, AppError> {
    let patterns = get_list(sections, section, "extract");
    if patterns.is_empty() {
        return Ok(None);
    }
    let destination = get_string(sections, section, "extract_to", "");
    let resolved = if destination.is_empty() {
        default_destination.to_string()
    } else {
        resolve_path(&destination, base_dir)
    };
    let strip_prefix = get_int(sections, section, "strip_prefix", 0)?;
    if strip_prefix < 0 {
        return Err(AppError::Config(format!(
            "Section [{section}]: 'strip_prefix' cannot be negative."
        )));
    }
    Ok(Some(ExtractOptions {
        patterns,
        destination: resolved,
        strip_prefix,
        overwrite: get_bool(sections, section, "overwrite", false)?,
        keep_zip: get_bool(sections, section, "keep_zip", true)?,
    }))
}

fn resolve_output_directory(
    sections: &Sections,
    section: &str,
    default_out: &str,
    base_dir: &str,
) -> String {
    let configured = get_string(sections, section, "out", "");
    let relative = if configured.is_empty() {
        default_out.to_string()
    } else {
        configured
    };
    resolve_path(&relative, base_dir)
}

pub fn resolve_path(path: &str, base_dir: &str) -> String {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_string_lossy().to_string()
    } else {
        Path::new(base_dir)
            .join(candidate)
            .to_string_lossy()
            .to_string()
    }
}

fn to_full_path(path: &str) -> String {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_string_lossy().to_string()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(candidate)
            .to_string_lossy()
            .to_string()
    }
}

fn parent_of(full: &str) -> String {
    Path::new(full)
        .parent()
        .map(|p| {
            let text = p.to_string_lossy().to_string();
            if text.is_empty() {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .to_string_lossy()
                    .to_string()
            } else {
                text
            }
        })
        .unwrap_or_else(|| ".".to_string())
}

fn lookup<'a>(sections: &'a Sections, section: &str, key: &str) -> Option<&'a String> {
    sections
        .get(&section.to_lowercase())
        .and_then(|keys| keys.get(&key.to_lowercase()))
}

fn get_string(sections: &Sections, section: &str, key: &str, fallback: &str) -> String {
    match lookup(sections, section, key) {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                fallback.to_string()
            } else {
                trimmed.to_string()
            }
        }
        None => fallback.to_string(),
    }
}

fn get_bool(
    sections: &Sections,
    section: &str,
    key: &str,
    fallback: bool,
) -> Result<bool, AppError> {
    let raw = get_string(sections, section, key, "");
    if raw.is_empty() {
        return Ok(fallback);
    }
    match raw.to_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(AppError::Config(format!(
            "Section [{section}]: '{key}' must be true or false, not '{raw}'."
        ))),
    }
}

fn get_int(
    sections: &Sections,
    section: &str,
    key: &str,
    fallback: i64,
) -> Result<i64, AppError> {
    let raw = get_string(sections, section, key, "");
    if raw.is_empty() {
        return Ok(fallback);
    }
    raw.parse::<i64>().map_err(|_| {
        AppError::Config(format!(
            "Section [{section}]: '{key}' must be a whole number, not '{raw}'."
        ))
    })
}

fn get_list(sections: &Sections, section: &str, key: &str) -> Vec<String> {
    match lookup(sections, section, key) {
        Some(raw) => raw
            .split(',')
            .map(|part| part.trim().to_string())
            .filter(|part| !part.is_empty())
            .collect(),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_ini(name: &str, body: &str) -> String {
        let path = std::env::temp_dir().join(format!("saishin-cfg-{}-{name}", std::process::id()));
        std::fs::write(&path, body).expect("write test ini");
        path.to_string_lossy().to_string()
    }

    #[test]
    fn parses_sources_with_defaults_and_lists() {
        let path = write_ini(
            "ok.ini",
            "[general]\nout_dir = ./dl\nnotify = yes\n\n[demo]\ntype = github\nowner = o\nrepo = r\ntag = all\nprivate = on\nfilters = *.zip, !*-dev-*\nchangelog = 1\nversion_suffix = true\nkeep = 2\n",
        );
        let config = load(&path).expect("load");
        assert_eq!(config.default_out_dir, "./dl");
        // The INI above names 'notify', which this build has no use for: keys with
        // no field are ignored, so a file written for an older build loads as it
        // always did.
        assert_eq!(config.keyring_service, "saishin");
        assert_eq!(config.github_api_url, "https://api.github.com");
        let source = &config.sources[0];
        assert_eq!(source.kind, SourceKind::GitHub);
        assert_eq!(source.tag, "all");
        assert!(source.private);
        assert_eq!(source.filters, vec!["*.zip", "!*-dev-*"]);
        assert!(source.changelog);
        assert!(source.version_suffix);
        assert_eq!(source.keep, 2);
        assert!(source.out_dir.ends_with("dl"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_general_gui_key_picks_the_graphical_front_end() {
        const ONE_SOURCE: &str = "[files]\ntype = url\nurls = https://example.com/a.zip\n";

        let on = load(&write_ini(
            "gui.ini",
            &format!("[general]\ngui = true\n\n{ONE_SOURCE}"),
        ))
        .expect("loads");
        assert!(on.gui);

        let off = load(&write_ini(
            "gui_off.ini",
            &format!("[general]\ngui = off\n\n{ONE_SOURCE}"),
        ))
        .expect("loads");
        assert!(!off.gui);

        let unwritten = load(&write_ini("gui_default.ini", ONE_SOURCE)).expect("loads");
        assert!(!unwritten.gui, "the CLI stays the default");

        let bad = write_ini(
            "gui_bad.ini",
            &format!("[general]\ngui = sometimes\n\n{ONE_SOURCE}"),
        );
        let err = load(&bad).expect_err("must fail");
        assert!(err
            .to_string()
            .contains("'gui' must be true or false, not 'sometimes'"));
    }

    #[test]
    fn backslashes_in_values_survive() {
        // A Windows path is the common case: rust-ini's default parse options
        // read '\' as an escape character and drop it, so `C:\Saishin\tables`
        // arrived as `C:Saishinstables`. Values are taken as they are written.
        let path = write_ini(
            "backslash.ini",
            "[general]\nout_dir = C:\\Saishin\\downloads\nsound = C:\\Windows\\media\\ding.wav\n\n[tables]\ntype = github\nowner = o\nrepo = r\nout = C:\\Saishin\\tables\nextract = *\nextract_to = C:\\Saishin\\tables\\vpin-mame\n",
        );
        let config = load(&path).expect("load");
        assert_eq!(config.default_out_dir, "C:\\Saishin\\downloads");
        // 'sound' is a key this build has no field for; the line rides along in
        // the file and is ignored, and the paths' backslashes are what the test is
        // about: read as written.
        let source = &config.sources[0];
        assert!(source.out_dir.ends_with("C:\\Saishin\\tables"));
        assert!(source
            .extract
            .as_ref()
            .expect("extract")
            .destination
            .ends_with("C:\\Saishin\\tables\\vpin-mame"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn section_and_key_names_are_case_insensitive() {
        let path = write_ini(
            "case.ini",
            "[GENERAL]\nOUT_DIR = ./x\n\n[Demo]\nTYPE = URL\nURLS = https://example.invalid/a.zip\n",
        );
        let config = load(&path).expect("load");
        assert_eq!(config.sources.len(), 1);
        assert_eq!(config.sources[0].kind, SourceKind::Url);
        assert_eq!(config.sources[0].urls, vec!["https://example.invalid/a.zip"]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_bad_configs_with_csharp_messages() {
        let missing_type = write_ini("missing.ini", "[demo]\nowner = o\n");
        let err = load(&missing_type).expect_err("must fail");
        assert_eq!(
            err.to_string(),
            "Section [demo] is missing the required 'type' key."
        );

        let bad_bool = write_ini("bool.ini", "[general]\nwindow_centered = maybe\n[demo]\ntype = url\nurls = https://example.invalid/a\n");
        let err = load(&bad_bool).expect_err("must fail");
        assert!(err.to_string().contains("must be true or false"));

        let negative_keep = write_ini(
            "keep.ini",
            "[demo]\ntype = url\nurls = https://example.invalid/a\nkeep = -1\n",
        );
        let err = load(&negative_keep).expect_err("must fail");
        assert!(err.to_string().contains("'keep' cannot be negative"));

        let unknown = write_ini("unknown.ini", "[demo]\ntype = ftp\n");
        let err = load(&unknown).expect_err("must fail");
        assert!(err.to_string().contains("unknown type 'ftp'"));

        let no_sources = write_ini("empty.ini", "[general]\nout_dir = ./dl\n");
        let err = load(&no_sources).expect_err("must fail");
        assert!(err.to_string().contains("No sources defined"));

        for path in [missing_type, bad_bool, negative_keep, unknown, no_sources] {
            std::fs::remove_file(&path).ok();
        }
    }

    /// The keys for the sound and the desktop toast are gone, with the players and
    /// the notification program they started: a run spawns nothing. An INI that
    /// still names them is the file of a machine that has been updating for years,
    /// so the file has to load, keys and all, with nothing said about a player
    /// this build cannot find.
    #[test]
    fn a_file_naming_the_removed_keys_loads() {
        let path = write_ini(
            "old-keys.ini",
            "[general]\nout_dir = ./dl\nnotify = true\nsound = ./sounds/update.mp3\nsound_player = ffplay -nodisp -autoexit\n\n[demo]\ntype = github\nowner = o\nrepo = r\n",
        );
        let config = load(&path).expect("an older file loads");
        assert_eq!(config.default_out_dir, "./dl");
        assert_eq!(config.sources.len(), 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn releases_defaults_to_ten_and_is_overridable_per_source() {
        let path = write_ini(
            "releases.ini",
            "[general]\nreleases = 25\n\n[copy_default]\ntype = github\nowner = o\nrepo = r\n\n[override_it]\ntype = actions\nowner = o\nrepo = r\nreleases = 3\n",
        );
        let config = load(&path).expect("load");
        assert_eq!(config.releases, 25);
        assert_eq!(config.sources[0].releases, 25);
        assert_eq!(config.sources[1].releases, 3);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn releases_must_be_positive() {
        let general = write_ini("releases-zero.ini", "[general]\nreleases = 0\n[demo]\ntype = url\nurls = https://example.invalid/a\n");
        let err = load(&general).expect_err("must fail");
        assert!(err.to_string().contains("'releases' must be at least 1"));

        let section = write_ini(
            "releases-section.ini",
            "[demo]\ntype = github\nowner = o\nrepo = r\nreleases = 0\n",
        );
        let err = load(&section).expect_err("must fail");
        assert!(err.to_string().contains("[demo]: 'releases' must be at least 1"));

        let not_a_number = write_ini(
            "releases-text.ini",
            "[demo]\ntype = github\nowner = o\nrepo = r\nreleases = many\n",
        );
        let err = load(&not_a_number).expect_err("must fail");
        assert!(err.to_string().contains("'releases' must be a whole number"));

        for path in [general, section, not_a_number] {
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn missing_file_is_a_configuration_error() {
        let err = load("definitely-not-here-saishin.ini").expect_err("must fail");
        assert!(matches!(err, AppError::Config(_)));
        assert_eq!(err.exit_code(), 1);
    }
}
