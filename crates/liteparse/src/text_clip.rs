//! Conservative glyph visibility under rectangular PDF graphics-state clips.
//! The content pass records each text object's effective rectangular clip in
//! page space; unknown (non-rectangular) clips preserve text, including OCR
//! text layers.

use crate::pdfium_compat as pdfium;
use pdfium::{Page, RectF};

use crate::extract::CharView;

/// Text-object clip lookup for one page.
pub(crate) struct TextClip;

impl TextClip {
    pub(crate) fn new(_page: &Page) -> Self {
        Self
    }

    /// Bounds of the clip active for the char's text object, in y-up page
    /// coordinates; inverted bounds mean an empty intersection.
    fn bounds(cv: &CharView<'_, '_>) -> Option<RectF> {
        let obj = cv.text_object()?;
        // SAFETY: `obj` was just reported by the live text page behind `cv`.
        match unsafe { pdfium::text_object_clip(obj) } {
            pdfium::ClipState::Rect(rect) => Some(RectF {
                left: rect.x0 as f32,
                bottom: rect.y0 as f32,
                right: rect.x1 as f32,
                top: rect.y1 as f32,
            }),
            pdfium::ClipState::None | pdfium::ClipState::Unknown => None,
        }
    }

    /// Generated separators have no reliable source object and are preserved.
    /// A glyph is kept whole when its ink box, widened by [`INK_MARGIN`] of its em
    /// extent, meets the clip; glyphs without ink are judged by their loose box.
    pub(crate) fn hides(&self, cv: &CharView<'_, '_>) -> bool {
        if cv.is_generated() {
            return false;
        }
        let Some(bounds) = Self::bounds(cv) else {
            return false;
        };
        // The ink (strict) box decides: a loose box carries the font's side bearings and
        // ascent, so it can graze a clip edge by a fraction of a point while every pixel of
        // the glyph lies outside. Whitespace and inkless glyphs keep the loose box.
        let inked = !char::from_u32(cv.unicode()).is_some_and(char::is_whitespace);
        let loose = cv.loose_char_box();
        let strict = cv
            .strict_char_box()
            .filter(|b| inked && b.right > b.left && b.top > b.bottom);
        let (glyph, margin) = match (strict, loose) {
            (Some(ink), _) => (ink, INK_MARGIN * loose.map_or(0.0, em_extent)),
            (None, Some(loose)) => (loose, 0.0),
            (None, None) => return false,
        };
        if ![glyph.left, glyph.right, glyph.bottom, glyph.top]
            .iter()
            .all(|v| v.is_finite())
            || glyph.left > glyph.right
            || glyph.bottom > glyph.top
        {
            return false;
        }
        // A tiny tolerance prevents float rounding at a clip edge losing a glyph.
        const EPS: f64 = 0.001;
        let tol = EPS + margin;
        bounds.left > bounds.right
            || bounds.bottom > bounds.top
            || f64::from(glyph.right) < f64::from(bounds.left) - tol
            || f64::from(glyph.left) > f64::from(bounds.right) + tol
            || f64::from(glyph.top) < f64::from(bounds.bottom) - tol
            || f64::from(glyph.bottom) > f64::from(bounds.top) + tol
    }
}

/// Share of a glyph's em extent by which its ink box may miss a clip and still count
/// as visible. A strict box can understate the painted outline (a substituted
/// font, quantised glyph bounds) by about 1% of the font size; that edge of ink is drawn.
const INK_MARGIN: f64 = 0.02;

/// The larger side of the loose box, about one em (ascent to descent) at any rotation.
fn em_extent(loose: RectF) -> f64 {
    let em = f64::from(
        (loose.right - loose.left)
            .abs()
            .max((loose.top - loose.bottom).abs()),
    );
    if em.is_finite() { em } else { 0.0 }
}
