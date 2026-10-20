//! Pages: PDFium's geometry model plus the page-object queries liteparse uses.

use std::rc::Rc;

use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::page::{Page as HayroPage, Rotation};
use hayro::kurbo::{Affine, PathEl, Rect};
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{RenderCache, RenderSettings, render_into};

use super::bitmap::Bitmap;
use super::content::{ContentOptions, PageContent};
use super::document::{Document, FormEnvironment};
use super::error::PdfiumError;
use super::text_page::TextPage;
use super::types::{Color, RectF};

pub use super::annots::{PdfAnnotation, PdfFormField, PdfLink};

/// Bounding box of an embedded image object on a page.
/// Coordinates are in PDF points with top-left origin (Y-down).
#[derive(Debug, Clone, Copy)]
pub struct ImageBounds {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Metadata for an embedded image object retained by the extraction filters.
/// `object_index` is its index among all image objects on the page.
#[derive(Debug, Clone)]
pub struct ImageObjectInfo {
    pub object_index: usize,
    pub bounds: ImageBounds,
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// Clockwise page-object rotation in degrees, normalized to `[0, 360)`.
    pub rotation: f32,
    /// Original JPEG stream bytes when the image is a bare DCT stream with a
    /// valid JPEG signature.
    pub jpeg_bytes: Option<Vec<u8>>,
    /// Raw encoded stream bytes, used to identify repeated image resources.
    pub raw_bytes: Option<Vec<u8>>,
    #[doc(hidden)]
    pub bits_per_pixel: u32,
    #[doc(hidden)]
    pub colorspace: i32,
}

#[derive(Debug, Clone)]
pub struct ImageObjects {
    pub images: Vec<ImageObjectInfo>,
    pub error_count: u32,
}

/// One segment of a vector path, in viewport space.
#[derive(Debug, Clone, Copy)]
pub struct PathSegment {
    pub kind: SegmentKind,
    pub x: f32,
    pub y: f32,
    /// Whether this segment closes the current subpath back to its MoveTo.
    pub close: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    MoveTo,
    LineTo,
    BezierTo,
}

/// A vector path object. Used by the markdown emitter for ruled-table,
/// horizontal-rule, and figure-cluster detection.
#[derive(Debug, Clone)]
pub struct PathObject {
    /// Object bbox in viewport space.
    pub bbox: RectF,
    pub stroke_color: Option<Color>,
    pub fill_color: Option<Color>,
    pub stroke_width: f32,
    pub is_stroked: bool,
    pub is_filled: bool,
    pub segments: Vec<PathSegment>,
}

/// Page-object kinds (kept for API parity).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageObjectKind {
    Unknown,
    Text,
    Path,
    Image,
    Shading,
    Form,
}

/// A raw path segment in page space (kept for API parity).
#[derive(Debug, Clone, Copy)]
pub struct RawPathSegment {
    pub kind: Option<SegmentKind>,
    pub point: Option<(f32, f32)>,
    pub close: bool,
}

/// Placeholder for PDFium's page-object handles. The hayro backend exposes
/// page content through the typed queries on [`Page`] instead.
#[derive(Debug)]
pub struct PageObject {
    _private: (),
}

/// A loaded page within a [`Document`].
pub struct Page<'doc, 'lib: 'doc> {
    pub(crate) doc: &'doc Document<'lib>,
    pub(crate) index: usize,
    pub(crate) page: &'doc HayroPage<'doc>,
    pub(crate) user_unit: f32,
    /// Whether visible widget appearances count as page content (PDFium's
    /// state after a flatten).
    pub(crate) flattened: bool,
}

impl<'doc, 'lib: 'doc> Page<'doc, 'lib> {
    /// CropBox ∩ MediaBox in page space (PDFium's page bounding box).
    fn bbox(&self) -> Rect {
        let media = self.page.media_box();
        let media = Rect::new(media.x0, media.y0, media.x1, media.y1).abs();
        let crop = self.doc.crop_override(self.index).unwrap_or_else(|| {
            let crop = self.page.crop_box();
            Rect::new(crop.x0, crop.y0, crop.x1, crop.y1).abs()
        });
        let bbox = crop.intersect(media);
        if bbox.width() <= 0.0 || bbox.height() <= 0.0 {
            media
        } else {
            bbox
        }
    }

    /// Page width in raw page units, rotation applied (ignores `/UserUnit`).
    pub fn width(&self) -> f32 {
        let bbox = self.bbox();
        if matches!(self.rotation(), 1 | 3) {
            bbox.height() as f32
        } else {
            bbox.width() as f32
        }
    }

    /// Page height in raw page units, rotation applied (see [`Self::width`]).
    pub fn height(&self) -> f32 {
        let bbox = self.bbox();
        if matches!(self.rotation(), 1 | 3) {
            bbox.width() as f32
        } else {
            bbox.height() as f32
        }
    }

    /// The page's `/UserUnit` multiplier, 1.0 when absent.
    pub fn user_unit(&self) -> f32 {
        self.user_unit
    }

    /// The page's `/Rotate` in quarter turns clockwise (0..=3), including any
    /// override set with [`Self::set_rotation`].
    pub fn rotation(&self) -> i32 {
        self.doc
            .rotation_override(self.index)
            .unwrap_or_else(|| native_rotation(self.page))
    }

    /// Override the page's rotation for the lifetime of the open document,
    /// like PDFium's `FPDFPage_SetRotation`. Values outside 0..=3 are ignored.
    pub fn set_rotation(&self, quarter_turns: i32) {
        if (0..=3).contains(&quarter_turns) {
            self.doc.set_rotation_override(self.index, quarter_turns);
        }
    }

    /// Replace the page's crop box (page space) for the lifetime of the open
    /// document.
    pub fn set_crop_box(&self, left: f32, bottom: f32, right: f32, top: f32) {
        self.doc.set_crop_override(
            self.index,
            Rect::new(left as f64, bottom as f64, right as f64, top as f64).abs(),
        );
    }

    /// Get the page bounding box (CropBox ∩ MediaBox) in page space.
    pub fn view_box(&self) -> Option<RectF> {
        let bbox = self.bbox();
        Some(RectF {
            left: bbox.x0 as f32,
            top: bbox.y1 as f32,
            right: bbox.x1 as f32,
            bottom: bbox.y0 as f32,
        })
    }

    /// Page dimensions in viewport space (rotation applied, `/UserUnit`
    /// scaled).
    pub fn viewport_size(&self, view_box: &RectF) -> (f32, f32) {
        let mut width = (view_box.right - view_box.left).abs() * self.user_unit;
        let mut height = (view_box.top - view_box.bottom).abs() * self.user_unit;
        if matches!(self.rotation(), 1 | 3) {
            std::mem::swap(&mut width, &mut height);
        }
        (width, height)
    }

    /// The exact page → viewport affine for `view_box`.
    pub(crate) fn viewport_affine(&self, view_box: &RectF) -> Affine {
        let (x0, x1) = (
            view_box.left.min(view_box.right) as f64,
            view_box.left.max(view_box.right) as f64,
        );
        let (y0, y1) = (
            view_box.bottom.min(view_box.top) as f64,
            view_box.bottom.max(view_box.top) as f64,
        );
        let u = self.user_unit as f64;
        // PDFium's `CPDF_Page::m_PageMatrix` per rotation, then the y-flip
        // onto a top-left device origin.
        match self.rotation() {
            1 => Affine::new([0.0, u, u, 0.0, -u * y0, -u * x0]),
            2 => Affine::new([-u, 0.0, 0.0, u, u * x1, -u * y0]),
            3 => Affine::new([0.0, -u, -u, 0.0, u * y1, u * x1]),
            _ => Affine::new([u, 0.0, 0.0, -u, -u * x0, u * y1]),
        }
    }

    /// Convert a point from page space to viewport space, quantized to
    /// 1/1000 pt like `FPDF_PageToDevice` at 1000× scale.
    pub fn page_to_viewport(&self, view_box: &RectF, page_x: f32, page_y: f32) -> (f32, f32) {
        let (vw, vh) = self.viewport_size(view_box);
        let mult = 1000.0f32.min((i32::MAX / 4) as f32 / vw.max(vh).max(1.0));
        let p =
            self.viewport_affine(view_box) * hayro::kurbo::Point::new(page_x as f64, page_y as f64);
        (
            ((p.x as f32) * mult).round() / mult,
            ((p.y as f32) * mult).round() / mult,
        )
    }

    /// Convert bounds from page space to viewport space (top-left origin).
    pub fn bounds_to_viewport(&self, view_box: &RectF, page_bounds: &RectF) -> RectF {
        let (ll_x, ll_y) = self.page_to_viewport(view_box, page_bounds.left, page_bounds.bottom);
        let (ur_x, ur_y) = self.page_to_viewport(view_box, page_bounds.right, page_bounds.top);
        RectF {
            left: ll_x.min(ur_x),
            top: ll_y.min(ur_y),
            right: ll_x.max(ur_x),
            bottom: ll_y.max(ur_y),
        }
    }

    /// Build a [`ViewportTransform`] by probing 3 points, as upstream does.
    pub fn viewport_transform(&self, view_box: &RectF) -> ViewportTransform {
        let (e, f) = self.page_to_viewport(view_box, 0.0, 0.0);
        let (ax_e, cx_f) = self.page_to_viewport(view_box, 1.0, 0.0);
        let (by_e, dy_f) = self.page_to_viewport(view_box, 0.0, 1.0);
        ViewportTransform {
            a: ax_e - e,
            b: by_e - e,
            c: cx_f - f,
            d: dy_f - f,
            e,
            f,
        }
    }

    pub(crate) fn content(&self, options: ContentOptions) -> Rc<PageContent> {
        let options = ContentOptions {
            flatten_widgets: options.flatten_widgets || self.flattened,
            ..options
        };
        self.doc.page_content(self.index, self.page, options)
    }

    pub fn text(&self) -> Result<TextPage<'_, 'lib>, PdfiumError> {
        let content = self.content(ContentOptions::default());
        Ok(TextPage::build(self, &content))
    }

    /// Render the page to a bitmap at the given DPI.
    pub fn render(&self, dpi: f32) -> Result<Bitmap<'lib>, PdfiumError> {
        self.render_with_form(dpi, None)
    }

    /// Render the page with annotation appearances (widgets included). There
    /// is no form-fill layer in the hayro backend, so `form` only signals
    /// intent; widget appearances are always drawn.
    pub fn render_with_form(
        &self,
        dpi: f32,
        _form: Option<&FormEnvironment>,
    ) -> Result<Bitmap<'lib>, PdfiumError> {
        let scale = dpi / 72.0 * self.user_unit;
        let width = (self.width() * scale).round();
        let height = (self.height() * scale).round();
        if !(width >= 1.0 && height >= 1.0) {
            return Err(PdfiumError::OperationFailed);
        }
        let width = width.min(u16::MAX as f32) as u16;
        let height = height.min(u16::MAX as f32) as u16;
        let view_box = self.view_box().ok_or(PdfiumError::OperationFailed)?;
        let transform = Affine::scale((dpi / 72.0) as f64) * self.viewport_affine(&view_box);
        Ok(self.render_transformed(width, height, transform))
    }

    /// Render with an explicit page → pixel transform. Annotation
    /// appearances are painted (PDFium's `FPDF_ANNOT`), which also covers a
    /// flattened page's widgets.
    pub(crate) fn render_transformed(
        &self,
        width: u16,
        height: u16,
        transform: Affine,
    ) -> Bitmap<'lib> {
        let settings = InterpreterSettings {
            render_annotations: true,
            ..InterpreterSettings::default()
        };
        let cache = RenderCache::new();
        let mut ctx = hayro::vello_cpu::RenderContext::new(width, height);
        render_into(
            self.page,
            &cache,
            &settings,
            &RenderSettings::default(),
            &mut ctx,
            transform,
        );
        ctx.flush();
        let mut pixmap = hayro::vello_cpu::Pixmap::new(ctx.width(), ctx.height());
        ctx.render_with(
            &mut pixmap,
            &mut hayro::vello_cpu::Resources::default(),
            hayro::vello_cpu::RasterizerSettings {
                target_init: hayro::vello_cpu::TargetInit::Clear(WHITE),
                ..Default::default()
            },
        );
        Bitmap::from_premultiplied(
            pixmap.width() as i32,
            pixmap.height() as i32,
            pixmap.data_as_u8_slice(),
        )
    }

    /// Union of all painted object bounds, in page space.
    pub fn content_bounds(&self) -> Option<RectF> {
        self.content(ContentOptions::default())
            .bounds
            .map(RectF::from_kurbo_page)
    }

    /// Bounding boxes of image objects in viewport space, filtered by size
    /// and page coverage.
    pub fn image_bounds(&self, min_size_pt: f32, max_page_coverage: f32) -> Vec<ImageBounds> {
        self.image_objects(min_size_pt, max_page_coverage, false)
            .images
            .into_iter()
            .map(|image| image.bounds)
            .collect()
    }

    /// Metadata (and optionally raw data) for the page's image objects.
    ///
    /// Unlike PDFium's top-level-only walk, images painted inside form
    /// XObjects are included: hayro reports content after form expansion.
    pub fn image_objects(
        &self,
        min_size_pt: f32,
        max_page_coverage: f32,
        include_data: bool,
    ) -> ImageObjects {
        let content = self.content(ContentOptions {
            image_streams: include_data,
            ..ContentOptions::default()
        });
        let page_width = self.width();
        let page_height = self.height();
        let view_box = self.view_box().unwrap_or_default();
        let mut images = Vec::new();
        for (object_index, image) in content.images.iter().enumerate() {
            let bounds = image.bounds();
            let w = bounds.width() as f32;
            let h = bounds.height() as f32;
            if w < min_size_pt || h < min_size_pt {
                continue;
            }
            if w > page_width * max_page_coverage && h > page_height * max_page_coverage {
                continue;
            }
            let viewport = self.bounds_to_viewport(&view_box, &RectF::from_kurbo_page(bounds));
            let [a, b, ..] = image.transform.as_coeffs();
            let (pixel_width, pixel_height, rotation) = if include_data {
                (
                    image.pixel_width,
                    image.pixel_height,
                    (b as f32).atan2(a as f32).to_degrees().rem_euclid(360.0),
                )
            } else {
                (0, 0, 0.0)
            };
            let raw_bytes = include_data.then(|| image.raw.clone()).flatten();
            let jpeg_bytes = (include_data && image.is_dct)
                .then(|| image.raw.clone().filter(|bytes| is_jpeg(bytes)))
                .flatten();
            images.push(ImageObjectInfo {
                object_index,
                bounds: ImageBounds {
                    x: viewport.left,
                    y: viewport.top,
                    width: viewport.right - viewport.left,
                    height: viewport.bottom - viewport.top,
                },
                pixel_width,
                pixel_height,
                rotation,
                jpeg_bytes,
                raw_bytes,
                bits_per_pixel: image.bits_per_pixel,
                colorspace: image.colorspace,
            });
        }
        ImageObjects {
            images,
            error_count: 0,
        }
    }

    /// Bounding boxes of dark, opaque filled paths, reported like PDFium's
    /// helper: `x` is the raw page-space left edge and `y` is measured down
    /// from the top of the (rotated) page height.
    pub fn filled_path_bounds(&self, min_size_pt: f32, max_page_coverage: f32) -> Vec<ImageBounds> {
        let content = self.content(ContentOptions::default());
        let page_width = self.width();
        let page_height = self.height();
        let mut out = Vec::new();
        for path in &content.paths {
            if !path.filled {
                continue;
            }
            if let Some(fill) = path.fill {
                if fill.a < 128 {
                    continue;
                }
                let luminance =
                    0.299 * fill.r as f32 + 0.587 * fill.g as f32 + 0.114 * fill.b as f32;
                if luminance > 140.0 {
                    continue;
                }
            }
            let bounds = path.bounds();
            let (w, h) = (bounds.width() as f32, bounds.height() as f32);
            if w < min_size_pt || h < min_size_pt {
                continue;
            }
            if w > page_width * max_page_coverage && h > page_height * max_page_coverage {
                continue;
            }
            out.push(ImageBounds {
                x: bounds.x0 as f32,
                y: page_height - bounds.y1 as f32,
                width: w,
                height: h,
            });
        }
        out
    }

    /// The decoded pixels of image object `image_obj_index` (in the order
    /// [`Self::image_objects`] reports `object_index`).
    pub fn render_image_object(&self, image_obj_index: usize) -> Result<Bitmap<'lib>, PdfiumError> {
        let content = self.content(ContentOptions {
            image_pixels: true,
            ..ContentOptions::default()
        });
        let image = content
            .images
            .get(image_obj_index)
            .ok_or(PdfiumError::OperationFailed)?;
        let (width, height, rgba) = image.rgba.as_ref().ok_or(PdfiumError::OperationFailed)?;
        if *width == 0 || *height == 0 {
            return Err(PdfiumError::OperationFailed);
        }
        Ok(Bitmap::from_rgba(
            *width as i32,
            *height as i32,
            rgba.clone(),
        ))
    }

    /// Vector path objects with points in viewport space.
    pub fn path_objects(&self, view_box: &RectF) -> Vec<PathObject> {
        let content = self.content(ContentOptions::default());
        let vp = self.viewport_transform(view_box);
        content
            .paths
            .iter()
            .map(|path| {
                let bounds = RectF::from_kurbo_page(path.bounds());
                PathObject {
                    bbox: vp.transform_bounds(&bounds),
                    stroke_color: path.stroke,
                    fill_color: path.fill,
                    stroke_width: path.local_line_width,
                    is_stroked: path.stroked,
                    is_filled: path.filled,
                    segments: path_segments(&path.path, &vp),
                }
            })
            .collect()
    }

    /// Text-run bounds that overlap any of `rects` (viewport space).
    pub fn text_objects_overlap(&self, view_box: &RectF, rects: &[RectF]) -> bool {
        if rects.is_empty() {
            return false;
        }
        let content = self.content(ContentOptions::default());
        content
            .runs
            .iter()
            .filter(|run| !run.from_widget)
            .any(|run| {
                let Some(bounds) = super::content::run_bounds(run) else {
                    return false;
                };
                let bounds = self.bounds_to_viewport(view_box, &RectF::from_kurbo_page(bounds));
                rects.iter().any(|rect| {
                    bounds.left < rect.right
                        && bounds.right > rect.left
                        && bounds.top < rect.bottom
                        && bounds.bottom > rect.top
                })
            })
    }

    /// Page objects are not exposed individually by the hayro backend.
    pub fn object_count(&self) -> usize {
        0
    }

    pub fn object(&self, _index: usize) -> Option<PageObject> {
        None
    }

    pub(crate) fn hayro_page(&self) -> &'doc HayroPage<'doc> {
        self.page
    }

    pub(crate) fn document(&self) -> &'doc Document<'lib> {
        self.doc
    }

    pub(crate) fn page_index(&self) -> usize {
        self.index
    }
}

pub(crate) fn native_rotation(page: &HayroPage<'_>) -> i32 {
    match page.rotation() {
        Rotation::None => 0,
        Rotation::Horizontal => 1,
        Rotation::Flipped => 2,
        Rotation::FlippedHorizontal => 3,
    }
}

/// PDFium-style segments: one per point, curves as three `BezierTo` points,
/// and `close` set on the segment that ends a closed subpath.
fn path_segments(path: &hayro::kurbo::BezPath, vp: &ViewportTransform) -> Vec<PathSegment> {
    let mut segments: Vec<PathSegment> = Vec::with_capacity(path.elements().len());
    let mut current = hayro::kurbo::Point::ZERO;
    let push = |segments: &mut Vec<PathSegment>, kind, p: hayro::kurbo::Point| {
        let (x, y) = vp.transform_point(p.x as f32, p.y as f32);
        segments.push(PathSegment {
            kind,
            x,
            y,
            close: false,
        });
    };
    for el in path.elements() {
        match *el {
            PathEl::MoveTo(p) => {
                push(&mut segments, SegmentKind::MoveTo, p);
                current = p;
            }
            PathEl::LineTo(p) => {
                push(&mut segments, SegmentKind::LineTo, p);
                current = p;
            }
            PathEl::QuadTo(c, p) => {
                let c1 = current + (c - current) * (2.0 / 3.0);
                let c2 = p + (c - p) * (2.0 / 3.0);
                for q in [c1, c2, p] {
                    push(&mut segments, SegmentKind::BezierTo, q);
                }
                current = p;
            }
            PathEl::CurveTo(c1, c2, p) => {
                for q in [c1, c2, p] {
                    push(&mut segments, SegmentKind::BezierTo, q);
                }
                current = p;
            }
            PathEl::ClosePath => {
                if let Some(last) = segments.last_mut() {
                    last.close = true;
                }
            }
        }
    }
    segments
}

fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xff, 0xd8, 0xff]) && bytes.ends_with(&[0xff, 0xd9])
}

/// Pre-computed affine transform from page space to viewport space.
#[derive(Debug, Clone, Copy)]
pub struct ViewportTransform {
    pub(crate) a: f32,
    pub(crate) b: f32,
    pub(crate) c: f32,
    pub(crate) d: f32,
    pub(crate) e: f32,
    pub(crate) f: f32,
}

impl ViewportTransform {
    /// Transform a single point from page space to viewport space.
    #[inline]
    pub fn transform_point(&self, page_x: f32, page_y: f32) -> (f32, f32) {
        (
            self.a * page_x + self.b * page_y + self.e,
            self.c * page_x + self.d * page_y + self.f,
        )
    }

    /// Transform a bounding rect from page space to viewport space.
    #[inline]
    pub fn transform_bounds(&self, page_bounds: &RectF) -> RectF {
        let (ll_x, ll_y) = self.transform_point(page_bounds.left, page_bounds.bottom);
        let (ur_x, ur_y) = self.transform_point(page_bounds.right, page_bounds.top);
        RectF {
            left: ll_x.min(ur_x),
            top: ll_y.min(ur_y),
            right: ll_x.max(ur_x),
            bottom: ll_y.max(ur_y),
        }
    }
}
