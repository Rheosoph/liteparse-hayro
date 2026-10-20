//! The text-showing operator behind each glyph run.
//!
//! hayro hands devices positioned glyphs, with the font size folded into the
//! glyph transform and without the character codes they were shown with.
//! PDFium's text API reports both: the `Tf` operand as the font size (with
//! a text matrix that excludes it) and the codes. Many producers set `/F1 1
//! Tf` and scale with `Tm` (IRS forms, for one), and PDFium's text-page
//! heuristics compare the raw size with page-space distances, so the split
//! shows in the output; liteparse also looks widths up by code.
//!
//! This pass replays the text state of a content stream the way hayro's
//! interpreter does, recursing into form XObjects at the same points, and
//! records every text-showing operator with its size and the codes hayro
//! will read from its strings. The device then matches each glyph run to its
//! operator by font and glyph transform.

use std::ops::Range;

use foldhash::HashMap;
use hayro::hayro_interpret::CacheKey;
use hayro::hayro_syntax::content::TypedIter;
use hayro::hayro_syntax::content::ops::TypedInstruction;
use hayro::hayro_syntax::object::{Array, Dict, Name, Object, Stream};
use hayro::hayro_syntax::page::Resources;
use hayro::kurbo::{Affine, Vec2};

use super::char_metrics::CodeReader;
use super::font::{Font, FontCatalog};
use super::types::Color;

/// Measures a font's 'A' and 'g' glyphs (see [`Font::set_glyph_extent`]),
/// given the resources and the name a `Tf` selected it by.
pub(crate) type GlyphExtentProbe<'p, 'a> = dyn FnMut(&Resources<'a>, &[u8], &Font) + 'p;

/// hayro's `MAX_NESTED_INTERPRETATION_DEPTH`.
const MAX_DEPTH: u32 = 50;

/// PDFium's view of a text object's size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TextSize {
    /// The `Tf` operand (`CPDF_TextObject::GetFontSize`).
    pub size: f32,
    /// `CPDF_TextState::GetFontSizeH`: the size scaled by the horizontal
    /// extent of `Th × Tm × CTM`, where the CTM is that of the content stream
    /// being parsed (a form starts from its own `/Matrix`).
    pub size_h: f32,
}

/// What the content scan knows about a glyph run.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ShowInfo<'s> {
    /// The operator's index, for [`TextShows::set_drift`].
    pub index: usize,
    /// The operator whose end position this one starts from, when no text
    /// positioning came between them.
    pub prev: Option<usize>,
    pub size: TextSize,
    /// Cache key of the font dictionary the operator used; `None` when hayro
    /// substitutes Helvetica.
    pub font: Option<u128>,
    /// The character code of each glyph, when the run has one glyph per
    /// code read.
    pub codes: Option<&'s [u32]>,
    /// With `codes`: the TJ adjustment after each glyph, as PDFium keeps it
    /// (`char_kernings_`: on the last glyph of every string but the last).
    pub kernings: Option<&'s [f32]>,
    /// `Tc`.
    pub char_space: f32,
    /// TJ adjustments after the last string, which PDFium drops but hayro
    /// moves the text position by.
    pub trailing_kerning: f32,
    /// The fill and stroke colors of the graphics state, which PDFium
    /// reports whether or not the text paints with them (`None` for color
    /// spaces this scan does not convert).
    pub fill: Option<Color>,
    pub stroke: Option<Color>,
    /// The form matrix PDFium processes the text object with: form space →
    /// page space, identity outside form XObjects.
    pub form: Affine,
    /// Whether the text is inside a form XObject (PDFium's page-level object
    /// list then holds the form, not the text).
    pub in_form: bool,
    /// Clip-only text (`7 Tr`) the object pass shows invisibly (see
    /// [`super::clip_text`]).
    pub clip_only: bool,
}

/// One text-showing operator that paints glyphs.
#[derive(Debug, Clone)]
struct Show {
    font: Option<u128>,
    size: TextSize,
    /// Also indexes `kernings`.
    codes: Range<usize>,
    char_space: f32,
    trailing_kerning: f32,
    fill: Option<Color>,
    stroke: Option<Color>,
    prev: Option<usize>,
    form: Affine,
    in_form: bool,
    clip_only: bool,
}

/// The text-showing operators of one interpretation pass, in order.
#[derive(Debug, Default)]
pub(crate) struct TextShows {
    shows: Vec<Show>,
    /// Character codes of all shows, back to back.
    codes: Vec<u32>,
    /// The TJ adjustment after each code in `codes`.
    kernings: Vec<f32>,
    /// Indices into `shows` (ascending) by the bits of the linear part of
    /// the glyph transform hayro reports: `Tm × [Tfs·Th 0 0 Tfs] / 1000`.
    by_linear: HashMap<[u64; 4], Vec<usize>>,
    /// Shows before this index are matched or skipped.
    cursor: usize,
    /// Page-space offset of PDFium's glyph positions from hayro's at the end
    /// of each matched show.
    drift: HashMap<usize, Vec2>,
}

impl TextShows {
    /// Scan the content stream `ops`, interpreted with `resources`; `ctm` is
    /// the CTM it starts with in PDFium's terms (identity for page content,
    /// the `/Matrix` of an appearance stream) and `form` the form matrix it
    /// is placed with, `None` for page content. With `revealed_clip_text`
    /// the stream is interpreted with its clip-only text made invisible
    /// instead.
    pub(crate) fn scan<'a>(
        ops: TypedIter<'_>,
        resources: &Resources<'a>,
        ctm: Affine,
        form: Option<Affine>,
        fonts: &FontCatalog,
        revealed_clip_text: bool,
        probe: &mut GlyphExtentProbe<'_, 'a>,
    ) -> Self {
        let mut scanner = Scanner {
            shows: Self::default(),
            fonts,
            probe,
            forms: Vec::new(),
        };
        let mut state = State {
            ctm,
            form: form.unwrap_or(Affine::IDENTITY),
            in_form: form.is_some(),
            reveal_clip: revealed_clip_text,
            tm: Affine::IDENTITY,
            font: None,
            reader: CodeReader::Fallback,
            size: 1.0,
            h_scale: 100.0,
            char_space: 0.0,
            render_mode: 0,
            fill: ColorSlot::default(),
            stroke: ColorSlot::default(),
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            last_show: None,
        };
        scanner.scan(ops, resources, &mut state, 0);
        scanner.shows
    }

    /// The next text-showing operator whose glyph transform has the linear
    /// part `linear` (the first glyph's, as hayro reports it) and whose font
    /// is `font` (`None` matches any font, for Type3 glyphs). An operator
    /// that read `glyphs` codes is preferred.
    ///
    /// Operators that paint nothing (clipped-only text, hidden optional
    /// content) are skipped over.
    pub(crate) fn take(
        &mut self,
        font: Option<u128>,
        linear: [f64; 4],
        glyphs: usize,
    ) -> Option<ShowInfo<'_>> {
        let candidates = self.by_linear.get(&linear.map(f64::to_bits))?;
        let start = candidates.partition_point(|&index| index < self.cursor);
        let font_matches = |index: &usize| match (self.shows[*index].font, font) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        };
        let matching = candidates[start..].iter().filter(|i| font_matches(i));
        let first = *matching.clone().next()?;
        // A nearby operator that read as many codes as the run has glyphs
        // is the better match; looking further could skip real ones.
        let index = matching
            .take(4)
            .find(|&&index| self.shows[index].codes.len() == glyphs)
            .copied()
            .unwrap_or(first);
        self.cursor = index + 1;
        let show = &self.shows[index];
        Some(ShowInfo {
            index,
            prev: show.prev,
            size: show.size,
            font: show.font,
            codes: (show.codes.len() == glyphs).then(|| &self.codes[show.codes.clone()]),
            kernings: (show.codes.len() == glyphs).then(|| &self.kernings[show.codes.clone()]),
            char_space: show.char_space,
            trailing_kerning: show.trailing_kerning,
            fill: show.fill,
            stroke: show.stroke,
            form: show.form,
            in_form: show.in_form,
            clip_only: show.clip_only,
        })
    }

    /// The drift a show continuing from `prev` starts with.
    pub(crate) fn carry(&self, prev: Option<usize>) -> Vec2 {
        prev.and_then(|p| self.drift.get(&p).copied())
            .unwrap_or(Vec2::ZERO)
    }

    pub(crate) fn set_drift(&mut self, index: usize, drift: Vec2) {
        if drift != Vec2::ZERO {
            self.drift.insert(index, drift);
        }
    }
}

/// The parts of hayro's (and PDFium's) graphics state that determine a text
/// object's size, font and codes.
#[derive(Clone)]
struct State {
    /// CTM relative to the current content stream.
    ctm: Affine,
    /// Form space of the current content stream → page space: the CTMs the
    /// enclosing `Do`s ran with (`CPDF_FormObject::form_matrix`).
    form: Affine,
    in_form: bool,
    /// Whether `7 Tr` text of this stream reaches the device (as invisible
    /// text).
    reveal_clip: bool,
    /// Linear part of the text matrix: only `Tm` and `BT` change it.
    tm: Affine,
    font: Option<u128>,
    reader: CodeReader,
    size: f32,
    /// `Tz` operand, in percent.
    h_scale: f32,
    /// `Tc`.
    char_space: f32,
    render_mode: i64,
    fill: ColorSlot,
    stroke: ColorSlot,
    /// `ca` and `CA`.
    fill_alpha: f32,
    stroke_alpha: f32,
    /// The last show, while the text position still follows from it.
    last_show: Option<usize>,
}

impl State {
    /// The linear part of hayro's `full_transform() × scale(1 / 1000)`,
    /// computed with the same operations so the bits agree.
    fn glyph_linear(&self) -> [f64; 4] {
        let size = self.size as f64;
        let h_scale = (self.h_scale / 100.0) as f64;
        let glyph = self.tm
            * Affine::new([size * h_scale, 0.0, 0.0, size, 0.0, 0.0])
            * Affine::scale(1.0 / 1000.0);
        let [a, b, c, d, ..] = glyph.as_coeffs();
        [a, b, c, d]
    }

    fn size_h(&self) -> f32 {
        let h_scale = (self.h_scale / 100.0) as f64;
        let matrix = self.ctm * self.tm * Affine::scale_non_uniform(h_scale, 1.0);
        let [a, b, ..] = matrix.as_coeffs();
        ((a as f32).hypot(b as f32) * self.size).abs()
    }

    /// `Tf` (or an ExtGState `/Font`) with the font dictionary `dict`.
    fn set_font(&mut self, dict: Option<Dict<'_>>, size: f32, fonts: &FontCatalog) {
        self.size = size;
        let font = dict.map(|dict| {
            let key = dict.cache_key();
            let reader = match fonts.known(key) {
                Some(font) => font.reader().clone(),
                None => CodeReader::from_dict(&dict),
            };
            (key, reader)
        });
        // hayro shows text in a font it cannot load in Helvetica instead.
        (self.font, self.reader) = match font {
            Some((key, reader)) if !matches!(reader, CodeReader::Fallback) => (Some(key), reader),
            _ => (None, CodeReader::Fallback),
        };
    }
}

struct Scanner<'f, 'p, 'a> {
    shows: TextShows,
    fonts: &'f FontCatalog,
    probe: &'f mut GlyphExtentProbe<'p, 'a>,
    /// Cache keys of the forms being scanned, to cut reference cycles.
    forms: Vec<u128>,
}

impl<'a> Scanner<'_, '_, 'a> {
    fn scan(
        &mut self,
        mut ops: TypedIter<'_>,
        resources: &Resources<'a>,
        state: &mut State,
        depth: u32,
    ) {
        let mut saved: Vec<State> = Vec::new();
        while let Some(op) = ops.next() {
            match op {
                TypedInstruction::SaveState(_) => saved.push(state.clone()),
                TypedInstruction::RestoreState(_) => {
                    if let Some(previous) = saved.pop() {
                        *state = previous;
                    }
                }
                TypedInstruction::Transform(t) => {
                    state.ctm *= Affine::new([
                        t.0.as_f64(),
                        t.1.as_f64(),
                        t.2.as_f64(),
                        t.3.as_f64(),
                        t.4.as_f64(),
                        t.5.as_f64(),
                    ]);
                }
                TypedInstruction::BeginText(_) => {
                    state.tm = Affine::IDENTITY;
                    state.last_show = None;
                }
                TypedInstruction::SetTextMatrix(m) => {
                    state.tm = Affine::new([
                        m.0.as_f64(),
                        m.1.as_f64(),
                        m.2.as_f64(),
                        m.3.as_f64(),
                        0.0,
                        0.0,
                    ]);
                    state.last_show = None;
                }
                TypedInstruction::NextLine(_)
                | TypedInstruction::NextLineAndSetLeading(_)
                | TypedInstruction::NextLineUsingLeading(_) => state.last_show = None,
                TypedInstruction::TextFont(t) => {
                    state.set_font(resources.get_font(t.0), t.1.as_f32(), self.fonts);
                    if let Some(font) = state.font.and_then(|key| self.fonts.known(key))
                        && font.needs_glyph_extent()
                    {
                        (self.probe)(resources, t.0.as_ref(), &font);
                    }
                }
                TypedInstruction::SetGraphicsState(gs) => {
                    if let Some(gs) = resources.get_ext_g_state(gs.0) {
                        if let Some((font, size)) = ext_g_state_font(&gs) {
                            state.set_font(Some(font), size, self.fonts);
                        }
                        if let Some(alpha) = gs.get::<f32>(b"ca") {
                            state.fill_alpha = alpha;
                        }
                        if let Some(alpha) = gs.get::<f32>(b"CA") {
                            state.stroke_alpha = alpha;
                        }
                    }
                }
                TypedInstruction::NonStrokeColorDeviceGray(c) => {
                    state.fill.set(Space::Gray, &[c.0.as_f32()])
                }
                TypedInstruction::StrokeColorDeviceGray(c) => {
                    state.stroke.set(Space::Gray, &[c.0.as_f32()])
                }
                TypedInstruction::NonStrokeColorDeviceRgb(c) => {
                    state
                        .fill
                        .set(Space::Rgb, &[c.0.as_f32(), c.1.as_f32(), c.2.as_f32()]);
                }
                TypedInstruction::StrokeColorDeviceRgb(c) => {
                    state
                        .stroke
                        .set(Space::Rgb, &[c.0.as_f32(), c.1.as_f32(), c.2.as_f32()]);
                }
                TypedInstruction::NonStrokeColorCmyk(c) => {
                    state.fill.set(
                        Space::Cmyk,
                        &[c.0.as_f32(), c.1.as_f32(), c.2.as_f32(), c.3.as_f32()],
                    );
                }
                TypedInstruction::StrokeColorCmyk(c) => {
                    state.stroke.set(
                        Space::Cmyk,
                        &[c.0.as_f32(), c.1.as_f32(), c.2.as_f32(), c.3.as_f32()],
                    );
                }
                TypedInstruction::ColorSpaceNonStroke(c) => {
                    state.fill.select(color_space(c.0, resources))
                }
                TypedInstruction::ColorSpaceStroke(c) => {
                    state.stroke.select(color_space(c.0, resources))
                }
                TypedInstruction::NonStrokeColor(c) => {
                    let space = state.fill.space;
                    state
                        .fill
                        .set(space, &c.0.iter().map(|n| n.as_f32()).collect::<Vec<_>>());
                }
                TypedInstruction::StrokeColor(c) => {
                    let space = state.stroke.space;
                    state
                        .stroke
                        .set(space, &c.0.iter().map(|n| n.as_f32()).collect::<Vec<_>>());
                }
                TypedInstruction::NonStrokeColorNamed(c) => {
                    let space = state.fill.space;
                    state
                        .fill
                        .set(space, &c.0.iter().map(|n| n.as_f32()).collect::<Vec<_>>());
                }
                TypedInstruction::StrokeColorNamed(c) => {
                    let space = state.stroke.space;
                    state
                        .stroke
                        .set(space, &c.0.iter().map(|n| n.as_f32()).collect::<Vec<_>>());
                }
                TypedInstruction::HorizontalScaling(h) => state.h_scale = h.0.as_f32(),
                TypedInstruction::CharacterSpacing(c) => state.char_space = c.0.as_f32(),
                TypedInstruction::TextRenderingMode(r) => state.render_mode = r.0.as_i64(),
                TypedInstruction::ShowText(s) => self.show(state, &[Element::Text(s.0.as_bytes())]),
                TypedInstruction::NextLineAndShowText(s) => {
                    state.last_show = None;
                    self.show(state, &[Element::Text(s.0.as_bytes())]);
                }
                TypedInstruction::ShowTextWithParameters(t) => {
                    state.last_show = None;
                    state.char_space = t.1.as_f32();
                    self.show(state, &[Element::Text(t.2.as_bytes())]);
                }
                TypedInstruction::ShowTexts(s) => {
                    let objects: Vec<_> = s.0.iter::<Object<'_>>().collect();
                    let elements: Vec<Element<'_>> = objects
                        .iter()
                        .filter_map(|o| match o {
                            Object::String(s) => Some(Element::Text(s.as_bytes())),
                            Object::Number(n) => Some(Element::Adjust(n.as_f32())),
                            _ => None,
                        })
                        .collect();
                    self.show(state, &elements);
                }
                TypedInstruction::XObject(x) => {
                    if depth + 1 < MAX_DEPTH
                        && let Some(stream) = resources.get_x_object(x.0)
                    {
                        self.form(&stream, resources, state, depth + 1);
                    }
                }
                _ => {}
            }
        }
    }

    /// A text-showing operator over `elements`, recorded if hayro paints
    /// glyphs for it.
    fn show(&mut self, state: &mut State, elements: &[Element<'_>]) {
        // `7 Tr` only clips: hayro hands the device nothing, unless the
        // stream was rewritten to show it.
        let clip_only = state.render_mode == 7;
        if clip_only && !state.reveal_clip {
            if elements
                .iter()
                .any(|e| matches!(e, Element::Text(s) if !s.is_empty()))
            {
                state.last_show = None;
            }
            return;
        }
        let start = self.shows.codes.len();
        let mut unseen = false;
        // `Handle_ShowText_Positioning`: adjustments before the first string
        // move the start; later ones attach to the last glyph before them,
        // except those after the last string, which PDFium drops.
        let mut pending = 0.0_f32;
        for element in elements {
            match *element {
                Element::Adjust(value) => pending += value,
                Element::Text(bytes) => {
                    // In its Helvetica fallback hayro shows ASCII strings
                    // only (and still advances over the others).
                    if bytes.is_empty()
                        || (matches!(state.reader, CodeReader::Fallback) && !bytes.is_ascii())
                    {
                        unseen |= !bytes.is_empty();
                        continue;
                    }
                    if self.shows.codes.len() > start {
                        *self.shows.kernings.last_mut().unwrap() += pending;
                    }
                    pending = 0.0;
                    state.reader.read(bytes, &mut self.shows.codes);
                    self.shows.kernings.resize(self.shows.codes.len(), 0.0);
                }
            }
        }
        if self.shows.codes.len() == start {
            if unseen {
                state.last_show = None;
            }
            return;
        }
        let index = self.shows.shows.len();
        let show = Show {
            font: state.font,
            size: TextSize {
                size: state.size,
                size_h: state.size_h(),
            },
            codes: start..self.shows.codes.len(),
            char_space: state.char_space,
            trailing_kerning: pending,
            fill: state.fill.color(state.fill_alpha),
            stroke: state.stroke.color(state.stroke_alpha),
            prev: state.last_show,
            form: state.form,
            in_form: state.in_form,
            clip_only,
        };
        let shows = &mut self.shows;
        shows
            .by_linear
            .entry(state.glyph_linear().map(f64::to_bits))
            .or_default()
            .push(index);
        shows.shows.push(show);
        state.last_show = (!unseen).then_some(index);
    }

    /// `Do` with a form XObject, as hayro's `FormXObject::draw` runs it.
    fn form(&mut self, stream: &Stream<'a>, resources: &Resources<'a>, state: &State, depth: u32) {
        let dict = stream.dict();
        if dict.get::<Name<'_>>(b"Subtype").as_deref() != Some(&b"Form"[..])
            || dict.get::<[f32; 4]>(b"BBox").is_none()
        {
            return;
        }
        let key = dict.cache_key();
        if self.forms.contains(&key) {
            return;
        }
        let Ok(data) = stream.decoded() else {
            return;
        };
        let matrix = dict
            .get::<[f64; 6]>(b"Matrix")
            .map(Affine::new)
            .unwrap_or(Affine::IDENTITY);
        let own = dict.get::<Dict<'_>>(b"Resources").map(Resources::new);
        // PDFium parses a form with a fresh CTM that starts at `/Matrix`;
        // everything else in the text state carries over.
        let mut inner = state.clone();
        inner.form = state.form * state.ctm;
        inner.in_form = true;
        inner.reveal_clip = false;
        inner.ctm = matrix;
        self.forms.push(key);
        self.scan(
            TypedIter::new(&data),
            own.as_ref().unwrap_or(resources),
            &mut inner,
            depth,
        );
        self.forms.pop();
    }
}

/// The color spaces this scan converts, as `CPDF_DeviceCS` does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Space {
    #[default]
    Gray,
    Rgb,
    Cmyk,
    Other,
}

/// One of the graphics state's two colors, as RGB.
#[derive(Debug, Clone, Copy)]
struct ColorSlot {
    space: Space,
    rgb: Option<[f32; 3]>,
}

impl Default for ColorSlot {
    fn default() -> Self {
        Self {
            space: Space::Gray,
            rgb: Some([0.0; 3]),
        }
    }
}

impl ColorSlot {
    /// `cs`/`CS`: the space's initial color.
    fn select(&mut self, space: Space) {
        self.space = space;
        self.rgb = (space != Space::Other).then_some([0.0; 3]);
    }

    fn set(&mut self, space: Space, c: &[f32]) {
        let v = |i: usize| c.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0);
        let space = match (space, c.len()) {
            (Space::Other, 1) => Space::Gray,
            (Space::Other, 3) => Space::Rgb,
            (Space::Other, 4) => Space::Cmyk,
            (space, _) => space,
        };
        self.rgb = match space {
            Space::Gray => Some([v(0); 3]),
            Space::Rgb => Some([v(0), v(1), v(2)]),
            // A plain conversion; PDFium's Adobe CMYK table differs slightly.
            Space::Cmyk => {
                let k = 1.0 - v(3);
                Some([(1.0 - v(0)) * k, (1.0 - v(1)) * k, (1.0 - v(2)) * k])
            }
            Space::Other => None,
        };
    }

    /// `FPDFText_GetFillColor`/`GetStrokeColor`.
    fn color(&self, alpha: f32) -> Option<Color> {
        let [r, g, b] = self.rgb?;
        let byte = |v: f32| (v * 255.0).round() as u8;
        Some(Color {
            r: byte(r),
            g: byte(g),
            b: byte(b),
            a: byte(alpha.clamp(0.0, 1.0)),
        })
    }
}

/// The converted space behind a `cs`/`CS` operand.
fn color_space(name: &Name<'_>, resources: &Resources<'_>) -> Space {
    let by_name = |name: &[u8]| match name {
        b"DeviceGray" | b"G" | b"CalGray" => Some(Space::Gray),
        b"DeviceRGB" | b"RGB" | b"CalRGB" => Some(Space::Rgb),
        b"DeviceCMYK" | b"CMYK" => Some(Space::Cmyk),
        b"Pattern" | b"Indexed" | b"I" | b"Separation" | b"DeviceN" | b"Lab" => Some(Space::Other),
        _ => None,
    };
    if let Some(space) = by_name(name.as_ref()) {
        return space;
    }
    match resources.get_color_space(name) {
        Some(Object::Name(name)) => by_name(name.as_ref()).unwrap_or(Space::Other),
        Some(Object::Array(array)) => {
            let mut items = array.iter::<Object<'_>>();
            match items.next() {
                Some(Object::Name(family)) if family.as_ref() == b"ICCBased" => {
                    match items
                        .next()
                        .and_then(|o| o.into_stream())
                        .and_then(|s| s.dict().get::<i32>(b"N"))
                    {
                        Some(1) => Space::Gray,
                        Some(3) => Space::Rgb,
                        Some(4) => Space::Cmyk,
                        _ => Space::Other,
                    }
                }
                Some(Object::Name(family)) => by_name(family.as_ref()).unwrap_or(Space::Other),
                _ => Space::Other,
            }
        }
        _ => Space::Other,
    }
}

/// A string or adjustment of a text-showing operator.
#[derive(Clone, Copy)]
enum Element<'a> {
    Text(&'a [u8]),
    Adjust(f32),
}

/// The `/Font [font size]` entry of an ExtGState.
fn ext_g_state_font<'a>(gs: &Dict<'a>) -> Option<(Dict<'a>, f32)> {
    let array = gs.get::<Array<'_>>(b"Font")?;
    let mut items = array.iter::<Object<'_>>();
    let font = items.next()?.into_dict()?;
    let size = items.next()?.into_number()?.as_f32();
    Some((font, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(content: &[u8]) -> TextShows {
        let fonts = FontCatalog::default();
        TextShows::scan(
            TypedIter::new(content),
            &Resources::new(Dict::empty()),
            Affine::IDENTITY,
            None,
            &fonts,
            false,
            &mut |_, _, _| {},
        )
    }

    /// The linear part hayro reports for a glyph scaled by `(a, d)` in text
    /// space, computed the same way.
    fn linear(a: f64, d: f64) -> [f64; 4] {
        let [a, b, c, d, ..] =
            (Affine::new([a, 0.0, 0.0, d, 0.0, 0.0]) * Affine::scale(1.0 / 1000.0)).as_coeffs();
        [a, b, c, d]
    }

    #[test]
    fn matrix_scaled_text_keeps_its_tf_size() {
        let mut shows =
            scan(b"BT /F1 1 Tf 10 0 0 10 72 700 Tm (Hi) Tj ET BT /F1 10 Tf 72 680 Td (Hi) Tj ET");
        let first = shows.take(None, linear(10.0, 10.0), 2).unwrap();
        assert_eq!(first.size.size, 1.0);
        assert!((first.size.size_h - 10.0).abs() < 1e-4);
        assert_eq!(first.codes, Some(&[b'H' as u32, b'i' as u32][..]));
        let second = shows.take(None, linear(10.0, 10.0), 2).unwrap();
        assert_eq!(second.size.size, 10.0);
        assert!(shows.take(None, linear(10.0, 10.0), 2).is_none());
    }

    #[test]
    fn clip_only_and_empty_shows_are_skipped() {
        let mut shows = scan(b"BT /F1 12 Tf 7 Tr (a) Tj 0 Tr () Tj 2 0 0 2 0 0 Tm (b) Tj ET");
        let first = shows.take(None, linear(24.0, 24.0), 1).unwrap();
        assert_eq!(first.size.size, 12.0);
        assert!(shows.take(None, linear(12.0, 12.0), 1).is_none());
    }

    #[test]
    fn horizontal_scaling_and_ctm_feed_size_h() {
        let mut shows = scan(b"2 0 0 2 0 0 cm BT /F1 5 Tf 50 Tz (a) Tj ET");
        let size = shows.take(None, linear(2.5, 5.0), 1).unwrap().size;
        assert_eq!(size.size, 5.0);
        assert!((size.size_h - 5.0).abs() < 1e-4);
    }

    #[test]
    fn tj_adjustments_attach_like_pdfium() {
        let mut shows = scan(b"BT /F1 10 Tf 0.5 Tc [-100 (ab) 50 () 25 (c) 300] TJ ET");
        let show = shows.take(None, linear(10.0, 10.0), 3).unwrap();
        assert_eq!(
            show.codes,
            Some(&[b'a' as u32, b'b' as u32, b'c' as u32][..])
        );
        assert_eq!(show.kernings, Some(&[0.0, 75.0, 0.0][..]));
        assert_eq!(show.trailing_kerning, 300.0);
        assert_eq!(show.char_space, 0.5);
    }

    #[test]
    fn fallback_font_skips_non_ascii_strings() {
        let mut shows = scan(b"BT /Missing 10 Tf [(ab) -250 (\xe9)] TJ ET");
        let show = shows.take(Some(7), linear(10.0, 10.0), 2).unwrap();
        assert_eq!(show.font, None);
        assert_eq!(show.codes, Some(&[b'a' as u32, b'b' as u32][..]));
    }
}
