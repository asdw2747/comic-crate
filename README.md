# ComicCrate v1.0.1

A lightweight, native desktop **comic archive converter and reader** written in
Rust.

- **Convert PDF and CBR → CBZ** with lossless passthrough by default.
- **Parallel encoding** with Rayon and optional PNG / WebP / JPEG modes.
- **Integrated reader** for `.cbz`, `.cbr`, and `.pdf`.
- Single self-contained executable, custom window/taskbar icon, dark UI.

## Features

### Conversion

- Reads **PDF** (`lopdf`, pure Rust) and **CBR** (bundled UnRAR, RAR4 + RAR5).
- Extracts embedded page images without resizing or downscaling.
- Default **Original / Raw Stream** mode:
  - `/DCTDecode` JPEG streams are copied byte-for-byte.
  - CBR images keep their original encoded bytes and format.
  - Flate/LZW/CCITT/JPEG 2000 pages become max-compression PNG.
- Optional output modes:
  - **Lossless PNG** (`CompressionType::Best`, adaptive filtering)
  - **Lossless WebP**
  - **Lossy WebP** (quality 70–100, default 92)
  - **High-quality JPEG** (quality 75–100, default 95)
- Pages are decoded/encoded in parallel on all CPU cores; strict page ordering is
  preserved when the CBZ is written.
- CBR entries are sorted naturally (`page2` before `page10`), renumbered
  sequentially (`001.ext`, `002.ext`, …), and existing `ComicInfo.xml` is
  preserved verbatim.
- Per-file source/output size comparison with green savings and amber/red
  increases.
- Batch queue supports drag-and-drop, folder drops, file dialogs, overwrite
  control, custom output folders, and optional source deletion.

### Reader

Open `.cbz`, `.cbr`, or `.pdf` via drag-and-drop, **Open Comic…**, or **Open in
Reader** on a finished conversion.

- Fit height (default), fit width with vertical scrolling, and two-page spreads.
- Cover-solo toggle for two-page mode.
- Left/Right, PageUp/PageDown, mouse wheel, Previous/Next buttons, and a bottom
  page scrubber.
- Ctrl + mouse wheel, `+`/`-`, and toolbar zoom; click-and-drag panning.
- Current/previous/next pages are decoded on background threads and uploaded as
  `TextureHandle`s with `TextureOptions::LINEAR`.
- Bounded texture cache for smooth high-DPI page flips.
- `F11` fullscreen (hides the top navigation and reader controls).
- PDF reading uses the same embedded-page-image engine as conversion; vector-only
  pages show a per-page error.
- `.jp2`/`.j2k` fallback pages produced by the converter are decoded with
  `hayro-jpeg2000` and displayed normally.

## Lossless extraction rules (Original mode)

| PDF image filter | Output | Behaviour |
| --- | --- | --- |
| `/DCTDecode` (JPEG) | `.jpg` | Copied byte-for-byte, never re-encoded. |
| `/JPXDecode` (JPEG 2000) | `.png` | Decoded with `hayro-jpeg2000`; raw `.jp2`/`.j2k` fallback if unsupported. |
| `/FlateDecode`, LZW, ASCII85/Hex, RunLength | `.png` | Raw samples reconstructed from PDF metadata and stored as max-compression PNG. Predictors supported. |
| `/CCITTFaxDecode` (G3/G4) | `.png` | Decoded with the pure-Rust `fax` crate. |
| `/JBIG2Decode` | — | Skipped with a warning. |

Colour spaces: DeviceGray/RGB/CMYK, ICCBased (1/3/4 components), Indexed,
CalGray/CalRGB, Lab, Separation, and single-colorant DeviceN. Soft masks become
PNG/WebP alpha; JPEG composites over white.

## Build

```sh
cargo build --release
```

Outputs:

```text
target/release/comic-crate.exe       # GUI
target/release/comic-crate-cli.exe   # console companion
```

On Linux the same files are named `comic-crate` and `comic-crate-cli` (without
the `.exe` suffix).

Requirements:

- Rust 1.95 or newer (edition 2021)
- A C++ compiler for the bundled libwebp and UnRAR builds
- Windows: MSVC Build Tools recommended. For `x86_64-pc-windows-gnu`, use a
  MinGW that links winpthread statically.

### Linux AppImage

A ready-to-run AppImage can be built on any Linux machine (x86_64 or aarch64):

```sh
bash packaging/build-appimage.sh
```

The result is written to `dist/ComicCrate-<version>-<arch>.AppImage`. See
[`packaging/README.md`](packaging/README.md) for the required Linux packages and
first-use download details.

### Icon

- `assets/icon.png` is embedded with `include_bytes!` and supplied to
  `egui::ViewportBuilder::with_icon`, so the window/taskbar icon works without
  runtime files.
- `assets/icon.ico` is embedded into the Windows executable by `build.rs`
  through `winresource` when present.

Release profile: `opt-level = 3`, `lto = true`, `codegen-units = 1`,
`strip = true`.

## Usage

1. Start `comic-crate.exe`.
2. Drag PDFs/CBRs (or a folder) into the **Converter** tab, or use **Select
   Files** / **Add folder**.
3. Choose the output mode, quality, overwrite behaviour, ComicInfo option,
   output directory, and optional source deletion.
4. Press **Convert**.
5. Click **Open in Reader** on a finished row, or switch to the **Reader** tab
   and open any CBZ/CBR/PDF.

The **About** button in the top bar shows the embedded icon, version, supported
formats, and MIT license notice.

### Console companion

```sh
target\release\comic-crate-cli.exe "Comic #1.cbr" --output "D:\Comics" --overwrite
```

Accepts PDF/CBR files and folders. Options: `--output/-o <DIR>`,
`--mode original|png|webp-lossless|webp-lossy|jpeg`,
`--webp-quality <70-100>`, `--jpeg-quality <75-100>`, `--overwrite`,
`--no-comicinfo`, `--delete-source`, `-h/--help`.

## Project layout

```text
src/
  main.rs           GUI entry point
  app.rs            tabs, converter UI, About modal, queue/event handling
  reader.rs         reader UI, background page decoding and texture cache
  worker.rs         conversion worker thread (mpsc)
  convert.rs        PDF/CBR conversion, Rayon encoding, ZIP writing
  comic_archive.rs  CBR extraction and natural sorting
  pdf_image.rs      PDF image decoding and PNG/WebP/JPEG encoding
  comic_info.rs     ComicInfo.xml generation
  model.rs          shared options/outcome/path helpers
  bin/
    comic-crate-cli.rs
tests/
  convert.rs        synthetic PDF tests
  cbr.rs            RAR3/RAR5 CBR tests
  fixtures/         tiny CC0/MIT comic fixtures
assets/
  icon.png          embedded window/about icon
  icon.ico          embedded Windows executable icon
build.rs            Win32 link libs + Windows icon/version resource
```

## Tests

```sh
cargo test
```

Coverage includes lossless Flate/16-bit/alpha extraction, JPEG passthrough, PNG
and WebP re-encoding, CCITT G4, CBR RAR3/RAR5 extraction, ComicInfo preservation,
parallel ordering/progress, reader page loading, spread math, overwrite/skip,
and malformed input handling.

## Third-party notices

ComicCrate bundles third-party components with their own license terms. The
required notices are reproduced in [`THIRD_PARTY_LICENSES.md`](THIRD_PARTY_LICENSES.md).

- RAR extraction uses the `unrar` crate and bundled UnRAR library, distributed
  under the freeware UnRAR license.
- WebP encoding uses `webp`/libwebp, distributed under the Google 3-clause BSD
  license.
- CBR test fixtures are documented in `tests/fixtures/README.md`.

## License

ComicCrate is released under the **MIT License**. See [`LICENSE`](LICENSE).
