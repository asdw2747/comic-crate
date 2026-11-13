//! egui user interface.

use crate::reader::Reader;
use crate::worker::{self, Command, Event};
use comic_crate::model::{
    is_converter_input_path, is_reader_path, output_path_for, plan_output_paths, ConvertOptions,
    ConvertOutcome, ImageOutputMode,
};
use eframe::egui;
use egui::{
    Align, Align2, Button, Color32, CornerRadius, FontId, Frame, Label, Layout, Margin,
    ProgressBar, RichText, ScrollArea, Stroke, Ui, Vec2,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const BG: Color32 = Color32::from_rgb(14, 15, 19);
const PANEL: Color32 = Color32::from_rgb(21, 22, 28);
const CARD: Color32 = Color32::from_rgb(28, 30, 38);
const CARD_ACTIVE: Color32 = Color32::from_rgb(33, 36, 46);
const MUTED: Color32 = Color32::from_rgb(148, 152, 166);
const ACCENT: Color32 = Color32::from_rgb(96, 160, 255);
const GREEN: Color32 = Color32::from_rgb(92, 200, 132);
const ORANGE: Color32 = Color32::from_rgb(240, 172, 84);
const RED: Color32 = Color32::from_rgb(240, 110, 110);
const YELLOW: Color32 = Color32::from_rgb(226, 200, 96);
const APP_ICON_BYTES: &[u8] = include_bytes!("../assets/icon.png");

#[derive(Clone, Copy, PartialEq, Eq)]
enum JobStatus {
    Probing,
    Queued,
    Processing,
    Done,
    Skipped,
    Error,
    Cancelled,
}

impl JobStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Probing => "Reading...",
            Self::Queued => "Queued",
            Self::Processing => "Processing...",
            Self::Done => "Done",
            Self::Skipped => "Skipped",
            Self::Error => "Error",
            Self::Cancelled => "Stopped",
        }
    }

    fn color(self) -> Color32 {
        match self {
            Self::Probing => MUTED,
            Self::Queued => ACCENT,
            Self::Processing => ORANGE,
            Self::Done => GREEN,
            Self::Skipped => YELLOW,
            Self::Error => RED,
            Self::Cancelled => MUTED,
        }
    }
}

struct Job {
    id: u64,
    path: PathBuf,
    pages: Option<usize>,
    status: JobStatus,
    output: Option<PathBuf>,
    message: Option<String>,
    warnings: Vec<String>,
    progress_done: usize,
    progress_total: usize,
    source_bytes: u64,
    bytes_written: u64,
    elapsed_ms: u128,
}

#[derive(Default)]
struct BatchState {
    total: usize,
    done: usize,
    succeeded: usize,
    failed: usize,
    cancelled: bool,
    current_id: Option<u64>,
    current_name: String,
    current_done: usize,
    current_total: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Converter,
    Reader,
}

pub struct App {
    tab: Tab,
    jobs: Vec<Job>,
    next_id: u64,
    worker: worker::Worker,
    reader: Reader,
    options: ConvertOptions,
    running: bool,
    cancel: Option<Arc<AtomicBool>>,
    batch: BatchState,
    status_line: String,
    drop_hover: bool,
    about_open: bool,
    about_icon: Option<egui::TextureHandle>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_style(&cc.egui_ctx);
        Self {
            tab: Tab::Converter,
            jobs: Vec::new(),
            next_id: 1,
            worker: worker::spawn(),
            reader: Reader::new(),
            options: ConvertOptions::default(),
            running: false,
            cancel: None,
            batch: BatchState::default(),
            status_line: "Drop PDF or CBR comics, or a folder, anywhere in this window.".to_owned(),
            drop_hover: false,
            about_open: false,
            about_icon: None,
        }
    }

    fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        if self.running {
            self.status_line = "Wait for the current batch to finish before adding files.".into();
            return;
        }

        let mut found = Vec::new();
        for path in paths {
            collect_pdfs(&path, &mut found, 0);
        }
        found.sort();
        found.dedup();

        let existing: std::collections::HashSet<String> =
            self.jobs.iter().map(|job| path_key(&job.path)).collect();
        let mut added = 0usize;
        for path in found {
            let key = path_key(&path);
            if existing.contains(&key) || self.jobs.iter().any(|job| path_key(&job.path) == key) {
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            let _ = self.worker.tx.send(Command::Add {
                id,
                path: path.clone(),
            });
            self.jobs.push(Job {
                id,
                path,
                pages: None,
                status: JobStatus::Probing,
                output: None,
                message: None,
                warnings: Vec::new(),
                progress_done: 0,
                progress_total: 0,
                source_bytes: 0,
                bytes_written: 0,
                elapsed_ms: 0,
            });
            added += 1;
        }

        self.status_line = match added {
            0 => "No new PDF/CBR files found in the dropped items.".to_owned(),
            1 => "Added 1 comic.".to_owned(),
            n => format!("Added {n} comics."),
        };
    }

    fn select_files(&mut self) {
        if let Some(files) = rfd::FileDialog::new()
            .add_filter("PDF / CBR comics", &["pdf", "cbr"])
            .pick_files()
        {
            self.add_paths(files);
        }
    }

    fn select_folder(&mut self) {
        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
            self.add_paths([folder]);
        }
    }

    fn choose_output_dir(&mut self) {
        let mut dialog = rfd::FileDialog::new();
        if let Some(current) = &self.options.output_dir {
            dialog = dialog.set_directory(current);
        }
        if let Some(folder) = dialog.pick_folder() {
            self.options.output_dir = Some(folder);
        }
    }

    fn remove_job(&mut self, id: u64) {
        if self.running {
            return;
        }
        self.jobs.retain(|job| job.id != id);
        let _ = self.worker.tx.send(Command::Remove(id));
    }

    fn clear_queue(&mut self) {
        if self.running {
            return;
        }
        self.jobs.clear();
        self.batch = BatchState::default();
        self.status_line = "Queue cleared.".to_owned();
        let _ = self.worker.tx.send(Command::Clear);
    }

    fn start_batch(&mut self) {
        if self.running {
            return;
        }
        let overwrite = self.options.overwrite;
        let ids: Vec<u64> = self
            .jobs
            .iter()
            .filter(|job| {
                matches!(
                    job.status,
                    JobStatus::Queued | JobStatus::Error | JobStatus::Cancelled
                ) || (overwrite && job.status == JobStatus::Skipped)
            })
            .map(|job| job.id)
            .collect();

        if ids.is_empty() {
            self.status_line =
                "Add a PDF or CBR first (or clear finished entries and retry).".to_owned();
            return;
        }

        for job in &mut self.jobs {
            if ids.contains(&job.id) {
                job.status = JobStatus::Queued;
                job.message = None;
                job.warnings.clear();
                job.output = None;
                job.progress_done = 0;
                job.progress_total = 0;
                job.source_bytes = 0;
                job.bytes_written = 0;
                job.elapsed_ms = 0;
            }
        }

        let planned = self.planned_outputs();
        let outputs: Vec<(u64, PathBuf)> = self
            .jobs
            .iter()
            .zip(planned)
            .filter(|(job, _)| ids.contains(&job.id))
            .map(|(job, output)| (job.id, output))
            .collect();

        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(Arc::clone(&cancel));
        self.batch = BatchState {
            total: ids.len(),
            ..BatchState::default()
        };
        self.running = true;
        self.status_line = format!("Converting {} file(s)...", ids.len());

        let _ = self.worker.tx.send(Command::Start {
            ids,
            options: self.options.clone(),
            outputs,
            cancel,
        });
    }

    fn planned_outputs(&self) -> Vec<PathBuf> {
        let inputs: Vec<PathBuf> = self.jobs.iter().map(|job| job.path.clone()).collect();
        plan_output_paths(&inputs, self.options.output_dir.as_deref())
    }

    fn stop_batch(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
            self.status_line = "Stopping after the in-flight pages...".to_owned();
        }
    }

    fn poll_worker(&mut self) {
        loop {
            match self.worker.rx.try_recv() {
                Ok(event) => self.handle_event(event),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.running = false;
                    self.cancel = None;
                    break;
                }
            }
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Probed { id, pages, error } => {
                if let Some(job) = self.job_mut(id) {
                    if let Some(error) = error {
                        job.status = JobStatus::Error;
                        job.message = Some(error);
                    } else {
                        job.status = JobStatus::Queued;
                        job.pages = pages;
                    }
                }
            }
            Event::Started { id } => {
                let info = self.job_mut(id).map(|job| {
                    job.status = JobStatus::Processing;
                    job.progress_done = 0;
                    job.progress_total = job.pages.unwrap_or(0);
                    let total = job.progress_total;
                    let name = job
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    (total, name)
                });
                let Some((current_total, current_name)) = info else {
                    return;
                };
                self.batch.current_id = Some(id);
                self.batch.current_name = current_name;
                self.batch.current_done = 0;
                self.batch.current_total = current_total;
            }
            Event::Progress { id, done, total } => {
                if let Some(job) = self.job_mut(id) {
                    job.progress_done = job.progress_done.max(done);
                    job.progress_total = total;
                    job.pages = Some(total);
                }
                if self.batch.current_id == Some(id) {
                    self.batch.current_done = self.batch.current_done.max(done);
                    self.batch.current_total = total;
                }
            }
            Event::Finished { id, outcome } => {
                if let Some(job) = self.job_mut(id) {
                    apply_outcome(job, &outcome);
                }
                self.batch.done += 1;
                self.batch.succeeded += 1;
                self.batch.current_id = None;
            }
            Event::Failed { id, error } => {
                if let Some(job) = self.job_mut(id) {
                    job.status = JobStatus::Error;
                    job.message = Some(error);
                    job.progress_done = 0;
                    job.progress_total = 0;
                }
                self.batch.done += 1;
                self.batch.failed += 1;
                self.batch.current_id = None;
            }
            Event::Cancelled { id } => {
                if let Some(job) = self.job_mut(id) {
                    job.status = JobStatus::Cancelled;
                    job.message = Some("Stopped by user.".to_owned());
                    job.progress_done = 0;
                    job.progress_total = 0;
                }
                self.batch.cancelled = true;
                self.batch.current_id = None;
            }
            Event::BatchDone {
                processed,
                succeeded,
                failed,
                cancelled,
            } => {
                self.running = false;
                self.cancel = None;
                self.batch.done = processed;
                self.batch.succeeded = succeeded;
                self.batch.failed = failed;
                self.batch.cancelled = cancelled;
                self.batch.current_id = None;
                self.status_line = if cancelled {
                    format!("Stopped after {processed} of {} file(s).", self.batch.total)
                } else if failed > 0 {
                    format!("Finished with errors: {succeeded} succeeded, {failed} failed.")
                } else {
                    format!("Finished {succeeded} file(s).")
                };
            }
        }
    }

    fn job_mut(&mut self, id: u64) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|job| job.id == id)
    }

    fn eligible_count(&self) -> usize {
        let overwrite = self.options.overwrite;
        self.jobs
            .iter()
            .filter(|job| {
                matches!(
                    job.status,
                    JobStatus::Queued | JobStatus::Error | JobStatus::Cancelled
                ) || (overwrite && job.status == JobStatus::Skipped)
            })
            .count()
    }

    fn handle_drop(&mut self, ctx: &egui::Context) {
        let (dropped, hovering) = ctx.input(|input| {
            (
                input.raw.dropped_files.clone(),
                !input.raw.hovered_files.is_empty(),
            )
        });
        self.drop_hover = hovering && (self.tab == Tab::Reader || !self.running);

        if dropped.is_empty() {
            return;
        }

        let paths: Vec<PathBuf> = dropped
            .into_iter()
            .map(|file| file.path().to_path_buf())
            .collect();
        if paths.is_empty() {
            self.status_line = "That drop did not contain a usable file path.".to_owned();
            return;
        }

        if self.tab == Tab::Reader {
            if let Some(path) = paths.iter().find(|path| is_reader_path(path)) {
                self.reader.open_path(path.clone(), ctx);
            } else {
                self.status_line = "Reader supports CBZ, CBR and PDF files.".to_owned();
            }
            return;
        }

        let converter_inputs: Vec<PathBuf> = paths
            .iter()
            .filter(|path| is_converter_input_path(path))
            .cloned()
            .collect();

        if self.running {
            if converter_inputs.is_empty() {
                if let Some(cbz) = paths.iter().find(|path| {
                    path.extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("cbz"))
                }) {
                    self.open_in_reader(cbz.clone(), ctx);
                } else {
                    self.status_line =
                        "Wait for the current batch to finish before adding files.".to_owned();
                }
            } else {
                self.status_line =
                    "Wait for the current batch to finish before adding files.".to_owned();
            }
            return;
        }

        if !converter_inputs.is_empty() {
            self.add_paths(converter_inputs);
            return;
        }

        if let Some(cbz) = paths.iter().find(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("cbz"))
        }) {
            self.open_in_reader(cbz.clone(), ctx);
        } else {
            self.status_line = "Drop PDF/CBR comics or a folder here.".to_owned();
        }
    }

    fn set_tab(&mut self, tab: Tab, ctx: &egui::Context) {
        if self.tab == tab {
            return;
        }
        if tab == Tab::Converter {
            self.reader.exit_fullscreen(ctx);
        }
        self.tab = tab;
        ctx.request_repaint();
    }

    fn open_in_reader(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.tab = Tab::Reader;
        self.reader.open_path(path, ctx);
    }

    fn about_icon(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        if self.about_icon.is_none() {
            let image = image::load_from_memory(APP_ICON_BYTES).ok()?;
            let image = image
                .resize_exact(128, 128, image::imageops::FilterType::Lanczos3)
                .to_rgba8();
            let rgba = image.into_raw();
            let color = egui::ColorImage::from_rgba_unmultiplied([128, 128], &rgba);
            self.about_icon = Some(ctx.load_texture(
                "comiccrate-about-icon",
                color,
                egui::TextureOptions::LINEAR,
            ));
        }
        self.about_icon.clone()
    }

    fn draw_about(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let icon = self.about_icon(ctx);
        let mut close = false;
        let response =
            egui::Modal::new(egui::Id::unique("comiccrate-about-modal")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.vertical_centered(|ui| {
                    if let Some(icon) = &icon {
                        ui.image((icon.id(), egui::vec2(96.0, 96.0)));
                        ui.add_space(8.0);
                    }
                    ui.label(
                        RichText::new(format!("ComicCrate v{}", env!("CARGO_PKG_VERSION")))
                            .size(24.0)
                            .strong(),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "A lightweight, parallel comic archive converter \
                             (PDF/CBR to CBZ) and reader.",
                        )
                        .small()
                        .color(MUTED),
                    );
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new("Supported formats: .pdf \u{00b7} .cbr \u{00b7} .cbz")
                            .small(),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Licensed under the MIT License")
                            .small()
                            .color(MUTED),
                    );
                    ui.add_space(14.0);
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            });

        if close || response.backdrop_response.clicked() {
            self.about_open = false;
        }
    }

    fn draw_header(&mut self, root: &mut Ui) {
        let ctx = root.ctx().clone();
        egui::Panel::top("header")
            .frame(
                Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(18, 11)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("ComicCrate")
                                .size(23.0)
                                .strong()
                                .color(Color32::from_rgb(238, 241, 248)),
                        );
                        ui.label(
                            RichText::new(format!(
                                "v{} \u{00b7} PDF/CBR \u{2192} CBZ \u{00b7} reader",
                                env!("CARGO_PKG_VERSION")
                            ))
                            .small()
                            .color(MUTED),
                        );
                    });

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .small_button("About")
                            .on_hover_text("About ComicCrate")
                            .clicked()
                        {
                            self.about_open = true;
                        }
                        ui.separator();
                        if ui
                            .selectable_label(self.tab == Tab::Reader, "Reader")
                            .clicked()
                        {
                            self.set_tab(Tab::Reader, &ctx);
                        }
                        if ui
                            .selectable_label(self.tab == Tab::Converter, "Converter")
                            .clicked()
                        {
                            self.set_tab(Tab::Converter, &ctx);
                        }
                    });
                });
            });
    }

    fn draw_options(&mut self, root: &mut Ui) {
        egui::Panel::right("options")
            .resizable(false)
            .exact_size(310.0)
            .frame(
                Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(16, 14)),
            )
            .show(root, |ui| {
                ui.label(RichText::new("Options").size(17.0).strong());
                ui.add_space(4.0);
                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                ui.add_enabled_ui(!self.running, |ui| {
                    ui.checkbox(&mut self.options.overwrite, "Overwrite existing .cbz files")
                        .on_hover_text(
                            "Off: files whose .cbz already exists are skipped. On: the old archive is replaced.",
                        );
                    ui.checkbox(&mut self.options.comic_info, "Include basic ComicInfo.xml")
                        .on_hover_text("Adds <Title> and <PageCount> metadata inside the archive.");
                    ui.checkbox(
                        &mut self.options.delete_source,
                        "Delete original file after conversion",
                    )
                    .on_hover_text("Only runs after the .cbz has been written successfully.");
                });

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(RichText::new("Output image").strong());
                ui.add_enabled_ui(!self.running, |ui| {
                    egui::ComboBox::from_id_salt("image_mode")
                        .width(ui.available_width())
                        .selected_text(self.options.image_mode.short_label())
                        .show_ui(ui, |ui| {
                            for mode in [
                                ImageOutputMode::Original,
                                ImageOutputMode::PngBest,
                                ImageOutputMode::WebpLossless,
                                ImageOutputMode::WebpLossy,
                                ImageOutputMode::Jpeg,
                            ] {
                                ui.selectable_value(
                                    &mut self.options.image_mode,
                                    mode,
                                    mode.label(),
                                );
                            }
                        });

                    match self.options.image_mode {
                        ImageOutputMode::WebpLossy => {
                            ui.add(
                                egui::Slider::new(&mut self.options.webp_quality, 70..=100)
                                    .text("WebP quality"),
                            );
                            ui.label(
                                RichText::new(
                                    "Lower quality = smaller files; pages are never resized.",
                                )
                                .small()
                                .color(MUTED),
                            );
                        }
                        ImageOutputMode::Jpeg => {
                            ui.add(
                                egui::Slider::new(&mut self.options.jpeg_quality, 75..=100)
                                    .text("JPEG quality"),
                            );
                            ui.label(
                                RichText::new(
                                    "Lower quality = smaller files; pages are never resized.",
                                )
                                .small()
                                .color(MUTED),
                            );
                        }
                        _ => {}
                    }
                });
                let note = match self.options.image_mode {
                    ImageOutputMode::Original => {
                        "JPEG streams are copied byte-for-byte; everything else becomes \
                         max-compression PNG."
                    }
                    ImageOutputMode::PngBest => {
                        "Every page is decoded and re-encoded as lossless max-compression PNG."
                    }
                    ImageOutputMode::WebpLossless => {
                        "Every page is decoded and re-encoded as lossless WebP."
                    }
                    ImageOutputMode::WebpLossy => {
                        "Every page is decoded and re-encoded as lossy WebP."
                    }
                    ImageOutputMode::Jpeg => {
                        "Every page is decoded and re-encoded as JPEG."
                    }
                };
                ui.label(RichText::new(note).small().color(MUTED));
                ui.label(
                    RichText::new(format!(
                        "Parallel encoding on {} CPU thread(s).",
                        rayon::current_num_threads()
                    ))
                    .small()
                    .color(MUTED),
                );

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(RichText::new("Output folder").strong());

                let mut custom = self.options.output_dir.is_some();
                ui.add_enabled_ui(!self.running, |ui| {
                    if ui
                        .radio_value(&mut custom, false, "Same as source file")
                        .clicked()
                    {
                        self.options.output_dir = None;
                    }
                    if ui
                        .radio_value(&mut custom, true, "Custom folder")
                        .clicked()
                        && self.options.output_dir.is_none()
                    {
                        self.options.output_dir = Some(default_output_dir(&self.jobs));
                    }
                });

                let output_dir = self.options.output_dir.clone();
                match &output_dir {
                    Some(dir) => {
                        let text = dir.display().to_string();
                        ui.add(
                            Label::new(RichText::new(text.clone()).small().color(MUTED))
                                .truncate(),
                        )
                        .on_hover_text(text);
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(!self.running, Button::new("Choose..."))
                                .clicked()
                            {
                                self.choose_output_dir();
                            }
                            if ui
                                .add_enabled(!self.running, Button::new("Open"))
                                .clicked()
                            {
                                open_directory(dir);
                            }
                        });
                    }
                    None => {
                        ui.label(
                            RichText::new("Each .cbz is written next to its source file.")
                                .small()
                                .color(MUTED),
                        );
                    }
                }

                ui.add_space(14.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Pixel dimensions are never resized or downscaled. Re-encoding modes \
                         decode each page first, then encode it in the selected format.",
                    )
                    .small()
                    .color(MUTED),
                );

                ui.add_space(12.0);
                let total = self.jobs.len();
                let done = self
                    .jobs
                    .iter()
                    .filter(|job| matches!(job.status, JobStatus::Done | JobStatus::Skipped))
                    .count();
                ui.label(
                    RichText::new(format!("{done} of {total} file(s) finished"))
                        .small()
                        .color(MUTED),
                );
                    });
            });
    }

    fn draw_bottom(&mut self, root: &mut Ui) {
        egui::Panel::bottom("actions")
            .frame(
                Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(18, 12)),
            )
            .show(root, |ui| {
                if self.running || self.batch.done > 0 {
                    let fraction = if self.batch.total == 0 {
                        0.0
                    } else {
                        (self.batch.done as f32 / self.batch.total as f32).clamp(0.0, 1.0)
                    };
                    let overall = format!("{} / {} files", self.batch.done, self.batch.total);
                    ui.add(
                        ProgressBar::new(fraction)
                            .desired_width(ui.available_width())
                            .text(overall)
                            .animate(self.running),
                    );

                    if self.running {
                        let page_fraction = if self.batch.current_total == 0 {
                            0.0
                        } else {
                            (self.batch.current_done as f32 / self.batch.current_total as f32)
                                .clamp(0.0, 1.0)
                        };
                        ui.add(
                            ProgressBar::new(page_fraction)
                                .desired_width(ui.available_width())
                                .text(format!(
                                    "{} \u{00b7} {} / {} pages",
                                    self.batch.current_name,
                                    self.batch.current_done,
                                    self.batch.current_total
                                )),
                        );
                    }
                    ui.add_space(4.0);
                }

                ui.horizontal(|ui| {
                    let probing = self.jobs.iter().any(|job| job.status == JobStatus::Probing);
                    let can_start = !self.running && !probing && self.eligible_count() > 0;
                    let convert_fill = if can_start {
                        ACCENT
                    } else {
                        Color32::from_rgb(40, 43, 52)
                    };
                    let convert = ui.add_enabled(
                        can_start,
                        Button::new(RichText::new("Convert").strong())
                            .fill(convert_fill)
                            .min_size(Vec2::new(120.0, 32.0)),
                    );
                    if convert.clicked() {
                        self.start_batch();
                    }

                    if self.running {
                        if ui
                            .add(Button::new("Stop").min_size(Vec2::new(80.0, 32.0)))
                            .clicked()
                        {
                            self.stop_batch();
                        }
                    } else if ui
                        .add_enabled(
                            !self.jobs.is_empty(),
                            Button::new("Clear queue").min_size(Vec2::new(0.0, 32.0)),
                        )
                        .clicked()
                    {
                        self.clear_queue();
                    }

                    if probing {
                        ui.spinner();
                    }

                    ui.label(RichText::new(self.status_line.clone()).small().color(MUTED));
                });
            });
    }

    fn draw_queue(&mut self, root: &mut Ui) {
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(BG)
                    .inner_margin(Margin::symmetric(18, 14)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Queue").size(17.0).strong());
                    let count = self.jobs.len();
                    if count > 0 {
                        ui.label(RichText::new(format!("({count})")).small().color(MUTED));
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(!self.running, Button::new("Add folder"))
                            .clicked()
                        {
                            self.select_folder();
                        }
                        if ui
                            .add_enabled(
                                !self.running,
                                Button::new(RichText::new("Select Files").strong()),
                            )
                            .clicked()
                        {
                            self.select_files();
                        }
                    });
                });
                ui.add_space(6.0);

                if self.jobs.is_empty() {
                    ui.add_space(70.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new("Drop PDF/CBR comics or a folder here")
                                .size(22.0)
                                .strong()
                                .color(Color32::from_rgb(210, 215, 226)),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(
                                "or use \u{201c}Select Files\u{201d} in the top-right corner",
                            )
                            .color(MUTED),
                        );
                        ui.add_space(18.0);
                        if ui
                            .add(
                                Button::new(RichText::new("Select Files").strong())
                                    .min_size(Vec2::new(140.0, 34.0)),
                            )
                            .clicked()
                        {
                            self.select_files();
                        }
                    });
                    return;
                }

                let mut remove_id = None;
                let mut reveal_path = None;
                let mut reader_path = None;

                let planned_outputs = self.planned_outputs();

                ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (job_index, job) in self.jobs.iter().enumerate() {
                            Frame::new()
                                .fill(if job.status == JobStatus::Processing {
                                    CARD_ACTIVE
                                } else {
                                    CARD
                                })
                                .stroke(Stroke::new(1.0, Color32::from_rgb(44, 47, 57)))
                                .corner_radius(CornerRadius::same(7))
                                .inner_margin(Margin::same(11))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        let name = job
                                            .path
                                            .file_name()
                                            .map(|name| name.to_string_lossy().into_owned())
                                            .unwrap_or_else(|| job.path.display().to_string());
                                        ui.add(
                                            Label::new(RichText::new(name).strong().size(15.0))
                                                .truncate(),
                                        )
                                        .on_hover_text(job.path.display().to_string());

                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                status_badge(ui, job.status);
                                                if !self.running
                                                    && job.status != JobStatus::Processing
                                                    && ui
                                                        .small_button("Remove")
                                                        .on_hover_text(
                                                            "Remove this entry from the queue",
                                                        )
                                                        .clicked()
                                                {
                                                    remove_id = Some(job.id);
                                                }
                                                if job.status == JobStatus::Done
                                                    && ui
                                                        .small_button("Show")
                                                        .on_hover_text(
                                                            "Show the .cbz in its folder",
                                                        )
                                                        .clicked()
                                                {
                                                    reveal_path = job
                                                        .output
                                                        .clone()
                                                        .or_else(|| Some(job.path.clone()));
                                                }
                                                if job.status == JobStatus::Done
                                                    && ui
                                                        .small_button("Open in Reader")
                                                        .on_hover_text(
                                                            "Read the converted .cbz now",
                                                        )
                                                        .clicked()
                                                {
                                                    reader_path = job.output.clone();
                                                }
                                            },
                                        );
                                    });

                                    let output_preview = job.output.clone().unwrap_or_else(|| {
                                        planned_outputs.get(job_index).cloned().unwrap_or_else(
                                            || {
                                                output_path_for(
                                                    &job.path,
                                                    self.options.output_dir.as_deref(),
                                                )
                                            },
                                        )
                                    });
                                    let meta = match job.status {
                                        JobStatus::Probing => "Reading comic...".to_owned(),
                                        JobStatus::Queued => {
                                            let pages = job
                                                .pages
                                                .map(|n| {
                                                    format!(
                                                        "{n} page{}",
                                                        if n == 1 { "" } else { "s" }
                                                    )
                                                })
                                                .unwrap_or_else(|| "Comic".to_owned());
                                            format!(
                                                "{pages} \u{00b7} \u{2192} {}",
                                                output_preview.display()
                                            )
                                        }
                                        JobStatus::Processing => {
                                            let pages = job
                                                .pages
                                                .map(|n| {
                                                    format!(
                                                        "{n} page{}",
                                                        if n == 1 { "" } else { "s" }
                                                    )
                                                })
                                                .unwrap_or_else(|| "Comic".to_owned());
                                            let progress = if job.progress_total > 0 {
                                                format!(
                                                    "{} / {} pages processed",
                                                    job.progress_done, job.progress_total
                                                )
                                            } else {
                                                "starting...".to_owned()
                                            };
                                            format!("{pages} \u{00b7} {progress}")
                                        }
                                        JobStatus::Done => format!(
                                            "{} page{} \u{00b7} {} \u{00b7} \u{2192} {}",
                                            job.pages.unwrap_or(0),
                                            if job.pages == Some(1) { "" } else { "s" },
                                            format_duration(job.elapsed_ms),
                                            output_preview.display()
                                        ),
                                        JobStatus::Skipped => {
                                            format!("Skipped \u{00b7} {}", output_preview.display())
                                        }
                                        JobStatus::Error => {
                                            "Could not convert this file.".to_owned()
                                        }
                                        JobStatus::Cancelled => "Conversion stopped.".to_owned(),
                                    };
                                    ui.add(
                                        Label::new(RichText::new(meta).small().color(MUTED))
                                            .truncate(),
                                    );

                                    if job.status == JobStatus::Done
                                        && job.source_bytes > 0
                                        && job.bytes_written > 0
                                    {
                                        let (size_color, size_text) =
                                            size_delta(job.source_bytes, job.bytes_written);
                                        let text = format!(
                                            "{} \u{2192} CBZ  {size_text}",
                                            input_kind_label(&job.path)
                                        );
                                        ui.add(
                                            Label::new(
                                                RichText::new(text)
                                                    .small()
                                                    .strong()
                                                    .color(size_color),
                                            )
                                            .truncate(),
                                        );
                                    }

                                    if let Some(message) = &job.message {
                                        if matches!(
                                            job.status,
                                            JobStatus::Error
                                                | JobStatus::Skipped
                                                | JobStatus::Cancelled
                                        ) {
                                            ui.add(
                                                Label::new(RichText::new(message).small().color(
                                                    match job.status {
                                                        JobStatus::Error => RED,
                                                        JobStatus::Skipped => YELLOW,
                                                        _ => MUTED,
                                                    },
                                                ))
                                                .wrap(),
                                            );
                                        }
                                    }

                                    if !job.warnings.is_empty() {
                                        egui::CollapsingHeader::new(format!(
                                            "{} warning{}",
                                            job.warnings.len(),
                                            if job.warnings.len() == 1 { "" } else { "s" }
                                        ))
                                        .id_salt(job.id)
                                        .show(ui, |ui| {
                                            for warning in &job.warnings {
                                                ui.label(
                                                    RichText::new(warning)
                                                        .small()
                                                        .color(Color32::from_rgb(210, 180, 130)),
                                                );
                                            }
                                        });
                                    }
                                });
                            ui.add_space(7.0);
                        }
                    });

                if let Some(id) = remove_id {
                    self.remove_job(id);
                }
                if let Some(path) = reveal_path {
                    reveal_in_folder(&path);
                }
                if let Some(path) = reader_path {
                    let ctx = ui.ctx().clone();
                    self.open_in_reader(path, &ctx);
                }
            });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.poll_worker();
        self.handle_drop(&ctx);

        let reader_fullscreen = self.tab == Tab::Reader && self.reader.fullscreen;
        if !reader_fullscreen {
            self.draw_header(ui);
        }

        match self.tab {
            Tab::Converter => {
                self.draw_options(ui);
                self.draw_bottom(ui);
                self.draw_queue(ui);
            }
            Tab::Reader => {
                self.reader.ui(ui);
            }
        }

        if self.drop_hover {
            let label = if self.tab == Tab::Reader {
                "Drop a CBZ, CBR or PDF to read"
            } else {
                "Drop PDF/CBR comics or a folder"
            };
            draw_drop_overlay(&ctx, ui.max_rect(), label);
        }

        self.draw_about(&ctx);

        let probing = self.jobs.iter().any(|job| job.status == JobStatus::Probing);
        if self.running {
            // Continuous repaints keep the animated progress bars at 60 FPS.
            ctx.request_repaint();
        } else if probing {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

fn configure_style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = Color32::from_rgb(11, 12, 16);
    visuals.faint_bg_color = Color32::from_rgb(25, 27, 33);
    visuals.selection.bg_fill = ACCENT.linear_multiply(0.45);
    visuals.hyperlink_color = ACCENT;
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(38, 41, 50);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(33, 36, 44);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(48, 52, 63);
    visuals.widgets.active.bg_fill = Color32::from_rgb(58, 63, 77);
    ctx.set_visuals(visuals);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);
    });
}

fn status_badge(ui: &mut Ui, status: JobStatus) {
    let color = status.color();
    Frame::new()
        .fill(color.gamma_multiply(0.16))
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::symmetric(7, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(status.label()).small().strong().color(color));
        });
}

fn apply_outcome(job: &mut Job, outcome: &ConvertOutcome) {
    job.output = Some(outcome.output.clone());
    job.source_bytes = outcome.source_bytes;
    job.bytes_written = outcome.bytes_written;
    job.elapsed_ms = outcome.elapsed_ms;
    job.warnings = outcome.warnings.clone();
    if outcome.skipped {
        job.status = JobStatus::Skipped;
        job.message = outcome.warnings.first().cloned();
        return;
    }
    job.status = JobStatus::Done;
    job.pages = Some(outcome.total_pages);
    job.progress_done = outcome.total_pages;
    job.progress_total = outcome.total_pages;
    if outcome.extracted_pages < outcome.total_pages {
        job.warnings.insert(
            0,
            format!(
                "{} of {} pages were extracted; see the page warnings below.",
                outcome.extracted_pages, outcome.total_pages
            ),
        );
    }
    if !job.warnings.is_empty() {
        job.message = None;
    }
}

fn collect_pdfs(path: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 12 {
        return;
    }
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        let mut children: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        children.sort();
        for child in children {
            collect_pdfs(&child, out, depth + 1);
        }
    } else if metadata.is_file() && is_converter_input_path(path) {
        out.push(path.to_path_buf());
    }
}

fn path_key(path: &Path) -> String {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    comic_crate::model::path_key(&canonical)
}

fn default_output_dir(jobs: &[Job]) -> PathBuf {
    jobs.first()
        .and_then(|job| job.path.parent())
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn input_kind_label(path: &Path) -> &'static str {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("cbr"))
    {
        "CBR"
    } else {
        "PDF"
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.2} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.1} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{bytes} B")
    }
}

fn size_delta(source: u64, output: u64) -> (Color32, String) {
    let percent = if source == 0 {
        0.0
    } else {
        (output as f64 - source as f64) / source as f64 * 100.0
    };
    let color = if percent < -0.05 {
        GREEN
    } else if percent > 10.0 {
        RED
    } else if percent > 0.05 {
        ORANGE
    } else {
        MUTED
    };
    (
        color,
        format!(
            "{} \u{2192} {} ({percent:+.1}%)",
            format_bytes(source),
            format_bytes(output)
        ),
    )
}

fn format_duration(ms: u128) -> String {
    if ms >= 1000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{ms} ms")
    }
}

fn draw_drop_overlay(ctx: &egui::Context, screen: egui::Rect, label: &str) {
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::unique("drop_overlay"),
    ));
    painter.rect_filled(
        screen,
        CornerRadius::same(0),
        Color32::from_rgba_unmultiplied(8, 9, 12, 220),
    );
    painter.text(
        screen.center(),
        Align2::CENTER_CENTER,
        label,
        FontId::proportional(25.0),
        Color32::from_rgb(235, 240, 250),
    );
}

fn reveal_in_folder(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer")
            .arg("/select,")
            .arg(path)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let folder = path.parent().unwrap_or(path);
        let _ = std::process::Command::new("xdg-open").arg(folder).spawn();
    }
}

fn open_directory(path: &Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

pub fn run() -> eframe::Result {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1060.0, 720.0])
        .with_min_inner_size([840.0, 560.0])
        .with_app_id("comiccrate")
        .with_title(concat!("ComicCrate v", env!("CARGO_PKG_VERSION")));
    if let Some(icon) = load_window_icon() {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        concat!("ComicCrate v", env!("CARGO_PKG_VERSION")),
        native_options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

fn load_window_icon() -> Option<egui::IconData> {
    let image = image::load_from_memory(APP_ICON_BYTES).ok()?;
    let image = image
        .resize_exact(256, 256, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    Some(egui::IconData {
        rgba: image.into_raw(),
        width: 256,
        height: 256,
    })
}
