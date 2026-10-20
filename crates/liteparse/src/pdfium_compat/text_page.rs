//! A port of PDFium's `CPDF_TextPage` character stream over hayro glyph runs.
//!
//! liteparse's segmenter (`crate::extract`) was tuned on the characters
//! PDFium's text API reports: real glyphs plus *generated* spaces and line
//! breaks, end-of-line hyphens marked as such, duplicate (fake-bold) runs
//! removed, and right-to-left runs reordered per line. This module rebuilds
//! that stream from the text runs [`super::content`] records, following
//! `core/fpdftext/cpdf_textpage.cpp` function by function.
//!
//! Frames: a text object's *text space* is the run's first glyph transform
//! normalized to the effective font size, so one text-space unit is one
//! point along the baseline and the font size is the rendered size. PDFium's
//! text space is the text matrix without `Tf`; the two agree whenever the
//! font size is not baked into the text matrix, and every threshold below is
//! a ratio of text-space quantities, so the choice cancels out otherwise.

use std::marker::PhantomData;
use std::sync::OnceLock;

use hayro::kurbo::{Affine, Point, Rect};
use unicode_bidi::BidiClass;

use super::char_metrics::CodeReader;
use super::content::{ClipState, GlyphText, PageContent, TextRun, run_bounds_in};
use super::font::{Font, SYNTHETIC_CODE_BASE};
use super::page::Page;
use super::pdfium_sys;
use super::types::{CharBox, Color, Matrix, RectF, TextRect};

const SIZE_EPSILON: f32 = 0.01;
const DEFAULT_FONT_SIZE: f32 = 1.0;
const INVALID_CODE: u32 = u32::MAX;

/// PDFium `CPDF_TextPage::CharType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CharType {
    Normal,
    Generated,
    NotUnicode,
    Hyphen,
    Piece,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextOrientation {
    Unknown,
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerateCharacter {
    None,
    Space,
    LineBreak,
    Hyphen,
}

/// PDFium's `CFX_FloatRect`: y-up, `left/bottom/right/top`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct FRect {
    left: f32,
    bottom: f32,
    right: f32,
    top: f32,
}

impl FRect {
    fn from_kurbo(r: Rect) -> Self {
        Self {
            left: r.x0 as f32,
            bottom: r.y0 as f32,
            right: r.x1 as f32,
            top: r.y1 as f32,
        }
    }

    fn point(p: Point) -> Self {
        Self {
            left: p.x as f32,
            bottom: p.y as f32,
            right: p.x as f32,
            top: p.y as f32,
        }
    }

    fn width(&self) -> f32 {
        self.right - self.left
    }

    fn height(&self) -> f32 {
        self.top - self.bottom
    }

    fn is_empty(&self) -> bool {
        self.left >= self.right || self.bottom >= self.top
    }

    fn normalize(&mut self) {
        if self.left > self.right {
            std::mem::swap(&mut self.left, &mut self.right);
        }
        if self.bottom > self.top {
            std::mem::swap(&mut self.bottom, &mut self.top);
        }
    }

    fn intersect(&mut self, other: &FRect) {
        let mut a = *self;
        a.normalize();
        let mut b = *other;
        b.normalize();
        let r = FRect {
            left: a.left.max(b.left),
            bottom: a.bottom.max(b.bottom),
            right: a.right.min(b.right),
            top: a.top.min(b.top),
        };
        *self = if r.left > r.right || r.bottom > r.top {
            FRect::default()
        } else {
            r
        };
    }

    fn union(&mut self, other: &FRect) {
        let mut a = *self;
        a.normalize();
        let mut b = *other;
        b.normalize();
        *self = FRect {
            left: a.left.min(b.left),
            bottom: a.bottom.min(b.bottom),
            right: a.right.max(b.right),
            top: a.top.max(b.top),
        };
    }

    fn contains(&self, p: Point) -> bool {
        let mut n = *self;
        n.normalize();
        let (x, y) = (p.x as f32, p.y as f32);
        x <= n.right && x >= n.left && y <= n.top && y >= n.bottom
    }
}

fn x_unit(m: &Affine) -> f32 {
    let [a, b, ..] = m.as_coeffs();
    let (a, b) = (a as f32, b as f32);
    if b == 0.0 {
        a.abs()
    } else if a == 0.0 {
        b.abs()
    } else {
        a.hypot(b)
    }
}

fn y_unit(m: &Affine) -> f32 {
    let [_, _, c, d, ..] = m.as_coeffs();
    let (c, d) = (c as f32, d as f32);
    if c == 0.0 {
        d.abs()
    } else if d == 0.0 {
        c.abs()
    } else {
        c.hypot(d)
    }
}

/// A `CFX_Matrix`: PDFium's matrix math runs in single precision, and its
/// line-break tests sit on knife edges such as "exactly one text-space unit
/// down", where the rounding decides.
#[derive(Debug, Clone, Copy)]
struct FMatrix([f32; 6]);

impl FMatrix {
    fn from_affine(m: &Affine) -> Self {
        Self(m.as_coeffs().map(|v| v as f32))
    }

    /// `CFX_Matrix::GetInverse`.
    fn inverse(self) -> Self {
        let [a, b, c, d, e, f] = self.0;
        let i = a * d - b * c;
        if i.abs() == 0.0 {
            return Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        }
        let j = -i;
        Self([
            d / i,
            b / j,
            c / j,
            a / i,
            (c * f - d * e) / i,
            (a * f - b * e) / j,
        ])
    }

    /// `CFX_Matrix::Transform`.
    fn transform(self, p: Point) -> Point {
        let [a, b, c, d, e, f] = self.0;
        let (x, y) = (p.x as f32, p.y as f32);
        Point::new((a * x + c * y + e) as f64, (b * x + d * y + f) as f64)
    }

    fn to_affine(self) -> Affine {
        Affine::new(self.0.map(f64::from))
    }
}

/// `CFX_Matrix::TransformDistance`.
fn transform_distance(m: &Affine, distance: f32) -> f32 {
    distance * (x_unit(m) + y_unit(m)) / 2.0
}

/// `CFX_Matrix::TransformXDistance`.
fn transform_x_distance(m: &Affine, dx: f32) -> f32 {
    let [a, b, ..] = m.as_coeffs();
    ((a as f32) * dx).hypot((b as f32) * dx)
}

fn transform_rect(m: &Affine, r: Rect) -> FRect {
    FRect::from_kurbo(m.transform_rect_bbox(r))
}

fn normalize_threshold(threshold: f32, t1: f32, t2: f32, t3: f32) -> f32 {
    if threshold < t1 {
        threshold / 2.0
    } else if threshold < t2 {
        threshold / 4.0
    } else if threshold < t3 {
        threshold / 5.0
    } else {
        threshold / 6.0
    }
}

fn is_hyphen_code(c: u32) -> bool {
    c == 0x2D || c == 0xAD
}

/// One glyph of a [`TextObject`].
#[derive(Debug, Clone)]
pub(crate) struct Item {
    pub code: u32,
    pub unicode: Option<GlyphText>,
    /// Whether the font maps the glyph to Unicode (`unicode` may hold a
    /// fallback otherwise).
    pub mapped: bool,
    /// Origin in the object's text space.
    pub origin: Point,
    /// Origin in page space.
    pub page_origin: Point,
    /// `CPDF_Font::GetCharWidth`: advance in glyph units (1/1000 em).
    pub width: f32,
    /// The text page's own `GetCharWidth` helper: the advance, or the
    /// glyph box width for a zero-width glyph.
    pub measure: f32,
    char_box: FRect,
    loose_box: FRect,
}

impl Item {
    fn first_unicode(&self) -> u32 {
        self.unicode
            .as_deref()
            .and_then(|s| s.chars().next())
            .map_or(0xFFFD, |c| c as u32)
    }

    fn last_unicode(&self) -> u32 {
        self.unicode
            .as_deref()
            .and_then(|s| s.chars().last())
            .map_or(0xFFFD, |c| c as u32)
    }
}

/// A text object: one text-showing operation. Its address is the
/// `FPDF_PAGEOBJECT` handle the text API reports.
#[derive(Debug)]
pub(crate) struct TextObject {
    pub font: Option<Font>,
    pub render_mode: i32,
    pub fill: Option<Color>,
    pub stroke: Option<Color>,
    pub mcid: Option<i32>,
    pub clip: ClipState,
    /// The `Tf` size (`GetFontSize`); the rendered size when the content
    /// scan could not find the run's operator.
    pub font_size: f32,
    /// `CPDF_TextState::GetFontSizeH`.
    font_size_h: f32,
    /// Page-space bounds.
    rect: FRect,
    /// Origin of the first glyph in page space.
    pos: Point,
    /// Text space → page space (`GetTextMatrix() × form matrix`).
    matrix: Affine,
    /// `GetRect`, `GetPos` and `GetTextMatrix` as PDFium keeps them: in the
    /// space of the form XObject the text is in (page space outside forms),
    /// which some of its tests compare across objects as they are.
    raw_rect: FRect,
    raw_pos: Point,
    raw_matrix: Affine,
    /// Inside a form XObject.
    in_form: bool,
    /// `GetCharKernings` (TJ units) and `Tc`, when the content scan knows
    /// them; estimated from glyph positions otherwise.
    kernings: Option<(Vec<f32>, f32)>,
    items: Vec<Item>,
}

impl TextObject {
    fn handle(&self) -> pdfium_sys::FPDF_PAGEOBJECT {
        self as *const TextObject as pdfium_sys::FPDF_PAGEOBJECT
    }

    fn from_run(run: &TextRun, font: Option<Font>) -> Option<Self> {
        let first = run.glyphs.first()?;
        // The text matrix is the glyph transform without the `Tf` scale:
        // `Th × Tm × CTM`, as PDFium reports it.
        let (font_size, matrix, font_size_h) = match run.size {
            Some(size) if size.size.is_finite() && size.size != 0.0 => (
                size.size,
                first.transform * Affine::scale(1000.0 / size.size as f64),
                Some(size.size_h),
            ),
            _ => {
                let [_, _, c, d, ..] = first.transform.as_coeffs();
                let size = (1000.0 * c.hypot(d)) as f32;
                if !(size.is_finite() && size > 0.0) {
                    return None;
                }
                (
                    size,
                    first.transform * Affine::scale(1000.0 / size as f64),
                    None,
                )
            }
        };
        if matrix.determinant() == 0.0 || !matrix.determinant().is_finite() {
            return None;
        }
        let font_size_h = font_size_h.unwrap_or_else(|| x_unit(&matrix) * font_size.abs());
        let inverse = matrix.inverse();
        // `GetLooseBounds`: the font's type ascent/descent, clamped to its
        // bounding box.
        let vertical_extent = font.as_ref().and_then(|f| {
            let info = f.info();
            let (mut ascent, mut descent) = (f.type_ascent()?, f.type_descent()?);
            if let Some(bbox) = info.font_bbox
                && bbox[3] > bbox[1]
            {
                ascent = ascent.min(bbox[3]);
                descent = descent.max(bbox[1]);
            }
            (ascent != descent).then_some((ascent, descent))
        });
        let items = run
            .glyphs
            .iter()
            .map(|glyph| {
                let page_origin = glyph.transform * Point::ZERO;
                let width = if glyph.advance > 0.0 || glyph.exact_advance {
                    glyph.advance
                } else {
                    glyph.ink.map_or(0.0, |ink| ink.width().max(0.0) as f32)
                };
                // `font->GetCharBBox` with PDFium's empty-box fix-ups.
                let mut local = glyph.ink.unwrap_or(Rect::ZERO);
                if (local.height() as f32).abs() < SIZE_EPSILON {
                    local.y1 = local.y0 + 1.0;
                }
                if (local.width() as f32).abs() < SIZE_EPSILON {
                    local.x1 = local.x0 + width as f64;
                }
                let char_box = transform_rect(&glyph.transform, local);
                let origin = inverse * page_origin;
                let loose_box = match vertical_extent {
                    Some((ascent, descent)) if !char_box.is_empty() => {
                        let em = font_size as f64 / 1000.0;
                        let mut loose = transform_rect(
                            &matrix,
                            Rect::new(
                                origin.x,
                                origin.y + descent as f64 * em,
                                origin.x + width as f64 * em,
                                origin.y + ascent as f64 * em,
                            ),
                        );
                        loose.union(&char_box);
                        loose
                    }
                    _ => char_box,
                };
                let unmapped = matches!(
                    glyph.unicode.as_deref(),
                    None | Some("\u{FFFE}" | "\u{FFFF}")
                );
                let (unicode, mapped) = if !unmapped {
                    (glyph.unicode.clone(), true)
                } else if glyph.code < SYNTHETIC_CODE_BASE {
                    // PDFium reads an unmapped code as its own value
                    // (`kNotUnicode`); code 0 stays empty.
                    match glyph.code {
                        0 => (Some(GlyphText::new("\0")), true),
                        code => (char::from_u32(code).map(GlyphText::from), false),
                    }
                } else if glyph.ink.is_none() {
                    // Without the code: a glyph that draws nothing is a
                    // space in all but name.
                    (Some(GlyphText::new(" ")), false)
                } else {
                    (None, false)
                };
                Item {
                    code: glyph.code,
                    unicode,
                    mapped,
                    origin,
                    page_origin,
                    width,
                    measure: if width > 0.0 {
                        width
                    } else {
                        glyph.ink.map_or(0.0, |ink| ink.width().max(0.0) as f32)
                    },
                    char_box,
                    loose_box,
                }
            })
            .collect();
        let inflate = |rect: &mut FRect, half: f32| {
            rect.left -= half;
            rect.bottom -= half;
            rect.right += half;
            rect.top += half;
        };
        let mut rect = FRect::from_kurbo(run_bounds_in(run, Affine::IDENTITY)?);
        let to_form = run.form.inverse();
        let mut raw_rect = FRect::from_kurbo(run_bounds_in(run, to_form)?);
        if matches!(run.render_mode, 1 | 2 | 5 | 6) {
            let half = run.line_width / 2.0;
            inflate(&mut rect, half);
            inflate(
                &mut raw_rect,
                half / (run.form.determinant().abs().sqrt() as f32).max(1e-6),
            );
        }
        let pos = first.transform * Point::ZERO;
        Some(Self {
            font,
            render_mode: run.render_mode,
            fill: run.fill,
            stroke: run.stroke,
            mcid: run.mcid,
            clip: run.clip,
            font_size,
            font_size_h,
            rect,
            pos,
            matrix,
            raw_rect,
            raw_pos: to_form * pos,
            raw_matrix: to_form * matrix,
            in_form: run.in_form,
            kernings: run
                .kernings
                .clone()
                .filter(|(k, _)| k.len() == run.glyphs.len()),
            items,
        })
    }

    fn char_width(&self, item: &Item) -> f32 {
        item.width * self.font_size / 1000.0
    }

    /// [`Item::measure`] at the object's font size.
    fn char_measure(&self, item: &Item) -> f32 {
        item.measure * self.font_size / 1000.0
    }

    /// `GetCharWidth(CharCodeFromUnicode(' '))`, `None` without a space code.
    fn space_width_units(&self) -> Option<f32> {
        let font = self.font.as_ref()?;
        font.space_char_code().map(|code| font.char_width(code))
    }

    fn font_key(&self) -> Option<u128> {
        self.font.as_ref().map(|f| f.key())
    }
}

/// One character of the text page (PDFium `CharInfo`).
#[derive(Debug, Clone)]
pub(crate) struct CharInfo {
    char_type: CharType,
    char_code: u32,
    unicode: u32,
    origin: Point,
    char_box: FRect,
    loose_box: FRect,
    matrix: Affine,
    object: Option<usize>,
}

/// Extracted text content of a [`Page`].
pub struct TextPage<'page, 'lib: 'page> {
    objects: Vec<TextObject>,
    chars: Vec<CharInfo>,
    _page: PhantomData<&'page Page<'page, 'lib>>,
}

impl<'page, 'lib: 'page> TextPage<'page, 'lib> {
    pub(crate) fn build(page: &Page<'_, '_>, content: &PageContent) -> Self {
        let fonts = page.document().fonts();
        let objects: Vec<TextObject> = content
            .runs
            .iter()
            .filter_map(|run| TextObject::from_run(run, Some(fonts.font(run.font_key))))
            .collect();
        let rtl = page.document().is_direction_r2l();
        let display = {
            let view_box = page.view_box().unwrap_or_default();
            // PDFium's display matrix ignores `/UserUnit`.
            let unit = page.user_unit() as f64;
            Affine::scale(1.0 / unit) * page.viewport_affine(&view_box)
        };
        let (page_width, page_height) = (page.width(), page.height());
        let chars = Builder::new(&objects, rtl, display).run(page_width, page_height);
        Self {
            objects,
            chars,
            _page: PhantomData,
        }
    }

    pub fn char_count(&self) -> i32 {
        self.chars.len() as i32
    }

    pub fn chars(&self) -> impl Iterator<Item = TextChar<'_>> + '_ {
        (0..self.chars.len()).map(move |index| TextChar {
            text_page: self,
            index,
        })
    }

    pub fn char_at(&self, index: i32) -> Option<TextChar<'_>> {
        (index >= 0 && (index as usize) < self.chars.len()).then_some(TextChar {
            text_page: self,
            index: index as usize,
        })
    }

    /// Like [`Self::char_at`]; `index` must be in `0..char_count`.
    pub fn char_at_unchecked(&self, index: i32) -> TextChar<'_> {
        TextChar {
            text_page: self,
            index: index as usize,
        }
    }

    /// PDFium's batched char-info API; not available in this backend, so
    /// callers take their per-character path.
    pub fn char_infos_batch(
        &self,
        _start: i32,
        _buf: &mut [pdfium_sys::FPDF_CHARINFO_LP],
    ) -> Option<usize> {
        None
    }

    /// Text of `count` characters from `start`, generated characters
    /// included (`FPDFText_GetText`).
    pub fn get_text(&self, start: i32, count: i32) -> String {
        let start = start.max(0) as usize;
        let end = if count < 0 {
            self.chars.len()
        } else {
            (start + count as usize).min(self.chars.len())
        };
        self.chars
            .get(start..end)
            .unwrap_or_default()
            .iter()
            .filter_map(|c| char::from_u32(c.unicode))
            .collect()
    }

    /// The characters whose boxes intersect the rectangle (page space).
    pub fn bounded_text(&self, left: f64, top: f64, right: f64, bottom: f64) -> String {
        let rect = FRect {
            left: left.min(right) as f32,
            bottom: bottom.min(top) as f32,
            right: left.max(right) as f32,
            top: bottom.max(top) as f32,
        };
        self.chars
            .iter()
            .filter(|c| {
                let mut b = c.char_box;
                b.intersect(&rect);
                !b.is_empty() || (c.char_type == CharType::Generated && rect.contains(c.origin))
            })
            .filter_map(|c| char::from_u32(c.unicode))
            .collect()
    }

    /// Number of rectangles covering `count` characters from `start`: one per
    /// run of characters from the same text object.
    pub fn count_rects(&self, start: i32, count: i32) -> i32 {
        self.rects(start, count).len() as i32
    }

    pub fn rect(&self, index: i32) -> Option<TextRect> {
        // Callers use `count_rects` over the whole page first.
        self.rects(0, -1).get(index.max(0) as usize).copied()
    }

    fn rects(&self, start: i32, count: i32) -> Vec<TextRect> {
        let start = start.max(0) as usize;
        let end = if count < 0 {
            self.chars.len()
        } else {
            (start + count as usize).min(self.chars.len())
        };
        let mut out: Vec<TextRect> = Vec::new();
        let mut current: Option<(Option<usize>, FRect)> = None;
        for c in self.chars.get(start..end).unwrap_or_default() {
            if c.char_type == CharType::Generated || c.char_box.is_empty() {
                continue;
            }
            match &mut current {
                Some((object, rect)) if *object == c.object => rect.union(&c.char_box),
                _ => {
                    if let Some((_, rect)) = current.take() {
                        out.push(text_rect(rect));
                    }
                    current = Some((c.object, c.char_box));
                }
            }
        }
        if let Some((_, rect)) = current {
            out.push(text_rect(rect));
        }
        out
    }
}

fn text_rect(rect: FRect) -> TextRect {
    TextRect {
        left: rect.left as f64,
        top: rect.top as f64,
        right: rect.right as f64,
        bottom: rect.bottom as f64,
    }
}

/// The clip state of the text object behind `obj`.
///
/// # Safety
/// `obj` must come from a [`TextPage`] that is still alive.
pub(crate) unsafe fn text_object_clip(obj: pdfium_sys::FPDF_PAGEOBJECT) -> ClipState {
    if obj.is_null() {
        return ClipState::None;
    }
    // SAFETY: per the contract above.
    unsafe { (*(obj as *const TextObject)).clip }
}

/// A view of one character of a [`TextPage`].
pub struct TextChar<'tp> {
    text_page: &'tp TextPage<'tp, 'tp>,
    index: usize,
}

impl TextChar<'_> {
    fn info(&self) -> &CharInfo {
        &self.text_page.chars[self.index]
    }

    fn object(&self) -> Option<&TextObject> {
        self.info().object.map(|i| &self.text_page.objects[i])
    }

    /// The glyph's origin in page space.
    pub fn origin(&self) -> Option<(f64, f64)> {
        let p = self.info().origin;
        Some((p.x, p.y))
    }

    pub fn unicode(&self) -> u32 {
        self.info().unicode
    }

    /// The glyph's synthetic character code (see the module docs of
    /// `pdfium_compat`); 0 for generated characters.
    pub fn char_code(&self) -> u32 {
        match self.info().char_code {
            INVALID_CODE => 0,
            code => code,
        }
    }

    pub fn font_size(&self) -> f64 {
        self.object().map_or(DEFAULT_FONT_SIZE, |o| o.font_size) as f64
    }

    pub fn font_weight(&self) -> i32 {
        match self.object().and_then(|o| o.font.as_ref()) {
            Some(font) => font.info().weight(),
            None => -1,
        }
    }

    /// Base font name and `/Flags`.
    pub fn font_info(&self) -> Option<(String, i32)> {
        let font = self.object()?.font.as_ref()?;
        Some((
            font.info().base_name.clone().unwrap_or_default(),
            font.info().flags,
        ))
    }

    pub fn font_name(&self) -> Option<String> {
        self.font_info().map(|(name, _)| name)
    }

    /// `FPDFText_GetCharAngle`: `atan2(c, a)` of the char matrix, in
    /// `[0, 2π)`.
    pub fn angle(&self) -> f32 {
        let [a, _, c, ..] = self.info().matrix.as_coeffs();
        let mut angle = (c as f32).atan2(a as f32);
        if angle < 0.0 {
            angle += 2.0 * std::f32::consts::PI;
        }
        angle
    }

    pub fn text_object(&self) -> Option<pdfium_sys::FPDF_PAGEOBJECT> {
        self.object().map(TextObject::handle)
    }

    /// Advance of the ASCII space in this character's font, per em.
    pub fn font_space_width(&self) -> Option<f32> {
        let font = self.object()?.font.as_ref()?;
        font.glyph_width_from_char_code(0x20, 1.0)
            .filter(|w| *w > 0.0)
            .or_else(|| font.glyph_width(0x20, 1.0).filter(|w| *w > 0.0))
    }

    pub fn stroke_color(&self) -> Option<Color> {
        self.object()?.stroke
    }

    pub fn fill_color(&self) -> Option<Color> {
        self.object()?.fill
    }

    pub fn text_render_mode(&self) -> Option<i32> {
        Some(self.object()?.render_mode)
    }

    pub fn marked_content_id(&self) -> Option<i32> {
        self.object()?.mcid
    }

    pub fn char_box(&self) -> Option<CharBox> {
        let b = self.info().char_box;
        Some(CharBox {
            left: b.left as f64,
            right: b.right as f64,
            bottom: b.bottom as f64,
            top: b.top as f64,
        })
    }

    pub fn loose_char_box(&self) -> Option<RectF> {
        let b = self.info().loose_box;
        Some(RectF {
            left: b.left,
            top: b.top,
            right: b.right,
            bottom: b.bottom,
        })
    }

    pub fn matrix(&self) -> Option<Matrix> {
        Some(Matrix::from_affine(self.info().matrix))
    }

    pub fn is_generated(&self) -> bool {
        self.info().char_type == CharType::Generated
    }

    /// A hyphen found at the end of a line; its unicode reads as 0x02.
    pub fn is_hyphen(&self) -> bool {
        self.info().char_type == CharType::Hyphen
    }

    pub fn has_unicode_map_error(&self) -> bool {
        self.info().char_type == CharType::NotUnicode
    }
}

/// The text-page construction state (`CPDF_TextPage` members).
struct Builder<'o> {
    objects: &'o [TextObject],
    rtl: bool,
    display: Affine,
    textline_dir: TextOrientation,
    char_list: Vec<CharInfo>,
    text_buf: Vec<u32>,
    temp_char_list: Vec<CharInfo>,
    temp_text_buf: Vec<u32>,
    buffered: Vec<usize>,
    prev_text_obj: Option<usize>,
    curline_rect: FRect,
}

impl<'o> Builder<'o> {
    fn new(objects: &'o [TextObject], rtl: bool, display: Affine) -> Self {
        Self {
            objects,
            rtl,
            display,
            textline_dir: TextOrientation::Unknown,
            char_list: Vec::new(),
            text_buf: Vec::new(),
            temp_char_list: Vec::new(),
            temp_text_buf: Vec::new(),
            buffered: Vec::new(),
            prev_text_obj: None,
            curline_rect: FRect::default(),
        }
    }

    fn run(mut self, page_width: f32, page_height: f32) -> Vec<CharInfo> {
        if self.objects.is_empty() {
            return Vec::new();
        }
        self.textline_dir = self.find_textline_flow_orientation(page_width, page_height);
        for index in 0..self.objects.len() {
            self.buffer_text_object(index);
        }
        for index in std::mem::take(&mut self.buffered) {
            self.process_text_object(index);
        }
        self.close_temp_line();
        self.char_list
    }

    fn find_textline_flow_orientation(&self, page_width: f32, page_height: f32) -> TextOrientation {
        let (w, h) = (page_width as i32, page_height as i32);
        if w <= 0 || h <= 0 {
            return TextOrientation::Unknown;
        }
        let mut horizontal = vec![false; w as usize];
        let mut vertical = vec![false; h as usize];
        let mut line_height = 0.0f32;
        let (mut start_h, mut end_h, mut start_v, mut end_v) = (w, 0, h, 0);
        // Only page-level text objects; PDFium skips form XObjects here.
        for obj in self.objects.iter().filter(|o| !o.in_form) {
            let min_h = obj.rect.left.clamp(0.0, w as f32) as i32;
            let max_h = obj.rect.right.clamp(0.0, w as f32) as i32;
            let min_v = obj.rect.bottom.clamp(0.0, h as f32) as i32;
            let max_v = obj.rect.top.clamp(0.0, h as f32) as i32;
            if min_h >= max_h || min_v >= max_v {
                continue;
            }
            horizontal[min_h as usize..max_h as usize].fill(true);
            vertical[min_v as usize..max_v as usize].fill(true);
            start_h = start_h.min(min_h);
            end_h = end_h.max(max_h);
            start_v = start_v.min(min_v);
            end_v = end_v.max(max_v);
            if line_height <= 0.0 {
                line_height = obj.rect.height();
            }
        }
        let double_line_height = (2.0 * line_height) as i32;
        if end_v - start_v < double_line_height {
            return TextOrientation::Horizontal;
        }
        if end_h - start_h < double_line_height {
            return TextOrientation::Vertical;
        }
        let filled = |mask: &[bool], start: i32, end: i32| -> f32 {
            if start >= end {
                return 0.0;
            }
            let count = mask[start as usize..end as usize]
                .iter()
                .filter(|v| **v)
                .count();
            count as f32 / (end - start) as f32
        };
        let sum_h = filled(&horizontal, start_h, end_h);
        if sum_h > 0.8 {
            return TextOrientation::Horizontal;
        }
        let sum_v = filled(&vertical, start_v, end_v);
        if sum_h > sum_v {
            TextOrientation::Horizontal
        } else if sum_h < sum_v {
            TextOrientation::Vertical
        } else {
            TextOrientation::Unknown
        }
    }

    /// The first `ProcessTextObject` overload: per-line buffering with
    /// left-to-right insertion.
    fn buffer_text_object(&mut self, index: usize) {
        let obj = &self.objects[index];
        if obj.raw_rect.width().abs() < SIZE_EPSILON {
            return;
        }
        if self.buffered.is_empty() {
            self.buffered.push(index);
            return;
        }
        if self.is_same_as_pre_text_object(index) {
            return;
        }
        let prev = &self.objects[*self.buffered.last().unwrap()];
        let Some(prev_item) = prev.items.last() else {
            return;
        };
        let prev_width = transform_distance(&prev.matrix, prev.char_measure(prev_item).abs());
        let this_width = obj.char_measure(&obj.items[0]).abs();
        let this_width = transform_distance(&obj.matrix, this_width);
        let threshold = prev_width.max(this_width) / 4.0;
        let prev_pos = self.display * prev.pos;
        let this_pos = self.display * obj.pos;
        if ((this_pos.y - prev_pos.y) as f32).abs() > threshold * 2.0 {
            for buffered in std::mem::take(&mut self.buffered) {
                self.process_text_object(buffered);
            }
            self.buffered.push(index);
            return;
        }
        for i in (1..=self.buffered.len()).rev() {
            let other = &self.objects[self.buffered[i - 1]];
            let other_pos = self.display * other.pos;
            if this_pos.x >= other_pos.x {
                self.buffered.insert(i, index);
                return;
            }
        }
        self.buffered.insert(0, index);
    }

    fn is_same_as_pre_text_object(&self, index: usize) -> bool {
        (0..index)
            .rev()
            .take(5)
            .any(|other| self.is_same_text_object(other, index))
    }

    /// `IsSameTextObject(pTextObj1 = other, pTextObj2 = current)`.
    fn is_same_text_object(&self, obj1: usize, obj2: usize) -> bool {
        let (o1, o2) = (&self.objects[obj1], &self.objects[obj2]);
        let mut rc_pre = o2.raw_rect;
        let rc_cur = o1.raw_rect;
        if rc_pre.is_empty() && rc_cur.is_empty() {
            let x_diff = (rc_pre.left - rc_cur.left).abs();
            if self.char_list.len() >= 2 {
                let space = self.char_list[self.char_list.len() - 2].char_box.width();
                if x_diff > space {
                    return false;
                }
            }
        }
        if !rc_pre.is_empty() || !rc_cur.is_empty() {
            rc_pre.intersect(&rc_cur);
            if rc_pre.is_empty() {
                return false;
            }
            if (rc_pre.width() - rc_cur.width()).abs() > rc_cur.width() / 2.0 {
                return false;
            }
            if o2.font_size != o1.font_size {
                return false;
            }
        }
        if o1.items.len() != o2.items.len() {
            return false;
        }
        if o2.items.is_empty() {
            return true;
        }
        if o1
            .items
            .iter()
            .zip(&o2.items)
            .any(|(a, b)| a.code != b.code)
        {
            return false;
        }
        let last = o2.items.last().unwrap();
        let diff = o1.raw_pos - o2.raw_pos;
        let font_size = o2.font_size;
        let char_size = last.measure;
        let max_pre_size = rc_pre.height().max(rc_pre.width()).max(font_size);
        (diff.x as f32).abs() <= 0.9 * char_size * font_size / 1000.0
            && (diff.y as f32).abs() <= max_pre_size / 8.0
    }

    /// The second `ProcessTextObject` overload.
    fn process_text_object(&mut self, index: usize) {
        let obj = &self.objects[index];
        if obj.raw_rect.width().abs() < SIZE_EPSILON {
            return;
        }
        if self.prev_text_obj.is_some() {
            let kind = self.process_insert_object(index);
            if kind == GenerateCharacter::LineBreak {
                self.curline_rect = obj.raw_rect;
            } else {
                self.curline_rect.union(&obj.raw_rect);
            }
            if !self.process_generate_character(kind, index) {
                return;
            }
        } else {
            self.curline_rect = obj.raw_rect;
        }
        self.prev_text_obj = Some(index);

        // Saved before `process_text_object_items` grows the buffers.
        let buf_start = self.temp_text_buf.len();
        let list_start = self.temp_char_list.len();
        if self.process_text_object_items(index) {
            self.temp_char_list[list_start..].reverse();
            self.temp_text_buf[buf_start..].reverse();
        }
    }

    /// `IsRightToLeft`: bidi direction of the object's characters with
    /// automatic ordering (strictly more right-to-left segments).
    fn is_right_to_left(&self, obj: &TextObject) -> bool {
        let text: Vec<u32> = obj.items.iter().map(Item::first_unicode).collect();
        let (segments, _) = bidi_segments(&text, false);
        let rights = segments
            .iter()
            .filter(|s| s.direction == Direction::Right)
            .count();
        let lefts = segments
            .iter()
            .filter(|s| s.direction == Direction::Left)
            .count();
        rights > lefts
    }

    fn prev_char_info(&self) -> Option<&CharInfo> {
        self.temp_char_list.last().or(self.char_list.last())
    }

    fn find_previous_text_object(&mut self) {
        if let Some(object) = self.prev_char_info().and_then(|c| c.object) {
            self.prev_text_obj = Some(object);
        }
    }

    fn writing_mode(&self, obj: &TextObject) -> TextOrientation {
        if obj.items.len() <= 1 {
            return self.textline_dir;
        }
        let first = obj.items[0].page_origin;
        let last = obj.items[obj.items.len() - 1].page_origin;
        let dx = ((last.x - first.x) as f32).abs();
        let dy = ((last.y - first.y) as f32).abs();
        const EPSILON: f32 = 0.0001;
        if dx <= EPSILON && dy <= EPSILON {
            return TextOrientation::Unknown;
        }
        const THRESHOLD: f32 = 0.0872;
        let len = dx.hypot(dy);
        let (vx, vy) = (dx / len, dy / len);
        let x_under = vx <= THRESHOLD;
        if vy <= THRESHOLD {
            if x_under {
                self.textline_dir
            } else {
                TextOrientation::Horizontal
            }
        } else if x_under {
            TextOrientation::Vertical
        } else {
            self.textline_dir
        }
    }

    fn is_hyphen(&self, cur_char: u32) -> bool {
        let text = if self.temp_text_buf.is_empty() {
            &self.text_buf
        } else {
            &self.temp_text_buf
        };
        if text.is_empty() {
            return false;
        }
        let mut i = text.len() - 1;
        while i > 0 && text[i] == 0x20 {
            i -= 1;
        }
        if !is_hyphen_code(text[i]) {
            return false;
        }
        if i > 0 {
            let before = text[i - 1];
            if char::from_u32(before).is_some_and(char::is_alphabetic)
                && char::from_u32(cur_char).is_some_and(char::is_alphanumeric)
            {
                return true;
            }
        }
        self.prev_char_info()
            .is_some_and(|c| c.char_type == CharType::Piece && is_hyphen_code(c.unicode))
    }

    fn process_insert_object(&mut self, index: usize) -> GenerateCharacter {
        self.find_previous_text_object();
        let Some(prev_index) = self.prev_text_obj else {
            return GenerateCharacter::None;
        };
        let obj = &self.objects[index];
        let prev = &self.objects[prev_index];
        let mut mode = self.writing_mode(obj);
        if mode == TextOrientation::Unknown {
            mode = self.writing_mode(prev);
        }
        let Some(prev_item) = prev.items.last() else {
            return GenerateCharacter::None;
        };
        let item = &obj.items[0];
        let cur_char = item.first_unicode();
        match mode {
            TextOrientation::Horizontal => {
                if end_horizontal_line(&obj.raw_rect, &prev.raw_rect) {
                    return self.hyphen_or_line_break(cur_char);
                }
            }
            TextOrientation::Vertical => {
                if end_vertical_line(
                    &obj.raw_rect,
                    &prev.raw_rect,
                    &self.curline_rect,
                    obj.font_size,
                    prev.font_size,
                ) {
                    return self.hyphen_or_line_break(cur_char);
                }
            }
            TextOrientation::Unknown => {}
        }

        let last_pos = prev_item.origin.x as f32;
        let n_last_width = prev_item.measure;
        let last_width = (n_last_width * prev.font_size / 1000.0).abs();
        let n_this_width = item.measure;
        let this_width = (n_this_width * obj.font_size / 1000.0).abs();
        let mut threshold = last_width.max(this_width) / 4.0;
        let prev_reverse_f = FMatrix::from_affine(&prev.matrix).inverse();
        let prev_reverse = prev_reverse_f.to_affine();
        let pos = prev_reverse_f.transform(obj.pos);
        if last_width < this_width {
            threshold = transform_distance(&prev_reverse, threshold);
        }

        let mut newline = false;
        if mode == TextOrientation::Horizontal {
            let mut rect = prev.raw_rect;
            let rect_height = rect.height();
            rect.normalize();
            let (px, py) = (pos.x as f32, pos.y as f32);
            if (rect.is_empty() && rect_height > 5.0)
                || ((py > threshold * 2.0 || py < threshold * -3.0)
                    && (py.abs() >= 1.0 || py.abs() > px.abs()))
            {
                newline = true;
                if prev.items.len() > 1 {
                    let first_item = &prev.items[0];
                    let [m_a, m_b, m_c, m_d, ..] = self.display.as_coeffs();
                    let [_, t_b, t_c, ..] = prev.raw_matrix.as_coeffs();
                    if prev_item.origin.x > first_item.origin.x
                        && m_a > 0.9
                        && m_b < 0.1
                        && m_c < 0.1
                        && m_d < -0.9
                        && t_b < 0.1
                        && t_c < 0.1
                    {
                        let re = FRect {
                            left: 0.0,
                            bottom: prev.raw_rect.bottom,
                            right: 1000.0,
                            top: prev.raw_rect.top,
                        };
                        if re.contains(obj.raw_pos) {
                            newline = false;
                        } else {
                            let re = FRect {
                                left: 0.0,
                                bottom: obj.raw_rect.bottom,
                                right: 1000.0,
                                top: obj.raw_rect.top,
                            };
                            if re.contains(prev.raw_pos) {
                                newline = false;
                            }
                        }
                    }
                }
            }
        }
        if newline {
            return self.hyphen_or_line_break(cur_char);
        }
        if obj.items.len() == 1 && is_hyphen_code(cur_char) && self.is_hyphen(cur_char) {
            return GenerateCharacter::Hyphen;
        }
        if cur_char == 0x20 {
            return GenerateCharacter::None;
        }
        if prev_item.last_unicode() == 0x20 {
            return GenerateCharacter::None;
        }

        let mut threshold2 =
            normalize_threshold(n_last_width.max(n_this_width), 400.0, 700.0, 800.0);
        if n_last_width >= n_this_width {
            threshold2 *= prev.font_size.abs();
        } else {
            threshold2 *= obj.font_size.abs();
            threshold2 = transform_distance(&obj.matrix, threshold2);
            threshold2 = transform_distance(&prev_reverse, threshold2);
        }
        threshold2 /= 1000.0;
        if (threshold2 < 1.4881 && threshold2 > 1.4879)
            || (threshold2 < 1.39001 && threshold2 > 1.38999)
        {
            threshold2 *= 1.5;
        }
        if generate_space(pos, last_pos, this_width, last_width, threshold2) {
            GenerateCharacter::Space
        } else {
            GenerateCharacter::None
        }
    }

    fn hyphen_or_line_break(&self, cur_char: u32) -> GenerateCharacter {
        if self.is_hyphen(cur_char) {
            GenerateCharacter::Hyphen
        } else {
            GenerateCharacter::LineBreak
        }
    }

    fn process_generate_character(&mut self, kind: GenerateCharacter, index: usize) -> bool {
        match kind {
            GenerateCharacter::None => true,
            GenerateCharacter::Space => {
                self.append_generated(0x20, true);
                true
            }
            GenerateCharacter::LineBreak => {
                self.close_temp_line();
                if !self.text_buf.is_empty() {
                    self.append_generated(0x0D, false);
                    self.append_generated(0x0A, false);
                }
                true
            }
            GenerateCharacter::Hyphen => {
                let obj = &self.objects[index];
                if obj.items.len() == 1 && is_hyphen_code(obj.items[0].first_unicode()) {
                    return false;
                }
                while self.temp_text_buf.last() == Some(&0x20) {
                    self.temp_text_buf.pop();
                    self.temp_char_list.pop();
                }
                if let Some(info) = self.temp_char_list.last_mut() {
                    self.temp_text_buf.pop();
                    info.char_type = CharType::Hyphen;
                    info.unicode = 0x2;
                    self.temp_text_buf.push(0xFFFE);
                }
                true
            }
        }
    }

    /// `AppendGeneratedCharacter` + `GenerateCharInfo`.
    fn append_generated(&mut self, unicode: u32, use_temp: bool) {
        let Some(prev) = self.prev_char_info() else {
            return;
        };
        let (pre_width, font_size) = match prev.object.map(|i| &self.objects[i]) {
            Some(object) if prev.char_code != INVALID_CODE => {
                let width = object
                    .items
                    .iter()
                    .find(|item| item.code == prev.char_code)
                    .map_or(0.0, |item| item.measure);
                (width, object.font_size)
            }
            Some(object) => (0.0, object.font_size),
            None => (0.0, prev.char_box.height()),
        };
        let font_size = if font_size == 0.0 {
            DEFAULT_FONT_SIZE
        } else {
            font_size
        };
        let origin = Point::new(
            prev.origin.x + (pre_width * font_size / 1000.0) as f64,
            prev.origin.y,
        );
        let info = CharInfo {
            char_type: CharType::Generated,
            char_code: INVALID_CODE,
            unicode,
            origin,
            char_box: FRect::point(origin),
            loose_box: FRect::point(origin),
            matrix: Affine::IDENTITY,
            object: None,
        };
        if use_temp {
            self.temp_text_buf.push(unicode);
            self.temp_char_list.push(info);
        } else {
            self.text_buf.push(unicode);
            self.char_list.push(info);
        }
    }

    /// `ProcessTextObjectItems`. Returns whether the object is right-to-left
    /// text under a mirrored matrix, whose characters the caller reverses.
    fn process_text_object_items(&mut self, index: usize) -> bool {
        let obj = &self.objects[index];
        // PDFium measures TJ kerning in `GetFontSizeH` units and `Tc` through
        // the text matrix.
        let (kernings, char_space) = match &obj.kernings {
            Some((tj, char_space)) => (
                tj.iter()
                    .map(|k| -obj.font_size_h * k / 1000.0)
                    .collect::<Vec<f32>>(),
                *char_space,
            ),
            None => {
                // Kerning gaps estimated in text space.
                let (gaps, char_space) = estimate_kernings(obj);
                let to_size_h = obj.font_size_h / obj.font_size;
                (gaps.iter().map(|gap| gap * to_size_h).collect(), char_space)
            }
        };
        let base_space = calculate_base_space(
            char_space,
            transform_distance(&obj.matrix, char_space),
            &kernings,
            obj.items.len(),
        ) + if char_space > 0.001 {
            -transform_distance(&obj.matrix, char_space)
        } else if char_space < -0.001 {
            transform_distance(&obj.matrix, char_space.abs())
        } else {
            0.0
        };

        for (i, item) in obj.items.iter().enumerate() {
            let mut spacing = 0.0;
            if i > 0 && kernings[i - 1] != 0.0 {
                let ends_with_space = if self.temp_text_buf.is_empty() {
                    self.text_buf.last()
                } else {
                    self.temp_text_buf.last()
                } == Some(&0x20);
                if !ends_with_space {
                    spacing = kernings[i - 1];
                }
            }
            spacing -= base_space;
            if spacing != 0.0 && i > 0 {
                let threshold = self.space_threshold(obj, item);
                if threshold != 0.0 && spacing >= threshold {
                    self.temp_text_buf.push(0x20);
                    self.temp_char_list.push(CharInfo {
                        char_type: CharType::Generated,
                        char_code: INVALID_CODE,
                        unicode: 0x20,
                        origin: item.page_origin,
                        char_box: FRect::point(item.page_origin),
                        loose_box: FRect::point(item.page_origin),
                        matrix: obj.matrix,
                        object: Some(index),
                    });
                }
            }

            // PDFium substitutes the char code for an unmapped glyph; with no
            // char code to offer, U+FFFD marks it (spaces were recovered when
            // the item was built).
            let (unicode, char_type) = match item.unicode.as_deref() {
                Some(text) if !text.is_empty() && item.mapped => (text, CharType::Normal),
                Some(text) if !text.is_empty() => (text, CharType::NotUnicode),
                _ => ("\u{FFFD}", CharType::NotUnicode),
            };
            let info = CharInfo {
                char_type,
                char_code: item.code,
                unicode: 0,
                origin: item.page_origin,
                char_box: item.char_box,
                loose_box: item.loose_box,
                matrix: obj.matrix,
                object: Some(index),
            };

            // Drop a glyph drawn again at (almost) the same spot by the same
            // font, as PDFium does for fake-bold rendering.
            let threshold = transform_x_distance(&obj.matrix, 0.07 * obj.font_size);
            let count = self.temp_char_list.len().min(7);
            let font_key = obj.font_key();
            let duplicate = self.temp_char_list[self.temp_char_list.len() - count..]
                .iter()
                .any(|other| {
                    let diff = other.origin - info.origin;
                    other.char_code == info.char_code
                        && other.object.and_then(|o| self.objects[o].font_key()) == font_key
                        && (diff.x as f32).abs() < threshold
                        && (diff.y as f32).abs() < threshold
                });
            if !duplicate {
                for c in unicode.chars() {
                    let mut piece = info.clone();
                    piece.unicode = c as u32;
                    self.temp_text_buf
                        .push(if c == '\0' { 0xFFFE } else { c as u32 });
                    self.temp_char_list.push(piece);
                }
            } else if i == 0 && self.temp_text_buf.last() == Some(&0x20) {
                self.temp_text_buf.pop();
                self.temp_char_list.pop();
            }
        }
        obj.matrix.determinant() < 0.0 && self.is_right_to_left(obj)
    }

    /// `CalculateSpaceThreshold`.
    fn space_threshold(&self, obj: &TextObject, item: &Item) -> f32 {
        let fontsize_h = obj.font_size_h;
        let mut threshold = obj
            .space_width_units()
            .map_or(0.0, |w| fontsize_h * w / 1000.0);
        if threshold > fontsize_h / 3.0 {
            threshold = 0.0;
        } else {
            threshold /= 2.0;
        }
        if threshold == 0.0 {
            threshold = normalize_threshold(item.measure, 300.0, 500.0, 700.0);
            threshold = fontsize_h * threshold / 1000.0;
        }
        threshold
    }

    fn close_temp_line(&mut self) {
        if self.temp_char_list.is_empty() {
            return;
        }
        // Collapse runs of spaces in one compaction pass (the llamaparse
        // PDFium patch replaced the quadratic delete-in-place loop).
        let mut write = 0;
        let mut prev_space = false;
        for read in 0..self.temp_text_buf.len() {
            let is_space = self.temp_text_buf[read] == 0x20;
            if is_space && prev_space {
                continue;
            }
            if write != read {
                self.temp_text_buf[write] = self.temp_text_buf[read];
                self.temp_char_list.swap(write, read);
            }
            prev_space = is_space;
            write += 1;
        }
        self.temp_text_buf.truncate(write);
        self.temp_char_list.truncate(write);

        let text = std::mem::take(&mut self.temp_text_buf);
        let infos = std::mem::take(&mut self.temp_char_list);
        // `CFX_BidiString(str, /*auto_order=*/false)`: segments stay in
        // logical order unless the document's direction is right-to-left.
        let (segments, overall_right) = bidi_segments(&text, self.rtl);
        let mut current_right = overall_right;
        for segment in segments {
            let range = segment.start..segment.start + segment.count;
            if segment.direction == Direction::Right
                || (segment.direction == Direction::Neutral && current_right)
            {
                current_right = true;
                for m in range.rev() {
                    self.add_char_info(text[m], &infos[m], true);
                }
            } else {
                if segment.direction != Direction::LeftWeak {
                    current_right = false;
                }
                for m in range {
                    self.add_char_info(text[m], &infos[m], false);
                }
            }
        }
    }

    /// `AddCharInfo`: right-to-left characters are mirrored and normalized
    /// (always as pieces), left-to-right ones only for the Latin ligatures.
    fn add_char_info(&mut self, unicode: u32, info: &CharInfo, is_rtl: bool) {
        if !is_normal_character(info) {
            self.char_list.push(info.clone());
            return;
        }
        let unicode = if is_rtl {
            char::from_u32(unicode)
                .and_then(unicode_bidi_mirroring::get_mirrored)
                .map_or(unicode, |c| c as u32)
        } else {
            unicode
        };
        let normalized = if is_rtl {
            Some(rtl_normalization(unicode).unwrap_or_else(|| {
                char::from_u32(unicode).map_or_else(String::new, |c| c.to_string())
            }))
        } else {
            ligature_expansion(unicode).map(str::to_string)
        };
        match normalized.filter(|n| !n.is_empty()) {
            Some(pieces) => {
                for c in pieces.chars() {
                    let mut piece = info.clone();
                    piece.char_type = CharType::Piece;
                    piece.unicode = c as u32;
                    self.text_buf.push(c as u32);
                    self.char_list.push(piece);
                }
            }
            None => {
                let mut info = info.clone();
                if is_rtl {
                    info.unicode = unicode;
                }
                self.text_buf.push(unicode);
                self.char_list.push(info);
            }
        }
    }
}

/// Per-glyph TJ kerning of a text object, recovered from glyph positions,
/// plus the character spacing the run was set with.
///
/// hayro exposes positioned glyphs rather than the TJ array, so each gap
/// beyond a glyph's advance is kerning plus `Tc` (plus `Tw` after a space).
/// `Tc` is uniform within a run: a gap shared by a clear majority of at
/// least three glyph pairs is taken as character spacing, otherwise `Tc` is
/// assumed 0, and the rest is kerning. In the common `Tc = 0` case this
/// reproduces PDFium's kerning array exactly.
fn estimate_kernings(obj: &TextObject) -> (Vec<f32>, f32) {
    let n = obj.items.len();
    if n < 2 {
        return (vec![0.0; n.saturating_sub(1)], 0.0);
    }
    let tolerance = (obj.font_size.abs() * 0.001).max(1e-4);
    let single_byte = obj
        .font
        .as_ref()
        .is_none_or(|font| !matches!(font.reader(), CodeReader::Cmap(_)));
    let gaps: Vec<Option<f32>> = (1..n)
        .map(|i| {
            let prev = &obj.items[i - 1];
            // Word spacing (`Tw`) widens the gap after a one-byte code 32;
            // without the code, judge by the character.
            let word_space = if prev.code < SYNTHETIC_CODE_BASE {
                prev.code == 0x20 && single_byte
            } else {
                prev.last_unicode() == 0x20
            };
            (!word_space)
                .then(|| (obj.items[i].origin.x - prev.origin.x) as f32 - obj.char_width(prev))
        })
        .collect();
    let candidates: Vec<f32> = gaps.iter().flatten().copied().collect();
    let char_space = candidates
        .iter()
        .find(|&&g| {
            let agreeing = candidates
                .iter()
                .filter(|&&o| (o - g).abs() <= tolerance)
                .count();
            agreeing >= 3 && agreeing * 2 > candidates.len()
        })
        .copied()
        .filter(|g| g.abs() > tolerance)
        .unwrap_or(0.0);
    let kernings = gaps
        .iter()
        .map(|gap| match gap {
            Some(g) if (g - char_space).abs() > tolerance => g - char_space,
            _ => 0.0,
        })
        .collect();
    (kernings, char_space)
}

/// `CalculateBaseSpace` (without the char-space adjustment): `char_space` in
/// text space, `spacing` the same through the text matrix, `kernings` in
/// `GetFontSizeH` units.
fn calculate_base_space(char_space: f32, spacing: f32, kernings: &[f32], items: usize) -> f32 {
    if char_space == 0.0 || items < 2 {
        return 0.0;
    }
    let mut base_space = spacing;
    let mut has_kerning = false;
    for &kerning in kernings {
        if kerning != 0.0 {
            base_space = base_space.min(kerning + spacing);
            has_kerning = true;
        }
    }
    if base_space < 0.0 || (items == 2 && has_kerning) {
        0.0
    } else {
        base_space
    }
}

fn is_control_char(info: &CharInfo) -> bool {
    matches!(
        info.unicode,
        0x2 | 0x3 | 0x93 | 0x94 | 0x96 | 0x97 | 0x98 | 0xFFFE
    ) && info.char_type != CharType::Hyphen
}

fn is_normal_character(info: &CharInfo) -> bool {
    if info.unicode != 0 {
        !is_control_char(info)
    } else {
        info.char_code != 0
    }
}

fn ligature_expansion(unicode: u32) -> Option<&'static str> {
    Some(match unicode {
        0xFB00 => "ff",
        0xFB01 => "fi",
        0xFB02 => "fl",
        0xFB03 => "ffi",
        0xFB04 => "ffl",
        0xFB05 => "\u{17F}t",
        0xFB06 => "st",
        _ => return None,
    })
}

/// PDFium normalizes every character of a right-to-left segment. Its table
/// is a compatibility decomposition; NFKC over the presentation-form blocks
/// gives the same base letters.
fn rtl_normalization(unicode: u32) -> Option<String> {
    use unicode_normalization::UnicodeNormalization;
    if let Some(expansion) = ligature_expansion(unicode) {
        return Some(expansion.to_string());
    }
    if !matches!(unicode, 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF) {
        return None;
    }
    let c = char::from_u32(unicode)?;
    let normalized: String = std::iter::once(c).nfkc().collect();
    (normalized.chars().count() > 0 && normalized != c.to_string()).then_some(normalized)
}

fn end_horizontal_line(this_rect: &FRect, prev_rect: &FRect) -> bool {
    if this_rect.height() <= 4.5 || prev_rect.height() <= 4.5 {
        return false;
    }
    let top = this_rect.top.min(prev_rect.top);
    let bottom = this_rect.bottom.max(prev_rect.bottom);
    bottom >= top
}

fn end_vertical_line(
    this_rect: &FRect,
    prev_rect: &FRect,
    curline_rect: &FRect,
    this_fontsize: f32,
    prev_fontsize: f32,
) -> bool {
    if this_rect.width() <= this_fontsize * 0.1 || prev_rect.width() <= prev_fontsize * 0.1 {
        return false;
    }
    let left = this_rect.left.max(curline_rect.left);
    let right = this_rect.right.min(curline_rect.right);
    right <= left
}

fn generate_space(
    pos: Point,
    last_pos: f32,
    this_width: f32,
    last_width: f32,
    threshold: f32,
) -> bool {
    let px = pos.x as f32;
    if (last_pos + last_width - px).abs() <= threshold {
        return false;
    }
    let threshold_pos = threshold + last_width;
    let pos_difference = px - last_pos;
    if pos_difference.abs() > threshold_pos {
        return true;
    }
    if px < 0.0 && -threshold_pos > pos_difference {
        return true;
    }
    pos_difference > this_width + last_width
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Neutral,
    Left,
    Right,
    LeftWeak,
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    start: usize,
    count: usize,
    direction: Direction,
}

fn direction_of(unicode: u32) -> Direction {
    // Most text comes from the first few blocks; classify those once.
    static LOW: OnceLock<[Direction; 0x800]> = OnceLock::new();
    let low = LOW.get_or_init(|| std::array::from_fn(|c| classify_direction(c as u32)));
    match low.get(unicode as usize) {
        Some(&direction) => direction,
        None => classify_direction(unicode),
    }
}

fn classify_direction(unicode: u32) -> Direction {
    let Some(c) = char::from_u32(unicode) else {
        return Direction::Neutral;
    };
    match unicode_bidi::bidi_class(c) {
        BidiClass::L => Direction::Left,
        BidiClass::AN
        | BidiClass::EN
        | BidiClass::NSM
        | BidiClass::CS
        | BidiClass::ES
        | BidiClass::ET
        | BidiClass::BN => Direction::LeftWeak,
        BidiClass::R | BidiClass::AL => Direction::Right,
        _ => Direction::Neutral,
    }
}

/// `CFX_BidiString` without automatic ordering: direction segments in
/// logical order, reversed (with a right-to-left overall direction) only
/// when `force_right` is set.
fn bidi_segments(text: &[u32], force_right: bool) -> (Vec<Segment>, bool) {
    let mut segments: Vec<Segment> = Vec::new();
    for (i, &c) in text.iter().enumerate() {
        let direction = direction_of(c);
        match segments.last_mut() {
            Some(last) if last.direction == direction => last.count += 1,
            _ => segments.push(Segment {
                start: i,
                count: 1,
                direction,
            }),
        }
    }
    if force_right {
        segments.reverse();
    }
    (segments, force_right)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transform_distance_matches_pdfium() {
        let m = Affine::new([2.0, 0.0, 0.0, 4.0, 0.0, 0.0]);
        assert_eq!(transform_distance(&m, 1.0), 3.0);
        assert_eq!(transform_x_distance(&m, 1.5), 3.0);
    }

    #[test]
    fn generate_space_needs_a_gap_beyond_threshold() {
        // Next glyph starts right where the previous one ended.
        assert!(!generate_space(Point::new(10.0, 0.0), 5.0, 5.0, 5.0, 1.0));
        // A word-sized gap.
        assert!(generate_space(Point::new(14.0, 0.0), 5.0, 5.0, 5.0, 1.0));
    }

    #[test]
    fn bidi_segments_keep_logical_order_unless_forced() {
        let mixed: Vec<u32> = "ab שלום".chars().map(|c| c as u32).collect();
        let (segments, right) = bidi_segments(&mixed, false);
        assert!(!right);
        let directions: Vec<Direction> = segments.iter().map(|s| s.direction).collect();
        assert_eq!(
            directions,
            [Direction::Left, Direction::Neutral, Direction::Right]
        );
        let (segments, right) = bidi_segments(&mixed, true);
        assert!(right);
        assert_eq!(segments[0].direction, Direction::Right);
    }

    #[test]
    fn base_space_follows_pdfium() {
        // No character spacing: nothing to subtract.
        assert_eq!(calculate_base_space(0.0, 0.0, &[0.0, 2.0], 3), 0.0);
        // Uniform character spacing, no kerning.
        assert_eq!(calculate_base_space(1.5, 1.5, &[0.0, 0.0], 3), 1.5);
        // Kerning below the char spacing lowers the base.
        assert_eq!(calculate_base_space(1.5, 1.5, &[-0.5, 0.0], 3), 1.0);
        // A two-glyph run with kerning has no base.
        assert_eq!(calculate_base_space(1.5, 1.5, &[-0.5], 2), 0.0);
    }

    #[test]
    fn rtl_normalization_maps_presentation_forms() {
        // ARABIC LETTER BEH INITIAL FORM → ARABIC LETTER BEH.
        assert_eq!(rtl_normalization(0xFE91).as_deref(), Some("\u{0628}"));
        assert_eq!(rtl_normalization('a' as u32), None);
    }
}
