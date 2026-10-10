mod config;
mod downloader;
mod error;
mod github;
mod glob;
mod log;
mod notifier;
mod secrets;
mod zip_handler;

use chrono::{DateTime, Utc};
use config::{AppConfig, SourceConfig, SourceKind};
use downloader::{format_bytes, Downloader};
use error::AppError;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

const GENERATION_GAP_HOURS: i64 = 6;

#[derive(Debug, Clone)]
struct PendingFile {
    url: String,
    file_name: String,
    size: Option<u64>,
    release_tag: String,
    changelog: String,
    published_at: Option<DateTime<Utc>>,
    json_accept: bool,
}

#[derive(Debug, Default)]
struct Options {
    config_path: Option<String>,
    sources: Vec<String>,
    dry_run: bool,
    force: bool,
    non_interactive: bool,
    login: bool,
    logout: bool,
    show_help: bool,
    quiet: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Options, String> {
        let mut options = Options::default();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "-h" | "--help" => options.show_help = true,
                "--dry-run" => options.dry_run = true,
                "-f" | "--force" => options.force = true,
                "--non-interactive" => options.non_interactive = true,
                "--login" => options.login = true,
                "--logout" => options.logout = true,
                "-q" | "--quiet" => options.quiet = true,
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
            log::error(&ex.to_string());
            ex.exit_code()
        }
    }
}

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
    let token = resolve_token(&config, &selected, &http, options).await?;

    let mut updated: Vec<String> = Vec::new();
    for source in &selected {
        let names = run_source(source, &config, &http, token.as_deref(), options).await?;
        updated.extend(names.into_iter().map(|name| format!("{}: {name}", source.name)));
    }

    if config.notify && !updated.is_empty() {
        notifier::send("saishin", &updated.join(", "));
    }
    if !config.sound.is_empty() && !updated.is_empty() {
        notifier::play_sound(&config.sound, &config.sound_player);
    }

    Ok(0)
}

fn find_config() -> Result<String, AppError> {
    let current = std::env::current_dir()
        .map(|dir| dir.join("config.ini").to_string_lossy().to_string())
        .unwrap_or_else(|_| "config.ini".to_string());
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("config.ini").to_string_lossy().to_string()));
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
        "No config.ini found. Pass one with --config <path>.".to_string(),
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

async fn run_source(
    source: &SourceConfig,
    config: &AppConfig,
    http: &Downloader,
    token: Option<&str>,
    options: &Options,
) -> Result<Vec<String>, AppError> {
    log::info(&format!("[{}]", source.name));

    let files = match source.kind {
        SourceKind::GitHub => {
            log::detail(&format!(
                "github {}/{}, tag '{}'",
                source.owner, source.repo, source.tag
            ));
            resolve_github(source, config, http, token).await?
        }
        SourceKind::Actions => {
            log::detail(&format!(
                "actions {}/{}, workflow '{}'",
                source.owner, source.repo, source.workflow
            ));
            resolve_actions(source, config, http, token).await?
        }
        SourceKind::Url => resolve_urls(source),
    };

    if files.is_empty() {
        log::info("    nothing matched");
        return Ok(Vec::new());
    }

    let mut downloaded_names = Vec::new();

    for file in files {
        let mut name = sanitize_file_name(&file.file_name);
        if name.is_empty() {
            log::warn(&format!("[{}] unusable file name, skipped.", source.name));
            continue;
        }

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

        let destination = Path::new(&source.out_dir)
            .join(&name)
            .to_string_lossy()
            .to_string();

        if options.dry_run {
            let size = match file.size {
                Some(bytes) => format_bytes(bytes),
                None => "size unknown".to_string(),
            };
            log::detail(&format!("would download {name} ({size})"));
            continue;
        }

        let downloaded = match http
            .download(&file.url, token, &destination, file.size, options.force, file.json_accept)
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
    }

    if !options.dry_run && source.keep > 0 {
        prune_old_files(&source.out_dir, source.keep);
    }

    Ok(downloaded_names)
}

async fn resolve_actions(
    source: &SourceConfig,
    config: &AppConfig,
    http: &Downloader,
    token: Option<&str>,
) -> Result<Vec<PendingFile>, AppError> {
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
                url: artifact.archive_download_url.clone(),
                file_name: name,
                size: Some(artifact.size as u64),
                release_tag: run.run_number.to_string(),
                changelog: String::new(),
                published_at: run.created_at,
                json_accept: true,
            });
        }
    }

    log::detail(&format!(
        "matched {} artifact(s) across {} run(s)",
        files.len(),
        runs.len()
    ));
    Ok(files)
}

async fn resolve_github(
    source: &SourceConfig,
    config: &AppConfig,
    http: &Downloader,
    token: Option<&str>,
) -> Result<Vec<PendingFile>, AppError> {
    let releases = github::get_releases(http, config, source, token, 100).await?;
    let selected = github::select(&releases, &source.tag);

    if selected.is_empty() {
        log::warn(&format!(
            "[{}] no release matched tag '{}'.",
            source.name, source.tag
        ));
        return Ok(Vec::new());
    }

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
                url,
                file_name: asset.name.clone(),
                size: Some(asset.size as u64),
                release_tag: release.tag.clone(),
                changelog: release.body.clone(),
                published_at: release.published_at,
                json_accept: false,
            });
        }
    }

    log::detail(&format!(
        "matched {} asset(s) across {} release(s)",
        files.len(),
        selected.len()
    ));
    Ok(files)
}

fn resolve_urls(source: &SourceConfig) -> Vec<PendingFile> {
    let mut files = Vec::new();
    for url in &source.urls {
        let name = file_name_from_url(url);
        if !source.filters.is_empty() && !glob::matches_any(&name, &source.filters) {
            continue;
        }
        files.push(PendingFile {
            url: url.clone(),
            file_name: name,
            size: None,
            release_tag: String::new(),
            changelog: String::new(),
            published_at: None,
            json_accept: false,
        });
    }
    log::detail(&format!("matched {} url(s)", files.len()));
    files
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

fn prune_old_files(out_dir: &str, keep: i64) {
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

fn print_usage() {
    println!(
        "\
saishin - download official releases from GitHub and other sources

USAGE
  saishin [config.ini] [options]

OPTIONS
  -c, --config <path>    Configuration file (default: ./config.ini)
  -s, --source <name>    Run only this source (repeatable)
      --dry-run          List what would be downloaded, download nothing
  -f, --force            Re-download and overwrite even when up to date
      --non-interactive  Never prompt; fail if credentials are missing
      --login            Store or replace the GitHub token, then exit
      --logout           Remove the stored GitHub token, then exit
  -q, --quiet            Suppress progress and informational output
  -h, --help             Show this help

EXIT CODES
  0  success
  1  configuration error
  2  download error
  3  authentication error
130  interrupted"
    );
}
