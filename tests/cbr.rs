use comic_crate::comic_archive;
use comic_crate::convert::{convert_cbr, convert_input, probe_input};
use comic_crate::model::{ConvertOptions, ImageOutputMode};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use zip::ZipArchive;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn read_zip(path: &Path) -> Vec<(String, Vec<u8>)> {
    let mut archive = ZipArchive::new(File::open(path).unwrap()).unwrap();
    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let name = entry.name().unwrap().into_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        entries.push((name, bytes));
    }
    entries
}

#[test]
fn reads_rar3_and_rar5_archives() {
    for name in ["testfile.rar3.cbr", "testfile.rar5.solid.cbr"] {
        let extracted = comic_archive::read_cbr(&fixture(name), &AtomicBool::new(false)).unwrap();
        assert_eq!(extracted.images.len(), 2, "{name}");
        assert_eq!(extracted.images[0].name, "testfile.jpg");
        assert_eq!(extracted.images[1].name, "testfile.png");
        assert!(extracted.images[0].bytes.starts_with(&[0xFF, 0xD8]));
        assert!(extracted.images[1]
            .bytes
            .starts_with(&[0x89, b'P', b'N', b'G']));
    }
}

#[test]
fn reads_comic_info_and_natural_order() {
    let extracted =
        comic_archive::read_cbr(&fixture("test_comic.cbr"), &AtomicBool::new(false)).unwrap();
    assert_eq!(extracted.images.len(), 3);
    assert_eq!(extracted.images[0].name, "page_001.png");
    assert_eq!(extracted.images[1].name, "page_002.png");
    assert_eq!(extracted.images[2].name, "page_003.png");
    let info = String::from_utf8(extracted.comic_info.unwrap()).unwrap();
    assert!(info.contains("<Title>Test Comic</Title>"));
    assert_eq!(
        comic_archive::count_cbr_images(&fixture("test_comic.cbr")).unwrap(),
        3
    );
    assert_eq!(probe_input(&fixture("test_comic.cbr")).unwrap(), 3);
}

#[test]
fn cbr_passthrough_conversion_preserves_pages_and_comic_info() {
    let source = fixture("test_comic.cbr");
    let dir = tempfile::tempdir().unwrap();
    let options = ConvertOptions {
        output_dir: Some(dir.path().to_path_buf()),
        // Existing ComicInfo must be preserved even when generation is off.
        comic_info: false,
        ..ConvertOptions::default()
    };
    let cancel = AtomicBool::new(false);
    let outcome = convert_cbr(&source, &options, &cancel, &|_, _| {}).unwrap();

    assert_eq!(outcome.output, dir.path().join("test_comic.cbz"));
    assert_eq!(outcome.total_pages, 3);
    assert_eq!(outcome.extracted_pages, 3);
    assert_eq!(
        outcome.source_bytes,
        std::fs::metadata(&source).unwrap().len()
    );
    assert_eq!(
        outcome.bytes_written,
        std::fs::metadata(&outcome.output).unwrap().len()
    );

    let entries = read_zip(&outcome.output);
    let names: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec!["ComicInfo.xml", "001.png", "002.png", "003.png"]
    );
    let info = String::from_utf8(entries[0].1.clone()).unwrap();
    assert!(info.contains("<Title>Test Comic</Title>"));
    assert!(entries[1].1.starts_with(&[0x89, b'P', b'N', b'G']));
}

#[test]
fn cbr_reencoding_applies_selected_mode() {
    let source = fixture("testfile.rar3.cbr");
    let dir = tempfile::tempdir().unwrap();
    let options = ConvertOptions {
        output_dir: Some(dir.path().to_path_buf()),
        comic_info: false,
        image_mode: ImageOutputMode::WebpLossless,
        ..ConvertOptions::default()
    };
    let cancel = AtomicBool::new(false);
    let outcome = convert_cbr(&source, &options, &cancel, &|_, _| {}).unwrap();
    let entries = read_zip(&outcome.output);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].0, "001.webp");
    assert_eq!(entries[1].0, "002.webp");
    for (_, bytes) in entries {
        let decoded = webp::Decoder::new(&bytes).decode().unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 2));
    }
}

#[test]
fn cbr_with_invalid_image_aborts_strictly() {
    let source = fixture("invalid_images.cbr");
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("invalid_images.cbz");
    let options = ConvertOptions {
        output_dir: Some(dir.path().to_path_buf()),
        comic_info: false,
        ..ConvertOptions::default()
    };
    let cancel = AtomicBool::new(false);
    let error = convert_cbr(&source, &options, &cancel, &|_, _| {}).unwrap_err();

    assert!(error.contains("Page 1"), "unexpected error: {error}");
    assert!(!output.exists(), "no partial CBZ may remain");
    assert!(source.exists(), "source must remain");
}

#[test]
fn read_cbr_honors_cancellation_between_entries() {
    let cancel = AtomicBool::new(true);
    let error = comic_archive::read_cbr(&fixture("test_comic.cbr"), &cancel).unwrap_err();
    assert!(
        error.to_lowercase().contains("cancel"),
        "unexpected error: {error}"
    );
}

#[test]
fn convert_input_dispatches_cbr() {
    let source = fixture("testfile.rar3.cbr");
    let dir = tempfile::tempdir().unwrap();
    let options = ConvertOptions {
        output_dir: Some(dir.path().to_path_buf()),
        comic_info: false,
        ..ConvertOptions::default()
    };
    let cancel = AtomicBool::new(false);
    let outcome = convert_input(&source, &options, &cancel, &|_, _| {}).unwrap();
    assert_eq!(outcome.extracted_pages, 2);
    assert_eq!(read_zip(&outcome.output).len(), 2);
}
