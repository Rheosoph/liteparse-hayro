use crate::ffi;

/// Wrapper around FPDF_FONT obtained from a text object.
/// This is a borrowed handle — it does not own the font and must not outlive
/// the page object it was obtained from.
#[derive(Clone)]
pub struct Font {
    handle: pdfium_sys::FPDF_FONT,
}

/// Font type enum matching PDFium's FPDF_FONT_TYPE values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontType {
    Unknown,
    Type1,
    TrueType,
    Type0,
    Type3,
    CidType0,
    CidType2,
}

impl Font {
    /// Create a Font from a text page object handle.
    /// Returns None if the object has no font.
    ///
    /// # Safety
    /// `obj` must be a valid `FPDF_PAGEOBJECT` handle obtained from PDFium.
    pub unsafe fn from_text_object(obj: pdfium_sys::FPDF_PAGEOBJECT) -> Option<Self> {
        let handle = unsafe { ffi!(FPDFTextObj_GetFont(obj)) };
        if handle.is_null() {
            None
        } else {
            Some(Font { handle })
        }
    }

    pub fn handle(&self) -> pdfium_sys::FPDF_FONT {
        self.handle
    }

    /// Get the base font name (PostScript name, subset prefix stripped by PDFium).
    pub fn base_name(&self) -> Option<String> {
        let len = unsafe {
            ffi!(FPDFFont_GetBaseFontName(
                self.handle,
                std::ptr::null_mut(),
                0
            ))
        };
        if len == 0 {
            return None;
        }
        let mut buf: Vec<u8> = vec![0; len];
        let written = unsafe {
            ffi!(FPDFFont_GetBaseFontName(
                self.handle,
                buf.as_mut_ptr() as *mut std::ffi::c_char,
                len,
            ))
        };
        if written == 0 {
            return None;
        }
        // Strip trailing NUL
        let str_len = if written > 0 && buf[written - 1] == 0 {
            written - 1
        } else {
            written
        };
        Some(String::from_utf8_lossy(&buf[..str_len]).into_owned())
    }

    /// Get font type.
    pub fn font_type(&self) -> FontType {
        let t = unsafe { ffi!(FPDFFont_GetType(self.handle)) };
        match t {
            pdfium_sys::FPDF_FONT_TYPE_FPDF_FONTTYPE_TYPE1 => FontType::Type1,
            pdfium_sys::FPDF_FONT_TYPE_FPDF_FONTTYPE_TRUETYPE => FontType::TrueType,
            pdfium_sys::FPDF_FONT_TYPE_FPDF_FONTTYPE_TYPE0 => FontType::Type0,
            pdfium_sys::FPDF_FONT_TYPE_FPDF_FONTTYPE_TYPE3 => FontType::Type3,
            pdfium_sys::FPDF_FONT_TYPE_FPDF_FONTTYPE_CID_TYPE0 => FontType::CidType0,
            pdfium_sys::FPDF_FONT_TYPE_FPDF_FONTTYPE_CID_TYPE2 => FontType::CidType2,
            _ => FontType::Unknown,
        }
    }

    /// Whether the font is embedded in the PDF.
    pub fn is_embedded(&self) -> bool {
        unsafe { ffi!(FPDFFont_GetIsEmbedded(self.handle)) != 0 }
    }

    /// Get font ascent for a given em size.
    pub fn ascent(&self, font_size: f32) -> Option<f32> {
        let mut val: f32 = 0.0;
        let ok = unsafe { ffi!(FPDFFont_GetAscent(self.handle, font_size, &mut val)) };
        if ok != 0 { Some(val) } else { None }
    }

    /// Get font descent for a given em size (typically negative).
    pub fn descent(&self, font_size: f32) -> Option<f32> {
        let mut val: f32 = 0.0;
        let ok = unsafe { ffi!(FPDFFont_GetDescent(self.handle, font_size, &mut val)) };
        if ok != 0 { Some(val) } else { None }
    }

    /// Get glyph width using the raw character code.
    pub fn glyph_width_from_char_code(&self, char_code: u32, font_size: f32) -> Option<f32> {
        let mut width: f32 = 0.0;
        let ok = unsafe {
            ffi!(FPDFFont_GetGlyphWidthFromCharCode(
                self.handle,
                char_code,
                font_size,
                &mut width,
            ))
        };
        if ok != 0 { Some(width) } else { None }
    }

    /// Walk the vector outline of the glyph for `char_code`, returning one
    /// entry per path segment as `(segment_type, x, y)`. `segment_type` is the
    /// raw PDFium `FPDF_SEGMENT_*` value (LINETO=0, BEZIERTO=1, MOVETO=2).
    /// Segments with type `FPDF_SEGMENT_UNKNOWN` (-1) are skipped, and a point
    /// that cannot be read is reported as `(type, 0.0, 0.0)` — matching the
    /// platform's `hashGlyphPath` packing convention so a downstream hash of
    /// these segments reproduces the platform font DB's `pathHash` key.
    ///
    /// Returns `None` when the font has no outline for this char code
    /// (e.g. whitespace / non-rendered glyph), distinct from `Some(vec![])`.
    pub fn glyph_path_segments(
        &self,
        char_code: u32,
        font_size: f32,
    ) -> Option<Vec<(i32, f32, f32)>> {
        let glyph_path = unsafe {
            ffi!(FPDFFont_GetGlyphPathFromCharCode(
                self.handle,
                char_code,
                font_size
            ))
        };
        if glyph_path.is_null() {
            return None;
        }
        let count = unsafe { ffi!(FPDFGlyphPath_CountGlyphSegments(glyph_path)) };
        let mut segments = Vec::new();
        for i in 0..count {
            let segment = unsafe { ffi!(FPDFGlyphPath_GetGlyphPathSegment(glyph_path, i)) };
            if segment.is_null() {
                break;
            }
            let seg_type = unsafe { ffi!(FPDFPathSegment_GetType(segment)) };
            if seg_type == pdfium_sys::FPDF_SEGMENT_UNKNOWN {
                continue;
            }
            let mut x: f32 = 0.0;
            let mut y: f32 = 0.0;
            let ok = unsafe { ffi!(FPDFPathSegment_GetPoint(segment, &mut x, &mut y)) };
            if ok == 0 {
                x = 0.0;
                y = 0.0;
            }
            segments.push((seg_type, x, y));
        }
        Some(segments)
    }

    /// Get glyph width using a Unicode codepoint.
    pub fn glyph_width(&self, unicode: u32, font_size: f32) -> Option<f32> {
        let mut width: f32 = 0.0;
        let ok = unsafe {
            ffi!(FPDFFont_GetGlyphWidth(
                self.handle,
                unicode,
                font_size,
                &mut width
            ))
        };
        if ok != 0 { Some(width) } else { None }
    }

    /// Whether the font dictionary defines a /ToUnicode CMap. When false, the
    /// unicode values PDFium reports for this font's chars are derived from
    /// the encoding alone and may be garbage for custom/Identity encodings.
    pub fn has_to_unicode(&self) -> bool {
        unsafe { ffi!(FPDFFont_HasToUnicode(self.handle)) != 0 }
    }

    /// Get the font's /Encoding name ("WinAnsiEncoding", "Identity-H", ...),
    /// the /BaseEncoding name when /Encoding is a dict, or "Custom" for
    /// font-private encodings.
    pub fn encoding(&self) -> Option<String> {
        let len =
            unsafe { ffi!(FPDFFont_GetEncoding(self.handle, std::ptr::null_mut(), 0)) } as usize;
        if len == 0 {
            return None;
        }
        let mut buf: Vec<u8> = vec![0; len];
        let written = unsafe {
            ffi!(FPDFFont_GetEncoding(
                self.handle,
                buf.as_mut_ptr() as *mut std::ffi::c_char,
                len as u64 as _,
            ))
        } as usize;
        if written == 0 {
            return None;
        }
        let str_len = if buf[written - 1] == 0 {
            written - 1
        } else {
            written
        };
        Some(String::from_utf8_lossy(&buf[..str_len]).into_owned())
    }

    /// Get the PostScript glyph name the font assigns to a raw char code
    /// (from /Encoding /Differences, falling back to the embedded font
    /// program's glyph name table). Resolve against the Adobe Glyph List to
    /// recover unicode when /ToUnicode is missing.
    pub fn char_glyph_name(&self, char_code: u32) -> Option<String> {
        let len = unsafe {
            ffi!(FPDFFont_GetCharGlyphName(
                self.handle,
                char_code,
                std::ptr::null_mut(),
                0
            ))
        } as usize;
        if len == 0 {
            return None;
        }
        let mut buf: Vec<u8> = vec![0; len];
        let written = unsafe {
            ffi!(FPDFFont_GetCharGlyphName(
                self.handle,
                char_code,
                buf.as_mut_ptr() as *mut std::ffi::c_char,
                len as u64 as _,
            ))
        } as usize;
        if written == 0 {
            return None;
        }
        let str_len = if buf[written - 1] == 0 {
            written - 1
        } else {
            written
        };
        Some(String::from_utf8_lossy(&buf[..str_len]).into_owned())
    }

    /// Get the glyph index in the embedded font program for a raw char code.
    /// Pair with glyph-path rendering for a per-glyph OCR fallback.
    pub fn char_glyph_index(&self, char_code: u32) -> Option<u32> {
        let idx = unsafe { ffi!(FPDFFont_GetCharGlyphIndex(self.handle, char_code)) };
        if idx >= 0 { Some(idx as u32) } else { None }
    }

    /// Get the embedded font program bytes (decompressed FontFile/2/3 stream,
    /// or the substitute font data for non-embedded fonts).
    pub fn font_data(&self) -> Option<Vec<u8>> {
        let mut size: usize = 0;
        let ok = unsafe {
            ffi!(FPDFFont_GetFontData(
                self.handle,
                std::ptr::null_mut(),
                0,
                &mut size
            ))
        };
        if ok == 0 || size == 0 {
            return None;
        }
        let mut buf: Vec<u8> = vec![0; size];
        let mut written: usize = 0;
        let ok = unsafe {
            ffi!(FPDFFont_GetFontData(
                self.handle,
                buf.as_mut_ptr(),
                buf.len(),
                &mut written
            ))
        };
        if ok == 0 || written == 0 {
            return None;
        }
        buf.truncate(written);
        Some(buf)
    }
}
