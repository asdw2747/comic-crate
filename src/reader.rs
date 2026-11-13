//! Integrated comic reader for CBZ, CBR and PDF files.

use comic_crate::comic_archive;
use comic_crate::convert;
use comic_crate::model::is_reader_path;
use comic_crate::pdf_image;
use eframe::egui::{
    self, Button, Color32, ColorImage, Context, Frame, Margin, Rect, RichText, Sense,
    TextureHandle, TextureOptions, Ui, Vec2,
};
use lopdf::{Document, ObjectId};
use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use zip::ZipArchive;

const BG: Color32 = Color32::from_rgb(10, 11, 14);
const PANEL: Color32 = Color32::from_rgb(21, 22, 28);
const MUTED: Color32 = Color32::from_rgb(148, 152, 166);
const ERROR: Color32 = Color32::from_rgb(240, 110, 110);
const SPREAD_GAP: f32 = 8.0;
const MAX_CACHED_TEXTURES: usize = 12;
const MAX_CONCURRENT_LOADS: usize = 6;
/// Refuse to decode a page larger than this, even before allocating pixels.
const MAX_DECODE_PIXELS: u64 = 100_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderView {
    FitHeight,
    FitWidth,
    Spread,
}

pub struct ComicBook {
    pub title: String,
    source: ComicSource,
}

enum ComicSource {
    Cbz {
        bytes: Vec<u8>,
        entry_indices: Vec<usize>,
    },
    Cbr {
        images: Vec<Vec<u8>>,
    },
    Pdf {
        document: Arc<Document>,
        page_ids: Vec<ObjectId>,
    },
}

impl ComicBook {
    pub fn open(path: &Path) -> Result<Self, String> {
        if !is_reader_path(path) {
            return Err("Reader supports CBZ, CBR and PDF files.".to_owned());
        }
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let title = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Comic".to_owned());

        // Content wins over the filename: a `.cbz` may actually be a RAR and
        // a `.cbr` may actually be a ZIP. Only fall back to the extension when
        // the magic bytes are unrecognised.
        let source = match comic_archive::sniff_archive_kind(path)? {
            comic_archive::ArchiveKind::Zip => open_cbz(path)?,
            comic_archive::ArchiveKind::Rar => open_cbr(path)?,
            comic_archive::ArchiveKind::Pdf => open_pdf(path)?,
            comic_archive::ArchiveKind::Unknown => match extension.as_str() {
                "cbz" => open_cbz(path)?,
                "cbr" => open_cbr(path)?,
                "pdf" => open_pdf(path)?,
                _ => return Err("Unsupported reader format.".to_owned()),
            },
        };

        Ok(Self { title, source })
    }

    pub fn page_count(&self) -> usize {
        match &self.source {
            ComicSource::Cbz { entry_indices, .. } => entry_indices.len(),
            ComicSource::Cbr { images } => images.len(),
            ComicSource::Pdf { page_ids, .. } => page_ids.len(),
        }
    }

    pub fn load_page_rgba(
        &self,
        index: usize,
        max_texture_side: u32,
    ) -> Result<(u32, u32, Vec<u8>), String> {
        if index >= self.page_count() {
            return Err("page index out of range".to_owned());
        }

        match &self.source {
            ComicSource::Cbz {
                bytes,
                entry_indices,
            } => {
                // The archive was loaded into memory when it was opened, so
                // page reads never touch the filesystem or perform a seek.
                let mut archive = ZipArchive::new(Cursor::new(bytes.as_slice()))
                    .map_err(|error| format!("Invalid CBZ archive: {error}"))?;
                let mut entry = archive
                    .by_index(entry_indices[index])
                    .map_err(|error| format!("Could not read CBZ page: {error}"))?;
                let declared_size = entry.size();
                if declared_size > comic_archive::MAX_IMAGE_BYTES {
                    return Err(format!(
                        "CBZ page is too large ({declared_size} bytes; limit is {} bytes).",
                        comic_archive::MAX_IMAGE_BYTES
                    ));
                }
                let mut page_bytes = Vec::new();
                entry
                    .by_ref()
                    .take(comic_archive::MAX_IMAGE_BYTES + 1)
                    .read_to_end(&mut page_bytes)
                    .map_err(|error| format!("Could not extract CBZ page: {error}"))?;
                if page_bytes.len() as u64 > comic_archive::MAX_IMAGE_BYTES {
                    return Err(format!(
                        "CBZ page expanded to {} bytes; limit is {} bytes.",
                        page_bytes.len(),
                        comic_archive::MAX_IMAGE_BYTES
                    ));
                }
                decode_image_rgba(&page_bytes, max_texture_side)
            }
            ComicSource::Cbr { images } => decode_image_rgba(&images[index], max_texture_side),
            ComicSource::Pdf { document, page_ids } => {
                let page = convert::pdf_page_rgba(document, page_ids[index])?;
                fit_rgba(page.width, page.height, page.rgba, max_texture_side)
            }
        }
    }
}

fn open_cbz(path: &Path) -> Result<ComicSource, String> {
    let bytes = comic_archive::read_archive_bytes(path)?;
    let entry_indices = {
        let mut archive = ZipArchive::new(Cursor::new(bytes.as_slice()))
            .map_err(|error| format!("Invalid CBZ archive: {error}"))?;
        let mut entries = Vec::new();
        for index in 0..archive.len() {
            let entry = archive
                .by_index(index)
                .map_err(|error| format!("Could not read CBZ directory: {error}"))?;
            if entry.is_dir() {
                continue;
            }
            let name = entry
                .name()
                .map_err(|error| format!("Invalid CBZ entry name: {error}"))?
                .into_owned();
            if comic_archive::is_image_name(&name) {
                entries.push((index, name));
            }
        }
        entries.sort_by(|left, right| comic_archive::natural_cmp(&left.1, &right.1));
        if entries.is_empty() {
            return Err("CBZ contains no supported images.".to_owned());
        }
        entries
            .into_iter()
            .map(|(index, _)| index)
            .collect::<Vec<_>>()
    };

    Ok(ComicSource::Cbz {
        bytes,
        entry_indices,
    })
}

fn open_cbr(path: &Path) -> Result<ComicSource, String> {
    let cancelled = AtomicBool::new(false);
    let extracted = comic_archive::read_cbr(path, &cancelled)?;
    Ok(ComicSource::Cbr {
        images: extracted
            .images
            .into_iter()
            .map(|image| image.bytes)
            .collect(),
    })
}

fn open_pdf(path: &Path) -> Result<ComicSource, String> {
    let document = convert::load_document(path)?;
    let pages = document.get_pages();
    if pages.is_empty() {
        return Err("PDF contains no pages.".to_owned());
    }
    Ok(ComicSource::Pdf {
        document: Arc::new(document),
        page_ids: pages.values().copied().collect(),
    })
}

fn decode_image_rgba(bytes: &[u8], max_texture_side: u32) -> Result<(u32, u32, Vec<u8>), String> {
    if pdf_image::is_jpeg2000(bytes) {
        let page = pdf_image::decode_jp2_rgba(bytes)?;
        return fit_rgba(page.width, page.height, page.rgba, max_texture_side);
    }

    // Read header dimensions before decoding: a tiny compressed page can
    // otherwise claim an enormous pixel buffer and exhaust memory.
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("Could not read comic page dimensions: {error}"))?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|error| format!("Could not read comic page dimensions: {error}"))?;
    if width == 0 || height == 0 {
        return Err("comic page has zero dimensions".to_owned());
    }
    if u64::from(width).saturating_mul(u64::from(height)) > MAX_DECODE_PIXELS {
        return Err(format!(
            "comic page is too large ({width} x {height}); limit is {MAX_DECODE_PIXELS} pixels"
        ));
    }

    let image = image::load_from_memory(bytes)
        .map_err(|error| format!("Could not decode comic page: {error}"))?;
    let image = fit_dynamic_image(image, max_texture_side);
    let rgba = image.to_rgba8();
    Ok((rgba.width(), rgba.height(), rgba.into_raw()))
}

fn fit_dynamic_image(image: image::DynamicImage, max_texture_side: u32) -> image::DynamicImage {
    let max_side = max_texture_side.max(1);
    let (width, height) = (image.width(), image.height());
    if width <= max_side && height <= max_side {
        return image;
    }
    let scale = (max_side as f32 / width as f32).min(max_side as f32 / height as f32);
    let new_width = ((width as f32 * scale).round() as u32).max(1);
    let new_height = ((height as f32 * scale).round() as u32).max(1);
    image.resize_exact(new_width, new_height, image::imageops::FilterType::Triangle)
}

fn fit_rgba(
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    max_texture_side: u32,
) -> Result<(u32, u32, Vec<u8>), String> {
    let max_side = max_texture_side.max(1);
    if width <= max_side && height <= max_side {
        return Ok((width, height, rgba));
    }
    let buffer = image::RgbaImage::from_raw(width, height, rgba)
        .ok_or_else(|| "invalid page buffer".to_owned())?;
    let scale = (max_side as f32 / width as f32).min(max_side as f32 / height as f32);
    let new_width = ((width as f32 * scale).round() as u32).max(1);
    let new_height = ((height as f32 * scale).round() as u32).max(1);
    let resized = image::imageops::resize(
        &buffer,
        new_width,
        new_height,
        image::imageops::FilterType::Triangle,
    );
    Ok((new_width, new_height, resized.into_raw()))
}

enum OpenRequest {
    Open { generation: u64, path: PathBuf },
}

struct OpenResult {
    generation: u64,
    result: Result<Arc<ComicBook>, String>,
}

struct PageMessage {
    generation: u64,
    index: usize,
    result: Result<TextureHandle, String>,
}

pub struct Reader {
    comic: Option<Arc<ComicBook>>,
    title: String,
    page: usize,
    view: ReaderView,
    cover_solo: bool,
    zoom: f32,
    pan: Vec2,
    start_at_top: bool,
    cache: HashMap<usize, TextureHandle>,
    loading: HashSet<usize>,
    page_errors: HashMap<usize, String>,
    generation: u64,
    opening: bool,
    open_error: Option<String>,
    last_wheel_flip: f64,
    pub fullscreen: bool,
    open_tx: Sender<OpenRequest>,
    open_rx: Receiver<OpenResult>,
    page_tx: Sender<PageMessage>,
    page_rx: Receiver<PageMessage>,
}

impl Reader {
    pub fn new() -> Self {
        let (open_tx, open_rx) = mpsc::channel();
        let (open_result_tx, open_result_rx) = mpsc::channel();
        thread::Builder::new()
            .name("comic-open".to_owned())
            .spawn(move || {
                while let Ok(request) = open_rx.recv() {
                    let OpenRequest::Open { generation, path } = request;
                    let result = catch_unwind(AssertUnwindSafe(|| ComicBook::open(&path)))
                        .map_err(|_| "internal error while opening comic".to_owned())
                        .and_then(|result| result)
                        .map(Arc::new);
                    if open_result_tx
                        .send(OpenResult { generation, result })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .ok();

        let (page_tx, page_rx) = mpsc::channel();

        Self {
            comic: None,
            title: "No comic open".to_owned(),
            page: 0,
            view: ReaderView::FitHeight,
            cover_solo: false,
            zoom: 1.0,
            pan: Vec2::ZERO,
            start_at_top: false,
            cache: HashMap::new(),
            loading: HashSet::new(),
            page_errors: HashMap::new(),
            generation: 0,
            opening: false,
            open_error: None,
            last_wheel_flip: 0.0,
            fullscreen: false,
            open_tx,
            open_rx: open_result_rx,
            page_tx,
            page_rx,
        }
    }

    pub fn open_path(&mut self, path: PathBuf, ctx: &Context) {
        if !is_reader_path(&path) {
            self.open_error = Some("Reader supports CBZ, CBR and PDF files.".to_owned());
            return;
        }
        self.generation = self.generation.wrapping_add(1);
        self.opening = true;
        self.open_error = None;
        self.comic = None;
        self.page = 0;
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
        self.start_at_top = true;
        self.cache.clear();
        self.loading.clear();
        self.page_errors.clear();
        self.title = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Comic".to_owned());
        let _ = self.open_tx.send(OpenRequest::Open {
            generation: self.generation,
            path,
        });
        ctx.request_repaint();
    }

    pub fn open_dialog(&mut self, ctx: &Context) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Comics", &["cbz", "cbr", "pdf"])
            .pick_file()
        {
            self.open_path(path, ctx);
        }
    }

    pub fn exit_fullscreen(&mut self, ctx: &Context) {
        if self.fullscreen {
            self.fullscreen = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
    }

    fn toggle_fullscreen(&mut self, ctx: &Context) {
        self.fullscreen = !self.fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        ctx.request_repaint();
    }

    fn poll(&mut self, ctx: &Context) {
        while let Ok(result) = self.open_rx.try_recv() {
            if result.generation != self.generation {
                continue;
            }
            self.opening = false;
            match result.result {
                Ok(comic) => {
                    self.page = 0;
                    self.zoom = 1.0;
                    self.pan = Vec2::ZERO;
                    self.start_at_top = true;
                    self.title = comic.title.clone();
                    self.comic = Some(comic);
                    self.open_error = None;
                    self.cache.clear();
                    self.loading.clear();
                    self.page_errors.clear();
                }
                Err(error) => {
                    self.open_error = Some(error);
                    self.comic = None;
                }
            }
        }

        while let Ok(message) = self.page_rx.try_recv() {
            if message.generation != self.generation {
                continue;
            }
            self.loading.remove(&message.index);
            match message.result {
                Ok(texture) => {
                    self.page_errors.remove(&message.index);
                    self.cache.insert(message.index, texture);
                    self.evict_cache();
                }
                Err(error) => {
                    self.page_errors.insert(message.index, error);
                }
            }
        }

        if self.opening || !self.loading.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }
    }

    fn evict_cache(&mut self) {
        if self.cache.len() <= MAX_CACHED_TEXTURES {
            return;
        }
        let current = self.page;
        let mut keys: Vec<usize> = self.cache.keys().copied().collect();
        keys.sort_by_key(|page| page.abs_diff(current));
        for key in keys.into_iter().skip(8) {
            self.cache.remove(&key);
        }
    }

    fn page_count(&self) -> usize {
        self.comic
            .as_ref()
            .map(|comic| comic.page_count())
            .unwrap_or(0)
    }

    fn spread_start(&self, page: usize) -> usize {
        if self.view != ReaderView::Spread {
            return page;
        }
        spread_start_for(page, self.cover_solo)
    }

    fn display_start(&self) -> usize {
        self.spread_start(self.page)
    }

    fn display_pages(&self) -> Vec<usize> {
        let total = self.page_count();
        if total == 0 {
            return Vec::new();
        }
        if self.view != ReaderView::Spread {
            return vec![self.page.min(total - 1)];
        }
        let start = self.display_start();
        if start + 1 < total {
            vec![start, start + 1]
        } else {
            vec![start]
        }
    }

    fn normalize_page(&mut self) {
        let total = self.page_count();
        if total == 0 {
            self.page = 0;
            return;
        }
        self.page = self.page.min(total - 1);
        if self.view == ReaderView::Spread {
            self.page = self.spread_start(self.page).min(total - 1);
        }
    }

    fn set_page(&mut self, page: usize) {
        let total = self.page_count();
        if total == 0 {
            return;
        }
        self.page = page.min(total - 1);
        self.normalize_page();
        // Retry every page in the newly active single/spread view, including
        // the second page of a two-page spread.
        for page in self.display_pages() {
            self.page_errors.remove(&page);
        }
        self.pan = Vec2::ZERO;
        self.start_at_top = true;
    }

    fn next_page(&mut self) {
        let total = self.page_count();
        if total == 0 {
            return;
        }
        if self.view == ReaderView::Spread {
            let start = self.display_start();
            let next = if self.cover_solo && start == 0 {
                1
            } else {
                start + 2
            };
            if next < total {
                self.set_page(next);
            }
        } else if self.page + 1 < total {
            self.set_page(self.page + 1);
        }
    }

    fn prev_page(&mut self) {
        if self.view == ReaderView::Spread {
            let start = self.display_start();
            if start == 0 {
                return;
            }
            let previous = if self.cover_solo && start <= 1 {
                0
            } else {
                start - 2
            };
            self.set_page(previous);
        } else if self.page > 0 {
            self.set_page(self.page - 1);
        }
    }

    fn can_next(&self) -> bool {
        let total = self.page_count();
        if total == 0 {
            return false;
        }
        if self.view == ReaderView::Spread {
            let start = self.display_start();
            let next = if self.cover_solo && start == 0 {
                1
            } else {
                start + 2
            };
            next < total
        } else {
            self.page + 1 < total
        }
    }

    fn can_prev(&self) -> bool {
        if self.view == ReaderView::Spread {
            self.display_start() > 0
        } else {
            self.page > 0
        }
    }

    fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(0.2, 8.0);
    }

    fn wanted_pages(&self) -> Vec<usize> {
        let total = self.page_count();
        if total == 0 {
            return Vec::new();
        }
        let display_pages = self.display_pages();
        let mut wanted = Vec::new();
        let mut seen = HashSet::new();
        let mut push = |page: usize| {
            if seen.insert(page) {
                wanted.push(page);
            }
        };

        // Current spread pages first: when the concurrency limit is reached,
        // nothing the user is looking at should wait behind prefetch pages.
        for &page in &display_pages {
            push(page);
        }
        for &page in &display_pages {
            if page > 0 {
                push(page - 1);
            }
            if page + 1 < total {
                push(page + 1);
            }
        }
        wanted
    }

    fn ensure_pages(&mut self, ctx: &Context) {
        let Some(comic) = self.comic.clone() else {
            return;
        };
        for index in self.wanted_pages() {
            if self.cache.contains_key(&index)
                || self.loading.contains(&index)
                || self.page_errors.contains_key(&index)
            {
                continue;
            }
            if self.loading.len() >= MAX_CONCURRENT_LOADS {
                break;
            }
            self.loading.insert(index);
            let generation = self.generation;
            let ctx = ctx.clone();
            let page_tx = self.page_tx.clone();
            let comic = Arc::clone(&comic);
            let result = thread::Builder::new()
                .name("comic-page".to_owned())
                .spawn(move || {
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        let max_side = ctx.input(|input| input.max_texture_side.max(1) as u32);
                        let (width, height, rgba) = comic.load_page_rgba(index, max_side)?;
                        let image = ColorImage::from_rgba_unmultiplied(
                            [width as usize, height as usize],
                            &rgba,
                        );
                        Ok(ctx.load_texture(
                            format!("comic-{generation}-{index}"),
                            image,
                            TextureOptions::LINEAR,
                        ))
                    }))
                    .unwrap_or_else(|_| Err("internal error while decoding this page".to_owned()));
                    let _ = page_tx.send(PageMessage {
                        generation,
                        index,
                        result,
                    });
                    ctx.request_repaint();
                });
            if result.is_err() {
                self.loading.remove(&index);
                self.page_errors
                    .insert(index, "could not start page loader".to_owned());
            }
        }
    }

    fn handle_input(&mut self, ctx: &Context) {
        if ctx.input(|input| input.key_pressed(egui::Key::F11)) {
            self.toggle_fullscreen(ctx);
        }
        if self.fullscreen && ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.exit_fullscreen(ctx);
        }

        if self.comic.is_none() || ctx.egui_wants_keyboard_input() {
            return;
        }

        let (left, right, zoom_in, zoom_out, wheel, ctrl) = ctx.input(|input| {
            (
                input.key_pressed(egui::Key::ArrowLeft) || input.key_pressed(egui::Key::PageUp),
                input.key_pressed(egui::Key::ArrowRight) || input.key_pressed(egui::Key::PageDown),
                input.key_pressed(egui::Key::Plus) || input.key_pressed(egui::Key::Equals),
                input.key_pressed(egui::Key::Minus),
                input.smooth_scroll_delta.y,
                input.modifiers.ctrl || input.modifiers.command,
            )
        });

        if left {
            self.prev_page();
        }
        if right {
            self.next_page();
        }
        if zoom_in {
            self.zoom_by(1.15);
        }
        if zoom_out {
            self.zoom_by(1.0 / 1.15);
        }

        if wheel.abs() > 0.01 {
            if ctrl {
                self.zoom_by(if wheel > 0.0 { 1.1 } else { 1.0 / 1.1 });
            } else if self.view == ReaderView::FitWidth {
                self.pan.y += wheel;
            } else {
                let now = ctx.input(|input| input.time);
                if wheel.abs() > 1.0 && now - self.last_wheel_flip > 0.14 {
                    self.last_wheel_flip = now;
                    if wheel < 0.0 {
                        self.next_page();
                    } else {
                        self.prev_page();
                    }
                }
            }
        }
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui
                .add(Button::new(RichText::new("Open Comic...").strong()))
                .clicked()
            {
                let ctx = ui.ctx().clone();
                self.open_dialog(&ctx);
            }

            ui.separator();
            let previous_view = self.view;
            ui.selectable_value(&mut self.view, ReaderView::FitHeight, "Fit height");
            ui.selectable_value(&mut self.view, ReaderView::FitWidth, "Fit width");
            ui.selectable_value(&mut self.view, ReaderView::Spread, "Two-page");
            if self.view != previous_view {
                self.normalize_page();
                self.zoom = 1.0;
                self.pan = Vec2::ZERO;
                self.start_at_top = true;
            }

            let previous_solo = self.cover_solo;
            ui.add_enabled(
                self.view == ReaderView::Spread,
                egui::Checkbox::new(&mut self.cover_solo, "Cover solo"),
            )
            .on_hover_text("Show the cover by itself on page 1");
            if self.cover_solo != previous_solo {
                self.normalize_page();
                self.pan = Vec2::ZERO;
            }

            ui.separator();
            if ui.button("-").on_hover_text("Zoom out").clicked() {
                self.zoom_by(1.0 / 1.15);
            }
            ui.label(RichText::new(format!("{:.0}%", self.zoom * 100.0)).small());
            if ui.button("+").on_hover_text("Zoom in").clicked() {
                self.zoom_by(1.15);
            }
            if ui.small_button("100%").clicked() {
                self.zoom = 1.0;
                self.pan = Vec2::ZERO;
            }

            ui.separator();
            let fullscreen_label = if self.fullscreen {
                "Exit fullscreen"
            } else {
                "Fullscreen"
            };
            if ui.button(fullscreen_label).on_hover_text("F11").clicked() {
                let ctx = ui.ctx().clone();
                self.toggle_fullscreen(&ctx);
            }
        });

        if !self.title.is_empty() {
            ui.label(RichText::new(&self.title).small().color(MUTED));
        }
    }

    fn pager(&mut self, ui: &mut Ui) {
        let total = self.page_count();
        if total == 0 {
            return;
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.can_prev(), Button::new("Previous"))
                .clicked()
            {
                self.prev_page();
            }
            if ui
                .add_enabled(self.can_next(), Button::new("Next"))
                .clicked()
            {
                self.next_page();
            }

            let shown = self.display_start() + 1;
            let mut slider_page = shown as i32;
            let slider_width = (ui.available_width() - 135.0).max(110.0);
            ui.spacing_mut().slider_width = slider_width;
            let response =
                ui.add(egui::Slider::new(&mut slider_page, 1..=total as i32).show_value(false));
            if response.changed() {
                self.set_page((slider_page.max(1) as usize) - 1);
            }
            ui.label(
                RichText::new(format!("Page {} / {}", self.display_start() + 1, total))
                    .small()
                    .color(MUTED),
            );
        });
    }

    fn canvas(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, egui::CornerRadius::ZERO, BG);

        if self.comic.is_none() {
            self.empty_state(ui);
            self.draw_reader_hint(&painter, rect);
            return;
        }

        self.ensure_pages(ui.ctx());
        let display_pages = self.display_pages();
        if display_pages.is_empty() {
            return;
        }

        let response = ui.interact(
            rect,
            ui.scope_id().with("reader_canvas"),
            Sense::click_and_drag(),
        );
        if response.dragged() {
            self.pan += response.drag_delta();
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }

        let mut textures = Vec::with_capacity(display_pages.len());
        for &page in &display_pages {
            match self.cache.get(&page) {
                Some(texture) => textures.push((page, texture.clone())),
                None => {
                    let message = if let Some(error) = self.page_errors.get(&page) {
                        RichText::new(error).color(ERROR)
                    } else {
                        RichText::new(format!("Loading page {}...", page + 1)).color(MUTED)
                    };
                    ui.vertical_centered(|ui| {
                        ui.add_space(rect.height() * 0.42);
                        if !self.page_errors.contains_key(&page) {
                            ui.spinner();
                        }
                        ui.label(message);
                    });
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(80));
                    self.draw_reader_hint(&painter, rect);
                    return;
                }
            }
        }

        let sizes: Vec<Vec2> = textures
            .iter()
            .map(|(_, texture)| texture.size_vec2())
            .collect();
        let total_width: f32 = sizes.iter().map(|size| size.x).sum();
        let max_height = sizes.iter().map(|size| size.y).fold(0.0_f32, f32::max);
        if total_width <= 0.0 || max_height <= 0.0 {
            return;
        }

        let available = rect.size();
        let gap_total = SPREAD_GAP * (textures.len().saturating_sub(1)) as f32;
        let base_scale = match self.view {
            ReaderView::FitHeight => available.y / max_height,
            ReaderView::FitWidth => available.x / total_width,
            ReaderView::Spread => {
                (available.y / max_height).min((available.x - gap_total) / total_width)
            }
        };
        let scale = (base_scale * self.zoom).max(0.01);
        let content_width = total_width * scale + gap_total;
        let content_height = max_height * scale;

        let overflow = Vec2::new(
            (content_width - available.x).max(0.0),
            (content_height - available.y).max(0.0),
        );
        if self.view == ReaderView::FitWidth && self.start_at_top && overflow.y > 0.0 {
            self.pan.y = -overflow.y / 2.0;
        }
        self.start_at_top = false;
        self.pan.x = self.pan.x.clamp(-overflow.x / 2.0, overflow.x / 2.0);
        self.pan.y = self.pan.y.clamp(-overflow.y / 2.0, overflow.y / 2.0);

        let center = rect.center() + self.pan;
        let mut x = center.x - content_width / 2.0;
        for ((_, texture), size) in textures.iter().zip(&sizes) {
            let page_width = size.x * scale;
            let page_height = size.y * scale;
            let page_rect = Rect::from_min_size(
                egui::pos2(x, center.y - page_height / 2.0),
                Vec2::new(page_width, page_height),
            );
            painter.rect_filled(
                page_rect.translate(Vec2::new(3.0, 4.0)),
                egui::CornerRadius::same(2),
                Color32::from_black_alpha(110),
            );
            painter.image(
                texture.id(),
                page_rect,
                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            x += page_width + SPREAD_GAP;
        }

        if overflow.y > 0.0 {
            let track = Rect::from_min_size(
                egui::pos2(rect.right() - 7.0, rect.top() + 6.0),
                Vec2::new(3.0, rect.height() - 12.0),
            );
            painter.rect_filled(
                track,
                egui::CornerRadius::same(2),
                Color32::from_white_alpha(20),
            );
            let thumb_height = (track.height() * available.y / content_height).max(24.0);
            let scroll_range = track.height() - thumb_height;
            let progress = if overflow.y > 0.0 {
                (-self.pan.y + overflow.y / 2.0) / overflow.y
            } else {
                0.0
            };
            let thumb_y = track.top() + scroll_range * progress.clamp(0.0, 1.0);
            painter.rect_filled(
                Rect::from_min_size(
                    egui::pos2(track.left(), thumb_y),
                    Vec2::new(track.width(), thumb_height),
                ),
                egui::CornerRadius::same(2),
                Color32::from_white_alpha(90),
            );
        }

        self.draw_reader_hint(&painter, rect);
    }

    fn empty_state(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, egui::CornerRadius::ZERO, BG);

        ui.vertical_centered(|ui| {
            ui.add_space(rect.height() * 0.28);
            if self.opening {
                ui.spinner();
                ui.label(RichText::new(format!("Opening {}...", self.title)).color(MUTED));
                return;
            }
            if let Some(error) = &self.open_error {
                ui.label(
                    RichText::new("Could not open this comic")
                        .size(20.0)
                        .strong()
                        .color(ERROR),
                );
                ui.add_space(6.0);
                ui.label(RichText::new(error).color(ERROR));
                ui.add_space(12.0);
                if ui.button("Open Comic...").clicked() {
                    let ctx = ui.ctx().clone();
                    self.open_dialog(&ctx);
                }
                return;
            }

            ui.label(
                RichText::new("Open a comic to start reading")
                    .size(22.0)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new("CBZ, CBR, or PDF \u{00b7} or drop a file onto this window")
                    .color(MUTED),
            );
            ui.add_space(14.0);
            if ui
                .add(
                    Button::new(RichText::new("Open Comic...").strong())
                        .min_size(Vec2::new(150.0, 34.0)),
                )
                .clicked()
            {
                let ctx = ui.ctx().clone();
                self.open_dialog(&ctx);
            }
        });
    }

    fn draw_reader_hint(&self, painter: &egui::Painter, rect: Rect) {
        if self.fullscreen {
            painter.text(
                egui::pos2(rect.right() - 12.0, rect.bottom() - 10.0),
                egui::Align2::RIGHT_BOTTOM,
                "F11 / Esc: exit fullscreen",
                egui::FontId::proportional(12.0),
                Color32::from_white_alpha(95),
            );
        }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.poll(&ctx);
        self.handle_input(&ctx);

        if !self.fullscreen {
            egui::Panel::top("reader_toolbar")
                .frame(
                    Frame::new()
                        .fill(PANEL)
                        .inner_margin(Margin::symmetric(14, 9)),
                )
                .show(ui, |ui| self.toolbar(ui));

            egui::Panel::bottom("reader_pager")
                .frame(
                    Frame::new()
                        .fill(PANEL)
                        .inner_margin(Margin::symmetric(14, 9)),
                )
                .show(ui, |ui| self.pager(ui));
        }

        egui::CentralPanel::default()
            .frame(Frame::new().fill(BG))
            .show(ui, |ui| self.canvas(ui));

        if self.opening || !self.loading.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }
    }
}

fn spread_start_for(page: usize, cover_solo: bool) -> usize {
    if cover_solo {
        if page == 0 {
            0
        } else {
            1 + ((page - 1) / 2) * 2
        }
    } else {
        (page / 2) * 2
    }
}

#[cfg(test)]
mod tests {
    use super::{spread_start_for, ComicBook};
    use std::path::Path;

    #[test]
    fn spread_starts_are_grouped_in_pairs() {
        assert_eq!(spread_start_for(0, false), 0);
        assert_eq!(spread_start_for(1, false), 0);
        assert_eq!(spread_start_for(2, false), 2);
        assert_eq!(spread_start_for(3, false), 2);
        assert_eq!(spread_start_for(10, false), 10);
    }

    #[test]
    fn cover_solo_gets_its_own_spread() {
        assert_eq!(spread_start_for(0, true), 0);
        assert_eq!(spread_start_for(1, true), 1);
        assert_eq!(spread_start_for(2, true), 1);
        assert_eq!(spread_start_for(3, true), 3);
        assert_eq!(spread_start_for(4, true), 3);
    }

    #[test]
    fn reader_opens_cbz_and_cbr_pages() {
        for name in ["test_comic.cbz", "test_comic.cbr"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join(name);
            let comic = ComicBook::open(&path).unwrap();
            assert_eq!(comic.page_count(), 3, "{name}");
            let (width, height, rgba) = comic.load_page_rgba(0, 4096).unwrap();
            assert!(width > 0 && height > 0, "{name}");
            assert_eq!(rgba.len(), width as usize * height as usize * 4, "{name}");
        }
    }

    #[test]
    fn reader_detects_archive_content_instead_of_extension() {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures");
        let dir = tempfile::tempdir().unwrap();
        let zip_with_cbr_name = dir.path().join("actually-zip.cbr");
        let rar_with_cbz_name = dir.path().join("actually-rar.cbz");
        std::fs::copy(fixtures.join("test_comic.cbz"), &zip_with_cbr_name).unwrap();
        std::fs::copy(fixtures.join("test_comic.cbr"), &rar_with_cbz_name).unwrap();

        for path in [&zip_with_cbr_name, &rar_with_cbz_name] {
            let comic = ComicBook::open(path).unwrap();
            assert_eq!(comic.page_count(), 3, "{}", path.display());
            let (width, height, rgba) = comic.load_page_rgba(0, 4096).unwrap();
            assert!(width > 0 && height > 0, "{}", path.display());
            assert_eq!(
                rgba.len(),
                width as usize * height as usize * 4,
                "{}",
                path.display()
            );
        }
    }
}
