//! Comic archive helpers: RAR/CBR extraction and natural filename ordering.

use std::cmp::Ordering;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use unrar::Archive;

/// Hard cap for one extracted image entry (256 MiB).
pub const MAX_IMAGE_BYTES: u64 = 256 * 1024 * 1024;
/// Hard cap for the total image payload kept in memory from one CBR (1 GiB).
pub const MAX_TOTAL_IMAGE_BYTES: u64 = 1024 * 1024 * 1024;
/// Hard cap for an embedded ComicInfo.xml payload (8 MiB).
pub const MAX_COMIC_INFO_BYTES: u64 = 8 * 1024 * 1024;
/// Hard cap for a CBZ loaded into memory (2 GiB).
pub const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Archive formats the reader can identify from their leading bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    Rar,
    Pdf,
    Unknown,
}

/// Detect a comic/archive format from its first bytes.
pub fn archive_kind_from_magic(bytes: &[u8]) -> ArchiveKind {
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        ArchiveKind::Zip
    } else if bytes.starts_with(b"Rar!\x1a\x07") {
        ArchiveKind::Rar
    } else if bytes.starts_with(b"%PDF-") {
        ArchiveKind::Pdf
    } else {
        ArchiveKind::Unknown
    }
}

/// Read only the short header needed for format detection. This is a pure
/// sequential read so it also works on FUSE/portal paths.
pub fn sniff_archive_kind(path: &Path) -> Result<ArchiveKind, String> {
    use std::io::Read;

    let mut file =
        std::fs::File::open(path).map_err(|error| format!("Could not open comic: {error}"))?;
    let mut header = [0u8; 8];
    let read = file
        .read(&mut header)
        .map_err(|error| format!("Could not read comic header: {error}"))?;
    Ok(archive_kind_from_magic(&header[..read]))
}

/// Read a CBZ sequentially into memory.
///
/// `zip::ZipArchive` seeks to the end of the file to find the EOCD record. On
/// FUSE/portal-mounted files (for example the document paths returned by
/// `rfd` under Wayland) that seek can fail. Reading the whole file first and
/// wrapping it in a `Cursor` avoids filesystem seeks entirely.
pub fn read_archive_bytes(path: &Path) -> Result<Vec<u8>, String> {
    let metadata =
        std::fs::metadata(path).map_err(|error| format!("Could not inspect archive: {error}"))?;
    if metadata.len() > MAX_ARCHIVE_BYTES {
        return Err(format!(
            "Archive is too large ({} bytes; limit is {} bytes).",
            metadata.len(),
            MAX_ARCHIVE_BYTES
        ));
    }

    let bytes = std::fs::read(path).map_err(|error| format!("Could not read archive: {error}"))?;
    if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
        return Err(format!(
            "Archive is too large ({} bytes; limit is {} bytes).",
            bytes.len(),
            MAX_ARCHIVE_BYTES
        ));
    }
    if bytes.is_empty() {
        return Err("Archive is empty.".to_owned());
    }
    Ok(bytes)
}

/// One image entry extracted from a CBR archive.
#[derive(Clone, Debug)]
pub struct NamedImage {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// All useful contents of a CBR archive.
#[derive(Debug, Default)]
pub struct ExtractedComic {
    /// Images in natural sort order.
    pub images: Vec<NamedImage>,
    /// Optional `ComicInfo.xml` payload (first one found).
    pub comic_info: Option<Vec<u8>>,
    pub warnings: Vec<String>,
}

/// Natural, case-insensitive comparison: `page2` sorts before `page10`.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let mut i = 0usize;
    let mut j = 0usize;

    while i < a_bytes.len() && j < b_bytes.len() {
        if a_bytes[i].is_ascii_digit() && b_bytes[j].is_ascii_digit() {
            let start_a = i;
            let start_b = j;
            while i < a_bytes.len() && a_bytes[i].is_ascii_digit() {
                i += 1;
            }
            while j < b_bytes.len() && b_bytes[j].is_ascii_digit() {
                j += 1;
            }

            let digits_a = &a[start_a..i];
            let digits_b = &b[start_b..j];
            let trimmed_a = digits_a.trim_start_matches('0');
            let trimmed_b = digits_b.trim_start_matches('0');
            let number_a = if trimmed_a.is_empty() { "0" } else { trimmed_a };
            let number_b = if trimmed_b.is_empty() { "0" } else { trimmed_b };

            let order = number_a
                .len()
                .cmp(&number_b.len())
                .then_with(|| number_a.cmp(number_b))
                .then_with(|| digits_a.len().cmp(&digits_b.len()))
                .then_with(|| digits_a.cmp(digits_b));
            if order != Ordering::Equal {
                return order;
            }
        } else {
            let char_a = a[i..].chars().next().expect("valid char boundary");
            let char_b = b[j..].chars().next().expect("valid char boundary");
            let lower_a = char_a.to_ascii_lowercase();
            let lower_b = char_b.to_ascii_lowercase();
            if lower_a != lower_b {
                return lower_a.cmp(&lower_b);
            }
            i += char_a.len_utf8();
            j += char_b.len_utf8();
        }
    }

    (a_bytes.len() - i)
        .cmp(&(b_bytes.len() - j))
        .then_with(|| a.cmp(b))
}

/// True for the image formats we accept inside comic archives.
pub fn is_image_name(name: &str) -> bool {
    if is_junk_entry(name) {
        return false;
    }
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "jp2" | "j2k"
    )
}

/// Ignore metadata/noise entries commonly added by macOS, Windows, and
/// AppleDouble-style archive tools.
pub fn is_junk_entry(name: &str) -> bool {
    let normalized = name.replace('\\', "/");
    if normalized
        .split('/')
        .any(|component| component.eq_ignore_ascii_case("__MACOSX"))
    {
        return true;
    }
    let filename = normalized.rsplit('/').next().unwrap_or(&normalized);
    filename.eq_ignore_ascii_case(".DS_Store")
        || filename.eq_ignore_ascii_case("Thumbs.db")
        || filename.starts_with("._")
}

/// True for a ComicInfo.xml entry at any depth.
pub fn is_comic_info_name(name: &str) -> bool {
    if is_junk_entry(name) {
        return false;
    }
    Path::new(name)
        .file_name()
        .and_then(|filename| filename.to_str())
        .is_some_and(|filename| filename.eq_ignore_ascii_case("ComicInfo.xml"))
}

/// Count image entries in a CBR without extracting them.
pub fn count_cbr_images(path: &Path) -> Result<usize, String> {
    let archive = Archive::new(path)
        .open_for_listing()
        .map_err(|error| format!("Could not open CBR archive: {error}"))?;

    let mut count = 0usize;
    for entry in archive {
        let entry = entry.map_err(|error| format!("Could not read CBR directory: {error}"))?;
        if entry.is_file() && is_image_name(&entry.filename.to_string_lossy()) {
            count += 1;
        }
    }

    if count == 0 {
        Err("CBR contains no supported image entries (PNG/JPEG/WebP/BMP/JP2).".to_owned())
    } else {
        Ok(count)
    }
}

/// Extract every image and any ComicInfo.xml from a CBR archive.
///
/// RAR has no random access API, so entries are processed front-to-back once.
/// Images are returned in natural filename order. The cancellation flag is
/// checked between entries so a large archive can be stopped promptly.
pub fn read_cbr(path: &Path, cancel: &AtomicBool) -> Result<ExtractedComic, String> {
    let mut archive = Archive::new(path)
        .open_for_processing()
        .map_err(|error| format!("Could not open CBR archive: {error}"))?;

    let mut extracted = ExtractedComic::default();
    let mut total_image_bytes = 0u64;

    loop {
        if cancel.load(AtomicOrdering::Relaxed) {
            return Err("CBR extraction cancelled.".to_owned());
        }

        let header = match archive.read_header() {
            Ok(Some(header)) => header,
            Ok(None) => break,
            Err(error) => return Err(format!("Could not read CBR header: {error}")),
        };

        let (name, is_file, declared_size) = {
            let entry = header.entry();
            (
                entry.filename.to_string_lossy().replace('\\', "/"),
                entry.is_file(),
                entry.unpacked_size,
            )
        };

        if !is_file {
            archive = header
                .skip()
                .map_err(|error| format!("Could not skip CBR entry {name}: {error}"))?;
            continue;
        }

        if is_comic_info_name(&name) {
            if declared_size > MAX_COMIC_INFO_BYTES {
                extracted.warnings.push(format!(
                    "Ignored oversized ComicInfo.xml ({declared_size} bytes) in this CBR."
                ));
                archive = header
                    .skip()
                    .map_err(|error| format!("Could not skip CBR entry {name}: {error}"))?;
                continue;
            }
            let (data, next) = header
                .read()
                .map_err(|error| format!("Could not read {name} from CBR: {error}"))?;
            if data.len() as u64 > MAX_COMIC_INFO_BYTES {
                extracted.warnings.push(format!(
                    "Ignored oversized ComicInfo.xml ({} bytes) in this CBR.",
                    data.len()
                ));
            } else if extracted.comic_info.is_none() {
                extracted.comic_info = Some(data);
            }
            archive = next;
        } else if is_image_name(&name) {
            if declared_size > MAX_IMAGE_BYTES {
                return Err(format!(
                    "CBR entry {name} is too large ({declared_size} bytes; the per-page limit is {MAX_IMAGE_BYTES} bytes)."
                ));
            }
            if total_image_bytes.saturating_add(declared_size) > MAX_TOTAL_IMAGE_BYTES {
                return Err(format!(
                    "CBR image payload exceeds the {MAX_TOTAL_IMAGE_BYTES}-byte archive limit at {name}."
                ));
            }
            let (data, next) = header
                .read()
                .map_err(|error| format!("Could not extract {name} from CBR: {error}"))?;
            let actual_size = data.len() as u64;
            if actual_size > MAX_IMAGE_BYTES {
                return Err(format!(
                    "CBR entry {name} expanded to {actual_size} bytes; the per-page limit is {MAX_IMAGE_BYTES} bytes."
                ));
            }
            total_image_bytes = total_image_bytes.saturating_add(actual_size);
            if total_image_bytes > MAX_TOTAL_IMAGE_BYTES {
                return Err(format!(
                    "CBR image payload exceeds the {MAX_TOTAL_IMAGE_BYTES}-byte archive limit at {name}."
                ));
            }
            extracted.images.push(NamedImage { name, bytes: data });
            archive = next;
        } else {
            archive = header
                .skip()
                .map_err(|error| format!("Could not skip CBR entry {name}: {error}"))?;
        }
    }

    extracted
        .images
        .sort_by(|left, right| natural_cmp(&left.name, &right.name));

    if extracted.images.is_empty() {
        return Err("CBR contains no supported image entries (PNG/JPEG/WebP/BMP/JP2).".to_owned());
    }

    Ok(extracted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order_handles_numbers_and_case() {
        let mut names = vec![
            "page10.jpg",
            "Page2.jpg",
            "page1.jpg",
            "page02.jpg",
            "page2.jpg",
            "chapter 1/page3.png",
            "chapter 1/page20.png",
        ];
        names.sort_by(|left, right| natural_cmp(left, right));
        assert_eq!(
            names,
            vec![
                "chapter 1/page3.png",
                "chapter 1/page20.png",
                "page1.jpg",
                "Page2.jpg",
                "page2.jpg",
                "page02.jpg",
                "page10.jpg",
            ]
        );
    }

    #[test]
    fn image_and_comic_info_detection() {
        assert!(is_image_name("001.JPG"));
        assert!(is_image_name("folder/page.webp"));
        assert!(is_image_name("scan/page.jp2"));
        assert!(is_image_name("scan/page.J2K"));
        assert!(!is_image_name("__MACOSX/._page.jpg"));
        assert!(!is_image_name(".DS_Store"));
        assert!(!is_image_name("Thumbs.db"));
        assert!(!is_image_name("ComicInfo.xml"));
        assert!(!is_image_name("notes.txt"));
        assert!(is_comic_info_name("ComicInfo.xml"));
        assert!(is_comic_info_name("sub/ComicInfo.xml"));
        assert!(!is_comic_info_name("__MACOSX/sub/ComicInfo.xml"));
        assert!(!is_comic_info_name("MyComicInfo.xml"));
    }

    #[test]
    fn archive_magic_detection() {
        assert_eq!(
            archive_kind_from_magic(b"PK\x03\x04rest-of-zip"),
            ArchiveKind::Zip
        );
        assert_eq!(
            archive_kind_from_magic(b"Rar!\x1a\x07\x01\x00rest"),
            ArchiveKind::Rar
        );
        assert_eq!(archive_kind_from_magic(b"%PDF-1.7\n"), ArchiveKind::Pdf);
        assert_eq!(
            archive_kind_from_magic(b"not-an-archive"),
            ArchiveKind::Unknown
        );
    }

    #[test]
    fn sniffing_real_fixtures_detects_their_formats() {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures");
        assert_eq!(
            sniff_archive_kind(&fixtures.join("test_comic.cbz")).unwrap(),
            ArchiveKind::Zip
        );
        assert_eq!(
            sniff_archive_kind(&fixtures.join("test_comic.cbr")).unwrap(),
            ArchiveKind::Rar
        );
    }

    #[test]
    fn empty_archive_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.cbz");
        std::fs::File::create(&path).unwrap();
        let error = read_archive_bytes(&path).unwrap_err();
        assert!(error.contains("empty"), "unexpected error: {error}");
    }
}
