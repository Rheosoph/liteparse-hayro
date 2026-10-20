//! A hayro-backed stand-in for the subset of the `pdfium` crate API that
//! liteparse uses.
//!
//! Upstream liteparse drives PDFium through the `liteparse-pdfium` wrapper
//! crate. This fork renders and extracts with [hayro] instead, a pure-Rust PDF
//! interpreter, so no native library is needed on any target (including
//! WASM). Rather than rewriting every call site, this module reproduces the
//! wrapper's types and methods on top of hayro: files that used `pdfium::...`
//! declare `use crate::pdfium_compat as pdfium;` and otherwise stay as
//! upstream wrote them, which keeps future upstream merges cheap.
//!
//! The emulation follows PDFium's observable behavior where liteparse depends
//! on it:
//!
//! * Geometry uses PDFium's page model: page space is unrotated PDF user
//!   space, the viewport is the CropBox ∩ MediaBox rotated by `/Rotate`,
//!   y-down, scaled by `/UserUnit` ([`page`]).
//! * The text API is a port of PDFium's `CPDF_TextPage` character stream —
//!   object line buffering, generated spaces and line breaks, hyphen marking,
//!   duplicate suppression and bidi line reordering — computed from hayro's
//!   glyph runs ([`text_page`]).
//! * Page objects (paths, images, text runs, clips, marked content) are
//!   collected by a single hayro [`Device`](hayro::hayro_interpret::Device)
//!   pass per page ([`content`]).
//! * hayro's glyph runs carry neither the `Tf` font size nor the character
//!   codes, both of which PDFium reports; a scan of the content stream
//!   recovers them per text-showing operator ([`text_state`]).
//!
//! Known gaps relative to PDFium:
//!
//! * A glyph the content scan cannot place (text hayro draws from places
//!   the scan does not follow) reports its rendered size and a synthetic
//!   character code, `0x10000 + glyph id`.
//! * Text in clip-only render mode (`7 Tr`) is recovered from page content
//!   only ([`clip_text`]); inside form XObjects it never reaches hayro's
//!   devices and so is missing from the text page.
//! * There is no form-fill environment: widget values come from the widgets'
//!   appearance streams and field dictionaries, and document JavaScript never
//!   runs.
//! * Marked-content `/ActualText` replacement is not applied.
//! * Glyph names resolve through hayro's tables, which also know TeX names
//!   that FreeType leaves unmapped; a few names map to other code points
//!   (`Omega` gives U+03A9 where PDFium reports U+2126).
//! * DeviceCMYK colors use a plain conversion rather than PDFium's Adobe
//!   table, and the color a path does not paint with (the stroke of a
//!   fill-only path, and vice versa) is reported as opaque black rather than
//!   the graphics state's color.
//! * [`Page::image_objects`] includes images painted inside form XObjects,
//!   which PDFium's top-level object walk skips.
//!
//! [hayro]: https://github.com/LaurenzV/hayro

// The module is hidden from the public docs; its links to private modules
// serve `--document-private-items` readers.
#![allow(rustdoc::private_intra_doc_links)]

mod annots;
mod bitmap;
mod char_metrics;
mod clip_text;
mod content;
mod document;
mod error;
mod font;
mod library;
mod objects;
mod page;
mod path_bounds;
mod struct_tree;
mod text_page;
mod text_state;
mod types;

pub use bitmap::{Bitmap, BitmapFormat};
pub use document::{Document, FormEnvironment, OutlineEntry, SignatureSummary, XfaPacket};
pub use error::PdfiumError;
pub use font::{Font, FontType};
pub use library::Library;
pub use page::{
    ImageBounds, ImageObjectInfo, ImageObjects, Page, PageObject, PageObjectKind, PathObject,
    PathSegment, PdfAnnotation, PdfFormField, PdfLink, RawPathSegment, SegmentKind,
    ViewportTransform,
};
pub use struct_tree::{StructNode, StructureAttributeValue, StructureElement};
pub use text_page::{TextChar, TextPage};
pub use types::{CharBox, Color, Matrix, RectF, TextRect};

pub(crate) use content::ClipState;
pub(crate) use text_page::text_object_clip;

/// Raw-handle types from `pdfium-sys` that liteparse code names directly.
///
/// The text-object handle points at a run record owned by the [`TextPage`]
/// that reported it, so it stays valid exactly as long as that text page —
/// the same contract PDFium's handles have.
#[allow(non_camel_case_types, non_snake_case)]
pub mod pdfium_sys {
    /// Opaque text-object handle (see the module docs).
    pub type FPDF_PAGEOBJECT = *mut std::ffi::c_void;

    /// Rectangle of a batched character record.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct FS_RECTF {
        pub left: f32,
        pub top: f32,
        pub right: f32,
        pub bottom: f32,
    }

    /// Record layout of PDFium's batched character-info API. The hayro
    /// backend never fills these: [`super::TextPage::char_infos_batch`]
    /// reports the API as unavailable, so callers take the per-character
    /// path.
    #[derive(Debug, Clone, Copy)]
    pub struct FPDF_CHARINFO_LP {
        pub unicode: u32,
        pub char_code: u32,
        pub char_type: i32,
        pub text_render_mode: i32,
        pub text_object: FPDF_PAGEOBJECT,
        pub loose_box: FS_RECTF,
        pub left: f64,
        pub top: f64,
        pub right: f64,
        pub bottom: f64,
    }

    impl Default for FPDF_CHARINFO_LP {
        fn default() -> Self {
            Self {
                unicode: 0,
                char_code: 0,
                char_type: 0,
                text_render_mode: -1,
                text_object: std::ptr::null_mut(),
                loose_box: FS_RECTF::default(),
                left: 0.0,
                top: 0.0,
                right: 0.0,
                bottom: 0.0,
            }
        }
    }
}
