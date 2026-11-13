//! PDF -> CBZ conversion pipeline.

use crate::comic_archive;
use crate::comic_info;
use crate::model::{
    comic_title, is_cbr_path, is_pdf_path, output_path_for, ConvertOptions, ConvertOutcome,
};
use crate::pdf_image::{encode_image_file, encode_page, EncodeOptions, EncodedImage};
use lopdf::{Dictionary, Document, Object, ObjectId, Stream};
use rayon::prelude::*;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Load a PDF, transparently decrypting the common "empty user password" case
/// (permissions-only encryption), and otherwise returning a helpful error.
pub fn load_document(path: &Path) -> Result<Document, String> {
    let mut doc = Document::load(path).map_err(|err| format!("Could not open PDF: {err}"))?;
    if doc.is_encrypted() {
        doc.decrypt("").map_err(|_| {
            "This PDF is password-protected. Remove the password (or decrypt it) first.".to_owned()
        })?;
    }
    Ok(doc)
}

/// Cheap page-count probe used when files enter the queue.
pub fn probe_pdf(path: &Path) -> Result<usize, String> {
    let doc = load_document(path)?;
    let pages = doc.get_pages();
    if pages.is_empty() {
        Err("PDF contains no pages.".to_owned())
    } else {
        Ok(pages.len())
    }
}

/// Probe either a PDF or a CBR for its page count.
pub fn probe_input(path: &Path) -> Result<usize, String> {
    if is_pdf_path(path) {
        probe_pdf(path)
    } else if is_cbr_path(path) {
        comic_archive::count_cbr_images(path)
    } else {
        Err("Unsupported input. Add a PDF or CBR file.".to_owned())
    }
}

/// Convert either a PDF or CBR into a CBZ archive.
pub fn convert_input(
    path: &Path,
    options: &ConvertOptions,
    cancel: &AtomicBool,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Result<ConvertOutcome, String> {
    if is_pdf_path(path) {
        convert_pdf(path, options, cancel, progress)
    } else if is_cbr_path(path) {
        convert_cbr(path, options, cancel, progress)
    } else {
        Err("Unsupported input. Add a PDF or CBR file.".to_owned())
    }
}

/// Convert one PDF into a CBZ archive.
///
/// All page images are first located in page order, then decoded/encoded in
/// parallel with Rayon. Results are collected in the original order and written
/// sequentially to the archive.
///
/// `progress(done_pages, total_pages)` is thread-safe and called after every
/// page finishes encoding.
pub fn convert_pdf(
    pdf: &Path,
    options: &ConvertOptions,
    cancel: &AtomicBool,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Result<ConvertOutcome, String> {
    let started = Instant::now();
    let output = options
        .output_override
        .clone()
        .unwrap_or_else(|| output_path_for(pdf, options.output_dir.as_deref()));
    let source_bytes = fs::metadata(pdf).map(|meta| meta.len()).unwrap_or(0);

    if cancel.load(Ordering::Relaxed) {
        return Err("Conversion cancelled.".to_owned());
    }

    if output.exists() && !options.overwrite {
        return Ok(ConvertOutcome {
            output: output.clone(),
            source_bytes,
            total_pages: 0,
            extracted_pages: 0,
            bytes_written: fs::metadata(&output).map(|meta| meta.len()).unwrap_or(0),
            warnings: vec![
                "Output already exists; skipped because overwrite is disabled.".to_owned(),
            ],
            skipped: true,
            elapsed_ms: started.elapsed().as_millis(),
        });
    }

    let output_dir = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(output_dir)
        .map_err(|err| format!("Could not create output folder: {err}"))?;

    let doc = load_document(pdf)?;
    let pages = doc.get_pages();
    if pages.is_empty() {
        return Err("PDF contains no pages.".to_owned());
    }
    let total_pages = pages.len();
    let digits = total_pages.to_string().len().max(3);

    // Phase 1: locate the image candidates for every page, in strict page order.
    let plans: Vec<PagePlan<'_>> = pages
        .iter()
        .map(|(page_number, page_id)| PagePlan {
            page_number: *page_number,
            candidates: page_candidates(&doc, *page_id),
        })
        .collect();

    // Phase 2: decode and re-encode all pages in parallel. Rayon's indexed
    // collect keeps the result vector in page order.
    progress(0, total_pages);
    let completed = AtomicUsize::new(0);
    let encode_options = EncodeOptions {
        mode: options.image_mode,
        webp_quality: options.webp_quality,
        jpeg_quality: options.jpeg_quality,
    };

    let results: Vec<PageResult> = plans
        .into_par_iter()
        .map(|plan| {
            let mut warnings = Vec::new();
            let page_number = plan.page_number;
            let mut image = None;
            let mut error = None;

            if cancel.load(Ordering::Relaxed) {
                error = Some("Conversion cancelled.".to_owned());
            } else if plan.candidates.is_empty() {
                error = Some("no embedded raster image (blank or vector-only page)".to_owned());
            } else {
                let mut failures = Vec::new();
                for stream in &plan.candidates {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    match encode_page(stream, &doc, &encode_options) {
                        Ok(encoded) => {
                            if plan.candidates.len() > 1 {
                                warnings.push(format!(
                                    "Page {page_number}: {} images found; used the largest \
                                     extractable image ({} x {}).",
                                    plan.candidates.len(),
                                    encoded.width,
                                    encoded.height
                                ));
                            }
                            if !failures.is_empty() {
                                let details = failures
                                    .iter()
                                    .take(3)
                                    .cloned()
                                    .collect::<Vec<_>>()
                                    .join("; ");
                                warnings.push(format!(
                                    "Page {page_number}: skipped {} image(s) that could not be \
                                     decoded ({details}).",
                                    failures.len()
                                ));
                            }
                            warnings.extend(
                                encoded
                                    .warnings
                                    .iter()
                                    .map(|warning| format!("Page {page_number}: {warning}")),
                            );
                            image = Some(encoded);
                            break;
                        }
                        Err(failure) => failures.push(failure),
                    }
                }

                if image.is_none() {
                    error = if cancel.load(Ordering::Relaxed) {
                        Some("Conversion cancelled.".to_owned())
                    } else {
                        let details = failures
                            .iter()
                            .take(3)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("; ");
                        Some(format!("no extractable image ({details})"))
                    };
                }
            }

            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
            progress(done, total_pages);

            PageResult {
                page_number,
                image,
                warnings,
                error,
            }
        })
        .collect();

    if cancel.load(Ordering::Relaxed) {
        return Err("Conversion cancelled.".to_owned());
    }

    if let Some(failed) = results.iter().find(|result| match &result.image {
        Some(image) => image.bytes.is_empty() || image.width == 0 || image.height == 0,
        None => true,
    }) {
        let reason = failed.error.clone().unwrap_or_else(|| {
            if failed.image.is_some() {
                "encoded page image is empty or has zero dimensions".to_owned()
            } else {
                "no valid page image was produced".to_owned()
            }
        });
        return Err(format!("Page {}: {reason}", failed.page_number));
    }
    let extracted_pages = results
        .iter()
        .filter(|result| result.image.is_some())
        .count();

    // Phase 3: write the finished buffers to the ZIP sequentially, preserving
    // exact page order.
    let temp = temp_path(&output);
    let mut guard = TempGuard::new(temp.clone());
    let file = File::create(&temp).map_err(|err| format!("Could not create .cbz file: {err}"))?;
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let file_options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    if options.comic_info {
        zip.start_file("ComicInfo.xml", file_options)
            .map_err(zip_error)?;
        zip.write_all(comic_info::build(&comic_title(pdf), total_pages).as_bytes())
            .map_err(|err| format!("Could not write ComicInfo.xml: {err}"))?;
    }

    let mut warnings = Vec::new();
    for result in results {
        warnings.extend(result.warnings);
        if let Some(error) = result.error {
            // Strict mode should have returned before we got here.
            return Err(format!("Page {}: {error}", result.page_number));
        }
        if let Some(image) = result.image {
            let entry_name = format!(
                "{:0width$}.{}",
                result.page_number,
                image.extension,
                width = digits
            );
            zip.start_file(&entry_name, file_options)
                .map_err(zip_error)?;
            zip.write_all(&image.bytes)
                .map_err(|err| format!("Could not write page {}: {err}", result.page_number))?;
        } else {
            return Err(format!(
                "Page {}: no valid page image was produced",
                result.page_number
            ));
        }
    }

    let mut writer = zip.finish().map_err(zip_error)?;
    writer
        .flush()
        .map_err(|err| format!("Could not finish .cbz file: {err}"))?;
    drop(writer);

    replace_file(&temp, &output)?;
    guard.disarm();

    let bytes_written = fs::metadata(&output).map(|meta| meta.len()).unwrap_or(0);

    if options.delete_source
        && pdf != output
        && total_pages > 0
        && extracted_pages == total_pages
        && output
            .metadata()
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false)
    {
        if let Err(err) = fs::remove_file(pdf) {
            warnings.push(format!("Could not delete the original PDF: {err}"));
        }
    }

    Ok(ConvertOutcome {
        output,
        source_bytes,
        total_pages,
        extracted_pages,
        bytes_written,
        warnings,
        skipped: false,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

/// Convert a CBR comic into a CBZ archive.
///
/// The RAR is read sequentially (the format has no random access), images are
/// naturally sorted, and then the existing Rayon encoding pipeline is applied.
pub fn convert_cbr(
    cbr: &Path,
    options: &ConvertOptions,
    cancel: &AtomicBool,
    progress: &(dyn Fn(usize, usize) + Sync),
) -> Result<ConvertOutcome, String> {
    let started = Instant::now();
    let output = options
        .output_override
        .clone()
        .unwrap_or_else(|| output_path_for(cbr, options.output_dir.as_deref()));
    let source_bytes = fs::metadata(cbr).map(|meta| meta.len()).unwrap_or(0);

    if cancel.load(Ordering::Relaxed) {
        return Err("Conversion cancelled.".to_owned());
    }

    if output.exists() && !options.overwrite {
        return Ok(ConvertOutcome {
            output: output.clone(),
            source_bytes,
            total_pages: 0,
            extracted_pages: 0,
            bytes_written: fs::metadata(&output).map(|meta| meta.len()).unwrap_or(0),
            warnings: vec![
                "Output already exists; skipped because overwrite is disabled.".to_owned(),
            ],
            skipped: true,
            elapsed_ms: started.elapsed().as_millis(),
        });
    }

    let output_dir = output.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(output_dir)
        .map_err(|err| format!("Could not create output folder: {err}"))?;

    let extracted = comic_archive::read_cbr(cbr, cancel)?;
    let comic_archive::ExtractedComic {
        images,
        comic_info: existing_comic_info,
        ..
    } = extracted;

    let total_pages = images.len();
    let digits = total_pages.to_string().len().max(3);
    if total_pages == 0 {
        return Err("CBR contains no supported image entries.".to_owned());
    }

    progress(0, total_pages);
    let completed = AtomicUsize::new(0);
    let encode_options = EncodeOptions {
        mode: options.image_mode,
        webp_quality: options.webp_quality,
        jpeg_quality: options.jpeg_quality,
    };

    let results: Vec<CbrPageResult> = images
        .into_par_iter()
        .map(|image| {
            let mut error = None;
            let encoded = if cancel.load(Ordering::Relaxed) {
                error = Some("Conversion cancelled.".to_owned());
                None
            } else {
                match encode_image_file(&image.bytes, &encode_options) {
                    Ok(encoded) => Some(encoded),
                    Err(failure) => {
                        error = Some(failure);
                        None
                    }
                }
            };

            let done = completed.fetch_add(1, Ordering::Relaxed) + 1;
            progress(done, total_pages);

            CbrPageResult {
                source_name: image.name,
                image: encoded,
                error,
            }
        })
        .collect();

    if cancel.load(Ordering::Relaxed) {
        return Err("Conversion cancelled.".to_owned());
    }

    if let Some((index, failed)) =
        results
            .iter()
            .enumerate()
            .find(|(_, result)| match &result.image {
                Some(image) => image.bytes.is_empty() || image.width == 0 || image.height == 0,
                None => true,
            })
    {
        let reason = failed.error.clone().unwrap_or_else(|| {
            if failed.image.is_some() {
                "encoded page image is empty or has zero dimensions".to_owned()
            } else {
                "no valid page image was produced".to_owned()
            }
        });
        return Err(format!(
            "Page {} ({}): {reason}",
            index + 1,
            failed.source_name
        ));
    }
    let extracted_pages = results
        .iter()
        .filter(|result| result.image.is_some())
        .count();

    let temp = temp_path(&output);
    let mut guard = TempGuard::new(temp.clone());
    let file = File::create(&temp).map_err(|err| format!("Could not create .cbz file: {err}"))?;
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let file_options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    if let Some(comic_info) = existing_comic_info {
        // Preserve an existing metadata file verbatim, even when the "include"
        // option is off.
        zip.start_file("ComicInfo.xml", file_options)
            .map_err(zip_error)?;
        zip.write_all(&comic_info)
            .map_err(|err| format!("Could not preserve ComicInfo.xml: {err}"))?;
    } else if options.comic_info {
        zip.start_file("ComicInfo.xml", file_options)
            .map_err(zip_error)?;
        zip.write_all(comic_info::build(&comic_title(cbr), total_pages).as_bytes())
            .map_err(|err| format!("Could not write ComicInfo.xml: {err}"))?;
    }

    let mut warnings = Vec::new();
    let mut page_number = 0usize;
    for result in results {
        if let Some(error) = result.error {
            // Strict mode should have returned before we got here.
            return Err(format!(
                "Page {} ({}): {error}",
                page_number + 1,
                result.source_name
            ));
        }
        if let Some(image) = result.image {
            page_number += 1;
            let entry_name = format!("{page_number:0width$}.{}", image.extension, width = digits);
            zip.start_file(&entry_name, file_options)
                .map_err(zip_error)?;
            zip.write_all(&image.bytes)
                .map_err(|err| format!("Could not write page {page_number}: {err}"))?;
        } else {
            return Err(format!(
                "Page {} ({}): no valid page image was produced",
                page_number + 1,
                result.source_name
            ));
        }
    }
    debug_assert_eq!(page_number, total_pages);

    let mut writer = zip.finish().map_err(zip_error)?;
    writer
        .flush()
        .map_err(|err| format!("Could not finish .cbz file: {err}"))?;
    drop(writer);

    replace_file(&temp, &output)?;
    guard.disarm();

    let bytes_written = fs::metadata(&output).map(|meta| meta.len()).unwrap_or(0);

    if options.delete_source
        && cbr != output
        && total_pages > 0
        && extracted_pages == total_pages
        && output
            .metadata()
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false)
    {
        if let Err(err) = fs::remove_file(cbr) {
            warnings.push(format!("Could not delete the original CBR: {err}"));
        }
    }

    Ok(ConvertOutcome {
        output,
        source_bytes,
        total_pages,
        extracted_pages: page_number,
        bytes_written,
        warnings,
        skipped: false,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

/// Decode one PDF page to RGBA for the integrated reader.
pub fn pdf_page_rgba(
    doc: &Document,
    page_id: ObjectId,
) -> Result<crate::pdf_image::RasterPage, String> {
    let candidates = page_candidates(doc, page_id);
    if candidates.is_empty() {
        return Err("page has no embedded raster image".to_owned());
    }

    let mut failures = Vec::new();
    for stream in candidates {
        match crate::pdf_image::decode_page_rgba(stream, doc) {
            Ok(page) => return Ok(page),
            Err(error) => failures.push(error),
        }
    }

    let details = failures
        .iter()
        .take(3)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!("no decodable image ({details})"))
}

struct CbrPageResult {
    source_name: String,
    image: Option<EncodedImage>,
    error: Option<String>,
}

struct PagePlan<'a> {
    page_number: u32,
    candidates: Vec<&'a Stream>,
}

struct PageResult {
    page_number: u32,
    image: Option<EncodedImage>,
    warnings: Vec<String>,
    error: Option<String>,
}

/// Image XObjects for one page, largest/most useful first.
fn page_candidates(doc: &Document, page_id: ObjectId) -> Vec<&Stream> {
    let streams = collect_page_images(doc, page_id);
    let mut candidates: Vec<(&Stream, u64)> = streams
        .into_iter()
        .filter_map(|stream| {
            let width = dimension(&stream.dict, b"Width", doc)?;
            let height = dimension(&stream.dict, b"Height", doc)?;
            if width == 0 || height == 0 {
                return None;
            }
            let mut score = u64::from(width).saturating_mul(u64::from(height));
            // A stencil mask has the same dimensions as the real image but contains
            // no colour. Strongly prefer a normal image when both are present.
            let is_mask = stream
                .dict
                .get(b"ImageMask")
                .ok()
                .and_then(|object| object.as_bool().ok())
                .unwrap_or(false);
            if !is_mask {
                score = score.saturating_mul(4);
            }
            Some((stream, score.max(1)))
        })
        .collect();

    candidates.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
    candidates.into_iter().map(|(stream, _)| stream).collect()
}

fn dimension(dict: &Dictionary, key: &[u8], doc: &Document) -> Option<u32> {
    let object = dict.get(key).ok()?;
    let object = doc.dereference(object).ok()?.1;
    match object {
        Object::Integer(value) if *value > 0 => Some(*value as u32),
        Object::Real(value) if *value > 0.0 => Some(value.round() as u32),
        _ => None,
    }
}

/// Collect image XObjects from a page and any nested Form XObjects.
fn collect_page_images(doc: &Document, page_id: ObjectId) -> Vec<&Stream> {
    let mut images = Vec::new();
    let mut seen_images = HashSet::new();
    let mut seen_forms = HashSet::new();

    let Ok((direct, referenced)) = doc.get_page_resources(page_id) else {
        return images;
    };
    if let Some(resources) = direct {
        collect_from_resources(
            doc,
            resources,
            &mut images,
            &mut seen_images,
            &mut seen_forms,
            0,
        );
    }
    for id in referenced {
        if let Ok(resources) = doc.get_dictionary(id) {
            collect_from_resources(
                doc,
                resources,
                &mut images,
                &mut seen_images,
                &mut seen_forms,
                0,
            );
        }
    }
    images
}

fn collect_from_resources<'a>(
    doc: &'a Document,
    resources: &'a Dictionary,
    images: &mut Vec<&'a Stream>,
    seen_images: &mut HashSet<ObjectId>,
    seen_forms: &mut HashSet<ObjectId>,
    depth: usize,
) {
    if depth > 4 {
        return;
    }
    let Ok(xobjects_object) = resources.get_deref(b"XObject", doc) else {
        return;
    };
    let Ok(xobjects) = xobjects_object.as_dict() else {
        return;
    };

    for (_name, object) in xobjects.iter() {
        let object_id = object.as_reference().ok();
        let Ok(resolved) = doc.dereference(object).map(|(_, object)| object) else {
            continue;
        };
        let Ok(stream) = resolved.as_stream() else {
            continue;
        };
        let subtype = stream
            .dict
            .get(b"Subtype")
            .ok()
            .and_then(|object| object.as_name().ok())
            .unwrap_or_default();

        if subtype == b"Image" {
            if let Some(id) = object_id {
                if !seen_images.insert(id) {
                    continue;
                }
            }
            images.push(stream);
        } else if subtype == b"Form" {
            if let Some(id) = object_id {
                if !seen_forms.insert(id) {
                    continue;
                }
            }
            if let Ok(resources) = stream.dict.get_deref(b"Resources", doc) {
                if let Ok(resources) = resources.as_dict() {
                    collect_from_resources(
                        doc,
                        resources,
                        images,
                        seen_images,
                        seen_forms,
                        depth + 1,
                    );
                }
            }
        }
    }
}

fn zip_error(err: zip::result::ZipError) -> String {
    format!("Could not build .cbz archive: {err}")
}

fn temp_path(output: &Path) -> PathBuf {
    sibling_path(output, "tmp")
}

fn sibling_path(output: &Path, tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let mut name = output.as_os_str().to_os_string();
    name.push(format!(".{tag}-{}-{nanos}", std::process::id()));
    PathBuf::from(name)
}

fn replace_file(temp: &Path, output: &Path) -> Result<(), String> {
    if !output.exists() {
        return fs::rename(temp, output)
            .map_err(|err| format!("Could not move the finished .cbz into place: {err}"));
    }

    // Keep the old archive until the new one is in place, so a failed rename
    // cannot destroy a previously good file.
    let backup = sibling_path(output, "bak");
    fs::rename(output, &backup)
        .map_err(|err| format!("Could not replace the existing .cbz file: {err}"))?;
    match fs::rename(temp, output) {
        Ok(()) => {
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(err) => {
            let _ = fs::rename(&backup, output);
            Err(format!(
                "Could not move the finished .cbz into place (is it open in another program?): {err}"
            ))
        }
    }
}

struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl TempGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}
