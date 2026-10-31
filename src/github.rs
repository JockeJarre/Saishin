use crate::config::{AppConfig, SourceConfig};
use crate::downloader::Downloader;
use crate::error::AppError;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_PAGES: i64 = 10;

/// The words for a call GitHub refused, which is a question about the call's
/// reach and not about the network. With a token in play it is a question about
/// that token: a fine-grained one is granted on the repositories its owner
/// picked, and it cannot be granted on a repository they do not control, which is
/// the shape of most of these refusals; a classic token with 'public_repo' reads
/// every public repository, and 'repo' adds the private ones. Without a token the
/// call is asked of a public repository's anonymous allowance - 60 calls an hour,
/// per machine - and GitHub answers a passed allowance with 403, so the sentence
/// has to say which of the two the run is looking at.
fn rejected(what: &str, status: u16, with_token: bool) -> String {
    if with_token {
        return format!(
            "GitHub refused the token for {what} (HTTP {status}). \
             A fine-grained token reaches only the repositories it was granted on, so \
             for a repository you do not control store a classic token: 'public_repo' \
             reads public repositories, including their artifact downloads, and 'repo' \
             adds the private ones. Run 'saishin --login' to replace it."
        );
    }
    format!(
        "GitHub refused the call for {what} (HTTP {status}) - and no token was sent, \
         so the call was an anonymous one. A public repository answers 60 of those an \
         hour, per machine, and GitHub's answer for an allowance that has run out is \
         403: this is the allowance, not the repository. Run 'saishin-cli --login' \
         with a classic token, which is answered 5000 calls an hour; 'public_repo' is \
         the scope that reads public repositories and their build downloads."
    )
}

/// The `owner/repo` a GitHub API URL is about, for a message that has nothing
/// else to go by: `.../repos/{owner}/{repo}/actions/runs/{id}/artifacts`.
fn repository_of(url: &str) -> String {
    match url.find("/repos/") {
        Some(at) => url[at + "/repos/".len()..]
            .split('/')
            .take(2)
            .collect::<Vec<_>>()
            .join("/"),
        None => url.to_string(),
    }
}

/// One GitHub API call, and the same call made a second time with the token left
/// at home when GitHub refuses the one it was sent with.
///
/// A fine-grained token is granted on the repositories its owner picked, and
/// GitHub's answer for every other is 403, which reads as a broken token. GitHub
/// writes of these permissions that they "are required to access private
/// resources" and that "some endpoints can also be used to access public
/// resources without these permissions", so a refusal is tried again without a
/// token: the listings of a public repository answer there, and a private one
/// does not answer at all. Only a success changes anything. A listing refused
/// with a token and without one reports the token's answer, which is the one that
/// explains something.
async fn api_get(http: &Downloader, url: &str, token: Option<&str>) -> Result<String, AppError> {
    let first = http.get_string(url, token).await;
    if token.is_none() || !was_refused(&first) {
        return first;
    }
    match http.get_string(url, None).await {
        Ok(body) => {
            say_once(&repository_of(url));
            Ok(body)
        }
        Err(_) => first,
    }
}

/// A source asks once for its runs and once per run for its artifacts, so a
/// default `runs = 10` makes eleven calls, and eleven copies of one sentence
/// would push the run's own lines out of a window's strip. So the run is told
/// once, and the name of the repository in it is the answer to "which one?".
static ANONYMOUS: AtomicBool = AtomicBool::new(false);

fn say_once(repository: &str) {
    if !ANONYMOUS.swap(true, Ordering::Relaxed) {
        crate::log::detail(&anonymous_note(repository));
    }
}

fn anonymous_note(repository: &str) -> String {
    format!(
        "GitHub refused the token for {repository}; its public listing was read \
         without a token (60 calls an hour are open without one)."
    )
}

/// The answers that are about a token's reach: 401 and 403. GitHub's 404 for a
/// repository that is not there, its 422, and a transport failure are not, and a
/// call with no token can make none of those better.
fn was_refused(answer: &Result<String, AppError>) -> bool {
    matches!(answer, Err(AppError::Download { status: 401 | 403, .. }))
}

#[derive(Debug, Clone)]
pub struct GhAsset {
    pub name: String,
    pub size: i64,
    pub browser_download_url: String,
    pub api_url: String,
}

#[derive(Debug, Clone)]
pub struct GhRelease {
    pub tag: String,
    pub prerelease: bool,
    pub draft: bool,
    pub body: String,
    pub published_at: Option<DateTime<Utc>>,
    pub assets: Vec<GhAsset>,
}

#[derive(Debug, Clone)]
pub struct GhRun {
    // The id is part of the API shape but not needed by the orchestration.
    #[allow(dead_code)]
    pub id: i64,
    pub name: String,
    pub status: String,
    pub conclusion: String,
    pub run_number: i64,
    pub created_at: Option<DateTime<Utc>>,
    pub artifacts_url: String,
}

#[derive(Debug, Clone)]
pub struct GhArtifact {
    // The id is part of the API shape but not needed by the orchestration.
    #[allow(dead_code)]
    pub id: i64,
    pub name: String,
    pub size: i64,
    pub archive_download_url: String,
    pub expired: bool,
}

#[derive(Debug, Deserialize)]
struct AssetJson {
    #[serde(default, deserialize_with = "null_string")]
    name: String,
    #[serde(default, deserialize_with = "null_i64")]
    size: i64,
    #[serde(default, deserialize_with = "null_string", rename = "browser_download_url")]
    browser_download_url: String,
    #[serde(default, deserialize_with = "null_string", rename = "url")]
    url: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseJson {
    #[serde(default, deserialize_with = "null_string", rename = "tag_name")]
    tag_name: String,
    #[serde(default, deserialize_with = "null_bool")]
    prerelease: bool,
    #[serde(default, deserialize_with = "null_bool")]
    draft: bool,
    #[serde(default, deserialize_with = "null_string")]
    body: String,
    #[serde(default, rename = "published_at")]
    published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    assets: Vec<AssetJson>,
}

#[derive(Debug, Deserialize)]
struct RunsJson {
    #[serde(default, rename = "workflow_runs")]
    workflow_runs: Vec<RunJson>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RunJson {
    #[serde(default, deserialize_with = "null_i64")]
    id: i64,
    #[serde(default, deserialize_with = "null_string")]
    name: String,
    #[serde(default, deserialize_with = "null_string")]
    status: String,
    #[serde(default, deserialize_with = "null_string")]
    conclusion: String,
    #[serde(default, deserialize_with = "null_i64", rename = "run_number")]
    run_number: i64,
    #[serde(default, rename = "created_at")]
    created_at: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "null_string", rename = "artifacts_url")]
    artifacts_url: String,
}

#[derive(Debug, Deserialize)]
struct ArtifactsJson {
    #[serde(default)]
    artifacts: Vec<ArtifactJson>,
}

#[derive(Debug, Deserialize)]
struct ArtifactJson {
    #[serde(default, deserialize_with = "null_i64")]
    id: i64,
    #[serde(default, deserialize_with = "null_string")]
    name: String,
    #[serde(default, deserialize_with = "null_i64", rename = "size_in_bytes")]
    size_in_bytes: i64,
    #[serde(default, deserialize_with = "null_string", rename = "archive_download_url")]
    archive_download_url: String,
    #[serde(default, deserialize_with = "null_bool")]
    expired: bool,
}

#[derive(Debug, Deserialize)]
struct UserJson {
    #[serde(default)]
    login: Option<String>,
}

// The GitHub API emits explicit nulls (e.g. "conclusion": null for
// in-progress runs, "body": null for notes-less releases). serde's
// #[serde(default)] only covers *missing* keys, so these helpers map null
// to "", 0 or false instead of failing the whole batch.
fn null_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

fn null_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<i64>::deserialize(deserializer)?.unwrap_or(0))
}

fn null_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<bool>::deserialize(deserializer)?.unwrap_or(false))
}

pub async fn get_releases(
    http: &Downloader,
    config: &AppConfig,
    source: &SourceConfig,
    token: Option<&str>,
    per_page: i64,
) -> Result<Vec<GhRelease>, AppError> {
    let wants_all = source.tag.trim().eq_ignore_ascii_case("all");
    let mut releases = Vec::new();
    for page in 1.. {
        let batch = get_release_page(http, config, source, token, per_page, page).await?;
        let full = batch.len() as i64 >= per_page;
        releases.extend(batch);
        if !wants_all || !full || page >= MAX_PAGES {
            break;
        }
    }
    Ok(releases)
}

/// The `limit` newest releases, independent of the source's `tag`. The GUI
/// version dropdown needs a bounded window of recent releases, so this
/// stops as soon as the limit is reached instead of walking every page.
pub async fn get_recent_releases(
    http: &Downloader,
    config: &AppConfig,
    source: &SourceConfig,
    token: Option<&str>,
    limit: i64,
) -> Result<Vec<GhRelease>, AppError> {
    let limit = limit.max(1);
    let per_page = limit.min(100);
    let mut releases = Vec::new();
    for page in 1..=MAX_PAGES {
        let batch = get_release_page(http, config, source, token, per_page, page).await?;
        let full = batch.len() as i64 >= per_page;
        releases.extend(batch);
        if !full || releases.len() as i64 >= limit {
            break;
        }
    }
    releases.truncate(limit as usize);
    Ok(releases)
}

async fn get_release_page(
    http: &Downloader,
    config: &AppConfig,
    source: &SourceConfig,
    token: Option<&str>,
    per_page: i64,
    page: i64,
) -> Result<Vec<GhRelease>, AppError> {
    let url = format!(
        "{}/repos/{}/{}/releases?per_page={per_page}&page={page}",
        config.github_api_url, source.owner, source.repo
    );
    let body = api_get(http, &url, token)
        .await
        .map_err(|ex| match &ex {
            AppError::Download { status, .. } if *status == 401 || *status == 403 => {
                AppError::Auth(rejected(
                    &format!("{}/{}", source.owner, source.repo),
                    *status,
                    token.is_some(),
                ))
            }
            _ => ex,
        })?;
    let parsed: Vec<ReleaseJson> = serde_json::from_str(&body)
        .map_err(|_| AppError::Download { status: 0, message: format!("Unexpected releases payload from {url}.") })?;
    Ok(parsed
        .into_iter()
        .map(|item| GhRelease {
            tag: item.tag_name,
            prerelease: item.prerelease,
            draft: item.draft,
            body: item.body,
            published_at: item.published_at,
            assets: item
                .assets
                .into_iter()
                .map(|asset| GhAsset {
                    name: asset.name,
                    size: asset.size,
                    browser_download_url: asset.browser_download_url,
                    api_url: asset.url,
                })
                .collect(),
        })
        .collect())
}

pub async fn get_runs(
    http: &Downloader,
    config: &AppConfig,
    source: &SourceConfig,
    token: Option<&str>,
    per_page: i64,
    max_runs: i64,
    filter: impl Fn(&GhRun) -> bool,
) -> Result<Vec<GhRun>, AppError> {
    let mut runs = Vec::new();
    for page in 1..=MAX_PAGES {
        let url = format!(
            "{}/repos/{}/{}/actions/runs?per_page={per_page}&page={page}",
            config.github_api_url, source.owner, source.repo
        );
        let batch = get_run_page(http, &url, token).await?;
        let full = batch.len() as i64 >= per_page;
        runs.extend(batch.into_iter().filter(|run| filter(run)));
        if !full || runs.len() as i64 >= max_runs {
            break;
        }
    }
    runs.truncate(max_runs as usize);
    Ok(runs)
}

async fn get_run_page(http: &Downloader, url: &str, token: Option<&str>) -> Result<Vec<GhRun>, AppError> {
    let body = api_get(http, url, token)
        .await
        .map_err(|ex| match &ex {
            AppError::Download { status, .. } if *status == 401 || *status == 403 => {
                AppError::Auth(rejected(&repository_of(url), *status, token.is_some()))
            }
            _ => ex,
        })?;
    let parsed: RunsJson = serde_json::from_str(&body)
        .map_err(|_| AppError::Download { status: 0, message: format!("Unexpected runs payload from {url}.") })?;
    Ok(parsed.workflow_runs.into_iter().map(convert_run).collect())
}

/// Maps one raw workflow run to the model. Split out so tests can feed
/// recorded payloads through the exact production mapping.
pub(crate) fn convert_run(item: RunJson) -> GhRun {
    GhRun {
        id: item.id,
        name: item.name,
        status: item.status,
        conclusion: item.conclusion,
        run_number: item.run_number,
        created_at: item.created_at,
        artifacts_url: item.artifacts_url,
    }
}

/// The completed/successful/workflow filter from resolving `actions`
/// sources. An empty workflow matches every run.
pub(crate) fn run_matches(run: &GhRun, workflow: &str) -> bool {
    run.status.eq_ignore_ascii_case("completed")
        && run.conclusion.eq_ignore_ascii_case("success")
        && (workflow.is_empty() || crate::glob::matches(&run.name, workflow))
}

pub async fn get_artifacts(
    http: &Downloader,
    run: &GhRun,
    token: Option<&str>,
    per_page: i64,
) -> Result<Vec<GhArtifact>, AppError> {
    let mut artifacts = Vec::new();
    for page in 1.. {
        let url = format!("{}?per_page={per_page}&page={page}", run.artifacts_url);
        let body = api_get(http, &url, token).await.map_err(|ex| match &ex {
            AppError::Download { status, .. } if *status == 401 || *status == 403 => {
                AppError::Auth(rejected(
                    &repository_of(&run.artifacts_url),
                    *status,
                    token.is_some(),
                ))
            }
            _ => ex,
        })?;
        let parsed: ArtifactsJson = serde_json::from_str(&body)
            .map_err(|_| AppError::Download { status: 0, message: format!("Unexpected artifacts payload from {url}.") })?;
        let full = parsed.artifacts.len() as i64 >= per_page;
        artifacts.extend(parsed.artifacts.into_iter().map(|item| GhArtifact {
            id: item.id,
            name: item.name,
            size: item.size_in_bytes,
            archive_download_url: item.archive_download_url,
            expired: item.expired,
        }));
        if !full {
            break;
        }
    }
    Ok(artifacts)
}

/// Selects releases by tag: latest stable (falling back to newest), newest
/// prerelease, everything, or one exact tag.
pub fn select(releases: &[GhRelease], tag: &str) -> Vec<GhRelease> {
    let published: Vec<&GhRelease> = releases.iter().filter(|r| !r.draft).collect();
    if published.is_empty() {
        return Vec::new();
    }
    let selector = tag.trim();
    if selector.is_empty() || selector.eq_ignore_ascii_case("latest") {
        let stable = published.iter().find(|r| !r.prerelease);
        return vec![(*stable.unwrap_or(&published[0])).clone()];
    }
    if selector.eq_ignore_ascii_case("prerelease") || selector.eq_ignore_ascii_case("pre") {
        let pre = published.iter().find(|r| r.prerelease);
        return vec![(*pre.unwrap_or(&published[0])).clone()];
    }
    if selector.eq_ignore_ascii_case("all") {
        return published.into_iter().cloned().collect();
    }
    published
        .into_iter()
        .filter(|r| r.tag == selector)
        .cloned()
        .collect()
}

pub async fn validate_token(
    http: &Downloader,
    config: &AppConfig,
    token: &str,
) -> Result<String, AppError> {
    let url = format!("{}/user", config.github_api_url);
    let body = http.get_string(&url, Some(token)).await?;
    let parsed: UserJson = serde_json::from_str(&body).unwrap_or(UserJson { login: None });
    Ok(parsed.login.unwrap_or_else(|| "unknown".to_string()))
}

pub async fn check_repository_access(
    http: &Downloader,
    config: &AppConfig,
    source: &SourceConfig,
    token: &str,
) -> Result<(), AppError> {
    let url = format!(
        "{}/repos/{}/{}",
        config.github_api_url, source.owner, source.repo
    );
    match http.get_string(&url, Some(token)).await {
        Ok(_) => Ok(()),
        Err(AppError::Download { status, .. }) if status == 401 || status == 403 || status == 404 => {
            Err(AppError::Auth(format!(
                "The token cannot read {}/{} (HTTP {status}). \
                 A fine-grained token reaches only the repositories it was granted \
                 on, so give it this repository, or store a classic token - \
                 'public_repo' reads public repositories, 'repo' reads private ones \
                 - and confirm your account can see the repository.",
                source.owner, source.repo
            )))
        }
        Err(ex) => Err(ex),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, prerelease: bool, draft: bool) -> GhRelease {
        GhRelease {
            tag: tag.to_string(),
            prerelease,
            draft,
            body: String::new(),
            published_at: None,
            assets: Vec::new(),
        }
    }

    #[test]
    fn select_mirrors_csharp_tag_rules() {
        let releases = vec![
            release("v2-pre", true, false),
            release("v1", false, false),
            release("v0-draft", false, true),
        ];
        // latest skips drafts and prefers the newest stable release.
        assert_eq!(select(&releases, "latest").iter().map(|r| r.tag.as_str()).collect::<Vec<_>>(), vec!["v1"]);
        assert_eq!(select(&releases, "").iter().map(|r| r.tag.as_str()).collect::<Vec<_>>(), vec!["v1"]);
        // prerelease (or pre) picks the newest prerelease.
        assert_eq!(select(&releases, "prerelease")[0].tag, "v2-pre");
        assert_eq!(select(&releases, "pre")[0].tag, "v2-pre");
        // all returns every non-draft release.
        assert_eq!(select(&releases, "all").len(), 2);
        // exact tag matches literally.
        assert_eq!(select(&releases, "v2-pre").len(), 1);
        assert!(select(&releases, "nope").is_empty());
        // No published releases at all.
        assert!(select(&[release("d", false, true)], "latest").is_empty());
        assert!(select(&[], "latest").is_empty());
    }

    #[test]
    fn parses_real_workflow_runs_payload() {
        let body = include_str!("../tests/fixtures/runs.json");
        let parsed: RunsJson = serde_json::from_str(body).expect("runs payload parses");
        assert_eq!(parsed.workflow_runs.len(), 2);
        for run in &parsed.workflow_runs {
            assert!(!run.name.is_empty());
            assert!(!run.status.is_empty());
            assert!(run.run_number > 0);
            assert!(!run.artifacts_url.is_empty());
        }
    }

    #[test]
    fn parses_real_artifacts_payload() {
        let body = include_str!("../tests/fixtures/artifacts.json");
        let parsed: ArtifactsJson = serde_json::from_str(body).expect("artifacts payload parses");
        assert!(!parsed.artifacts.is_empty());
        for artifact in &parsed.artifacts {
            assert!(!artifact.name.is_empty());
            assert!(!artifact.archive_download_url.is_empty());
        }
    }

    #[test]
    fn in_progress_runs_have_null_conclusions() {
        // Regression test: vpinball/pinmame has in-progress runs whose
        // "conclusion" is null. Those must be tolerated and filtered
        // out; the whole batch must still parse.
        let body = include_str!("../tests/fixtures/pinmame-runs.json");
        let parsed: RunsJson = serde_json::from_str(body).expect("runs payload parses");
        assert!(parsed.workflow_runs.len() > 5);
        let unfinished = parsed
            .workflow_runs
            .iter()
            .filter(|run| run.conclusion.is_empty())
            .count();
        assert!(unfinished > 0, "fixture must contain null conclusions");
    }

    #[test]
    fn pinmame_fixture_yields_matchable_vpinmame_runs() {
        // End-to-end through the production mapping + filter with the
        // workflow from the reported config (workflow = vpinmame).
        let body = include_str!("../tests/fixtures/pinmame-runs.json");
        let parsed: RunsJson = serde_json::from_str(body).expect("runs payload parses");
        let runs: Vec<GhRun> = parsed.workflow_runs.into_iter().map(convert_run).collect();
        let matched: Vec<&GhRun> = runs
            .iter()
            .filter(|run| run_matches(run, "vpinmame"))
            .collect();
        assert!(!matched.is_empty(), "vpinmame workflow must match runs");
        for run in matched {
            assert!(run.status.eq_ignore_ascii_case("completed"));
            assert!(run.conclusion.eq_ignore_ascii_case("success"));
            assert!(!run.artifacts_url.is_empty());
        }
    }

        #[test]
    fn null_body_and_missing_fields_fall_back_to_defaults() {        let parsed: ReleaseJson = serde_json::from_str(
            r#"{"tag_name": "v1", "body": null, "prerelease": null, "assets": [{"name": "a.zip"}]}"#,
        )
        .expect("release payload parses");
        assert_eq!(parsed.tag_name, "v1");
        assert!(parsed.body.is_empty());
        assert!(!parsed.prerelease);
        assert!(!parsed.draft);
        assert_eq!(parsed.assets.len(), 1);
        assert_eq!(parsed.assets[0].size, 0);
        assert!(parsed.assets[0].browser_download_url.is_empty());
    }

    #[test]
    fn only_a_refusal_is_a_question_a_call_without_a_token_can_answer() {
        let refused = |status: u16| Err(AppError::Download { status, message: String::new() });
        assert!(was_refused(&refused(401)));
        assert!(was_refused(&refused(403)));
        // A repository that is not there, a query GitHub will not take, a rate
        // limit, and a call that never arrived are not a token's reach, and an
        // anonymous call improves none of them.
        assert!(!was_refused(&refused(404)));
        assert!(!was_refused(&refused(422)));
        assert!(!was_refused(&refused(429)));
        assert!(!was_refused(&refused(0)));
        assert!(!was_refused(&Ok("payload".to_string())));
    }

    #[test]
    fn an_api_url_says_which_repository_it_is_about() {
        assert_eq!(
            repository_of("https://api.github.com/repos/vpinball/vpinball/actions/runs/41187570440/artifacts"),
            "vpinball/vpinball"
        );
        assert_eq!(
            repository_of("https://ghe.example.com/repos/JockeJarre/VPinballX.starter/releases?per_page=100"),
            "JockeJarre/VPinballX.starter"
        );
        // A URL that names no repository is reported as it came.
        assert_eq!(repository_of("https://api.github.com/user"), "https://api.github.com/user");
    }

    #[test]
    fn a_refusal_with_a_token_names_the_token_shape_that_reaches_the_repository() {
        let words = rejected("vpinball/vpinball", 403, true);
        assert!(words.contains("vpinball/vpinball"));
        assert!(words.contains("403"));
        assert!(words.contains("fine-grained"));
        assert!(words.contains("'public_repo'"));
        assert!(words.contains("--login"));
    }

    #[test]
    fn a_refusal_without_a_token_says_so_and_names_the_allowance() {
        // A run with no token has no token to blame, and 403 is also GitHub's
        // answer for an anonymous allowance that has run out: a sentence about a
        // broken token would send the reader off to make a second one.
        let words = rejected("vpinball/vpinball", 403, false);
        assert!(words.contains("no token was sent"));
        assert!(words.contains("60 of those an hour"));
        assert!(words.contains("5000 calls an hour"));
        assert!(!words.contains("fine-grained"));
    }

    #[test]
    fn the_note_about_a_listing_read_without_a_token_names_it_and_its_price() {
        let words = anonymous_note("vpinball/vpinball");
        assert!(words.contains("vpinball/vpinball"));
        assert!(words.contains("without a token"));
        // The rate a run is spending its calls at is the part worth knowing.
        assert!(words.contains("60 calls an hour"));
    }
}
