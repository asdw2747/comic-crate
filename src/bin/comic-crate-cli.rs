//! Console companion for batch/automated conversions.
//!
//! The GUI binary (`comic-crate`) uses the Windows GUI subsystem and therefore
//! has no console. This small binary shares the same conversion engine and is
//! built alongside it.

use comic_crate::convert;
use comic_crate::model::{
    is_converter_input_path, plan_output_paths, ConvertOptions, ImageOutputMode,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return;
    }
    std::process::exit(run(&args));
}

fn print_help() {
    println!(
        "ComicCrate v{} \u{2014} PDF/CBR-to-CBZ converter (console companion)\n\
         \n\
         Usage:\n\
         \x20 comic-crate-cli <PDF/CBR or folder...> [options]\n\
         \n\
         Options:\n\
         \x20 -o, --output <DIR>   Write .cbz files into DIR (default: next to each PDF)\n\
         \x20 --mode <MODE>        original | png | webp-lossless | webp-lossy | jpeg\n\
         \x20 --webp-quality <N>   Lossy WebP quality, 70-100 (default 92)\n\
         \x20 --jpeg-quality <N>   JPEG quality, 75-100 (default 95)\n\
         \x20 --overwrite          Replace existing .cbz files\n\
         \x20 --no-comicinfo       Do not add ComicInfo.xml\n\
         \x20 --delete-source      Delete each PDF after a successful conversion\n\
         \x20 -h, --help           Show this help\n\
         \n\
         Folders are scanned recursively for .pdf and .cbr files.",
        env!("CARGO_PKG_VERSION")
    );
}

fn run(args: &[String]) -> i32 {
    let mut options = ConvertOptions::default();
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut index = 0usize;

    while index < args.len() {
        match args[index].as_str() {
            "--output" | "-o" => {
                index += 1;
                let Some(directory) = args.get(index) else {
                    eprintln!("error: --output needs a folder path");
                    return 2;
                };
                options.output_dir = Some(PathBuf::from(directory));
            }
            "--overwrite" => options.overwrite = true,
            "--no-comicinfo" => options.comic_info = false,
            "--delete-source" => options.delete_source = true,
            "--mode" => {
                index += 1;
                let Some(mode) = args.get(index) else {
                    eprintln!("error: --mode needs a value");
                    return 2;
                };
                options.image_mode = match mode.as_str() {
                    "original" | "raw" => ImageOutputMode::Original,
                    "png" => ImageOutputMode::PngBest,
                    "webp-lossless" | "webp" => ImageOutputMode::WebpLossless,
                    "webp-lossy" => ImageOutputMode::WebpLossy,
                    "jpeg" | "jpg" => ImageOutputMode::Jpeg,
                    other => {
                        eprintln!("error: unknown image mode {other}");
                        return 2;
                    }
                };
            }
            "--webp-quality" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("error: --webp-quality needs a value");
                    return 2;
                };
                match value.parse::<u8>() {
                    Ok(quality @ 70..=100) => options.webp_quality = quality,
                    _ => {
                        eprintln!("error: --webp-quality must be between 70 and 100");
                        return 2;
                    }
                }
            }
            "--jpeg-quality" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("error: --jpeg-quality needs a value");
                    return 2;
                };
                match value.parse::<u8>() {
                    Ok(quality @ 75..=100) => options.jpeg_quality = quality,
                    _ => {
                        eprintln!("error: --jpeg-quality must be between 75 and 100");
                        return 2;
                    }
                }
            }
            other if other.starts_with('-') => {
                eprintln!("error: unknown option {other}");
                return 2;
            }
            path => inputs.push(PathBuf::from(path)),
        }
        index += 1;
    }

    let mut pdfs = Vec::new();
    for path in &inputs {
        collect_pdfs(path, &mut pdfs, 0);
    }
    pdfs.sort();
    pdfs.dedup();

    if pdfs.is_empty() {
        eprintln!("error: no PDF or CBR files found");
        return 2;
    }

    let cancel = AtomicBool::new(false);
    let total = pdfs.len();
    let mut failures = 0usize;
    let planned = plan_output_paths(&pdfs, options.output_dir.as_deref());

    for (index, (pdf, output)) in pdfs.iter().zip(planned).enumerate() {
        println!("[{}/{}] {}", index + 1, total, pdf.display());
        let mut job_options = options.clone();
        job_options.output_override = Some(output);
        match convert::convert_input(pdf, &job_options, &cancel, &|_, _| {}) {
            Ok(outcome) if outcome.skipped => {
                println!("  skipped: {}", outcome.output.display());
            }
            Ok(outcome) => {
                let delta = if outcome.source_bytes > 0 {
                    let percent = (outcome.bytes_written as f64 - outcome.source_bytes as f64)
                        / outcome.source_bytes as f64
                        * 100.0;
                    format!(
                        "{} -> {} ({percent:+.1}%)",
                        format_size(outcome.source_bytes),
                        format_size(outcome.bytes_written)
                    )
                } else {
                    format_size(outcome.bytes_written)
                };
                println!(
                    "  -> {} ({} pages, {delta}, {:.1} s)",
                    outcome.output.display(),
                    outcome.extracted_pages,
                    outcome.elapsed_ms as f64 / 1000.0
                );
                for warning in &outcome.warnings {
                    println!("  warning: {warning}");
                }
            }
            Err(error) => {
                failures += 1;
                eprintln!("  error: {error}");
            }
        }
    }

    if failures > 0 {
        1
    } else {
        0
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

fn format_size(bytes: u64) -> String {
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
