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

/// One `pattern -> name` pair of the `extract_as` key: an entry whose archive
/// path matches `pattern` is written to `target` under `extract_to` instead of
/// to its own path inside the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameRule {
    /// Glob matched against the entry path — the same string `extract` matches.
    pub pattern: String,
    /// Destination path relative to `extract_to`, `/`-separated.
    pub target: String,
}

#[derive(Debug, Clone, Default)]
pub struct ExtractOptions {
    pub patterns: Vec<String>,
    pub destination: String,
    pub strip_prefix: i64,
    pub overwrite: bool,
    pub keep_zip: bool,
    /// Checked in order, first match wins; an entry no rule matches keeps the
    /// name it has in the archive.
    pub renames: Vec<RenameRule>,
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
    /// Actions only: a run must have ended 'success' to offer its files.
    /// False offers a finished red run's files too - a failed build can
    /// carry the artifact someone wants.
    pub only_successful: bool,
    /// Actions only: runs of this branch only. Empty reads every branch.
    pub branch: String,
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
pub struct AppConfig {    // Base directory of the INI file; currently informational only.
    #[allow(dead_code)]
    pub base_directory: String,
    pub default_out_dir: String,
    pub github_api_url: String,
    pub keyring_service: String,
    pub window_centered: bool,
    /// What the window does when a run ends. False keeps it open, on the end
    /// state with a Close button, so the log strip stays readable.
    pub close_on_finish: bool,
    /// Open the GUI by default (`[general] gui`). Which entry point that means
    /// is the key's own choice, and `--gui` / `--no-gui` on the command line
    /// win over it.
    pub gui: GuiMode,
    pub releases: i64,
    pub sources: Vec<SourceConfig>,
}

type Sections = HashMap<String, HashMap<String, String>>;

/// One source section as the editor sees it: the section name and the
/// `key = value` lines exactly as they stand in the file, in file order.
/// Values are never escaped and never resolved, so a path stays a path
/// (`C:\Saishin\tables` survives) and keys this build has no field for ride
/// along untouched. The typed view, `SourceConfig`, is derived on demand by
/// `to_source`, so parsing keeps one home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditableSource {
    pub name: String,
    pub pairs: Vec<(String, String)>,
}

/// The file as the editor sees it: everything up to and including the
/// `[general]` block is kept verbatim - comments, blank lines, all of it - and
/// the source sections are the editable pairs. `save` writes the header back
/// byte for byte and regenerates the sections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigModel {
    pub path: String,
    /// The file text up to the first source section, header comments and the
    /// `[general]` section included. Written back as it was read.
    pub header: String,
    pub sources: Vec<EditableSource>,
}

impl EditableSource {
    /// The value of `key`, case-insensitive, last occurrence winning as the
    /// loader's map does.
    pub fn get(&self, key: &str) -> Option<&str> {
        let want = key.to_lowercase();
        self.pairs
            .iter()
            .rev()
            .find(|(k, _)| k.to_lowercase() == want)
            .map(|(_, v)| v.as_str())
    }

    /// Sets `key` to `value`. An existing key keeps its written case and place;
    /// a new key is appended. An empty value removes the key, which is how the
    /// editor spells "this type has no use for that parameter".
    pub fn set(&mut self, key: &str, value: &str) {
        let want = key.to_lowercase();
        let hits: Vec<usize> = self
            .pairs
            .iter()
            .enumerate()
            .filter(|(_, (k, _))| k.to_lowercase() == want)
            .map(|(i, _)| i)
            .collect();
        if value.is_empty() {
            for i in hits.into_iter().rev() {
                self.pairs.remove(i);
            }
            return;
        }
        match hits.first() {
            Some(&first) => {
                self.pairs[first].1 = value.to_string();
                // Keep the one-winner contract the loader's map has.
                for i in hits.into_iter().rev().skip(1) {
                    self.pairs.remove(i);
                }
            }
            None => self.pairs.push((key.to_string(), value.to_string())),
        }
    }

    /// The typed view, through the loader's own `read_source`: the same rules,
    /// the same defaults and the same error words a bad file gets at load time.
    pub fn to_source(
        &self,
        default_out: &str,
        base_dir: &str,
        default_releases: i64,
    ) -> Result<SourceConfig, AppError> {
        let mut sections: Sections = HashMap::new();
        let keys = sections.entry(self.name.to_lowercase()).or_default();
        for (key, value) in &self.pairs {
            keys.insert(key.to_lowercase(), value.clone());
        }
        read_source(&sections, &self.name, default_out, base_dir, default_releases)?
            .ok_or_else(|| AppError::Config(format!("Section [{}] has no type.", self.name)))
    }
}

impl ConfigModel {
    pub fn find(&self, name: &str) -> Option<&EditableSource> {
        self.sources.iter().find(|s| s.name.eq_ignore_ascii_case(name))
    }

    // The editor reaches sections through `merge_source`/`remove`; a live
    // handle is only ever wanted from tests, so the method is test-only.
    #[cfg(test)]
    pub fn find_mut(&mut self, name: &str) -> Option<&mut EditableSource> {
        self.sources.iter_mut().find(|s| s.name.eq_ignore_ascii_case(name))
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.sources.len();
        self.sources.retain(|s| !s.name.eq_ignore_ascii_case(name));
        self.sources.len() != before
    }

    /// A section name from free text: the name is kept as typed, with the
    /// characters that would break an INI header (`[] = ; #` and newlines)
    /// replaced, so a repo name or a URL tail lands in one piece.
    pub fn sanitize_name(raw: &str) -> String {
        let cleaned: String = raw
            .trim()
            .chars()
            .map(|c| match c {
                '[' | ']' | '=' | ';' | '#' | '\n' | '\r' => '_',
                c => c,
            })
            .collect();
        let cleaned = cleaned.trim_matches(|c| c == '.' || c == '_' || c == '-').to_string();
        if cleaned.is_empty() {
            "source".to_string()
        } else {
            cleaned
        }
    }

    /// `base` made unique against the model, case-insensitively the way the
    /// loader reads section names: `demo`, taken, becomes `demo-2`.
    pub fn unique_name(&self, base: &str) -> String {
        let base = Self::sanitize_name(base);
        if self.find(&base).is_none() {
            return base;
        }
        for n in 2.. {
            let candidate = format!("{base}-{n}");
            if self.find(&candidate).is_none() {
                return candidate;
            }
        }
        unreachable!()
    }
}

/// Read the file for the editor. Unlike `load` this tolerates a file with no
/// source sections at all - deleting every source is a legal edit, and the
/// file has to come back as the state it shows. The strict "No sources
/// defined" rule belongs to the CLI, where a job with nothing to do is an
/// error, and `load` keeps it.
pub fn load_model(path: &str) -> Result<ConfigModel, AppError> {
    let text = std::fs::read_to_string(path)
        .map_err(|ex| AppError::Config(format!("Could not read {path}: {ex}")))?;
    Ok(parse_model(path, &text))
}

fn parse_model(path: &str, text: &str) -> ConfigModel {
    let mut header = String::new();
    let mut sources: Vec<EditableSource> = Vec::new();
    let mut in_general = true;
    let mut current: Option<EditableSource> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if in_general {
            if let Some(name) = section_header(trimmed) {
                if !name.eq_ignore_ascii_case(GENERAL_SECTION) {
                    // This line opens the first source section: it is the
                    // section's own, not the header's.
                    in_general = false;
                    current = Some(EditableSource { name, pairs: Vec::new() });
                    continue;
                }
            }
            header.push_str(line);
            header.push('\n');
            continue;
        }
        if let Some(name) = section_header(trimmed) {
            if let Some(done) = current.take() {
                merge_source(&mut sources, done);
            }
            current = Some(EditableSource { name, pairs: Vec::new() });
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }
        // A key line, kept exactly as written: no case folding, no unescaping.
        // A bare key with no '=' is not a value the loader can reach, and a
        // bare-key line in a source section is rarer than the editor's own
        // hand; it is dropped with the section's comments.
        if let Some((key, value)) = trimmed.split_once('=') {
            if let Some(source) = current.as_mut() {
                source.pairs.push((key.trim().to_string(), value.trim().to_string()));
            }
        }
    }
    if let Some(done) = current.take() {
        merge_source(&mut sources, done);
    }
    ConfigModel { path: path.to_string(), header, sources }
}

/// The name inside a `[name]` header line, `None` for anything else.
fn section_header(trimmed: &str) -> Option<String> {
    let rest = trimmed.strip_prefix('[')?;
    let end = rest.find(']')?;
    let name = rest[..end].trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// A section named twice: the loader's map merges its keys into the first,
/// later keys winning, and the model does the same so `to_source` sees what
/// `load` would.
fn merge_source(sources: &mut Vec<EditableSource>, added: EditableSource) {
    match sources.iter_mut().find(|s| s.name.eq_ignore_ascii_case(&added.name)) {
        Some(existing) => existing.pairs.extend(added.pairs),
        None => sources.push(added),
    }
}

/// Write the model: the header byte for byte, then one section per source in
/// list order, `key = value` as stored. The write is one step: the new text
/// goes to a side file, the old file moves to `path.bak`, the side file takes
/// its place - so a crash mid-write leaves one of the two whole files, never
/// a half-written config.
pub fn save(model: &ConfigModel) -> Result<(), AppError> {
    let mut text = String::new();
    text.push_str(&model.header);
    if !model.header.is_empty() && !model.header.ends_with('\n') {
        text.push('\n');
    }
    for source in &model.sources {
        // One blank line between blocks - and only one: a hand-written file
        // already left its blank line at the header's end.
        if !text.is_empty() && !text.ends_with("\n\n") {
            text.push('\n');
        }
        text.push_str(&format!("[{}]\n", source.name));
        for (key, value) in &source.pairs {
            text.push_str(&format!("{key} = {value}\n"));
        }
    }
    let path = Path::new(&model.path);
    let side = path.with_extension("saishin-tmp");
    std::fs::write(&side, &text)
        .map_err(|ex| AppError::Config(format!("Could not write {}: {ex}", model.path)))?;
    if path.exists() {
        let bak = path.with_extension("bak");
        // The backup is a convenience; a failed copy must not block the save.
        let _ = std::fs::copy(path, &bak);
    }
    std::fs::rename(&side, path)
        .map_err(|ex| AppError::Config(format!("Could not replace {}: {ex}", model.path)))?;
    Ok(())
}

/// What `[general] gui` asks for: which entry point opens the window when the
/// command line says nothing. The two entry points are the icon's launcher
/// (`saishin`, `saishin.exe` on a double-click, which starts the program with
/// `SAISHIN_LAUNCHER` set) and the program run from a terminal
/// (`saishin-cli`), and they want opposite things: the icon wants a window, a
/// terminal typed at wants its transcript. `true` is the launcher, which is
/// what the key has meant all along.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GuiMode {
    /// Headless everywhere: the window comes from `--gui` alone.
    #[default]
    Off,
    /// The window under the launcher, headless in a terminal.
    Launcher,
    /// The window for a terminal run, headless under the launcher.
    Cli,
    /// The window from both.
    Always,
}

impl GuiMode {
    pub fn opens_window_for(self, from_launcher: bool) -> bool {
        match self {
            GuiMode::Off => false,
            GuiMode::Launcher => from_launcher,
            GuiMode::Cli => !from_launcher,
            GuiMode::Always => true,
        }
    }
}

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
        gui: read_gui(&sections, GENERAL_SECTION)?,
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
                only_successful: true,
                branch: String::new(),
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
            only_successful: true,
            branch: String::new(),
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
                only_successful: get_bool(sections, section, "only_successful_builds", true)?,
                branch: get_string(sections, section, "branch", ""),
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

/// `[general] gui`: the words the key takes, and the two old booleans among
/// them, so the file of a machine that has been updating for years keeps
/// behaving as it always did.
fn read_gui(sections: &Sections, section: &str) -> Result<GuiMode, AppError> {
    let raw = get_string(sections, section, "gui", "");
    Ok(match raw.to_lowercase().as_str() {
        "" => GuiMode::Off,
        "launcher" | "true" | "yes" | "on" | "1" => GuiMode::Launcher,
        "off" | "never" | "false" | "no" | "0" => GuiMode::Off,
        "cli" | "terminal" => GuiMode::Cli,
        "always" | "both" => GuiMode::Always,
        _ => {
            return Err(AppError::Config(format!(
                "Section [{section}]: 'gui' must be off, launcher, cli, always, true or false, not '{raw}'."
            )))
        }
    })
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
        renames: read_renames(sections, section)?,
    }))
}

/// `extract_as = *.exe -> app.exe, docs/README* -> README.txt`
///
/// Every comma-separated item is a glob, an arrow (`->` or `=>`) and the name
/// the match is written as. The arrow is found by its `>`, so a pattern may
/// itself contain `-` and `=`.
fn read_renames(sections: &Sections, section: &str) -> Result<Vec<RenameRule>, AppError> {
    let mut rules = Vec::new();
    for item in get_list(sections, section, "extract_as") {
        let Some((raw_pattern, raw_target)) = split_arrow(&item) else {
            return Err(AppError::Config(format!(
                "Section [{section}]: 'extract_as' item '{item}' needs an arrow, as in '*.exe -> app.exe'."
            )));
        };
        let pattern = raw_pattern.trim().replace('\\', "/");
        if pattern.is_empty() {
            return Err(AppError::Config(format!(
                "Section [{section}]: 'extract_as' item '{item}' has no pattern before the arrow."
            )));
        }
        rules.push(RenameRule {
            pattern,
            target: clean_target(section, &item, raw_target)?,
        });
    }
    Ok(rules)
}

fn split_arrow(item: &str) -> Option<(&str, &str)> {
    let index = item.find('>')?;
    let before = &item[..index];
    if !before.ends_with('-') && !before.ends_with('=') {
        return None;
    }
    // The arrow is the '>' and the '-' or '=' in front of it.
    Some((&item[..index - 1], &item[index + 1..]))
}

/// A rename target is always a path **inside** `extract_to`: relative, with no
/// `..` segment and no drive letter, so a rename can never write outside the
/// destination the way a crafted archive entry cannot.
fn clean_target(section: &str, item: &str, raw: &str) -> Result<String, AppError> {
    let raw = raw.trim();
    let text = raw.replace('\\', "/");
    let reject = |reason: &str| -> AppError {
        AppError::Config(format!(
            "Section [{section}]: 'extract_as' item '{item}' names '{raw}', which {reason}."
        ))
    };
    if text.is_empty() {
        return Err(reject("has no file name after the arrow"));
    }
    if text.starts_with('/') {
        return Err(reject("is an absolute path; the name is taken relative to extract_to"));
    }
    let segments: Vec<&str> = text.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    if segments.is_empty() {
        return Err(reject("has no file name after the arrow"));
    }
    if segments.iter().any(|s| *s == "..") {
        return Err(reject("would leave the destination directory"));
    }
    if segments[0].ends_with(':') {
        return Err(reject("is an absolute path; the name is taken relative to extract_to"));
    }
    Ok(segments.join("/"))
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

    #[test]
    fn the_editor_round_trips_a_file() {
        let original = "[general]\nout_dir = ./dl\n; comment kept\n\n[tables]\ntype = github\nowner = o\nrepo = r\nout = C:\\Saishin\\tables\n\n[media]\ntype = url\nurls = https://example.invalid/a.zip\n";
        let path = write_ini("edit-roundtrip.ini", original);
        let model = load_model(&path).expect("model");
        save(&model).expect("save");
        // Touched and untouched alike: the header rides byte for byte, the
        // pairs come back exactly as they went in - and the Windows path is
        // still a Windows path.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let config = load(&path).expect("the written file loads");
        assert!(config.sources[0].out_dir.ends_with("C:\\Saishin\\tables"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn deleting_every_source_is_a_legal_edit() {
        let path = write_ini(
            "empty-after.ini",
            "[general]\nout_dir = ./dl\n\n[one]\ntype = url\nurls = https://example.invalid/a.zip\n",
        );
        let mut model = load_model(&path).expect("model");
        assert!(model.remove("ONE"));
        save(&model).expect("a file with no sources writes");
        let model2 = load_model(&path).expect("and reads back");
        assert!(model2.sources.is_empty());
        // The strict CLI rule stays the CLI's: a job with nothing to do is a
        // loud error, exactly as before.
        assert!(load(&path).is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn an_edited_section_keeps_untouched_keys_and_loses_only_its_comments() {
        let path = write_ini(
            "edit-keep.ini",
            "[general]\n\n[a]\ntype = github\nowner = o\nrepo = r\n; a comment\nextract_as = *.exe -> app.exe\n",
        );
        let mut model = load_model(&path).expect("model");
        model.find_mut("A").expect("section").set("keep", "3");
        save(&model).expect("save");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("extract_as = *.exe -> app.exe"));
        assert!(text.contains("keep = 3"));
        assert!(!text.contains("; a comment"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn names_are_made_safe_and_unique() {
        let path = write_ini("names.ini", "[general]\n\n[demo]\ntype = url\nurls = https://example.invalid/a.zip\n");
        let model = load_model(&path).expect("model");
        assert_eq!(model.unique_name("demo"), "demo-2");
        assert_eq!(model.unique_name("DEMO"), "DEMO-2");
        assert_eq!(model.unique_name("new"), "new");
        assert_eq!(model.unique_name("a[b]=c"), "a_b__c");
        assert_eq!(ConfigModel::sanitize_name("  "), "source");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn set_replaces_in_place_removes_empty_and_appends() {
        let mut s = EditableSource {
            name: "a".to_string(),
            pairs: vec![
                ("type".to_string(), "url".to_string()),
                ("OUT".to_string(), "C:\\x".to_string()),
            ],
        };
        s.set("out", "D:\\y");
        // The key keeps its written case and its place in the section.
        assert_eq!(s.pairs[1], ("OUT".to_string(), "D:\\y".to_string()));
        s.set("filters", "*.zip");
        assert_eq!(s.pairs.last().expect("pair").0, "filters");
        s.set("type", "");
        assert_eq!(s.pairs.len(), 2);
        assert_eq!(s.get("TYPE"), None);
    }

    #[test]
    fn a_model_section_derives_the_typed_source_the_loader_would() {
        let path = write_ini(
            "to-source.ini",
            "[general]\n\n[demo]\ntype = actions\nowner = o\nrepo = r\nworkflow = CI\nruns = 4\nkeep = 2\n",
        );
        let model = load_model(&path).expect("model");
        let source = model
            .find("demo")
            .expect("section")
            .to_source("./dl", "/tmp", 10)
            .expect("derives");
        assert_eq!(source.kind, SourceKind::Actions);
        assert_eq!(source.workflow, "CI");
        assert_eq!(source.max_runs, 4);
        assert_eq!(source.keep, 2);
        // The two run filters default to the old behavior: green only,
        // every branch.
        assert!(source.only_successful);
        assert!(source.branch.is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_red_build_and_a_named_branch_are_configurable() {
        let path = write_ini(
            "red-and-branch.ini",
            "[general]\n\n[x]\ntype = actions\nowner = o\nrepo = r\nbranch = main\nonly_successful_builds = false\n",
        );
        let source = load(&path).expect("loads").sources.remove(0);
        assert_eq!(source.branch, "main");
        assert!(!source.only_successful, "false lets a red build offer its files");
        std::fs::remove_file(&path).ok();
    }

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

        let mode = |value: &str| {
            load(&write_ini(
                "gui.ini",
                &format!("[general]\ngui = {value}\n\n{ONE_SOURCE}"),
            ))
            .expect("loads")
            .gui
        };

        assert_eq!(mode("launcher"), GuiMode::Launcher);
        assert_eq!(mode("cli"), GuiMode::Cli);
        assert_eq!(mode("always"), GuiMode::Always);
        assert_eq!(mode("off"), GuiMode::Off);
        // The old booleans keep their meaning: true is the launcher's window,
        // which is what a double-click of the icon does today.
        assert_eq!(mode("true"), GuiMode::Launcher);
        assert_eq!(mode("yes"), GuiMode::Launcher);
        assert_eq!(mode("false"), GuiMode::Off);
        assert_eq!(
            load(&write_ini("gui_default.ini", ONE_SOURCE))
                .expect("loads")
                .gui,
            GuiMode::Off,
            "the CLI stays the default"
        );

        let bad = write_ini(
            "gui_bad.ini",
            &format!("[general]\ngui = sometimes\n\n{ONE_SOURCE}"),
        );
        let err = load(&bad).expect_err("must fail");
        assert!(
            err.to_string()
                .contains("'gui' must be off, launcher, cli, always, true or false, not 'sometimes'"),
            "{err}"
        );
    }

    #[test]
    fn gui_modes_name_the_entry_point_that_gets_the_window() {
        assert!(!GuiMode::Off.opens_window_for(true));
        assert!(!GuiMode::Off.opens_window_for(false));
        assert!(GuiMode::Launcher.opens_window_for(true));
        assert!(!GuiMode::Launcher.opens_window_for(false));
        assert!(!GuiMode::Cli.opens_window_for(true));
        assert!(GuiMode::Cli.opens_window_for(false));
        assert!(GuiMode::Always.opens_window_for(true));
        assert!(GuiMode::Always.opens_window_for(false));
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
    fn parses_extract_as_pairs() {
        let path = write_ini(
            "extract-as.ini",
            "[demo]\ntype = github\nowner = o\nrepo = r\nextract = *\nextract_as = *.exe -> bin/mytool.exe, docs/README* => README.txt\n",
        );
        let config = load(&path).expect("load");
        let extract = config.sources[0].extract.as_ref().expect("extract");
        assert_eq!(
            extract.renames,
            vec![
                RenameRule {
                    pattern: "*.exe".to_string(),
                    target: "bin/mytool.exe".to_string()
                },
                RenameRule {
                    pattern: "docs/README*".to_string(),
                    target: "README.txt".to_string()
                },
            ]
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_windows_rename_target_is_read_as_a_relative_path() {
        let path = write_ini(
            "extract-as-win.ini",
            "[demo]\ntype = github\nowner = o\nrepo = r\nextract = *\nextract_as = Tables\\*.vpt -> tables\\pinball.vpt\n",
        );
        let config = load(&path).expect("load");
        let extract = config.sources[0].extract.as_ref().expect("extract");
        assert_eq!(extract.renames[0].pattern, "Tables/*.vpt");
        assert_eq!(extract.renames[0].target, "tables/pinball.vpt");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_extract_as_items_that_would_escape_the_destination() {
        for (item, phrase) in [
            ("*.exe", "needs an arrow"),
            ("*.exe -> ", "has no file name after the arrow"),
            ("*.exe -> ../../elsewhere.exe", "would leave the destination"),
            ("*.exe -> /opt/x.exe", "is an absolute path"),
            ("*.exe -> C:/x.exe", "is an absolute path"),
        ] {
            let path = write_ini(
                "extract-as-bad.ini",
                &format!("[demo]\ntype = github\nowner = o\nrepo = r\nextract = *\nextract_as = {item}\n"),
            );
            let err = load(&path).expect_err("must fail");
            assert!(
                err.to_string().contains(phrase),
                "'{item}': {err} must mention '{phrase}'"
            );
            std::fs::remove_file(&path).ok();
        }
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
