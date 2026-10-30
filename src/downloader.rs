use crate::error::AppError;
use crate::log;
use futures_util::StreamExt;
use std::io::Write as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const USER_AGENT: &str = "saishin/1.0";
const PARALLEL_CHUNKS: usize = 4;
const PARALLEL_MIN_BYTES: u64 = 8_000_000;

/// Fast downloads would otherwise repaint the progress line on every 80 KB
/// buffer, so repaints are throttled to this interval. Completion always
/// repaints. The mutex also serializes console writes so parallel chunk
/// tasks cannot interleave on one line.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
static LAST_RENDER: Mutex<Option<Instant>> = Mutex::new(None);

/// The half downloaded files a cancelled run leaves on disk, in the order they
/// were left. A cancel keeps the pieces, because resuming is the whole point of
/// writing to a `.part` file, so somebody has to say what to do with them: the
/// window asks. A run that finishes leaves nothing - the piece is renamed into
/// place and the chunks are removed - so the list stays empty.
static PARTIALS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Notes one piece left behind. An empty piece is not listed: it resumes
/// nothing, and a window asking about a 0-byte file is noise.
fn record_partial(path: &str) {
    if safe_file_length(path).unwrap_or(0) == 0 {
        return;
    }
    let mut partials = PARTIALS.lock().unwrap_or_else(|guard| guard.into_inner());
    if !partials.iter().any(|seen| seen == path) {
        partials.push(path.to_string());
    }
}

/// The pieces so far, with the list emptied: the window asks once per run.
pub fn take_partials() -> Vec<String> {
    std::mem::take(&mut *PARTIALS.lock().unwrap_or_else(|guard| guard.into_inner()))
}

pub struct Downloader {
    client: reqwest::Client,
}

impl Downloader {
    pub fn new() -> Result<Self, AppError> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30 * 60))
            .build()
            .map_err(|ex| AppError::Download {
                status: 0,
                message: format!("Could not create the HTTP client: {ex}"),
            })?;
        Ok(Self { client })
    }

    fn auth(builder: reqwest::RequestBuilder, token: Option<&str>) -> reqwest::RequestBuilder {
        match token {
            Some(value) if !value.is_empty() => builder.bearer_auth(value),
            _ => builder,
        }
    }

    pub async fn get_string(&self, url: &str, token: Option<&str>) -> Result<String, AppError> {
        let response = Self::auth(
            self.client
                .get(url)
                .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28"),
            token,
        )
        .send()
        .await
        .map_err(|ex| AppError::Download {
            status: status_of(&ex),
            message: format!("GET {url} failed: {}", first_line(&ex.to_string())),
        })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AppError::Download {
                status: status.as_u16(),
                message: format!(
                    "GET {url} failed with {} {}: {}",
                    status.as_u16(),
                    reason_of(status),
                    summarize(&body)
                ),
            });
        }
        Ok(body)
    }

    pub async fn try_get_content_length(&self, url: &str, token: Option<&str>) -> Option<u64> {
        let response = Self::auth(self.client.head(url), token).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        response.content_length()
    }

    async fn supports_range(&self, url: &str, token: Option<&str>, json_accept: bool) -> bool {
        let accept = if json_accept {
            "application/json"
        } else {
            "application/octet-stream"
        };
        let response = Self::auth(
            self.client
                .get(url)
                .header(reqwest::header::ACCEPT, accept)
                .header(reqwest::header::RANGE, "bytes=0-0"),
            token,
        )
        .send()
        .await;
        matches!(response, Ok(response) if response.status() == reqwest::StatusCode::PARTIAL_CONTENT)
    }

    /// Downloads a file, resuming and parallelising where possible.
    /// Returns true when a download happened, false when the destination
    /// was already current.
    pub async fn download(
        &self,
        url: &str,
        token: Option<&str>,
        destination: &str,
        expected_size: Option<u64>,
        force: bool,
        json_accept: bool,
        cancel: &Arc<AtomicBool>,
    ) -> Result<bool, AppError> {
        if !force && Path::new(destination).exists() {
            let actual = safe_file_length(destination);
            let target = match expected_size {
                Some(size) => Some(size),
                None => self.try_get_content_length(url, token).await,
            };
            if let Some(have) = actual {
                if target.is_none_or(|want| have == want) {
                    log::detail(&format!(
                        "skip (already up to date): {}",
                        file_name_of(destination)
                    ));
                    return Ok(false);
                }
            }
        }

        if let Some(parent) = Path::new(destination).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|ex| AppError::Download {
                    status: 0,
                    message: format!("Could not create {}: {ex}", parent.display()),
                })?;
            }
        }

        if let Some(total) = expected_size {
            if total > PARALLEL_MIN_BYTES && self.supports_range(url, token, json_accept).await {
                return self
                    .download_parallel(url, token, json_accept, destination, total, cancel)
                    .await;
            }
        }

        self.download_single(url, token, json_accept, destination, expected_size, cancel)
            .await
    }

    async fn download_parallel(
        &self,
        url: &str,
        token: Option<&str>,
        json_accept: bool,
        destination: &str,
        total: u64,
        cancel: &Arc<AtomicBool>,
    ) -> Result<bool, AppError> {
        let chunk_size = total.div_ceil(PARALLEL_CHUNKS as u64);
        let written = Arc::new(AtomicU64::new(0));
        let mut parts: Vec<(String, u64, u64, u64)> = Vec::new();

        for i in 0..PARALLEL_CHUNKS {
            let start = i as u64 * chunk_size;
            let end = (total - 1).min(start + chunk_size - 1);
            let length = end - start + 1;
            let part = format!("{destination}.part.{i}");
            let have = safe_file_length(&part).unwrap_or(0).min(length);
            if safe_file_length(&part).is_some_and(|have| have > length) {
                std::fs::write(&part, []).map_err(|ex| AppError::Download {
                    status: 0,
                    message: format!("Could not truncate {part}: {ex}"),
                })?;
            }
            written.fetch_add(have, Ordering::Relaxed);
            parts.push((part, start, end, length));
        }

        let name = file_name_of(destination);
        render_progress(&name, written.load(Ordering::Relaxed), Some(total));

        let mut handles = Vec::new();
        for (part, start, end, length) in parts {
            let resume_from = start + safe_file_length(&part).unwrap_or(0).min(length);
            let client = self.client.clone();
            let url = url.to_string();
            let token = token.map(str::to_string);
            let name = name.clone();
            let written = Arc::clone(&written);
            let cancel_arc = Arc::clone(cancel);
            handles.push(tokio::spawn(async move {
                download_range(
                    &client, &url, token.as_deref(), json_accept, &part, resume_from, end, &name,
                    total, &written, &*cancel_arc,
                )
                .await
            }));
        }

        for handle in handles {
            handle.await.map_err(|ex| AppError::Download {
                status: 0,
                message: format!("chunk task failed: {ex}"),
            })??;
        }

        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            // Every chunk has closed its own line and listed its own piece.
            log::progress_done();
            return Ok(false);
        }

        for i in 0..PARALLEL_CHUNKS {
            let part = format!("{destination}.part.{i}");
            let start = i as u64 * chunk_size;
            let end = (total - 1).min(start + chunk_size - 1);
            if safe_file_length(&part) != Some(end - start + 1) {
                return Err(AppError::Download {
                    status: 0,
                    message: format!("chunk {i} of {name} is incomplete"),
                });
            }
        }

        let mut out = std::fs::File::create(destination).map_err(|ex| AppError::Download {
            status: 0,
            message: format!("Could not write {destination}: {ex}"),
        })?;
        for i in 0..PARALLEL_CHUNKS {
            let part = format!("{destination}.part.{i}");
            let mut chunk = std::fs::File::open(&part).map_err(|ex| AppError::Download {
                status: 0,
                message: format!("Could not read {part}: {ex}"),
            })?;
            std::io::copy(&mut chunk, &mut out).map_err(|ex| AppError::Download {
                status: 0,
                message: format!("Could not assemble {destination}: {ex}"),
            })?;
        }
        for i in 0..PARALLEL_CHUNKS {
            let _ = std::fs::remove_file(format!("{destination}.part.{i}"));
        }

        render_progress(&name, total, Some(total));
        log::progress_done();
        Ok(true)
    }

    async fn download_single(
        &self,
        url: &str,
        token: Option<&str>,
        json_accept: bool,
        destination: &str,
        expected_size: Option<u64>,
        cancel: &Arc<AtomicBool>,
    ) -> Result<bool, AppError> {
        let temporary = format!("{destination}.part");
        let existing = if Path::new(&temporary).exists() {
            safe_file_length(&temporary).unwrap_or(0)
        } else {
            0
        };

        if let Some(total) = expected_size {
            if total > 0 && existing == total {
                std::fs::rename(&temporary, destination).map_err(|ex| AppError::Download {
                    status: 0,
                    message: format!("Could not move {temporary}: {ex}"),
                })?;
                log::detail(&format!("resumed complete: {}", file_name_of(destination)));
                return Ok(true);
            }
        }

        let accept = if json_accept {
            "application/json"
        } else {
            "application/octet-stream"
        };
        let mut request = Self::auth(
            self.client
                .get(url)
                .header(reqwest::header::ACCEPT, accept),
            token,
        );
        if existing > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={existing}-"));
        }

        let result = self
            .download_single_response(request, url, destination, &temporary, existing, expected_size, cancel)
            .await;
        if result.is_err() {
            try_delete(&temporary);
        }
        result
    }

    async fn download_single_response(
        &self,
        request: reqwest::RequestBuilder,
        url: &str,
        destination: &str,
        temporary: &str,
        existing: u64,
        expected_size: Option<u64>,
        cancel: &Arc<AtomicBool>,
    ) -> Result<bool, AppError> {
        let response = request.send().await.map_err(|ex| AppError::Download {
            status: status_of(&ex),
            message: format!("GET {url} failed: {}", first_line(&ex.to_string())),
        })?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(AppError::Download {
                status: status.as_u16(),
                message: format!(
                    "GET {url} failed with {} {}: {}",
                    status.as_u16(),
                    reason_of(status),
                    summarize(&body)
                ),
            });
        }

        let append = existing > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
        let mut written = if append { existing } else { 0 };
        let announced = response.content_length();
        let total = match expected_size {
            Some(size) => Some(size),
            None if append => announced.map(|len| existing + len),
            None => announced,
        };

        let mut source = response.bytes_stream();
        let mut target = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(temporary)
            .map_err(|ex| AppError::Download {
                status: 0,
                message: format!("Could not write {temporary}: {ex}"),
            })?;

        let name = file_name_of(destination);
        while let Some(chunk) = source.next().await {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                // The line is closed, and the piece stays: the next run starts
                // from what is here, and the window asks whether that is wanted.
                log::progress_done();
                record_partial(temporary);
                return Ok(false);
            }
            let chunk = chunk.map_err(|ex| AppError::Download {
                status: 0,
                message: format!("GET {url} failed mid-transfer: {}", first_line(&ex.to_string())),
            })?;
            target.write_all(&chunk).map_err(|ex| AppError::Download {
                status: 0,
                message: format!("Could not write {temporary}: {ex}"),
            })?;
            written += chunk.len() as u64;
            render_progress(&name, written, total);
        }

        render_progress(&name, written, total);
        log::progress_done();

        std::fs::rename(temporary, destination).map_err(|ex| AppError::Download {
            status: 0,
            message: format!("Could not move {temporary}: {ex}"),
        })?;
        Ok(true)
    }
}

async fn download_range(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    json_accept: bool,
    file: &str,
    start: u64,
    end: u64,
    name: &str,
    total: u64,
    written: &Arc<AtomicU64>,
    cancel: &AtomicBool,
) -> Result<(), AppError> {
    if start > end {
        if !Path::new(file).exists() {
            std::fs::write(file, []).map_err(|ex| AppError::Download {
                status: 0,
                message: format!("Could not write {file}: {ex}"),
            })?;
        }
        return Ok(());
    }

    let accept = if json_accept {
        "application/json"
    } else {
        "application/octet-stream"
    };
    let mut request = client
        .get(url)
        .header(reqwest::header::ACCEPT, accept)
        .header(reqwest::header::RANGE, format!("bytes={start}-{end}"));
    if let Some(value) = token {
        if !value.is_empty() {
            request = request.bearer_auth(value);
        }
    }

    let response = request.send().await.map_err(|ex| AppError::Download {
        status: status_of(&ex),
        message: format!("GET {url} failed: {}", first_line(&ex.to_string())),
    })?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::Download {
            status: status.as_u16(),
            message: format!(
                "GET {url} chunk {start}-{end} failed with {} {}: {}",
                status.as_u16(),
                reason_of(status),
                summarize(&body)
            ),
        });
    }
    if status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(AppError::Download {
            status: status.as_u16(),
            message: format!("server ignored the Range header for {file}"),
        });
    }

    let mut stream = response.bytes_stream();
    let mut target = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .map_err(|ex| AppError::Download {
            status: 0,
            message: format!("Could not write {file}: {ex}"),
        })?;
    while let Some(chunk) = stream.next().await {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            log::progress_done();
            record_partial(file);
            return Ok(());
        }
        let chunk = chunk.map_err(|ex| AppError::Download {
            status: 0,
            message: format!("GET {url} failed mid-transfer: {}", first_line(&ex.to_string())),
        })?;
        target.write_all(&chunk).map_err(|ex| AppError::Download {
            status: 0,
            message: format!("Could not write {file}: {ex}"),
        })?;
        let now = written.fetch_add(chunk.len() as u64, Ordering::Relaxed) + chunk.len() as u64;
        render_progress(name, now, Some(total));
    }
    Ok(())
}

fn render_progress(name: &str, written: u64, total: Option<u64>) {
    if log::is_quiet() {
        return;
    }
    let line = match total {
        Some(value) if value > 0 => {
            let percent = ((written as f64 * 100.0) / value as f64).clamp(0.0, 100.0) as u64;
            if percent == 100 && written < value {
                return;
            }
            format!(
                "{name}  {percent:3}%  {} / {}",
                format_bytes(written),
                format_bytes(value)
            )
        }
        _ => format!("{name}  {}", format_bytes(written)),
    };
    let complete = matches!(total, Some(value) if value > 0 && written >= value);

    // One lock guards both the throttle timestamp and the console write, so
    // parallel chunk tasks can neither spam repaints nor interleave on a line.
    let mut last = LAST_RENDER.lock().unwrap_or_else(|guard| guard.into_inner());
    if !complete {
        if last.is_some_and(|seen| seen.elapsed() < PROGRESS_INTERVAL) {
            return;
        }
    }
    *last = Some(Instant::now());
    // The line is handed to the log, not written at the console by hand: the log
    // is what knows where a line belongs (a carriage return on a console, the
    // newest line of a window's strip) and a run that reports in a window has no
    // console - printing here is text in a pipe nobody reads.
    log::progress(&line);
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        let rounded = (value * 100.0).round() / 100.0;
        let text = if rounded.fract() == 0.0 {
            format!("{rounded:.0}")
        } else if (rounded * 10.0).fract() == 0.0 {
            format!("{rounded:.1}")
        } else {
            format!("{rounded:.2}")
        };
        format!("{} {}", text, UNITS[unit])
    }
}

fn file_name_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

fn safe_file_length(path: &str) -> Option<u64> {
    std::fs::metadata(path).ok().map(|meta| meta.len())
}

fn try_delete(path: &str) {
    let _ = std::fs::remove_file(path);
}

fn summarize(body: &str) -> String {
    let single: String = body
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = single.trim();
    if trimmed.len() <= 200 {
        trimmed.to_string()
    } else {
        format!("{}...", &trimmed[..200])
    }
}

fn reason_of(status: reqwest::StatusCode) -> String {
    status
        .canonical_reason()
        .unwrap_or(status.as_str())
        .to_string()
}

fn status_of(ex: &reqwest::Error) -> u16 {
    ex.status().map(|status| status.as_u16()).unwrap_or(0)
}

fn first_line(message: &str) -> String {
    message
        .split(['\n', '\r'])
        .next()
        .unwrap_or(message)
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_half_downloaded_piece_is_listed_once_and_handed_over_once() {
        let dir = std::env::temp_dir().join(format!("saishin-partials-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let piece = dir.join("big.zip.part");
        let empty = dir.join("nothing.zip.part");
        std::fs::write(&piece, "half of it").unwrap();
        std::fs::write(&empty, []).unwrap();
        let listed = piece.to_string_lossy().to_string();

        // Twice for the same piece: four chunk tasks cancel at their own moments,
        // and a window listing the same file four times is not a question.
        record_partial(&listed);
        record_partial(&listed);
        record_partial(empty.to_string_lossy().as_ref());

        assert_eq!(take_partials(), vec![listed]);
        assert!(take_partials().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}
