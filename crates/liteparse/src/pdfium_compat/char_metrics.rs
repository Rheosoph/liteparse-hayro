//! Character codes and the per-code widths PDFium derives from a font
//! dictionary (`CPDF_Font::GetCharWidth`).
//!
//! Text items look up widths by character code, sometimes in a different
//! font than the one that drew the character (a segment keeps its first
//! font), so widths must be answerable for codes no glyph run produced.

use std::rc::Rc;
use std::sync::OnceLock;

use foldhash::HashMap;
use hayro::hayro_interpret::hayro_cmap::{self, BfString, CMap, CMapName, CidFamily};
use hayro::hayro_syntax::content::TypedIter;
use hayro::hayro_syntax::content::ops::TypedInstruction;
use hayro::hayro_syntax::object::{Array, Dict, Object, Stream};
use hayro::kurbo::{Affine, Rect};

use super::objects::{dict_name, object_f32};

/// How the bytes of a text string split into character codes, as hayro
/// reads them.
#[derive(Debug, Clone)]
pub(crate) enum CodeReader {
    /// One byte per code (simple fonts).
    OneByte,
    /// A composite font's `/Encoding` CMap.
    Cmap(Rc<CMap>),
    /// hayro cannot load the font and draws ASCII strings in Helvetica.
    Fallback,
}

impl CodeReader {
    pub(crate) fn from_dict(dict: &Dict<'_>) -> Self {
        match dict_name(dict, b"Subtype").as_deref() {
            Some("Type0") => {
                let has_descendant = dict
                    .get::<Array<'_>>(b"DescendantFonts")
                    .is_some_and(|fonts| fonts.iter::<Dict<'_>>().next().is_some());
                match encoding_cmap(dict).filter(|_| has_descendant) {
                    Some(cmap) => Self::Cmap(Rc::new(cmap)),
                    None => Self::Fallback,
                }
            }
            Some("Type1" | "MMType1" | "TrueType" | "OpenType" | "Type3") => Self::OneByte,
            _ => Self::Fallback,
        }
    }

    /// Append the codes of `bytes` to `out` (hayro's `Font::read_code`).
    pub(crate) fn read(&self, bytes: &[u8], out: &mut Vec<u32>) {
        match self {
            Self::OneByte | Self::Fallback => out.extend(bytes.iter().map(|b| *b as u32)),
            Self::Cmap(cmap) => {
                let mut offset = 0;
                while offset < bytes.len() {
                    let mut code = 0_u32;
                    let mut read = (0, 1);
                    for n in 0..4.min(bytes.len() - offset) {
                        code = (code << 8) | bytes[offset + n] as u32;
                        if cmap.lookup_cid_code(code, (n + 1) as u8).is_some() {
                            read = (code, n + 1);
                            break;
                        }
                    }
                    out.push(read.0);
                    offset += read.1;
                }
            }
        }
    }
}

/// hayro's `read_encoding`.
fn encoding_cmap(dict: &Dict<'_>) -> Option<CMap> {
    match dict.get::<Object<'_>>(b"Encoding")? {
        Object::Name(name) => match CMapName::from_bytes(name.as_ref()) {
            CMapName::IdentityH => Some(CMap::identity_h()),
            CMapName::IdentityV => Some(CMap::identity_v()),
            other => CMap::parse(hayro_cmap::load_embedded(other)?, hayro_cmap::load_embedded),
        },
        Object::Stream(stream) => CMap::parse(&stream.decoded().ok()?, hayro_cmap::load_embedded),
        _ => None,
    }
}

/// What `GetCharWidth` needs from a font dictionary.
#[derive(Debug, Default)]
pub(crate) enum CharMetrics {
    /// Simple fonts: `char_width_`, filled from `/Widths` and
    /// `/MissingWidth` when the font has `/Widths` (`UNSET` elsewhere).
    /// Without `/Widths` PDFium measures the font program instead.
    Simple(Option<Box<[u16; 256]>>),
    /// Type3 fonts: `/Widths` through `/FontMatrix`, in glyph units, as
    /// PDFium rounds them and as hayro positions with them.
    Type3(Box<Type3Widths>),
    Cid(Box<CidMetrics>),
    #[default]
    Unknown,
}

/// `char_width_` entries PDFium has not loaded.
const UNSET: u16 = 0xFFFF;

#[derive(Debug)]
pub(crate) struct Type3Widths {
    pdfium: [i32; 256],
    hayro: [f32; 256],
    /// `CPDF_Type3Char` per code: the width and, from a `d1` with a proper
    /// box, the glyph box, both in glyph units (see [`type3_procs`]).
    procs: HashMap<u32, (i32, Option<Rect>)>,
}

#[derive(Debug)]
pub(crate) struct CidMetrics {
    /// Code → CID; `None` for the identity CMaps.
    cmap: Option<Rc<CMap>>,
    /// `/W`, see [`WidthList`].
    widths: WidthList,
    default_width: i32,
    vertical: bool,
    /// `/W2` (`w1y` values).
    vertical_widths: WidthList,
    default_w1: i32,
}

/// A `/W`-style list: PDFium takes the first entry covering a CID. Runs of
/// single CIDs (the `c [w1 w2 …]` form) go into a map, keeping the first
/// value per CID; ranges stay in order and are only consulted for CIDs the
/// map lacks, which matches unless a range precedes an overlapping single.
#[derive(Debug, Default)]
struct WidthList {
    singles: HashMap<i32, i32>,
    ranges: Vec<(i32, i32, i32)>,
}

impl WidthList {
    fn new(entries: Vec<(i32, i32, i32)>) -> Self {
        let mut list = Self::default();
        for (first, last, width) in entries {
            if first == last {
                list.singles.entry(first).or_insert(width);
            } else {
                list.ranges.push((first, last, width));
            }
        }
        list
    }

    fn get(&self, cid: i32) -> Option<i32> {
        self.singles.get(&cid).copied().or_else(|| {
            self.ranges
                .iter()
                .find(|(low, high, _)| *low <= cid && cid <= *high)
                .map(|entry| entry.2)
        })
    }
}

impl CharMetrics {
    pub(crate) fn from_dict(dict: &Dict<'_>, reader: &CodeReader) -> Self {
        match dict_name(dict, b"Subtype").as_deref() {
            Some("Type0") => {
                let Some(descendant) = dict
                    .get::<Array<'_>>(b"DescendantFonts")
                    .and_then(|fonts| fonts.iter::<Dict<'_>>().next())
                else {
                    return Self::Unknown;
                };
                let cmap = match reader {
                    CodeReader::Cmap(cmap) => Some(cmap.clone()),
                    _ => None,
                };
                let identity = matches!(
                    dict_name(dict, b"Encoding").as_deref(),
                    Some("Identity-H" | "Identity-V")
                );
                let vertical = cmap.as_ref().is_some_and(|c| {
                    c.metadata().writing_mode == Some(hayro_cmap::WritingMode::Vertical)
                });
                let dw2 = descendant.get::<Array<'_>>(b"DW2");
                let default_w1 = dw2
                    .as_ref()
                    .and_then(|a| a.iter::<Object<'_>>().nth(1))
                    .and_then(|o| object_f32(&o))
                    .map_or(-1000, |v| v as i32);
                Self::Cid(Box::new(CidMetrics {
                    cmap: if identity { None } else { cmap },
                    widths: WidthList::new(
                        descendant
                            .get::<Array<'_>>(b"W")
                            .map(|w| metrics_array(&w, 1))
                            .unwrap_or_default(),
                    ),
                    default_width: descendant
                        .get::<Object<'_>>(b"DW")
                        .and_then(|o| object_f32(&o))
                        .map_or(1000, |v| v as i32),
                    vertical,
                    vertical_widths: WidthList::new(if vertical {
                        descendant
                            .get::<Array<'_>>(b"W2")
                            .map(|w| metrics_array(&w, 3))
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    }),
                    default_w1,
                }))
            }
            Some("Type3") => Self::Type3(Box::new(type3_widths(dict))),
            Some("Type1" | "MMType1" | "TrueType" | "OpenType") => {
                Self::Simple(simple_widths(dict))
            }
            _ => Self::Unknown,
        }
    }

    /// `FPDFFont_GetGlyphWidthFromCharCode` in glyph units, when the
    /// dictionary answers it. `None` means PDFium would measure the font
    /// program (simple fonts without `/Widths`, Type3 glyphs without one).
    pub(crate) fn width(&self, code: u32) -> Option<i32> {
        match self {
            Self::Simple(Some(widths)) => {
                let code = if code > 0xFF { 0 } else { code as usize };
                Some(match widths[code] {
                    UNSET => 0,
                    width => width as i32,
                })
            }
            Self::Simple(None) | Self::Unknown => None,
            Self::Type3(widths) => {
                let code = if code > 0xFF { 0 } else { code as usize };
                // `char_width_l_`, else the glyph's own `d0`/`d1` width.
                Some(widths.pdfium[code])
                    .filter(|w| *w != 0)
                    .or_else(|| widths.procs.get(&(code as u32)).map(|p| p.0))
                    .filter(|w| *w != 0)
            }
            Self::Cid(cid) => Some(cid.width(code)),
        }
    }

    /// The advance hayro positions a Type3 glyph with (`Type3::glyph_width`).
    pub(crate) fn type3_advance(&self, code: u32) -> Option<f32> {
        match self {
            Self::Type3(widths) => widths.hayro.get(code as usize).copied(),
            _ => None,
        }
    }

    /// For a Type3 glyph: whether its char proc names a box (`d1`), and
    /// that box in glyph units. `None` for unknown codes.
    pub(crate) fn type3_box(&self, code: u32) -> Option<Option<Rect>> {
        match self {
            Self::Type3(widths) => widths.procs.get(&code).map(|p| p.1),
            _ => None,
        }
    }
}

impl CharMetrics {
    /// `CIDFromCharCode` for composite fonts.
    pub(crate) fn cid(&self, code: u32) -> Option<i32> {
        match self {
            Self::Cid(cid) => Some(cid.cid(code)),
            _ => None,
        }
    }
}

/// The CID character sets PDFium carries Unicode tables for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Charset {
    Gb1,
    Cns1,
    Japan1,
    Korea1,
    /// `UCS`: CIDs are Unicode values.
    Ucs,
}

impl Charset {
    /// `CPDF_CMapParser::CharsetFromOrdering`.
    fn from_ordering(ordering: &[u8]) -> Option<Self> {
        Some(match ordering {
            b"GB1" => Self::Gb1,
            b"CNS1" => Self::Cns1,
            b"Japan1" => Self::Japan1,
            b"Korea1" => Self::Korea1,
            b"UCS" => Self::Ucs,
            _ => return None,
        })
    }

    fn from_family(family: &CidFamily) -> Option<Self> {
        match family {
            CidFamily::AdobeGB1 => Some(Self::Gb1),
            CidFamily::AdobeCNS1 => Some(Self::Cns1),
            CidFamily::AdobeJapan1 => Some(Self::Japan1),
            CidFamily::AdobeKorea1 => Some(Self::Korea1),
            CidFamily::Custom { ordering, .. } => Self::from_ordering(ordering),
            CidFamily::AdobeIdentity => None,
        }
    }

    /// The first CID the table maps to U+0020.
    fn space_cid(self) -> Option<u32> {
        static SPACES: [OnceLock<Option<u32>>; 5] = [const { OnceLock::new() }; 5];
        *SPACES[self as usize]
            .get_or_init(|| (0..0x10000).find(|&cid| self.unicode(cid) == Some(' ')))
    }

    /// `CPDF_CID2UnicodeMap::UnicodeFromCID`, from hayro's copies of Adobe's
    /// `*-UCS2` CMaps.
    fn unicode(self, cid: u32) -> Option<char> {
        static TABLES: [OnceLock<Option<CMap>>; 4] = [const { OnceLock::new() }; 4];
        let (slot, name) = match self {
            Self::Ucs => return char::from_u32(cid & 0xFFFF).filter(|c| *c != '\0'),
            Self::Gb1 => (0, CMapName::AdobeGb1Ucs2),
            Self::Cns1 => (1, CMapName::AdobeCns1Ucs2),
            Self::Japan1 => (2, CMapName::AdobeJapan1Ucs2),
            Self::Korea1 => (3, CMapName::AdobeKorea1Ucs2),
        };
        let table = TABLES[slot]
            .get_or_init(|| {
                CMap::parse(hayro_cmap::load_embedded(name)?, hayro_cmap::load_embedded)
            })
            .as_ref()?;
        match table.lookup_bf_string(cid & 0xFFFF)? {
            BfString::Char(c) => Some(c),
            BfString::String(s) => s.chars().next(),
        }
        .filter(|c| *c != '\0')
    }
}

/// How a composite font's CMap codes characters (`CIDCoding`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Coding {
    /// Not a composite font.
    #[default]
    None,
    /// An embedded CMap stream, or a name PDFium does not know.
    Unknown,
    /// `Identity-H`/`Identity-V`: codes are CIDs.
    Cid,
    /// UCS-2 and UTF-16 CMaps: codes are Unicode values.
    Unicode,
    /// Other predefined CMaps.
    MultiByte,
}

/// What `CPDF_CIDFont` needs to read codes as Unicode when `ToUnicode` does
/// not cover them (`GetUnicodeFromCharCode`, `CharCodeFromUnicode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CidUnicode {
    coding: Coding,
    /// The character set PDFium has a CID → Unicode table for.
    charset: Option<Charset>,
}

impl CidUnicode {
    pub(crate) fn from_dict(dict: &Dict<'_>, reader: &CodeReader) -> Self {
        if dict_name(dict, b"Subtype").as_deref() != Some("Type0") {
            return Self::default();
        }
        let (coding, mut charset) = match dict.get::<Object<'_>>(b"Encoding") {
            Some(Object::Name(name)) => match name.as_ref() {
                b"Identity-H" | b"Identity-V" => (Coding::Cid, None),
                name => match predefined_cmap(name) {
                    Some((charset, true)) => (Coding::Unicode, Some(charset)),
                    Some((charset, false)) => (Coding::MultiByte, Some(charset)),
                    None => (Coding::Unknown, None),
                },
            },
            Some(Object::Stream(_)) => {
                let charset = match reader {
                    CodeReader::Cmap(cmap) => cmap
                        .metadata()
                        .character_collection
                        .as_ref()
                        .and_then(|cc| Charset::from_family(&cc.family)),
                    _ => None,
                };
                (Coding::Unknown, charset)
            }
            _ => (Coding::Unknown, None),
        };
        if charset.is_none() {
            charset = dict
                .get::<Array<'_>>(b"DescendantFonts")
                .and_then(|fonts| fonts.iter::<Dict<'_>>().next())
                .and_then(|font| font.get::<Dict<'_>>(b"CIDSystemInfo"))
                .and_then(|info| info.get::<hayro::hayro_syntax::object::String<'_>>(b"Ordering"))
                .and_then(|ordering| Charset::from_ordering(ordering.as_bytes()));
        }
        Self { coding, charset }
    }

    /// `GetUnicodeFromCharCode` for `code`, whose CID is `cid`.
    pub(crate) fn unicode(self, code: u32, cid: Option<i32>) -> Option<char> {
        match self.coding {
            Coding::None => None,
            Coding::Unicode => char::from_u32(code & 0xFFFF).filter(|c| *c != '\0'),
            Coding::Cid => self.charset?.unicode(code),
            Coding::Unknown | Coding::MultiByte => self.charset?.unicode(cid? as u32),
        }
    }

    /// `CPDF_CIDFont::CharCodeFromUnicode(' ')` past the `ToUnicode` lookup;
    /// `None` for simple fonts.
    pub(crate) fn space_code(self) -> Option<u32> {
        Some(match self.coding {
            Coding::None => return None,
            Coding::Unknown => 0,
            Coding::Unicode | Coding::MultiByte => 0x20,
            Coding::Cid => match self.charset {
                None => 0,
                Some(charset) => charset.space_cid().unwrap_or(0x20),
            },
        })
    }
}

/// PDFium's `kPredefinedCMaps`: the charset of a predefined CMap and
/// whether its codes are Unicode (UCS-2/UTF-16), matched by the name
/// without its `-H`/`-V` suffix.
fn predefined_cmap(name: &[u8]) -> Option<(Charset, bool)> {
    let base = if name.len() > 2 {
        &name[..name.len() - 2]
    } else {
        name
    };
    Some(match base {
        b"GB-EUC" | b"GBpc-EUC" | b"GBK-EUC" | b"GBKp-EUC" | b"GBK2K-EUC" | b"GBK2K" => {
            (Charset::Gb1, false)
        }
        b"UniGB-UCS2" | b"UniGB-UTF16" => (Charset::Gb1, true),
        b"B5pc" | b"HKscs-B5" | b"ETen-B5" | b"ETenms-B5" => (Charset::Cns1, false),
        b"UniCNS-UCS2" | b"UniCNS-UTF16" => (Charset::Cns1, true),
        b"83pv-RKSJ" | b"90ms-RKSJ" | b"90msp-RKSJ" | b"90pv-RKSJ" | b"Add-RKSJ" | b"EUC"
        | b"H" | b"V" | b"Ext-RKSJ" => (Charset::Japan1, false),
        b"UniJIS-UCS2" | b"UniJIS-UCS2-HW" | b"UniJIS-UTF16" => (Charset::Japan1, true),
        b"KSC-EUC" | b"KSCms-UHC" | b"KSCms-UHC-HW" | b"KSCpc-EUC" => (Charset::Korea1, false),
        b"UniKS-UCS2" | b"UniKS-UTF16" => (Charset::Korea1, true),
        _ => return None,
    })
}

impl CidMetrics {
    fn cid(&self, code: u32) -> i32 {
        let Some(cmap) = &self.cmap else {
            return (code & 0xFFFF) as i32;
        };
        (1..=4u8)
            .filter(|len| *len == 4 || code < 1 << (8 * *len as u32))
            .find_map(|len| cmap.lookup_cid_code(code, len))
            .map_or(0, |cid| (cid & 0xFFFF) as i32)
    }

    fn width(&self, code: u32) -> i32 {
        let cid = self.cid(code);
        if self.vertical {
            self.vertical_widths.get(cid).unwrap_or(self.default_w1)
        } else {
            self.widths.get(cid).unwrap_or(self.default_width)
        }
    }
}

/// `LoadMetricsArray`: `c [w1 w2 …]` and `c_first c_last w` runs of
/// `elements` values each, keeping the first value of each.
fn metrics_array(array: &Array<'_>, elements: usize) -> Vec<(i32, i32, i32)> {
    let int = |o: &Object<'_>| object_f32(o).map_or(0, |v| v as i32);
    let mut out = Vec::new();
    let (mut status, mut current, mut first, mut last) = (0, 0, 0, 0);
    for object in array.iter::<Object<'_>>() {
        if let Object::Array(values) = &object {
            if status != 1 {
                return out;
            }
            let values: Vec<i32> = values.iter::<Object<'_>>().map(|o| int(&o)).collect();
            for chunk in values.chunks(elements) {
                out.push((first, first, chunk[0]));
                first += 1;
            }
            status = 0;
            continue;
        }
        let value = int(&object);
        match status {
            0 => {
                first = value;
                status = 1;
            }
            1 => {
                last = value;
                status = 2;
                current = 0;
            }
            _ => {
                if current == 0 {
                    out.push((first, last, value));
                }
                current += 1;
                if current == elements {
                    status = 0;
                }
            }
        }
    }
    out
}

/// `CPDF_FaceBasedSimpleFont::LoadCharWidths`.
fn simple_widths(dict: &Dict<'_>) -> Option<Box<[u16; 256]>> {
    let widths = dict.get::<Array<'_>>(b"Widths")?;
    let widths: Vec<i32> = widths
        .iter::<Object<'_>>()
        .map(|o| object_f32(&o).map_or(0, |v| v as i32))
        .collect();
    let mut table = Box::new([UNSET; 256]);
    if let Some(missing) = dict
        .get::<Dict<'_>>(b"FontDescriptor")
        .and_then(|d| d.get::<Object<'_>>(b"MissingWidth"))
    {
        table.fill(object_f32(&missing).map_or(0, |v| v as i32) as u16);
    }
    let int = |key: &[u8]| {
        dict.get::<Object<'_>>(key)
            .and_then(|o| object_f32(&o))
            .map_or(0, |v| v as i64)
    };
    let start = int(b"FirstChar");
    if !(0..=255).contains(&start) {
        return Some(table);
    }
    let len = widths.len() as i64;
    let mut end = int(b"LastChar");
    if end == 0 || end >= start + len {
        end = start + len - 1;
    }
    for code in start..=end.min(255) {
        let width = widths.get((code - start) as usize).copied().unwrap_or(0);
        table[code as usize] = width as u16;
    }
    Some(table)
}

/// `CPDF_Type3Font::Load` (`round(width × FontMatrix.a × 1000)`, no
/// `/LastChar` needed) next to hayro's `read_widths` (`/FirstChar`,
/// `/LastChar` and `/Widths` all required, `/MissingWidth` elsewhere).
fn type3_widths(dict: &Dict<'_>) -> Type3Widths {
    let mut table = Type3Widths {
        pdfium: [0; 256],
        hayro: [0.0; 256],
        procs: type3_procs(dict),
    };
    let scale = dict.get::<[f32; 6]>(b"FontMatrix").map_or(0.001, |m| m[0]);
    let widths: Vec<f32> = dict
        .get::<Array<'_>>(b"Widths")
        .map(|w| {
            w.iter::<Object<'_>>()
                .map(|o| object_f32(&o).unwrap_or(0.0))
                .collect()
        })
        .unwrap_or_default();
    let start = dict
        .get::<Object<'_>>(b"FirstChar")
        .and_then(|o| object_f32(&o))
        .map_or(0, |v| v as i64);
    if (0..256).contains(&start) {
        for (i, width) in widths.iter().enumerate().take(256 - start as usize) {
            table.pdfium[start as usize + i] = (width * scale * 1000.0).round() as i32;
        }
    }
    let missing = dict.get::<f32>(b"MissingWidth").unwrap_or(0.0);
    let first = dict.get::<usize>(b"FirstChar");
    let last = dict.get::<usize>(b"LastChar");
    let hayro_width = |w: f32| (w * scale) * 1000.0;
    table.hayro = [hayro_width(missing); 256];
    if let (Some(first), Some(last)) = (first, last)
        && last >= first
    {
        for (i, width) in widths.iter().take(last - first + 1).enumerate() {
            if let Some(slot) = table.hayro.get_mut(first + i) {
                *slot = hayro_width(*width);
            }
        }
    }
    table
}

/// `CPDF_Type3Font::LoadChar` and `CPDF_Type3Char` for every code the
/// `/Encoding` differences name a char proc for: the `d0`/`d1` width and,
/// for `d1` with a non-empty box, that box, both through `/FontMatrix` into
/// glyph units and rounded. A glyph without such a box is measured from its
/// drawing instead (`CalcBoundingBox`, see `content`).
fn type3_procs(dict: &Dict<'_>) -> HashMap<u32, (i32, Option<Rect>)> {
    let mut procs = HashMap::default();
    let Some(char_procs) = dict.get::<Dict<'_>>(b"CharProcs") else {
        return procs;
    };
    let matrix = dict
        .get::<[f64; 6]>(b"FontMatrix")
        .map_or(Affine::scale(0.001), Affine::new);
    let x_unit = {
        let [a, b, ..] = matrix.as_coeffs();
        if b == 0.0 {
            a.abs()
        } else if a == 0.0 {
            b.abs()
        } else {
            a.hypot(b)
        }
    } as f32;
    let names = dict
        .get::<Dict<'_>>(b"Encoding")
        .and_then(|encoding| encoding.get::<Array<'_>>(b"Differences"))
        .map(|differences| differences_names(&differences))
        .unwrap_or_default();
    for (code, name) in names {
        let Some(stream) = char_procs.get::<Stream<'_>>(name.as_bytes()) else {
            continue;
        };
        let Ok(data) = stream.decoded() else {
            continue;
        };
        let mut ops = TypedIter::new(&data);
        let mut entry = None;
        while let Some(op) = ops.next() {
            let glyph_unit = |v: f32| (v * 1000.0).round();
            match op {
                TypedInstruction::ColorGlyph(d0) => {
                    entry = Some((glyph_unit(d0.0.as_f32()), None));
                    break;
                }
                TypedInstruction::ShapeGlyph(d1) => {
                    let [llx, lly, urx, ury] =
                        [d1.2.as_f32(), d1.3.as_f32(), d1.4.as_f32(), d1.5.as_f32()]
                            .map(glyph_unit);
                    let bbox = (urx > llx && ury > lly).then(|| {
                        let rect = Rect::new(llx as f64, lly as f64, urx as f64, ury as f64);
                        let mapped = matrix.transform_rect_bbox(rect);
                        Rect::new(
                            mapped.x0.round(),
                            mapped.y0.round(),
                            mapped.x1.round(),
                            mapped.y1.round(),
                        )
                    });
                    entry = Some((glyph_unit(d1.0.as_f32()), bbox));
                    break;
                }
                _ => {}
            }
        }
        if let Some((width, bbox)) = entry {
            procs.insert(code, ((width * x_unit + 0.5) as i32, bbox));
        }
    }
    procs
}

/// The glyph names a `/Differences` array assigns, by code.
fn differences_names(differences: &Array<'_>) -> Vec<(u32, String)> {
    let mut names = Vec::new();
    let mut code = 0_i64;
    for object in differences.iter::<Object<'_>>() {
        match object {
            Object::Number(n) => code = n.as_f64() as i64,
            Object::Name(name) => {
                if (0..256).contains(&code) {
                    names.push((
                        code as u32,
                        String::from_utf8_lossy(name.as_ref()).into_owned(),
                    ));
                }
                code += 1;
            }
            _ => {}
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_array_reads_both_forms() {
        use hayro::hayro_syntax::object::FromBytes;
        let array = Array::from_bytes(b"[1 [500 600] 10 12 700 [9]]").unwrap();
        // The trailing `[9]` follows a complete run, so parsing stops there.
        assert_eq!(
            metrics_array(&array, 1),
            vec![(1, 1, 500), (2, 2, 600), (10, 12, 700)]
        );
    }

    #[test]
    fn simple_widths_follow_first_char_and_missing_width() {
        use hayro::hayro_syntax::object::FromBytes;
        let dict = Dict::from_bytes(
            b"<< /Subtype /Type1 /FirstChar 65 /LastChar 66 /Widths [600 700] /FontDescriptor << /MissingWidth 250 >> >>",
        )
        .unwrap();
        let metrics = CharMetrics::from_dict(&dict, &CodeReader::OneByte);
        assert_eq!(metrics.width(65), Some(600));
        assert_eq!(metrics.width(66), Some(700));
        assert_eq!(metrics.width(67), Some(250));
        // Codes beyond one byte read as code 0.
        assert_eq!(metrics.width(0x141), Some(250));
    }

    #[test]
    fn identity_cid_widths_fall_back_to_dw() {
        use hayro::hayro_syntax::object::FromBytes;
        let dict = Dict::from_bytes(
            b"<< /Subtype /Type0 /Encoding /Identity-H /DescendantFonts [<< /Subtype /CIDFontType2 /W [3 [250]] /DW 900 >>] >>",
        )
        .unwrap();
        let reader = CodeReader::from_dict(&dict);
        assert!(matches!(reader, CodeReader::Cmap(_)));
        let metrics = CharMetrics::from_dict(&dict, &reader);
        assert_eq!(metrics.width(3), Some(250));
        assert_eq!(metrics.width(0x41), Some(900));
        let mut codes = Vec::new();
        reader.read(&[0, 3, 0, 0x41], &mut codes);
        assert_eq!(codes, vec![3, 0x41]);
    }
}
