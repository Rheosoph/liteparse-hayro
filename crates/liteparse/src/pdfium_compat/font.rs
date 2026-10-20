//! Font metadata for the compat text API.
//!
//! hayro identifies a glyph's font only by an opaque cache key
//! (`OutlineGlyph::font_cache_key`), which is a hash of the font dictionary's
//! bytes. [`FontCatalog`] walks a page's resources, hashes every font
//! dictionary the same way, and parses the dictionaries into [`FontInfo`], so
//! glyphs can be traced back to their font's name, flags and metrics the way
//! PDFium reports them.

use std::cell::RefCell;
use std::rc::Rc;

use foldhash::{HashMap, HashSet};
use hayro::hayro_interpret::CacheKey;
use hayro::hayro_syntax::object::{Array, Dict, Stream};
use hayro::kurbo::{BezPath, PathEl, Rect};

use super::char_metrics::{CharMetrics, CidUnicode, CodeReader};
use super::objects::dict_name;

/// Base offset of the synthetic character codes the compat layer reports
/// for glyphs whose source code is unknown.
///
/// hayro does not expose a glyph's character code; the content scan
/// ([`super::text_state`]) recovers it from the text-showing operators, and
/// a glyph it cannot place is identified by `SYNTHETIC_CODE_BASE + glyph id`
/// instead. The offset keeps these out of the range of real codes (at most
/// four bytes, but in practice two), so code-based queries stay unambiguous.
pub(crate) const SYNTHETIC_CODE_BASE: u32 = 0x1_0000;

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

/// Font-level facts, mirroring what PDFium (chromium/8058, the build
/// upstream liteparse pins) derives from a font dictionary.
#[derive(Debug, Clone)]
pub(crate) struct FontInfo {
    /// `GetBaseFontName`: see [`FontInfo::from_dict`] for the rules.
    pub base_name: Option<String>,
    pub font_type: FontType,
    pub is_embedded: bool,
    /// `/Flags` with PDFium's italic-angle and external-attribute bits.
    pub flags: i32,
    /// `/StemV`, 0 when absent.
    pub stem_v: i32,
    /// `/FontWeight`, when positive.
    pub font_weight: Option<i32>,
    /// `GetTypeAscent`/`GetTypeDescent` in glyph units (1/1000 em).
    pub ascent: Option<f32>,
    pub descent: Option<f32>,
    /// `/FontBBox` in glyph units.
    pub font_bbox: Option<[f32; 4]>,
    pub has_to_unicode: bool,
    pub encoding: Option<String>,
    /// Advance of char code 0x20 in glyph units, when the font defines it.
    pub space_width: Option<f32>,
    pub is_vertical: bool,
    /// The descriptor gave no ascent or descent: PDFium then measures the
    /// glyphs of codes 'A' and 'g' (`CheckFontMetrics`), and until they are
    /// probed `ascent`/`descent` hold the font box.
    pub metrics_from_glyphs: bool,
}

impl FontInfo {
    fn unknown() -> Self {
        Self {
            base_name: None,
            font_type: FontType::Unknown,
            is_embedded: false,
            flags: FONT_FLAG_NONSYMBOLIC,
            stem_v: 0,
            font_weight: None,
            ascent: None,
            descent: None,
            font_bbox: None,
            has_to_unicode: false,
            encoding: None,
            space_width: None,
            is_vertical: false,
            metrics_from_glyphs: false,
        }
    }

    /// Parse a font dictionary the way PDFium loads it:
    ///
    /// * Composite fonts take their name, descriptor and type from the
    ///   descendant CIDFont, and keep the name as written.
    /// * A Type1 font naming a standard font (or an alias such as
    ///   `Arial,Bold`) is renamed to the canonical base-14 name and uses the
    ///   built-in face's metrics.
    /// * Embedded Type1/TrueType fonts drop a `ABCDEF+` subset prefix.
    /// * Integer-valued descriptor entries are truncated like
    ///   `GetIntegerFor`.
    pub(crate) fn from_dict(dict: &Dict<'_>) -> Self {
        let subtype = dict_name(dict, b"Subtype").unwrap_or_default();
        let mut info = Self::unknown();
        info.base_name = dict_name(dict, b"BaseFont");
        info.has_to_unicode = dict.contains_key(b"ToUnicode");

        let descriptor_owner;
        let mut standard = None;
        match subtype.as_str() {
            "Type0" => {
                let descendant = dict
                    .get::<Array<'_>>(b"DescendantFonts")
                    .and_then(|fonts| fonts.iter::<Dict<'_>>().next());
                info.font_type = match descendant
                    .as_ref()
                    .and_then(|d| dict_name(d, b"Subtype"))
                    .as_deref()
                {
                    Some("CIDFontType0") => FontType::CidType0,
                    Some("CIDFontType2") => FontType::CidType2,
                    _ => FontType::Type0,
                };
                if let Some(name) = descendant.as_ref().and_then(|d| dict_name(d, b"BaseFont")) {
                    info.base_name = Some(name);
                }
                info.encoding = dict_name(dict, b"Encoding").or_else(|| {
                    dict.get::<Stream<'_>>(b"Encoding")
                        .and_then(|s| dict_name(s.dict(), b"CMapName"))
                });
                info.is_vertical = info
                    .encoding
                    .as_deref()
                    .is_some_and(|name| name.ends_with("-V"));
                descriptor_owner = descendant;
            }
            "Type3" => {
                // `CPDF_Type3Font::Load` reads no descriptor: flags stay 0,
                // and the font counts as embedded.
                info.font_type = FontType::Type3;
                info.encoding = simple_encoding_name(dict);
                info.font_bbox = type3_bbox(dict);
                info.flags = 0;
                info.is_embedded = true;
                descriptor_owner = None;
            }
            other => {
                info.font_type = match other {
                    "Type1" | "MMType1" => FontType::Type1,
                    "TrueType" | "OpenType" => FontType::TrueType,
                    _ => FontType::Unknown,
                };
                if info.font_type == FontType::Type1 {
                    standard = info.base_name.as_deref().and_then(StandardFont::lookup);
                    if let Some(std) = standard {
                        info.base_name = Some(std.canonical_name().to_string());
                    }
                }
                info.encoding = simple_encoding_name(dict);
                info.space_width = simple_width(dict, 0x20);
                descriptor_owner = Some(dict.clone());
            }
        }

        let descriptor = descriptor_owner
            .as_ref()
            .and_then(|owner| owner.get::<Dict<'_>>(b"FontDescriptor"));
        if let Some(desc) = &descriptor {
            let int = |key: &[u8]| desc.get::<f32>(key).map(|v| v as i32);
            info.flags = int(b"Flags").unwrap_or(FONT_FLAG_NONSYMBOLIC);
            let italic_angle = int(b"ItalicAngle");
            if italic_angle.is_some_and(|angle| angle < 0) {
                info.flags |= FONT_FLAG_ITALIC;
            }
            let stem_v = int(b"StemV");
            info.stem_v = stem_v.unwrap_or(0);
            info.font_weight = int(b"FontWeight").filter(|w| *w > 0);
            let ascent = int(b"Ascent");
            let descent = int(b"Descent").map(|d| if d > 10 { -d } else { d });
            if italic_angle.is_some()
                && ascent.is_some()
                && desc.contains_key(b"CapHeight")
                && descent.is_some()
                && (stem_v.is_some() || info.font_weight.is_some())
            {
                info.flags |= FONT_FLAG_USE_EXTERN_ATTR;
            }
            info.ascent = ascent.map(|a| a as f32);
            info.descent = descent.map(|d| d as f32);
            if info.font_type != FontType::Type3 {
                info.font_bbox = desc
                    .get::<[f32; 4]>(b"FontBBox")
                    .map(|b| b.map(|v| v.trunc()))
                    .filter(|b| b.iter().any(|v| *v != 0.0));
            }
            info.is_embedded = desc.contains_key(b"FontFile")
                || desc.contains_key(b"FontFile2")
                || desc.contains_key(b"FontFile3");
        }

        if let Some(std) = standard {
            // `CPDF_Type1Font::Load` for base-14 fonts.
            if !descriptor
                .as_ref()
                .is_some_and(|d| d.contains_key(b"Flags"))
            {
                info.flags = if std.is_symbolic() {
                    FONT_FLAG_SYMBOLIC
                } else {
                    FONT_FLAG_NONSYMBOLIC
                };
            }
            let (ascent, descent) = std.face_metrics();
            if info.ascent.unwrap_or(0.0) == 0.0 && info.descent.unwrap_or(0.0) == 0.0 {
                info.ascent = Some(ascent);
                info.descent = Some(descent);
            }
            if info.space_width.is_none() {
                info.space_width = Some(std.space_width());
            }
        }
        if info.is_embedded
            && matches!(info.font_type, FontType::Type1 | FontType::TrueType)
            && let Some(name) = info.base_name.as_mut()
        {
            remove_subset_prefix(name);
        }
        if info.ascent.unwrap_or(0.0) == 0.0
            && info.descent.unwrap_or(0.0) == 0.0
            && let Some(bbox) = info.font_bbox
        {
            info.ascent = Some(bbox[3]);
            info.descent = Some(bbox[1]);
            // Type3 glyph boxes are not measured here.
            info.metrics_from_glyphs = info.font_type != FontType::Type3;
        }
        info
    }

    /// `CPDF_Font::GetFontWeight`: `/FontWeight` when present, else derived
    /// from `/StemV`.
    pub(crate) fn weight(&self) -> i32 {
        if let Some(weight) = self.font_weight {
            return weight;
        }
        if self.stem_v < 140 {
            self.stem_v.saturating_mul(5)
        } else {
            self.stem_v.saturating_mul(4).saturating_add(140)
        }
    }
}

/// `MaybeRemoveSubsettedFontPrefix`: strip `ABCDEF+` (six uppercase
/// letters and a plus).
fn remove_subset_prefix(name: &mut String) {
    let bytes = name.as_bytes();
    if bytes.len() > 6 && bytes[6] == b'+' && bytes[..6].iter().all(u8::is_ascii_uppercase) {
        name.drain(..7);
    }
}

pub(crate) const FONT_FLAG_SYMBOLIC: i32 = 1 << 2;
pub(crate) const FONT_FLAG_NONSYMBOLIC: i32 = 1 << 5;
pub(crate) const FONT_FLAG_ITALIC: i32 = 1 << 6;
/// PDFium's `kFontUseExternAttr`: the descriptor carries a complete set of
/// metrics.
pub(crate) const FONT_FLAG_USE_EXTERN_ATTR: i32 = 0x80000;

fn simple_encoding_name(dict: &Dict<'_>) -> Option<String> {
    if let Some(name) = dict_name(dict, b"Encoding") {
        return Some(name);
    }
    let enc = dict.get::<Dict<'_>>(b"Encoding")?;
    Some(dict_name(&enc, b"BaseEncoding").unwrap_or_else(|| "Custom".to_string()))
}

/// `/Widths` entry for `code` in a simple font.
fn simple_width(dict: &Dict<'_>, code: u32) -> Option<f32> {
    let first = dict.get::<i32>(b"FirstChar")?;
    let widths = dict.get::<Array<'_>>(b"Widths")?;
    let index = code.checked_sub(u32::try_from(first).ok()?)?;
    widths
        .iter::<f32>()
        .nth(index as usize)
        .filter(|w| *w > 0.0)
}

/// A Type3 font's `/FontBBox` in glyph units as `CPDF_Type3Font::Load` sees
/// it: scaled by the `/FontMatrix` diagonal, times 1000, truncated.
fn type3_bbox(dict: &Dict<'_>) -> Option<[f32; 4]> {
    let bbox = dict.get::<[f32; 4]>(b"FontBBox")?;
    let (x_scale, y_scale) = dict
        .get::<[f32; 6]>(b"FontMatrix")
        .map_or((1.0, 1.0), |m| (m[0], m[3]));
    let rect = [
        (bbox[0] * x_scale * 1000.0).trunc(),
        (bbox[1] * y_scale * 1000.0).trunc(),
        (bbox[2] * x_scale * 1000.0).trunc(),
        (bbox[3] * y_scale * 1000.0).trunc(),
    ];
    rect.iter().any(|v| *v != 0.0).then_some(rect)
}

/// The 14 standard fonts, as PDFium's `CFX_StandardFont` knows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StandardFont {
    Courier,
    CourierBold,
    CourierBoldOblique,
    CourierOblique,
    Helvetica,
    HelveticaBold,
    HelveticaBoldOblique,
    HelveticaOblique,
    Times,
    TimesBold,
    TimesBoldOblique,
    TimesOblique,
    Symbol,
    Dingbats,
}

impl StandardFont {
    /// `CFX_StandardFont::GetStandardFontIndex`: a case-insensitive match
    /// against the canonical names and PDFium's alias table.
    fn lookup(name: &str) -> Option<Self> {
        use StandardFont::*;
        const ALIASES: &[(&str, StandardFont)] = &[
            ("Arial", Helvetica),
            ("Arial,Bold", HelveticaBold),
            ("Arial,BoldItalic", HelveticaBoldOblique),
            ("Arial,Italic", HelveticaOblique),
            ("Arial-Bold", HelveticaBold),
            ("Arial-BoldItalic", HelveticaBoldOblique),
            ("Arial-BoldItalicMT", HelveticaBoldOblique),
            ("Arial-BoldMT", HelveticaBold),
            ("Arial-Italic", HelveticaOblique),
            ("Arial-ItalicMT", HelveticaOblique),
            ("ArialBold", HelveticaBold),
            ("ArialBoldItalic", HelveticaBoldOblique),
            ("ArialItalic", HelveticaOblique),
            ("ArialMT", Helvetica),
            ("ArialMT,Bold", HelveticaBold),
            ("ArialMT,BoldItalic", HelveticaBoldOblique),
            ("ArialMT,Italic", HelveticaOblique),
            ("ArialRoundedMTBold", HelveticaBold),
            ("Courier", Courier),
            ("Courier,Bold", CourierBold),
            ("Courier,BoldItalic", CourierBoldOblique),
            ("Courier,Italic", CourierOblique),
            ("Courier-Bold", CourierBold),
            ("Courier-BoldOblique", CourierBoldOblique),
            ("Courier-Oblique", CourierOblique),
            ("CourierBold", CourierBold),
            ("CourierBoldItalic", CourierBoldOblique),
            ("CourierItalic", CourierOblique),
            ("CourierNew", Courier),
            ("CourierNew,Bold", CourierBold),
            ("CourierNew,BoldItalic", CourierBoldOblique),
            ("CourierNew,Italic", CourierOblique),
            ("CourierNew-Bold", CourierBold),
            ("CourierNew-BoldItalic", CourierBoldOblique),
            ("CourierNew-Italic", CourierOblique),
            ("CourierNewBold", CourierBold),
            ("CourierNewBoldItalic", CourierBoldOblique),
            ("CourierNewItalic", CourierOblique),
            ("CourierNewPS-BoldItalicMT", CourierBoldOblique),
            ("CourierNewPS-BoldMT", CourierBold),
            ("CourierNewPS-ItalicMT", CourierOblique),
            ("CourierNewPSMT", Courier),
            ("CourierStd", Courier),
            ("CourierStd-Bold", CourierBold),
            ("CourierStd-BoldOblique", CourierBoldOblique),
            ("CourierStd-Oblique", CourierOblique),
            ("Helvetica", Helvetica),
            ("Helvetica,Bold", HelveticaBold),
            ("Helvetica,BoldItalic", HelveticaBoldOblique),
            ("Helvetica,Italic", HelveticaOblique),
            ("Helvetica-Bold", HelveticaBold),
            ("Helvetica-BoldItalic", HelveticaBoldOblique),
            ("Helvetica-BoldOblique", HelveticaBoldOblique),
            ("Helvetica-Italic", HelveticaOblique),
            ("Helvetica-Oblique", HelveticaOblique),
            ("HelveticaBold", HelveticaBold),
            ("HelveticaBoldItalic", HelveticaBoldOblique),
            ("HelveticaItalic", HelveticaOblique),
            ("Symbol", Symbol),
            ("SymbolMT", Symbol),
            ("Times-Bold", TimesBold),
            ("Times-BoldItalic", TimesBoldOblique),
            ("Times-Italic", TimesOblique),
            ("Times-Roman", Times),
            ("TimesBold", TimesBold),
            ("TimesBoldItalic", TimesBoldOblique),
            ("TimesItalic", TimesOblique),
            ("TimesNewRoman", Times),
            ("TimesNewRoman,Bold", TimesBold),
            ("TimesNewRoman,BoldItalic", TimesBoldOblique),
            ("TimesNewRoman,Italic", TimesOblique),
            ("TimesNewRoman-Bold", TimesBold),
            ("TimesNewRoman-BoldItalic", TimesBoldOblique),
            ("TimesNewRoman-Italic", TimesOblique),
            ("TimesNewRomanBold", TimesBold),
            ("TimesNewRomanBoldItalic", TimesBoldOblique),
            ("TimesNewRomanItalic", TimesOblique),
            ("TimesNewRomanPS", Times),
            ("TimesNewRomanPS-Bold", TimesBold),
            ("TimesNewRomanPS-BoldItalic", TimesBoldOblique),
            ("TimesNewRomanPS-BoldItalicMT", TimesBoldOblique),
            ("TimesNewRomanPS-BoldMT", TimesBold),
            ("TimesNewRomanPS-Italic", TimesOblique),
            ("TimesNewRomanPS-ItalicMT", TimesOblique),
            ("TimesNewRomanPSMT", Times),
            ("TimesNewRomanPSMT,Bold", TimesBold),
            ("TimesNewRomanPSMT,BoldItalic", TimesBoldOblique),
            ("TimesNewRomanPSMT,Italic", TimesOblique),
            ("ZapfDingbats", Dingbats),
        ];
        ALIASES
            .iter()
            .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
            .map(|(_, font)| *font)
    }

    fn canonical_name(self) -> &'static str {
        use StandardFont::*;
        match self {
            Courier => "Courier",
            CourierBold => "Courier-Bold",
            CourierBoldOblique => "Courier-BoldOblique",
            CourierOblique => "Courier-Oblique",
            Helvetica => "Helvetica",
            HelveticaBold => "Helvetica-Bold",
            HelveticaBoldOblique => "Helvetica-BoldOblique",
            HelveticaOblique => "Helvetica-Oblique",
            Times => "Times-Roman",
            TimesBold => "Times-Bold",
            TimesBoldOblique => "Times-BoldItalic",
            TimesOblique => "Times-Italic",
            Symbol => "Symbol",
            Dingbats => "ZapfDingbats",
        }
    }

    fn is_symbolic(self) -> bool {
        matches!(self, StandardFont::Symbol | StandardFont::Dingbats)
    }

    /// Ascender/descender of PDFium's built-in face for the font, as its
    /// `GetTypeAscent`/`GetTypeDescent` report them.
    fn face_metrics(self) -> (f32, f32) {
        use StandardFont::*;
        match self {
            Courier | CourierBold | CourierBoldOblique | CourierOblique => (833.0, -299.0),
            Helvetica | HelveticaBold | HelveticaBoldOblique | HelveticaOblique => (905.0, -211.0),
            Times | TimesBold | TimesBoldOblique | TimesOblique => (891.0, -215.0),
            Symbol => (1010.0, -293.0),
            Dingbats => (826.0, -163.0),
        }
    }

    /// AFM advance of the space glyph.
    fn space_width(self) -> f32 {
        use StandardFont::*;
        match self {
            Courier | CourierBold | CourierBoldOblique | CourierOblique => 600.0,
            Helvetica | HelveticaBold | HelveticaBoldOblique | HelveticaOblique => 278.0,
            Times | TimesBold | TimesBoldOblique | TimesOblique | Symbol => 250.0,
            Dingbats => 278.0,
        }
    }
}

/// Per-glyph data cached for a font, keyed by glyph id.
#[derive(Debug, Clone)]
pub(crate) struct GlyphEntry {
    /// Advance in glyph units (1/1000 em).
    pub advance: Option<f32>,
    /// Outline in glyph units, `None` when the glyph draws nothing.
    pub outline: Option<Rc<BezPath>>,
    pub ink: Option<Rect>,
}

/// A font as seen by the text API.
pub(crate) struct FontData {
    pub key: u128,
    pub info: FontInfo,
    /// How text strings split into codes for this font.
    pub reader: CodeReader,
    /// Per-code widths from the font dictionary.
    pub metrics: CharMetrics,
    /// PDFium's Unicode for codes `ToUnicode` leaves out (composite fonts).
    pub cid_unicode: CidUnicode,
    /// Raw font program bytes (TrueType/OpenType/CFF), when hayro exposes them.
    pub program: RefCell<Option<Option<Vec<u8>>>>,
    pub glyphs: RefCell<HashMap<u32, GlyphEntry>>,
    /// Glyph id of each character code drawn so far.
    pub codes: RefCell<HashMap<u32, u32>>,
    /// The lowest code drawn so far whose Unicode is U+0020.
    pub space_code: RefCell<Option<u32>>,
    /// `GetTypeAscent`/`GetTypeDescent` measured from the glyphs of 'A' and
    /// 'g' (see [`FontInfo::metrics_from_glyphs`]); `None` until probed.
    pub glyph_extent: RefCell<Option<(f32, f32)>>,
    /// Advance of the first glyph seen for each unicode scalar.
    pub unicode_advances: RefCell<HashMap<u32, f32>>,
}

impl FontData {
    fn new(
        key: u128,
        info: FontInfo,
        reader: CodeReader,
        metrics: CharMetrics,
        cid_unicode: CidUnicode,
    ) -> Self {
        Self {
            key,
            info,
            reader,
            metrics,
            cid_unicode,
            program: RefCell::new(None),
            glyphs: RefCell::new(HashMap::default()),
            codes: RefCell::new(HashMap::default()),
            space_code: RefCell::new(None),
            glyph_extent: RefCell::new(None),
            unicode_advances: RefCell::new(HashMap::default()),
        }
    }

    /// The glyph id behind `char_code`: a synthetic code's own, or the one a
    /// glyph run drew for a real code.
    fn glyph_id(&self, char_code: u32) -> Option<u32> {
        char_code
            .checked_sub(SYNTHETIC_CODE_BASE)
            .or_else(|| self.codes.borrow().get(&char_code).copied())
    }
}

/// Handle to a font, cheap to clone. Mirrors `pdfium::Font`.
#[derive(Clone)]
pub struct Font(pub(crate) Rc<FontData>);

impl std::fmt::Debug for Font {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Font")
            .field("base_name", &self.0.info.base_name)
            .field("font_type", &self.0.info.font_type)
            .finish()
    }
}

impl Font {
    /// Font of the text object `obj` (a handle reported by
    /// [`super::TextChar::text_object`]).
    ///
    /// # Safety
    /// `obj` must come from a [`super::TextPage`] that is still alive.
    pub unsafe fn from_text_object(obj: super::pdfium_sys::FPDF_PAGEOBJECT) -> Option<Font> {
        if obj.is_null() {
            return None;
        }
        // SAFETY: per the contract above, `obj` points at a live run record.
        let run = unsafe { &*(obj as *const super::text_page::TextObject) };
        run.font.clone()
    }

    /// A stable per-font identity, usable as a map key.
    pub fn handle(&self) -> *const std::ffi::c_void {
        Rc::as_ptr(&self.0).cast()
    }

    pub fn base_name(&self) -> Option<String> {
        self.0.info.base_name.clone()
    }

    pub fn font_type(&self) -> FontType {
        self.0.info.font_type
    }

    pub fn is_embedded(&self) -> bool {
        self.0.info.is_embedded
    }

    pub fn ascent(&self, font_size: f32) -> Option<f32> {
        self.type_ascent().map(|a| a * font_size / 1000.0)
    }

    pub fn descent(&self, font_size: f32) -> Option<f32> {
        self.type_descent().map(|d| d * font_size / 1000.0)
    }

    /// `GetTypeAscent` in glyph units.
    pub(crate) fn type_ascent(&self) -> Option<f32> {
        match *self.0.glyph_extent.borrow() {
            Some((ascent, _)) => Some(ascent),
            None => self.0.info.ascent,
        }
    }

    /// `GetTypeDescent` in glyph units.
    pub(crate) fn type_descent(&self) -> Option<f32> {
        match *self.0.glyph_extent.borrow() {
            Some((_, descent)) => Some(descent),
            None => self.0.info.descent,
        }
    }

    /// Whether the font's ascent and descent still wait for
    /// [`Font::set_glyph_extent`].
    pub(crate) fn needs_glyph_extent(&self) -> bool {
        self.0.info.metrics_from_glyphs && self.0.glyph_extent.borrow().is_none()
    }

    /// `CheckFontMetrics` from the boxes of the glyphs for codes 'A' and
    /// 'g': their top and bottom, or the font box's where a box is empty.
    pub(crate) fn set_glyph_extent(&self, a: Option<Rect>, g: Option<Rect>) {
        let bbox = self.0.info.font_bbox.unwrap_or_default();
        let ascent = a.filter(|r| r.y0 != r.y1).map_or(bbox[3], |r| r.y1 as f32);
        let descent = g.filter(|r| r.y0 != r.y1).map_or(bbox[1], |r| r.y0 as f32);
        *self.0.glyph_extent.borrow_mut() = Some((ascent, descent));
    }

    /// `FPDFFont_GetGlyphWidthFromCharCode`: the width PDFium assigns
    /// `char_code` in this font, at `font_size`. Answered from the font
    /// dictionary where PDFium reads it from there, else from the glyph a
    /// run drew for the code (PDFium measures the font program). Synthetic
    /// codes (see [`SYNTHETIC_CODE_BASE`]) address a glyph id.
    pub fn glyph_width_from_char_code(&self, char_code: u32, font_size: f32) -> Option<f32> {
        let dict_width = (char_code < SYNTHETIC_CODE_BASE)
            .then(|| self.0.metrics.width(char_code))
            .flatten();
        let units = match dict_width {
            Some(width) => width as f32,
            None => match self.0.glyph_id(char_code) {
                Some(gid) => self.0.glyphs.borrow().get(&gid)?.advance?,
                None if char_code == 0x20 => self.0.info.space_width?,
                None => return None,
            },
        };
        Some(units * font_size / 1000.0)
    }

    /// Outline of the glyph for `char_code` as PDFium-style
    /// `(segment_type, x, y)` triples (LINETO=0, BEZIERTO=1, MOVETO=2; one
    /// triple per point). `None` for inkless glyphs.
    ///
    /// Mirrors `CFX_Font::LoadGlyphPath` behind the fork's
    /// `FPDFFont_GetGlyphPathFromCharCode`: coordinates are in em units at
    /// FreeType's 1/4096-em resolution (independent of `font_size`, which
    /// PDFium only uses to pick the glyph), quadratic curves become cubics,
    /// and every contour ends with an explicit line back to its start.
    pub fn glyph_path_segments(
        &self,
        char_code: u32,
        _font_size: f32,
    ) -> Option<Vec<(i32, f32, f32)>> {
        let gid = self.0.glyph_id(char_code)?;
        let outline = self.0.glyphs.borrow().get(&gid)?.outline.clone()?;
        let em = |p: hayro::kurbo::Point| {
            let q = |v: f64| ((v / 1000.0 * 4096.0).round() / 4096.0) as f32;
            (q(p.x), q(p.y))
        };
        let mut out = Vec::new();
        let mut current = hayro::kurbo::Point::ZERO;
        let mut start = hayro::kurbo::Point::ZERO;
        let push = |out: &mut Vec<(i32, f32, f32)>, kind: i32, p| {
            let (x, y) = em(p);
            out.push((kind, x, y));
        };
        for el in outline.elements() {
            match *el {
                PathEl::MoveTo(p) => {
                    push(&mut out, 2, p);
                    current = p;
                    start = p;
                }
                PathEl::LineTo(p) => {
                    push(&mut out, 0, p);
                    current = p;
                }
                PathEl::QuadTo(c, p) => {
                    let c1 = current + (c - current) * (2.0 / 3.0);
                    let c2 = p + (c - p) * (2.0 / 3.0);
                    for q in [c1, c2, p] {
                        push(&mut out, 1, q);
                    }
                    current = p;
                }
                PathEl::CurveTo(c1, c2, p) => {
                    for q in [c1, c2, p] {
                        push(&mut out, 1, q);
                    }
                    current = p;
                }
                PathEl::ClosePath => {
                    push(&mut out, 0, start);
                    current = start;
                }
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// Advance of the glyph that maps to `unicode`, when this font has drawn
    /// one (0x20 also falls back to the font's declared space width).
    pub fn glyph_width(&self, unicode: u32, font_size: f32) -> Option<f32> {
        let units = self
            .0
            .unicode_advances
            .borrow()
            .get(&unicode)
            .copied()
            .or_else(|| {
                (unicode == 0x20)
                    .then_some(self.0.info.space_width)
                    .flatten()
            })?;
        Some(units * font_size / 1000.0)
    }

    pub fn has_to_unicode(&self) -> bool {
        self.0.info.has_to_unicode
    }

    pub fn encoding(&self) -> Option<String> {
        self.0.info.encoding.clone()
    }

    /// hayro already resolves glyph names to Unicode while decoding text, so
    /// there is no separate glyph-name lookup.
    pub fn char_glyph_name(&self, _char_code: u32) -> Option<String> {
        None
    }

    pub fn char_glyph_index(&self, char_code: u32) -> Option<u32> {
        self.0.glyph_id(char_code)
    }

    /// The embedded font program, when hayro can expose it (not for Type1).
    pub fn font_data(&self) -> Option<Vec<u8>> {
        self.0.program.borrow().clone().flatten()
    }

    pub(crate) fn info(&self) -> &FontInfo {
        &self.0.info
    }

    pub(crate) fn key(&self) -> u128 {
        self.0.key
    }

    pub(crate) fn reader(&self) -> &CodeReader {
        &self.0.reader
    }

    /// PDFium's Unicode for a code of a composite font that hayro could not
    /// map (no `ToUnicode` entry).
    pub(crate) fn cid_unicode(&self, char_code: u32) -> Option<char> {
        self.0
            .cid_unicode
            .unicode(char_code, self.0.metrics.cid(char_code))
    }

    /// Record that this font drew glyph `gid` for `char_code`, which hayro
    /// mapped to `unicode`.
    pub(crate) fn note_code(&self, char_code: u32, gid: u32, unicode: Option<&str>) {
        self.0.codes.borrow_mut().entry(char_code).or_insert(gid);
        if unicode == Some(" ") && char_code != 0 {
            let mut space = self.0.space_code.borrow_mut();
            *space = Some(space.map_or(char_code, |code| code.min(char_code)));
        }
    }

    /// `CharCodeFromUnicode(' ')`, the code PDFium measures word spaces by;
    /// `None` is `kInvalidCharCode`. The `ToUnicode` reverse lookup only
    /// knows the codes drawn so far.
    pub(crate) fn space_char_code(&self) -> Option<u32> {
        if self.0.info.has_to_unicode
            && let Some(code) = *self.0.space_code.borrow()
        {
            return Some(code);
        }
        match self.0.info.font_type {
            FontType::Type0 | FontType::CidType0 | FontType::CidType2 => {
                self.0.cid_unicode.space_code()
            }
            // Type3 encodings carry no Unicode.
            FontType::Type3 => None,
            _ => Some(0x20),
        }
    }

    /// `GetCharWidth(char_code)` in glyph units.
    pub(crate) fn char_width(&self, char_code: u32) -> f32 {
        self.glyph_width_from_char_code(char_code, 1000.0)
            .unwrap_or(0.0)
    }
}

/// Fonts known to a document, keyed by hayro's font cache key.
#[derive(Default)]
pub(crate) struct FontCatalog {
    fonts: RefCell<HashMap<u128, Font>>,
    /// Resource dictionaries already walked, by cache key.
    walked: RefCell<HashSet<u128>>,
}

impl FontCatalog {
    /// Register every font reachable from `resources` (recursing into form
    /// XObjects, tiling patterns, Type3 fonts and soft-mask groups).
    pub(crate) fn register_resources(&self, resources: &Dict<'_>) {
        self.walk_resources(resources, 0);
    }

    /// Register the fonts used by an annotation appearance stream.
    pub(crate) fn register_stream(&self, stream: &Stream<'_>) {
        if let Some(res) = stream.dict().get::<Dict<'_>>(b"Resources") {
            self.walk_resources(&res, 0);
        }
    }

    fn walk_resources(&self, resources: &Dict<'_>, depth: u32) {
        if depth > 12 || !self.walked.borrow_mut().insert(resources.cache_key()) {
            return;
        }
        if let Some(fonts) = resources.get::<Dict<'_>>(b"Font") {
            for name in fonts.keys().collect::<Vec<_>>() {
                if let Some(font) = fonts.get::<Dict<'_>>(name.as_ref()) {
                    self.register_font(&font, depth);
                }
            }
        }
        for key in [&b"XObject"[..], b"Pattern"] {
            if let Some(entries) = resources.get::<Dict<'_>>(key) {
                for name in entries.keys().collect::<Vec<_>>() {
                    if let Some(stream) = entries.get::<Stream<'_>>(name.as_ref())
                        && let Some(res) = stream.dict().get::<Dict<'_>>(b"Resources")
                    {
                        self.walk_resources(&res, depth + 1);
                    }
                }
            }
        }
        if let Some(states) = resources.get::<Dict<'_>>(b"ExtGState") {
            for name in states.keys().collect::<Vec<_>>() {
                let Some(state) = states.get::<Dict<'_>>(name.as_ref()) else {
                    continue;
                };
                // `/Font [font size]` sets the text font like `Tf`.
                if let Some(font) = state
                    .get::<Array<'_>>(b"Font")
                    .and_then(|entry| entry.iter::<Dict<'_>>().next())
                {
                    self.register_font(&font, depth);
                }
                if let Some(mask) = state.get::<Dict<'_>>(b"SMask")
                    && let Some(group) = mask.get::<Stream<'_>>(b"G")
                    && let Some(res) = group.dict().get::<Dict<'_>>(b"Resources")
                {
                    self.walk_resources(&res, depth + 1);
                }
            }
        }
    }

    fn register_font(&self, dict: &Dict<'_>, depth: u32) {
        let key = dict.cache_key();
        if self.fonts.borrow().contains_key(&key) {
            return;
        }
        let info = FontInfo::from_dict(dict);
        let is_type3 = info.font_type == FontType::Type3;
        let reader = CodeReader::from_dict(dict);
        let metrics = CharMetrics::from_dict(dict, &reader);
        let cid_unicode = CidUnicode::from_dict(dict, &reader);
        self.fonts.borrow_mut().insert(
            key,
            Font(Rc::new(FontData::new(
                key,
                info,
                reader,
                metrics,
                cid_unicode,
            ))),
        );
        if is_type3 && let Some(res) = dict.get::<Dict<'_>>(b"Resources") {
            self.walk_resources(&res, depth + 1);
        }
    }

    /// The font with hayro cache key `key`, if the resource walk found it.
    pub(crate) fn known(&self, key: u128) -> Option<Font> {
        self.fonts.borrow().get(&key).cloned()
    }

    /// The font with hayro cache key `key`, creating an anonymous entry for
    /// fonts the resource walk did not reach (e.g. hayro's fallback font).
    pub(crate) fn font(&self, key: u128) -> Font {
        if let Some(font) = self.fonts.borrow().get(&key) {
            return font.clone();
        }
        let font = Font(Rc::new(FontData::new(
            key,
            FontInfo::unknown(),
            CodeReader::Fallback,
            CharMetrics::Unknown,
            CidUnicode::default(),
        )));
        self.fonts.borrow_mut().insert(key, font.clone());
        font
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weight_prefers_font_weight_then_stem_v() {
        let mut info = FontInfo::unknown();
        info.stem_v = 80;
        assert_eq!(info.weight(), 400);
        info.stem_v = 152;
        assert_eq!(info.weight(), 748);
        info.font_weight = Some(700);
        assert_eq!(info.weight(), 700);
    }

    #[test]
    fn standard_font_aliases_are_case_insensitive() {
        assert_eq!(
            StandardFont::lookup("arial,bold").map(StandardFont::canonical_name),
            Some("Helvetica-Bold")
        );
        assert!(StandardFont::lookup("ABCDEF+Helvetica").is_none());
        assert!(StandardFont::lookup("CMR10").is_none());
    }

    #[test]
    fn subset_prefix_needs_six_uppercase_letters() {
        let mut name = "ACEGLD+CMR6".to_string();
        remove_subset_prefix(&mut name);
        assert_eq!(name, "CMR6");
        let mut name = "AbcDEF+Font".to_string();
        remove_subset_prefix(&mut name);
        assert_eq!(name, "AbcDEF+Font");
    }
}
