//! Pure-Rust extraction and (re-)encoding of embedded PDF images.
//!
//! * In [`ImageOutputMode::Original`], `DCTDecode` (JPEG) streams are copied
//!   byte-for-byte; everything else is decoded and stored as max-compression PNG.
//! * In the re-encoding modes, every source (JPEG, JPEG 2000, CCITT, raw Flate)
//!   is decoded to pixels first and then encoded as PNG/WebP/JPEG at the exact
//!   original pixel dimensions.
//! * JPEG 2000 is decoded with `hayro-jpeg2000`; CCITT G3/G4 with `fax`.

use crate::model::ImageOutputMode;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::{DynamicImage, ExtendedColorType, ImageEncoder};
use lopdf::{Dictionary, Document, Object, Stream};

/// Refuse absurd page dimensions before allocating anything.
const MAX_PIXELS: u64 = 100_000_000;
/// Hard ceiling for one decoded stream (~512 MiB).
const MAX_DECODED_BYTES: usize = 512 * 1024 * 1024;
/// Hard ceiling for one rendered RGBA/RGB16 buffer (~768 MiB).
const MAX_OUTPUT_BYTES: usize = 768 * 1024 * 1024;

/// Settings for the final page encoding pass.
#[derive(Clone, Copy, Debug)]
pub struct EncodeOptions {
    pub mode: ImageOutputMode,
    pub webp_quality: u8,
    pub jpeg_quality: u8,
}

/// A page image, ready to be placed in the CBZ archive.
#[derive(Debug)]
pub struct EncodedImage {
    pub width: u32,
    pub height: u32,
    /// `jpg`, `png`, `webp`, `jp2` or `j2k`.
    pub extension: &'static str,
    pub bytes: Vec<u8>,
    pub warnings: Vec<String>,
}

/// A fully decoded page for the integrated reader.
pub struct RasterPage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Decoded pixels in a PNG-compatible layout.
struct RenderedImage {
    width: u32,
    height: u32,
    color_type: ExtendedColorType,
    /// Native-endian for 16-bit samples.
    data: Vec<u8>,
    warnings: Vec<String>,
}

/// Extract and encode one image XObject according to `options`.
pub fn encode_page(
    stream: &Stream,
    doc: &Document,
    options: &EncodeOptions,
) -> Result<EncodedImage, String> {
    let dict = &stream.dict;

    let subtype = dict_name(dict, b"Subtype", doc).unwrap_or_default();
    if subtype != "Image" {
        return Err(format!("XObject is /{subtype}, not an image"));
    }

    let width = dict_number(dict, b"Width", doc)
        .filter(|n| *n > 0.0)
        .ok_or_else(|| "image is missing a valid /Width".to_owned())?;
    let height = dict_number(dict, b"Height", doc)
        .filter(|n| *n > 0.0)
        .ok_or_else(|| "image is missing a valid /Height".to_owned())?;
    let width_u32 = width.round() as u32;
    let height_u32 = height.round() as u32;
    if u64::from(width_u32).saturating_mul(u64::from(height_u32)) > MAX_PIXELS {
        return Err(format!("image is too large ({width_u32} x {height_u32})"));
    }

    let filters = filters_of(stream, doc);
    let warnings = Vec::new();

    // CCITT Group 3/4 fax data is bi-level; decode it to grayscale pixels.
    if let Some(index) = filters.iter().position(|f| f == b"CCITTFaxDecode") {
        if index + 1 != filters.len() {
            return Err(format!(
                "unsupported image filter chain: {}",
                display_filters(&filters)
            ));
        }
        if width_u32 > u32::from(u16::MAX) || height_u32 > u32::from(u16::MAX) {
            return Err("CCITT image dimensions exceed 65535 pixels".to_owned());
        }
        let payload = decode_with_filters(stream, &filters[..index], MAX_DECODED_BYTES, doc)?;
        let rendered = decode_ccitt(stream, doc, &payload, width_u32, height_u32)?;
        return encode_rendered(rendered, options);
    }

    // JPEG / JPEG 2000 payloads are already complete image files.
    if let Some(codec_index) = filters
        .iter()
        .position(|f| f == b"DCTDecode" || f == b"JPXDecode")
    {
        let codec = filters[codec_index].clone();
        if codec_index + 1 != filters.len() {
            return Err(format!(
                "unsupported image filter chain: {}",
                display_filters(&filters)
            ));
        }

        let payload = decode_with_filters(stream, &filters[..codec_index], MAX_DECODED_BYTES, doc)?;
        if payload.is_empty() {
            return Err("embedded image stream is empty".to_owned());
        }

        if codec == b"DCTDecode" {
            if options.mode == ImageOutputMode::Original {
                return Ok(EncodedImage {
                    width: width_u32,
                    height: height_u32,
                    extension: "jpg",
                    bytes: payload,
                    warnings,
                });
            }
            let rendered = decode_jpeg(&payload)?;
            return encode_rendered(rendered, options);
        }

        // JPEG 2000.
        return match decode_jpx_image(&payload) {
            Ok(rendered) => encode_rendered(rendered, options),
            Err(err) if options.mode == ImageOutputMode::Original => {
                let mut warnings = warnings;
                warnings.push(format!(
                    "JPEG 2000 decoding failed ({err}); storing the original codestream unchanged."
                ));
                let extension = if is_jp2(&payload) { "jp2" } else { "j2k" };
                Ok(EncodedImage {
                    width: width_u32,
                    height: height_u32,
                    extension,
                    bytes: payload,
                    warnings,
                })
            }
            Err(err) => Err(format!("JPEG 2000 decoding failed: {err}")),
        };
    }

    let raw = decode_pixels(stream, doc, false)?;
    let mut warnings = warnings;
    let alpha = decode_smask(dict, doc, &mut warnings);
    let rendered = render_image(&raw, alpha.as_ref(), warnings)?;
    encode_rendered(rendered, options)
}

/// Encode a standalone image file (for example a CBR page) according to the
/// selected output mode. Original mode preserves the encoded bytes.
pub fn encode_image_file(bytes: &[u8], options: &EncodeOptions) -> Result<EncodedImage, String> {
    if options.mode == ImageOutputMode::Original {
        return passthrough_image(bytes);
    }

    if bytes.is_empty() {
        return Err("image file is empty".to_owned());
    }

    if is_jp2(bytes) || is_j2k(bytes) {
        return encode_rendered(decode_jpx_image(bytes)?, options);
    }

    // Read only the header first: a malformed archive can claim an enormous
    // image, and we must reject it before `load_from_memory` allocates pixels.
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("Could not read image dimensions: {error}"))?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|error| format!("Could not read image dimensions: {error}"))?;
    if width == 0 || height == 0 {
        return Err("image has zero dimensions".to_owned());
    }
    if u64::from(width).saturating_mul(u64::from(height)) > MAX_PIXELS {
        return Err(format!("image is too large ({width} x {height})"));
    }

    let image = image::load_from_memory(bytes)
        .map_err(|error| format!("Could not decode image: {error}"))?;
    encode_rendered(dynamic_to_rendered(image), options)
}

/// Keep an already-encoded image exactly as it is while reading its format and
/// dimensions for logging/validation.
pub fn passthrough_image(bytes: &[u8]) -> Result<EncodedImage, String> {
    if bytes.is_empty() {
        return Err("image file is empty".to_owned());
    }

    if is_jp2(bytes) || is_j2k(bytes) {
        let image = hayro_jpeg2000::Image::new(bytes, &hayro_jpeg2000::DecodeSettings::default())
            .map_err(|error| format!("Could not read JPEG 2000 image: {error}"))?;
        let (width, height) = (image.width(), image.height());
        if width == 0 || height == 0 {
            return Err("image has zero dimensions".to_owned());
        }
        if u64::from(width).saturating_mul(u64::from(height)) > MAX_PIXELS {
            return Err(format!("image is too large ({width} x {height})"));
        }
        return Ok(EncodedImage {
            width,
            height,
            extension: if is_jp2(bytes) { "jp2" } else { "j2k" },
            bytes: bytes.to_vec(),
            warnings: Vec::new(),
        });
    }

    let format = image::guess_format(bytes)
        .map_err(|error| format!("Unrecognised image format: {error}"))?;
    let extension = match format {
        image::ImageFormat::Png => "png",
        image::ImageFormat::Jpeg => "jpg",
        image::ImageFormat::WebP => "webp",
        image::ImageFormat::Bmp => "bmp",
        other => {
            return Err(format!(
                "unsupported image format inside archive: {other:?}"
            ));
        }
    };

    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("Could not read image dimensions: {error}"))?;
    let (width, height) = reader
        .into_dimensions()
        .map_err(|error| format!("Could not read image dimensions: {error}"))?;
    if width == 0 || height == 0 {
        return Err("image has zero dimensions".to_owned());
    }
    if u64::from(width).saturating_mul(u64::from(height)) > MAX_PIXELS {
        return Err(format!("image is too large ({width} x {height})"));
    }

    Ok(EncodedImage {
        width,
        height,
        extension,
        bytes: bytes.to_vec(),
        warnings: Vec::new(),
    })
}

/// Decode one PDF image XObject to RGBA for the integrated reader.
pub fn decode_page_rgba(stream: &Stream, doc: &Document) -> Result<RasterPage, String> {
    let rendered = decode_page_rendered(stream, doc)?;
    let rgba = rendered_to_rgba8(&rendered)?;
    Ok(RasterPage {
        width: rendered.width,
        height: rendered.height,
        rgba,
    })
}

/// True when `bytes` is a JP2 container or a raw JPEG 2000 codestream.
pub fn is_jpeg2000(bytes: &[u8]) -> bool {
    is_jp2(bytes) || is_j2k(bytes)
}

/// Decode a JP2/J2K fallback page produced by the converter into RGBA so the
/// reader can always display ComicCrate's own output.
pub fn decode_jp2_rgba(bytes: &[u8]) -> Result<RasterPage, String> {
    let rendered = decode_jpx_image(bytes)?;
    let rgba = rendered_to_rgba8(&rendered)?;
    Ok(RasterPage {
        width: rendered.width,
        height: rendered.height,
        rgba,
    })
}

/// Decode a PDF page image fully to pixels, applying transport filters and any
/// soft mask. Unlike `encode_page`, this never returns a raw JPEG/JP2 payload.
fn decode_page_rendered(stream: &Stream, doc: &Document) -> Result<RenderedImage, String> {
    let dict = &stream.dict;

    let subtype = dict_name(dict, b"Subtype", doc).unwrap_or_default();
    if subtype != "Image" {
        return Err(format!("XObject is /{subtype}, not an image"));
    }

    let width = dict_number(dict, b"Width", doc)
        .filter(|n| *n > 0.0)
        .ok_or_else(|| "image is missing a valid /Width".to_owned())?
        .round() as u32;
    let height = dict_number(dict, b"Height", doc)
        .filter(|n| *n > 0.0)
        .ok_or_else(|| "image is missing a valid /Height".to_owned())?
        .round() as u32;
    if u64::from(width).saturating_mul(u64::from(height)) > MAX_PIXELS {
        return Err(format!("image is too large ({width} x {height})"));
    }

    let filters = filters_of(stream, doc);

    if let Some(index) = filters
        .iter()
        .position(|filter| filter == b"CCITTFaxDecode")
    {
        if index + 1 != filters.len() {
            return Err(format!(
                "unsupported image filter chain: {}",
                display_filters(&filters)
            ));
        }
        if width > u32::from(u16::MAX) || height > u32::from(u16::MAX) {
            return Err("CCITT image dimensions exceed 65535 pixels".to_owned());
        }
        let payload = decode_with_filters(stream, &filters[..index], MAX_DECODED_BYTES, doc)?;
        return decode_ccitt(stream, doc, &payload, width, height);
    }

    if let Some(codec_index) = filters
        .iter()
        .position(|filter| filter == b"DCTDecode" || filter == b"JPXDecode")
    {
        let codec = filters[codec_index].clone();
        if codec_index + 1 != filters.len() {
            return Err(format!(
                "unsupported image filter chain: {}",
                display_filters(&filters)
            ));
        }
        let payload = decode_with_filters(stream, &filters[..codec_index], MAX_DECODED_BYTES, doc)?;
        if payload.is_empty() {
            return Err("embedded image stream is empty".to_owned());
        }
        if codec == b"DCTDecode" {
            return decode_jpeg(&payload);
        }
        return decode_jpx_image(&payload);
    }

    let raw = decode_pixels(stream, doc, false)?;
    let mut warnings = Vec::new();
    let alpha = decode_smask(dict, doc, &mut warnings);
    render_image(&raw, alpha.as_ref(), warnings)
}

fn encode_rendered(
    rendered: RenderedImage,
    options: &EncodeOptions,
) -> Result<EncodedImage, String> {
    let (extension, bytes) = match options.mode {
        ImageOutputMode::Original | ImageOutputMode::PngBest => ("png", encode_png(&rendered)?),
        ImageOutputMode::WebpLossless => ("webp", encode_webp(&rendered, None)?),
        ImageOutputMode::WebpLossy => (
            "webp",
            encode_webp(&rendered, Some(options.webp_quality.clamp(1, 100)))?,
        ),
        ImageOutputMode::Jpeg => (
            "jpg",
            encode_jpeg(&rendered, options.jpeg_quality.clamp(1, 100))?,
        ),
    };

    Ok(EncodedImage {
        width: rendered.width,
        height: rendered.height,
        extension,
        bytes,
        warnings: rendered.warnings,
    })
}

fn decode_jpeg(payload: &[u8]) -> Result<RenderedImage, String> {
    let image = image::load_from_memory_with_format(payload, image::ImageFormat::Jpeg)
        .map_err(|err| format!("JPEG decoding failed: {err}"))?;
    if u64::from(image.width()).saturating_mul(u64::from(image.height())) > MAX_PIXELS {
        return Err(format!(
            "decoded JPEG is too large ({} x {})",
            image.width(),
            image.height()
        ));
    }
    Ok(dynamic_to_rendered(image))
}

fn dynamic_to_rendered(image: DynamicImage) -> RenderedImage {
    let width = image.width();
    let height = image.height();
    let (color_type, data) = match image {
        DynamicImage::ImageLuma8(buffer) => (ExtendedColorType::L8, buffer.into_raw()),
        DynamicImage::ImageLumaA8(buffer) => (ExtendedColorType::La8, buffer.into_raw()),
        DynamicImage::ImageRgb8(buffer) => (ExtendedColorType::Rgb8, buffer.into_raw()),
        DynamicImage::ImageRgba8(buffer) => (ExtendedColorType::Rgba8, buffer.into_raw()),
        DynamicImage::ImageLuma16(buffer) => (
            ExtendedColorType::L16,
            u16_buffer_to_bytes(buffer.into_raw()),
        ),
        DynamicImage::ImageLumaA16(buffer) => (
            ExtendedColorType::La16,
            u16_buffer_to_bytes(buffer.into_raw()),
        ),
        DynamicImage::ImageRgb16(buffer) => (
            ExtendedColorType::Rgb16,
            u16_buffer_to_bytes(buffer.into_raw()),
        ),
        DynamicImage::ImageRgba16(buffer) => (
            ExtendedColorType::Rgba16,
            u16_buffer_to_bytes(buffer.into_raw()),
        ),
        other => (ExtendedColorType::Rgba8, other.to_rgba8().into_raw()),
    };
    RenderedImage {
        width,
        height,
        color_type,
        data,
        warnings: Vec::new(),
    }
}

fn u16_buffer_to_bytes(samples: Vec<u16>) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        out.extend_from_slice(&sample.to_ne_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// Raw (non-JPEG) image decoding
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IndexedBase {
    Gray,
    Rgb,
    Cmyk,
}

#[derive(Clone, Debug)]
enum ColorModel {
    Gray,
    Rgb,
    Cmyk,
    Indexed {
        base: IndexedBase,
        lookup: Vec<u8>,
        hival: usize,
    },
    Lab {
        white: [f32; 3],
    },
}

impl ColorModel {
    fn components(&self) -> usize {
        match self {
            Self::Gray => 1,
            Self::Rgb | Self::Lab { .. } => 3,
            Self::Cmyk => 4,
            Self::Indexed { .. } => 1,
        }
    }

    fn default_ranges(&self) -> Vec<(f32, f32)> {
        match self {
            Self::Gray | Self::Rgb | Self::Cmyk => {
                vec![(0.0, 1.0); self.components()]
            }
            // PDF default for Lab: L 0..100, a/b -100..100.
            Self::Lab { .. } => vec![(0.0, 100.0), (-100.0, 100.0), (-100.0, 100.0)],
            // Indexed values are used directly unless /Decode overrides them.
            Self::Indexed { hival, .. } => vec![(0.0, *hival as f32)],
        }
    }
}

struct RawPixels {
    width: u32,
    height: u32,
    bpc: u8,
    model: ColorModel,
    data: Vec<u8>,
    decode: Vec<f32>,
}

fn decode_pixels(stream: &Stream, doc: &Document, default_gray: bool) -> Result<RawPixels, String> {
    let dict = &stream.dict;

    let width = dict_number(dict, b"Width", doc)
        .filter(|n| *n > 0.0)
        .ok_or_else(|| "image is missing a valid /Width".to_owned())?
        .round() as u32;
    let height = dict_number(dict, b"Height", doc)
        .filter(|n| *n > 0.0)
        .ok_or_else(|| "image is missing a valid /Height".to_owned())?
        .round() as u32;
    if u64::from(width).saturating_mul(u64::from(height)) > MAX_PIXELS {
        return Err(format!("image is too large ({width} x {height})"));
    }

    let is_mask = dict
        .get(b"ImageMask")
        .ok()
        .and_then(|o| o.as_bool().ok())
        .unwrap_or(false);

    let bpc = dict_number(dict, b"BitsPerComponent", doc)
        .map(|n| n.round() as u8)
        .unwrap_or(if is_mask { 1 } else { 8 });
    if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
        return Err(format!("unsupported /BitsPerComponent value: {bpc}"));
    }

    let model = if is_mask {
        ColorModel::Gray
    } else {
        match dict.get(b"ColorSpace") {
            Ok(cs) => resolve_color_model(cs, doc)?,
            Err(_) if default_gray => ColorModel::Gray,
            Err(_) => return Err("image is missing its /ColorSpace".to_owned()),
        }
    };

    let components = model.components();
    let row_bytes = (width as usize)
        .checked_mul(components)
        .and_then(|n| n.checked_mul(bpc as usize))
        .map(|bits| bits.div_ceil(8))
        .ok_or_else(|| "image row size overflows".to_owned())?;
    let expected = row_bytes
        .checked_mul(height as usize)
        .ok_or_else(|| "image size overflows".to_owned())?;
    if expected > MAX_DECODED_BYTES {
        return Err(format!(
            "decoded image would be too large ({} MiB)",
            expected / (1024 * 1024)
        ));
    }

    let limit = expected.saturating_add(1024 * 1024).min(MAX_DECODED_BYTES);
    let mut data = decode_stream_data(stream, doc, limit)?;
    if data.len() < expected {
        return Err(format!(
            "decoded image is truncated (got {} bytes, need {expected})",
            data.len()
        ));
    }
    data.truncate(expected);

    let decode = read_decode_array(dict, doc);

    Ok(RawPixels {
        width,
        height,
        bpc,
        model,
        data,
        decode,
    })
}

struct AlphaMask {
    raw: RawPixels,
    ranges: Vec<(f32, f32)>,
}

impl AlphaMask {
    fn row(&self, y: usize, out_width: usize, out_height: usize) -> Vec<f32> {
        let mut samples = Vec::new();
        let mask_height = self.raw.height as usize;
        let mask_width = self.raw.width as usize;
        let my = if out_height > 1 {
            (y * mask_height / out_height).min(mask_height - 1)
        } else {
            0
        };
        read_row_into(&self.raw, my, &mut samples);

        let components = self.raw.model.components();
        let mut row = Vec::with_capacity(out_width);
        for x in 0..out_width {
            let mx = if out_width > 1 {
                (x * mask_width / out_width).min(mask_width - 1)
            } else {
                0
            };
            let s = &samples[mx * components..(mx + 1) * components];
            let alpha = match &self.raw.model {
                ColorModel::Gray => map_component(s[0], self.raw.bpc, self.ranges[0]),
                ColorModel::Rgb => {
                    let r = map_component(s[0], self.raw.bpc, self.ranges[0]);
                    let g = map_component(s[1], self.raw.bpc, self.ranges[1]);
                    let b = map_component(s[2], self.raw.bpc, self.ranges[2]);
                    0.2126 * r + 0.7152 * g + 0.0722 * b
                }
                ColorModel::Cmyk => {
                    let c = map_component(s[0], self.raw.bpc, self.ranges[0]);
                    let m = map_component(s[1], self.raw.bpc, self.ranges[1]);
                    let y = map_component(s[2], self.raw.bpc, self.ranges[2]);
                    let k = map_component(s[3], self.raw.bpc, self.ranges[3]);
                    1.0 - (0.3 * c + 0.59 * m + 0.11 * y) * (1.0 - k)
                }
                _ => 1.0,
            };
            row.push(alpha.clamp(0.0, 1.0));
        }
        row
    }
}

fn decode_smask(
    dict: &Dictionary,
    doc: &Document,
    warnings: &mut Vec<String>,
) -> Option<AlphaMask> {
    let object = dict.get(b"SMask").ok()?;
    let object = match deref(doc, object) {
        Ok(o) => o,
        Err(_) => return None,
    };
    let stream = match object.as_stream() {
        Ok(s) => s,
        Err(_) => return None,
    };

    match decode_pixels(stream, doc, true) {
        Ok(raw) => {
            if !matches!(raw.model, ColorModel::Gray) {
                warnings.push(
                    "Soft mask is not grayscale; its luminance was used as the alpha channel."
                        .to_owned(),
                );
            }
            let ranges = ranges_for(&raw.model, &raw.decode);
            Some(AlphaMask { raw, ranges })
        }
        Err(err) => {
            warnings.push(format!("Soft mask ignored: {err}"));
            None
        }
    }
}

fn ranges_for(model: &ColorModel, decode: &[f32]) -> Vec<(f32, f32)> {
    let mut ranges = model.default_ranges();
    if decode.len() >= ranges.len() * 2 {
        for (i, range) in ranges.iter_mut().enumerate() {
            range.0 = decode[i * 2];
            range.1 = decode[i * 2 + 1];
        }
    }
    ranges
}

fn render_image(
    raw: &RawPixels,
    alpha: Option<&AlphaMask>,
    warnings: Vec<String>,
) -> Result<RenderedImage, String> {
    let wide = raw.bpc == 16 && matches!(raw.model, ColorModel::Gray | ColorModel::Rgb);
    let ranges = ranges_for(&raw.model, &raw.decode);

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Gray,
        Rgb,
    }

    let kind = match &raw.model {
        ColorModel::Gray => Kind::Gray,
        ColorModel::Indexed {
            base: IndexedBase::Gray,
            ..
        } => Kind::Gray,
        _ => Kind::Rgb,
    };

    let has_alpha = alpha.is_some();
    let channels = match kind {
        Kind::Gray => 1,
        Kind::Rgb => 3,
    } + usize::from(has_alpha);
    let sample_bytes = if wide { 2 } else { 1 };

    let width = raw.width as usize;
    let height = raw.height as usize;
    let output_len = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(channels))
        .and_then(|n| n.checked_mul(sample_bytes))
        .ok_or_else(|| "output image size overflows".to_owned())?;
    if output_len > MAX_OUTPUT_BYTES {
        return Err(format!(
            "rendered image would be too large ({} MiB)",
            output_len / (1024 * 1024)
        ));
    }

    let color_type = match (wide, kind, has_alpha) {
        (false, Kind::Gray, false) => ExtendedColorType::L8,
        (false, Kind::Gray, true) => ExtendedColorType::La8,
        (false, Kind::Rgb, false) => ExtendedColorType::Rgb8,
        (false, Kind::Rgb, true) => ExtendedColorType::Rgba8,
        (true, Kind::Gray, false) => ExtendedColorType::L16,
        (true, Kind::Gray, true) => ExtendedColorType::La16,
        (true, Kind::Rgb, false) => ExtendedColorType::Rgb16,
        (true, Kind::Rgb, true) => ExtendedColorType::Rgba16,
    };

    let components = raw.model.components();
    let mut output = Vec::with_capacity(output_len);
    let mut samples = Vec::with_capacity(width * components);

    for y in 0..height {
        read_row_into(raw, y, &mut samples);
        let alpha_row = alpha.map(|a| a.row(y, width, height));

        for x in 0..width {
            let s = &samples[x * components..(x + 1) * components];
            match &raw.model {
                ColorModel::Gray => {
                    push_sample(&mut output, map_component(s[0], raw.bpc, ranges[0]), wide);
                }
                ColorModel::Rgb => {
                    for (c, range) in ranges.iter().enumerate() {
                        push_sample(&mut output, map_component(s[c], raw.bpc, *range), wide);
                    }
                }
                ColorModel::Cmyk => {
                    let c = map_component(s[0], raw.bpc, ranges[0]).clamp(0.0, 1.0);
                    let m = map_component(s[1], raw.bpc, ranges[1]).clamp(0.0, 1.0);
                    let yv = map_component(s[2], raw.bpc, ranges[2]).clamp(0.0, 1.0);
                    let k = map_component(s[3], raw.bpc, ranges[3]).clamp(0.0, 1.0);
                    push_sample(&mut output, (1.0 - c) * (1.0 - k), wide);
                    push_sample(&mut output, (1.0 - m) * (1.0 - k), wide);
                    push_sample(&mut output, (1.0 - yv) * (1.0 - k), wide);
                }
                ColorModel::Indexed {
                    base,
                    lookup,
                    hival,
                } => {
                    let index = if raw.decode.is_empty() {
                        s[0] as usize
                    } else {
                        map_component(s[0], raw.bpc, ranges[0])
                            .round()
                            .clamp(0.0, *hival as f32) as usize
                    };
                    let index = index.min(*hival);
                    let base_components = match base {
                        IndexedBase::Gray => 1,
                        IndexedBase::Rgb => 3,
                        IndexedBase::Cmyk => 4,
                    };
                    let start = index * base_components;
                    let entry = lookup.get(start..start + base_components);
                    match (base, entry) {
                        (IndexedBase::Gray, Some(e)) => {
                            push_sample(&mut output, e[0] as f32 / 255.0, wide);
                        }
                        (IndexedBase::Rgb, Some(e)) => {
                            for v in e {
                                push_sample(&mut output, *v as f32 / 255.0, wide);
                            }
                        }
                        (IndexedBase::Cmyk, Some(e)) => {
                            let c = e[0] as f32 / 255.0;
                            let m = e[1] as f32 / 255.0;
                            let yv = e[2] as f32 / 255.0;
                            let k = e[3] as f32 / 255.0;
                            push_sample(&mut output, (1.0 - c) * (1.0 - k), wide);
                            push_sample(&mut output, (1.0 - m) * (1.0 - k), wide);
                            push_sample(&mut output, (1.0 - yv) * (1.0 - k), wide);
                        }
                        _ => {
                            // Malformed palette: emit black rather than panic.
                            push_sample(&mut output, 0.0, wide);
                            if *base != IndexedBase::Gray {
                                push_sample(&mut output, 0.0, wide);
                                push_sample(&mut output, 0.0, wide);
                            }
                        }
                    }
                }
                ColorModel::Lab { white } => {
                    let l = map_component(s[0], raw.bpc, ranges[0]);
                    let a = map_component(s[1], raw.bpc, ranges[1]);
                    let b = map_component(s[2], raw.bpc, ranges[2]);
                    let [r, g, bl] = lab_to_srgb(l, a, b, *white);
                    push_sample(&mut output, r, wide);
                    push_sample(&mut output, g, wide);
                    push_sample(&mut output, bl, wide);
                }
            }

            if let Some(row) = &alpha_row {
                push_sample(&mut output, row[x], wide);
            }
        }
    }

    debug_assert_eq!(output.len(), output_len);

    Ok(RenderedImage {
        width: raw.width,
        height: raw.height,
        color_type,
        data: output,
        warnings,
    })
}

#[inline]
fn push_sample(output: &mut Vec<u8>, value: f32, wide: bool) {
    let v = value.clamp(0.0, 1.0);
    if wide {
        let sample = (v * 65535.0).round() as u16;
        output.extend_from_slice(&sample.to_ne_bytes());
    } else {
        output.push((v * 255.0).round() as u8);
    }
}

#[inline]
fn map_component(sample: u32, bpc: u8, range: (f32, f32)) -> f32 {
    let max = ((1u32 << bpc) - 1) as f32;
    let normalized = sample as f32 / max;
    range.0 + normalized * (range.1 - range.0)
}

fn read_row_into(raw: &RawPixels, y: usize, out: &mut Vec<u32>) {
    let components = raw.model.components();
    let count = raw.width as usize * components;
    let row_bytes = (raw.width as usize * components * raw.bpc as usize).div_ceil(8);
    let start = y * row_bytes;

    out.clear();
    match raw.bpc {
        8 => {
            out.extend(raw.data[start..start + count].iter().map(|&b| u32::from(b)));
        }
        16 => {
            for bytes in raw.data[start..start + count * 2].as_chunks::<2>().0 {
                out.push(u32::from(u16::from_be_bytes([bytes[0], bytes[1]])));
            }
        }
        bpc => {
            let bpc = bpc as usize;
            let mask = (1u32 << bpc) - 1;
            for i in 0..count {
                let bit = i * bpc;
                let byte = raw.data[start + bit / 8];
                let shift = 8 - bpc - (bit % 8);
                out.push((u32::from(byte) >> shift) & mask);
            }
        }
    }
}

fn color_layout(color_type: ExtendedColorType) -> Result<(usize, usize), String> {
    let channels = match color_type {
        ExtendedColorType::L8 | ExtendedColorType::L16 => 1,
        ExtendedColorType::La8 | ExtendedColorType::La16 => 2,
        ExtendedColorType::Rgb8 | ExtendedColorType::Rgb16 => 3,
        ExtendedColorType::Rgba8 | ExtendedColorType::Rgba16 => 4,
        other => return Err(format!("unsupported pixel colour type: {other:?}")),
    };
    let sample_bytes = if matches!(
        color_type,
        ExtendedColorType::L16
            | ExtendedColorType::La16
            | ExtendedColorType::Rgb16
            | ExtendedColorType::Rgba16
    ) {
        2
    } else {
        1
    };
    Ok((channels, sample_bytes))
}

fn image_buffer_len(
    width: u32,
    height: u32,
    channels: usize,
    sample_bytes: usize,
) -> Result<usize, String> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(channels))
        .and_then(|n| n.checked_mul(sample_bytes))
        .ok_or_else(|| "image buffer size overflows".to_owned())
}

fn encode_png(rendered: &RenderedImage) -> Result<Vec<u8>, String> {
    let (channels, sample_bytes) = color_layout(rendered.color_type)?;
    let expected = image_buffer_len(rendered.width, rendered.height, channels, sample_bytes)?;
    if rendered.data.len() != expected {
        return Err("internal error: PNG buffer size mismatch".to_owned());
    }

    let mut output = Vec::new();
    PngEncoder::new_with_quality(&mut output, CompressionType::Best, PngFilter::Adaptive)
        .write_image(
            &rendered.data,
            rendered.width,
            rendered.height,
            rendered.color_type,
        )
        .map_err(|err| format!("PNG encoding failed: {err}"))?;
    Ok(output)
}

fn encode_webp(rendered: &RenderedImage, quality: Option<u8>) -> Result<Vec<u8>, String> {
    if rendered.width > 16_383 || rendered.height > 16_383 {
        return Err(format!(
            "WebP supports at most 16383 pixels per side (image is {} x {})",
            rendered.width, rendered.height
        ));
    }
    let rgba = rendered_to_rgba8(rendered)?;
    let encoder = webp::Encoder::from_rgba(&rgba, rendered.width, rendered.height);
    let memory = match quality {
        Some(quality) => encoder.encode(f32::from(quality)),
        None => encoder.encode_lossless(),
    };
    let bytes = memory.to_vec();
    if bytes.is_empty() {
        return Err("WebP encoding failed".to_owned());
    }
    Ok(bytes)
}

fn encode_jpeg(rendered: &RenderedImage, quality: u8) -> Result<Vec<u8>, String> {
    if rendered.width > 65_535 || rendered.height > 65_535 {
        return Err(format!(
            "JPEG supports at most 65535 pixels per side (image is {} x {})",
            rendered.width, rendered.height
        ));
    }
    let rgb = rendered_to_rgb8(rendered)?;
    let mut output = Vec::new();
    JpegEncoder::new_with_quality(&mut output, quality)
        .write_image(
            &rgb,
            rendered.width,
            rendered.height,
            ExtendedColorType::Rgb8,
        )
        .map_err(|err| format!("JPEG encoding failed: {err}"))?;
    Ok(output)
}

fn rendered_to_rgba8(rendered: &RenderedImage) -> Result<Vec<u8>, String> {
    let (channels, sample_bytes) = color_layout(rendered.color_type)?;
    let expected = image_buffer_len(rendered.width, rendered.height, channels, sample_bytes)?;
    if rendered.data.len() != expected {
        return Err("internal error: pixel buffer size mismatch".to_owned());
    }
    let pixels = rendered.width as usize * rendered.height as usize;
    let mut output = Vec::with_capacity(pixels * 4);
    let data = &rendered.data;

    if sample_bytes == 1 {
        match rendered.color_type {
            ExtendedColorType::L8 => {
                for &gray in data {
                    output.extend_from_slice(&[gray, gray, gray, 255]);
                }
            }
            ExtendedColorType::La8 => {
                for pixel in data.as_chunks::<2>().0 {
                    output.extend_from_slice(&[pixel[0], pixel[0], pixel[0], pixel[1]]);
                }
            }
            ExtendedColorType::Rgb8 => {
                for pixel in data.as_chunks::<3>().0 {
                    output.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
                }
            }
            ExtendedColorType::Rgba8 => output.extend_from_slice(data),
            _ => unreachable!(),
        }
    } else {
        match rendered.color_type {
            ExtendedColorType::L16 => {
                for sample in data.as_chunks::<2>().0 {
                    let gray = u16_to_u8(u16::from_ne_bytes(*sample));
                    output.extend_from_slice(&[gray, gray, gray, 255]);
                }
            }
            ExtendedColorType::La16 => {
                for pixel in data.as_chunks::<4>().0 {
                    let gray = u16_to_u8(u16::from_ne_bytes([pixel[0], pixel[1]]));
                    let alpha = u16_to_u8(u16::from_ne_bytes([pixel[2], pixel[3]]));
                    output.extend_from_slice(&[gray, gray, gray, alpha]);
                }
            }
            ExtendedColorType::Rgb16 => {
                for pixel in data.as_chunks::<6>().0 {
                    let r = u16_to_u8(u16::from_ne_bytes([pixel[0], pixel[1]]));
                    let g = u16_to_u8(u16::from_ne_bytes([pixel[2], pixel[3]]));
                    let b = u16_to_u8(u16::from_ne_bytes([pixel[4], pixel[5]]));
                    output.extend_from_slice(&[r, g, b, 255]);
                }
            }
            ExtendedColorType::Rgba16 => {
                for pixel in data.as_chunks::<8>().0 {
                    let r = u16_to_u8(u16::from_ne_bytes([pixel[0], pixel[1]]));
                    let g = u16_to_u8(u16::from_ne_bytes([pixel[2], pixel[3]]));
                    let b = u16_to_u8(u16::from_ne_bytes([pixel[4], pixel[5]]));
                    let a = u16_to_u8(u16::from_ne_bytes([pixel[6], pixel[7]]));
                    output.extend_from_slice(&[r, g, b, a]);
                }
            }
            _ => unreachable!(),
        }
    }

    Ok(output)
}

fn rendered_to_rgb8(rendered: &RenderedImage) -> Result<Vec<u8>, String> {
    let rgba = rendered_to_rgba8(rendered)?;
    let mut output = Vec::with_capacity(rgba.len() / 4 * 3);
    for pixel in rgba.as_chunks::<4>().0 {
        let alpha = u32::from(pixel[3]);
        if alpha == 255 {
            output.extend_from_slice(&pixel[..3]);
        } else {
            // JPEG has no alpha channel: composite transparent pixels over white.
            for &channel in &pixel[..3] {
                let value = (u32::from(channel) * alpha + 255 * (255 - alpha) + 127) / 255;
                output.push(value as u8);
            }
        }
    }
    Ok(output)
}

#[inline]
fn u16_to_u8(value: u16) -> u8 {
    ((u32::from(value) + 128) / 257) as u8
}

// ---------------------------------------------------------------------------
// CCITT Group 3/4 fax
// ---------------------------------------------------------------------------

fn decode_ccitt(
    stream: &Stream,
    doc: &Document,
    payload: &[u8],
    width: u32,
    height: u32,
) -> Result<RenderedImage, String> {
    let (k, columns, rows) = ccitt_params(stream, doc, width, height);
    if rows == 0 {
        return Err("CCITT image has zero rows".to_owned());
    }

    let w = columns as usize;
    let h = rows as usize;
    let mut pixels = vec![255u8; w * h];
    let mut emitted = 0usize;
    let mut warnings = Vec::new();

    let result = if k < 0 {
        fax::decoder::decode_g4(
            payload.iter().copied(),
            columns as u16,
            Some(rows as u16),
            |line| ccitt_emit(line, &mut pixels, &mut emitted, w, columns as u16),
        )
    } else if k == 0 {
        fax::decoder::decode_g3(payload.iter().copied(), |line| {
            ccitt_emit(line, &mut pixels, &mut emitted, w, columns as u16)
        })
    } else {
        return Err("CCITT Group 3 2-D encoding is not supported".to_owned());
    };

    if emitted == 0 {
        return Err("CCITT fax data contained no scan lines".to_owned());
    }
    if result.is_none() && emitted < h {
        warnings.push(format!(
            "CCITT stream ended after {emitted} of {h} rows; missing rows were filled white."
        ));
    }

    // /Decode [1 0] inverts the image.
    let decode = read_decode_array(&stream.dict, doc);
    if decode.first().is_some_and(|value| *value > 0.5) {
        for pixel in &mut pixels {
            *pixel = 255 - *pixel;
        }
    }

    Ok(RenderedImage {
        width: columns,
        height: rows,
        color_type: ExtendedColorType::L8,
        data: pixels,
        warnings,
    })
}

fn ccitt_emit(line: &[u16], pixels: &mut [u8], emitted: &mut usize, width: usize, columns: u16) {
    if *emitted < pixels.len() / width {
        let base = *emitted * width;
        for (x, color) in fax::decoder::pels(line, columns).enumerate().take(width) {
            if color == fax::Color::Black {
                pixels[base + x] = 0;
            }
        }
    }
    *emitted += 1;
}

fn ccitt_params(stream: &Stream, doc: &Document, width: u32, height: u32) -> (i64, u32, u32) {
    let mut k = 0i64;
    let mut columns = width;
    let mut rows = height;

    if let Ok(object) = stream.dict.get(b"DecodeParms") {
        if let Ok(object) = deref(doc, object) {
            let params = match object {
                Object::Dictionary(dict) => Some(dict),
                Object::Array(items) => items
                    .iter()
                    .find_map(|item| deref(doc, item).ok().and_then(|item| item.as_dict().ok())),
                _ => None,
            };
            if let Some(params) = params {
                let value = |key: &[u8]| {
                    params
                        .get(key)
                        .ok()
                        .and_then(|object| deref(doc, object).ok())
                        .and_then(|object| object.as_i64().ok())
                };
                k = value(b"K").unwrap_or(0);
                columns = value(b"Columns")
                    .filter(|value| *value > 0)
                    .map(|value| value as u32)
                    .unwrap_or(width);
                rows = value(b"Rows")
                    .filter(|value| *value > 0)
                    .map(|value| value as u32)
                    .unwrap_or(height);
            }
        }
    }

    (
        k,
        columns.clamp(1, u32::from(u16::MAX)),
        rows.clamp(1, u32::from(u16::MAX)),
    )
}

// ---------------------------------------------------------------------------
// JPEG 2000
// ---------------------------------------------------------------------------

fn decode_jpx_image(payload: &[u8]) -> Result<RenderedImage, String> {
    use hayro_jpeg2000::{ColorSpace, DecodeSettings, DecoderContext, Image};

    let image = Image::new(payload, &DecodeSettings::default()).map_err(|err| err.to_string())?;
    let width = image.width();
    let height = image.height();
    if u64::from(width).saturating_mul(u64::from(height)) > MAX_PIXELS {
        return Err(format!("JPEG 2000 image is too large ({width} x {height})"));
    }
    let has_alpha = image.has_alpha();
    let color_space = image.color_space().clone();

    let mut context = DecoderContext::default();
    let decoded = image.decode(&mut context).map_err(|err| err.to_string())?;

    let channels = color_space.num_channels() as usize + usize::from(has_alpha);
    let component_count = decoded.components().len();
    let max_depth = decoded
        .components()
        .iter()
        .map(|component| component.bit_depth())
        .max()
        .unwrap_or(8);
    let uniform_depth = decoded
        .components()
        .iter()
        .all(|component| component.bit_depth() == max_depth);

    if max_depth > 16 {
        return Err(format!(
            "JPEG 2000 bit depth {max_depth} is higher than PNG can store"
        ));
    }

    if max_depth > 8 {
        // Preserve high-bit-depth greyscale/RGB data exactly in 16-bit output.
        // CMYK/ICC or subsampled layouts are safer as the original codestream.
        let full_resolution = decoded
            .components()
            .iter()
            .all(|component| component.samples().len() == width as usize * height as usize);
        if !uniform_depth
            || !full_resolution
            || component_count != channels
            || !matches!(&color_space, ColorSpace::Gray | ColorSpace::RGB)
        {
            return Err(format!("unsupported {max_depth}-bit JPEG 2000 layout"));
        }

        let data = components_to_png16(decoded.components(), max_depth, channels)?;
        let color_type = match (&color_space, has_alpha) {
            (ColorSpace::Gray, false) => ExtendedColorType::L16,
            (ColorSpace::Gray, true) => ExtendedColorType::La16,
            (ColorSpace::RGB, false) => ExtendedColorType::Rgb16,
            (ColorSpace::RGB, true) => ExtendedColorType::Rgba16,
            _ => unreachable!(),
        };
        return Ok(RenderedImage {
            width,
            height,
            color_type,
            data,
            warnings: Vec::new(),
        });
    }

    let data = decoded.data_u8();
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(channels))
        .ok_or_else(|| "JPEG 2000 image dimensions overflow".to_owned())?;
    if data.len() < expected {
        return Err(format!(
            "JPEG 2000 decoder returned {} bytes, expected {expected}",
            data.len()
        ));
    }

    let (color_type, pixels) = match color_space {
        ColorSpace::Gray => (
            if has_alpha {
                ExtendedColorType::La8
            } else {
                ExtendedColorType::L8
            },
            data[..expected].to_vec(),
        ),
        ColorSpace::RGB => (
            if has_alpha {
                ExtendedColorType::Rgba8
            } else {
                ExtendedColorType::Rgb8
            },
            data[..expected].to_vec(),
        ),
        ColorSpace::CMYK => (
            if has_alpha {
                ExtendedColorType::Rgba8
            } else {
                ExtendedColorType::Rgb8
            },
            cmyk_buffer_to_rgb(&data[..expected], has_alpha),
        ),
        ColorSpace::Icc { num_channels, .. } | ColorSpace::Unknown { num_channels } => {
            match num_channels {
                1 => (
                    if has_alpha {
                        ExtendedColorType::La8
                    } else {
                        ExtendedColorType::L8
                    },
                    data[..expected].to_vec(),
                ),
                3 => (
                    if has_alpha {
                        ExtendedColorType::Rgba8
                    } else {
                        ExtendedColorType::Rgb8
                    },
                    data[..expected].to_vec(),
                ),
                4 => (
                    if has_alpha {
                        ExtendedColorType::Rgba8
                    } else {
                        ExtendedColorType::Rgb8
                    },
                    cmyk_buffer_to_rgb(&data[..expected], has_alpha),
                ),
                other => {
                    return Err(format!(
                        "unsupported JPEG 2000 colour space with {other} channels"
                    ));
                }
            }
        }
    };

    Ok(RenderedImage {
        width,
        height,
        color_type,
        data: pixels,
        warnings: Vec::new(),
    })
}

fn components_to_png16(
    components: &[hayro_jpeg2000::ComponentData],
    bit_depth: u8,
    channels: usize,
) -> Result<Vec<u8>, String> {
    let sample_count = components
        .first()
        .map(|component| component.samples().len())
        .unwrap_or(0);
    let max = ((1u32 << bit_depth) - 1) as f32;
    let max_int = u64::from((1u32 << bit_depth) - 1);
    let mut out = Vec::with_capacity(sample_count * channels * 2);

    for sample_index in 0..sample_count {
        for component in components.iter().take(channels) {
            let sample = component
                .samples()
                .get(sample_index)
                .copied()
                .ok_or_else(|| "JPEG 2000 components have inconsistent lengths".to_owned())?;
            let value = sample.clamp(0.0, max).round() as u64;
            let scaled = if bit_depth == 16 {
                value
            } else {
                (value * 65_535 + max_int / 2) / max_int
            };
            out.extend_from_slice(&(scaled as u16).to_ne_bytes());
        }
    }
    Ok(out)
}

fn cmyk_buffer_to_rgb(data: &[u8], has_alpha: bool) -> Vec<u8> {
    let stride = if has_alpha { 5 } else { 4 };
    let mut out = Vec::with_capacity(data.len() / stride * if has_alpha { 4 } else { 3 });
    for pixel in data.chunks_exact(stride) {
        let c = u32::from(pixel[0]);
        let m = u32::from(pixel[1]);
        let y = u32::from(pixel[2]);
        let k = 255 - u32::from(pixel[3]);
        out.push((((255 - c) * k + 127) / 255) as u8);
        out.push((((255 - m) * k + 127) / 255) as u8);
        out.push((((255 - y) * k + 127) / 255) as u8);
        if has_alpha {
            out.push(pixel[4]);
        }
    }
    out
}

fn is_jp2(data: &[u8]) -> bool {
    data.starts_with(&[
        0x00, 0x00, 0x00, 0x0C, 0x6A, 0x50, 0x20, 0x20, 0x0D, 0x0A, 0x87, 0x0A,
    ])
}

fn is_j2k(data: &[u8]) -> bool {
    data.starts_with(&[0xFF, 0x4F, 0xFF, 0x51])
}

// ---------------------------------------------------------------------------
// PDF filter plumbing
// ---------------------------------------------------------------------------

fn filters_of(stream: &Stream, doc: &Document) -> Vec<Vec<u8>> {
    let Ok(filter) = stream.dict.get(b"Filter") else {
        return Vec::new();
    };
    let filter = deref(doc, filter).unwrap_or(filter);
    match filter {
        Object::Name(name) => vec![canonical_filter(name).to_vec()],
        Object::Array(items) => items
            .iter()
            .filter_map(|item| {
                let item = deref(doc, item).ok()?;
                let name = item.as_name().ok()?;
                Some(canonical_filter(name).to_vec())
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn canonical_filter(name: &[u8]) -> &[u8] {
    match name {
        b"Fl" => b"FlateDecode",
        b"LZW" => b"LZWDecode",
        b"DCT" => b"DCTDecode",
        b"JPX" => b"JPXDecode",
        b"CCF" => b"CCITTFaxDecode",
        b"AHx" => b"ASCIIHexDecode",
        b"A85" => b"ASCII85Decode",
        b"RL" => b"RunLengthDecode",
        other => other,
    }
}

fn display_filters(filters: &[Vec<u8>]) -> String {
    filters
        .iter()
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect::<Vec<_>>()
        .join(" -> ")
}

fn set_filters(dict: &mut Dictionary, filters: &[Vec<u8>]) {
    let objects: Vec<Object> = filters
        .iter()
        .map(|name| Object::Name(name.clone()))
        .collect();
    if objects.len() == 1 {
        if let Some(object) = objects.into_iter().next() {
            dict.set(b"Filter", object);
        }
    } else {
        dict.set(b"Filter", Object::Array(objects));
    }
}

/// Decode a stream, optionally replacing its filter chain with `filters`.
///
/// `filters` must not contain an image codec (DCT/JPX); those are handled by
/// the caller and their payloads are never modified.
fn decode_with_filters(
    stream: &Stream,
    filters: &[Vec<u8>],
    limit: usize,
    doc: &Document,
) -> Result<Vec<u8>, String> {
    if filters.is_empty() {
        if stream.content.len() > limit {
            return Err("image stream exceeds the size limit".to_owned());
        }
        return Ok(stream.content.clone());
    }

    let mut dict = stream.dict.clone();
    set_filters(&mut dict, filters);
    if let Ok(params) = stream.dict.get(b"DecodeParms") {
        let mut params = resolve_decode_parms(params, doc);
        if let Object::Array(list) = &mut params {
            list.truncate(filters.len());
        }
        if params.is_null() {
            dict.remove(b"DecodeParms");
        } else {
            dict.set(b"DecodeParms", params);
        }
    }

    Stream::new(dict, stream.content.clone())
        .decompressed_content_with_limit(limit)
        .map_err(|err| format!("could not decode image stream: {err}"))
}

fn resolve_decode_parms(object: &Object, doc: &Document) -> Object {
    let object = deref(doc, object).unwrap_or(object);
    match object {
        Object::Dictionary(dict) => Object::Dictionary(dict.clone()),
        Object::Array(items) => Object::Array(
            items
                .iter()
                .map(|item| match deref(doc, item) {
                    Ok(Object::Dictionary(dict)) => Object::Dictionary(dict.clone()),
                    _ => Object::Null,
                })
                .collect(),
        ),
        _ => Object::Null,
    }
}

fn decode_stream_data(stream: &Stream, doc: &Document, limit: usize) -> Result<Vec<u8>, String> {
    let filters = filters_of(stream, doc);
    if filters.is_empty() {
        // No /Filter (or an empty filter array): the content is already raw.
        if stream.content.len() > limit {
            return Err("image stream exceeds the size limit".to_owned());
        }
        return Ok(stream.content.clone());
    }

    if filters
        .iter()
        .any(|filter| filter == b"CCITTFaxDecode" || filter == b"JBIG2Decode")
    {
        return Err("CCITT/JBIG2 compression is not supported in this stream context".to_owned());
    }

    // Normalize filter aliases and indirect /DecodeParms before handing the
    // stream to lopdf. This keeps predictor handling correct for the (valid but
    // uncommon) case where /DecodeParms is an indirect object.
    let mut dict = stream.dict.clone();
    set_filters(&mut dict, &filters);
    if let Ok(params) = stream.dict.get(b"DecodeParms") {
        let params = resolve_decode_parms(params, doc);
        if params.is_null() {
            dict.remove(b"DecodeParms");
        } else {
            dict.set(b"DecodeParms", params);
        }
    }

    Stream::new(dict, stream.content.clone())
        .decompressed_content_with_limit(limit)
        .map_err(|err| format!("could not decode image stream: {err}"))
}

// ---------------------------------------------------------------------------
// PDF object helpers
// ---------------------------------------------------------------------------

fn deref<'a>(doc: &'a Document, object: &'a Object) -> Result<&'a Object, String> {
    doc.dereference(object)
        .map(|(_, object)| object)
        .map_err(|err| format!("invalid PDF object reference: {err}"))
}

fn dict_name(dict: &Dictionary, key: &[u8], doc: &Document) -> Option<String> {
    let object = dict.get(key).ok()?;
    let object = deref(doc, object).ok()?;
    object
        .as_name()
        .ok()
        .map(|name| String::from_utf8_lossy(name).into_owned())
}

fn dict_number(dict: &Dictionary, key: &[u8], doc: &Document) -> Option<f64> {
    let object = dict.get(key).ok()?;
    let object = deref(doc, object).ok()?;
    match object {
        Object::Integer(value) => Some(*value as f64),
        Object::Real(value) => Some(f64::from(*value)),
        _ => None,
    }
}

fn read_decode_array(dict: &Dictionary, doc: &Document) -> Vec<f32> {
    dict.get(b"Decode")
        .ok()
        .and_then(|object| deref(doc, object).ok())
        .and_then(|object| object.as_array().ok())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| match value {
                    Object::Integer(v) => Some(*v as f32),
                    Object::Real(v) => Some(*v),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn resolve_color_model(object: &Object, doc: &Document) -> Result<ColorModel, String> {
    let object = deref(doc, object)?;
    match object {
        Object::Name(name) => color_model_from_name(name),
        Object::Stream(stream) => icc_model(stream, doc),
        Object::Array(items) => {
            let family = items
                .first()
                .ok_or_else(|| "empty /ColorSpace array".to_owned())?;
            let family = deref(doc, family)?;
            let family = family
                .as_name()
                .map_err(|_| "invalid /ColorSpace family".to_owned())?;

            match family {
                b"ICCBased" => {
                    let stream_object = items
                        .get(1)
                        .ok_or_else(|| "ICCBased colour space is missing its stream".to_owned())?;
                    let stream_object = deref(doc, stream_object)?;
                    let stream = stream_object
                        .as_stream()
                        .map_err(|_| "ICCBased colour space is not a stream".to_owned())?;
                    icc_model(stream, doc)
                }
                b"Indexed" | b"I" => {
                    let base_object = items
                        .get(1)
                        .ok_or_else(|| "Indexed colour space is missing its base".to_owned())?;
                    let base_model = resolve_color_model(base_object, doc)?;
                    let base = match base_model {
                        ColorModel::Gray => IndexedBase::Gray,
                        ColorModel::Rgb => IndexedBase::Rgb,
                        ColorModel::Cmyk => IndexedBase::Cmyk,
                        _ => {
                            return Err("unsupported Indexed colour-space base".to_owned());
                        }
                    };
                    let hival = items
                        .get(2)
                        .and_then(|value| deref(doc, value).ok())
                        .and_then(|value| value.as_i64().ok())
                        .unwrap_or(255)
                        .max(0) as usize;

                    let lookup_object = items
                        .get(3)
                        .ok_or_else(|| "Indexed colour space is missing its palette".to_owned())?;
                    let lookup_object = deref(doc, lookup_object)?;
                    let lookup = match lookup_object {
                        Object::String(bytes, _) => bytes.clone(),
                        Object::Stream(stream) => stream
                            .decompressed_content_with_limit(64 * 1024 * 1024)
                            .map_err(|err| format!("could not decode Indexed palette: {err}"))?,
                        _ => return Err("invalid Indexed colour-space palette".to_owned()),
                    };
                    let base_components = match base {
                        IndexedBase::Gray => 1,
                        IndexedBase::Rgb => 3,
                        IndexedBase::Cmyk => 4,
                    };
                    if lookup.len() < base_components {
                        return Err("Indexed colour-space palette is empty".to_owned());
                    }
                    let entries = lookup.len() / base_components;
                    Ok(ColorModel::Indexed {
                        base,
                        lookup,
                        hival: hival.min(entries - 1),
                    })
                }
                b"CalRGB" => Ok(ColorModel::Rgb),
                b"CalGray" => Ok(ColorModel::Gray),
                b"Lab" => {
                    let mut white = [0.9642, 1.0, 0.8249]; // D50
                    if let Some(Ok(Object::Dictionary(params))) =
                        items.get(1).map(|object| deref(doc, object))
                    {
                        let values = params
                            .get(b"WhitePoint")
                            .ok()
                            .and_then(|object| deref(doc, object).ok())
                            .and_then(|object| object.as_array().ok());
                        if let Some(values) = values {
                            if values.len() >= 3 {
                                if let (Ok(x), Ok(y), Ok(z)) = (
                                    values[0].as_float(),
                                    values[1].as_float(),
                                    values[2].as_float(),
                                ) {
                                    if x > 0.0 && y > 0.0 && z > 0.0 {
                                        white = [x, y, z];
                                    }
                                }
                            }
                        }
                    }
                    Ok(ColorModel::Lab { white })
                }
                b"Separation" => Ok(ColorModel::Gray),
                b"DeviceN" => {
                    let names = items
                        .get(1)
                        .and_then(|value| deref(doc, value).ok())
                        .and_then(|value| value.as_array().ok());
                    match names.map(Vec::len) {
                        Some(1) => Ok(ColorModel::Gray),
                        Some(count) => Err(format!(
                            "DeviceN images with {count} colorants are not supported"
                        )),
                        None => Err("invalid DeviceN colour space".to_owned()),
                    }
                }
                b"Pattern" => Err("pattern colour spaces contain no extractable pixels".to_owned()),
                other => Err(format!(
                    "unsupported colour space /{}",
                    String::from_utf8_lossy(other)
                )),
            }
        }
        _ => Err("invalid /ColorSpace object".to_owned()),
    }
}

fn color_model_from_name(name: &[u8]) -> Result<ColorModel, String> {
    match name {
        b"DeviceGray" | b"G" | b"CalGray" => Ok(ColorModel::Gray),
        b"DeviceRGB" | b"RGB" | b"CalRGB" => Ok(ColorModel::Rgb),
        b"DeviceCMYK" | b"CMYK" => Ok(ColorModel::Cmyk),
        b"Pattern" => Err("pattern colour spaces contain no extractable pixels".to_owned()),
        other => Err(format!(
            "unsupported colour space /{}",
            String::from_utf8_lossy(other)
        )),
    }
}

fn icc_model(stream: &Stream, doc: &Document) -> Result<ColorModel, String> {
    let components = stream
        .dict
        .get_deref(b"N", doc)
        .ok()
        .and_then(|object| object.as_i64().ok())
        .unwrap_or(3);
    match components {
        1 => Ok(ColorModel::Gray),
        3 => Ok(ColorModel::Rgb),
        4 => Ok(ColorModel::Cmyk),
        other => Err(format!(
            "unsupported ICCBased colour space with {other} components"
        )),
    }
}

// ---------------------------------------------------------------------------
// Lab -> sRGB
// ---------------------------------------------------------------------------

fn lab_to_srgb(l: f32, a: f32, b: f32, white: [f32; 3]) -> [f32; 3] {
    const EPSILON: f32 = 216.0 / 24_389.0;
    const KAPPA: f32 = 24_389.0 / 27.0;

    let fy = (l + 16.0) / 116.0;
    let fx = a / 500.0 + fy;
    let fz = fy - b / 200.0;

    let xr = if fx.powi(3) > EPSILON {
        fx.powi(3)
    } else {
        (116.0 * fx - 16.0) / KAPPA
    };
    let yr = if l > KAPPA * EPSILON {
        ((l + 16.0) / 116.0).powi(3)
    } else {
        l / KAPPA
    };
    let zr = if fz.powi(3) > EPSILON {
        fz.powi(3)
    } else {
        (116.0 * fz - 16.0) / KAPPA
    };

    let x = xr * white[0];
    let y = yr * white[1];
    let z = zr * white[2];

    // D50 XYZ -> linear sRGB.
    let r = 3.133_856 * x - 1.616_867 * y - 0.490_615 * z;
    let g = -0.978_768 * x + 1.916_142 * y + 0.033_454 * z;
    let bl = 0.071_945 * x - 0.228_991 * y + 1.405_243 * z;

    [gamma(r), gamma(g), gamma(bl)]
}

fn gamma(value: f32) -> f32 {
    if value <= 0.003_130_8 {
        12.92 * value
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
    .clamp(0.0, 1.0)
}
