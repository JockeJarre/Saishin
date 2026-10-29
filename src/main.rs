mod config;
mod downloader;
mod error;
mod github;
mod glob;
mod gui;
mod log;
mod notifier;
mod secrets;
mod zip_handler;

use chrono::{DateTime, Utc};
use config::{AppConfig, SourceConfig, SourceKind};
use downloader::{format_bytes, Downloader};
use error::AppError;
use futures_util::future::join_all;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, UNIX_EPOCH};

const GENERATION_GAP_HOURS: i64 = 6;

/// Build identity stamped by build.rs: the version from Cargo.toml, the
/// commit, the target triple and the profile. Cargo.toml is the only place
/// a version is ever written.
const BUILD_VERSION: &str = env!("SAISHIN_VERSION");
const BUILD_GIT: &str = env!("SAISHIN_GIT");
const BUILD_TARGET: &str = env!("SAISHIN_TARGET");
const BUILD_PROFILE: &str = env!("SAISHIN_PROFILE");

/// One line for `-V`, and the tail of `--help`.
fn version_string() -> String {
    format!("saishin {BUILD_VERSION} ({BUILD_GIT}, {BUILD_TARGET}, {BUILD_PROFILE})")
}

#[derive(Debug, Clone)]
pub(crate) struct PendingFile {
    source_name: String,
    url: String,
    link_url: String,
    file_name: String,
    /// Key that groups the versions of one artifact across releases (or
    /// workflow runs). Derived from the asset name with the version, build
    /// number and commit tokens removed; set by `apply_names`, so GUI rows
    /// and version lists always agree.
    group_key: String,
    /// Version shown in the GUI dropdown: release tag, or run number.
    version_label: String,
    size: Option<u64>,
    release_tag: String,
    changelog: String,
    published_at: Option<DateTime<Utc>>,
    json_accept: bool,
    destination: String,
}

/// One artifact's versions across the last N releases (or workflow runs),
/// newest first, as offered by the GUI version dropdown.
#[derive(Debug, Clone)]
pub(crate) struct ArtifactVersions {
    source_name: String,
    group_key: String,
    versions: Vec<PendingFile>,
}

#[derive(Debug, Clone, Default)]
struct Options {
    config_path: Option<String>,
    sources: Vec<String>,
    dry_run: bool,
    force: bool,
    non_interactive: bool,
    login: bool,
    logout: bool,
    show_help: bool,
    show_version: bool,
    quiet: bool,
    /// GUI override for `[general] gui`: `Some(true)` from `--gui`, `Some(false)`
    /// from `--no-gui`, `None` (no flag given) follows the config.
    gui: Option<bool>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Options, String> {
        let mut options = Options::default();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "-h" | "--help" => options.show_help = true,
                "-V" | "--version" => options.show_version = true,
                "--dry-run" => options.dry_run = true,
                "-f" | "--force" => options.force = true,
                "--non-interactive" => options.non_interactive = true,
                "--login" => options.login = true,
                "--logout" => options.logout = true,
                "-q" | "--quiet" => options.quiet = true,
                "--gui" => options.gui = Some(true),
                "--no-gui" => options.gui = Some(false),
                "-c" | "--config" => {
                    options.config_path = Some(next_value(args, &mut i)?);
                }
                "-s" | "--source" => {
                    options.sources.push(next_value(args, &mut i)?);
                }
                other if other.starts_with('-') => {
                    return Err(format!("Unknown option '{other}'."));
                }
                positional => {
                    if options.config_path.is_none() {
                        options.config_path = Some(positional.to_string());
                    } else {
                        return Err(format!("Unexpected argument '{positional}'."));
                    }
                }
            }
            i += 1;
        }
        Ok(options)
    }
}

fn next_value(args: &[String], index: &mut usize) -> Result<String, String> {
    if *index + 1 >= args.len() {
        return Err(format!("Option '{}' requires a value.", args[*index]));
    }
    *index += 1;
    Ok(args[*index].clone())
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let options = match Options::parse(&raw) {
        Ok(options) => options,
        Err(message) => {
            log::error(&message);
            eprintln!();
            print_usage();
            return 1;
        }
    };

    if options.show_help {
        print_usage();
        return 0;
    }

    if options.show_version {
        println!("{}", version_string());
        return 0;
    }

    log::set_quiet(options.quiet);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(ex) => {
            log::error(&format!("Could not start the runtime: {ex}"));
            return 2;
        }
    };

    runtime.block_on(async {
        tokio::select! {
            code = async_main(&options) => code,
            _ = tokio::signal::ctrl_c() => {
                log::error("Cancelled.");
                130
            }
        }
    })
}

async fn async_main(options: &Options) -> i32 {
    match async_main_inner(options).await {
        Ok(code) => code,
        Err(ex) => {
            let message = ex.to_string();
            log::error(&message);
            if box_is_the_report() {
                notifier::alert("Saishin", &message);
            }
            ex.exit_code()
        }
    }
}

/// Whether a box is the only way this run can report a failure. The GUI took the
/// report into a window and released the console, so nothing was printed; a
/// console created for this process alone, which is what launching the exe from
/// a shortcut gives, closes with it and hides the text. A terminal has a process
/// of its own on the console and keeps the text, and a service has no console
/// and writes to a log: neither wants a box.
fn box_is_the_report() -> bool {
    // A launcher running in front of this program (saishin.exe, the icon's
    // target) reads the text we print and draws the box, so drawing one here
    // too would report the same failure twice.
    let launcher_reports = std::env::var_os("SAISHIN_LAUNCHER").is_some();
    !launcher_reports && (!log::is_console_output() || console_is_ours())
}

#[cfg(windows)]
fn console_is_ours() -> bool {
    // Kernel32 is already linked by the windowing stack.
    #[link(name = "kernel32")]
    extern "system" {
        fn GetConsoleProcessList(list: *mut u32, size: u32) -> u32;
    }
    let mut processes = [0u32; 8];
    // Zero when there is no console attached, one for a console of our own.
    let attached = unsafe { GetConsoleProcessList(processes.as_mut_ptr(), processes.len() as u32) };
    attached == 1
}

#[cfg(not(windows))]
fn console_is_ours() -> bool {
    false
}

/// Which front end runs. The command line wins: `--gui` opens the window even
/// when the config says headless, `--no-gui` is the escape hatch for a config
/// with `gui = true` (a scheduled task, a script). Without a flag, `[general]
/// gui` decides.
fn use_gui(options: &Options, config: &AppConfig) -> bool {
    options.gui.unwrap_or(config.gui)
}

/// Give back a console that launching the exe from a shortcut created on its
/// own. A terminal that started this process keeps its text, and a console
/// attached to a parent is not ours to close.
#[cfg(windows)]
fn detach_console() {
    // Kernel32 is already linked by the windowing stack.
    #[link(name = "kernel32")]
    extern "system" {
        fn FreeConsole() -> i32;
    }
    // Safety: a GUI run prints nothing after this point.
    unsafe { FreeConsole() };
}

#[cfg(not(windows))]
fn detach_console() {}

async fn async_main_inner(options: &Options) -> Result<i32, AppError> {
    let config_path = match &options.config_path {
        Some(path) => path.clone(),
        None => find_config()?,
    };
    let config = config::load(&config_path)?;

    if options.logout {
        secrets::remove(&config);
        return Ok(0);
    }

    let http = Downloader::new()?;

    if options.login {
        return login(&config, &http).await;
    }

    let selected = select_sources(&config, options)?;

    // Decided here, before the token is looked up: a run that opens the window
    // prints no stream of text, and the token's provenance is part of that
    // stream. Warnings and errors keep the console rule, so a failure before
    // the window exists is still something a launcher can show.
    if use_gui(options, &config) {
        log::window_is_the_report();
    }

    let token = resolve_token(&config, &selected, &http, options).await?;

    let mut updated: Vec<String> = Vec::new();
    if use_gui(options, &config) {
        // The stream is already off (decided above, before the token). Here the
        // problems go too: the console is released just below, and text in the
        // console a double-click created is what keeps that window standing.
        log::set_console_output(false);
        detach_console();
        // The window opens with the sources not yet asked. The asking happens on
        // the window's worker thread, and the lines it logs are the progress: the
        // strip shows a source as soon as it answers, which is the tell that a
        // run of several seconds is doing something.
        updated.extend(gui::run_gui(selected.clone(), config.clone(), token.clone(), options.clone())?);
    } else {
        for source in &selected {
            let names = run_source(source, &config, &http, token.as_deref(), options).await?;
            updated.extend(names.into_iter().map(|name| format!("{}: {name}", source.name)));
        }
    }

    if config.notify && !updated.is_empty() {
        notifier::send("saishin", &updated.join(", "));
    }
    if !config.sound.is_empty() && !updated.is_empty() {
        notifier::play_sound(&config.sound, &config.sound_player);
    }

    Ok(0)
}

/// What a front end asks the sources for: the files that are new, and the
/// versions each group offers. Called from the window's worker thread, after the
/// window is on screen, so the lines this logs are the progress a user reads in
/// the window's strip.
pub(crate) async fn collect_new(
    selected: &[SourceConfig],
    config: &AppConfig,
    http: &Downloader,
    token: Option<&str>,
    options: &Options,
) -> Result<(Vec<PendingFile>, HashMap<(String, String), Vec<PendingFile>>), AppError> {
    let per_source = join_all(
        selected
            .iter()
            .map(|source| collect_pending(source, config, http, token)),
    )
    .await;
    let mut pending: Vec<PendingFile> = Vec::new();
    for (source, result) in selected.iter().zip(per_source) {
        let (files, summary) = result?;
        log::info(&format!("[{}]", source.name));
        match source.kind {
            SourceKind::GitHub => log::detail(&format!(
                "github {}/{}, tag '{}'",
                source.owner, source.repo, source.tag
            )),
            SourceKind::Actions => log::detail(&format!(
                "actions {}/{}, workflow '{}'",
                source.owner, source.repo, source.workflow
            )),
            SourceKind::Url => log::detail(&format!("urls {}", source.urls.join(", "))),
        }
        if !summary.is_empty() {
            log::detail(&summary);
        }
        pending.extend(files);
    }

    let candidates: Vec<PendingFile> = pending
        .into_iter()
        .filter(|file| !file.file_name.is_empty())
        .collect();
    let (flags, version_lists) = tokio::join!(
        join_all(
            candidates
                .iter()
                .map(|file| is_new_artifact(http, file, token, options.force)),
        ),
        join_all(
            selected
                .iter()
                .map(|source| collect_versions(source, config, http, token)),
        )
    );
    let new_entries: Vec<PendingFile> = candidates
        .into_iter()
        .zip(flags)
        .filter(|(_, is_new)| *is_new)
        .map(|(file, _)| file)
        .collect();

    let mut versions: HashMap<(String, String), Vec<PendingFile>> = HashMap::new();
    for result in version_lists {
        for group in result? {
            versions.insert((group.source_name, group.group_key), group.versions);
        }
    }

    Ok((new_entries, versions))
}

fn find_config() -> Result<String, AppError> {
    let current = std::env::current_dir()
        .map(|dir| dir.join("saishin.ini").to_string_lossy().to_string())
        .unwrap_or_else(|_| "saishin.ini".to_string());
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("saishin.ini").to_string_lossy().to_string()));
    let mut candidates = vec![current];
    if let Some(path) = beside_exe {
        candidates.push(path);
    }
    for candidate in candidates {
        if Path::new(&candidate).exists() {
            return Ok(candidate);
        }
    }
    Err(AppError::Config(
        "No saishin.ini found. Pass one with --config <path>.".to_string(),
    ))
}

fn select_sources(config: &AppConfig, options: &Options) -> Result<Vec<SourceConfig>, AppError> {
    if options.sources.is_empty() {
        return Ok(config.sources.clone());
    }
    let known: Vec<&str> = config.sources.iter().map(|s| s.name.as_str()).collect();
    let unknown: Vec<&str> = options
        .sources
        .iter()
        .filter(|requested| {
            !known
                .iter()
                .any(|k| k.eq_ignore_ascii_case(requested))
        })
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        return Err(AppError::Config(format!(
            "Unknown source(s): {}. Configured sources: {}",
            unknown.join(", "),
            known.join(", ")
        )));
    }
    Ok(config
        .sources
        .iter()
        .filter(|s| {
            options
                .sources
                .iter()
                .any(|wanted| wanted.eq_ignore_ascii_case(&s.name))
        })
        .cloned()
        .collect())
}

async fn resolve_token(
    config: &AppConfig,
    selected: &[SourceConfig],
    http: &Downloader,
    options: &Options,
) -> Result<Option<String>, AppError> {
    let github_sources: Vec<&SourceConfig> = selected
        .iter()
        .filter(|s| s.kind == SourceKind::GitHub)
        .collect();
    let actions_sources: Vec<&SourceConfig> = selected
        .iter()
        .filter(|s| s.kind == SourceKind::Actions)
        .collect();
    if github_sources.is_empty() && actions_sources.is_empty() {
        return Ok(None);
    }

    if let Some(from_environment) = secrets::from_environment() {
        log::detail("GitHub token taken from the environment.");
        return Ok(Some(from_environment));
    }

    if let Some(stored) = secrets::from_keyring(config) {
        log::detail("GitHub token taken from the secure store.");
        return Ok(Some(stored));
    }

    let requires_account = github_sources.iter().any(|s| s.private) || !actions_sources.is_empty();
    if !requires_account {
        log::detail("No token stored; using anonymous access for public releases.");
        return Ok(None);
    }

    if options.non_interactive {
        return Err(AppError::Auth(
            "Some sources need a GitHub account but no token is stored. \
             Run 'saishin --login' once interactively, then re-run this scheduled job."
                .to_string(),
        ));
    }

    log::info("This source needs a GitHub account (private repository or authenticated action builds).");
    let token = secrets::prompt_for_token()?;

    for source in github_sources.iter().filter(|s| s.private) {
        github::check_repository_access(http, config, source, &token).await?;
    }
    for source in &actions_sources {
        github::check_repository_access(http, config, source, &token).await?;
    }

    secrets::store(config, &token)?;
    log::info("Token stored in the secure store. Scheduled runs will reuse it without prompting.");
    Ok(Some(token))
}

async fn login(config: &AppConfig, http: &Downloader) -> Result<i32, AppError> {
    let token = secrets::prompt_for_token()?;
    match github::validate_token(http, config, &token).await {
        Ok(login) => {
            secrets::store(config, &token)?;
            log::info(&format!(
                "Token verified for '{login}' and stored in the secure store."
            ));
            Ok(0)
        }
        Err(AppError::Download { status: 401, .. }) | Err(AppError::Download { status: 403, .. }) => {
            log::error("GitHub rejected that token. Nothing was stored.");
            Ok(3)
        }
        Err(ex) => Err(ex),
    }
}

trait ProgressSink {
    fn report(&self, _file: &PendingFile, _started: bool) {}
}

struct NullSink {}

impl ProgressSink for NullSink {}

async fn run_source(
    source: &SourceConfig,
    config: &AppConfig,
    http: &Downloader,
    token: Option<&str>,
    options: &Options,
) -> Result<Vec<String>, AppError> {
    log::info(&format!("[{}]", source.name));

    match source.kind {
        SourceKind::GitHub => log::detail(&format!(
            "github {}/{}, tag '{}'",
            source.owner, source.repo, source.tag
        )),
        SourceKind::Actions => log::detail(&format!(
            "actions {}/{}, workflow '{}'",
            source.owner, source.repo, source.workflow
        )),
        SourceKind::Url => log::detail(&format!("urls {}", source.urls.join(", "))),
    }

    let (files, summary) = collect_pending(source, config, http, token).await?;
    if !summary.is_empty() {
        log::detail(&summary);
    }
    if files.is_empty() {
        log::info("    nothing matched");
        return Ok(Vec::new());
    }

    let null_sink = NullSink {};
    let never_cancel = Arc::new(AtomicBool::new(false));
    let downloaded_names = download_pending(source, config, http, token, &files, options, &never_cancel, &null_sink).await?;

    if !options.dry_run && source.keep > 0 {
        prune_old_files(&source.out_dir, source.keep, &source.filters);
    }

    Ok(downloaded_names)
}

async fn collect_pending(
    source: &SourceConfig,
    config: &AppConfig,
    http: &Downloader,
    token: Option<&str>,
) -> Result<(Vec<PendingFile>, String), AppError> {
    let (raw, summary) = match source.kind {
        SourceKind::GitHub => resolve_github(source, config, http, token).await?,
        SourceKind::Actions => resolve_actions(source, config, http, token).await?,
        SourceKind::Url => resolve_urls(source),
    };

    let mut files = Vec::new();
    for file in raw {
        let mut out = file.clone();
        apply_names(source, &mut out);
        files.push(out);
    }

    Ok((files, summary))
}

/// Applies the local file name (with the optional version suffix) and the
/// destination path. Shared by the CLI/row resolution and by the GUI version
/// lists so both name the same artifact identically.
fn apply_names(source: &SourceConfig, file: &mut PendingFile) {
    let mut name = sanitize_file_name(&file.file_name);
    file.group_key = version_group_key(&name);
    if source.version_suffix && !file.release_tag.trim().is_empty() {
        let version = sanitize_file_name(file.release_tag.trim());
        if !version.is_empty() {
            let path = Path::new(&name);
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let extension = path
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            name = format!("{stem}-{version}{extension}");
        }
    }
    file.file_name = name.clone();
    file.destination = Path::new(&source.out_dir)
        .join(&name)
        .to_string_lossy()
        .to_string();
}

/// Groups the versions of one artifact across releases (or workflow runs).
/// Asset names normally embed the version, the build number and the commit,
/// so a plain name comparison only matches within one release. Those tokens
/// are dropped, and the tokens that tell builds apart - architecture, variant,
/// extension, short numbers - are kept:
///
/// ```text
/// dmdext-v2.5.0-x64.zip                      -> dmdext-x64.zip
/// VPinMAME-sc-3.7.0-1038-aed5459-win-x86.zip -> vpinmame-sc-win-x86.zip
/// VPinballX_BGFX-10.8.1-6480-8ac5b79c5-windows-x64-Release.zip
///                                            -> vpinballx-bgfx-windows-x64-release.zip
/// FlexDMD.zip                                -> flexdmd.zip
/// ```
fn version_group_key(name: &str) -> String {
    // The extension is split off first: in `VPinballX.starter-1.8.zip` the
    // version is glued to it, and dropping the version must not drop the
    // extension, or a .zip and a .msi of one release would share a dropdown.
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty()
                && extension.len() <= 5
                && extension.chars().all(|c| c.is_ascii_alphabetic()) =>
        {
            (stem, Some(extension.to_ascii_lowercase()))
        }
        _ => (name, None),
    };

    let mut parts: Vec<String> = Vec::new();
    for token in stem.split(['-', '_', ' ']) {
        if token.is_empty() || is_version_token(token) {
            continue;
        }
        parts.push(token.to_ascii_lowercase());
    }
    if parts.is_empty() {
        // Nothing but version tokens; keep the name so it stays one group.
        return name.to_ascii_lowercase();
    }
    match extension {
        Some(extension) => format!("{}.{}", parts.join("-"), extension),
        None => parts.join("-"),
    }
}

/// True for the tokens that change from one release of the same artifact to
/// the next: dotted versions (`3.7.0`, `v2.5.0`), long digit runs (`1038`,
/// `6480`) and commit shas (`aed5459`, `8ac5b79c5`). Short numbers (`2` in
/// `two_balls_2.zip`) and architecture tokens (`x64`, `arm64`) are not
/// versions, so artifacts that differ by them stay in separate groups.
fn is_version_token(token: &str) -> bool {
    let body = token.strip_prefix('v').unwrap_or(token);
    if !body.chars().any(|c| c.is_ascii_digit()) {
        return false;
    }
    if body.contains('.') {
        return body.chars().all(|c| c.is_ascii_digit() || c == '.');
    }
    let all_digits = body.chars().all(|c| c.is_ascii_digit());
    let all_hex = body.chars().all(|c| c.is_ascii_hexdigit());
    (all_digits && body.len() >= 3) || (all_hex && body.len() >= 7)
}

/// Collects the versions of every artifact of a source, newest first, for the
/// GUI version dropdown. Only used by `--gui`; the CLI path never calls this.
async fn collect_versions(
    source: &SourceConfig,
    config: &AppConfig,
    http: &Downloader,
    token: Option<&str>,
) -> Result<Vec<ArtifactVersions>, AppError> {
    let web_base = github_web_base(config);
    match source.kind {
        SourceKind::Url => Ok(Vec::new()),
        SourceKind::GitHub => {
            let releases =
                github::get_recent_releases(http, config, source, token, source.releases).await?;
            Ok(group_release_versions(source, &web_base, token.is_some(), &releases))
        }
        SourceKind::Actions => {
            let workflow = source.workflow.clone();
            let runs = github::get_runs(
                http,
                config,
                source,
                token,
                100,
                source.releases,
                |run| github::run_matches(run, &workflow),
            )
            .await?;
            let fetched = join_all(runs.iter().map(|run| github::get_artifacts(http, run, token, 100))).await;
            let mut per_run = Vec::new();
            for (run, artifacts) in runs.iter().zip(fetched) {
                per_run.push((run, artifacts?));
            }
            Ok(group_run_versions(source, &web_base, &per_run))
        }
    }
}

fn group_release_versions(
    source: &SourceConfig,
    web_base: &str,
    with_token: bool,
    releases: &[github::GhRelease],
) -> Vec<ArtifactVersions> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<PendingFile>> = HashMap::new();
    for release in releases.iter().filter(|release| !release.draft) {
        for asset in &release.assets {
            if !source.filters.is_empty() && !glob::matches_any(&asset.name, &source.filters) {
                continue;
            }
            let url = if with_token {
                asset.api_url.clone()
            } else {
                asset.browser_download_url.clone()
            };
            if url.is_empty() {
                continue;
            }
            let mut file = PendingFile {
                source_name: source.name.clone(),
                url,
                link_url: format!(
                    "{web_base}/{}/{}/releases/tag/{}",
                    source.owner, source.repo, release.tag
                ),
                file_name: asset.name.clone(),
                group_key: String::new(),
                version_label: release.tag.clone(),
                size: Some(asset.size as u64),
                release_tag: release.tag.clone(),
                changelog: release.body.clone(),
                published_at: release.published_at,
                json_accept: false,
                destination: String::new(),
            };
            apply_names(source, &mut file);
            push_version(&mut order, &mut groups, file);
        }
    }
    build_groups(&source.name, order, groups)
}

fn group_run_versions(
    source: &SourceConfig,
    web_base: &str,
    runs: &[(&github::GhRun, Vec<github::GhArtifact>)],
) -> Vec<ArtifactVersions> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<PendingFile>> = HashMap::new();
    for (run, artifacts) in runs {
        for artifact in artifacts {
            if artifact.expired || artifact.archive_download_url.is_empty() {
                continue;
            }
            if !source.filters.is_empty() && !glob::matches_any(&artifact.name, &source.filters) {
                continue;
            }
            let name = if artifact.name.to_lowercase().ends_with(".zip") {
                artifact.name.clone()
            } else {
                format!("{}.zip", artifact.name)
            };
            let mut file = PendingFile {
                source_name: source.name.clone(),
                url: artifact.archive_download_url.clone(),
                link_url: format!(
                    "{web_base}/{}/{}/actions/runs/{}",
                    source.owner, source.repo, run.id
                ),
                file_name: name.clone(),
                group_key: String::new(),
                version_label: format!("run #{}", run.run_number),
                size: Some(artifact.size as u64),
                release_tag: run.run_number.to_string(),
                changelog: String::new(),
                published_at: run.created_at,
                json_accept: true,
                destination: String::new(),
            };
            apply_names(source, &mut file);
            push_version(&mut order, &mut groups, file);
        }
    }
    build_groups(&source.name, order, groups)
}

fn push_version(
    order: &mut Vec<String>,
    groups: &mut HashMap<String, Vec<PendingFile>>,
    file: PendingFile,
) {
    let key = file.group_key.clone();
    match groups.get_mut(&key) {
        Some(versions) => versions.push(file),
        None => {
            order.push(key.clone());
            groups.insert(key, vec![file]);
        }
    }
}

fn build_groups(
    source_name: &str,
    order: Vec<String>,
    groups: HashMap<String, Vec<PendingFile>>,
) -> Vec<ArtifactVersions> {
    order
        .into_iter()
        .map(|group_key| ArtifactVersions {
            source_name: source_name.to_string(),
            group_key: group_key.clone(),
            versions: groups.get(&group_key).cloned().unwrap_or_default(),
        })
        .collect()
}

async fn download_pending(
    source: &SourceConfig,
    _config: &AppConfig,
    http: &Downloader,
    token: Option<&str>,
    files: &[PendingFile],
    options: &Options,
    cancel: &Arc<AtomicBool>,
    report: &dyn ProgressSink,
) -> Result<Vec<String>, AppError> {
    let mut downloaded_names = Vec::new();
    let total = files.len();

    for (index, file) in files.iter().enumerate() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(downloaded_names);
        }
        let name = file.file_name.clone();
        if name.is_empty() {
            log::warn(&format!("[{}] unusable file name, skipped.", source.name));
            continue;
        }

        let destination = file.destination.clone();

        if options.dry_run {
            let size = match file.size {
                Some(bytes) => format_bytes(bytes),
                None => "size unknown".to_string(),
            };
            log::detail(&format!("would download {name} ({size})"));
            continue;
        }

        report.report(file, true);
        log::info(&format!(
            "Downloading {} of {}: {name}",
            index + 1,
            total
        ));
        let downloaded = match http
            .download(&file.url, token, &destination, file.size, options.force, file.json_accept, cancel)
            .await
        {
            Ok(done) => done,
            Err(AppError::Download { status, .. })
                if status == 401 || status == 403 || status == 404 =>
            {
                return Err(AppError::Auth(format!(
                    "[{}] the server returned {status} for {name}. \
                     If the repository is private or the build is account-gated, run 'saishin --login' \
                     and make sure the token has the 'repo' scope.",
                    source.name
                )));
            }
            Err(ex) => return Err(ex),
        };

        if downloaded {
            downloaded_names.push(name.clone());
        }

        if downloaded && source.changelog && !file.changelog.is_empty() {
            let stem = Path::new(&name)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| name.clone());
            let changelog_path = Path::new(&source.out_dir)
                .join(format!("{stem}.txt"))
                .to_string_lossy()
                .to_string();
            std::fs::write(&changelog_path, &file.changelog).map_err(|ex| AppError::Download {
                status: 0,
                message: format!("Could not write {changelog_path}: {ex}"),
            })?;
            let short = Path::new(&changelog_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or(changelog_path);
            log::detail(&format!("changelog written to {short}"));
        }

        if downloaded {
            if let Some(published) = file.published_at {
                set_mtime(&destination, published);
                let local = published.with_timezone(&chrono::Local);
                log::detail(&format!(
                    "timestamp set to publish date {}",
                    local.format("%Y-%m-%d %H:%M")
                ));
            }
        }

        if downloaded {
            if let Some(extract) = &source.extract {
                if zip_handler::is_archive(&name) {
                    log::detail(&format!("extracting into {}", extract.destination));
                    zip_handler::extract(&destination, extract, options.force)?;
                    if !extract.keep_zip {
                        let _ = std::fs::remove_file(&destination);
                        log::detail("archive removed (keep_zip = false)");
                    }
                }
            }
        }

        report.report(file, false);
    }

    Ok(downloaded_names)
}

async fn is_new_artifact(
    http: &Downloader,
    file: &PendingFile,
    token: Option<&str>,
    force: bool,
) -> bool {
    if force {
        return true;
    }

    if Path::new(&file.destination).exists() {
        let actual = std::fs::metadata(&file.destination).ok().map(|meta| meta.len());
        let target = match file.size {
            Some(size) => Some(size),
            None => http.try_get_content_length(&file.url, token).await,
        };
        if let Some(have) = actual {
            if target.is_none_or(|want| have == want) {
                return false;
            }
        }
    }

    true
}

fn github_web_base(config: &AppConfig) -> String {
    match url::Url::parse(&config.github_api_url) {
        Err(_) => "https://github.com".to_string(),
        Ok(parsed) => {
            let host = parsed.host_str().unwrap_or("github.com");
            if host == "api.github.com" {
                return "https://github.com".to_string();
            }
            format!("{}://{}", parsed.scheme(), host)
        }
    }
}

async fn resolve_actions(
    source: &SourceConfig,
    config: &AppConfig,
    http: &Downloader,
    token: Option<&str>,
) -> Result<(Vec<PendingFile>, String), AppError> {
    let workflow = source.workflow.clone();
    let runs = github::get_runs(
        http,
        config,
        source,
        token,
        100,
        source.max_runs,
        |run| github::run_matches(run, &workflow),
    )
    .await?;

    let web_base = github_web_base(config);
    let mut files = Vec::new();
    for run in &runs {
        let artifacts = github::get_artifacts(http, run, token, 100).await?;
        for artifact in artifacts {
            if artifact.expired {
                continue;
            }
            if !source.filters.is_empty() && !glob::matches_any(&artifact.name, &source.filters) {
                continue;
            }
            let name = if artifact
                .name
                .to_lowercase()
                .ends_with(".zip")
            {
                artifact.name.clone()
            } else {
                format!("{}.zip", artifact.name)
            };
            if artifact.archive_download_url.is_empty() {
                continue;
            }
            files.push(PendingFile {
                source_name: source.name.clone(),
                url: artifact.archive_download_url.clone(),
                link_url: format!(
                    "{web_base}/{}/{}/actions/runs/{}",
                    source.owner, source.repo, run.id
                ),
                file_name: name.clone(),
                group_key: String::new(),
                version_label: format!("run #{}", run.run_number),
                size: Some(artifact.size as u64),
                release_tag: run.run_number.to_string(),
                changelog: String::new(),
                published_at: run.created_at,
                json_accept: true,
                destination: String::new(),
            });
        }
    }

    let summary = format!(
        "matched {} artifact(s) across {} run(s)",
        files.len(),
        runs.len()
    );
    Ok((files, summary))
}

async fn resolve_github(
    source: &SourceConfig,
    config: &AppConfig,
    http: &Downloader,
    token: Option<&str>,
) -> Result<(Vec<PendingFile>, String), AppError> {
    let releases = github::get_releases(http, config, source, token, 100).await?;
    let selected = github::select(&releases, &source.tag);

    if selected.is_empty() {
        log::warn(&format!(
            "[{}] no release matched tag '{}'.",
            source.name, source.tag
        ));
        return Ok((Vec::new(), String::new()));
    }

    let web_base = github_web_base(config);
    let mut files = Vec::new();
    for release in &selected {
        for asset in &release.assets {
            if !source.filters.is_empty() && !glob::matches_any(&asset.name, &source.filters) {
                continue;
            }
            let url = if token.is_some() {
                asset.api_url.clone()
            } else {
                asset.browser_download_url.clone()
            };
            if url.is_empty() {
                continue;
            }
            files.push(PendingFile {
                source_name: source.name.clone(),
                url,
                link_url: format!(
                    "{web_base}/{}/{}/releases/tag/{}",
                    source.owner, source.repo, release.tag
                ),
                file_name: asset.name.clone(),
                group_key: String::new(),
                version_label: release.tag.clone(),
                size: Some(asset.size as u64),
                release_tag: release.tag.clone(),
                changelog: release.body.clone(),
                published_at: release.published_at,
                json_accept: false,
                destination: String::new(),
            });
        }
    }

    let summary = format!(
        "matched {} asset(s) across {} release(s)",
        files.len(),
        selected.len()
    );
    Ok((files, summary))
}

fn resolve_urls(source: &SourceConfig) -> (Vec<PendingFile>, String) {
    let mut files = Vec::new();
    for url in &source.urls {
        let name = file_name_from_url(url);
        if !source.filters.is_empty() && !glob::matches_any(&name, &source.filters) {
            continue;
        }
        files.push(PendingFile {
            source_name: source.name.clone(),
            url: url.clone(),
            link_url: url.clone(),
            file_name: name.clone(),
            group_key: String::new(),
            version_label: String::new(),
            size: None,
            release_tag: String::new(),
            changelog: String::new(),
            published_at: None,
            json_accept: false,
            destination: String::new(),
        });
    }
    let summary = format!("matched {} url(s)", files.len());
    (files, summary)
}

fn file_name_from_url(url: &str) -> String {
    let parsed = url::Url::parse(url);
    let Ok(parsed) = parsed else {
        return "download.bin".to_string();
    };
    let last = parsed
        .path_segments()
        .and_then(|segments| segments.filter(|s| !s.is_empty()).last().map(str::to_string));
    match last {
        Some(candidate) if !candidate.trim().is_empty() => candidate,
        _ => "download.bin".to_string(),
    }
}

fn sanitize_file_name(file_name: &str) -> String {
    let base = Path::new(file_name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if base.trim().is_empty() {
        return String::new();
    }
    let invalid: &[char] = if cfg!(windows) {
        &['\\', '/', ':', '*', '?', '"', '<', '>', '|']
    } else {
        &['/']
    };
    base.replace(invalid, "_").trim().to_string()
}

fn set_mtime(path: &str, published: DateTime<Utc>) {
    let secs = published.timestamp().max(0) as u64;
    let time = UNIX_EPOCH + Duration::from_secs(secs);
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_modified(time);
    }
}

fn prune_old_files(out_dir: &str, keep: i64, filters: &[String]) {
    let dir = Path::new(out_dir);
    if !dir.is_dir() {
        return;
    }
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if !filters.is_empty() && !glob::matches_any(&name, filters) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        files.push((path, mtime));
    }
    files.sort_by(|a, b| b.1.cmp(&a.1));

    let mut generations: Vec<Vec<(PathBuf, std::time::SystemTime)>> = Vec::new();
    for file in files {
        let same_generation = generations.last().is_some_and(|current| {
            let newest = current[0].1;
            newest
                .duration_since(file.1)
                .or_else(|_| file.1.duration_since(newest))
                .map(|gap| gap <= Duration::from_secs((GENERATION_GAP_HOURS * 3600) as u64))
                .unwrap_or(false)
        });
        if same_generation {
            generations.last_mut().expect("current generation").push(file);
        } else {
            generations.push(vec![file]);
        }
    }

    for generation in generations.into_iter().skip(keep as usize) {
        for (path, _) in generation {
            let _ = std::fs::remove_file(&path);
            log::detail(&format!(
                "pruned old file: {}",
                path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string())
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(kind: SourceKind, version_suffix: bool) -> SourceConfig {
        SourceConfig {
            name: "demo".to_string(),
            kind,
            owner: "owner".to_string(),
            repo: "repo".to_string(),
            tag: "latest".to_string(),
            private: false,
            workflow: String::new(),
            max_runs: 10,
            releases: 10,
            keep: 0,
            changelog: false,
            version_suffix,
            urls: Vec::new(),
            filters: vec!["*.zip".to_string()],
            out_dir: "out".to_string(),
            extract: None,
        }
    }

    fn asset(name: &str, size: i64) -> github::GhAsset {
        github::GhAsset {
            name: name.to_string(),
            size,
            browser_download_url: format!("https://download.invalid/{name}"),
            api_url: format!("https://api.invalid/{name}"),
        }
    }

    fn run(id: i64, number: i64) -> github::GhRun {
        github::GhRun {
            id,
            name: "vpinmame".to_string(),
            status: "completed".to_string(),
            conclusion: "success".to_string(),
            run_number: number,
            created_at: None,
            artifacts_url: String::new(),
        }
    }

    fn artifact_named(name: &str, size: i64) -> github::GhArtifact {
        github::GhArtifact {
            id: 1,
            name: name.to_string(),
            size,
            archive_download_url: format!("https://download.invalid/{name}.zip"),
            expired: false,
        }
    }

    #[test]
    fn version_dropdown_rows_offers_older_releases_of_the_same_asset() {
        let source = source(SourceKind::GitHub, false);
        let releases = vec![
            github::GhRelease {
                tag: "v2".to_string(),
                prerelease: false,
                draft: false,
                body: "new".to_string(),
                published_at: None,
                assets: vec![asset("game.zip", 20), asset("other.zip", 5)],
            },
            github::GhRelease {
                tag: "v1".to_string(),
                prerelease: false,
                draft: false,
                body: "old".to_string(),
                published_at: None,
                assets: vec![asset("game.zip", 10)],
            },
        ];

        let groups = group_release_versions(&source, "https://github.com", false, &releases);
        assert_eq!(groups.len(), 2);

        let game = groups.iter().find(|g| g.group_key == "game.zip").expect("game.zip");
        assert_eq!(game.versions.len(), 2);
        // Newest first, and each version carries its own tag, size and link.
        assert_eq!(game.versions[0].version_label, "v2");
        assert_eq!(game.versions[1].version_label, "v1");
        assert_eq!(game.versions[1].size, Some(10));
        assert_eq!(game.versions[1].url, "https://download.invalid/game.zip");
        assert_eq!(
            game.versions[1].link_url,
            "https://github.com/owner/repo/releases/tag/v1"
        );
        assert_eq!(
            game.versions[1].destination,
            Path::new("out").join("game.zip").to_string_lossy()
        );
        assert_eq!(groups.iter().find(|g| g.group_key == "other.zip").unwrap().versions.len(), 1);
    }

    #[test]
    fn version_dropdown_prefers_the_api_url_when_a_token_is_present() {
        let source = source(SourceKind::GitHub, false);
        let releases = vec![github::GhRelease {
            tag: "v1".to_string(),
            prerelease: false,
            draft: false,
            body: String::new(),
            published_at: None,
            assets: vec![asset("game.zip", 10)],
        }];
        let groups = group_release_versions(&source, "https://github.com", true, &releases);
        assert_eq!(groups[0].versions[0].url, "https://api.invalid/game.zip");
    }

    #[test]
    fn version_dropdown_skips_drafts_unmatched_assets_and_empty_urls() {
        let source = source(SourceKind::GitHub, false);
        let releases = vec![
            github::GhRelease {
                tag: "draft".to_string(),
                prerelease: false,
                draft: true,
                body: String::new(),
                published_at: None,
                assets: vec![asset("game.zip", 1)],
            },
            github::GhRelease {
                tag: "v1".to_string(),
                prerelease: false,
                draft: false,
                body: String::new(),
                published_at: None,
                assets: vec![
                    asset("notes.txt", 1),
                    github::GhAsset {
                        name: "no-url.zip".to_string(),
                        size: 1,
                        browser_download_url: String::new(),
                        api_url: String::new(),
                    },
                ],
            },
        ];
        let groups = group_release_versions(&source, "https://github.com", false, &releases);
        assert!(groups.is_empty());
        // With a token the API url is used, so the same asset is offered.
        let with_token = group_release_versions(&source, "https://github.com", true, &releases);
        assert!(with_token.is_empty());
    }

    #[test]
    fn version_dropdown_gives_each_version_its_own_suffixed_file() {
        let source = source(SourceKind::GitHub, true);
        let releases = vec![
            github::GhRelease {
                tag: "v2".to_string(),
                prerelease: false,
                draft: false,
                body: String::new(),
                published_at: None,
                assets: vec![asset("game.zip", 20)],
            },
            github::GhRelease {
                tag: "v1".to_string(),
                prerelease: false,
                draft: false,
                body: String::new(),
                published_at: None,
                assets: vec![asset("game.zip", 10)],
            },
        ];
        let groups = group_release_versions(&source, "https://github.com", false, &releases);
        let game = &groups[0].versions;
        // One key, so the dropdown groups them, but distinct files on disk.
        assert_eq!(game[0].group_key, game[1].group_key);
        assert_eq!(game[0].file_name, "game-v2.zip");
        assert_eq!(game[1].file_name, "game-v1.zip");
        assert_ne!(game[0].destination, game[1].destination);
    }

    #[test]
    fn run_versions_label_runs_and_skip_expired_artifacts() {
        let source = source(SourceKind::Actions, false);
        let run = |id: i64, number: i64| github::GhRun {
            id,
            name: "vpinmame".to_string(),
            status: "completed".to_string(),
            conclusion: "success".to_string(),
            run_number: number,
            created_at: None,
            artifacts_url: String::new(),
        };
        let artifact = |name: &str, expired: bool| github::GhArtifact {
            id: 1,
            name: name.to_string(),
            size: 7,
            archive_download_url: format!("https://download.invalid/{name}.zip"),
            expired,
        };
        let newest = run(100, 12);
        let older = run(90, 11);
        let runs = vec![
            (&newest, vec![artifact("game.zip", false), artifact("gone.zip", true)]),
            (&older, vec![artifact("game.zip", false)]),
        ];

        let groups = group_run_versions(&source, "https://github.com", &runs);
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group.group_key, "game.zip");
        assert_eq!(group.versions.len(), 2);
        assert_eq!(group.versions[0].version_label, "run #12");
        assert_eq!(group.versions[1].version_label, "run #11");
        assert_eq!(
            group.versions[1].link_url,
            "https://github.com/owner/repo/actions/runs/90"
        );
        assert!(group.versions[0].json_accept);
    }

    #[test]
    fn apply_names_sanitizes_the_key_and_builds_the_destination() {
        let source = source(SourceKind::GitHub, false);
        let mut file = PendingFile {
            source_name: "demo".to_string(),
            url: "https://download.invalid/x".to_string(),
            link_url: String::new(),
            file_name: "sub/game:win.zip".to_string(),
            group_key: String::new(),
            version_label: "v1".to_string(),
            size: None,
            release_tag: "v1".to_string(),
            changelog: String::new(),
            published_at: None,
            json_accept: false,
            destination: String::new(),
        };
        apply_names(&source, &mut file);
        // Only the last path segment survives, and path-hostile characters
        // become underscores. Key and file name stay in step. ':' is hostile
        // on Windows only, so it survives on unix.
        let sanitized = if cfg!(windows) {
            "game_win.zip"
        } else {
            "game:win.zip"
        };
        assert_eq!(file.file_name, sanitized);
        // The group key is name-only: separators normalize, case folds.
        assert_eq!(
            file.group_key,
            if cfg!(windows) {
                "game-win.zip"
            } else {
                "game:win.zip"
            }
        );
        assert_eq!(
            file.destination,
            Path::new("out").join(sanitized).to_string_lossy()
        );
    }

    #[test]
    fn options_parse_the_version_switch() {
        let options = Options::parse(&["-V".to_string()]).expect("parses");
        assert!(options.show_version);
        assert_eq!(options.gui, None);
        assert!(Options::parse(&["--version".to_string()])
            .expect("parses")
            .show_version);
    }

    #[test]
    fn the_gui_switch_is_an_option_not_a_default() {
        assert_eq!(Options::parse(&[]).expect("parses").gui, None);
        assert_eq!(
            Options::parse(&["--gui".to_string()]).expect("parses").gui,
            Some(true)
        );
        assert_eq!(
            Options::parse(&["--no-gui".to_string()]).expect("parses").gui,
            Some(false)
        );
    }

    #[test]
    fn the_config_gui_setting_is_the_default_front_end() {
        let headless = config_with_gui_flag(false);
        let graphical = config_with_gui_flag(true);
        let no_flag = Options::parse(&[]).expect("parses");
        assert!(!use_gui(&no_flag, &headless));
        assert!(use_gui(&no_flag, &graphical));
    }

    #[test]
    fn command_line_gui_flags_win_over_the_config() {
        let headless = config_with_gui_flag(false);
        let graphical = config_with_gui_flag(true);
        assert!(use_gui(
            &Options::parse(&["--gui".to_string()]).expect("parses"),
            &headless
        ));
        assert!(!use_gui(
            &Options::parse(&["--no-gui".to_string()]).expect("parses"),
            &graphical
        ));
    }

    /// A config differing only in `[general] gui`.
    fn config_with_gui_flag(gui: bool) -> AppConfig {
        AppConfig {
            base_directory: String::new(),
            default_out_dir: String::new(),
            github_api_url: "https://api.github.com".to_string(),
            keyring_service: "saishin".to_string(),
            notify: false,
            window_centered: true,
            close_on_finish: false,
            gui,
            releases: 10,
            sound: String::new(),
            sound_player: String::new(),
            sources: Vec::new(),
        }
    }

    #[test]
    fn version_string_names_the_build() {
        let line = version_string();
        assert!(line.starts_with("saishin "));
        assert!(line.contains(env!("SAISHIN_VERSION")));
        // Same shape as Cargo.toml, so a release tag can be checked against it.
        assert_eq!(env!("SAISHIN_VERSION"), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn version_group_key_drops_version_build_and_commit_tokens() {
        // Real asset and artifact names: the version rides in the name, so an
        // exact comparison would only ever match within one release.
        assert_eq!(version_group_key("dmdext-v2.5.0-x64.zip"), "dmdext-x64.zip");
        assert_eq!(
            version_group_key("VPinMAME-sc-3.7.0-1038-aed5459-win-x86.zip"),
            "vpinmame-sc-win-x86.zip"
        );
        assert_eq!(
            version_group_key("VPinballX_BGFX-10.8.1-6480-8ac5b79c5-windows-x64-Release.zip"),
            "vpinballx-bgfx-windows-x64-release.zip"
        );
        assert_eq!(version_group_key("FlexDMD.zip"), "flexdmd.zip");
        // Version glued to the extension, and the same artifact one release older.
        assert_eq!(version_group_key("VPinballX.starter-1.8.zip"), "vpinballx.starter.zip");
        assert_eq!(
            version_group_key("libpinmame-3.7.0-1038-aed5459-macos.tar.gz"),
            "libpinmame-macos.tar.gz"
        );
    }

    #[test]
    fn version_group_key_keeps_the_tokens_that_tell_artifacts_apart() {
        // Architectures and variants stay, so they never share a dropdown.
        assert_ne!(
            version_group_key("dmdext-v2.5.0-x64.zip"),
            version_group_key("dmdext-v2.5.0-x86.zip")
        );
        assert_ne!(
            version_group_key("dmdext-v2.5.0-x64.msi"),
            version_group_key("dmdext-v2.5.0-x64.zip")
        );
        assert_ne!(
            version_group_key("VPinballX-10.8.1-6480-a-windows-x64-Release.zip"),
            version_group_key("VPinballX_BGFX-10.8.1-6480-a-windows-x64-Release.zip")
        );
        // A short number is a name, not a build number.
        assert_ne!(version_group_key("two_balls_2.zip"), version_group_key("two_balls_3.zip"));
        assert_eq!(version_group_key("arm64-x64-x86_64.zip"), "arm64-x64-x86-64.zip");
        // A name made only of version tokens stays one group of its own.
        assert_eq!(version_group_key("1.2.3.zip"), "1.2.3.zip");
        assert_eq!(version_group_key(""), "");
    }

    #[test]
    fn version_dropdown_groups_version_named_assets_of_older_releases() {
        // The reported bug: only a source whose asset name is stable (FlexDMD)
        // produced a dropdown. Names carrying the version must group too.
        let source = source(SourceKind::GitHub, false);
        let releases = vec![
            github::GhRelease {
                tag: "v2.5.0".to_string(),
                prerelease: false,
                draft: false,
                body: String::new(),
                published_at: None,
                assets: vec![asset("dmdext-v2.5.0-x64.zip", 100), asset("dmdext-v2.5.0-x86.zip", 90)],
            },
            github::GhRelease {
                tag: "v2.4.0".to_string(),
                prerelease: false,
                draft: false,
                body: String::new(),
                published_at: None,
                assets: vec![asset("dmdext-v2.4.0-x64.zip", 80), asset("dmdext-v2.4.0-x86.zip", 70)],
            },
            github::GhRelease {
                tag: "v2.3.1".to_string(),
                prerelease: false,
                draft: false,
                body: String::new(),
                published_at: None,
                assets: vec![asset("dmdext-v2.3.1-x64.zip", 60)],
            },
        ];

        let groups = group_release_versions(&source, "https://github.com", false, &releases);
        assert_eq!(groups.len(), 2, "x64 and x86 stay separate groups");

        let x64 = groups.iter().find(|g| g.group_key == "dmdext-x64.zip").expect("x64 group");
        assert_eq!(x64.versions.len(), 3);
        assert_eq!(x64.versions[0].file_name, "dmdext-v2.5.0-x64.zip");
        assert_eq!(x64.versions[2].file_name, "dmdext-v2.3.1-x64.zip");
        assert_eq!(x64.versions[2].release_tag, "v2.3.1");

        let x86 = groups.iter().find(|g| g.group_key == "dmdext-x86.zip").expect("x86 group");
        assert_eq!(x86.versions.len(), 2);
    }

    #[test]
    fn run_versions_group_across_runs_whose_names_carry_the_version() {
        let source = source(SourceKind::Actions, false);
        let newest = run(1_000, 6_241);
        let older = run(999, 6_240);
        let runs = vec![
            (
                &newest,
                vec![artifact_named(
                    "VPinballX-10.8.1-6480-8ac5b79c5-windows-x64-Release.zip",
                    20,
                )],
            ),
            (
                &older,
                vec![artifact_named(
                    "VPinballX-10.8.1-6477-099c8a386-windows-x64-Release.zip",
                    19,
                )],
            ),
        ];

        let groups = group_run_versions(&source, "https://github.com", &runs);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group_key, "vpinballx-windows-x64-release.zip");
        assert_eq!(groups[0].versions.len(), 2);
        assert_eq!(groups[0].versions[0].version_label, "run #6241");
        assert_eq!(groups[0].versions[1].version_label, "run #6240");
    }

    #[test]
    fn version_cell_label_shows_names_only_when_the_group_needs_them() {
        let source = source(SourceKind::GitHub, false);
        let mut versioned = PendingFile {
            source_name: "demo".to_string(),
            url: "https://download.invalid/a".to_string(),
            link_url: String::new(),
            file_name: "dmdext-v2.5.0-x64.zip".to_string(),
            group_key: String::new(),
            version_label: "v2.5.0".to_string(),
            size: None,
            release_tag: "v2.5.0".to_string(),
            changelog: String::new(),
            published_at: None,
            json_accept: false,
            destination: String::new(),
        };
        apply_names(&source, &mut versioned);
        assert_eq!(
            gui::version_cell_label(&versioned, true),
            "v2.5.0  -  dmdext-v2.5.0-x64.zip"
        );
        assert_eq!(gui::version_cell_label(&versioned, false), "v2.5.0");

        let mut plain = versioned.clone();
        plain.file_name = "FlexDMD.zip".to_string();
        plain.version_label = String::new();
        assert_eq!(gui::version_cell_label(&plain, true), "FlexDMD.zip");
    }

    #[test]
    fn github_web_base_default_is_github_com() {
        let config = AppConfig {
            base_directory: String::new(),
            default_out_dir: String::new(),
            github_api_url: "https://api.github.com".to_string(),
            keyring_service: "saishin".to_string(),
            notify: false,
            window_centered: true,
            close_on_finish: false,
            gui: false,
            releases: 10,
            sound: String::new(),
            sound_player: String::new(),
            sources: Vec::new(),
        };
        assert_eq!(github_web_base(&config), "https://github.com");
    }

    #[test]
    fn github_web_base_enterprise_keeps_web_host() {
        let config = AppConfig {
            base_directory: String::new(),
            default_out_dir: String::new(),
            github_api_url: "https://ghe.example.com/api/v3".to_string(),
            keyring_service: "saishin".to_string(),
            notify: false,
            window_centered: true,
            close_on_finish: false,
            gui: false,
            releases: 10,
            sound: String::new(),
            sound_player: String::new(),
            sources: Vec::new(),
        };
        assert_eq!(github_web_base(&config), "https://ghe.example.com");
    }
}

fn print_usage() {
    println!(
        "\
saishin - download official releases from GitHub and other sources

USAGE
  saishin [saishin.ini] [options]

OPTIONS
  -c, --config <path>    Configuration file (default: ./saishin.ini)
  -s, --source <name>    Run only this source (repeatable)
      --dry-run          List what would be downloaded, download nothing
  -f, --force            Re-download and overwrite even when up to date
      --non-interactive  Never prompt; fail if credentials are missing
      --login            Store or replace the GitHub token, then exit
      --logout           Remove the stored GitHub token, then exit
  -q, --quiet            Suppress progress and informational output
      --gui              Show a selection window for new artifacts before downloading
      --no-gui           Run headless, ignoring [general] gui
  -V, --version          Show the version, commit and target, then exit
  -h, --help             Show this help

EXIT CODES
  0  success
  1  configuration error
  2  download error
  3  authentication error
130  interrupted"
    );
}
