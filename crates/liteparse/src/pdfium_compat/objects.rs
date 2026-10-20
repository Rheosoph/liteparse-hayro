//! Helpers for reading PDF objects through hayro-syntax.

use hayro::hayro_syntax::Pdf;
use hayro::hayro_syntax::object::{Dict, Name, Object};
use hayro::hayro_syntax::reader::{Reader, ReaderExt};

/// Decode a PDF text string (PDF 32000 §7.9.2.2): UTF-16BE with a BOM,
/// UTF-8 with a BOM (PDF 2.0), otherwise PDFDocEncoding.
pub(crate) fn decode_text_string(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        return strip_language_escapes(String::from_utf16_lossy(&units));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        // Not allowed by the spec, but written by some producers.
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    bytes.iter().map(|&b| pdf_doc_char(b)).collect()
}

/// UTF-16 text strings may embed `ESC lang ESC` language markers; drop them.
fn strip_language_escapes(s: String) -> String {
    if !s.contains('\u{1B}') {
        return s;
    }
    let mut out = String::with_capacity(s.len());
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\u{1B}' {
            in_escape = !in_escape;
        } else if !in_escape {
            out.push(c);
        }
    }
    out
}

/// PDFDocEncoding → Unicode. Latin-1 except for the 0x18–0x1F and 0x80–0xA0
/// ranges (PDF 32000 Annex D.2).
fn pdf_doc_char(b: u8) -> char {
    const HIGH: [u16; 33] = [
        0x2022, 0x2020, 0x2021, 0x2026, 0x2014, 0x2013, 0x0192, 0x2044, 0x2039, 0x203A, 0x2212,
        0x2030, 0x201E, 0x201C, 0x201D, 0x2018, 0x2019, 0x201A, 0x2122, 0xFB01, 0xFB02, 0x0141,
        0x0152, 0x0160, 0x0178, 0x017D, 0x0131, 0x0142, 0x0153, 0x0161, 0x017E, 0xFFFD, 0x20AC,
    ];
    const LOW: [u16; 8] = [
        0x02D8, 0x02C7, 0x02C6, 0x02D9, 0x02DD, 0x02DB, 0x02DA, 0x02DC,
    ];
    let code = match b {
        0x18..=0x1F => LOW[(b - 0x18) as usize],
        0x80..=0xA0 => HIGH[(b - 0x80) as usize],
        _ => b as u16,
    };
    char::from_u32(code as u32).unwrap_or('\u{FFFD}')
}

/// A text-string entry of `dict`, decoded. `None` when absent or not a string.
pub(crate) fn dict_text(dict: &Dict<'_>, key: &[u8]) -> Option<String> {
    dict.get::<hayro::hayro_syntax::object::String<'_>>(key)
        .map(|s| decode_text_string(s.as_bytes()))
}

/// A name entry of `dict` as an owned string.
pub(crate) fn dict_name(dict: &Dict<'_>, key: &[u8]) -> Option<String> {
    dict.get::<Name<'_>>(key).map(|n| n.as_str().to_string())
}

/// A text string or name entry, whichever the producer wrote.
pub(crate) fn dict_text_or_name(dict: &Dict<'_>, key: &[u8]) -> Option<String> {
    dict_text(dict, key).or_else(|| dict_name(dict, key))
}

/// The trailer dictionary of the file's final cross-reference section.
///
/// hayro-syntax resolves the trailer internally but does not expose it, so
/// this finds it again: the dict after the last `trailer` keyword for classic
/// tables, otherwise the dict of the cross-reference stream `startxref` points
/// at. Values must be read with `get_ref` + [`Pdf::xref`] lookups — the dict is
/// parsed without an xref context, so direct `get` of a reference fails.
pub(crate) fn trailer_dict(pdf: &Pdf) -> Option<Dict<'_>> {
    let data: &[u8] = pdf.data().as_ref();
    if let Some(pos) = memchr::memmem::rfind(data, b"trailer") {
        let mut reader = Reader::new(&data[pos + b"trailer".len()..]);
        reader.skip_white_spaces_and_comments();
        if let Some(dict) = reader.read_without_context::<Dict<'_>>()
            && dict.contains_key(b"Root")
        {
            return Some(dict);
        }
    }
    let pos = memchr::memmem::rfind(data, b"startxref")?;
    let tail = std::str::from_utf8(&data[pos + b"startxref".len()..])
        .ok()
        .or_else(|| {
            let end = (pos + 64).min(data.len());
            std::str::from_utf8(&data[pos + b"startxref".len()..end]).ok()
        })?;
    let offset: usize = tail.split_whitespace().next()?.parse().ok()?;
    let object = data.get(offset..)?;
    // `N G obj << ... >> stream`
    let dict_start = memchr::memmem::find(&object[..object.len().min(64)], b"<<")?;
    let mut reader = Reader::new(&object[dict_start..]);
    reader.read_without_context::<Dict<'_>>()
}

/// Resolve a reference stored in the (context-free) trailer dictionary.
pub(crate) fn trailer_ref_dict<'a>(pdf: &'a Pdf, key: &[u8]) -> Option<Dict<'a>> {
    let trailer = trailer_dict(pdf)?;
    if let Some(r) = trailer.get_ref(key) {
        return pdf.xref().get::<Dict<'_>>(r.into());
    }
    None
}

/// The document catalog.
pub(crate) fn catalog(pdf: &Pdf) -> Option<Dict<'_>> {
    pdf.xref().get::<Dict<'_>>(pdf.xref().root_id())
}

/// Numeric value of `object`, when it is a number.
pub(crate) fn object_f32(object: &Object<'_>) -> Option<f32> {
    match object {
        Object::Number(n) => Some(n.as_f32()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_utf16be_with_bom() {
        let bytes = [0xFE, 0xFF, 0x00, 0x48, 0x00, 0x69];
        assert_eq!(decode_text_string(&bytes), "Hi");
    }

    #[test]
    fn decodes_pdf_doc_encoding_specials() {
        assert_eq!(decode_text_string(b"a\x84b"), "a\u{2014}b");
        assert_eq!(decode_text_string(b"caf\xe9"), "café");
    }

    #[test]
    fn strips_utf16_language_escape() {
        let bytes = [
            0xFE, 0xFF, 0x00, 0x1B, 0x00, b'e', 0x00, b'n', 0x00, 0x1B, 0x00, b'x',
        ];
        assert_eq!(decode_text_string(&bytes), "x");
    }
}
