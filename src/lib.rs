//! Lossless PDF-to-CBZ conversion library.
//!
//! The GUI binary is intentionally a thin shell around these modules, which
//! makes the conversion engine easy to test and reuse.

pub mod comic_archive;
pub mod comic_info;
pub mod convert;
pub mod model;
pub mod pdf_image;
