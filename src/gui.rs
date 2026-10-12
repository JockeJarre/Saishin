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
use egui_extras::install_image_loaders;
use std::thread;
use tokio::runtime::Builder;
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::collections::HashMap;

struct GuiJob {
    sources: Vec<SourceConfig>,
    entries: Vec<PendingFile>,
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
}

struct SelectionApp {
    entries: Vec<ArtifactEntry>,
    job: Arc<GuiJob>,
    result: Arc<GuiResult>,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<GuiProgress>>,
    centered: bool,
    window_centered: bool,
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

impl SelectionApp {
    fn new(entries: Vec<PendingFile>, job: Arc<GuiJob>, result: Arc<GuiResult>, cancel: Arc<AtomicBool>, progress: Arc<Mutex<GuiProgress>>, window_centered: bool) -> SelectionApp {
        SelectionApp {
            entries: entries
                .into_iter()
                .map(|file| ArtifactEntry { file, selected: true })
                .collect(),
            job,
            result,
            cancel,
            progress,
            centered: false,
            window_centered,
        }
    }

    fn start_download(&mut self, chosen: Vec<usize>) {
        let mut progress = self.progress.lock().unwrap_or_else(|guard| guard.into_inner());
        progress.phase = Phase::Downloading;
        progress.done = 0;
        progress.total = chosen.len();
        progress.message = "".to_string();
        progress.rows = self.entries
            .iter()
            .map(|_| RowState::Pending)
            .collect();

        let job = Arc::clone(&self.job);
        let cancel = Arc::clone(&self.cancel);
        let progress_arc = Arc::clone(&self.progress);
        let result = Arc::clone(&self.result);

        let index: HashMap<(String, String), usize> = job.entries
            .iter()
            .enumerate()
            .map(|(i, file)| ((file.source_name.clone(), file.file_name.clone()), i))
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
                            let files: Vec<PendingFile> = job.entries
                                .iter()
                                .enumerate()
                                .filter(|(index, file)| chosen.contains(index) && file.source_name == source.name)
                                .map(|(_, file)| file.clone())
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
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        install_image_loaders(ctx);

        if self.window_centered && !self.centered {
            let (monitor, outer) = ctx.input(|i| {
                let v = i.viewport();
                (v.monitor_size, v.outer_rect)
            });
            if let (Some(m), Some(o)) = (monitor, outer) {
                let size = o.size();
                let pos = egui::pos2(m.x / 2.0 - size.x / 2.0, m.y / 2.0 - size.y / 2.0);
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                self.centered = true;
            }
        }

        let phase = self.progress.lock().unwrap_or_else(|guard| guard.into_inner()).phase;

        egui::TopBottomPanel::top("header_logo").exact_height(128.0).show_separator_line(false).show(ctx, |ui| {
            ui.centered_and_justified(|ui| {
                ui.add(
                    egui::Image::new(egui::include_image!("../saishin-png-icon-set/saishin-master-1024.png"))
                        .max_width(120.0),
                );
            });
        });

        egui::TopBottomPanel::top("header_kanji").exact_height(60.0).show_separator_line(false).show(ctx, |ui| {
            ui.centered_and_justified(|ui| {
                ui.add(
                    egui::Image::new(egui::include_image!("../saishin-png-icon-set/saishin-kanji-strip-inverted.png"))
                        .max_width(100.0),
                );
            });
        });

        match phase {
            Phase::Selecting => {
                egui::TopBottomPanel::bottom("actions").show(ctx, |ui| {
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
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                });

                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.heading("Saishin - new artifacts");

                    if self.entries.is_empty() {
                        ui.label("No new artifacts.");
                        return;
                    }

                    ui.label(format!("{} new artifact(s)", self.entries.len()));

                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .scroll_bar_visibility(egui::containers::scroll_area::ScrollBarVisibility::AlwaysVisible)
                        .show(ui, |ui| {
                            egui::Grid::new("artifact_list")
                                .spacing(egui::vec2(8.0, 4.0))
                                .show(ui, |ui| {
                                    for (_index, entry) in self.entries.iter_mut().enumerate() {
                                        ui.checkbox(&mut entry.selected, "");
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
            Phase::Downloading => {
                egui::TopBottomPanel::bottom("actions").show(ctx, |ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Cancel").clicked() {
                            self.cancel.store(true, Ordering::Relaxed);
                        }
                    });
                });

                egui::CentralPanel::default().show(ctx, |ui| {
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
                egui::CentralPanel::default().show(ctx, |ui| {
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
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

pub fn run_gui(
    entries: Vec<PendingFile>,
    sources: Vec<SourceConfig>,
    config: AppConfig,
    token: Option<String>,
    options: Options,
) -> Result<Vec<String>, AppError> {
    let window_centered = config.window_centered;
    let job = Arc::new(GuiJob { sources, entries: entries.clone(), config, token, options });
    let result = Arc::new(GuiResult { updated: Mutex::new(Vec::new()) });
    let cancel = Arc::new(AtomicBool::new(false));
    let progress = Arc::new(Mutex::new(GuiProgress { phase: Phase::Selecting, done: 0, total: 0, message: "".to_string(), rows: Vec::new() }));

    let mut native_options = eframe::NativeOptions::default();
    let app_icon = match eframe::icon_data::from_png_bytes(
        include_bytes!("../saishin-png-icon-set/saishin-master-1024.png")
    ) {
        Ok(icon) => Arc::new(icon),
        Err(_) => Arc::new(egui::IconData::default()),
    };
    native_options.viewport = egui::ViewportBuilder::default()
        .with_inner_size([520.0, 420.0])
        .with_min_inner_size([520.0, 420.0])
        .with_icon(app_icon);
    let gui_result = Arc::clone(&result);
    let outcome = run_native(
        "Saishin",
        native_options,
        Box::new(move |_| Box::new(SelectionApp::new(entries, job, gui_result, cancel, progress, window_centered))),
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
