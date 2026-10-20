//! Typed, borrowed access to a page's content objects: pdfium's page-object API
//! (`FPDFPage_GetObject`, `FPDFPageObj_*`, `FPDFFormObj_*`, `FPDFPath_*`,
//! `FPDFImageObj_*`) as thin accessors over one handle.
//!
//! [`Page::path_objects`] and [`Page::image_objects`] walk objects with this crate's
//! own rules (form recursion depth, size filters, viewport transforms). A caller
//! that has to reproduce *another* extractor's walk — which forms to descend, how
//! matrices compose, what an object's bounds mean — needs the objects themselves.
//! Every method here is one pdfium call with no policy on top: geometry is left in
//! the space pdfium reports it in, failures are `None`, and nothing is filtered.
//!
//! [`Page::path_objects`]: crate::Page::path_objects
//! [`Page::image_objects`]: crate::Page::image_objects

use std::marker::PhantomData;

use crate::bitmap::Bitmap;
use crate::ffi;
use crate::library::Library;
use crate::page::{Page, SegmentKind, image_object_data, read_color};
use crate::types::{Color, Matrix, RectF};

/// What a page object draws (`FPDFPageObj_GetType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageObjectKind {
    Text,
    Path,
    Image,
    Shading,
    /// A Form XObject: a container whose children are reached through
    /// [`PageObject::form_object`].
    Form,
    Unknown,
}

/// `FPDFPath_GetDrawMode`: whether a path is painted at all. A path that is
/// neither filled nor stroked is a clipping path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathDrawMode {
    /// Fill mode is not `FPDF_FILLMODE_NONE`.
    pub filled: bool,
    pub stroked: bool,
}

/// One path segment exactly as pdfium reports it: the point is in the object's own
/// coordinate space (apply [`PageObject::matrix`] to reach the page), `kind` is
/// `None` for `FPDF_SEGMENT_UNKNOWN`, `point` is `None` when
/// `FPDFPathSegment_GetPoint` fails.
#[derive(Debug, Clone, Copy)]
pub struct RawPathSegment {
    pub kind: Option<SegmentKind>,
    pub point: Option<(f32, f32)>,
    /// Whether this segment closes the current subpath.
    pub close: bool,
}

/// `FPDF_IMAGEOBJ_METADATA`: facts about an image object's pixels. The DPI values
/// are the image's effective resolution as placed on its page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImageMetadata {
    pub width: u32,
    pub height: u32,
    pub horizontal_dpi: f32,
    pub vertical_dpi: f32,
    pub bits_per_pixel: u32,
    /// An `FPDF_COLORSPACE_*` value.
    pub colorspace: i32,
    /// `-1` when the image is not in marked content.
    pub marked_content_id: i32,
}

/// One content object of a page, borrowed for `'page`.
///
/// Objects inside a Form XObject are reached through [`PageObject::form_object`]
/// and carry the same borrow: pdfium owns every object for as long as the page is
/// loaded, so none can outlive it.
pub struct PageObject<'page, 'lib> {
    handle: pdfium_sys::FPDF_PAGEOBJECT,
    page: pdfium_sys::FPDF_PAGE,
    _page: PhantomData<&'page ()>,
    _lib: PhantomData<&'lib Library>,
}

impl<'doc, 'lib: 'doc> Page<'doc, 'lib> {
    /// Number of top-level content objects (`FPDFPage_CountObjects`), 0 on error.
    pub fn object_count(&self) -> usize {
        let count = unsafe { ffi!(FPDFPage_CountObjects(self.handle)) };
        usize::try_from(count).unwrap_or(0)
    }

    /// The top-level content object at `index`, in content-stream order.
    pub fn object(&self, index: usize) -> Option<PageObject<'_, 'lib>> {
        let index = std::os::raw::c_int::try_from(index).ok()?;
        let handle = unsafe { ffi!(FPDFPage_GetObject(self.handle, index)) };
        (!handle.is_null()).then_some(PageObject {
            handle,
            page: self.handle,
            _page: PhantomData,
            _lib: PhantomData,
        })
    }
}

impl<'page, 'lib> PageObject<'page, 'lib> {
    /// An identity for this object that is stable while the page is loaded, so a
    /// caller can tell "the same form" apart from "a different form" across two
    /// visits without holding the object.
    pub fn id(&self) -> usize {
        self.handle as usize
    }

    pub fn kind(&self) -> PageObjectKind {
        match unsafe { ffi!(FPDFPageObj_GetType(self.handle)) } as u32 {
            pdfium_sys::FPDF_PAGEOBJ_TEXT => PageObjectKind::Text,
            pdfium_sys::FPDF_PAGEOBJ_PATH => PageObjectKind::Path,
            pdfium_sys::FPDF_PAGEOBJ_IMAGE => PageObjectKind::Image,
            pdfium_sys::FPDF_PAGEOBJ_SHADING => PageObjectKind::Shading,
            pdfium_sys::FPDF_PAGEOBJ_FORM => PageObjectKind::Form,
            _ => PageObjectKind::Unknown,
        }
    }

    /// Clip paths in the containing form's coordinates (page coordinates at the
    /// top level). The object's own matrix is already applied to clip points.
    /// No clip is an empty list; unavailable APIs, failed reads, or more than
    /// 1024 segments return `None`. Text-based clipping is not exposed by PDFium.
    pub fn clip_paths(&self) -> Option<Vec<Vec<RawPathSegment>>> {
        #[cfg(not(target_arch = "wasm32"))]
        let (get_clip, count_paths, count_segments, get_segment) = {
            let api = pdfium_sys::dynamic::pdfium();
            (
                api.FPDFPageObj_GetClipPath?,
                api.FPDFClipPath_CountPaths?,
                api.FPDFClipPath_CountPathSegments?,
                api.FPDFClipPath_GetPathSegment?,
            )
        };
        #[cfg(target_arch = "wasm32")]
        let (get_clip, count_paths, count_segments, get_segment) = (
            pdfium_sys::FPDFPageObj_GetClipPath,
            pdfium_sys::FPDFClipPath_CountPaths,
            pdfium_sys::FPDFClipPath_CountPathSegments,
            pdfium_sys::FPDFClipPath_GetPathSegment,
        );
        let clip = unsafe { get_clip(self.handle) };
        if clip.is_null() {
            return None;
        }
        let count = unsafe { count_paths(clip) };
        // PDFium returns -1 for an object with no clip-path reference.
        if count == -1 {
            return Some(Vec::new());
        }
        if !(0..=1024).contains(&count) {
            return None;
        }
        let mut paths = Vec::new();
        let mut budget = 1024;
        for path in 0..count {
            let count = unsafe { count_segments(clip, path) };
            if count < 0 || count > budget {
                return None;
            }
            budget -= count;
            let mut segments = Vec::new();
            for index in 0..count {
                let segment = unsafe { get_segment(clip, path, index) };
                if segment.is_null() {
                    return None;
                }
                let kind = match unsafe { ffi!(FPDFPathSegment_GetType(segment)) } {
                    t if t == pdfium_sys::FPDF_SEGMENT_MOVETO as i32 => Some(SegmentKind::MoveTo),
                    t if t == pdfium_sys::FPDF_SEGMENT_LINETO as i32 => Some(SegmentKind::LineTo),
                    t if t == pdfium_sys::FPDF_SEGMENT_BEZIERTO as i32 => {
                        Some(SegmentKind::BezierTo)
                    }
                    _ => None,
                };
                let mut x = 0.0;
                let mut y = 0.0;
                if unsafe { ffi!(FPDFPathSegment_GetPoint(segment, &mut x, &mut y)) } == 0 {
                    return None;
                }
                segments.push(RawPathSegment {
                    kind,
                    point: Some((x, y)),
                    close: unsafe { ffi!(FPDFPathSegment_GetClose(segment)) } != 0,
                });
            }
            paths.push(segments);
        }
        Some(paths)
    }

    /// The object's transformation matrix (`FPDFPageObj_GetMatrix`). For an object
    /// inside a form this is relative to the form, not the page. `None` when pdfium
    /// reports none — normal for a shading object without a clip path.
    pub fn matrix(&self) -> Option<Matrix> {
        let mut m = pdfium_sys::FS_MATRIX {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: 1.0,
            e: 0.0,
            f: 0.0,
        };
        let ok = unsafe { ffi!(FPDFPageObj_GetMatrix(self.handle, &mut m)) };
        (ok != 0).then_some(Matrix {
            a: m.a,
            b: m.b,
            c: m.c,
            d: m.d,
            e: m.e,
            f: m.f,
        })
    }

    /// The object's bounding box (`FPDFPageObj_GetBounds`) in the space pdfium
    /// reports it in — its own matrix applied, ancestor form matrices not — as a
    /// y-up rect (`top > bottom`).
    pub fn bounds(&self) -> Option<RectF> {
        let mut left = 0.0f32;
        let mut bottom = 0.0f32;
        let mut right = 0.0f32;
        let mut top = 0.0f32;
        let ok = unsafe {
            ffi!(FPDFPageObj_GetBounds(
                self.handle,
                &mut left,
                &mut bottom,
                &mut right,
                &mut top
            ))
        };
        (ok != 0).then_some(RectF {
            left,
            top,
            right,
            bottom,
        })
    }

    /// Number of objects inside a Form XObject (`FPDFFormObj_CountObjects`); `None`
    /// when this is not a form or pdfium reports an error.
    pub fn form_object_count(&self) -> Option<usize> {
        let count = unsafe { ffi!(FPDFFormObj_CountObjects(self.handle)) };
        usize::try_from(count).ok()
    }

    /// The child at `index` of a Form XObject, in content-stream order.
    pub fn form_object(&self, index: usize) -> Option<PageObject<'page, 'lib>> {
        let index = std::os::raw::c_ulong::try_from(index).ok()?;
        let handle = unsafe { ffi!(FPDFFormObj_GetObject(self.handle, index)) };
        (!handle.is_null()).then_some(PageObject {
            handle,
            page: self.page,
            _page: PhantomData,
            _lib: PhantomData,
        })
    }

    /// A path's fill/stroke mode (`FPDFPath_GetDrawMode`); `None` for non-paths.
    pub fn path_draw_mode(&self) -> Option<PathDrawMode> {
        let mut fill_mode = 0i32;
        let mut stroke = 0i32;
        let ok = unsafe {
            ffi!(FPDFPath_GetDrawMode(
                self.handle,
                &mut fill_mode,
                &mut stroke
            ))
        };
        (ok != 0).then_some(PathDrawMode {
            filled: fill_mode != pdfium_sys::FPDF_FILLMODE_NONE as i32,
            stroked: stroke != 0,
        })
    }

    /// Line width in the object's own space (`FPDFPageObj_GetStrokeWidth`).
    pub fn stroke_width(&self) -> Option<f32> {
        let mut width = 0.0f32;
        let ok = unsafe { ffi!(FPDFPageObj_GetStrokeWidth(self.handle, &mut width)) };
        (ok != 0).then_some(width)
    }

    /// `FPDFPageObj_GetStrokeColor`; `None` when the object has no RGB stroke colour
    /// (a pattern, or pdfium could not resolve the colour space).
    pub fn stroke_color(&self) -> Option<Color> {
        read_color(|r, g, b, a| unsafe {
            ffi!(FPDFPageObj_GetStrokeColor(self.handle, r, g, b, a))
        })
    }

    /// `FPDFPageObj_GetFillColor`; `None` on the same terms as [`Self::stroke_color`].
    pub fn fill_color(&self) -> Option<Color> {
        read_color(|r, g, b, a| unsafe { ffi!(FPDFPageObj_GetFillColor(self.handle, r, g, b, a)) })
    }

    /// Number of segments in a path (`FPDFPath_CountSegments`); `None` for non-paths.
    pub fn path_segment_count(&self) -> Option<usize> {
        let count = unsafe { ffi!(FPDFPath_CountSegments(self.handle)) };
        usize::try_from(count).ok()
    }

    /// The segment at `index`, or `None` when pdfium has no segment there.
    pub fn path_segment(&self, index: usize) -> Option<RawPathSegment> {
        let index = std::os::raw::c_int::try_from(index).ok()?;
        let segment = unsafe { ffi!(FPDFPath_GetPathSegment(self.handle, index)) };
        if segment.is_null() {
            return None;
        }
        let kind = match unsafe { ffi!(FPDFPathSegment_GetType(segment)) } {
            t if t == pdfium_sys::FPDF_SEGMENT_MOVETO as i32 => Some(SegmentKind::MoveTo),
            t if t == pdfium_sys::FPDF_SEGMENT_LINETO as i32 => Some(SegmentKind::LineTo),
            t if t == pdfium_sys::FPDF_SEGMENT_BEZIERTO as i32 => Some(SegmentKind::BezierTo),
            _ => None,
        };
        let mut x = 0.0f32;
        let mut y = 0.0f32;
        let ok = unsafe { ffi!(FPDFPathSegment_GetPoint(segment, &mut x, &mut y)) };
        let close = unsafe { ffi!(FPDFPathSegment_GetClose(segment)) } != 0;
        Some(RawPathSegment {
            kind,
            point: (ok != 0).then_some((x, y)),
            close,
        })
    }

    /// An image's pixel facts (`FPDFImageObj_GetImageMetadata`, which needs the page
    /// to work out the effective DPI); `None` for non-images or on failure.
    pub fn image_metadata(&self) -> Option<ImageMetadata> {
        let mut metadata = pdfium_sys::FPDF_IMAGEOBJ_METADATA::default();
        let ok = unsafe {
            ffi!(FPDFImageObj_GetImageMetadata(
                self.handle,
                self.page,
                &mut metadata
            ))
        };
        (ok != 0).then_some(ImageMetadata {
            width: metadata.width,
            height: metadata.height,
            horizontal_dpi: metadata.horizontal_dpi,
            vertical_dpi: metadata.vertical_dpi,
            bits_per_pixel: metadata.bits_per_pixel,
            colorspace: metadata.colorspace,
            marked_content_id: metadata.marked_content_id,
        })
    }

    /// Number of filters on an image's stream (`FPDFImageObj_GetImageFilterCount`);
    /// `None` for non-images or on error.
    pub fn image_filter_count(&self) -> Option<usize> {
        let count = unsafe { ffi!(FPDFImageObj_GetImageFilterCount(self.handle)) };
        usize::try_from(count).ok()
    }

    /// The filter name at `index` (`DCTDecode`, `FlateDecode`, …), however long;
    /// `None` when pdfium reports nothing for that index.
    pub fn image_filter(&self, index: usize) -> Option<String> {
        let index = std::os::raw::c_int::try_from(index).ok()?;
        let size = unsafe {
            ffi!(FPDFImageObj_GetImageFilter(
                self.handle,
                index,
                std::ptr::null_mut(),
                0
            ))
        };
        if size == 0 {
            return None;
        }
        let mut bytes = vec![0u8; size as usize];
        let written = unsafe {
            ffi!(FPDFImageObj_GetImageFilter(
                self.handle,
                index,
                bytes.as_mut_ptr().cast(),
                size
            ))
        };
        if written == 0 {
            return None;
        }
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }

    /// The image stream's bytes as stored, every filter still applied
    /// (`FPDFImageObj_GetImageDataRaw`).
    pub fn image_data_raw(&self) -> Option<Vec<u8>> {
        image_object_data(self.handle, false)
    }

    /// The image stream after pdfium undoes its lossless filters
    /// (`FPDFImageObj_GetImageDataDecoded`): for a `DCTDecode` image this is the
    /// JPEG file itself.
    pub fn image_data_decoded(&self) -> Option<Vec<u8>> {
        image_object_data(self.handle, true)
    }

    /// The image's own pixels at their stored size (`FPDFImageObj_GetBitmap`) — not
    /// rendered through the object's matrix, unlike `FPDFImageObj_GetRenderedBitmap`.
    /// The format is whatever pdfium decoded to; read [`Bitmap::format`] before the
    /// buffer.
    pub fn image_bitmap(&self) -> Option<Bitmap<'lib>> {
        let handle = unsafe { ffi!(FPDFImageObj_GetBitmap(self.handle)) };
        if handle.is_null() {
            return None;
        }
        // SAFETY: a freshly created bitmap we own; `'lib` is the page's lock lifetime.
        Some(unsafe { Bitmap::from_handle(handle) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Library;
    use crate::bitmap::BitmapFormat;

    /// A one-page PDF with, in content order: a filled rectangle under a translation,
    /// a Form XObject holding one stroked rectangle, and a 2×2 8-bit grey image.
    fn objects_pdf() -> Vec<u8> {
        fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
            let mut out = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
            out.extend_from_slice(data);
            out.extend_from_slice(b"\nendstream");
            out
        }
        let content =
            b"q 1 0 0 1 10 20 cm 0 0 50 30 re f Q q /Fx1 Do Q q 40 0 0 20 100 50 cm /Im1 Do Q";
        let objects: Vec<Vec<u8>> = vec![
            b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R /Resources << /XObject << /Im1 5 0 R /Fx1 6 0 R >> >> >>".to_vec(),
            stream("", content),
            stream(
                "/Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8",
                &[0x00, 0xff, 0xff, 0x00],
            ),
            stream(
                "/Type /XObject /Subtype /Form /BBox [0 0 100 100] /Matrix [2 0 0 2 5 5]",
                b"0 0 10 10 re S",
            ),
        ];
        let mut pdf = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::with_capacity(objects.len());
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
            pdf.extend_from_slice(object);
            pdf.extend_from_slice(b"\nendobj\n");
        }
        let xref = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[test]
    fn walks_path_form_and_image_objects_with_raw_accessors() {
        let bytes = objects_pdf();
        let library = Library::init();
        let document = library.load_document_from_bytes(&bytes, None).unwrap();
        let page = document.page(0).unwrap();

        assert_eq!(page.object_count(), 3);
        assert!(page.object(3).is_none());
        let kinds: Vec<PageObjectKind> = (0..3).map(|i| page.object(i).unwrap().kind()).collect();
        assert_eq!(
            kinds,
            [
                PageObjectKind::Path,
                PageObjectKind::Form,
                PageObjectKind::Image
            ]
        );

        // The filled rectangle: its matrix carries the `cm`, its points are local.
        let path = page.object(0).unwrap();
        assert_eq!(
            path.path_draw_mode(),
            Some(PathDrawMode {
                filled: true,
                stroked: false
            })
        );
        let m = path.matrix().unwrap();
        assert_eq!((m.a, m.d, m.e, m.f), (1.0, 1.0, 10.0, 20.0));
        let b = path.bounds().unwrap();
        assert_eq!((b.left, b.bottom, b.right, b.top), (10.0, 20.0, 60.0, 50.0));
        assert_eq!(path.fill_color().map(|c| (c.r, c.g, c.b)), Some((0, 0, 0)));
        let count = path.path_segment_count().unwrap();
        assert!(count >= 4);
        let segments: Vec<RawPathSegment> =
            (0..count).map(|i| path.path_segment(i).unwrap()).collect();
        assert_eq!(segments[0].kind, Some(SegmentKind::MoveTo));
        assert_eq!(segments[0].point, Some((0.0, 0.0)));
        assert!(
            segments
                .iter()
                .skip(1)
                .all(|s| s.kind == Some(SegmentKind::LineTo))
        );
        assert!(segments.iter().any(|s| s.close));
        assert!(path.path_segment(count).is_none());
        assert!(path.image_metadata().is_none());

        // The form: one stroked path inside, reached only through `form_object`.
        let form = page.object(1).unwrap();
        assert_eq!(form.form_object_count(), Some(1));
        let inner = form.form_object(0).unwrap();
        assert_eq!(inner.kind(), PageObjectKind::Path);
        assert_eq!(
            inner.path_draw_mode(),
            Some(PathDrawMode {
                filled: false,
                stroked: true
            })
        );
        assert_eq!(inner.stroke_width(), Some(1.0));
        assert!(form.form_object(1).is_none());
        assert_ne!(form.id(), inner.id());
        assert_eq!(page.object(1).unwrap().id(), form.id());

        // The image: metadata, no filters, raw == decoded, and its own grey pixels.
        let image = page.object(2).unwrap();
        let meta = image.image_metadata().unwrap();
        assert_eq!((meta.width, meta.height, meta.bits_per_pixel), (2, 2, 8));
        assert_eq!(
            meta.colorspace,
            pdfium_sys::FPDF_COLORSPACE_DEVICEGRAY as i32
        );
        assert_eq!(image.image_filter_count(), Some(0));
        assert!(image.image_filter(0).is_none());
        assert_eq!(image.image_data_raw().unwrap(), [0x00, 0xff, 0xff, 0x00]);
        assert_eq!(
            image.image_data_decoded().unwrap(),
            [0x00, 0xff, 0xff, 0x00]
        );
        let bitmap = image.image_bitmap().unwrap();
        assert_eq!(bitmap.format(), BitmapFormat::Gray);
        assert_eq!((bitmap.width(), bitmap.height()), (2, 2));
        let stride = bitmap.stride() as usize;
        let buffer = bitmap.buffer();
        assert_eq!(&buffer[..2], &[0x00, 0xff]);
        assert_eq!(&buffer[stride..stride + 2], &[0xff, 0x00]);
        assert!(page.object(0).unwrap().image_bitmap().is_none());
    }
}
