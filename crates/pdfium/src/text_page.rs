use std::marker::PhantomData;

use crate::ffi;
use crate::page::Page;
use crate::types::{CharBox, Color, Matrix, RectF, TextRect};

/// Extracted text content of a [`Page`].
///
/// `'page` is the borrow of the parent page; `'lib` carries the PDFium-lock
/// lifetime through so that no FFI call can occur after the lock is released.
pub struct TextPage<'page, 'lib: 'page> {
    pub(crate) handle: pdfium_sys::FPDF_TEXTPAGE,
    pub(crate) _page: PhantomData<&'page Page<'page, 'lib>>,
}

impl<'page, 'lib: 'page> TextPage<'page, 'lib> {
    pub fn char_count(&self) -> i32 {
        unsafe { ffi!(FPDFText_CountChars(self.handle)) }
    }

    pub fn chars(&self) -> TextCharIter<'_> {
        TextCharIter {
            text_page: self,
            index: 0,
            count: self.char_count(),
        }
    }

    pub fn char_at(&self, index: i32) -> Option<TextChar<'_>> {
        if index >= 0 && index < self.char_count() {
            Some(TextChar {
                text_page: self,
                index,
            })
        } else {
            None
        }
    }

    /// Like `char_at`, but skips the `char_count()` FFI bounds check.
    /// The caller must ensure `index` is in `0..char_count`.
    pub fn char_at_unchecked(&self, index: i32) -> TextChar<'_> {
        TextChar {
            text_page: self,
            index,
        }
    }

    /// Fill `buf` with per-character records starting at `start` using the
    /// fork's `FPDFText_GetCharInfoBatch` (chromium/8028+), replacing several
    /// FFI round-trips per character with one per chunk. Returns the number
    /// of records written (0 at end of range), or `None` when the loaded
    /// pdfium build predates the batch API — callers must fall back to the
    /// per-character getters.
    ///
    /// Record fields mirror the single-char getters exactly: raw page-space
    /// boxes, `char_type` as the raw `CPDF_TextPage::CharType` value
    /// (1 = generated, 2 = no-unicode-mapping), and `text_render_mode` of the
    /// char's text object (-1 when it has none).
    pub fn char_infos_batch(
        &self,
        start: i32,
        buf: &mut [pdfium_sys::FPDF_CHARINFO_LP],
    ) -> Option<usize> {
        #[cfg(not(target_arch = "wasm32"))]
        let written = {
            let batch_fn = pdfium_sys::dynamic::pdfium().FPDFText_GetCharInfoBatch?;
            if buf.is_empty() {
                return Some(0);
            }
            unsafe { batch_fn(self.handle, start, buf.len() as i32, buf.as_mut_ptr()) }
        };
        #[cfg(target_arch = "wasm32")]
        if buf.is_empty() {
            return Some(0);
        }
        #[cfg(target_arch = "wasm32")]
        let written = unsafe {
            pdfium_sys::FPDFText_GetCharInfoBatch(
                self.handle,
                start,
                buf.len() as i32,
                buf.as_mut_ptr(),
            )
        };
        Some(written.max(0) as usize)
    }

    /// Count rectangular areas occupied by a text segment.
    /// Must be called before `rect()`.
    pub fn count_rects(&self, start: i32, count: i32) -> i32 {
        unsafe { ffi!(FPDFText_CountRects(self.handle, start, count)) }
    }

    /// Get a rectangle from the last `count_rects()` call.
    pub fn rect(&self, index: i32) -> Option<TextRect> {
        let mut left = 0.0;
        let mut top = 0.0;
        let mut right = 0.0;
        let mut bottom = 0.0;
        let ok = unsafe {
            ffi!(FPDFText_GetRect(
                self.handle,
                index,
                &mut left,
                &mut top,
                &mut right,
                &mut bottom,
            ))
        };
        if ok != 0 {
            Some(TextRect {
                left,
                top,
                right,
                bottom,
            })
        } else {
            None
        }
    }

    /// Extract text within a rectangular boundary.
    pub fn bounded_text(&self, left: f64, top: f64, right: f64, bottom: f64) -> String {
        // First call to get required buffer length (in UTF-16 code units, excluding terminator)
        let len = unsafe {
            ffi!(FPDFText_GetBoundedText(
                self.handle,
                left,
                top,
                right,
                bottom,
                std::ptr::null_mut(),
                0,
            ))
        };
        if len <= 0 {
            return String::new();
        }

        // Allocate buffer with space for terminator
        let buf_len = len + 1;
        let mut buf: Vec<u16> = vec![0; buf_len as usize];
        let written = unsafe {
            ffi!(FPDFText_GetBoundedText(
                self.handle,
                left,
                top,
                right,
                bottom,
                buf.as_mut_ptr(),
                buf_len,
            ))
        };

        if written <= 0 {
            return String::new();
        }

        // Strip trailing NUL if present
        let text_len = if written > 0 && buf[(written - 1) as usize] == 0 {
            (written - 1) as usize
        } else {
            written as usize
        };

        String::from_utf16_lossy(&buf[..text_len])
    }

    /// Extract text for a range of characters.
    pub fn get_text(&self, start: i32, count: i32) -> String {
        if count <= 0 {
            return String::new();
        }
        let buf_len = count + 1; // +1 for terminator
        let mut buf: Vec<u16> = vec![0; buf_len as usize];
        let written = unsafe {
            ffi!(FPDFText_GetText(
                self.handle,
                start,
                count,
                buf.as_mut_ptr()
            ))
        };
        if written <= 0 {
            return String::new();
        }
        let text_len = if written > 0 && buf[(written - 1) as usize] == 0 {
            (written - 1) as usize
        } else {
            written as usize
        };
        String::from_utf16_lossy(&buf[..text_len])
    }
}

impl Drop for TextPage<'_, '_> {
    fn drop(&mut self) {
        unsafe { ffi!(FPDFText_ClosePage(self.handle)) };
    }
}

// -- TextChar: zero-cost view into a TextPage --

pub struct TextChar<'tp> {
    // We don't care about the inner lifetimes here — we only need to know
    // that we're borrowing the text page (and transitively the library lock)
    // for at least `'tp`. Using `'tp` for the inner params makes the type
    // covariant in `'tp` and lets borrow-checking flow naturally from the
    // outer borrow.
    text_page: &'tp TextPage<'tp, 'tp>,
    pub(crate) index: i32,
}

impl TextChar<'_> {
    /// The glyph's origin in page space (`FPDFText_GetCharOrigin`): the point
    /// the text matrix places on the baseline, in unrotated PDF user units.
    pub fn origin(&self) -> Option<(f64, f64)> {
        let (mut x, mut y) = (0.0f64, 0.0f64);
        let ok = unsafe {
            ffi!(FPDFText_GetCharOrigin(
                self.text_page.handle,
                self.index,
                &mut x,
                &mut y
            ))
        };
        (ok != 0).then_some((x, y))
    }

    pub fn unicode(&self) -> u32 {
        unsafe { ffi!(FPDFText_GetUnicode(self.text_page.handle, self.index)) }
    }

    /// Raw character code from the PDF content stream (not Unicode).
    /// Only meaningful for non-generated characters.
    pub fn char_code(&self) -> u32 {
        unsafe { ffi!(FPDFText_GetCharCode(self.text_page.handle, self.index)) }
    }

    pub fn font_size(&self) -> f64 {
        unsafe { ffi!(FPDFText_GetFontSize(self.text_page.handle, self.index)) }
    }

    pub fn font_weight(&self) -> i32 {
        unsafe { ffi!(FPDFText_GetFontWeight(self.text_page.handle, self.index)) }
    }

    /// Get font info name and flags. Returns (name, flags) or None.
    pub fn font_info(&self) -> Option<(String, i32)> {
        let mut flags: i32 = 0;
        let len = unsafe {
            ffi!(FPDFText_GetFontInfo(
                self.text_page.handle,
                self.index,
                std::ptr::null_mut(),
                0,
                &mut flags,
            ))
        };
        if len == 0 {
            return None;
        }
        let mut buf: Vec<u8> = vec![0; len as usize];
        let written = unsafe {
            ffi!(FPDFText_GetFontInfo(
                self.text_page.handle,
                self.index,
                buf.as_mut_ptr() as *mut std::ffi::c_void,
                len,
                &mut flags,
            ))
        };
        if written == 0 {
            return None;
        }
        let str_len = if written > 0 && buf[(written - 1) as usize] == 0 {
            (written - 1) as usize
        } else {
            written as usize
        };
        Some((String::from_utf8_lossy(&buf[..str_len]).into_owned(), flags))
    }

    pub fn font_name(&self) -> Option<String> {
        self.font_info().map(|(name, _)| name)
    }

    /// Angle in radians. Returns -1 on error.
    pub fn angle(&self) -> f32 {
        unsafe { ffi!(FPDFText_GetCharAngle(self.text_page.handle, self.index)) }
    }

    /// Get the FPDF_PAGEOBJECT for this character (for font/color extraction).
    pub fn text_object(&self) -> Option<pdfium_sys::FPDF_PAGEOBJECT> {
        let obj = unsafe { ffi!(FPDFText_GetTextObject(self.text_page.handle, self.index)) };
        if obj.is_null() { None } else { Some(obj) }
    }

    /// Advance width of the ASCII space (char code 0x20) in this character's
    /// font, expressed per em (i.e. for `font_size = 1.0`). Multiply by the
    /// actual font size to get the space width in text-space points. Returns
    /// `None` when the font or glyph is unavailable. Used to set a font-aware
    /// threshold for detecting word boundaries when PDFium omits space glyphs.
    pub fn font_space_width(&self) -> Option<f32> {
        let obj = self.text_object()?;
        let font = unsafe { crate::font::Font::from_text_object(obj)? };
        font.glyph_width_from_char_code(0x20, 1.0)
            .filter(|w| *w > 0.0)
            .or_else(|| font.glyph_width(0x20, 1.0).filter(|w| *w > 0.0))
    }

    /// Get stroke color (r, g, b, a).
    pub fn stroke_color(&self) -> Option<Color> {
        let mut r = 0u32;
        let mut g = 0u32;
        let mut b = 0u32;
        let mut a = 0u32;
        let ok = unsafe {
            ffi!(FPDFText_GetStrokeColor(
                self.text_page.handle,
                self.index,
                &mut r,
                &mut g,
                &mut b,
                &mut a,
            ))
        };
        if ok != 0 {
            Some(Color {
                r: r as u8,
                g: g as u8,
                b: b as u8,
                a: a as u8,
            })
        } else {
            None
        }
    }

    /// Get fill color (r, g, b, a).
    pub fn fill_color(&self) -> Option<Color> {
        let mut r = 0u32;
        let mut g = 0u32;
        let mut b = 0u32;
        let mut a = 0u32;
        let ok = unsafe {
            ffi!(FPDFText_GetFillColor(
                self.text_page.handle,
                self.index,
                &mut r,
                &mut g,
                &mut b,
                &mut a,
            ))
        };
        if ok != 0 {
            Some(Color {
                r: r as u8,
                g: g as u8,
                b: b as u8,
                a: a as u8,
            })
        } else {
            None
        }
    }

    /// Get text render mode from the page object.
    /// Returns the raw FPDF_TEXT_RENDERMODE value (0=fill, 1=stroke, 2=fill+stroke, 3=invisible, etc.)
    pub fn text_render_mode(&self) -> Option<i32> {
        let obj = self.text_object()?;
        let mode = unsafe { ffi!(FPDFTextObj_GetTextRenderMode(obj)) };
        if mode >= 0 { Some(mode) } else { None }
    }

    /// Get marked content ID from the page object (-1 if none).
    pub fn marked_content_id(&self) -> Option<i32> {
        let obj = self.text_object()?;
        let mcid = unsafe { ffi!(FPDFPageObj_GetMarkedContentID(obj)) };
        if mcid >= 0 { Some(mcid) } else { None }
    }

    pub fn char_box(&self) -> Option<CharBox> {
        let mut left = 0.0;
        let mut right = 0.0;
        let mut bottom = 0.0;
        let mut top = 0.0;
        let ok = unsafe {
            ffi!(FPDFText_GetCharBox(
                self.text_page.handle,
                self.index,
                &mut left,
                &mut right,
                &mut bottom,
                &mut top,
            ))
        };
        if ok != 0 {
            Some(CharBox {
                left,
                right,
                bottom,
                top,
            })
        } else {
            None
        }
    }

    pub fn loose_char_box(&self) -> Option<RectF> {
        let mut rect: pdfium_sys::FS_RECTF = pdfium_sys::FS_RECTF {
            left: 0.0,
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
        };
        let ok = unsafe {
            ffi!(FPDFText_GetLooseCharBox(
                self.text_page.handle,
                self.index,
                &mut rect
            ))
        };
        if ok != 0 {
            Some(RectF {
                left: rect.left,
                top: rect.top,
                right: rect.right,
                bottom: rect.bottom,
            })
        } else {
            None
        }
    }

    pub fn matrix(&self) -> Option<Matrix> {
        let mut m: pdfium_sys::FS_MATRIX = pdfium_sys::FS_MATRIX {
            a: 0.0,
            b: 0.0,
            c: 0.0,
            d: 0.0,
            e: 0.0,
            f: 0.0,
        };
        let ok = unsafe {
            ffi!(FPDFText_GetMatrix(
                self.text_page.handle,
                self.index,
                &mut m
            ))
        };
        if ok != 0 {
            Some(Matrix {
                a: m.a,
                b: m.b,
                c: m.c,
                d: m.d,
                e: m.e,
                f: m.f,
            })
        } else {
            None
        }
    }

    pub fn is_generated(&self) -> bool {
        unsafe { ffi!(FPDFText_IsGenerated(self.text_page.handle, self.index)) == 1 }
    }

    /// A hyphen pdfium found at the end of a line; its unicode reads as 0x02.
    pub fn is_hyphen(&self) -> bool {
        unsafe { ffi!(FPDFText_IsHyphen(self.text_page.handle, self.index)) == 1 }
    }

    pub fn has_unicode_map_error(&self) -> bool {
        unsafe {
            ffi!(FPDFText_HasUnicodeMapError(
                self.text_page.handle,
                self.index
            )) == 1
        }
    }
}

// -- TextCharIter --

pub struct TextCharIter<'tp> {
    text_page: &'tp TextPage<'tp, 'tp>,
    index: i32,
    count: i32,
}

impl<'tp> Iterator for TextCharIter<'tp> {
    type Item = TextChar<'tp>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.count {
            let ch = TextChar {
                text_page: self.text_page,
                index: self.index,
            };
            self.index += 1;
            Some(ch)
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = (self.count - self.index) as usize;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for TextCharIter<'_> {}
