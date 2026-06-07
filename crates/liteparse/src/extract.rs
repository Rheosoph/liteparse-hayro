use std::sync::Arc;

use crate::error::LiteParseError;
use crate::types::{Page as LitePage, PdfInput, TextItem};
use hayro::hayro_interpret::font::Glyph;
use hayro::hayro_interpret::hayro_syntax::{Pdf, PdfData};
use hayro::hayro_interpret::util::PageExt;
use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterSettings, Paint,
    PathDrawMode, SoftMask, interpret_page,
};
use kurbo::{Affine, BezPath, Rect, Shape};

pub(crate) type Document = Pdf;

/// Bounding box of an embedded image object on a page.
/// Coordinates are in viewport space with top-left origin.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ImageBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy)]
struct RectF {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl RectF {
    fn from_kurbo(rect: Rect) -> Option<Self> {
        let left = rect.x0.min(rect.x1) as f32;
        let right = rect.x0.max(rect.x1) as f32;
        let top = rect.y0.min(rect.y1) as f32;
        let bottom = rect.y0.max(rect.y1) as f32;

        if !left.is_finite()
            || !right.is_finite()
            || !top.is_finite()
            || !bottom.is_finite()
            || right <= left
            || bottom <= top
        {
            return None;
        }

        Some(Self {
            left,
            top,
            right,
            bottom,
        })
    }

    fn width(self) -> f32 {
        self.right - self.left
    }

    fn height(self) -> f32 {
        self.bottom - self.top
    }
}

#[derive(Debug, Clone)]
struct GlyphItem {
    text: char,
    loose: RectF,
    strict: RectF,
    invisible: bool,
    rotation_deg: f32,
    font_size: Option<f32>,
    text_width: Option<f32>,
    fill_color: Option<String>,
    stroke_color: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GlyphKey {
    text: char,
    coeffs: [i64; 6],
}

#[derive(Default)]
struct PageContentCollector {
    glyphs: Vec<GlyphItem>,
    image_bounds: Vec<ImageBounds>,
    last_glyph: Option<GlyphKey>,
}

/// Open a PDF from path or bytes with an optional password.
pub(crate) fn load_document_from_input(
    input: &PdfInput,
    password: Option<&str>,
) -> Result<Document, LiteParseError> {
    let bytes = match input {
        PdfInput::Path(path) => std::fs::read(path)?,
        PdfInput::Bytes(data) => data.clone(),
    };

    let data: PdfData = Arc::new(bytes);
    let password = password.unwrap_or_default();

    if password.is_empty() {
        Pdf::new(data).map_err(|e| LiteParseError::Pdf(format!("{e:?}")))
    } else {
        Pdf::new_with_password(data, password).map_err(|e| LiteParseError::Pdf(format!("{e:?}")))
    }
}

/// Extract pages from a `PdfInput` (file path or bytes) with filtering.
pub fn extract_pages_from_input(
    input: &PdfInput,
    target_pages: Option<&[u32]>,
    max_pages: usize,
    password: Option<&str>,
) -> Result<Vec<LitePage>, LiteParseError> {
    let document = load_document_from_input(input, password)?;
    extract_pages_from_document(&document, target_pages, max_pages)
}

/// Extract pages from an already-open hayro document.
pub(crate) fn extract_pages_from_document(
    document: &Document,
    target_pages: Option<&[u32]>,
    max_pages: usize,
) -> Result<Vec<LitePage>, LiteParseError> {
    let mut pages = Vec::new();

    for (page_index, page) in document.pages().iter().enumerate() {
        let page_number = page_index as u32 + 1;

        if let Some(targets) = target_pages
            && !targets.contains(&page_number)
        {
            continue;
        }

        if pages.len() >= max_pages {
            break;
        }

        let (page_width, page_height) = page.render_dimensions();
        let content = collect_page_content(page);
        let text_items = extract_page_text_items(content.glyphs);

        pages.push(LitePage {
            page_number: page_number as usize,
            page_width,
            page_height,
            text_items,
        });
    }

    Ok(pages)
}

/// Extract raw text items and print each page as a JSON-line object to stdout.
pub fn extract(pdf_path: &str, page_num: Option<u32>) -> Result<(), LiteParseError> {
    let target_pages: Option<Vec<u32>> = page_num.map(|p| vec![p]);
    let pages = extract_pages_from_input(
        &PdfInput::Path(pdf_path.to_string()),
        target_pages.as_deref(),
        usize::MAX,
        None,
    )?;
    for page in &pages {
        println!("{}", serde_json::to_string(page)?);
    }
    Ok(())
}

pub(crate) fn image_bounds_for_page(
    document: &Document,
    page_index: usize,
    min_size_pt: f32,
    max_page_coverage: f32,
) -> Result<Vec<ImageBounds>, LiteParseError> {
    let page = document
        .pages()
        .get(page_index)
        .ok_or_else(|| LiteParseError::Other(format!("page {} out of range", page_index + 1)))?;
    let (page_width, page_height) = page.render_dimensions();
    let page_area = page_width * page_height;

    let bounds = collect_page_content(page)
        .image_bounds
        .into_iter()
        .filter(|b| b.width >= min_size_pt && b.height >= min_size_pt)
        .filter(|b| {
            page_area <= 0.0
                || !(b.width > page_width * max_page_coverage
                    && b.height > page_height * max_page_coverage)
        })
        .collect();

    Ok(bounds)
}

fn collect_page_content(
    page: &hayro::hayro_interpret::hayro_syntax::page::Page<'_>,
) -> PageContentCollector {
    let (width, height) = page.render_dimensions();
    let initial_transform = page.initial_transform(true);
    let settings = InterpreterSettings::default();
    let mut context = Context::new(
        initial_transform,
        Rect::new(0.0, 0.0, width as f64, height as f64),
        page.xref(),
        settings,
    );
    let mut collector = PageContentCollector::default();

    interpret_page(page, &mut context, &mut collector);

    collector
}

/// Check if the page has any visible (non-render-mode-3) printable characters.
fn should_skip_invisible(glyphs: &[GlyphItem]) -> bool {
    let mut visible = 0u32;
    let mut invisible = 0u32;

    for glyph in glyphs {
        let c = glyph.text;
        if c.is_whitespace() || c.is_control() {
            continue;
        }
        if glyph.invisible {
            invisible += 1;
        } else {
            visible += 1;
        }
    }

    if visible == 0 || invisible == 0 {
        return false;
    }

    let total = visible + invisible;
    let invisible_ratio = invisible as f64 / total as f64;
    invisible_ratio < 0.3
}

/// Character-level text extraction.
///
/// Glyphs are collected from hayro's interpreter and grouped by spatial
/// proximity. This mirrors liteparse's PDFium-era grouping behavior without
/// requiring the PDFium native library.
fn extract_page_text_items(glyphs: Vec<GlyphItem>) -> Vec<TextItem> {
    if glyphs.is_empty() {
        return Vec::new();
    }

    // Hard limit: gaps larger than this always cause a split (column breaks).
    const MAX_INLINE_GAP: f32 = 15.0;

    let debug = std::env::var("LITEPARSE_DEBUG").is_ok();
    let skip_invisible = should_skip_invisible(&glyphs);

    if debug {
        eprintln!(
            "[extract-debug] glyph_count={}, skip_invisible={skip_invisible}",
            glyphs.len()
        );
    }

    let mut items: Vec<TextItem> = Vec::new();
    let mut seg = SegmentBuilder::new();

    for glyph in glyphs {
        if skip_invisible && glyph.invisible {
            continue;
        }

        let c = glyph.text;

        if c == '\n' || c == '\r' {
            seg.flush(&mut items);
            continue;
        }

        if c == ' ' {
            seg.mark_pending_space();
            continue;
        }

        if c.is_control() {
            continue;
        }

        let (c, ligature_tail): (char, &str) = match c as u32 {
            0x02 => ('-', ""),
            0x1A => ('f', "f"),
            0x1B => ('f', "t"),
            0x1C => ('f', "i"),
            0x1D => ('T', "h"),
            0x1E => ('f', "fi"),
            0x1F => ('f', "l"),
            _ => (c, ""),
        };

        let vp_loose = glyph.loose;
        let vp_strict = glyph.strict;

        // Skip zero-height characters (phantom dots from dot leader decorations).
        if vp_loose.height() < 0.5 {
            continue;
        }

        if seg.has_content {
            let y_tolerance: f32 = 2.0;
            let y_overlap = vp_loose.top < seg.vp_bottom + y_tolerance
                && vp_loose.bottom > seg.vp_top - y_tolerance;

            let gap = vp_strict.left - seg.last_char_right;
            let strict_below = vp_strict.top > seg.last_char_bottom;
            let large_leftward_jump = gap < -5.0;
            let seg_width = seg.vp_right - seg.vp_left;
            let very_large_leftward_jump = seg_width > 20.0 && gap < -(seg_width * 0.5);
            let line_changed = vp_strict.top > seg.last_char_bottom + y_tolerance
                || (strict_below && large_leftward_jump)
                || very_large_leftward_jump;

            let dot_leader_break = if seg.pending_space {
                (c == '.' && seg.has_non_dot_content())
                    || (c != '.' && !seg.has_non_dot_content() && seg.char_count >= 3)
            } else {
                c == '.' && seg.has_non_dot_content() && gap > seg.avg_char_width() * 1.4
            };

            if !y_overlap || line_changed || gap >= MAX_INLINE_GAP || dot_leader_break {
                seg.flush(&mut items);
                seg.start(c, &vp_loose, &vp_strict, &glyph);
                seg.append_ligature_tail(ligature_tail);
            } else if seg.pending_space {
                let avg_cw = seg.avg_char_width();
                if gap > avg_cw * 2.2 {
                    seg.flush(&mut items);
                    seg.start(c, &vp_loose, &vp_strict, &glyph);
                    seg.append_ligature_tail(ligature_tail);
                } else {
                    seg.commit_pending_space();
                    seg.push_char(c, &vp_loose, &vp_strict, &glyph);
                    seg.append_ligature_tail(ligature_tail);
                }
            } else {
                if seg.should_insert_inferred_space(c, gap) {
                    seg.insert_inferred_space();
                }
                seg.push_char(c, &vp_loose, &vp_strict, &glyph);
                seg.append_ligature_tail(ligature_tail);
            }
        } else {
            seg.start(c, &vp_loose, &vp_strict, &glyph);
            seg.append_ligature_tail(ligature_tail);
        }
    }

    seg.flush(&mut items);
    dedup_overlapping_items(&mut items, debug);

    items
}

/// Remove duplicate text items: exact text matches with any bbox overlap,
/// and near-duplicates (different text) with high bbox overlap (>50% area).
fn dedup_overlapping_items(items: &mut Vec<TextItem>, debug: bool) {
    if items.len() < 2 {
        return;
    }

    let mut keep = vec![true; items.len()];
    for i in 0..items.len() {
        if !keep[i] {
            continue;
        }
        for j in (i + 1)..items.len() {
            if !keep[j] {
                continue;
            }

            let a = &items[i];
            let b = &items[j];

            let ix_left = a.x.max(b.x);
            let ix_right = (a.x + a.width).min(b.x + b.width);
            let iy_top = a.y.max(b.y);
            let iy_bottom = (a.y + a.height).min(b.y + b.height);

            if ix_left >= ix_right || iy_top >= iy_bottom {
                continue;
            }

            let intersection = (ix_right - ix_left) * (iy_bottom - iy_top);
            let area_a = a.width * a.height;
            let area_b = b.width * b.height;
            let smaller_area = area_a.min(area_b);

            if items[i].text == items[j].text {
                if debug {
                    eprintln!(
                        "[extract-debug] DEDUP exact-match drop i={i} text='{}'",
                        items[i].text
                    );
                }
                keep[i] = false;
                break;
            } else if smaller_area > 0.0 && intersection / smaller_area > 0.5 {
                let larger_area = area_a.max(area_b);
                if larger_area / smaller_area > 5.0 {
                    continue;
                }
                if debug {
                    eprintln!(
                        "[extract-debug] DEDUP overlap drop i={i} text='{}'",
                        items[i].text
                    );
                }
                keep[i] = false;
                break;
            }
        }
    }

    let mut idx = 0;
    items.retain(|_| {
        let k = keep[idx];
        idx += 1;
        k
    });
}

fn is_buggy_codepoint(unicode: u32) -> bool {
    unicode <= 0x1F || (unicode > 0xE000 && unicode <= 0xF8FF)
}

/// Accumulates characters into a single TextItem segment.
struct SegmentBuilder {
    text: String,
    vp_left: f32,
    vp_right: f32,
    vp_top: f32,
    vp_bottom: f32,
    last_char_right: f32,
    last_char_bottom: f32,
    char_count: usize,
    font_size: Option<f32>,
    font_height: Option<f32>,
    font_is_buggy: bool,
    rotation_deg: f32,
    text_width: f32,
    fill_color: Option<String>,
    stroke_color: Option<String>,
    has_content: bool,
    pending_space: bool,
    last_char: Option<char>,
}

impl SegmentBuilder {
    fn new() -> Self {
        Self {
            text: String::new(),
            vp_left: f32::MAX,
            vp_right: f32::MIN,
            vp_top: f32::MAX,
            vp_bottom: f32::MIN,
            last_char_right: f32::MIN,
            last_char_bottom: f32::MIN,
            char_count: 0,
            font_size: None,
            font_height: None,
            font_is_buggy: false,
            rotation_deg: 0.0,
            text_width: 0.0,
            fill_color: None,
            stroke_color: None,
            has_content: false,
            pending_space: false,
            last_char: None,
        }
    }

    fn avg_char_width(&self) -> f32 {
        if self.char_count == 0 {
            return 5.0;
        }
        if self.text_width > 0.0 {
            self.text_width / self.char_count as f32
        } else {
            (self.vp_right - self.vp_left) / self.char_count as f32
        }
    }

    fn start(&mut self, c: char, vp_loose: &RectF, vp_strict: &RectF, glyph: &GlyphItem) {
        self.text.clear();
        self.text.push(c);
        self.vp_left = vp_loose.left;
        self.vp_right = vp_loose.right;
        self.vp_top = vp_loose.top;
        self.vp_bottom = vp_loose.bottom;
        self.last_char_right = vp_strict.right;
        self.last_char_bottom = vp_strict.bottom;
        self.char_count = 1;
        self.has_content = true;
        self.pending_space = false;
        self.text_width = glyph
            .text_width
            .unwrap_or_else(|| vp_strict.width().max(0.0));
        self.font_size = glyph.font_size.or(Some(vp_loose.height().abs()));
        self.font_height = Some(vp_loose.height().abs());
        self.rotation_deg = glyph.rotation_deg;
        self.fill_color = glyph.fill_color.clone();
        self.stroke_color = glyph.stroke_color.clone();
        self.font_is_buggy = is_buggy_codepoint(c as u32);
        self.last_char = Some(c);
    }

    fn push_char(&mut self, c: char, vp_loose: &RectF, vp_strict: &RectF, glyph: &GlyphItem) {
        self.text.push(c);
        self.vp_left = self.vp_left.min(vp_loose.left);
        self.vp_right = self.vp_right.max(vp_loose.right);
        self.vp_top = self.vp_top.min(vp_loose.top);
        self.vp_bottom = self.vp_bottom.max(vp_loose.bottom);
        self.last_char_right = vp_strict.right;
        self.last_char_bottom = vp_strict.bottom;
        self.char_count += 1;
        self.text_width += glyph
            .text_width
            .unwrap_or_else(|| vp_strict.width().max(0.0));
        self.font_is_buggy |= is_buggy_codepoint(c as u32);
        self.last_char = Some(c);
    }

    fn append_ligature_tail(&mut self, tail: &str) {
        self.text.push_str(tail);
        if let Some(c) = tail.chars().last() {
            self.last_char = Some(c);
        }
    }

    fn has_non_dot_content(&self) -> bool {
        self.text
            .chars()
            .any(|c| c != '.' && c != ' ' && c != '*' && c != '-')
    }

    fn mark_pending_space(&mut self) {
        if self.has_content {
            self.pending_space = true;
        }
    }

    fn commit_pending_space(&mut self) {
        if self.pending_space {
            self.text.push(' ');
            self.pending_space = false;
        }
    }

    fn should_insert_inferred_space(&self, next: char, gap: f32) -> bool {
        if self.pending_space || gap <= 0.0 {
            return false;
        }

        let Some(prev) = self.last_char else {
            return false;
        };

        if prev.is_whitespace()
            || next.is_whitespace()
            || prev == '-'
            || no_space_after(prev)
            || no_space_before(next)
        {
            return false;
        }

        let avg = self.avg_char_width().max(0.1);
        let threshold = (avg * 0.55).clamp(1.2, 4.0);
        gap > threshold
    }

    fn insert_inferred_space(&mut self) {
        if self.has_content && !self.text.ends_with(' ') {
            self.text.push(' ');
        }
    }

    fn flush(&mut self, items: &mut Vec<TextItem>) {
        if !self.has_content {
            return;
        }

        let normalized_text = normalize_segment_text(self.text.trim());
        let trimmed = normalized_text.trim();
        if !trimmed.is_empty() {
            let width = self.vp_right - self.vp_left;
            let height = self.vp_bottom - self.vp_top;

            items.push(TextItem {
                text: trimmed.to_string(),
                x: self.vp_left,
                y: self.vp_top,
                width,
                height,
                rotation: self.rotation_deg,
                font_name: None,
                font_size: self.font_size.or(Some(height)),
                font_height: self.font_height,
                font_ascent: None,
                font_descent: None,
                font_weight: None,
                font_flags: None,
                text_width: if self.text_width > 0.0 {
                    Some(self.text_width)
                } else {
                    None
                },
                font_is_buggy: self.font_is_buggy,
                mcid: None,
                fill_color: self.fill_color.clone(),
                stroke_color: self.stroke_color.clone(),
                confidence: None,
            });
        }

        *self = Self::new();
    }
}

fn no_space_before(c: char) -> bool {
    matches!(
        c,
        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '%' | '/' | '\\' | '\'' | '"'
    )
}

fn no_space_after(c: char) -> bool {
    matches!(c, '(' | '[' | '{' | '/' | '\\' | '\'' | '"')
}

fn normalize_segment_text(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut normalized = String::with_capacity(text.len());
    let mut idx = 0;

    while idx < chars.len() {
        let c = chars[idx];
        if c == ' ' {
            if let Some((next_idx, next)) = next_non_space(&chars, idx + 1) {
                if should_collapse_space_before_punctuation(next)
                    || should_collapse_domain_space_after_dot(&normalized, &chars, next_idx)
                {
                    idx += 1;
                    continue;
                }
            }
        }

        normalized.push(c);
        idx += 1;
    }

    normalized
}

fn next_non_space(chars: &[char], start: usize) -> Option<(usize, char)> {
    chars
        .iter()
        .copied()
        .enumerate()
        .skip(start)
        .find(|(_, c)| *c != ' ')
}

fn should_collapse_space_before_punctuation(c: char) -> bool {
    matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '%')
}

fn should_collapse_domain_space_after_dot(output: &str, chars: &[char], next_idx: usize) -> bool {
    if !output.ends_with('.') || !chars[next_idx].is_ascii_lowercase() {
        return false;
    }

    let tld_len = chars[next_idx..]
        .iter()
        .take_while(|c| c.is_ascii_lowercase())
        .count();
    if !(2..=6).contains(&tld_len) {
        return false;
    }

    let after_tld_idx = next_idx + tld_len;
    if chars
        .get(after_tld_idx)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '-')
    {
        return false;
    }

    let Some(before_dot) = output.strip_suffix('.') else {
        return false;
    };
    let token_start = before_dot
        .rfind(|c: char| {
            c.is_whitespace() || matches!(c, '(' | '[' | '{' | '<' | '"' | '\'' | ',' | ';' | ':')
        })
        .map(|idx| idx + 1)
        .unwrap_or(0);
    let token = &before_dot[token_start..];

    token.len() >= 3
        && token.chars().any(|c| c.is_ascii_alphabetic())
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '@')
        && (token.starts_with("www")
            || token.contains('@')
            || token.chars().all(|c| !c.is_ascii_uppercase()))
}

impl<'a> Device<'a> for PageContentCollector {
    fn set_soft_mask(&mut self, _mask: Option<SoftMask<'a>>) {}

    fn set_blend_mode(&mut self, _blend_mode: BlendMode) {}

    fn draw_path(
        &mut self,
        _path: &BezPath,
        _transform: Affine,
        _paint: &Paint<'a>,
        _draw_mode: &PathDrawMode,
    ) {
    }

    fn push_clip_path(&mut self, _clip_path: &ClipPath) {}

    fn push_transparency_group(
        &mut self,
        _opacity: f32,
        _mask: Option<SoftMask<'a>>,
        _blend_mode: BlendMode,
    ) {
    }

    fn draw_glyph(
        &mut self,
        glyph: &Glyph<'a>,
        transform: Affine,
        glyph_transform: Affine,
        paint: &Paint<'a>,
        draw_mode: &GlyphDrawMode,
    ) {
        let Some(text) = glyph.as_unicode() else {
            return;
        };

        let composite = transform * glyph_transform;
        let key = glyph_key(text, composite);
        if matches!(draw_mode, GlyphDrawMode::Stroke(_)) && self.last_glyph == Some(key) {
            return;
        }

        let Some(bounds) = glyph_bounds(glyph, composite, text) else {
            return;
        };

        let coeffs = composite.as_coeffs();
        let scale_y = (coeffs[2].mul_add(coeffs[2], coeffs[3] * coeffs[3])).sqrt();
        let rotation_deg = coeffs[1].atan2(coeffs[0]).to_degrees() as f32;
        let color = paint_to_argb_hex(paint);
        let (fill_color, stroke_color) = match draw_mode {
            GlyphDrawMode::Stroke(_) => (None, color),
            GlyphDrawMode::Fill | GlyphDrawMode::Invisible => (color, None),
        };

        self.glyphs.push(GlyphItem {
            text,
            loose: bounds,
            strict: bounds,
            invisible: matches!(draw_mode, GlyphDrawMode::Invisible),
            rotation_deg,
            font_size: Some((scale_y * 1000.0) as f32).filter(|v| *v > 0.0),
            text_width: Some(bounds.width().max(0.0)),
            fill_color,
            stroke_color,
        });
        self.last_glyph = Some(key);
    }

    fn draw_image(&mut self, image: Image<'a, '_>, transform: Affine) {
        let rect = Rect::new(0.0, 0.0, image.width() as f64, image.height() as f64);
        let bounds = (transform * rect.to_path(0.1)).bounding_box();
        if let Some(bounds) = RectF::from_kurbo(bounds) {
            self.image_bounds.push(ImageBounds {
                x: bounds.left,
                y: bounds.top,
                width: bounds.width(),
                height: bounds.height(),
            });
        }
    }

    fn pop_clip_path(&mut self) {}

    fn pop_transparency_group(&mut self) {}
}

fn glyph_bounds(glyph: &Glyph<'_>, transform: Affine, text: char) -> Option<RectF> {
    let rect = match glyph {
        Glyph::Outline(outline) => {
            let path = outline.outline();
            if path.segments().next().is_some() {
                (transform * path).bounding_box()
            } else {
                fallback_glyph_rect(transform, text)
            }
        }
        Glyph::Type3(_) => fallback_glyph_rect(transform, text),
    };

    RectF::from_kurbo(rect)
}

fn fallback_glyph_rect(transform: Affine, text: char) -> Rect {
    let em_width = if text == ' ' { 250.0 } else { 1000.0 };
    (transform * Rect::new(0.0, -1000.0, em_width, 200.0).to_path(0.1)).bounding_box()
}

fn glyph_key(text: char, transform: Affine) -> GlyphKey {
    let coeffs = transform.as_coeffs().map(|v| (v * 1000.0).round() as i64);
    GlyphKey { text, coeffs }
}

fn paint_to_argb_hex(paint: &Paint<'_>) -> Option<String> {
    let Paint::Color(color) = paint else {
        return None;
    };
    let [r, g, b, a] = color.to_rgba().to_rgba8();
    Some(format!("{a:02x}{r:02x}{g:02x}{b:02x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ti(text: &str, x: f32, y: f32, w: f32, h: f32) -> TextItem {
        TextItem {
            text: text.to_string(),
            x,
            y,
            width: w,
            height: h,
            ..Default::default()
        }
    }

    fn glyph_item(text: char, left: f32, right: f32) -> GlyphItem {
        GlyphItem {
            text,
            loose: RectF {
                left,
                top: 0.0,
                right,
                bottom: 10.0,
            },
            strict: RectF {
                left,
                top: 0.0,
                right,
                bottom: 10.0,
            },
            invisible: false,
            rotation_deg: 0.0,
            font_size: Some(10.0),
            text_width: Some((right - left).max(0.0)),
            fill_color: None,
            stroke_color: None,
        }
    }

    #[test]
    fn dedup_exact_overlap_keeps_later_item() {
        let mut items = vec![ti("A", 0.0, 0.0, 10.0, 10.0), ti("A", 1.0, 1.0, 10.0, 10.0)];
        dedup_overlapping_items(&mut items, false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].x, 1.0);
    }

    #[test]
    fn dedup_keeps_non_overlapping_items() {
        let mut items = vec![
            ti("A", 0.0, 0.0, 10.0, 10.0),
            ti("A", 20.0, 0.0, 10.0, 10.0),
        ];
        dedup_overlapping_items(&mut items, false);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn invisible_only_text_is_kept() {
        let glyphs = vec![GlyphItem {
            text: 'A',
            loose: RectF {
                left: 0.0,
                top: 0.0,
                right: 10.0,
                bottom: 10.0,
            },
            strict: RectF {
                left: 0.0,
                top: 0.0,
                right: 10.0,
                bottom: 10.0,
            },
            invisible: true,
            rotation_deg: 0.0,
            font_size: Some(10.0),
            text_width: Some(10.0),
            fill_color: None,
            stroke_color: None,
        }];

        assert!(!should_skip_invisible(&glyphs));
    }

    #[test]
    fn inferred_space_from_glyph_gap() {
        let glyphs = vec![
            glyph_item('H', 0.0, 5.0),
            glyph_item('i', 5.5, 6.5),
            glyph_item('t', 10.0, 14.0),
            glyph_item('h', 14.2, 18.2),
            glyph_item('e', 18.4, 22.4),
            glyph_item('r', 22.6, 25.6),
            glyph_item('e', 25.8, 29.8),
        ];

        let items = extract_page_text_items(glyphs);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "Hi there");
    }

    #[test]
    fn inferred_space_skips_punctuation() {
        let glyphs = vec![glyph_item('A', 0.0, 5.0), glyph_item(',', 9.0, 11.0)];

        let items = extract_page_text_items(glyphs);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "A,");
    }

    #[test]
    fn normalize_removes_space_before_punctuation() {
        assert_eq!(normalize_segment_text("Smith , John ."), "Smith, John.");
    }

    #[test]
    fn normalize_collapses_domain_dot_spacing() {
        assert_eq!(normalize_segment_text("dkriesel . com"), "dkriesel.com");
    }

    #[test]
    fn normalize_preserves_slash_spacing() {
        assert_eq!(normalize_segment_text("Popoola / Data"), "Popoola / Data");
    }
}
