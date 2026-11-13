//! Shared data types used by the converter, worker and UI.

use std::path::{Path, PathBuf};

/// How page images should be encoded in the resulting archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ImageOutputMode {
    /// Keep JPEG streams untouched and encode everything else as PNG.
    #[default]
    Original,
    /// Decode every page and re-encode as maximum-compression PNG.
    PngBest,
    /// Decode every page and encode as lossless WebP.
    WebpLossless,
    /// Decode every page and encode as lossy WebP using `webp_quality`.
    WebpLossy,
    /// Decode every page and encode as JPEG using `jpeg_quality`.
    Jpeg,
}

impl ImageOutputMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Original => "Original / raw stream (passthrough)",
            Self::PngBest => "Lossless PNG (max compression)",
            Self::WebpLossless => "Lossless WebP",
            Self::WebpLossy => "Lossy WebP (custom quality)",
            Self::Jpeg => "High-quality JPEG (custom quality)",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Original => "Original / raw",
            Self::PngBest => "PNG (max)",
            Self::WebpLossless => "WebP lossless",
            Self::WebpLossy => "WebP lossy",
            Self::Jpeg => "JPEG",
        }
    }
}

/// User-selectable conversion options.
#[derive(Clone, Debug)]
pub struct ConvertOptions {
    /// Replace an existing `.cbz` file instead of skipping it.
    pub overwrite: bool,
    /// Add a minimal `ComicInfo.xml` to the archive.
    pub comic_info: bool,
    /// Delete the source PDF after a successful conversion.
    pub delete_source: bool,
    /// `None` = write next to the source PDF.
    pub output_dir: Option<PathBuf>,
    /// Exact output path chosen for this job (used to resolve collisions).
    pub output_override: Option<PathBuf>,
    /// Output image format/re-encoding strategy.
    pub image_mode: ImageOutputMode,
    /// Quality for lossy WebP, 70..=100.
    pub webp_quality: u8,
    /// Quality for JPEG, 75..=100.
    pub jpeg_quality: u8,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            overwrite: false,
            comic_info: true,
            delete_source: false,
            output_dir: None,
            output_override: None,
            image_mode: ImageOutputMode::Original,
            webp_quality: 92,
            jpeg_quality: 95,
        }
    }
}

/// Result of converting one PDF.
#[derive(Clone, Debug)]
pub struct ConvertOutcome {
    pub output: PathBuf,
    pub source_bytes: u64,
    pub total_pages: usize,
    pub extracted_pages: usize,
    pub bytes_written: u64,
    pub warnings: Vec<String>,
    pub skipped: bool,
    pub elapsed_ms: u128,
}

/// The `.cbz` path that a given PDF/option pair produces.
pub fn output_path_for(pdf: &Path, output_dir: Option<&Path>) -> PathBuf {
    let stem = pdf
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "comic".to_owned());

    let dir = match output_dir {
        Some(dir) => dir.to_path_buf(),
        None => pdf
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    };

    dir.join(format!("{stem}.cbz"))
}

/// Normalized comparison key for paths. Windows and macOS use
/// case-insensitive filesystems by default; Linux does not.
#[cfg(any(windows, target_os = "macos"))]
pub fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}

/// Normalized comparison key for paths. Windows and macOS use
/// case-insensitive filesystems by default; Linux does not.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Resolve one unique output path per input. Jobs that would otherwise write
/// the same `.cbz` get a source-kind suffix (`_pdf`/`_cbr`), then a numbered
/// suffix if they still collide.
pub fn plan_output_paths(inputs: &[PathBuf], output_dir: Option<&Path>) -> Vec<PathBuf> {
    use std::collections::{HashMap, HashSet};

    let bases: Vec<PathBuf> = inputs
        .iter()
        .map(|input| output_path_for(input, output_dir))
        .collect();
    let keys: Vec<String> = bases.iter().map(|path| path_key(path)).collect();

    let mut counts: HashMap<&str, usize> = HashMap::new();
    for key in &keys {
        *counts.entry(key.as_str()).or_default() += 1;
    }

    // Reserve the paths of non-colliding jobs so an appended suffix can never
    // steal a name that another job already owns.
    let mut used: HashSet<String> = HashSet::new();
    for key in &keys {
        if counts[key.as_str()] == 1 {
            used.insert(key.clone());
        }
    }

    let mut planned = bases.clone();
    for (index, input) in inputs.iter().enumerate() {
        if counts[keys[index].as_str()] == 1 {
            continue;
        }

        let stem = bases[index]
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "comic".to_owned());
        let dir = bases[index].parent().unwrap_or_else(|| Path::new("."));
        let kind = input
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
            .unwrap_or_default();
        let suffix = match kind.as_str() {
            "pdf" => "pdf",
            "cbr" => "cbr",
            _ => "file",
        };

        let mut candidate = dir.join(format!("{stem}_{suffix}.cbz"));
        if used.contains(&path_key(&candidate)) {
            let mut number = 1usize;
            loop {
                candidate = dir.join(format!("{stem}_{suffix} ({number}).cbz"));
                if !used.contains(&path_key(&candidate)) {
                    break;
                }
                number += 1;
            }
        }
        used.insert(path_key(&candidate));
        planned[index] = candidate;
    }

    planned
}

/// Title stored in `ComicInfo.xml` (the source filename without extension).
pub fn comic_title(pdf: &Path) -> String {
    pdf.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Unknown".to_owned())
}

/// Case-insensitive `.pdf` extension check.
pub fn is_pdf_path(path: &Path) -> bool {
    has_extension(path, &["pdf"])
}

/// Case-insensitive `.cbr` extension check.
pub fn is_cbr_path(path: &Path) -> bool {
    has_extension(path, &["cbr"])
}

/// Inputs accepted by the converter queue.
pub fn is_converter_input_path(path: &Path) -> bool {
    has_extension(path, &["pdf", "cbr"])
}

/// Formats accepted by the integrated reader.
pub fn is_reader_path(path: &Path) -> bool {
    has_extension(path, &["pdf", "cbr", "cbz"])
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            extensions
                .iter()
                .any(|candidate| ext.eq_ignore_ascii_case(candidate))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_plan_keeps_unique_names() {
        let inputs = vec![
            PathBuf::from("comics/Alpha.pdf"),
            PathBuf::from("comics/Beta.cbr"),
        ];
        let planned = plan_output_paths(&inputs, Some(Path::new("out")));
        assert_eq!(planned[0], Path::new("out/Alpha.cbz"));
        assert_eq!(planned[1], Path::new("out/Beta.cbz"));
    }

    #[test]
    fn output_plan_suffixes_pdf_and_cbr_collisions() {
        let inputs = vec![
            PathBuf::from("pdf/Comic.pdf"),
            PathBuf::from("cbr/Comic.cbr"),
        ];
        let planned = plan_output_paths(&inputs, Some(Path::new("out")));
        assert_eq!(planned[0], Path::new("out/Comic_pdf.cbz"));
        assert_eq!(planned[1], Path::new("out/Comic_cbr.cbz"));
    }

    #[test]
    fn output_plan_numbers_same_kind_collisions() {
        let inputs = vec![
            PathBuf::from("one/Comic.pdf"),
            PathBuf::from("two/Comic.pdf"),
        ];
        let planned = plan_output_paths(&inputs, Some(Path::new("out")));
        assert_eq!(planned[0], Path::new("out/Comic_pdf.cbz"));
        assert_eq!(planned[1], Path::new("out/Comic_pdf (1).cbz"));
    }

    #[test]
    fn output_plan_does_not_steal_an_existing_unique_name() {
        let inputs = vec![
            PathBuf::from("a/Comic.pdf"),
            PathBuf::from("b/Comic.cbr"),
            PathBuf::from("c/Comic_pdf.pdf"),
        ];
        let planned = plan_output_paths(&inputs, Some(Path::new("out")));
        assert_eq!(planned[0], Path::new("out/Comic_pdf (1).cbz"));
        assert_eq!(planned[1], Path::new("out/Comic_cbr.cbz"));
        assert_eq!(planned[2], Path::new("out/Comic_pdf.cbz"));
    }
}
