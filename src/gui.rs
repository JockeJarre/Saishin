use crate::downloader::format_bytes;
use crate::error::AppError;
use crate::PendingFile;
use crate::Options;
use crate::config::AppConfig;
use crate::config::SourceConfig;
use crate::downloader::Downloader;
use crate::prune_old_files;
use crate::download_pending;
use crate::ProgressSink;
use eframe::App;
use eframe::egui;
use eframe::run_native;
use std::fs;
use std::path::PathBuf;
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
    Selecting,
    Downloading,
    Done,
    Cancelled,
    Failed,
}

#[derive(Clone, Copy)]
enum RowState {
    Pending,
    Downloading,
    Done,
}

struct GuiProgress {
    phase: Phase,
    done: usize,
    total: usize,
    message: String,
    rows: Vec<RowState>,
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
    /// The header logo, uploaded to the GPU on the first frame.
    logo: Option<egui::TextureHandle>,
    /// Whether a font carrying the glyphs of 最新 was found, which is what
    /// decides if the header can spell the name out.
    kanji: bool,
    shown: bool,
    centered_at_startup: bool,
    window_centered: bool,
}

/// The artifact list is five columns wide and the file names carry the
/// version, the build number and the commit, so it needs room to be readable.
const WINDOW_WIDTH: f32 = 940.0;
const WINDOW_HEIGHT: f32 = 640.0;

/// The header logo as raw RGBA8 pixels, decoded and shrunk from the PNG by
/// `build.rs`. Pixels and not PNG bytes on purpose: the embedded bytes are
/// uploaded to the texture as they are, so showing the logo does not need an
/// image decoder at run time.
const LOGO_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/saishin-logo.rgba"));
/// Pixels on a side of the embedded logo: a quarter of the 1024 px PNG, which
/// is what a 120 pt logo takes on a 2x display. `build.rs` reports the size it
/// wrote, and the test at the bottom of this file keeps the two in step.
const LOGO_SIZE: usize = 256;
const _: () = assert!(LOGO_RGBA.len() == LOGO_SIZE * LOGO_SIZE * 4);

/// The header spells 最新 out as text, and egui's built-in font carries no CJK
/// glyphs, so a font from the machine joins the font stack as a fallback.
/// Every platform this app runs on ships one that has the two glyphs.
///
/// Returns whether a font was installed: the header only draws the word when
/// it can be drawn properly, and shows boxes on a machine with no CJK font.
fn install_cjk_font(ctx: &egui::Context) -> bool {
    let Some(path) = cjk_font_file() else { return false };
    let Ok(bytes) = fs::read(&path) else { return false };

    ctx.add_font(egui::FontInsert {
        name: "saishin-cjk".to_owned(),
        data: egui::FontData::from_owned(bytes),
        families: vec![
            egui::InsertFontFamily { family: egui::FontFamily::Proportional, priority: egui::FontPriority::Lowest },
            egui::InsertFontFamily { family: egui::FontFamily::Monospace, priority: egui::FontPriority::Lowest },
        ],
    });
    true
}

/// The first font file on this machine that carries CJK glyphs.
fn cjk_font_file() -> Option<PathBuf> {
    cjk_font_candidates().into_iter().find(|path| path.is_file())
}

/// The font files to try, per platform. The `.ttc` collections are read at
/// face 0, which is the plain face of each of these.
fn cjk_font_candidates() -> Vec<PathBuf> {
    if cfg!(windows) {
        vec![
            // Microsoft YaHei, shipped with every Windows since Windows 7.
            PathBuf::from(r"C:\Windows\Fonts\msyh.ttc"),
            // SimHei, and the Japanese faces, both carry the two glyphs too.
            PathBuf::from(r"C:\Windows\Fonts\simhei.ttf"),
            PathBuf::from(r"C:\Windows\Fonts\YuGothR.ttc"),
        ]
    } else if cfg!(target_os = "macos") {
        vec![
            PathBuf::from("/System/Library/Fonts/PingFang.ttc"),
            PathBuf::from("/System/Library/Fonts/Supplemental/Hiragino Sans GB.ttc"),
        ]
    } else {
        vec![
            // The usual places of `fonts-noto-cjk` and its equivalents: Debian
            // and Ubuntu, Fedora, Arch.
            PathBuf::from("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc"),
            PathBuf::from("/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc"),
            PathBuf::from("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc"),
            PathBuf::from("/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc"),
        ]
    }
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

struct GuiSink {
    progress: Arc<Mutex<GuiProgress>>,
    index: HashMap<(String, String), usize>,
}

impl ProgressSink for GuiSink {
    fn report(&self, file: &PendingFile, started: bool) {
        let key = (file.source_name.clone(), file.file_name.clone());
        if let Some(index) = self.index.get(&key).copied() {
            let mut progress = self.progress.lock().unwrap_or_else(|guard| guard.into_inner());
            if index < progress.rows.len() {
                progress.rows[index] = if started { RowState::Downloading } else { RowState::Done };
                if !started {
                    progress.done += 1;
                }
            }
        }
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
        kanji: bool,
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
            logo: None,
            kanji,
            shown: false,
            centered_at_startup,
            window_centered,
        }
    }

    fn start_download(&mut self, chosen: Vec<usize>) {
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
                                    return;
                                }
                            }
                            if cancel_ref.load(Ordering::Relaxed) {
                                let mut seen = progress_arc.lock().unwrap_or_else(|guard| guard.into_inner());
                                seen.phase = Phase::Cancelled;
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
        let phase = self.progress.lock().unwrap_or_else(|guard| guard.into_inner()).phase;

        egui::Panel::top("header_logo").exact_size(128.0).show_separator_line(false).show(ui, |ui| {
            ui.centered_and_justified(|ui| {
                if let Some(logo) = &self.logo {
                    ui.add(egui::Image::from_texture(logo).max_width(120.0));
                }
            });
        });

        if self.kanji {
            egui::Panel::top("header_kanji").exact_size(60.0).show_separator_line(false).show(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("最新").size(40.0).strong());
                });
            });
        }

        match phase {
            Phase::Selecting => {
                egui::Panel::bottom("actions").show(ui, |ui| {
                    ui.horizontal(|ui| {
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
                        ui.label("No new artifacts.");
                        return;
                    }

                    ui.label(format!("{} new artifact(s)", self.entries.len()));
                    ui.label("Pick an older version in the version column to download that instead.");

                    // Picking a version changes rows, so the picks are
                    // collected while drawing and applied once the list is
                    // rendered.
                    let mut picks: Vec<(usize, usize)> = Vec::new();

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
                                        ui.checkbox(&mut entry.selected, "");
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
                        });

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
                    let progress = self.progress.lock().unwrap_or_else(|guard| guard.into_inner());
                    let rows = progress.rows.clone();
                    ui.heading("Downloading...");
                    ui.label(format!("{} of {} files", progress.done, progress.total));

                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .scroll_bar_visibility(egui::containers::scroll_area::ScrollBarVisibility::AlwaysVisible)
                        .show(ui, |ui| {
                            egui::Grid::new("download_list")
                                .spacing(egui::vec2(8.0, 4.0))
                                .show(ui, |ui| {
                                    for (index, entry) in self.entries.iter().enumerate() {
                                        let state = if index < rows.len() { rows[index] } else { RowState::Pending };
                                        let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                                        match state {
                                            RowState::Downloading => egui::Spinner::new().size(16.0).paint_at(ui, rect),
                                            RowState::Done => {
                                                let stroke = egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(60, 180, 90));
                                                ui.painter().line_segment([egui::pos2(rect.left() + 3.0, rect.center().y), egui::pos2(rect.center().x, rect.bottom() - 4.0)], stroke);
                                                ui.painter().line_segment([egui::pos2(rect.center().x, rect.bottom() - 4.0), egui::pos2(rect.right() - 3.0, rect.top() + 3.0)], stroke);
                                            }
                                            RowState::Pending => {}
                                        }
                                        ui.label(entry.file.source_name.clone());
                                        let size = match entry.file.size {
                                            Some(bytes) => format_bytes(bytes),
                                            None => "size unknown".to_string(),
                                        };
                                        ui.label(size);
                                        ui.hyperlink_to(&entry.file.file_name, &entry.file.link_url);
                                        ui.end_row();
                                    }
                                });
                        });
                });
            }
            _ => {
                egui::CentralPanel::default().show(ui, |ui| {
                    let progress = self.progress.lock().unwrap_or_else(|guard| guard.into_inner());
                    match progress.phase {
                        Phase::Done => {
                            ui.heading("\u{2714} Done");
                            ui.label(format!("{} file(s) updated.", progress.done));
                        }
                        Phase::Cancelled => {
                            ui.heading("Cancelled");
                            ui.label("Download cancelled.");
                        }
                        Phase::Failed => {
                            ui.heading("Download failed");
                            ui.label(progress.message.clone());
                        }
                        _ => {}
                    }
                });
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
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
    entries: Vec<PendingFile>,
    version_map: HashMap<(String, String), Vec<PendingFile>>,
    sources: Vec<SourceConfig>,
    config: AppConfig,
    token: Option<String>,
    options: Options,
) -> Result<Vec<String>, AppError> {
    let window_centered = config.window_centered;
    let job = Arc::new(GuiJob { sources, config, token, options });
    let result = Arc::new(GuiResult { updated: Mutex::new(Vec::new()) });
    let cancel = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Mutex::new(GuiProgress { phase: Phase::Selecting, done: 0, total: 0, message: "".to_string(), rows: Vec::new() }));

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
        .with_icon(app_icon);
    builder = match startup_center {
        Some([x, y]) => builder.with_position([x, y]).with_visible(false),
        None => builder.with_visible(!window_centered),
    };
    native_options.viewport = builder;
    let gui_result = Arc::clone(&result);
    let outcome = run_native(
        "Saishin",
        native_options,
        Box::new(move |cc: &eframe::CreationContext| {
            // The header word is set in a font this app does not ship, so the
            // machine's own CJK font joins the font stack here, before egui
            // lays out the first frame.
            let kanji = install_cjk_font(&cc.egui_ctx);
            Ok(Box::new(SelectionApp::new(entries, version_map, job, gui_result, cancel, progress, kanji, window_centered, centered_at_startup)))
        }),
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

    /// The header word is set in a font the app does not ship, so every
    /// platform needs its list of the fonts that ship with it.
    #[test]
    fn a_cjk_font_is_listed_for_every_platform() {
        assert!(!cjk_font_candidates().is_empty());
    }
}
