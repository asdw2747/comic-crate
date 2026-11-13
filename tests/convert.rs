use comic_crate::convert::{convert_pdf, probe_pdf};
use comic_crate::model::{output_path_for, plan_output_paths, ConvertOptions, ImageOutputMode};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use image::RgbImage;
use lopdf::{dictionary, Document, Object, Stream};
use std::fs::File;
use std::io::{Cursor, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use zip::ZipArchive;

enum TestPage {
    Rgb {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        /// Optional PNG predictor to exercise DecodeParms handling.
        predictor: bool,
    },
    Rgb16 {
        width: u32,
        height: u32,
        samples: Vec<u16>,
    },
    RgbAlpha {
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        alpha: Vec<u8>,
    },
    Jpeg {
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    },
    Jpx {
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    },
    Ccitt {
        width: u32,
        height: u32,
        /// 0 = black, 255 = white, row-major.
        pixels: Vec<u8>,
    },
    NoImage,
    CorruptFlate {
        width: u32,
        height: u32,
    },
}

fn flate(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

fn png_predictor(data: &[u8], width: usize, components: usize) -> Vec<u8> {
    let bpp = components;
    let stride = width * components;
    let mut out = Vec::new();
    for (row_index, row) in data.chunks_exact(stride).enumerate() {
        // Filter type 2 (Up) for every row.
        out.push(2);
        for i in 0..stride {
            let above = if row_index == 0 {
                0
            } else {
                data[(row_index - 1) * stride + i]
            };
            out.push(row[i].wrapping_sub(above));
        }
    }
    let _ = bpp;
    out
}

fn jpeg_bytes(width: u32, height: u32) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([
            (x * 40 % 256) as u8,
            (y * 70 % 256) as u8,
            ((x + y) * 20 % 256) as u8,
        ])
    });
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
        .unwrap();
    bytes
}

fn build_pdf(path: &Path, pages: &[TestPage]) {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let mut kids = Vec::new();

    for (index, page) in pages.iter().enumerate() {
        let (image_id, content) = match page {
            TestPage::Rgb {
                width,
                height,
                pixels,
                predictor,
            } => {
                let encoded = if *predictor {
                    flate(&png_predictor(pixels, *width as usize, 3))
                } else {
                    flate(pixels)
                };
                // The predictor is set to 15, but the actual per-row filter bytes
                // (2 = Up) are supplied by png_predictor.
                let mut dict = dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => *width as i64,
                    "Height" => *height as i64,
                    "ColorSpace" => "DeviceRGB",
                    "BitsPerComponent" => 8,
                    "Filter" => "FlateDecode",
                };
                if *predictor {
                    // Exercise indirect /Filter and /DecodeParms objects plus
                    // the abbreviated /Fl filter name.
                    let filter_id = doc.add_object(Object::Name(b"Fl".to_vec()));
                    let params_id = doc.add_object(dictionary! {
                        "Predictor" => 15,
                        "Colors" => 3,
                        "BitsPerComponent" => 8,
                        "Columns" => *width as i64,
                    });
                    dict.set("Filter", filter_id);
                    dict.set("DecodeParms", params_id);
                }
                (
                    Some(doc.add_object(Stream::new(dict, encoded))),
                    "q 100 0 0 100 0 0 cm /Im0 Do Q",
                )
            }
            TestPage::Rgb16 {
                width,
                height,
                samples,
            } => {
                let bytes: Vec<u8> = samples
                    .iter()
                    .flat_map(|sample| sample.to_be_bytes())
                    .collect();
                let dict = dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => *width as i64,
                    "Height" => *height as i64,
                    "ColorSpace" => "DeviceRGB",
                    "BitsPerComponent" => 16,
                    "Filter" => "FlateDecode",
                };
                (
                    Some(doc.add_object(Stream::new(dict, flate(&bytes)))),
                    "q 100 0 0 100 0 0 cm /Im0 Do Q",
                )
            }
            TestPage::RgbAlpha {
                width,
                height,
                pixels,
                alpha,
            } => {
                let mask_dict = dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => *width as i64,
                    "Height" => *height as i64,
                    "ColorSpace" => "DeviceGray",
                    "BitsPerComponent" => 8,
                    "Filter" => "FlateDecode",
                };
                let mask_id = doc.add_object(Stream::new(mask_dict, flate(alpha)));
                let dict = dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => *width as i64,
                    "Height" => *height as i64,
                    "ColorSpace" => "DeviceRGB",
                    "BitsPerComponent" => 8,
                    "Filter" => "FlateDecode",
                    "SMask" => mask_id,
                };
                (
                    Some(doc.add_object(Stream::new(dict, flate(pixels)))),
                    "q 100 0 0 100 0 0 cm /Im0 Do Q",
                )
            }
            TestPage::Jpeg {
                width,
                height,
                bytes,
            } => {
                let dict = dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => *width as i64,
                    "Height" => *height as i64,
                    "ColorSpace" => "DeviceRGB",
                    "BitsPerComponent" => 8,
                    "Filter" => "DCTDecode",
                };
                (
                    Some(doc.add_object(Stream::new(dict, bytes.clone()))),
                    "q 100 0 0 100 0 0 cm /Im0 Do Q",
                )
            }
            TestPage::Jpx {
                width,
                height,
                bytes,
            } => {
                let dict = dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => *width as i64,
                    "Height" => *height as i64,
                    "ColorSpace" => "DeviceRGB",
                    "BitsPerComponent" => 8,
                    "Filter" => "JPXDecode",
                };
                (
                    Some(doc.add_object(Stream::new(dict, bytes.clone()))),
                    "q 100 0 0 100 0 0 cm /Im0 Do Q",
                )
            }
            TestPage::Ccitt {
                width,
                height,
                pixels,
            } => {
                let mut encoder = fax::encoder::Encoder::new(fax::VecWriter::new());
                for row in pixels.chunks_exact(*width as usize) {
                    encoder
                        .encode_line(
                            row.iter().map(|&pixel| {
                                if pixel == 0 {
                                    fax::Color::Black
                                } else {
                                    fax::Color::White
                                }
                            }),
                            *width as u16,
                        )
                        .unwrap();
                }
                let payload = encoder.finish().unwrap().finish();
                let dict = dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => *width as i64,
                    "Height" => *height as i64,
                    "ColorSpace" => "DeviceGray",
                    "BitsPerComponent" => 1,
                    "Filter" => "CCITTFaxDecode",
                    "DecodeParms" => dictionary! {
                        "K" => -1,
                        "Columns" => *width as i64,
                        "Rows" => *height as i64,
                        "BlackIs1" => false,
                    },
                };
                (
                    Some(doc.add_object(Stream::new(dict, payload))),
                    "q 100 0 0 100 0 0 cm /Im0 Do Q",
                )
            }
            TestPage::NoImage => (None, ""),
            TestPage::CorruptFlate { width, height } => {
                let dict = dictionary! {
                    "Type" => "XObject",
                    "Subtype" => "Image",
                    "Width" => *width as i64,
                    "Height" => *height as i64,
                    "ColorSpace" => "DeviceRGB",
                    "BitsPerComponent" => 8,
                    "Filter" => "FlateDecode",
                };
                (
                    Some(doc.add_object(Stream::new(dict, b"this is not a zlib stream".to_vec()))),
                    "q 100 0 0 100 0 0 cm /Im0 Do Q",
                )
            }
        };

        let resources_id = match image_id {
            Some(image_id) => doc.add_object(dictionary! {
                "XObject" => dictionary! {
                    "Im0" => image_id,
                },
            }),
            None => doc.add_object(dictionary! {}),
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.as_bytes().to_vec()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
            "Resources" => resources_id,
            "Contents" => content_id,
        });
        kids.push(Object::Reference(page_id));
        let _ = index;
    }

    let pages_node = dictionary! {
        "Type" => "Pages",
        "Kids" => kids,
        "Count" => pages.len() as i64,
    };
    doc.objects.insert(pages_id, Object::Dictionary(pages_node));
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);
    doc.save(path).unwrap();
}

fn read_zip(path: &Path) -> Vec<(String, Vec<u8>)> {
    let file = File::open(path).unwrap();
    let mut archive = ZipArchive::new(file).unwrap();
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

fn convert(pdf: &Path, options: &ConvertOptions) -> comic_crate::model::ConvertOutcome {
    let progress_calls = AtomicUsize::new(0);
    let cancel = AtomicBool::new(false);
    let outcome = convert_pdf(pdf, options, &cancel, &|_, _| {
        progress_calls.fetch_add(1, Ordering::Relaxed);
    })
    .unwrap();
    assert!(progress_calls.load(Ordering::Relaxed) > 0);
    outcome
}

#[test]
fn flate_rgb_is_lossless_and_writes_comic_info() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("Comic #1.pdf");
    let pixels = vec![
        255, 0, 0, 0, 255, 0, // red, green
        0, 0, 255, 255, 255, 255, // blue, white
    ];
    build_pdf(
        &pdf,
        &[TestPage::Rgb {
            width: 2,
            height: 2,
            pixels: pixels.clone(),
            predictor: true,
        }],
    );

    assert_eq!(probe_pdf(&pdf).unwrap(), 1);
    let outcome = convert(&pdf, &ConvertOptions::default());
    assert_eq!(
        outcome.output,
        output_path_for(&pdf, None),
        "output must sit next to the source PDF"
    );
    assert_eq!(outcome.extracted_pages, 1);
    assert!(!outcome.skipped);
    assert!(outcome.source_bytes > 0);
    assert!(outcome.bytes_written > 0);
    assert_eq!(
        outcome.bytes_written,
        std::fs::metadata(&outcome.output).unwrap().len()
    );

    let entries = read_zip(&outcome.output);
    let names: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, vec!["ComicInfo.xml", "001.png"]);

    let png = &entries[1].1;
    let decoded = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .unwrap()
        .to_rgb8();
    assert_eq!(decoded.dimensions(), (2, 2));
    assert_eq!(decoded.into_raw(), pixels, "PNG pixels must be unchanged");

    let comic_info = String::from_utf8(entries[0].1.clone()).unwrap();
    assert!(comic_info.contains("<Title>Comic #1</Title>"));
    assert!(comic_info.contains("<PageCount>1</PageCount>"));
}

#[test]
fn jpeg_stream_is_copied_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("photo.pdf");
    let jpeg = jpeg_bytes(5, 3);
    build_pdf(
        &pdf,
        &[TestPage::Jpeg {
            width: 5,
            height: 3,
            bytes: jpeg.clone(),
        }],
    );

    let options = ConvertOptions {
        comic_info: false,
        ..ConvertOptions::default()
    };
    let outcome = convert(&pdf, &options);
    let entries = read_zip(&outcome.output);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "001.jpg");
    assert_eq!(entries[0].1, jpeg, "JPEG payload must not be re-encoded");
}

#[test]
fn multi_page_names_are_padded_and_ordered() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("book.pdf");
    let page = |r: u8| TestPage::Rgb {
        width: 1,
        height: 1,
        pixels: vec![r, 0, 0],
        predictor: false,
    };
    build_pdf(&pdf, &[page(10), page(20), page(30)]);

    let outcome = convert(&pdf, &ConvertOptions::default());
    let entries = read_zip(&outcome.output);
    let names: Vec<String> = entries.iter().map(|(name, _)| name.clone()).collect();
    assert_eq!(
        names,
        vec!["ComicInfo.xml", "001.png", "002.png", "003.png"]
    );
    for (entry, expected_red) in entries[1..].iter().zip([10u8, 20, 30]) {
        let decoded = image::load_from_memory_with_format(&entry.1, image::ImageFormat::Png)
            .unwrap()
            .to_rgb8();
        assert_eq!(decoded.get_pixel(0, 0).0, [expected_red, 0, 0]);
    }
}

#[test]
fn parallel_encoding_reports_every_completed_page() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("progress.pdf");
    let page = |r: u8| TestPage::Rgb {
        width: 1,
        height: 1,
        pixels: vec![r, 0, 0],
        predictor: false,
    };
    build_pdf(&pdf, &[page(1), page(2), page(3)]);

    let seen = std::sync::Mutex::new(Vec::new());
    let cancel = AtomicBool::new(false);
    let outcome = convert_pdf(&pdf, &ConvertOptions::default(), &cancel, &|done, total| {
        seen.lock().unwrap().push((done, total));
    })
    .unwrap();
    assert_eq!(outcome.extracted_pages, 3);

    let mut calls = seen.into_inner().unwrap();
    calls.sort_unstable();
    assert_eq!(calls, vec![(0, 3), (1, 3), (2, 3), (3, 3)]);
}

#[test]
fn existing_output_is_skipped_without_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("skip.pdf");
    build_pdf(
        &pdf,
        &[TestPage::Rgb {
            width: 1,
            height: 1,
            pixels: vec![1, 2, 3],
            predictor: false,
        }],
    );
    let output = output_path_for(&pdf, None);
    std::fs::write(&output, b"keep me").unwrap();

    let outcome = convert_pdf(
        &pdf,
        &ConvertOptions::default(),
        &AtomicBool::new(false),
        &|_, _| {},
    )
    .unwrap();
    assert!(outcome.skipped);
    assert_eq!(std::fs::read(&output).unwrap(), b"keep me");

    let outcome = convert_pdf(
        &pdf,
        &ConvertOptions {
            overwrite: true,
            ..ConvertOptions::default()
        },
        &AtomicBool::new(false),
        &|_, _| {},
    )
    .unwrap();
    assert!(!outcome.skipped);
    assert_eq!(read_zip(&outcome.output).len(), 2);
}

#[test]
fn custom_output_directory_and_delete_source() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    let pdf = source.join("delete me.pdf");
    build_pdf(
        &pdf,
        &[TestPage::Rgb {
            width: 1,
            height: 1,
            pixels: vec![9, 9, 9],
            predictor: false,
        }],
    );

    let outcome = convert(
        &pdf,
        &ConvertOptions {
            delete_source: true,
            output_dir: Some(target.clone()),
            ..ConvertOptions::default()
        },
    );

    assert_eq!(outcome.output, target.join("delete me.cbz"));
    assert!(outcome.output.exists());
    assert!(!pdf.exists(), "source PDF should be deleted after success");
}

#[test]
fn sixteen_bit_rgb_png_is_preserved_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("deep.pdf");
    let samples: Vec<u16> = vec![
        0x0000, 0x1234, 0xFFFF, 0x8000, 0x00FF, 0xFF00, 0xABCD, 0x0001, 0x7FFF, 0xDEAD, 0xBEEF,
        0x5555,
    ];
    build_pdf(
        &pdf,
        &[TestPage::Rgb16 {
            width: 2,
            height: 2,
            samples: samples.clone(),
        }],
    );

    let outcome = convert(&pdf, &ConvertOptions::default());
    let entries = read_zip(&outcome.output);
    let png = &entries[1].1;
    let decoded = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .unwrap()
        .to_rgb16();
    assert_eq!(decoded.dimensions(), (2, 2));
    assert_eq!(
        decoded.into_raw(),
        samples,
        "16-bit samples must be unchanged"
    );
}

#[test]
fn soft_mask_becomes_the_png_alpha_channel() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("alpha.pdf");
    let pixels = vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
    let alpha = vec![0, 85, 170, 255];
    build_pdf(
        &pdf,
        &[TestPage::RgbAlpha {
            width: 2,
            height: 2,
            pixels: pixels.clone(),
            alpha: alpha.clone(),
        }],
    );

    let outcome = convert(&pdf, &ConvertOptions::default());
    let entries = read_zip(&outcome.output);
    let decoded = image::load_from_memory_with_format(&entries[1].1, image::ImageFormat::Png)
        .unwrap()
        .to_rgba8();
    assert_eq!(decoded.dimensions(), (2, 2));
    let raw = decoded.into_raw();
    for (index, expected_alpha) in alpha.iter().enumerate() {
        assert_eq!(
            &raw[index * 4..index * 4 + 3],
            &pixels[index * 3..index * 3 + 3]
        );
        assert_eq!(raw[index * 4 + 3], *expected_alpha);
    }
}

#[test]
fn undecodable_jpx_is_kept_as_the_original_codestream() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("jpx.pdf");
    let payload = b"\xFF\x4F\xFF\x51not-a-real-codestream".to_vec();
    build_pdf(
        &pdf,
        &[TestPage::Jpx {
            width: 4,
            height: 3,
            bytes: payload.clone(),
        }],
    );

    let outcome = convert(&pdf, &ConvertOptions::default());
    assert!(
        outcome
            .warnings
            .iter()
            .any(|warning| warning.contains("JPEG 2000")),
        "expected a JPEG 2000 fallback warning, got {:?}",
        outcome.warnings
    );
    let entries = read_zip(&outcome.output);
    assert_eq!(entries[1].0, "001.j2k");
    assert_eq!(
        entries[1].1, payload,
        "fallback must preserve the codestream"
    );
}

#[test]
fn ccitt_group4_is_decoded_to_lossless_grayscale() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("fax.pdf");
    let (width, height) = (16usize, 6usize);
    let mut pixels = vec![255u8; width * height];
    for y in 0..height {
        for x in 0..width {
            // A mix of long runs, all-white rows and isolated pixels.
            if (y == 2) || (x / 3 + y) % 3 == 0 {
                pixels[y * width + x] = 0;
            }
        }
    }
    build_pdf(
        &pdf,
        &[TestPage::Ccitt {
            width: width as u32,
            height: height as u32,
            pixels: pixels.clone(),
        }],
    );

    let outcome = convert(&pdf, &ConvertOptions::default());
    assert!(
        outcome.warnings.is_empty(),
        "unexpected warnings: {:?}",
        outcome.warnings
    );
    let entries = read_zip(&outcome.output);
    assert_eq!(entries[1].0, "001.png");
    let decoded = image::load_from_memory_with_format(&entries[1].1, image::ImageFormat::Png)
        .unwrap()
        .to_luma8();
    assert_eq!(decoded.dimensions(), (width as u32, height as u32));
    assert_eq!(decoded.into_raw(), pixels);
}

#[test]
fn png_best_reencodes_jpeg_at_original_dimensions() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("reencode.pdf");
    let jpeg = jpeg_bytes(7, 4);
    build_pdf(
        &pdf,
        &[TestPage::Jpeg {
            width: 7,
            height: 4,
            bytes: jpeg.clone(),
        }],
    );

    let outcome = convert(
        &pdf,
        &ConvertOptions {
            image_mode: ImageOutputMode::PngBest,
            comic_info: false,
            ..ConvertOptions::default()
        },
    );
    let entries = read_zip(&outcome.output);
    assert_eq!(entries[0].0, "001.png");

    let expected = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
        .unwrap()
        .to_rgb8();
    let decoded = image::load_from_memory_with_format(&entries[0].1, image::ImageFormat::Png)
        .unwrap()
        .to_rgb8();
    assert_eq!(decoded.dimensions(), expected.dimensions());
    assert_eq!(decoded.into_raw(), expected.into_raw());
}

#[test]
fn lossless_webp_round_trips_pixels_and_dimensions() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("webp.pdf");
    let jpeg = jpeg_bytes(6, 5);
    build_pdf(
        &pdf,
        &[TestPage::Jpeg {
            width: 6,
            height: 5,
            bytes: jpeg.clone(),
        }],
    );

    let outcome = convert(
        &pdf,
        &ConvertOptions {
            image_mode: ImageOutputMode::WebpLossless,
            comic_info: false,
            ..ConvertOptions::default()
        },
    );
    let entries = read_zip(&outcome.output);
    assert_eq!(entries[0].0, "001.webp");
    let decoded = webp::Decoder::new(&entries[0].1).decode().unwrap();
    assert_eq!((decoded.width(), decoded.height()), (6, 5));

    let expected = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
        .unwrap()
        .to_rgba8();
    let actual = decoded.to_image().to_rgba8();
    assert_eq!(actual.dimensions(), expected.dimensions());
    assert_eq!(actual.into_raw(), expected.into_raw());
}

#[test]
fn lossy_webp_and_jpeg_modes_preserve_dimensions() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("lossy.pdf");
    let pixels = vec![
        200, 30, 40, 30, 200, 40, 40, 30, 200, 120, 120, 120, 220, 220, 30, 30, 30, 30,
    ];
    build_pdf(
        &pdf,
        &[TestPage::Rgb {
            width: 3,
            height: 2,
            pixels,
            predictor: false,
        }],
    );

    let webp_outcome = convert(
        &pdf,
        &ConvertOptions {
            image_mode: ImageOutputMode::WebpLossy,
            webp_quality: 80,
            comic_info: false,
            overwrite: true,
            ..ConvertOptions::default()
        },
    );
    let webp_entries = read_zip(&webp_outcome.output);
    assert_eq!(webp_entries[0].0, "001.webp");
    let webp_image = webp::Decoder::new(&webp_entries[0].1).decode().unwrap();
    assert_eq!((webp_image.width(), webp_image.height()), (3, 2));

    let jpeg_outcome = convert(
        &pdf,
        &ConvertOptions {
            image_mode: ImageOutputMode::Jpeg,
            jpeg_quality: 95,
            comic_info: false,
            overwrite: true,
            ..ConvertOptions::default()
        },
    );
    let jpeg_entries = read_zip(&jpeg_outcome.output);
    assert_eq!(jpeg_entries[0].0, "001.jpg");
    let jpeg_image =
        image::load_from_memory_with_format(&jpeg_entries[0].1, image::ImageFormat::Jpeg).unwrap();
    assert_eq!((jpeg_image.width(), jpeg_image.height()), (3, 2));
}

#[test]
fn pdf_missing_page_aborts_conversion_and_keeps_source() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("strict-missing.pdf");
    build_pdf(
        &pdf,
        &[
            TestPage::Rgb {
                width: 1,
                height: 1,
                pixels: vec![10, 20, 30],
                predictor: false,
            },
            TestPage::NoImage,
        ],
    );

    let output = output_path_for(&pdf, Some(dir.path()));
    let options = ConvertOptions {
        output_dir: Some(dir.path().to_path_buf()),
        delete_source: true,
        ..ConvertOptions::default()
    };
    let cancel = AtomicBool::new(false);
    let error = convert_pdf(&pdf, &options, &cancel, &|_, _| {}).unwrap_err();

    assert!(error.contains("Page 2"), "unexpected error: {error}");
    assert!(pdf.exists(), "source must not be deleted on failure");
    assert!(!output.exists(), "no partial CBZ may remain");
    assert!(
        !dir.path()
            .read_dir()
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".tmp-")),
        "temporary archive must be cleaned up"
    );
}

#[test]
fn pdf_undecodable_page_aborts_conversion() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("strict-corrupt.pdf");
    build_pdf(
        &pdf,
        &[
            TestPage::Rgb {
                width: 1,
                height: 1,
                pixels: vec![1, 2, 3],
                predictor: false,
            },
            TestPage::CorruptFlate {
                width: 2,
                height: 2,
            },
        ],
    );

    let output = output_path_for(&pdf, Some(dir.path()));
    let options = ConvertOptions {
        output_dir: Some(dir.path().to_path_buf()),
        ..ConvertOptions::default()
    };
    let cancel = AtomicBool::new(false);
    let error = convert_pdf(&pdf, &options, &cancel, &|_, _| {}).unwrap_err();

    assert!(error.contains("Page 2"), "unexpected error: {error}");
    assert!(!output.exists(), "no partial CBZ may remain");
}

#[test]
fn planned_collision_outputs_convert_to_distinct_archives() {
    let dir = tempfile::tempdir().unwrap();
    let first_dir = dir.path().join("first");
    let second_dir = dir.path().join("second");
    std::fs::create_dir_all(&first_dir).unwrap();
    std::fs::create_dir_all(&second_dir).unwrap();

    let first = first_dir.join("Comic.pdf");
    let second = second_dir.join("Comic.pdf");
    for path in [&first, &second] {
        build_pdf(
            path,
            &[TestPage::Rgb {
                width: 1,
                height: 1,
                pixels: vec![10, 20, 30],
                predictor: false,
            }],
        );
    }

    let output_dir = dir.path().join("out");
    let inputs = vec![first.clone(), second.clone()];
    let planned = plan_output_paths(&inputs, Some(&output_dir));
    assert_eq!(planned[0].file_name().unwrap(), "Comic_pdf.cbz");
    assert_eq!(planned[1].file_name().unwrap(), "Comic_pdf (1).cbz");

    for (input, output) in inputs.iter().zip(planned) {
        let options = ConvertOptions {
            output_dir: Some(output_dir.clone()),
            output_override: Some(output.clone()),
            ..ConvertOptions::default()
        };
        let outcome = convert(input, &options);
        assert_eq!(outcome.output, output);
        assert!(output.is_file());
        assert!(std::fs::metadata(&output).unwrap().len() > 0);
    }
}

#[test]
fn encrypted_or_garbage_pdf_reports_an_error_instead_of_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("garbage.pdf");
    std::fs::write(&pdf, b"this is not a PDF at all").unwrap();
    assert!(probe_pdf(&pdf).is_err());
    let result = convert_pdf(
        &pdf,
        &ConvertOptions::default(),
        &AtomicBool::new(false),
        &|_, _| {},
    );
    assert!(result.is_err());
}
