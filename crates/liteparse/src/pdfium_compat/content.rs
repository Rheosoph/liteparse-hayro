//! One hayro interpretation pass per page, recorded as PDFium-style page
//! objects.
//!
//! The page is interpreted with an identity initial transform, so everything
//! recorded here is in *page space* (unrotated PDF user space, y-up) — the
//! space PDFium's object and text APIs report in. [`super::page`] maps it to
//! the viewport.

use std::collections::HashMap;
use std::rc::Rc;

use hayro::hayro_interpret::font::{Glyph, GlyphRun};
use hayro::hayro_interpret::pattern::Pattern;
use hayro::hayro_interpret::{
    BlendMode, CacheKey, ClipPath, Context, Device, DrawMode, DrawProps, Image, ImageData,
    ImageDrawProps, InterpreterCache, InterpreterSettings, Paint, SoftMask, interpret,
    interpret_page,
};
use hayro::hayro_syntax::content::TypedIter;
use hayro::hayro_syntax::object::{Array, Dict, Name, Object, Stream};
use hayro::hayro_syntax::page::{Page as HayroPage, Resources};
use hayro::kurbo::{Affine, BezPath, Join, PathEl, Point, Rect, Shape, Vec2};

use super::char_metrics::CodeReader;
use super::clip_text::reveal_clip_text;
use super::font::{Font, FontCatalog, FontType, GlyphEntry, SYNTHETIC_CODE_BASE};
use super::path_bounds::path_object_bounds;
use super::text_state::{TextShows, TextSize};
use super::types::Color;

/// Font key used for Type3 glyphs, whose font hayro does not identify.
const TYPE3_FONT_KEY: u128 = u128::MAX;

/// What a content pass should record beyond geometry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct ContentOptions {
    /// Decode image pixels (needed for image extraction).
    pub image_pixels: bool,
    /// Keep raw (still encoded) image streams.
    pub image_streams: bool,
    /// Append visible form-widget appearances after the page content, as
    /// PDFium's flatten does.
    pub flatten_widgets: bool,
}

/// Effective clip for a page object, as far as rectangular clips go.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ClipState {
    /// No clip was active.
    None,
    /// The intersection of the active rectangular clips (page space; may be
    /// empty, i.e. inverted).
    Rect(Rect),
    /// A non-rectangular clip is active; visibility is unknown.
    Unknown,
}

/// One glyph of a text run.
#[derive(Debug, Clone)]
pub(crate) struct RunGlyph {
    /// Unicode text, `None` when the font has no mapping for the glyph.
    pub unicode: Option<GlyphText>,
    /// Character code, or a synthetic one when the content scan could not
    /// place the glyph (see [`SYNTHETIC_CODE_BASE`]).
    pub code: u32,
    /// Glyph id within the font (a pseudo id for Type3 glyphs).
    pub gid: u32,
    /// Glyph space (1/1000 em, y-up) → page space.
    pub transform: Affine,
    /// Ink box in glyph space; `None` for inkless glyphs.
    pub ink: Option<Rect>,
    /// Advance in glyph units, 0 when unknown.
    pub advance: f32,
    /// Whether `advance` is PDFium's width for the glyph's code (possibly
    /// 0), not an estimate.
    pub exact_advance: bool,
}

/// A glyph's Unicode text. It is nearly always a character or two, so it is
/// kept inline instead of allocating per glyph.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum GlyphText {
    Inline {
        len: u8,
        bytes: [u8; GlyphText::INLINE],
    },
    Heap(Box<str>),
}

impl GlyphText {
    const INLINE: usize = 22;

    pub(crate) fn new(text: &str) -> Self {
        if text.len() <= Self::INLINE {
            let mut bytes = [0; Self::INLINE];
            bytes[..text.len()].copy_from_slice(text.as_bytes());
            Self::Inline {
                len: text.len() as u8,
                bytes,
            }
        } else {
            Self::Heap(text.into())
        }
    }

    pub(crate) fn as_str(&self) -> &str {
        match self {
            // Copied from a `str` in `new`, so always valid.
            Self::Inline { len, bytes } => {
                std::str::from_utf8(&bytes[..*len as usize]).unwrap_or_default()
            }
            Self::Heap(text) => text,
        }
    }
}

impl From<char> for GlyphText {
    fn from(c: char) -> Self {
        Self::new(c.encode_utf8(&mut [0; 4]))
    }
}

impl std::ops::Deref for GlyphText {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Debug for GlyphText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}

/// One text-showing operation (`Tj`, `TJ`, `'`, `"`) — PDFium's text object.
#[derive(Debug, Clone)]
pub(crate) struct TextRun {
    pub glyphs: Vec<RunGlyph>,
    /// PDFium text render mode (0 fill, 1 stroke, 2 fill+stroke, 3 invisible).
    pub render_mode: i32,
    pub fill: Option<Color>,
    pub stroke: Option<Color>,
    /// Stroke line width in page units.
    pub line_width: f32,
    pub mcid: Option<i32>,
    pub clip: ClipState,
    pub font_key: u128,
    /// The `Tf` size the run was shown with, when the content scan found its
    /// operator.
    pub size: Option<TextSize>,
    /// Form space → page space for text inside form XObjects (see
    /// [`super::text_state::ShowInfo::form`]).
    pub form: Affine,
    /// Drawn inside a form XObject (or a flattened widget appearance).
    pub in_form: bool,
    /// The operator's TJ adjustment after each glyph and its `Tc`, when the
    /// content scan placed the run's codes.
    pub kernings: Option<(Vec<f32>, f32)>,
    /// Painted by a form-widget appearance (only with `flatten_widgets`).
    pub from_widget: bool,
}

/// A painted vector path.
#[derive(Debug, Clone)]
pub(crate) struct PathRecord {
    /// Page-space path with PDFium's point layout (rectangles as five
    /// points, curves as three control points).
    pub path: BezPath,
    pub filled: bool,
    pub stroked: bool,
    pub fill: Option<Color>,
    pub stroke: Option<Color>,
    /// Line width in page units.
    pub line_width: f32,
    /// The path as drawn, before `ctm`, and the line width in that space
    /// (`FPDFPageObj_GetStrokeWidth`).
    local: BezPath,
    ctm: Affine,
    pub local_line_width: f32,
    /// A zero-area fill hayro handed over as the diagonal of its bounds.
    zero_area: bool,
}

impl PathRecord {
    /// Bounds like `FPDFPageObj_GetBounds` (`CPDF_PathObject::
    /// CalcBoundingBox`).
    pub(crate) fn bounds(&self) -> Rect {
        path_object_bounds(&self.local, self.ctm, self.stroked, self.local_line_width)
    }
}

/// A painted image.
#[derive(Debug, Clone)]
pub(crate) struct ImageRecord {
    /// Unit square → page space.
    pub transform: Affine,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub bits_per_pixel: u32,
    /// PDFium `FPDF_COLORSPACE_*` value.
    pub colorspace: i32,
    /// Raw stream bytes (with `image_streams`).
    pub raw: Option<Vec<u8>>,
    /// Whether the stream is a bare DCT (JPEG) stream.
    pub is_dct: bool,
    /// Decoded RGBA pixels (with `image_pixels`), with their dimensions.
    pub rgba: Option<(u32, u32, Vec<u8>)>,
}

impl ImageRecord {
    pub(crate) fn bounds(&self) -> Rect {
        self.transform
            .transform_rect_bbox(Rect::new(0.0, 0.0, 1.0, 1.0))
    }
}

/// Everything one content pass recorded.
#[derive(Debug, Default)]
pub(crate) struct PageContent {
    pub runs: Vec<TextRun>,
    pub paths: Vec<PathRecord>,
    pub images: Vec<ImageRecord>,
    /// Union of every painted object's bounds (page space).
    pub bounds: Option<Rect>,
    /// Union of the bounds painted under each marked-content id.
    pub mcid_bounds: HashMap<i32, Rect>,
    /// Shading (`sh`) fills, which PDFium reports as shading objects rather
    /// than paths. Kept for content bounds only.
    pub shadings: usize,
}

/// Interpret `page` and record its objects.
pub(crate) fn collect<'a>(
    page: &HayroPage<'a>,
    fonts: &FontCatalog,
    cache: &InterpreterCache<'a>,
    options: ContentOptions,
) -> PageContent {
    let raw = page.raw();
    if let Some(res) = raw.get::<Dict<'_>>(b"Resources") {
        fonts.register_resources(&res);
    } else {
        fonts.register_resources(&resources_dict(page));
    }

    let settings = InterpreterSettings {
        render_annotations: false,
        ..InterpreterSettings::default()
    };
    let media = page.media_box();
    let bbox = Rect::new(media.x0, media.y0, media.x1, media.y1);
    let mut device = Collector::new(fonts, options);
    let mut probe = |resources: &Resources<'a>, name: &[u8], font: &Font| {
        probe_glyph_extent(cache, page.xref(), &settings, resources, name, font);
    };
    let revealed = page.page_stream().and_then(reveal_clip_text);
    device.shows = TextShows::scan(
        page.typed_operations(),
        page.resources(),
        Affine::IDENTITY,
        None,
        fonts,
        revealed.is_some(),
        &mut probe,
    );
    {
        let mut context =
            Context::new(Affine::IDENTITY, bbox, cache, page.xref(), settings.clone());
        match &revealed {
            Some(content) => interpret(
                TypedIter::new(content),
                page.resources(),
                &mut context,
                &mut device,
            ),
            None => interpret_page(page, &mut context, &mut device),
        }
    }

    if options.flatten_widgets {
        for annot in visible_widgets(raw) {
            let Some(appearance) = normal_appearance(&annot) else {
                continue;
            };
            let Some(rect) = annot.get::<[f32; 4]>(b"Rect") else {
                continue;
            };
            fonts.register_stream(&appearance);
            let Some(transform) = appearance_transform(&appearance, rect) else {
                continue;
            };
            let Ok(data) = appearance.decoded() else {
                continue;
            };
            let resources = Resources::new(
                appearance
                    .dict()
                    .get::<Dict<'_>>(b"Resources")
                    .unwrap_or_default(),
            );
            // Flattening wraps the appearance in a form, so PDFium parses it
            // with a CTM that starts at its `/Matrix`.
            let matrix = appearance
                .dict()
                .get::<[f64; 6]>(b"Matrix")
                .map(Affine::new)
                .unwrap_or(Affine::IDENTITY);
            // The appearance is drawn as a form placed by the fit to the
            // annotation rectangle.
            let form = transform * matrix.inverse();
            device.shows = TextShows::scan(
                TypedIter::new(&data),
                &resources,
                matrix,
                Some(form),
                fonts,
                false,
                &mut probe,
            );
            device.in_widget = true;
            device.nested_depth += 1;
            let mut context = Context::new(transform, bbox, cache, page.xref(), settings.clone());
            interpret(TypedIter::new(&data), &resources, &mut context, &mut device);
            device.nested_depth -= 1;
            device.in_widget = false;
        }
    }

    device.finish()
}

/// Measure the glyphs of codes 'A' and 'g' in `font` (selected by `name` in
/// `resources`) by interpreting a two-glyph text object, for PDFium's
/// fallback metrics (`CheckFontMetrics`).
fn probe_glyph_extent<'a>(
    cache: &InterpreterCache<'a>,
    xref: &'a hayro::hayro_syntax::xref::XRef,
    settings: &InterpreterSettings,
    resources: &Resources<'a>,
    name: &[u8],
    font: &Font,
) {
    let code_bytes = |code: u32| -> Vec<u8> {
        match font.reader() {
            CodeReader::Cmap(cmap) => {
                let len = (1..=4u8)
                    .find(|&len| cmap.lookup_cid_code(code, len).is_some())
                    .unwrap_or(2);
                code.to_be_bytes()[4 - len as usize..].to_vec()
            }
            _ => vec![code as u8],
        }
    };
    let mut content = b"BT /".to_vec();
    for &b in name {
        if b.is_ascii_graphic() && !b"()<>[]{}/%#".contains(&b) {
            content.push(b);
        } else {
            content.extend(format!("#{b:02X}").bytes());
        }
    }
    content.extend_from_slice(b" 1000 Tf");
    for code in [0x41, 0x67] {
        content.extend_from_slice(b" <");
        for b in code_bytes(code) {
            content.extend(format!("{b:02X}").bytes());
        }
        content.extend_from_slice(b"> Tj");
    }
    content.extend_from_slice(b" ET");
    let mut device = ExtentProbe {
        font: font.key(),
        composite: matches!(
            font.info().font_type,
            FontType::Type0 | FontType::CidType0 | FontType::CidType2
        ),
        boxes: Vec::new(),
    };
    let mut context = Context::new(
        Affine::IDENTITY,
        Rect::new(0.0, 0.0, 1.0, 1.0),
        cache,
        xref,
        settings.clone(),
    );
    interpret(
        TypedIter::new(&content),
        resources,
        &mut context,
        &mut device,
    );
    let boxes = device.boxes;
    font.set_glyph_extent(
        boxes.first().copied().flatten(),
        boxes.get(1).copied().flatten(),
    );
}

/// `CPDF_Type3Font::GetCharBBox`: the `d1` box of the glyph's char proc,
/// or else the bounds of what it draws (`CalcBoundingBox`), in glyph units
/// and rounded. Measured boxes are cached with the font.
fn type3_glyph_box<'a>(
    font: &Font,
    gid: u32,
    code: u32,
    glyph: &hayro::hayro_interpret::font::Type3Glyph<'a>,
    paint: &Paint<'a>,
) -> Option<Rect> {
    if let Some(Some(bbox)) = font.0.metrics.type3_box(code) {
        return Some(bbox);
    }
    if let Some(entry) = font.0.glyphs.borrow().get(&gid) {
        return entry.ink;
    }
    let mut device = BoundsProbe { bounds: None };
    glyph.interpret(&mut device, Affine::IDENTITY, Affine::IDENTITY, paint);
    let ink = device
        .bounds
        .filter(|r| r.is_finite())
        .map(|r| Rect::new(r.x0.round(), r.y0.round(), r.x1.round(), r.y1.round()));
    font.0.glyphs.borrow_mut().insert(
        gid,
        GlyphEntry {
            advance: None,
            outline: None,
            ink,
        },
    );
    ink
}

/// The union of everything drawn, like `CPDF_PageObjectHolder`'s
/// `CalcBoundingBox`.
struct BoundsProbe {
    bounds: Option<Rect>,
}

impl BoundsProbe {
    fn add(&mut self, rect: Rect) {
        self.bounds = Some(self.bounds.map_or(rect, |b| b.union(rect)));
    }
}

impl<'a> Device<'a> for BoundsProbe {
    fn draw_path(&mut self, path: &BezPath, props: DrawProps<'a>, draw_mode: &DrawMode) {
        let mut bounds = (props.transform * path.clone()).bounding_box();
        if let DrawMode::Stroke(stroke) | DrawMode::FillAndStroke(_, stroke) = draw_mode {
            let half = stroke.line_width as f64 * props.transform.determinant().abs().sqrt() / 2.0;
            bounds = bounds.inflate(half, half);
        }
        self.add(bounds);
    }
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, glyph_run: &GlyphRun<'_, 'a>, props: DrawProps<'a>, _: &DrawMode) {
        for glyph in glyph_run.glyphs() {
            let origin = props.transform * glyph.transform() * Point::ZERO;
            self.add(Rect::from_points(origin, origin));
        }
    }
    fn draw_image(&mut self, image: Image<'a, '_>, props: ImageDrawProps<'a>) {
        let (w, h) = (image.width() as f64, image.height() as f64);
        self.add(
            props
                .transform
                .transform_rect_bbox(Rect::new(0.0, 0.0, w, h)),
        );
    }
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

/// Collects the [`char_bbox`] of every glyph `font` draws.
struct ExtentProbe {
    font: u128,
    composite: bool,
    boxes: Vec<Option<Rect>>,
}

impl<'a> Device<'a> for ExtentProbe {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, glyph_run: &GlyphRun<'_, 'a>, _: DrawProps<'a>, _: &DrawMode) {
        for glyph in glyph_run.glyphs() {
            self.boxes.push(match &**glyph {
                Glyph::Outline(outline) if outline.font_cache_key() == self.font => {
                    char_bbox(&outline.outline(), self.composite)
                }
                _ => None,
            });
        }
    }
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

fn resources_dict<'a>(page: &HayroPage<'a>) -> Dict<'a> {
    // Inherited resources: hayro resolves them for interpretation but only
    // exposes the wrapper. Walk up the page tree for the font walk.
    let mut node = Some(page.raw().clone());
    let mut depth = 0;
    while let Some(dict) = node {
        if let Some(res) = dict.get::<Dict<'_>>(b"Resources") {
            return res;
        }
        depth += 1;
        if depth > 32 {
            break;
        }
        node = dict.get::<Dict<'_>>(b"Parent");
    }
    Dict::empty()
}

/// Widget annotations a viewer would paint: not `Invisible` (1), `Hidden` (2)
/// or `NoView` (32).
pub(crate) fn visible_widgets<'a>(page: &Dict<'a>) -> Vec<Dict<'a>> {
    let Some(annots) = page.get::<Array<'_>>(b"Annots") else {
        return Vec::new();
    };
    annots
        .iter::<Dict<'_>>()
        .filter(|annot| {
            annot.get::<Name<'_>>(b"Subtype").as_deref() == Some(&b"Widget"[..])
                && annot.get::<i32>(b"F").unwrap_or(0) & (1 | 2 | 32) == 0
        })
        .collect()
}

/// The annotation's normal appearance stream, honoring `/AS` for state
/// dictionaries.
pub(crate) fn normal_appearance<'a>(annot: &Dict<'a>) -> Option<Stream<'a>> {
    match annot.get::<Dict<'_>>(b"AP")?.get::<Object<'_>>(b"N")? {
        Object::Stream(stream) => Some(stream),
        Object::Dict(states) => annot
            .get::<Name<'_>>(b"AS")
            .and_then(|state| states.get::<Stream<'_>>(state.as_ref())),
        _ => None,
    }
}

/// PDF 32000 §12.5.5: map the appearance's form space onto the annotation
/// rectangle.
pub(crate) fn appearance_transform(appearance: &Stream<'_>, rect: [f32; 4]) -> Option<Affine> {
    let dict = appearance.dict();
    let bbox = dict.get::<[f32; 4]>(b"BBox")?;
    let matrix = dict
        .get::<[f32; 6]>(b"Matrix")
        .map(|m| Affine::new(m.map(f64::from)))
        .unwrap_or(Affine::IDENTITY);
    let transformed = (matrix
        * Rect::new(
            bbox[0] as f64,
            bbox[1] as f64,
            bbox[2] as f64,
            bbox[3] as f64,
        )
        .to_path(0.1))
    .bounding_box();
    if transformed.width() <= 0.0 || transformed.height() <= 0.0 {
        return None;
    }
    let annot = Rect::new(
        rect[0].min(rect[2]) as f64,
        rect[1].min(rect[3]) as f64,
        rect[0].max(rect[2]) as f64,
        rect[1].max(rect[3]) as f64,
    );
    let fit = Affine::new([
        annot.width() / transformed.width(),
        0.0,
        0.0,
        annot.height() / transformed.height(),
        annot.x0 - transformed.x0 * annot.width() / transformed.width(),
        annot.y0 - transformed.y0 * annot.height() / transformed.height(),
    ]);
    Some(fit * matrix)
}

struct ClipEntry {
    rect: Option<Rect>,
}

struct Collector<'f> {
    fonts: &'f FontCatalog,
    options: ContentOptions,
    content: PageContent,
    clips: Vec<ClipEntry>,
    marked: Vec<Option<i32>>,
    /// Text-showing operators of the stream being interpreted.
    shows: TextShows,
    /// Glyph ids and hayro's transforms of the last run, to recognize the
    /// stroke pass of a fill-and-stroke run.
    last_raw: Vec<(u32, Affine)>,
    in_widget: bool,
    nested_depth: u32,
}

impl<'f> Collector<'f> {
    fn new(fonts: &'f FontCatalog, options: ContentOptions) -> Self {
        Self {
            fonts,
            options,
            content: PageContent::default(),
            clips: Vec::new(),
            marked: Vec::new(),
            shows: TextShows::default(),
            last_raw: Vec::new(),
            in_widget: false,
            nested_depth: 0,
        }
    }

    fn finish(self) -> PageContent {
        self.content
    }

    fn mcid(&self) -> Option<i32> {
        self.marked.iter().rev().find_map(|m| *m)
    }

    fn clip_state(&self) -> ClipState {
        let mut state = ClipState::None;
        for entry in &self.clips {
            match (entry.rect, state) {
                (None, _) => return ClipState::Unknown,
                (Some(rect), ClipState::None) => state = ClipState::Rect(rect),
                (Some(rect), ClipState::Rect(current)) => {
                    // An empty intersection stays inverted so callers can
                    // tell it apart from "no clip".
                    state = ClipState::Rect(Rect::new(
                        current.x0.max(rect.x0),
                        current.y0.max(rect.y0),
                        current.x1.min(rect.x1),
                        current.y1.min(rect.y1),
                    ));
                }
                (Some(_), ClipState::Unknown) => return ClipState::Unknown,
            }
        }
        state
    }

    fn clip_bounds(&self) -> Option<Rect> {
        match self.clip_state() {
            ClipState::Rect(rect) => Some(rect),
            _ => None,
        }
    }

    fn add_bounds(&mut self, bounds: Rect) {
        if !bounds.is_finite() {
            return;
        }
        self.content.bounds = Some(match self.content.bounds {
            Some(b) => b.union(bounds),
            None => bounds,
        });
        if let Some(mcid) = self.mcid() {
            self.content
                .mcid_bounds
                .entry(mcid)
                .and_modify(|b| *b = b.union(bounds))
                .or_insert(bounds);
        }
    }

    fn record_path(&mut self, path: BezPath, props: &DrawProps<'_>, mode: &DrawMode) {
        let (mut filled, mut stroked, local_line_width) = match mode {
            DrawMode::Fill(_) => (true, false, 0.0),
            DrawMode::Stroke(stroke) => (false, true, stroke.line_width),
            DrawMode::FillAndStroke(_, stroke) => (true, true, stroke.line_width),
            DrawMode::Invisible => return,
        };
        // hayro turns zero-area fills into hairline strokes (pdf.js #4260);
        // PDFium keeps them as fills.
        let mut zero_area = false;
        if let DrawMode::Stroke(stroke) = mode
            && stroke.line_width == 0.001
            && matches!(stroke.line_join, Join::Bevel)
            && path.elements().len() == 2
        {
            filled = true;
            stroked = false;
            zero_area = true;
        }
        let page_path = props.transform * path.clone();
        let color = paint_color(&props.paint);
        let scale = props.transform.determinant().abs().sqrt() as f32;
        let line_width = local_line_width * scale;

        // `sh` paints the whole clip area with a shading; PDFium reports that
        // as a shading object, not a path.
        if filled
            && !stroked
            && matches!(&props.paint, Paint::Pattern(p) if matches!(**p, Pattern::Shading(_)))
            && self.is_clip_area(&page_path)
        {
            self.content.shadings += 1;
            let bounds = page_path.bounding_box();
            self.add_bounds(bounds);
            return;
        }

        // A fill followed by a stroke of the same path (`B`, `b`) is one
        // PDFium path object.
        if stroked
            && !filled
            && let Some(last) = self.content.paths.last_mut()
            && last.filled
            && !last.stroked
            && (last.path == page_path
                || (last.zero_area && last.path.bounding_box() == page_path.bounding_box()))
        {
            if last.zero_area {
                // The stroke carries the path as written.
                last.path = page_path;
                last.local = path;
                last.ctm = props.transform;
                last.zero_area = false;
            }
            last.stroked = true;
            last.stroke = color;
            last.line_width = line_width;
            last.local_line_width = local_line_width;
            let bounds = last.bounds();
            self.add_bounds(bounds);
            return;
        }

        // PDFium reports both colors of the graphics state; hayro hands over
        // the painting one, so the other only says that there is one.
        let unpainted = Some(Color {
            r: 0,
            g: 0,
            b: 0,
            a: 255,
        });
        let record = PathRecord {
            path: page_path,
            filled,
            stroked,
            fill: if filled { color } else { unpainted },
            stroke: if stroked { color } else { unpainted },
            line_width: if stroked { line_width } else { 0.0 },
            local: path,
            ctm: props.transform,
            local_line_width: if stroked { local_line_width } else { 0.0 },
            zero_area,
        };
        let bounds = record.bounds();
        self.content.paths.push(record);
        self.add_bounds(bounds);
    }

    fn is_clip_area(&self, page_path: &BezPath) -> bool {
        let bounds = page_path.bounding_box();
        match self.clip_bounds() {
            Some(clip) => {
                (bounds.x0 - clip.x0).abs() < 0.5
                    && (bounds.y0 - clip.y0).abs() < 0.5
                    && (bounds.x1 - clip.x1).abs() < 0.5
                    && (bounds.y1 - clip.y1).abs() < 0.5
            }
            // Without a known clip, `sh` fills the context bbox (the page).
            None => page_path.elements().len() <= 6,
        }
    }

    fn glyph_entry(&self, glyph: &Glyph<'_>) -> (u128, u32, GlyphEntry, Option<GlyphText>) {
        match glyph {
            Glyph::Outline(outline) => {
                let font_key = outline.font_cache_key();
                let gid = outline.glyph_id().to_u32();
                let font = self.fonts.font(font_key);
                let unicode = outline.as_unicode().map(bf_to_text);
                let cached = font.0.glyphs.borrow().get(&gid).cloned();
                let entry = cached.unwrap_or_else(|| {
                    let path = outline.outline();
                    let has_ink = path
                        .elements()
                        .iter()
                        .any(|el| !matches!(el, PathEl::MoveTo(_)));
                    let composite = matches!(
                        font.info().font_type,
                        FontType::Type0 | FontType::CidType0 | FontType::CidType2
                    );
                    let ink = has_ink.then(|| char_bbox(&path, composite)).flatten();
                    let entry = GlyphEntry {
                        advance: outline.advance_width(),
                        outline: has_ink.then(|| Rc::new(path)),
                        ink,
                    };
                    font.0.glyphs.borrow_mut().insert(gid, entry.clone());
                    if font.0.program.borrow().is_none() {
                        *font.0.program.borrow_mut() = Some(
                            outline
                                .font_data()
                                .map(|data| data.data.as_ref().as_ref().to_vec()),
                        );
                    }
                    entry
                });
                if let (Some(text), Some(advance)) = (&unicode, entry.advance) {
                    let mut chars = text.chars();
                    if let (Some(c), None) = (chars.next(), chars.next()) {
                        font.0
                            .unicode_advances
                            .borrow_mut()
                            .entry(c as u32)
                            .or_insert(advance);
                    }
                }
                (font_key, gid, entry, unicode)
            }
            Glyph::Type3(type3) => {
                let gid = (type3.cache_key() & 0x7FFF_FFFF) as u32;
                let unicode = type3.as_unicode().map(bf_to_text);
                // Type3 glyphs are PDF content; their extent is not exposed,
                // so approximate an em box (the advance is filled in by the
                // caller from the glyph positions).
                let entry = GlyphEntry {
                    advance: None,
                    outline: None,
                    ink: Some(Rect::new(0.0, 0.0, 500.0, 700.0)),
                };
                (TYPE3_FONT_KEY, gid, entry, unicode)
            }
        }
    }
}

/// `GetCharBBox`: FreeType's glyph metrics box — the control box of the
/// outline, in whole glyph units — which `CFX_Face::GetCharBBox` (used for
/// composite fonts) heightens by 1/64 of its top.
fn char_bbox(path: &BezPath, composite: bool) -> Option<Rect> {
    let mut points = path.elements().iter().flat_map(|el| match *el {
        PathEl::MoveTo(p) | PathEl::LineTo(p) => [Some(p), None, None],
        PathEl::QuadTo(c, p) => [Some(c), Some(p), None],
        PathEl::CurveTo(c1, c2, p) => [Some(c1), Some(c2), Some(p)],
        PathEl::ClosePath => [None, None, None],
    });
    let first = points.find_map(|p| p)?;
    let mut bounds = Rect::from_points(first, first);
    for p in points.flatten() {
        bounds = bounds.union_pt(p);
    }
    // `NormalizeFontMetric` rounds half up and truncates toward zero.
    let normalize = |v: f64| (v + 0.5).trunc();
    let (x0, y0, x1) = (
        normalize(bounds.x0),
        normalize(bounds.y0),
        normalize(bounds.x1),
    );
    let mut y1 = normalize(bounds.y1);
    if composite {
        y1 += (y1 as i64 / 64) as f64;
    }
    let rect = Rect::new(x0, y0, x1, y1);
    rect.is_finite().then_some(rect)
}

fn bf_to_text(bf: hayro::hayro_interpret::hayro_cmap::BfString) -> GlyphText {
    match bf {
        hayro::hayro_interpret::hayro_cmap::BfString::Char(c) => GlyphText::from(c),
        hayro::hayro_interpret::hayro_cmap::BfString::String(s) => GlyphText::new(&s),
    }
}

fn paint_color(paint: &Paint<'_>) -> Option<Color> {
    match paint {
        Paint::Color(color) => {
            let [r, g, b, a] = color.to_rgba().to_rgba8();
            Some(Color { r, g, b, a })
        }
        Paint::Pattern(_) => None,
    }
}

/// Re-express a rectangle as PDFium's `re` point layout (`AddPathRect`):
/// five points from the origin corner (right, up, left, back) with the
/// subpath closed, the far corner summed in single precision. hayro hands
/// the rectangle over normalized, so this is the layout of a `re` with a
/// positive width and height.
fn pdfium_rect_path(rect: &Rect) -> BezPath {
    let (x, y) = (rect.x0 as f32, rect.y0 as f32);
    let far_x = (x + rect.width() as f32) as f64;
    let far_y = (y + rect.height() as f32) as f64;
    let (x, y) = (x as f64, y as f64);
    let mut path = BezPath::new();
    path.move_to((x, y));
    path.line_to((far_x, y));
    path.line_to((far_x, far_y));
    path.line_to((x, far_y));
    path.line_to((x, y));
    path.close_path();
    path
}

impl<'a> Device<'a> for Collector<'_> {
    fn draw_path(&mut self, path: &BezPath, props: DrawProps<'a>, draw_mode: &DrawMode) {
        self.record_path(path.clone(), &props, draw_mode);
    }

    fn draw_rect(&mut self, rect: &Rect, props: DrawProps<'a>, draw_mode: &DrawMode) {
        self.record_path(pdfium_rect_path(rect), &props, draw_mode);
    }

    fn push_clip_path(&mut self, clip_path: &ClipPath) {
        let rect = clip_as_rect(&clip_path.path);
        self.clips.push(ClipEntry { rect });
    }

    fn push_clip_rect(&mut self, rect: &Rect) {
        self.clips.push(ClipEntry { rect: Some(*rect) });
    }

    fn push_transparency_group(
        &mut self,
        _opacity: f32,
        _mask: Option<SoftMask<'a>>,
        _blend_mode: BlendMode,
    ) {
    }

    fn draw_glyph_run(
        &mut self,
        glyph_run: &GlyphRun<'_, 'a>,
        props: DrawProps<'a>,
        draw_mode: &DrawMode,
    ) {
        let render_mode = match draw_mode {
            DrawMode::Fill(_) => 0,
            DrawMode::Stroke(_) => 1,
            DrawMode::FillAndStroke(..) => 2,
            DrawMode::Invisible => 3,
        };
        let color = paint_color(&props.paint);
        let scale = props.transform.determinant().abs().sqrt() as f32;
        let line_width = match draw_mode {
            DrawMode::Stroke(stroke) | DrawMode::FillAndStroke(_, stroke) => {
                stroke.line_width * scale
            }
            _ => 0.0,
        };

        let Some(first) = glyph_run.glyphs().first() else {
            return;
        };
        // Matched against the content scan, which cannot name Type3 fonts.
        let scan_font = match &**first {
            Glyph::Outline(outline) => Some(outline.font_cache_key()),
            Glyph::Type3(_) => None,
        };
        let [a, b, c, d, ..] = first.transform().as_coeffs();
        let scan_linear = [a, b, c, d];

        let mut glyphs = Vec::with_capacity(glyph_run.glyphs().len());
        let mut font_key = None;
        for positioned in glyph_run.glyphs() {
            let (key, gid, entry, unicode) = self.glyph_entry(positioned);
            font_key.get_or_insert(key);
            // Per glyph: the width can depend on the code, not just the glyph.
            let advance = match &**positioned {
                Glyph::Outline(outline) => outline.advance_width(),
                Glyph::Type3(_) => None,
            };
            glyphs.push(RunGlyph {
                unicode,
                code: SYNTHETIC_CODE_BASE + gid,
                gid,
                transform: props.transform * positioned.transform(),
                ink: entry.ink,
                advance: advance.unwrap_or(0.0),
                exact_advance: false,
            });
        }
        if glyphs.is_empty() {
            return;
        }

        // Fill-then-stroke render modes arrive as two runs over the same
        // glyphs; PDFium has one text object.
        if render_mode == 1
            && let Some(last) = self.content.runs.last_mut()
            && last.render_mode == 0
            && self.last_raw.len() == glyphs.len()
            && self
                .last_raw
                .iter()
                .zip(&glyphs)
                .all(|((gid, transform), b)| *gid == b.gid && *transform == b.transform)
        {
            last.render_mode = 2;
            last.stroke = color;
            last.line_width = line_width;
            return;
        }
        self.last_raw = glyphs.iter().map(|g| (g.gid, g.transform)).collect();

        let mut font_key = font_key.unwrap_or(TYPE3_FONT_KEY);
        let mut kernings = None;
        let mut state_colors = (None, None);
        let show = self.shows.take(scan_font, scan_linear, glyphs.len());
        let size = show.map(|show| show.size);
        let form = show.map_or(Affine::IDENTITY, |show| show.form);
        // Without a match, judge by hayro's nesting (widgets count as forms).
        let in_form = show.map_or(self.nested_depth > 0, |show| show.in_form);
        let mut render_mode = render_mode;
        if let Some(show) = show {
            if show.clip_only {
                render_mode = 7;
            }
            state_colors = (show.fill, show.stroke);
            // The scan names the font of Type3 glyphs, which hayro does not.
            if font_key == TYPE3_FONT_KEY
                && let Some(key) = show.font
            {
                font_key = key;
            }
            let (index, prev, codes) = (show.index, show.prev, show.codes.map(<[u32]>::to_vec));
            if scan_font.is_none()
                && let Some(codes) = &codes
            {
                let font = self.fonts.font(font_key);
                for ((glyph, positioned), &code) in
                    glyphs.iter_mut().zip(glyph_run.glyphs()).zip(codes)
                {
                    if let Glyph::Type3(type3) = &**positioned {
                        glyph.ink = type3_glyph_box(&font, glyph.gid, code, type3, &props.paint);
                    }
                }
            }
            kernings = show
                .kernings
                .filter(|_| codes.is_some())
                .map(|k| (k.to_vec(), show.char_space));
            let trailing = show.trailing_kerning;
            let font = self.fonts.font(font_key);
            let carry = self.shows.carry(prev);
            let mut drift = match codes {
                Some(codes) => {
                    pdfium_positions(&mut glyphs, &codes, &font, scan_font.is_none(), carry)
                }
                None => {
                    shift_glyphs(&mut glyphs, carry);
                    carry
                }
            };
            // hayro moves on by the adjustments after the last string;
            // PDFium does not.
            if trailing != 0.0
                && let Some(last) = glyphs.last()
            {
                let [a, b, c, d, ..] = last.transform.as_coeffs();
                let units = -trailing as f64;
                drift += if font.info().is_vertical {
                    Vec2::new(c * units, d * units)
                } else {
                    Vec2::new(a * units, b * units)
                };
            }
            self.shows.set_drift(index, drift);
        }
        fill_missing_advances(&mut glyphs);

        let run = TextRun {
            glyphs,
            render_mode,
            // PDFium reports both colors of the graphics state; hayro hands
            // over the painting one.
            fill: if render_mode == 1 {
                state_colors.0
            } else {
                color
            },
            stroke: if render_mode == 1 {
                color
            } else {
                state_colors.1
            },
            line_width,
            mcid: self.mcid(),
            clip: self.clip_state(),
            font_key,
            size,
            form,
            in_form,
            kernings,
            from_widget: self.in_widget,
        };
        let bounds = run_bounds(&run);
        self.content.runs.push(run);
        if let Some(bounds) = bounds {
            self.add_bounds(bounds);
        }
    }

    fn draw_image(&mut self, image: Image<'a, '_>, props: ImageDrawProps<'a>) {
        // hayro hands over `CTM × [1/w 0 0 -1/h 0 1]` (pixel space → page);
        // undo the pixel mapping to get the image's CTM, the unit-square
        // matrix PDFium reports.
        let (w, h) = (image.width().max(1) as f64, image.height().max(1) as f64);
        let mut record = ImageRecord {
            transform: props.transform * Affine::new([w, 0.0, 0.0, -h, 0.0, h]),
            pixel_width: image.width(),
            pixel_height: image.height(),
            bits_per_pixel: 0,
            colorspace: 0,
            raw: None,
            is_dct: false,
            rgba: None,
        };
        match &image {
            Image::Raster(raster) => {
                let stream = raster.stream();
                let (bpp, colorspace) = image_format(stream.dict());
                record.bits_per_pixel = bpp;
                record.colorspace = colorspace;
                let filters = stream.filters();
                record.is_dct = filters.len() == 1
                    && matches!(filters[0], hayro::hayro_syntax::Filter::DctDecode);
                if self.options.image_streams {
                    record.raw = Some(stream.raw_data().into_owned());
                }
                if self.options.image_pixels {
                    raster.with_rgba(
                        |data, alpha| {
                            record.rgba = Some(to_rgba(&data, alpha.as_ref()));
                        },
                        None,
                    );
                }
            }
            Image::Stencil(stencil) => {
                record.bits_per_pixel = 1;
                record.colorspace = 1;
                if self.options.image_pixels {
                    stencil.with_stencil(
                        |luma, paint| {
                            let color = paint_color(paint).unwrap_or(Color {
                                r: 0,
                                g: 0,
                                b: 0,
                                a: 255,
                            });
                            let mut rgba = Vec::with_capacity(luma.data.len() * 4);
                            for &coverage in &luma.data {
                                rgba.extend_from_slice(&[color.r, color.g, color.b, coverage]);
                            }
                            record.rgba = Some((luma.width, luma.height, rgba));
                        },
                        None,
                    );
                }
            }
        }
        let bounds = record.bounds();
        self.content.images.push(record);
        self.add_bounds(bounds);
    }

    fn pop_clip(&mut self) {
        self.clips.pop();
    }

    fn pop_transparency_group(&mut self) {}

    fn begin_marked_content(&mut self, _tag: &[u8], mcid: Option<i32>) {
        self.marked.push(mcid);
    }

    fn end_marked_content(&mut self) {
        self.marked.pop();
    }
}

/// Give `glyphs` their character `codes` and move them to where PDFium puts
/// them, returning the page-space drift at the end of the run.
///
/// PDFium advances by `GetCharWidth`, an integer: `/Widths` entries are
/// truncated and font-program widths rounded, while hayro advances by the
/// exact value. Over a line of fractional `/Widths` (pdfTeX writes them) the
/// difference adds up to a visible offset, so each glyph moves back by the
/// widths hayro added beyond PDFium's, plus the `carry` from the show this
/// run continues.
fn pdfium_positions(
    glyphs: &mut [RunGlyph],
    codes: &[u32],
    font: &super::font::Font,
    type3: bool,
    carry: Vec2,
) -> Vec2 {
    let vertical = font.info().is_vertical;
    let mut excess = 0.0_f64;
    let mut drift = carry;
    for (glyph, &code) in glyphs.iter_mut().zip(codes) {
        glyph.code = code;
        font.note_code(code, glyph.gid, glyph.unicode.as_deref());
        if glyph.unicode.is_none()
            && let Some(c) = font.cid_unicode(code)
        {
            glyph.unicode = Some(GlyphText::from(c));
        }
        let hayro = if type3 {
            font.0.metrics.type3_advance(code).unwrap_or(0.0)
        } else {
            glyph.advance
        };
        let [a, b, ..] = glyph.transform.as_coeffs();
        drift = carry + Vec2::new(a * excess, b * excess);
        glyph.transform = Affine::translate(-drift) * glyph.transform;
        if vertical {
            continue;
        }
        let pdfium = font
            .0
            .metrics
            .width(code)
            .map_or_else(|| (hayro + 0.5).trunc(), |w| w as f32);
        glyph.advance = pdfium;
        glyph.exact_advance = true;
        excess += (hayro - pdfium) as f64;
        drift = carry + Vec2::new(a * excess, b * excess);
    }
    drift
}

fn shift_glyphs(glyphs: &mut [RunGlyph], offset: Vec2) {
    if offset != Vec2::ZERO {
        for glyph in glyphs {
            glyph.transform = Affine::translate(-offset) * glyph.transform;
        }
    }
}

/// Type3 glyphs (and broken fonts) report no advance; derive it from the
/// distance to the next glyph so geometry downstream stays sensible.
fn fill_missing_advances(glyphs: &mut [RunGlyph]) {
    for i in 0..glyphs.len() {
        if glyphs[i].advance > 0.0 || glyphs[i].exact_advance {
            continue;
        }
        let next = glyphs.get(i + 1).map(|n| n.transform);
        let advance = next
            .and_then(|next| {
                let inverse = glyphs[i].transform.inverse();
                let origin = inverse * (next * Point::ZERO);
                (origin.x.is_finite() && origin.x > 0.0).then_some(origin.x as f32)
            })
            .or_else(|| glyphs[i].ink.map(|ink| ink.x1.max(0.0) as f32))
            .unwrap_or(0.0);
        glyphs[i].advance = advance;
    }
}

/// Union of a run's glyph boxes in page space.
pub(crate) fn run_bounds(run: &TextRun) -> Option<Rect> {
    run_bounds_in(run, Affine::IDENTITY)
}

/// Union of a run's glyph boxes mapped by `space` (page space → the space
/// wanted), like `CPDF_TextObject::GetRect` with `space` the inverse form
/// matrix.
pub(crate) fn run_bounds_in(run: &TextRun, space: Affine) -> Option<Rect> {
    let mut bounds: Option<Rect> = None;
    for glyph in &run.glyphs {
        let transform = space * glyph.transform;
        let rect = match glyph.ink {
            Some(ink) => transform.transform_rect_bbox(ink),
            None => {
                let origin = transform * Point::ZERO;
                Rect::from_points(origin, origin)
            }
        };
        bounds = Some(bounds.map_or(rect, |b| b.union(rect)));
    }
    bounds
}

fn clip_as_rect(path: &BezPath) -> Option<Rect> {
    let mut points = Vec::with_capacity(5);
    let mut closed = false;
    for el in path.elements() {
        match el {
            PathEl::MoveTo(p) if points.is_empty() => points.push(*p),
            PathEl::LineTo(p) if !points.is_empty() && !closed => points.push(*p),
            PathEl::ClosePath if !closed => closed = true,
            _ => return None,
        }
    }
    if points.len() == 5 && points[4] == points[0] {
        points.pop();
    }
    if points.len() != 4 {
        return None;
    }
    let axis = |a: Point, b: Point| {
        let horizontal = (a.y - b.y).abs() < 1e-9 && (a.x - b.x).abs() > 1e-9;
        let vertical = (a.x - b.x).abs() < 1e-9 && (a.y - b.y).abs() > 1e-9;
        if horizontal {
            Some(true)
        } else if vertical {
            Some(false)
        } else {
            None
        }
    };
    let mut previous = None;
    for i in 0..4 {
        let current = axis(points[i], points[(i + 1) % 4])?;
        if previous == Some(current) {
            return None;
        }
        previous = Some(current);
    }
    let xs = points.iter().map(|p| p.x);
    let ys = points.iter().map(|p| p.y);
    Some(Rect::new(
        xs.clone().fold(f64::INFINITY, f64::min),
        ys.clone().fold(f64::INFINITY, f64::min),
        xs.fold(f64::NEG_INFINITY, f64::max),
        ys.fold(f64::NEG_INFINITY, f64::max),
    ))
}

/// `(bits per pixel, FPDF_COLORSPACE_*)` from an image dictionary.
fn image_format(dict: &Dict<'_>) -> (u32, i32) {
    let bpc = dict.get::<u32>(b"BitsPerComponent").unwrap_or(8);
    let (components, colorspace) = match dict.get::<Object<'_>>(b"ColorSpace") {
        Some(Object::Name(name)) => colorspace_by_name(name.as_ref()),
        Some(Object::Array(array)) => array
            .iter::<Name<'_>>()
            .next()
            .map(|name| colorspace_by_name(name.as_ref()))
            .unwrap_or((3, 0)),
        _ => (3, 0),
    };
    (bpc * components, colorspace)
}

fn colorspace_by_name(name: &[u8]) -> (u32, i32) {
    match name {
        b"DeviceGray" | b"G" => (1, 1),
        b"DeviceRGB" | b"RGB" => (3, 2),
        b"DeviceCMYK" | b"CMYK" => (4, 3),
        b"CalGray" => (1, 4),
        b"CalRGB" => (3, 5),
        b"Lab" => (3, 6),
        b"ICCBased" => (3, 7),
        b"Separation" => (1, 8),
        b"DeviceN" => (4, 9),
        b"Indexed" | b"I" => (1, 10),
        _ => (3, 0),
    }
}

fn to_rgba(
    data: &ImageData,
    alpha: Option<&hayro::hayro_interpret::LumaData>,
) -> (u32, u32, Vec<u8>) {
    let (width, height) = (data.width(), data.height());
    let pixels = (width * height) as usize;
    let mut rgba = Vec::with_capacity(pixels * 4);
    let alpha_at = |i: usize| -> u8 {
        alpha
            .filter(|a| a.width == width && a.height == height)
            .and_then(|a| a.data.get(i).copied())
            .unwrap_or(255)
    };
    match data {
        ImageData::Rgb(rgb) => {
            for (i, px) in rgb.data.chunks_exact(3).enumerate().take(pixels) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], alpha_at(i)]);
            }
        }
        ImageData::Luma(luma) => {
            for (i, &v) in luma.data.iter().enumerate().take(pixels) {
                rgba.extend_from_slice(&[v, v, v, alpha_at(i)]);
            }
        }
    }
    (width, height, rgba)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_text_keeps_short_text_inline() {
        let text = GlyphText::from('é');
        assert!(matches!(text, GlyphText::Inline { .. }));
        assert_eq!(&*text, "é");
        let ligature = GlyphText::new("ffi");
        assert_eq!(ligature.chars().count(), 3);
        let long = "a".repeat(GlyphText::INLINE + 1);
        let heap = GlyphText::new(&long);
        assert!(matches!(heap, GlyphText::Heap(_)));
        assert_eq!(heap.as_str(), long);
        assert_eq!(GlyphText::new(&long[1..]).as_str(), &long[1..]);
    }
}
