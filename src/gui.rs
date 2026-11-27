use crate::downloader::format_bytes;
use crate::error::AppError;
use crate::PendingFile;
use crate::Options;
use crate::config::{self, AppConfig, ConfigModel, EditableSource, SourceKind};
use crate::collect_new;
use crate::sources_note;
use crate::config::SourceConfig;
use crate::log::{self, Level};
use std::time::Duration;
use crate::downloader::take_partials;
use crate::downloader::Downloader;
use crate::prune_old_files;
use crate::download_pending;
use crate::ProgressSink;
use crate::WorkState;
use eframe::App;
use eframe::egui;
use eframe::run_native;
use std::thread;
use tokio::runtime::Builder;
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::collections::HashMap;

struct GuiJob {
    sources: Vec<SourceConfig>,
    config: AppConfig,
    token: Option<String>,
    options: Options,
}

struct GuiResult {
    updated: Mutex<Vec<String>>,
}

#[derive(Clone, Copy)]
enum Phase {
    /// The window is on screen, the sources are being asked.
    Loading,
    Selecting,
    Downloading,
    Done,
    Cancelled,
    Failed,
}

/// True when the end state has a run to continue. A run that stopped with files
/// picked can start them again from where they left off - the pieces on disk are
/// what the transfer continues from - while a finished run has nothing left to
/// do, and a window that never started a download has no files to continue.
fn can_resume(phase: Phase, chosen: &[usize]) -> bool {
    !chosen.is_empty() && matches!(phase, Phase::Cancelled | Phase::Failed)
}

/// The rows a run's sources offer, as the worker thread delivers them. The
/// window is already on screen while this is `Working`, which is what lets the
/// log strip narrate a fetch of several seconds: the window is not something a
/// finished fetch hands over, it is the thing the fetch reports into.
enum LoadState {
    Working,
    Ready {
        entries: Vec<PendingFile>,
        versions: HashMap<(String, String), Vec<PendingFile>>,
        /// "2 of 3 sources answered" when a source was refused, nothing when all
        /// of them answered. The refusals themselves are in the strip below; this
        /// is the heading that makes a user look at it.
        note: Option<String>,
    },
    Failed(String),
}

#[derive(Clone, Copy)]
enum RowState {
    Pending,
    Downloading,
    /// The file is down and being unpacked: a wait of its own, and drawn as one.
    Extracting,
    Done,
}

/// The colors a row's mark wears. The mark is the whole of a row's telling: a
/// spinning circle is a transfer, this amber square an archive being unpacked,
/// this green check a file that is finished, this grey ring a row a cancel
/// stopped while it was busy. The name beside the mark is the row's only text,
/// so no word has to repeat what the drawing says.
const UNZIP_MARK: egui::Color32 = egui::Color32::from_rgb(224, 165, 60);
const DONE_MARK: egui::Color32 = egui::Color32::from_rgb(60, 180, 90);
const STOPPED_MARK: egui::Color32 = egui::Color32::from_rgb(150, 150, 150);
/// A mark's cell: square, and the size of the spinner that fills it.
const MARK_SIZE: f32 = 16.0;
/// The end screen shows the list and asks its question and answers, so the list
/// takes the rows it has and at most this much height.
const END_LIST_MAX_HEIGHT: f32 = 220.0;
/// The end screen's dismiss button. Two letters need little room, and the run's
/// last press should not be aimed at a small target, so it is a button of a size
/// a hand finds in one look.
const OK_WIDTH: f32 = 72.0;
const OK_HEIGHT: f32 = 26.0;
/// The list's source cell, in pixels. A cell is a `Grid` column's width, a
/// truncating label asks for as little as it can get away with, and the columns
/// of a list that changes as a run moves want a width that does not move with
/// the text - so the cell is this wide and its text is cut to it.
const ROW_SOURCE_WIDTH: f32 = 150.0;
const ROW_CELL_HEIGHT: f32 = 20.0;

/// The mark for a file being unzipped: a square turning where the download's
/// circle spins, in an amber no download wears. The motion is the same, so the
/// row keeps saying "busy"; the shape and the color say it is the second wait.
fn paint_unzipping(ui: &egui::Ui, rect: egui::Rect) {
    // Turns, not seconds: the clock is a seconds counter that grows all day, and
    // the fraction of a turn is the only part of it that draws.
    let turns = (ui.input(|i| i.time) * 0.35).rem_euclid(1.0) as f32;
    let center = rect.center();
    let radius = rect.width().min(rect.height()) * 0.34;
    let stroke = egui::Stroke::new(2.0_f32, UNZIP_MARK);
    let corners: Vec<egui::Pos2> = (0..5)
        .map(|i| {
            let angle = turns * std::f32::consts::TAU + std::f32::consts::FRAC_PI_2 * (i as f32);
            center + egui::vec2(angle.cos() * radius, angle.sin() * radius)
        })
        .collect();
    ui.painter().add(egui::Shape::line(corners, stroke));
}

/// The mark of a row, in its cell, for as long as the row is on screen. The run
/// that owns the row says which marks move: while it runs, a transfer and an
/// archive spin; when it is over, a row that was busy takes a still grey ring,
/// because a mark turning over a run that has ended says the run is still going.
fn row_mark(ui: &egui::Ui, rect: egui::Rect, state: RowState, running: bool) {
    match state {
        RowState::Pending => {}
        RowState::Done => {
            let stroke = egui::Stroke::new(2.0_f32, DONE_MARK);
            ui.painter().line_segment([egui::pos2(rect.left() + 3.0, rect.center().y), egui::pos2(rect.center().x, rect.bottom() - 4.0)], stroke);
            ui.painter().line_segment([egui::pos2(rect.center().x, rect.bottom() - 4.0), egui::pos2(rect.right() - 3.0, rect.top() + 3.0)], stroke);
        }
        RowState::Downloading if running => egui::Spinner::new().size(MARK_SIZE).paint_at(ui, rect),
        RowState::Extracting if running => paint_unzipping(ui, rect),
        RowState::Downloading | RowState::Extracting => {
            // The shape of the transfer, in a grey that is neither busy nor
            // finished, and standing still: this row was mid-flight.
            let stroke = egui::Stroke::new(2.0_f32, STOPPED_MARK);
            ui.painter().circle_stroke(rect.center(), rect.width() * 0.34, stroke);
        }
    }
}

/// The size of a half downloaded file, as the question lists it. "gone" is an
/// answer the list can show: the file is one the run left, and anything may
/// have happened to it in the meantime.
fn partial_size(path: &str) -> String {
    std::fs::metadata(path)
        .map(|meta| format_bytes(meta.len()))
        .unwrap_or_else(|_| "gone".to_string())
}

struct GuiProgress {
    phase: Phase,
    done: usize,
    total: usize,
    message: String,
    /// The heading of the end state. Empty means "the phase says it", which is
    /// how the download worker leaves it; the load path sets its own words.
    heading: String,
    rows: Vec<RowState>,
    /// The half downloaded files a cancelled run left on disk, and what the
    /// window is asking about. Emptied by the answer.
    partials: Vec<String>,
    /// The window's own words about the answer to that question.
    answer: String,
}

struct ArtifactEntry {
    file: PendingFile,
    selected: bool,
    /// Older releases (or workflow runs) of the same artifact, newest first.
    versions: Vec<PendingFile>,
    version_index: usize,
    /// Rows offering exactly the same versions (the files of one release, or
    /// of one workflow run) share one dropdown. `None` when the row has no
    /// versions to offer.
    family: Option<usize>,
}

/// Rows sharing one version dropdown: one release or one workflow run usually
/// carries several files (architectures, installers), and picking a version
/// there has to switch all of them, not just the row the dropdown sits on.
struct VersionFamily {
    rows: Vec<usize>,
    /// The versions every row of the family offers, newest first.
    labels: Vec<String>,
    picked: usize,
}

/// True when `row` is the row of `family` that draws the version dropdown.
fn is_family_owner(families: &[VersionFamily], family: usize, row: usize) -> bool {
    families
        .get(family)
        .map(|family| family.rows.first() == Some(&row))
        .unwrap_or(false)
}

/// Applies the version picked in a family dropdown to every row of it, so the
/// file names and sizes beside the rows follow the pick.
fn apply_picks(entries: &mut [ArtifactEntry], families: &mut [VersionFamily], picks: Vec<(usize, usize)>) {
    for (family_index, label_index) in picks {
        let Some(family) = families.get_mut(family_index) else { continue };
        let Some(label) = family.labels.get(label_index).cloned() else { continue };
        family.picked = label_index;
        for &row in &family.rows {
            if let Some(entry) = entries.get_mut(row) {
                let position = entry
                    .versions
                    .iter()
                    .position(|version| version_cell_label(version, false) == label);
                if let Some(position) = position {
                    entry.version_index = position;
                    entry.file = entry.versions[position].clone();
                }
            }
        }
    }
}

struct SelectionApp {
    entries: Vec<ArtifactEntry>,
    families: Vec<VersionFamily>,
    job: Arc<GuiJob>,
    result: Arc<GuiResult>,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<GuiProgress>>,
    /// What the worker thread has found out about the sources.
    load: Arc<Mutex<LoadState>>,
    /// saishin.ini as the editor sees it: the header verbatim, the sections
    /// as editable pairs. Every add, rename and delete is applied here, then
    /// written to the file.
    model: ConfigModel,
    /// The editor's modal state: closed, a form, or the delete question.
    editor: Editor,
    /// The rows the last run was started with, as indexes into `entries`. This
    /// is what a resume starts again: the same files, picked as they now stand
    /// in the rows, so a version swapped in the dropdown is resumed as the
    /// version that is showing.
    chosen: Vec<usize>,
    /// Whether the finished run's **OK** has been given the keys. Once, so the
    /// button answers Enter and Space from the moment the run ends, and so the
    /// focus is not taken back from a button the window's own keys moved to.
    end_focused: bool,
    /// The run's lines as the window shows them: a copy taken when the count of
    /// lines moved, so the list is not cloned on every frame.
    log_lines: Vec<log::Line>,
    log_revision: u64,
    /// The one-frame order to throw the log view to its end: the scroll area
    /// is asked for an offset past the last line, lands on it, and re-sticks
    /// there on the following frame.
    log_jump: bool,
    /// The header logo, uploaded to the GPU on the first frame.
    logo: Option<egui::TextureHandle>,
    shown: bool,
    centered_at_startup: bool,
    window_centered: bool,
}

/// The artifact list is five columns wide and the file names carry the
/// version, the build number and the commit, so it needs room to be readable.
const WINDOW_WIDTH: f32 = 940.0;
/// The height fits the list: the header is one row, so the window does not
/// need the 640 px a header of two stacked rows took.
const WINDOW_HEIGHT: f32 = 520.0;

/// The header logo as raw RGBA8 pixels, decoded and shrunk from the PNG by
/// `build.rs`. Pixels and not PNG bytes on purpose: the embedded bytes are
/// uploaded to the texture as they are, so showing the logo does not need an
/// image decoder at run time.
const LOGO_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/saishin-logo.rgba"));
/// Pixels on a side of the embedded logo: a quarter of the 1024 px PNG, which
/// is what the header's 52 pt logo takes on a 2x display. `build.rs` reports
/// the size it wrote, and the test at the bottom of this file keeps the two in
/// step.
const LOGO_SIZE: usize = 256;
const _: () = assert!(LOGO_RGBA.len() == LOGO_SIZE * LOGO_SIZE * 4);

/// The header is one row: the logo at the far left, the app's name 最新,
/// "newest", right beside it.
const HEADER_WORD: &str = "最新";
const HEADER_ROW: f32 = 64.0;
/// The logo's height. The row centers logo and word vertically, so this is the
/// tallest thing in it.
const HEADER_LOGO: f32 = 52.0;
/// The word's size in points.
const HEADER_WORD_SIZE: f32 = 34.0;
/// The header's inset at the left and the right, in pixels.
const HEADER_SIDE_MARGIN: i8 = 12;
const _: () = assert!(HEADER_ROW > HEADER_LOGO && HEADER_LOGO > HEADER_WORD_SIZE);

/// The height the log strip starts with when it is open. The strip is
/// resizable by its divider, so this is only the first height; past it the
/// list scrolls and keeps following the newest line, so a run of a thousand
/// lines cannot make the window a thousand rows tall.
const LOG_LINES_HEIGHT: f32 = 180.0;
/// The least height the strip's divider can be dragged to: the header row and
/// a couple of lines. Below that the list is a strip with extra steps, and a
/// divider dragged that far has meant to close the strip, which the header's
/// click says properly.
const LOG_MIN_HEIGHT: f32 = 80.0;

/// The strip's text size. The small style is 9 pt and the body 13 pt, and a log
/// sits between them: paths, sizes and the columns of the progress lines stay
/// legible at a glance, and a run's lines still fill the strip's height.
const LOG_TEXT_SIZE: f32 = 11.0;

/// The strip's monospace advance as a fraction of its text size: a monospace
/// face is about 0.5 to 0.62 em wide. This turns a width into a number of
/// characters, and the number does not have to be exact - a line a character too
/// long is cut with an ellipsis and given in full on hover. Its job is to follow
/// the window's width, so the strip reads a long line whole in a wide window and
/// a short one in a narrow one, where a fixed count fits neither.
const STRIP_CHAR_EM: f32 = 0.6;

/// How many characters of the strip's text a width holds.
fn strip_chars(width: f32) -> usize {
    (f64::from(width) / f64::from(LOG_TEXT_SIZE * STRIP_CHAR_EM))
        .floor()
        .max(1.0) as usize
}

/// One line of the log strip: monospace, so paths, sizes and the columns of the
/// progress lines keep their shape, and a colour for the two levels that need
/// attention.
fn log_line_text(line: &log::Line) -> egui::RichText {
    let text = egui::RichText::new(line.text.as_str())
        .monospace()
        .size(LOG_TEXT_SIZE);
    match line.level {
        Level::Warn => text.color(egui::Color32::from_rgb(204, 153, 51)),
        Level::Error => text.color(egui::Color32::from_rgb(210, 80, 80)),
        Level::Detail | Level::Progress => text.weak(),
        Level::Info => text,
    }
}

/// The strip's one line: the newest line that is short enough to be read whole
/// in the width the strip has (`max_chars`). A run writes its progress and its
/// summary in short lines and its paths and urls in long ones, so this is the
/// newest line while the run moves and as it ends, and the long lines - a full
/// path being written, a url - are passed over, because a path cut in the middle
/// says nothing. Nothing is lost: every line is in the open strip, and the closed
/// strip's text gives the whole of its line on hover.
fn strip_line(lines: &[log::Line], max_chars: usize) -> Option<&log::Line> {
    let newest = lines.last()?;
    Some(
        lines
            .iter()
            .rev()
            .find(|line| line.text.chars().count() <= max_chars)
            .unwrap_or(newest),
    )
}

/// A file name that fits a row: the head, an ellipsis, and the tail. A
/// download's name carries its version and its ending at the end, so a name cut
/// from the front cannot be checked against the source; the head and the tail
/// together say which file the row is, and the name's middle is what the row has
/// no room for. The full name is the link's text, in the strip, and on disk.
fn short_name(name: &str) -> String {
    const HEAD: usize = 18;
    const TAIL: usize = 22;
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= HEAD + TAIL + 1 {
        return name.to_string();
    }
    let head: String = chars[..HEAD].iter().collect();
    let tail: String = chars[chars.len() - TAIL..].iter().collect();
    format!("{head}\u{2026}{tail}")
}

/// The header logo as an egui image, ready to upload: the bytes `build.rs`
/// wrote, read as un-premultiplied RGBA.
fn logo_image() -> egui::ColorImage {
    egui::ColorImage::from_rgba_unmultiplied([LOGO_SIZE, LOGO_SIZE], LOGO_RGBA)
}

fn error_message(ex: AppError) -> String {
    match ex {
        AppError::Download { message, .. } => message,
        AppError::Config(message) => message,
        AppError::Auth(message) => message,
        AppError::Interrupted => "interrupted".to_string(),
    }
}

/// The fetch is over and nothing is coming: the window shows this message, and
/// the strip keeps the lines that led to it.
fn fail_load(load: &Arc<Mutex<LoadState>>, message: String) {
    *load.lock().unwrap_or_else(|guard| guard.into_inner()) = LoadState::Failed(message);
}

struct GuiSink {
    progress: Arc<Mutex<GuiProgress>>,
    index: HashMap<(String, String), usize>,
}

impl ProgressSink for GuiSink {
    fn report(&self, file: &PendingFile, state: WorkState) {
        let key = (file.source_name.clone(), file.file_name.clone());
        if let Some(index) = self.index.get(&key).copied() {
            let mut progress = self.progress.lock().unwrap_or_else(|guard| guard.into_inner());
            if index < progress.rows.len() {
                let finished = matches!(state, WorkState::Done);
                let was_done = matches!(progress.rows[index], RowState::Done);
                progress.rows[index] = match state {
                    WorkState::Downloading => RowState::Downloading,
                    WorkState::Extracting => RowState::Extracting,
                    WorkState::Done => RowState::Done,
                };
                // A row arrives at the end once, whatever marks it wore on the
                // way: the count is files finished, not reports seen.
                if finished && !was_done {
                    progress.done += 1;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The source editor: the window's **Add source**, pencil and trash.
//
// The editor works on the file's own key/value pairs (`config::EditableSource`),
// never on the resolved `SourceConfig`: paths stay exactly as written (a
// Windows `C:\...` survives), and keys the form has no field for -
// `extract_as`, keys of newer builds - ride through an edit untouched. Before
// a save, every section is derived through the loader's own rules, so the file
// is never left in a state the CLI would refuse. Deleting every source is a
// legal edit: the window keeps working on the empty list, and the CLI's
// "No sources defined" stays the CLI's own loud refusal.
// ---------------------------------------------------------------------------

/// The modal state of the editor: closed, filling the add form, filling the
/// edit form (with the section it started from), or asking about a delete.
enum Editor {
    Closed,
    Add(SourceForm),
    Edit(String, SourceForm),
    ConfirmDelete(String),
}

/// The form's fields as text and flags: what a person types, not what the
/// loader would make of it. A count kept empty means "take the default", and
/// that is written as "no key", so the file keeps saying the defaults by
/// silence the way a hand-written ini does.
struct SourceForm {
    kind: SourceKind,
    name: String,
    owner: String,
    repo: String,
    tag: String,
    private: bool,
    workflow: String,
    runs: String,
    /// Actions only: the run's branch, empty for every branch.
    branch: String,
    /// Actions only: the run must have ended 'success' to show. Off lets
    /// a finished red build offer its files too.
    success_only: bool,
    urls: String,
    filters: String,
    out: String,
    releases: String,
    keep: String,
    changelog: bool,
    version_suffix: bool,
    extract: String,
    extract_to: String,
    strip_prefix: String,
    overwrite: bool,
    keep_zip: bool,
    /// The reason the last **Save** was refused, shown inside the dialog.
    error: String,
}

/// The form's one-line lists (`filters`, `urls`, `extract`) are the INI's
/// comma-separated grammar: trimmed items, no empties, one space after each
/// comma.
fn clean_list(raw: &str) -> String {
    raw.split(',')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The loader's true words: 'true', 'yes', 'on', '1'.
fn form_bool(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|v| v.to_lowercase()).as_deref(),
        Some("true") | Some("yes") | Some("on") | Some("1")
    )
}

fn parse_count(raw: &str, min: i64, what: &str) -> Result<(), String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(());
    }
    let value: i64 = raw
        .parse()
        .map_err(|_| format!("'{what}' must be a whole number, not '{raw}'."))?;
    if value < min {
        return Err(format!("'{what}' must be at least {min}."));
    }
    Ok(())
}

/// The hover documentation of one key: the same words as the template's
/// comments, so the dialog and the file cannot drift apart.
fn doc(key: &str) -> &'static str {
    match key {
        "type" => "Where the files come from: a GitHub release page, a workflow's uploaded artifacts, or plain web addresses.",
        "name" => "The section's name: [name] in saishin.ini, and the label every row from this source wears.",
        "owner" => "The GitHub account or organization that owns the repository.",
        "repo" => "The repository that carries the files.",
        "tag" => "Which releases the rows offer: empty (the default) is the newest release, 'all' lists every release.",
        "private" => "The repository is private, so reading it needs a stored token: 'saishin --login', once.",
        "workflow" => "A glob over the workflow name (the 'name:' line in the file), e.g. 'CI'. Empty reads every workflow of the repository.",
        "runs" => "How many recent workflow runs the check reads. Default: 10.",
        "branch" => "Only runs of this branch are offered, e.g. 'main'. Empty reads every branch.",
        "only_successful_builds" => "Only runs that ended 'success' offer their files. Off offers a finished red run's files too - a failed build can carry the download you want. Default: on.",
        "urls" => "The addresses to offer, comma separated. Each needs http:// or https://.",
        "filters" => "Globs the file names are matched against, comma separated; a '!' before a glob refuses a match. Empty takes every file. An actions artifact always gets '.zip' appended.",
        "out" => "The directory downloaded files land in. Empty takes [general]'s out_dir. A relative path here counts from the directory of the ini file.",
        "releases" => "How many releases (or runs) the version dropdown offers per artifact. Default: [general]'s 'releases', 10.",
        "keep" => "How many newest files the out directory keeps after a run; older ones are pruned. 0 keeps everything.",
        "changelog" => "Offer the release notes as a file of their own next to each release's assets.",
        "version_suffix" => "Append the version to the saved file name.",
        "extract" => "Globs of the archive entries to unpack after the download, comma separated; '*' unpacks all. Empty leaves the archive alone.",
        "extract_to" => "The directory archives unpack into. Empty unpacks into a directory named after the source, under out.",
        "strip_prefix" => "How many leading path segments to drop from each entry: '1' unpacks a zip that keeps everything in one top folder flat.",
        "overwrite" => "Replace files already at the destination. false skips them.",
        "keep_zip" => "Keep the downloaded archive after unpacking. The default is true.",
        _ => "",
    }
}

impl SourceForm {
    fn new() -> SourceForm {
        SourceForm {
            kind: SourceKind::GitHub,
            name: String::new(),
            owner: String::new(),
            repo: String::new(),
            tag: String::new(),
            private: false,
            workflow: String::new(),
            runs: String::new(),
            branch: String::new(),
            success_only: true,
            urls: String::new(),
            filters: String::new(),
            out: String::new(),
            releases: String::new(),
            keep: String::new(),
            changelog: false,
            version_suffix: false,
            extract: String::new(),
            extract_to: String::new(),
            strip_prefix: String::new(),
            overwrite: false,
            keep_zip: true,
            error: String::new(),
        }
    }

    /// The form filled from a section: the keys the form owns are read back,
    /// and values that equal the loader's default are shown as empty, so the
    /// form says "default" and a Save can write the key away again.
    fn from_source(source: &EditableSource) -> SourceForm {
        let kind = match source.get("type").unwrap_or("github").to_lowercase().as_str() {
            "actions" => SourceKind::Actions,
            "url" => SourceKind::Url,
            _ => SourceKind::GitHub,
        };
        let shown = |key: &str, default: &str| {
            source
                .get(key)
                .filter(|v| *v != default)
                .unwrap_or("")
                .to_string()
        };
        SourceForm {
            kind,
            name: source.name.clone(),
            owner: source.get("owner").unwrap_or("").to_string(),
            repo: source.get("repo").unwrap_or("").to_string(),
            tag: shown("tag", "latest"),
            private: form_bool(source.get("private")),
            workflow: source.get("workflow").unwrap_or("").to_string(),
            runs: shown("runs", "10"),
            branch: source.get("branch").unwrap_or("").to_string(),
            // Absent means the loader's default, which is 'on'.
            success_only: !matches!(
                source.get("only_successful_builds").map(|v| v.to_lowercase()).as_deref(),
                Some("false") | Some("no") | Some("off") | Some("0")
            ),
            urls: clean_list(source.get("urls").unwrap_or("")),
            filters: clean_list(source.get("filters").unwrap_or("")),
            out: source.get("out").unwrap_or("").to_string(),
            releases: source.get("releases").unwrap_or("").to_string(),
            keep: shown("keep", "0"),
            changelog: form_bool(source.get("changelog")),
            version_suffix: form_bool(source.get("version_suffix")),
            extract: clean_list(source.get("extract").unwrap_or("")),
            extract_to: source.get("extract_to").unwrap_or("").to_string(),
            strip_prefix: shown("strip_prefix", "0"),
            overwrite: form_bool(source.get("overwrite")),
            keep_zip: !matches!(
                source.get("keep_zip").map(|v| v.to_lowercase()).as_deref(),
                Some("false") | Some("no") | Some("off") | Some("0")
            ),
            error: String::new(),
        }
    }

    /// The form checked and turned into a section. The checks are the loader's
    /// own requirements - the things `read_source` refuses - stated in the
    /// dialog's words before the file is touched.
    fn apply(&self, model: &ConfigModel, old_name: Option<&str>) -> Result<EditableSource, String> {
        let base = if !self.name.trim().is_empty() {
            ConfigModel::sanitize_name(&self.name)
        } else {
            match self.kind {
                SourceKind::Url => {
                    let tail = clean_list(&self.urls)
                        .split(',')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .rsplit('/')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    ConfigModel::sanitize_name(tail.split('.').next().unwrap_or("source"))
                }
                _ => ConfigModel::sanitize_name(&format!(
                    "{}-{}",
                    self.owner.trim(),
                    self.repo.trim()
                )),
            }
        };
        if base.is_empty() {
            return Err("Give the source a name, or owner and repo, or at least one url.".to_string());
        }
        let name = match old_name {
            Some(old) if base.eq_ignore_ascii_case(old) => old.to_string(),
            Some(_) => {
                if model.find(&base).is_some() {
                    return Err(format!("A section named [{base}] already exists."));
                }
                base
            }
            None => model.unique_name(&base),
        };

        match self.kind {
            SourceKind::GitHub | SourceKind::Actions => {
                if self.owner.trim().is_empty() || self.repo.trim().is_empty() {
                    return Err("A GitHub source needs both owner and repo.".to_string());
                }
            }
            SourceKind::Url => {
                let urls = clean_list(&self.urls);
                if urls.is_empty() {
                    return Err("A web source needs at least one url.".to_string());
                }
                for url in urls.split(", ") {
                    url::Url::parse(url).map_err(|ex| format!("'{url}' is not a web address: {ex}."))?;
                }
            }
        }
        parse_count(&self.runs, 1, "runs")?;
        parse_count(&self.releases, 1, "releases")?;
        parse_count(&self.keep, 0, "keep")?;
        parse_count(&self.strip_prefix, 0, "strip_prefix")?;

        // The pairs ride: an edit starts from the section as it stands, so the
        // keys the form does not own keep their place, case and value.
        let mut source = match old_name.and_then(|old| model.find(old)) {
            Some(existing) => existing.clone(),
            None => EditableSource { name: name.clone(), pairs: Vec::new() },
        };
        source.name = name;

        const MANAGED: &[&str] = &[
            "type", "owner", "repo", "tag", "private", "workflow", "runs", "branch",
            "only_successful_builds", "urls", "filters", "out", "releases", "keep",
            "changelog", "version_suffix", "extract", "extract_to", "strip_prefix",
            "overwrite", "keep_zip",
        ];
        for key in MANAGED {
            source.set(key, "");
        }

        let (kind_key, typed): (&str, Vec<(&str, String)>) = match self.kind {
            SourceKind::GitHub => ("github", vec![
                ("owner", self.owner.trim().to_string()),
                ("repo", self.repo.trim().to_string()),
                ("tag", self.tag.trim().to_string()),
                ("private", if self.private { "true".to_string() } else { String::new() }),
                ("changelog", if self.changelog { "true".to_string() } else { String::new() }),
                ("version_suffix", if self.version_suffix { "true".to_string() } else { String::new() }),
            ]),
            SourceKind::Actions => ("actions", vec![
                ("owner", self.owner.trim().to_string()),
                ("repo", self.repo.trim().to_string()),
                ("workflow", self.workflow.trim().to_string()),
                ("runs", self.runs.trim().to_string()),
                ("branch", self.branch.trim().to_string()),
                // On is the default, said by silence; only off writes.
                ("only_successful_builds", if self.success_only { String::new() } else { "false".to_string() }),
            ]),
            SourceKind::Url => ("url", vec![("urls", clean_list(&self.urls))]),
        };
        source.set("type", kind_key);
        for (key, value) in typed {
            source.set(key, &value);
        }
        source.set("filters", &clean_list(&self.filters));
        source.set("out", self.out.trim());
        source.set("releases", self.releases.trim());
        source.set("keep", self.keep.trim());
        let extract = clean_list(&self.extract);
        source.set("extract", &extract);
        if extract.is_empty() {
            // No 'extract' means the loader never reads the rest of the block;
            // writing the keys away keeps the section honest.
            source.set("extract_to", "");
            source.set("strip_prefix", "");
            source.set("overwrite", "");
            source.set("keep_zip", "");
        } else {
            source.set("extract_to", self.extract_to.trim());
            source.set("strip_prefix", self.strip_prefix.trim());
            source.set("overwrite", if self.overwrite { "true" } else { "" });
            source.set("keep_zip", if self.keep_zip { "" } else { "false" });
        }
        Ok(source)
    }
}

const FIELD_LABEL: f32 = 130.0;
const FIELD_WIDTH: f32 = 300.0;
const ICON_SIZE: f32 = 20.0;
const TRASH_RED: egui::Color32 = egui::Color32::from_rgb(200, 70, 70);
const PENCIL_BLUE: egui::Color32 = egui::Color32::from_rgb(90, 140, 210);

/// One labeled line of the form: the name at the left, the box at the right,
/// and the key's documentation on hover of the whole line.
fn field(ui: &mut egui::Ui, label: &str, text: &str, hint: &str, value: &mut String) {
    let response = ui
        .horizontal(|ui| {
            ui.add_sized(
                [FIELD_LABEL, 20.0],
                egui::Label::new(egui::RichText::new(label).weak()).selectable(false),
            );
            let edit = egui::TextEdit::singleline(value).desired_width(FIELD_WIDTH - 8.0);
            ui.add_sized(
                [FIELD_WIDTH, 20.0],
                if hint.is_empty() { edit } else { edit.hint_text(hint) },
            );
        })
        .response;
    response.on_hover_text(text);
}

/// A clipboard line made to fit a one-line field: the line breaks and tabs
/// the copy carries become `", "` - the separator the list fields speak - and
/// the ends of the whole are trimmed.
fn paste_clean(text: &str) -> String {
    text.split(['\n', '\r', '\t'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

fn check_field(ui: &mut egui::Ui, label: &str, text: &str, value: &mut bool) {
    let response = ui
        .horizontal(|ui| {
            ui.add_sized(
                [FIELD_LABEL, 20.0],
                egui::Label::new(egui::RichText::new(label).weak()).selectable(false),
            );
            ui.checkbox(value, "");
        })
        .response;
    response.on_hover_text(text);
}

/// A small icon button, drawn with lines: the bundled font carries no icons,
/// and the codebase already paints its own marks (the run's spinner, the
/// unzip's square). The tint darkens on hover - red for the trash, blue for
/// the pencil - so intent is visible before the click.
fn icon_button(
    ui: &mut egui::Ui,
    salt: &str,
    tooltip: &str,
    hover: egui::Color32,
    draw: fn(&egui::Painter, egui::Rect, egui::Color32),
) -> bool {
    let _ = salt;
    // Click sense, not just hover: a hover-only response reports no clicks,
    // and the mark would answer nothing.
    let (rect, response) = ui.allocate_at_least(egui::vec2(ICON_SIZE, ICON_SIZE), egui::Sense::click());
    if response.hovered() {
        ui.painter().rect_filled(rect.shrink(1.0), 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
    }
    let tint = if response.hovered() { hover } else { ui.visuals().weak_text_color() };
    draw(ui.painter(), rect.shrink(3.0), tint);
    response.on_hover_text(tooltip).clicked()
}

fn paint_trash(painter: &egui::Painter, r: egui::Rect, color: egui::Color32) {
    let stroke = egui::Stroke::new(1.3_f32, color);
    let cx = r.center().x;
    let lid = r.top() + r.height() * 0.3;
    // The handle: a small bar on feet above the lid.
    painter.line_segment(
        [egui::pos2(cx - r.width() * 0.14, r.top()), egui::pos2(cx + r.width() * 0.14, r.top())],
        stroke,
    );
    painter.line_segment(
        [egui::pos2(cx - r.width() * 0.14, r.top()), egui::pos2(cx - r.width() * 0.14, lid)],
        stroke,
    );
    painter.line_segment(
        [egui::pos2(cx + r.width() * 0.14, r.top()), egui::pos2(cx + r.width() * 0.14, lid)],
        stroke,
    );
    // The lid, and the bucket narrowing to its base.
    painter.line_segment([egui::pos2(r.left(), lid), egui::pos2(r.right(), lid)], stroke);
    painter.add(egui::Shape::closed_line(
        vec![
            egui::pos2(cx - r.width() * 0.34, lid),
            egui::pos2(cx - r.width() * 0.26, r.bottom()),
            egui::pos2(cx + r.width() * 0.26, r.bottom()),
            egui::pos2(cx + r.width() * 0.34, lid),
        ],
        stroke,
    ));
}

fn paint_pencil(painter: &egui::Painter, r: egui::Rect, color: egui::Color32) {
    let stroke = egui::Stroke::new(2.2_f32, color);
    let tip = egui::pos2(r.left() + r.width() * 0.15, r.bottom() - r.height() * 0.15);
    let top = egui::pos2(r.left() + r.width() * 0.8, r.top() + r.height() * 0.25);
    painter.line_segment([tip, top], stroke);
    painter.circle_filled(tip, 1.4, color);
}

impl SelectionApp {
    /// The modal dialogs, drawn over the panel of whichever phase is showing.
    /// The editor is taken out and put back, as the log strip takes its lines:
    /// the dialog writes while the app is read.
    fn editor_windows(&mut self, ui: &mut egui::Ui) {
        let mut editor = std::mem::replace(&mut self.editor, Editor::Closed);
        let model = self.model.clone();
        match &mut editor {
            Editor::Closed => {}
            Editor::ConfirmDelete(name) => {
                // The name is taken out of the editor first: the arm writes
                // the editor back at its end, and a borrow held into a modal
                // closure would keep that assignment from being allowed.
                let name = name.clone();
                let mut close = false;
                let mut remove = false;
                // The id is from the name, not unique: a unique id would be
                // a new one every frame, and the dialog's widgets - the
                // fields above all - need to be the same widget between
                // frames to be writable.
                egui::Modal::new(egui::Id::new("delete_source")).show(ui.ctx(), |ui| {
                    ui.heading("Delete source?");
                    ui.label(format!("Remove [{name}] from saishin.ini?"));
                        ui.label("Files already downloaded stay on disk.");
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                            let del = ui.add_sized(
                                egui::vec2(OK_WIDTH, OK_HEIGHT),
                                egui::Button::new(egui::RichText::new("Delete").strong()),
                            );
                            if ui.ctx().memory(|m| m.focused().is_none()) {
                                del.request_focus();
                            }
                            if del.clicked() {
                                remove = true;
                            }
                        });
                    });
                if close {
                    editor = Editor::Closed;
                }
                if remove {
                    let mut next = model.clone();
                    next.remove(&name);
                    // A refused write is told in the strip; the dialog closes
                    // either way - the list it offered was never the problem.
                    let _ = self.commit(next);
                    editor = Editor::Closed;
                }
            }
            Editor::Add(form) => {
                if self.form_window(ui, "Add source", None, &model, form) {
                    editor = Editor::Closed;
                }
            }
            Editor::Edit(old, form) => {
                let old = old.clone();
                if self.form_window(ui, "Edit source", Some(old), &model, form) {
                    editor = Editor::Closed;
                }
            }
        }
        self.editor = editor;
    }

    fn form_window(
        &mut self,
        ui: &mut egui::Ui,
        title: &str,
        old_name: Option<String>,
        model: &ConfigModel,
        form: &mut SourceForm,
    ) -> bool {
        let mut close = false;
        // The id is from the name and stays: it is one window serving both
        // **Add source** and **Edit source**, and its widgets - the text
        // fields most of all - are only editable while the id says they are
        // the same widget from frame to frame.
        egui::Modal::new(egui::Id::new("source_form")).show(ui.ctx(), |ui| {
            ui.heading(title);

                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.weak("type:");
                            ui.selectable_value(&mut form.kind, SourceKind::GitHub, "GitHub release")
                                .on_hover_text(doc("type"));
                            ui.selectable_value(&mut form.kind, SourceKind::Actions, "GitHub action")
                                .on_hover_text(doc("type"));
                            ui.selectable_value(&mut form.kind, SourceKind::Url, "Web URL")
                                .on_hover_text(doc("type"));
                        });
                        ui.separator();
                        field(ui, "name", doc("name"), "from owner, repo or url", &mut form.name);
                        match form.kind {
                            SourceKind::GitHub | SourceKind::Actions => {
                                field(ui, "owner", doc("owner"), "", &mut form.owner);
                                field(ui, "repo", doc("repo"), "", &mut form.repo);
                            }
                            SourceKind::Url => {
                                field(ui, "urls", doc("urls"), "https://…/a.zip, …", &mut form.urls);
                            }
                        }
                        match form.kind {
                            SourceKind::GitHub => {
                                field(ui, "tag", doc("tag"), "latest", &mut form.tag);
                                check_field(ui, "private", doc("private"), &mut form.private);
                                check_field(ui, "changelog", doc("changelog"), &mut form.changelog);
                                check_field(ui, "version_suffix", doc("version_suffix"), &mut form.version_suffix);
                            }
                            SourceKind::Actions => {
                                field(ui, "workflow", doc("workflow"), "CI", &mut form.workflow);
                                field(ui, "runs", doc("runs"), "10", &mut form.runs);
                                field(ui, "branch", doc("branch"), "main", &mut form.branch);
                                check_field(ui, "success only", doc("only_successful_builds"), &mut form.success_only);
                            }
                            SourceKind::Url => {}
                        }
                        field(ui, "filters", doc("filters"), "*.zip, !*-dev-*", &mut form.filters);
                        field(ui, "out", doc("out"), "[general] out_dir", &mut form.out);
                        field(ui, "releases", doc("releases"), "10", &mut form.releases);
                        field(ui, "keep", doc("keep"), "0", &mut form.keep);
                        ui.separator();
                        field(ui, "extract", doc("extract"), "*", &mut form.extract);
                        field(ui, "extract_to", doc("extract_to"), "out/name", &mut form.extract_to);
                        field(ui, "strip_prefix", doc("strip_prefix"), "0", &mut form.strip_prefix);
                        check_field(ui, "overwrite", doc("overwrite"), &mut form.overwrite);
                        check_field(ui, "keep_zip", doc("keep_zip"), &mut form.keep_zip);
                        if form.private && matches!(form.kind, SourceKind::GitHub) {
                            ui.weak("A private repository needs a token that can read it: run 'saishin --login' once.");
                        }
                        if !form.error.is_empty() {
                            ui.colored_label(egui::Color32::from_rgb(210, 80, 80), &form.error);
                        }
                    });
                ui.separator();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let save = ui.add_sized(
                        egui::vec2(OK_WIDTH, OK_HEIGHT),
                        egui::Button::new(egui::RichText::new("Save").strong()),
                    );
                    // The keys are handed over only while the dialog holds
                    // nothing focused: asked every frame, the request would
                    // pull the caret out of any field the cursor picked, and
                    // the fields could not be typed in at all.
                    if ui.ctx().memory(|m| m.focused().is_none()) {
                        save.request_focus();
                    }
                    if save.clicked() {
                        match form.apply(model, old_name.as_deref()) {
                            Err(msg) => form.error = msg,
                            Ok(source) => {
                                let mut next = model.clone();
                                match &old_name {
                                    Some(old) => {
                                        // A rename keeps the section's place in
                                        // the file: the order of the ini is the
                                        // order of the run.
                                        let at = next
                                            .sources
                                            .iter()
                                            .position(|s| s.name.eq_ignore_ascii_case(old));
                                        next.remove(old);
                                        match at {
                                            Some(i) => next.sources.insert(i.min(next.sources.len()), source),
                                            None => next.sources.push(source),
                                        }
                                    }
                                    None => next.sources.push(source),
                                }
                                match self.commit(next) {
                                    Ok(()) => close = true,
                                    Err(msg) => form.error = msg,
                                }
                            }
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        close
    }

    /// The sections with no rows, listed with the marks: a source that found
    /// nothing new is invisible in the table above, and a person editing the
    /// file from the window has to be able to reach those too - they are the
    /// majority of it on a quiet day. One row per section, trash and pencil
    /// at the left, the name saying which. `seen` is the table's sources,
    /// lower-cased; the empty screen passes nothing and lists every section.
    fn quiet_list(
        &self,
        ui: &mut egui::Ui,
        model: &ConfigModel,
        seen: &[String],
    ) -> Option<(String, bool)> {
        let quiet: Vec<&EditableSource> = model
            .sources
            .iter()
            .filter(|section| !seen.contains(&section.name.to_lowercase()))
            .collect();
        if quiet.is_empty() {
            return None;
        }
        let mut action: Option<(String, bool)> = None;
        ui.add_space(10.0);
        ui.weak("sources with no new artifacts:");
        for section in quiet {
            let name = section.name.clone();
            ui.horizontal(|ui| {
                let trash = icon_button(
                    ui,
                    &format!("trash-{name}"),
                    &format!("Delete [{name}] from saishin.ini"),
                    TRASH_RED,
                    paint_trash,
                );
                let edit = icon_button(
                    ui,
                    &format!("edit-{name}"),
                    &format!("Edit [{name}]"),
                    PENCIL_BLUE,
                    paint_pencil,
                );
                if trash {
                    action = Some((name.clone(), false));
                }
                if edit {
                    action = Some((name, true));
                }
                ui.label(section.name.clone());
                ui.weak("no new artifacts");
            });
        }
        action
    }

    /// Write the model and restart the check against it. The typed sources are
    /// derived first, through the loader's own rules, so a section the CLI
    /// would refuse never reaches the file; the write itself is atomic and
    /// leaves the previous file in saishin.ini.bak. Comments inside an edited
    /// source section are not kept; the header up to [general] is.
    fn commit(&mut self, model: ConfigModel) -> Result<(), String> {
        let cfg = &self.job.config;
        let derived: Result<Vec<SourceConfig>, AppError> = model
            .sources
            .iter()
            .map(|s| s.to_source(&cfg.default_out_dir, &cfg.base_directory, cfg.releases))
            .collect();
        let sources = match derived {
            Ok(sources) => sources,
            Err(ex) => {
                let msg = error_message(ex);
                log::error(&format!("The change was refused: {msg}"));
                return Err(msg);
            }
        };
        if let Err(ex) = config::save(&model) {
            let msg = error_message(ex);
            log::error(&format!("Could not save saishin.ini: {msg}"));
            return Err(msg);
        }
        log::info("saishin.ini updated.");
        self.model = model;
        self.job = Arc::new(GuiJob {
            sources,
            config: cfg.clone(),
            token: self.job.token.clone(),
            options: self.job.options.clone(),
        });
        self.entries.clear();
        self.families.clear();
        self.chosen.clear();
        {
            let mut progress = self.progress.lock().unwrap_or_else(|guard| guard.into_inner());
            progress.phase = Phase::Loading;
            progress.heading.clear();
            progress.message.clear();
            progress.rows.clear();
        }
        self.start_load();
        Ok(())
    }
}

/// Text for one entry of the version dropdown. Versions of the same artifact
/// often carry different file names (`dmdext-v2.5.0-x64.zip` per release), so
/// the name is added next to the version whenever the group's names differ -
/// otherwise the list would be a column of tags telling you nothing about what
/// you are about to download.
pub(crate) fn version_cell_label(version: &PendingFile, show_names: bool) -> String {
    let label = version.version_label.trim();
    if label.is_empty() {
        return version.file_name.clone();
    }
    if show_names {
        format!("{label}  -  {}", version.file_name)
    } else {
        label.to_string()
    }
}

impl SelectionApp {
    #[allow(clippy::too_many_arguments)]
    fn new(
        entries: Vec<PendingFile>,
        version_map: HashMap<(String, String), Vec<PendingFile>>,
        job: Arc<GuiJob>,
        result: Arc<GuiResult>,
        cancel: Arc<AtomicBool>,
        progress: Arc<Mutex<GuiProgress>>,
        load: Arc<Mutex<LoadState>>,
        model: ConfigModel,
        window_centered: bool,
        centered_at_startup: bool,
    ) -> SelectionApp {
        let mut entries: Vec<ArtifactEntry> = entries
            .into_iter()
            .map(|file| {
                let mut versions = version_map
                    .get(&(file.source_name.clone(), file.group_key.clone()))
                    .cloned()
                    .unwrap_or_default();
                let version_index = match versions.iter().position(|version| version.url == file.url) {
                    Some(index) => index,
                    None => {
                        // The row sits outside the version window (a source
                        // with tag = all, for example); keep it selectable.
                        versions.insert(0, file.clone());
                        0
                    }
                };
                ArtifactEntry {
                    file,
                    selected: true,
                    versions,
                    version_index,
                    family: None,
                }
            })
            .collect();

        // One release, or one workflow run, usually carries several files.
        // Rows that offer exactly the same versions are given a family, and
        // the family gets a single dropdown that switches all of its rows.
        let mut families: Vec<VersionFamily> = Vec::new();
        let mut by_labels: HashMap<(String, Vec<String>), usize> = HashMap::new();
        for (index, entry) in entries.iter_mut().enumerate() {
            if entry.versions.is_empty() {
                continue;
            }
            let labels: Vec<String> = entry
                .versions
                .iter()
                .map(|version| version_cell_label(version, false))
                .collect();
            let key = (entry.file.source_name.clone(), labels.clone());
            match by_labels.get(&key) {
                Some(family) => {
                    let family = *family;
                    if let Some(family) = families.get_mut(family) {
                        family.rows.push(index);
                    }
                    entry.family = Some(family);
                }
                None => {
                    by_labels.insert(key, families.len());
                    families.push(VersionFamily {
                        rows: vec![index],
                        labels,
                        picked: entry.version_index,
                    });
                    entry.family = Some(families.len() - 1);
                }
            }
        }

        SelectionApp {
            entries,
            families,
            job,
            result,
            cancel,
            progress,
            load,
            model,
            editor: Editor::Closed,
            chosen: Vec::new(),
            end_focused: false,
            log_lines: Vec::new(),
            log_revision: 0,
            log_jump: false,
            logo: None,
            shown: false,
            centered_at_startup,
            window_centered,
        }
    }

    /// The run's text in a strip at the bottom of the window. The window is the
    /// report, so everything a console run prints is readable here: the token's
    /// provenance, which urls matched, the download lines, the warnings. Closed
    /// it is one line - the newest that can be read whole in the width there is,
    /// with a line cut by that width given in full on hover; open, a list that
    /// follows the newest line, its lines wrapped to the window's width, and whose
    /// text can be selected and copied.
    fn log_panel(&mut self, ui: &mut egui::Ui) {
        // The run writes from its own thread and egui draws on input, so a new
        // line has to ask for the frame that shows it.
        let revision = log::revision();
        if revision != self.log_revision {
            self.log_revision = revision;
            self.log_lines = log::lines();
            ui.ctx().request_repaint();
        }

        // Out of self while the strip is drawn: the header's click is written
        // while its content is read.
        let lines = std::mem::take(&mut self.log_lines);
        let count = lines.len();

        // Which side of the strip this frame is. The collapsing header keeps
        // its open state under its id, and both sides give theirs the same
        // id_salt, so clicking the triangle on either side carries the toggle
        // to the other; this copy is what picks the panel to draw.
        let open_id = ui.scope_id().with("log-strip-open");
        let mut open = ui.data_mut(|d| d.get_persisted::<bool>(open_id)).unwrap_or(false);

        // The lines are the point of this strip. The header's toggle needs the
        // left inset and nothing needs the right, so the frame hugs the right
        // edge; the body is drawn unindented, because the 18 pt of indent a
        // collapsing header asks its body for is width a wrapped path line loses
        // and gains nothing by; and the scrollbar floats over the text instead of
        // taking a column of its own.
        let frame = |ui: &egui::Ui| {
            egui::Frame::new().fill(ui.style().visuals.panel_fill).inner_margin(
                egui::Margin { left: 8, right: 2, top: 2, bottom: 2 },
            )
        };

        if open {
            // A resizable panel: its top edge is a divider the pointer can
            // take, and egui remembers the height it leaves. The height is
            // only honoured while the contents fill the panel, so the list
            // below asks for the panel's whole remaining height rather than a
            // fixed 180 pt, and the divider has the full edge to drag on.
            egui::Panel::bottom("log")
                .resizable(true)
                .default_size(LOG_LINES_HEIGHT)
                .min_size(LOG_MIN_HEIGHT)
                .frame(frame(ui))
                .show(ui, |ui| {
                    let style = ui.style_mut();
                    style.spacing.scroll.floating = true;
                    style.spacing.scroll.floating_allocated_width = 0.0;

                    let header = egui::CollapsingHeader::new(format!("Log ({count} lines)"))
                        .id_salt("log")
                        .default_open(true)
                        .show_unindented(ui, |ui| {
                            let list_height = ui.available_height();
                            let mut scroll = egui::ScrollArea::vertical()
                                .id_salt("log_scroll")
                                .stick_to_bottom(true)
                                .max_height(list_height)
                                // Fill both ways: a vertical scroll area hands
                                // its content an unbounded width by default, and
                                // a label with no width to wrap at draws one long
                                // line that loses its end at the window's edge;
                                // a height that shrinks to its rows would leave
                                // the space the divider's drag gained unused.
                                .auto_shrink([false, false]);
                            if self.log_jump {
                                // An offset past the last line lands on the
                                // last line; a frame later the handle touches
                                // the end and sticks, so the lines that come
                                // after the jump follow on their own.
                                scroll = scroll.vertical_scroll_offset(f32::MAX);
                                self.log_jump = false;
                            }
                            let view = scroll.show(ui, |ui| {
                                for line in lines.iter() {
                                    ui.add(egui::Label::new(log_line_text(line)).selectable(true).wrap());
                                }
                            });
                            // The end of the scroll, computed from the view:
                            // the offset has reached the distance the content
                            // exceeds the viewport by - within a line's slack,
                            // because a wheel tick that lands on the last line
                            // is a scroll to its end, not a miss.
                            let past = view.content_size.y - view.inner_rect.height();
                            let at_end = view.state.clamped_offset().y >= past - 24.0;
                            if !at_end {
                                // Scrolled up: the newest line is off-screen,
                                // and a download that keeps writing is a story
                                // the window is no longer watching. Say so, and
                                // offer the way back - the button is gone as
                                // soon as the view is at the end again, by this
                                // button or by the user's own scroll.
                                ui.horizontal_centered(|ui| {
                                    if ui.button("\u{2193} end of log").clicked() {
                                        self.log_jump = true;
                                    }
                                });
                            }
                        });
                    // The body is `None` exactly while the header's click has
                    // closed it, and the strip's one-line side takes the next
                    // frame. The divider's remembered height is the open panel's
                    // own state and waits under the strip for the next opening.
                    if header.body_returned.is_none() {
                        open = false;
                    }
                });
        } else {
            // The closed strip is a panel of its own, unresizable: a resizable
            // panel keeps the height the divider left it, and that much empty
            // frame under a one-line strip is not a strip at all. Its id is not
            // the open panel's, so its own small size is what gets stored and
            // the strip's list keeps the height the last drag left.
            egui::Panel::bottom("log_closed")
                .frame(frame(ui))
                .show(ui, |ui| {
                    let style = ui.style_mut();
                    style.spacing.scroll.floating = true;
                    style.spacing.scroll.floating_allocated_width = 0.0;

                    // The width the strip's text has, in characters. The window
                    // is whatever width the user left it, so the line the closed
                    // strip picks is chosen against the room there is, not a
                    // fixed count.
                    let chars = strip_chars(ui.available_width());

                    let header = egui::CollapsingHeader::new(format!("Log ({count} lines)"))
                        .id_salt("log")
                        .default_open(false)
                        .show_unindented(ui, |_ui| {});
                    if header.body_returned.is_some() {
                        // The body ran, so the triangle has been clicked: the
                        // strip asks to be the open side from the next frame.
                        open = true;
                    } else if let Some(line) = strip_line(&lines, chars) {
                        // `truncate` keeps the strip one line tall whatever the
                        // text, and the tooltip is what makes a line that is cut
                        // by that readable rather than merely short.
                        let strip = ui.add(
                            egui::Label::new(log_line_text(line))
                                .selectable(true)
                                .truncate()
                                .show_tooltip_when_elided(true),
                        );
                        // A plain click opens the strip too; a selection drag is
                        // not a click, so copying a path out of the strip keeps
                        // working.
                        if strip.clicked() {
                            open = true;
                        }
                    }
                });
        }

        ui.data_mut(|d| d.insert_persisted(open_id, open));
        self.log_lines = lines;
    }

    /// The app as the window opens it: no rows yet, the sources not yet asked,
    /// and the asking already running on its own thread.
    fn new_loading(
        job: Arc<GuiJob>,
        result: Arc<GuiResult>,
        cancel: Arc<AtomicBool>,
        progress: Arc<Mutex<GuiProgress>>,
        load: Arc<Mutex<LoadState>>,
        model: ConfigModel,
        window_centered: bool,
        centered_at_startup: bool,
    ) -> SelectionApp {
        let app = SelectionApp::new(
            Vec::new(),
            HashMap::new(),
            job,
            result,
            cancel,
            progress,
            load,
            model,
            window_centered,
            centered_at_startup,
        );
        app.start_load();
        app
    }

    /// Asking the sources, off the UI thread: seconds when there are several of
    /// them, and every line it logs is readable in the window while it runs.
    fn start_load(&self) {
        let job = Arc::clone(&self.job);
        let load = Arc::clone(&self.load);
        thread::spawn(
            move || {
                let runtime = match Builder::new_multi_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(ex) => {
                        fail_load(&load, format!("Could not start the check: {ex}"));
                        return;
                    }
                };
                let http = match Downloader::new() {
                    Ok(http) => http,
                    Err(ex) => {
                        fail_load(&load, error_message(ex));
                        return;
                    }
                };
                runtime.block_on(
                    async {
                        match collect_new(&job.sources, &job.config, &http, job.token.as_deref(), &job.options).await {
                            Ok((entries, versions, failures)) => {
                                let total = job.sources.len();
                                let note = sources_note(total - failures.len(), total);
                                *load.lock().unwrap_or_else(|guard| guard.into_inner()) = LoadState::Ready { entries, versions, note };
                            }
                            Err(ex) => fail_load(&load, error_message(ex)),
                        }
                    },
                );
            },
        );
    }

    /// The rows for the files the sources offered. `SelectionApp::new` is the
    /// code that builds rows and version families, so the window hands it the
    /// empty list at start-up and the full list when the fetch lands, and keeps
    /// the two fields it worked out.
    fn adopt_rows(&mut self, entries: Vec<PendingFile>, versions: HashMap<(String, String), Vec<PendingFile>>) {
        let built = SelectionApp::new(entries, versions, Arc::clone(&self.job), Arc::clone(&self.result), Arc::clone(&self.cancel), Arc::clone(&self.progress), Arc::clone(&self.load), self.model.clone(), false, false);
        self.entries = built.entries;
        self.families = built.families;
    }

    fn start_download(&mut self, chosen: Vec<usize>) {
        // Remembered, because the end state offers to start these rows again
        // from where they stopped.
        self.chosen = chosen.clone();
        // The next run's end gets its own focused **OK**.
        self.end_focused = false;
        // Cleared here, because every way a run starts comes through this
        // function: the flag **Cancel** set for the run that is over is the
        // flag the new worker reads first, and a run that inherits a cancel
        // stops before it moves.
        self.cancel.store(false, Ordering::Relaxed);
        let chosen_files: Vec<PendingFile> = chosen
            .iter()
            .filter_map(|index| self.entries.get(*index))
            .map(|entry| entry.file.clone())
            .collect();

        let mut progress = self.progress.lock().unwrap_or_else(|guard| guard.into_inner());
        progress.phase = Phase::Downloading;
        progress.done = 0;
        progress.total = chosen_files.len();
        progress.message = "".to_string();
        progress.rows = self.entries
            .iter()
            .map(|_| RowState::Pending)
            .collect();

        let job = Arc::clone(&self.job);
        let cancel = Arc::clone(&self.cancel);
        let progress_arc = Arc::clone(&self.progress);
        let result = Arc::clone(&self.result);

        // Built from the rows as they now stand, so a version swapped in the
        // dropdown reports progress on the right row.
        let index: HashMap<(String, String), usize> = self.entries
            .iter()
            .enumerate()
            .map(|(i, entry)| ((entry.file.source_name.clone(), entry.file.file_name.clone()), i))
            .collect();
        let sink = GuiSink { progress: Arc::clone(&self.progress), index };

        thread::spawn(
            move || {
                let http = match Downloader::new() {
                    Ok(http) => http,
                    Err(ex) => {
                        let mut seen = progress_arc.lock().unwrap_or_else(|guard| guard.into_inner());
                        seen.phase = Phase::Failed;
                        seen.message = error_message(ex);
                        return;
                    }
                };
                let runtime = match Builder::new_multi_thread().enable_all().build() {
                    Ok(runtime) => runtime,
                    Err(ex) => {
                        let mut seen = progress_arc.lock().unwrap_or_else(|guard| guard.into_inner());
                        seen.phase = Phase::Failed;
                        seen.message = format!("Could not start the download worker: {ex}");
                        return;
                    }
                };
                runtime.block_on(
                    async {
                        let cancel_ref: &Arc<AtomicBool> = &cancel;
                        for source in job.sources.iter() {
                            let files: Vec<PendingFile> = chosen_files
                                .iter()
                                .filter(|file| file.source_name == source.name)
                                .cloned()
                                .collect();
                            if files.is_empty() {
                                continue;
                            }
                            match download_pending(source, &job.config, &http, job.token.as_deref(), &files, &job.options, cancel_ref, &sink).await {
                                Ok(names) => {
                                    for name in names.iter() {
                                        result.updated.lock().unwrap_or_else(|guard| guard.into_inner()).push(format!("{}: {name}", source.name));
                                    }
                                }
                                Err(ex) => {
                                    let mut seen = progress_arc.lock().unwrap_or_else(|guard| guard.into_inner());
                                    seen.phase = Phase::Failed;
                                    seen.message = error_message(ex);
                                    // A transfer that broke mid-way leaves pieces
                                    // too, and the end state asks about those.
                                    seen.partials = take_partials();
                                    return;
                                }
                            }
                            if cancel_ref.load(Ordering::Relaxed) {
                                let mut seen = progress_arc.lock().unwrap_or_else(|guard| guard.into_inner());
                                seen.phase = Phase::Cancelled;
                                seen.partials = take_partials();
                                return;
                            }
                            if !job.options.dry_run && source.keep > 0 {
                                prune_old_files(&source.out_dir, source.keep, &source.filters);
                            }
                        }
                        let mut seen = progress_arc.lock().unwrap_or_else(|guard| guard.into_inner());
                        if matches!(seen.phase, Phase::Downloading) {
                            seen.phase = Phase::Done;
                        }
                    },
                );
            },
        );
    }

    /// Answers the question about the half downloaded files.
    ///
    /// Keeping changes nothing on disk: the pieces are exactly what the next run
    /// resumes from, which is why a cancel leaves them there in the first place.
    /// Deleting removes the files this run reported, no others, and the strip
    /// says how many went. An answer is final for this window: the list is
    /// emptied, and the window's own words take the question's place.
    fn answer_partials(&mut self, listed: &[String], keep: bool) {
        let mut progress = self
            .progress
            .lock()
            .unwrap_or_else(|guard| guard.into_inner());
        progress.partials.clear();
        if keep {
            progress.answer = format!(
                "Keeping {} half downloaded file(s): the next run continues them.",
                listed.len()
            );
            log::info(&progress.answer);
            return;
        }
        let mut removed = 0usize;
        for path in listed {
            if std::fs::remove_file(path).is_ok() {
                removed += 1;
            }
        }
        progress.answer = format!(
            "Removed {removed} of {} half downloaded file(s).",
            listed.len()
        );
        log::info(&progress.answer);
    }

    /// **Delete** is the answer that empties the run. The half downloaded files
    /// are what a transfer continues from, so with them gone there is nothing
    /// left to continue and the offer goes with them: `chosen` goes, the cancel
    /// flag goes with the run that is over, and the window goes back to the rows
    /// with their boxes as the last run left them - which is where a pick that
    /// starts over belongs. The strip keeps its lines: they are the finished
    /// run's own, and the next run adds its own to them.
    fn delete_partials(&mut self, listed: &[String]) {
        self.answer_partials(listed, false);
        self.chosen.clear();
        self.cancel.store(false, Ordering::Relaxed);
        let mut progress = self
            .progress
            .lock()
            .unwrap_or_else(|guard| guard.into_inner());
        progress.heading.clear();
        progress.phase = Phase::Selecting;
    }

    /// Starts the same rows again, from where the run left off. The half
    /// downloaded files are exactly what a transfer continues from, so resuming
    /// is the answer **Keep** and the download again in one press: the question
    /// is answered, the pieces stay where they are, and the rows start moving
    /// again under the same heading.
    fn resume(&mut self) {
        let chosen = self.chosen.clone();
        let words = {
            let mut progress = self
                .progress
                .lock()
                .unwrap_or_else(|guard| guard.into_inner());
            let kept = progress.partials.len();
            progress.partials.clear();
            // The phase's own heading again, as on the first run.
            progress.heading = String::new();
            progress.answer = if kept == 0 {
                "Resuming the run from where it stopped.".to_string()
            } else {
                format!("Resuming: keeping the {kept} half downloaded file(s) to continue them.")
            };
            progress.answer.clone()
        };
        log::info(&words);
        // The start clears the cancel flag the **Cancel** button set, so the
        // resumed worker is not the one that was stopped.
        self.start_download(chosen);
    }

    /// The files as rows: the mark of what each is doing, its source, its size,
    /// and a link to where it came from. The same list serves the running screen
    /// and the end screen, so a run's rows stay on screen as its result: the
    /// checks are the list of what came down. `running` is the window's say on
    /// whether the run is still up, which decides what the busy rows draw.
    fn file_rows(&self, ui: &mut egui::Ui, rows: &[RowState], running: bool) {
        let list = egui::ScrollArea::vertical()
            .auto_shrink([false, !running])
            .scroll_bar_visibility(egui::containers::scroll_area::ScrollBarVisibility::AlwaysVisible);
        let list = if running {
            list
        } else {
            list.max_height(END_LIST_MAX_HEIGHT)
        };
        list.show(ui, |ui| {
            egui::Grid::new("download_list")
                .spacing(egui::vec2(8.0, 4.0))
                .show(ui, |ui| {
                    for (index, entry) in self.entries.iter().enumerate() {
                        let state = if index < rows.len() {
                            rows[index]
                        } else {
                            RowState::Pending
                        };
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(MARK_SIZE, MARK_SIZE), egui::Sense::hover());
                        row_mark(ui, rect, state, running);
                        // One line to a row: the list is the run's result, and a
                        // cell that wraps puts the row's mark beside its own
                        // second line, which reads as two rows. The cells are cut
                        // instead, and the whole of a name is one hover, one
                        // click, or a strip line away.
                        ui.add_sized(
                            egui::vec2(ROW_SOURCE_WIDTH, ROW_CELL_HEIGHT),
                            egui::Label::new(entry.file.source_name.clone())
                                .truncate()
                                .show_tooltip_when_elided(true),
                        );
                        let size = match entry.file.size {
                            Some(bytes) => format_bytes(bytes),
                            None => "size unknown".to_string(),
                        };
                        ui.label(size);
                        ui.hyperlink_to(short_name(&entry.file.file_name), &entry.file.link_url);
                        ui.end_row();
                    }
                });
        });
    }
}

impl App for SelectionApp {
    /// egui 0.37 split `update` in two: `logic` runs on the context (viewport
    /// commands belong here), `ui` gets the root `Ui` the panels are drawn into.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // One upload, from bytes that are already pixels. The texture lives on
        // the GPU for the rest of the session.
        if self.logo.is_none() {
            self.logo = Some(ctx.load_texture("saishin logo", logo_image(), egui::TextureOptions::LINEAR));
        }

        // While work is running, the window asks for a frame about eight times a
        // second: that is what makes the strip, the spinner and the arrival of
        // the rows show without anybody moving the mouse.
        let phase = self.progress.lock().unwrap_or_else(|guard| guard.into_inner()).phase;
        if matches!(phase, Phase::Loading | Phase::Downloading) {
            ctx.request_repaint_after(Duration::from_millis(120));
        }

        if matches!(phase, Phase::Loading) {
            let taken = std::mem::replace(
                &mut *self.load.lock().unwrap_or_else(|guard| guard.into_inner()),
                LoadState::Working,
            );
            match taken {
                LoadState::Ready { entries, versions, note } => {
                    self.adopt_rows(entries, versions);
                    let mut progress = self.progress.lock().unwrap_or_else(|guard| guard.into_inner());
                    if let Some(note) = note {
                        // The rows are the whole story when nothing was refused,
                        // so the heading is free to carry the part that is not a
                        // row: a source that stayed silent has no row.
                        progress.heading = note;
                    }
                    progress.phase = Phase::Selecting;
                    ctx.request_repaint();
                }
                LoadState::Failed(message) => {
                    let mut progress = self.progress.lock().unwrap_or_else(|guard| guard.into_inner());
                    progress.heading = "Could not check the sources".to_string();
                    progress.message = message;
                    progress.phase = Phase::Failed;
                    ctx.request_repaint();
                }
                LoadState::Working => {}
            }
        }

        if self.window_centered && !self.shown {
            if self.centered_at_startup {
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                self.shown = true;
            } else {
                let (monitor, outer) = ctx.input(|i| {
                    let v = i.viewport();
                    (v.monitor_size, v.outer_rect)
                });
                if let (Some(m), Some(o)) = (monitor, outer) {
                    let size = o.size();
                    let target = egui::pos2(m.x / 2.0 - size.x / 2.0, m.y / 2.0 - size.y / 2.0);
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(target));
                    if (o.min.x - target.x).abs() < 2.0 && (o.min.y - target.y).abs() < 2.0 {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                        self.shown = true;
                    }
                }
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // The clipboard hands over whole lines - browser address bars and ini
        // snippets carry newlines and tabs - and the fields are one line
        // wide, with the list fields read as comma separated. Straightening
        // the paste here, before any field sees it, is what turns a pasted
        // block of urls into one list field the loader can read.
        ui.ctx().input_mut(|input| {
            for event in &mut input.events {
                if let egui::Event::Paste(text) = event {
                    *text = paste_clean(text);
                }
            }
        });

        let phase = self.progress.lock().unwrap_or_else(|guard| guard.into_inner()).phase;

        // The two kanji are outside egui's bundled font, so the glyphs have to
        // come from this machine. Asking for them is what makes eframe's system
        // font provider look them up; a machine without a font carrying them
        // gets the logo on its own, and no row of tofu.
        let kanji = ui.ctx().fonts_mut(|fonts| {
            fonts.has_glyphs(&egui::FontId::proportional(HEADER_WORD_SIZE), HEADER_WORD)
        });

        egui::Panel::top("header")
            .exact_size(HEADER_ROW)
            // The frame keeps the panel's own background; only the inset is set.
            .frame(
                egui::Frame::new()
                    .fill(ui.style().visuals.panel_fill)
                    .inner_margin(egui::Margin::symmetric(HEADER_SIDE_MARGIN, 0)),
            )
            .show_separator_line(false)
            .show(ui, |ui| {
                // Full height, centered: the logo and the word sit on one line
                // at the left, and the row keeps its height with no word in it.
                ui.horizontal_centered(|ui| {
                    if let Some(logo) = &self.logo {
                        ui.add(egui::Image::from_texture(logo).max_height(HEADER_LOGO));
                    }
                    if kanji {
                        ui.label(egui::RichText::new(HEADER_WORD).size(HEADER_WORD_SIZE).strong());
                    }
                });
            });

        self.log_panel(ui);

        match phase {
            Phase::Selecting => {
                egui::Panel::bottom("actions").show(ui, |ui| {
                    if self.entries.is_empty() {
                        // The check is the whole run when it finds nothing: no
                        // rows to pick, nothing to download, so one button answers
                        // it. What the sources said is in the strip above, and the
                        // heading says when one of them was refused. **Add
                        // source** stays: the empty list is exactly when a source
                        // is wanted, and with every source deleted this is the
                        // window's only way out.
                        ui.horizontal(|ui| {
                            if ui.button("+ Add source").on_hover_text("Add a new download source to saishin.ini.").clicked() {
                                self.editor = Editor::Add(SourceForm::new());
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let ok = ui.add_sized(
                                    egui::vec2(OK_WIDTH, OK_HEIGHT),
                                    egui::Button::new(egui::RichText::new("OK").strong()),
                                );
                                if !self.end_focused {
                                    ok.request_focus();
                                    self.end_focused = true;
                                }
                                if ok.clicked() {
                                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            });
                        });
                        return;
                    }
                    ui.horizontal(|ui| {
                        if ui.button("+ Add source").on_hover_text("Add a new download source to saishin.ini.").clicked() {
                            self.editor = Editor::Add(SourceForm::new());
                        }
                        if ui.button("Select all").clicked() {
                            for entry in self.entries.iter_mut() {
                                entry.selected = true;
                            }
                        }
                        if ui.button("Select none").clicked() {
                            for entry in self.entries.iter_mut() {
                                entry.selected = false;
                            }
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Download selected").clicked() {
                            let chosen = self.entries
                                .iter()
                                .enumerate()
                                .filter(|(_, entry)| entry.selected)
                                .map(|(index, _)| index)
                                .collect();
                            self.start_download(chosen);
                        }
                        if ui.button("Cancel").clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });

                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("Saishin - new artifacts");

                    if self.entries.is_empty() {
                        // Every source deleted is a legal state of the file, and
                        // this is the window's one screen for saying so.
                        if self.model.sources.is_empty() {
                            ui.label("No sources in saishin.ini. Use **Add source** below.");
                            return;
                        }
                        ui.label("No new artifacts.");
                        // The sections are all reachable here: none of them has
                        // rows, so the list below is the whole file.
                        let model = self.model.clone();
                        if let Some((name, is_edit)) = self.quiet_list(ui, &model, &[]) {
                            self.editor = if is_edit {
                                match model.find(&name) {
                                    Some(section) => {
                                        Editor::Edit(name, SourceForm::from_source(section))
                                    }
                                    None => Editor::Closed,
                                }
                            } else {
                                Editor::ConfirmDelete(name)
                            };
                        }
                        return;
                    }

                    ui.label(format!("{} new artifact(s)", self.entries.len()));
                    ui.label("Pick an older version in the version column to download that instead.");

                    // Picking a version changes rows, so the picks are
                    // collected while drawing and applied once the list is
                    // rendered. The same pattern serves the row's trash and
                    // pencil: a click is remembered, and the editor opens
                    // when the grid has let go of the entries.
                    let mut picks: Vec<(usize, usize)> = Vec::new();
                    let mut row_action: Option<(String, bool)> = None;
                    // The rows ask the model for the section a name edits,
                    // and the drawing loop already borrows the app.
                    let model = self.model.clone();
                    // The marks ride the first row of each source only: the
                    // rows of one source stand under one section, and the
                    // marks above its name answer for all of them.
                    let mut last_source: Option<String> = None;
                    // Which sections have rows: the ones without are listed
                    // under the table, where their marks can reach them too.
                    let seen: Vec<String> = self
                        .entries
                        .iter()
                        .map(|entry| entry.file.source_name.to_lowercase())
                        .collect();

                    egui::ScrollArea::both()
                        .auto_shrink([false, false])
                        .scroll_bar_visibility(egui::containers::scroll_area::ScrollBarVisibility::AlwaysVisible)
                        .show(ui, |ui| {
                            egui::Grid::new("artifact_list")
                                .spacing(egui::vec2(8.0, 4.0))
                                .show(ui, |ui| {
                                    ui.label("");
                                    ui.strong("source");
                                    ui.strong("size");
                                    ui.strong("version");
                                    ui.strong("file");
                                    ui.end_row();
                                    for (index, entry) in self.entries.iter_mut().enumerate() {
                                        // Checkmark on every row; the trash and
                                        // the pencil only where the source
                                        // starts: the marks edit one section,
                                        // and repeating them per row is noise.
                                        let source = entry.file.source_name.clone();
                                        let marks = last_source
                                            .as_deref()
                                            .map_or(true, |prev| !prev.eq_ignore_ascii_case(&source));
                                        last_source = Some(source.clone());
                                        ui.horizontal(|ui| {
                                            ui.checkbox(&mut entry.selected, "");
                                            if marks {
                                                let trash = icon_button(
                                                    ui,
                                                    &format!("trash-{source}"),
                                                    &format!("Delete [{source}] from saishin.ini"),
                                                    TRASH_RED,
                                                    paint_trash,
                                                );
                                                let edit = icon_button(
                                                    ui,
                                                    &format!("edit-{source}"),
                                                    &format!("Edit [{source}]"),
                                                    PENCIL_BLUE,
                                                    paint_pencil,
                                                );
                                                if trash {
                                                    row_action = Some((source.clone(), false));
                                                }
                                                if edit {
                                                    row_action = Some((source, true));
                                                }
                                            }
                                        });
                                        ui.label(entry.file.source_name.clone());
                                        let size = match entry.file.size {
                                            Some(bytes) => format_bytes(bytes),
                                            None => "size unknown".to_string(),
                                        };
                                        ui.label(size);
                                        match entry.family {
                                            None => { ui.label(entry.file.version_label.clone()); }
                                            Some(family) if !is_family_owner(&self.families, family, index) => {
                                                // Same versions as the row that
                                                // owns the dropdown, and its
                                                // pick switches this row too.
                                                ui.label(entry.file.version_label.clone());
                                            }
                                            Some(family) => {
                                                // One dropdown for every row of
                                                // the family.
                                                let labels = self.families[family].labels.clone();
                                                let picked = self.families[family].picked;
                                                // Full file names are listed
                                                // next to the version whenever
                                                // this row's names differ, so
                                                // picking an entry is never a
                                                // guess.
                                                let show_names = entry.versions.len() > 1
                                                    && entry.versions.iter().any(|version| {
                                                        version.file_name != entry.versions[0].file_name
                                                    });
                                                let versions = entry.versions.clone();
                                                let mut current = picked;
                                                egui::ComboBox::from_id_salt((index, &entry.file.source_name, &entry.file.group_key))
                                                    .selected_text(version_cell_label(&entry.file, false))
                                                    .show_ui(ui, |ui| {
                                                        for (i, label) in labels.iter().enumerate() {
                                                            let text = match versions.get(i) {
                                                                Some(version) => version_cell_label(version, show_names),
                                                                None => label.clone(),
                                                            };
                                                            if ui.selectable_value(&mut current, i, text).changed() {
                                                                picks.push((family, i));
                                                            }
                                                        }
                                                    });
                                            }
                                        }
                                        ui.hyperlink_to(&entry.file.file_name, &entry.file.link_url);
                                        ui.end_row();
                                    }
                                });

                            // The sources the run found nothing new for stand
                            // below the table, so the marks reach every section
                            // in the file, not just the ones with rows.
                            if let Some(action) = self.quiet_list(ui, &model, &seen) {
                                row_action = Some(action);
                            }
                        });

                    if let Some((name, is_edit)) = row_action {
                        self.editor = if is_edit {
                            match model.find(&name) {
                                Some(section) => Editor::Edit(name, SourceForm::from_source(section)),
                                // A row whose section is gone (edited away in
                                // this very frame) opens nothing.
                                None => Editor::Closed,
                            }
                        } else {
                            Editor::ConfirmDelete(name)
                        };
                        ui.ctx().request_repaint();
                    }

                    if !picks.is_empty() {
                        apply_picks(&mut self.entries, &mut self.families, picks);
                        // Redraw right away, so the columns beside the picked
                        // row show the version that was chosen.
                        ui.ctx().request_repaint();
                    }
                });
            }
            Phase::Downloading => {
                egui::Panel::bottom("actions").show(ui, |ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            self.cancel.store(true, Ordering::Relaxed);
                        }
                    });
                });

                egui::CentralPanel::default().show(ui, |ui| {
                    let (rows, done, total) = {
                        let progress = self
                            .progress
                            .lock()
                            .unwrap_or_else(|guard| guard.into_inner());
                        (progress.rows.clone(), progress.done, progress.total)
                    };
                    ui.heading("Downloading...");
                    ui.label(format!("{done} of {total} files"));
                    self.file_rows(ui, &rows, true);
                });
            }
            Phase::Loading => {
                egui::Panel::bottom("actions").show(ui, |ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });

                egui::CentralPanel::default().show(ui, |ui| {
                    ui.heading("Checking sources...");
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(24.0, 24.0), egui::Sense::hover());
                    egui::Spinner::new().size(24.0).paint_at(ui, rect);
                    ui.label("The strip below lists each source as it answers.");
                });
            }
            _ => {
                // close_on_finish is the old habit; the default keeps the window
                // on the end state, because the strip is the report.
                let partials = self
                    .progress
                    .lock()
                    .unwrap_or_else(|guard| guard.into_inner())
                    .partials
                    .clone();
                let phase = self
                    .progress
                    .lock()
                    .unwrap_or_else(|guard| guard.into_inner())
                    .phase;
                // A run that stopped with a file half downloaded asks about it
                // first, and a window that closes itself cannot ask: the question
                // outranks the config's habit, once, for as long as the pieces
                // are on disk.
                let close_on_finish = self.job.config.close_on_finish && partials.is_empty();
                if !close_on_finish {
                    egui::Panel::bottom("end_actions").show(ui, |ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // **OK** is the word for a run that is over, and it
                            // sits in the corner where a dismiss button belongs.
                            // It is the wider button, because it is the one every
                            // end state answers, and it is the focused one, so the
                            // run's last press can be Enter on the keyboard.
                            let ok = ui.add_sized(
                                egui::vec2(OK_WIDTH, OK_HEIGHT),
                                egui::Button::new(egui::RichText::new("OK").strong()),
                            );
                            if !self.end_focused {
                                ok.request_focus();
                                self.end_focused = true;
                            }
                            if ok.clicked() {
                                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            if !partials.is_empty() {
                                // The answers are buttons, not a box: the window
                                // is where this run's story is, and the list the
                                // question is about is drawn right above them.
                                if ui.button("Delete").clicked() {
                                    // Deleting the pieces takes the resume with
                                    // them and puts the window back at the rows.
                                    self.delete_partials(&partials);
                                }
                                if ui.button("Keep").clicked() {
                                    self.answer_partials(&partials, true);
                                }
                            }
                            if can_resume(phase, &self.chosen) && ui.button("Resume").clicked() {
                                // One press answers the question and starts the
                                // same rows again where they stopped.
                                self.resume();
                            }
                        });
                    });
                }

                egui::CentralPanel::default().show(ui, |ui| {
                    // Read once and cloned: the list is drawn by `file_rows`,
                    // which needs the window, and a guard held across that would
                    // keep the window borrowed.
                    let (heading, phase, message, answer, rows, done) = {
                        let progress = self
                            .progress
                            .lock()
                            .unwrap_or_else(|guard| guard.into_inner());
                        let heading = if progress.heading.is_empty() {
                            match progress.phase {
                                Phase::Done => "\u{2714} Done".to_string(),
                                Phase::Cancelled => "Cancelled".to_string(),
                                Phase::Failed => "Download failed".to_string(),
                                _ => "Saishin".to_string(),
                            }
                        } else {
                            progress.heading.clone()
                        };
                        (
                            heading,
                            progress.phase,
                            progress.message.clone(),
                            progress.answer.clone(),
                            progress.rows.clone(),
                            progress.done,
                        )
                    };
                    ui.heading(heading);
                    match phase {
                        Phase::Done => {
                            ui.label(format!("{done} file(s) updated."));
                        }
                        Phase::Cancelled => {
                            ui.label("Download cancelled.");
                        }
                        Phase::Failed => {
                            ui.label(message);
                        }
                        _ => {}
                    }
                    // The list the run worked through is the run's result, so it
                    // stays: the checks say what came down, and a row a cancel
                    // stopped is drawn standing still. A run that ended before
                    // any file started has no rows to show.
                    if !rows.is_empty() {
                        self.file_rows(ui, &rows, false);
                    }
                    if !partials.is_empty() {
                        ui.label(format!(
                            "{} file(s) are half downloaded, where they were being written:",
                            partials.len()
                        ));
                        for path in partials.iter() {
                            ui.label(format!("  {}  {}", path, partial_size(path)));
                        }
                        ui.label("Keeping them lets the next run continue where this one stopped. Deleting them leaves nothing behind.");
                    } else if !answer.is_empty() {
                        ui.label(answer);
                    }
                    if !close_on_finish {
                        ui.label("The strip below lists what this run did.");
                    }
                });

                if close_on_finish {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        // The editor's dialogs sit over whichever phase is showing, drawn last
        // so the modal covers the panels it borrows.
        self.editor_windows(ui);
    }
}

#[cfg(windows)]
fn centered_position(width: f32, height: f32) -> Option<[f32; 2]> {
    use std::ffi::c_void;
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::HiDpi::GetDpiForSystem;
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETWORKAREA,
    };
    let mut rect = RECT::default();
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rect as *mut RECT as *mut c_void),
            windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() {
        return None;
    }
    let dpi = unsafe { GetDpiForSystem() } as f32;
    let scale = if dpi > 0.0 { dpi / 96.0 } else { 1.0 };
    let left = rect.left as f32 / scale;
    let top = rect.top as f32 / scale;
    let work_w = (rect.right - rect.left) as f32 / scale;
    let work_h = (rect.bottom - rect.top) as f32 / scale;
    Some([left + (work_w - width) / 2.0, top + (work_h - height) / 2.0])
}

#[cfg(not(windows))]
fn centered_position(_width: f32, _height: f32) -> Option<[f32; 2]> {
    None
}

pub fn run_gui(
    sources: Vec<SourceConfig>,
    config: AppConfig,
    token: Option<String>,
    options: Options,
    config_path: String,
) -> Result<Vec<String>, AppError> {
    let window_centered = config.window_centered;
    // The file as the editor sees it. Read before the window opens: a file
    // that cannot be read at all is a start-up failure, told like any other.
    let model = config::load_model(&config_path)?;
    let job = Arc::new(GuiJob { sources, config, token, options });
    let result = Arc::new(GuiResult { updated: Mutex::new(Vec::new()) });
    let cancel = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Mutex::new(GuiProgress { phase: Phase::Loading, done: 0, total: 0, message: "".to_string(), heading: "".to_string(), rows: Vec::new(), partials: Vec::new(), answer: String::new() }));
    let load = Arc::new(Mutex::new(LoadState::Working));

    let mut native_options = eframe::NativeOptions::default();
    native_options.centered = false;
    native_options.persist_window = false;
    // The window icon is the logo, taken as the pixels the build script wrote:
    // no PNG decode at start-up.
    let app_icon = Arc::new(egui::IconData {
        rgba: LOGO_RGBA.to_vec(),
        width: LOGO_SIZE as u32,
        height: LOGO_SIZE as u32,
    });
    let startup_center = if window_centered {
        centered_position(WINDOW_WIDTH, WINDOW_HEIGHT)
    } else {
        None
    };
    let centered_at_startup = startup_center.is_some();
    let mut builder = egui::ViewportBuilder::default()
        .with_inner_size([WINDOW_WIDTH, WINDOW_HEIGHT])
        .with_min_inner_size([620.0, 380.0])
        .with_icon(app_icon)
        // The same identity `-V` answers: a screenshot, a bug report and a
        // taskbar row can all name the build they came from.
        .with_title(format!(
            "Saishin {} ({})",
            crate::BUILD_VERSION,
            crate::BUILD_GIT
        ));
    builder = match startup_center {
        Some([x, y]) => builder.with_position([x, y]).with_visible(false),
        None => builder.with_visible(!window_centered),
    };
    native_options.viewport = builder;
    let gui_result = Arc::clone(&result);
    let outcome = run_native(
        "Saishin",
        native_options,
        Box::new(move |_| Ok(Box::new(SelectionApp::new_loading(job, gui_result, cancel, progress, load, model, window_centered, centered_at_startup)))),
    );

    if let Err(ex) = outcome {
        return Err(AppError::Download {
            status: 0,
            message: format!("GUI failed: {ex}"),
        });
    }

    let updated = result.updated.lock().unwrap_or_else(|guard| guard.into_inner());
    Ok(updated.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_model() -> ConfigModel {
        ConfigModel { path: String::new(), header: String::new(), sources: Vec::new() }
    }

    #[test]
    fn adding_a_source_needs_the_fields_its_type_requires() {
        let model = empty_model();
        let mut form = SourceForm::new();
        assert!(form.apply(&model, None).unwrap_err().contains("owner and repo"));
        form.owner = "o".to_string();
        form.repo = "r".to_string();
        let source = form.apply(&model, None).expect("a github with owner and repo applies");
        assert_eq!(source.name, "o-r");
        assert_eq!(source.get("type"), Some("github"));
        // Defaults are written by silence: no key, not a key set to the default.
        assert_eq!(source.get("tag"), None);
        assert_eq!(source.get("private"), None);

        form.kind = SourceKind::Url;
        form.urls = "not a url".to_string();
        assert!(form.apply(&model, None).unwrap_err().contains("not a web address"));
        form.urls = "https://example.invalid/a.zip".to_string();
        let url_source = form.apply(&model, None).expect("a url applies");
        assert_eq!(url_source.get("urls"), Some("https://example.invalid/a.zip"));
        assert_eq!(url_source.name, "a");
    }

    #[test]
    fn counts_are_checked_before_the_file_is_written() {
        let model = empty_model();
        let mut form = SourceForm::new();
        form.kind = SourceKind::Url;
        form.urls = "https://example.invalid/a.zip".to_string();
        form.keep = "-1".to_string();
        assert!(form.apply(&model, None).unwrap_err().contains("'keep'"));
        form.keep = String::new();
        form.releases = "many".to_string();
        assert!(form.apply(&model, None).unwrap_err().contains("'releases'"));
    }

    #[test]
    fn an_edit_can_rename_and_renames_must_be_free() {
        let model = ConfigModel {
            path: String::new(),
            header: String::new(),
            sources: vec![
                EditableSource {
                    name: "demo".to_string(),
                    pairs: vec![
                        ("type".to_string(), "url".to_string()),
                        ("urls".to_string(), "https://example.invalid/a.zip".to_string()),
                        ("extract_as".to_string(), "*.exe -> app.exe".to_string()),
                    ],
                },
                EditableSource {
                    name: "taken".to_string(),
                    pairs: vec![
                        ("type".to_string(), "url".to_string()),
                        ("urls".to_string(), "https://example.invalid/b.zip".to_string()),
                    ],
                },
            ],
        };
        let mut form = SourceForm::from_source(model.find("demo").expect("section"));
        assert_eq!(form.urls, "https://example.invalid/a.zip");
        // The key the form has no field for rides through untouched.
        let kept = form.apply(&model, Some("demo")).expect("staying named is fine");
        assert_eq!(kept.name, "demo");
        assert_eq!(kept.get("extract_as"), Some("*.exe -> app.exe"));
        form.name = "taken".to_string();
        assert!(form.apply(&model, Some("demo")).unwrap_err().contains("already exists"));
        form.name = "fresh".to_string();
        assert_eq!(form.apply(&model, Some("demo")).expect("rename").name, "fresh");
    }

    #[test]
    fn the_form_reflects_what_a_section_says() {
        let source = EditableSource {
            name: "builds".to_string(),
            pairs: vec![
                ("type".to_string(), "actions".to_string()),
                ("owner".to_string(), "o".to_string()),
                ("repo".to_string(), "r".to_string()),
                ("workflow".to_string(), "CI".to_string()),
                ("runs".to_string(), "10".to_string()),
                ("branch".to_string(), "main".to_string()),
                ("only_successful_builds".to_string(), "false".to_string()),
                ("filters".to_string(), "*win* , *linux*".to_string()),
            ],
        };
        let form = SourceForm::from_source(&source);
        assert_eq!(form.kind, SourceKind::Actions);
        assert_eq!(form.branch, "main");
        assert!(!form.success_only, "the file says red builds are wanted");
        // The default count shows as empty, and writing it back keeps the
        // section saying the defaults by silence.
        assert_eq!(form.runs, "");
        assert_eq!(form.filters, "*win*, *linux*");
        let model = ConfigModel { path: String::new(), header: String::new(), sources: vec![source] };
        let out = form.apply(&model, Some("builds")).expect("applies");
        assert_eq!(out.get("runs"), None);
        assert_eq!(out.get("workflow"), Some("CI"));
        assert_eq!(out.get("branch"), Some("main"));
        assert_eq!(out.get("only_successful_builds"), Some("false"));
    }

    #[test]
    fn green_only_and_every_branch_say_themselves_by_silence() {
        let mut form = SourceForm::new();
        form.owner = "o".to_string();
        form.repo = "r".to_string();
        form.kind = SourceKind::Actions;
        assert!(form.success_only, "the new source starts on the safe setting");
        let model = empty_model();
        let out = form.apply(&model, None).expect("applies");
        // Defaults stay unwritten; branch empty means every branch, also unwritten.
        assert_eq!(out.get("only_successful_builds"), None);
        assert_eq!(out.get("branch"), None);
        form.success_only = false;
        form.branch = "develop".to_string();
        let out = form.apply(&model, None).expect("applies again");
        assert_eq!(out.get("only_successful_builds"), Some("false"));
        assert_eq!(out.get("branch"), Some("develop"));
    }

    /// A run that stopped offers to start again where it stopped, and only such a
    /// run: a finished run has nothing left to do, a run still going has its
    /// Cancel, and a window that never started a download has no files to
    /// continue.
    #[test]
    fn only_a_stopped_run_with_files_can_resume() {
        assert!(can_resume(Phase::Cancelled, &[0, 1]));
        // A transfer that broke leaves pieces too, and they are the same resume.
        assert!(can_resume(Phase::Failed, &[0]));
        assert!(!can_resume(Phase::Done, &[0, 1]));
        assert!(!can_resume(Phase::Downloading, &[0]));
        assert!(!can_resume(Phase::Selecting, &[0]));
        // Nothing picked, nothing to continue.
        assert!(!can_resume(Phase::Cancelled, &[]));
        assert!(!can_resume(Phase::Loading, &[]));
    }

    /// **Delete** removes the files a transfer continues from, so it takes the
    /// offer along with them: the window is back at the rows, their boxes as
    /// the last run left them, with nothing left to continue. Both halves of
    /// the gate close on that - the phase is no longer a stopped run's, and the
    /// pick those rows started is gone.
    #[test]
    fn deleting_the_pieces_ends_the_offer() {
        // The state **Delete** leaves: the rows, and no pick.
        assert!(!can_resume(Phase::Selecting, &[]));
        // The stopped run it came from, with the pick taken away: the same
        // answer, whatever the phase says.
        assert!(!can_resume(Phase::Cancelled, &[]));
        // The case the two clears above exist to break: a stopped run that
        // still has its rows and its pieces keeps the offer.
        assert!(can_resume(Phase::Cancelled, &[0, 3]));
    }

    /// The strip's one line is the newest that can be read whole in the width the
    /// strip has, so a run's long path lines do not leave the strip showing half a
    /// path - and the same line a narrow window passes over is readable in a wide
    /// one, because the count comes from the window's width.
    #[test]
    fn the_strips_line_is_the_newest_that_fits() {
        let line = |text: &str| log::Line {
            level: Level::Info,
            text: text.to_string(),
        };
        let short = line("archive: 658 extracted, 0 skipped");
        let long = line(&format!("C:\\vpin\\{}", "a-long-folder-name\\".repeat(9)));
        let room = strip_chars(940.0);
        assert!(long.text.chars().count() > room);
        assert!(short.text.chars().count() <= room);

        // The newest that fits, not the newest that is there.
        let lines = vec![short.clone(), long.clone(), short.clone()];
        assert_eq!(strip_line(&lines, room).unwrap().text, short.text);

        // The same two lines in a window twice as wide: the long one is the
        // newest, it fits, and it is what the strip shows. This is the reason the
        // count comes from the width and is not a constant.
        let two = vec![short.clone(), long.clone()];
        assert_eq!(strip_line(&two, room).unwrap().text, short.text);
        let wide = strip_chars(1880.0);
        assert!(long.text.chars().count() <= wide);
        assert_eq!(strip_line(&two, wide).unwrap().text, long.text);

        // A run whose every line is long still gets its newest line, cut to what
        // the strip has room for, with the whole of it on hover: showing nothing
        // is the worse choice.
        assert_eq!(strip_line(std::slice::from_ref(&long), 10).unwrap().text, long.text);
        assert!(strip_line(&[], 10).is_none());
    }

    /// A row's name keeps its ends: the head is what a source's files share and
    /// the tail is the version and the ending, and the middle is what a row that
    /// has to stay one line tall has no room for.
    #[test]
    fn a_rows_name_keeps_its_ends() {
        assert_eq!(short_name("vpinball_10.2.zip"), "vpinball_10.2.zip");
        let name = "VisualPinball_x64_10.2_GitHub_b1b8574f9c2c4b0a9d.zip";
        let shown = short_name(name);
        assert!(shown.starts_with("VisualPinball_x64_"));
        assert!(shown.ends_with("9d.zip"));
        assert!(shown.contains('\u{2026}'));
        assert!(shown.chars().count() < name.chars().count());
    }

    #[test]
    fn a_rows_mark_is_the_whole_of_its_telling() {
        // The row carries no word for its state, so the marks have to read apart
        // on their own: an amber square for an archive, a green check for a file
        // that is finished, and neither is the other's color.
        assert_ne!(UNZIP_MARK, DONE_MARK);
        let (r, g, b, _) = UNZIP_MARK.to_tuple();
        assert!(r > 200 && g > 120 && b < 100, "the unzip mark has to read amber, not {r},{g},{b}");
        let (r, g, b, _) = DONE_MARK.to_tuple();
        assert!(g > r && g > b, "the done mark has to read green, not {r},{g},{b}");
        // The mark of a row a cancel left mid-flight is grey on purpose: no hue
        // to read as busy, and no motion to read as a run that is still up.
        let (r, g, b, _) = STOPPED_MARK.to_tuple();
        assert_eq!(r, g);
        assert_eq!(g, b);
        assert_ne!(STOPPED_MARK, DONE_MARK);
        assert_ne!(STOPPED_MARK, UNZIP_MARK);
    }

    /// `build.rs` picks the embedded logo's size from the PNG, and this file
    /// draws a logo of a size it assumes. The two have to agree.
    #[test]
    fn the_embedded_logo_is_the_size_the_build_script_wrote() {
        let built: usize = env!("SAISHIN_LOGO_SIZE").parse().expect("build.rs reports the logo size");
        assert_eq!(built, LOGO_SIZE);
        assert_eq!(LOGO_RGBA.len(), LOGO_SIZE * LOGO_SIZE * 4);
    }

    /// The logo is drawn over the header, so it needs an alpha channel with
    /// both empty and filled pixels in it.
    #[test]
    fn the_embedded_logo_keeps_its_transparency() {
        let mut alpha = LOGO_RGBA.iter().skip(3).step_by(4);
        assert!(alpha.clone().any(|&a| a == 0), "the logo has no transparent pixels");
        assert!(alpha.any(|&a| a == 255), "the logo has no opaque pixels");
    }

    /// The header row, the text size and the width of the window are all set
    /// for a two-glyph word.
    #[test]
    fn the_header_word_is_the_apps_name() {
        assert_eq!(HEADER_WORD.chars().count(), 2);
    }
}
