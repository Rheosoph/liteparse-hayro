//! Detection of the PDF `/UserUnit` page key, which PDFium ignores entirely.
//!
//! `/UserUnit` rescales a page's coordinate space: 1 unit = `UserUnit`/72
//! inch instead of 1/72 inch. Generators (notably spreadsheet/report
//! exporters) use it to exceed the 14,400 pt MediaBox limit by writing a
//! small MediaBox with a large multiplier — e.g. a 35 × 14,156 pt MediaBox
//! with `/UserUnit 36` is really a 1,278 × 509,625 pt page whose "0.3 pt"
//! text is an ordinary 11 pt font. Because PDFium reports the raw MediaBox
//! units, such pages look like microscopic text on a tiny page and get
//! filtered out wholesale unless the multiplier is reapplied.
//!
//! Stock PDFium's public API has no way to read `/UserUnit`. The primary
//! source is the LlamaParse fork's `FPDFPage_GetUserUnit` export, which
//! reads the real page dictionary; when the loaded binary predates that
//! export (or on wasm, where a static reference to a missing symbol would
//! be a link error), this module is the fallback: it scans the raw PDF
//! bytes for page objects that declare the key. Matching a found entry
//! back to a page index is done by comparing the object's `/MediaBox`
//! dimensions against PDFium's reported page size — object numbers alone
//! can't be mapped to page indices without reimplementing the xref/page
//! tree walk.
//!
//! Known limitations of the byte-scan fallback (all fail safe, i.e. behave
//! as before this module; none apply to the `FPDFPage_GetUserUnit` path):
//! - `/UserUnit` inside a compressed object stream is invisible to the scan.
//! - A page object that *inherits* its `/MediaBox` from `/Pages` is skipped
//!   (`/UserUnit` itself is not inheritable, so both keys are normally
//!   written together by the generators that use it).
//! - Two same-sized pages where only one declares `/UserUnit` would both be
//!   scaled; in practice identical freak page sizes come from the same
//!   generator path and carry the same multiplier.

/// One `/UserUnit` declaration found in the raw bytes, keyed by the
/// MediaBox dimensions written in the same page object.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct UserUnitEntry {
    pub media_width: f32,
    pub media_height: f32,
    pub user_unit: f32,
}

/// The spec restricts `/UserUnit` to positive values; Acrobat additionally
/// treats values below 1.0 as 1.0 and documents 75,000 as the maximum.
const MIN_USER_UNIT: f32 = 1.0;
const MAX_USER_UNIT: f32 = 75_000.0;

const NEEDLE: &[u8] = b"/UserUnit";

/// Scan raw PDF bytes for page objects declaring `/UserUnit`.
///
/// Returns only entries with a `/MediaBox` in the same `obj … endobj` span
/// and a user unit that is finite and > 1.0 (1.0 is the default and needs
/// no rescale).
pub(crate) fn scan_user_units(data: &[u8]) -> Vec<UserUnitEntry> {
    let mut entries = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = find(&data[from..], NEEDLE) {
        let pos = from + rel;
        from = pos + NEEDLE.len();
        // Reject prefixes of longer names like `/UserUnitFoo`.
        match data.get(pos + NEEDLE.len()) {
            Some(next) if is_regular_char(*next) => continue,
            None => break,
            _ => {}
        }
        let Some(user_unit) = parse_number_at(data, pos + NEEDLE.len()) else {
            continue;
        };
        let user_unit = user_unit as f32;
        if !user_unit.is_finite() || user_unit <= MIN_USER_UNIT || user_unit > MAX_USER_UNIT {
            continue;
        }
        let Some((obj_start, obj_end)) = enclosing_object(data, pos) else {
            continue;
        };
        let object = &data[obj_start..obj_end];
        let Some((media_width, media_height)) = parse_media_box(object) else {
            continue;
        };
        if media_width <= 0.0 || media_height <= 0.0 {
            continue;
        }
        entries.push(UserUnitEntry {
            media_width,
            media_height,
            user_unit,
        });
    }
    entries
}

/// Map scanned entries to a per-page-index multiplier by matching each
/// page's PDFium-reported size (already available to the caller) against
/// the entries' MediaBox dimensions. Unmatched pages get 1.0.
pub(crate) fn match_user_unit(entries: &[UserUnitEntry], width: f32, height: f32) -> f32 {
    for entry in entries {
        // FPDF_GetPageSizeByIndexF reflects /Rotate, so compare both ways.
        if (close(width, entry.media_width) && close(height, entry.media_height))
            || (close(width, entry.media_height) && close(height, entry.media_width))
        {
            return entry.user_unit;
        }
    }
    1.0
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() <= 0.5 + a.abs().max(b.abs()) * 1e-4
}

/// Whether a byte can continue a PDF name (i.e. is not a delimiter or
/// whitespace per the PDF lexer).
fn is_regular_char(byte: u8) -> bool {
    !matches!(
        byte,
        b'\0'
            | b'\t'
            | b'\n'
            | b'\x0c'
            | b'\r'
            | b' '
            | b'('
            | b')'
            | b'<'
            | b'>'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b'/'
            | b'%'
    )
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Parse the first number at or after `start`, skipping PDF whitespace.
fn parse_number_at(data: &[u8], start: usize) -> Option<f64> {
    let mut i = start;
    while i < data.len() && matches!(data[i], b'\0' | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ') {
        i += 1;
    }
    let num_start = i;
    if i < data.len() && matches!(data[i], b'+' | b'-') {
        i += 1;
    }
    let mut seen_digit = false;
    while i < data.len() && (data[i].is_ascii_digit() || data[i] == b'.') {
        seen_digit |= data[i].is_ascii_digit();
        i += 1;
    }
    if !seen_digit {
        return None;
    }
    std::str::from_utf8(&data[num_start..i]).ok()?.parse().ok()
}

/// Find the `N G obj … endobj` span containing `pos`. Returns byte offsets
/// just after `obj` and at `endobj`.
fn enclosing_object(data: &[u8], pos: usize) -> Option<(usize, usize)> {
    // Backwards: nearest `obj` token preceded by whitespace (part of the
    // `N G obj` header, distinguishing it from `endobj`).
    let before = &data[..pos];
    let mut obj_start = None;
    let mut i = before.len();
    while i >= 3 {
        if &before[i - 3..i] == b"obj"
            && (i < 6 || &before[i - 6..i] != b"endobj")
            && before
                .get(i.wrapping_sub(4))
                .is_some_and(|b| b.is_ascii_whitespace())
        {
            obj_start = Some(i);
            break;
        }
        i -= 1;
    }
    let obj_start = obj_start?;
    let after = &data[pos..];
    let obj_end = pos + find(after, b"endobj")?;
    Some((obj_start, obj_end))
}

/// Parse `/MediaBox [a b c d]` inside an object slice into (width, height).
fn parse_media_box(object: &[u8]) -> Option<(f32, f32)> {
    let key_pos = find(object, b"/MediaBox")?;
    let mut i = key_pos + b"/MediaBox".len();
    while i < object.len() && object[i] != b'[' {
        // Only whitespace may sit between the key and its array.
        if !object[i].is_ascii_whitespace() {
            return None;
        }
        i += 1;
    }
    i += 1; // past '['
    let mut values = [0.0f64; 4];
    for value in &mut values {
        *value = parse_number_at(object, i)?;
        // Advance past the parsed number.
        while i < object.len() && object[i].is_ascii_whitespace() {
            i += 1;
        }
        while i < object.len() && matches!(object[i], b'+' | b'-' | b'.' | b'0'..=b'9') {
            i += 1;
        }
    }
    Some((
        (values[2] - values[0]).abs() as f32,
        (values[3] - values[1]).abs() as f32,
    ))
}

/// Cheap containment check for path-loaded documents: stream the file in
/// chunks looking for `/UserUnit` so the (overwhelmingly common) negative
/// case never allocates the whole file.
pub(crate) fn file_mentions_user_unit(path: &str) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = vec![0u8; 256 * 1024];
    let overlap = NEEDLE.len() - 1;
    let mut filled = 0usize;
    loop {
        let read = match file.read(&mut buf[filled..]) {
            Ok(0) => return false,
            Ok(n) => n,
            Err(_) => return false,
        };
        let end = filled + read;
        if find(&buf[..end], NEEDLE).is_some() {
            return true;
        }
        // Keep the tail so a needle spanning the chunk boundary still hits.
        let keep = end.min(overlap);
        buf.copy_within(end - keep..end, 0);
        filled = keep;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_user_unit_with_media_box() {
        let pdf = b"6 0 obj\n<</Type/Page/Parent 15 0 R/MediaBox[0 0 35.5 14156.25]\n/UserUnit 36/Tabs/S>>\nendobj\n";
        let entries = scan_user_units(pdf);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].user_unit, 36.0);
        assert!((entries[0].media_width - 35.5).abs() < 1e-3);
        assert!((entries[0].media_height - 14156.25).abs() < 1e-2);
    }

    #[test]
    fn ignores_user_unit_without_media_box_in_object() {
        let pdf = b"6 0 obj\n<</Type/Page/Parent 15 0 R/UserUnit 36>>\nendobj\n";
        assert!(scan_user_units(pdf).is_empty());
    }

    #[test]
    fn ignores_default_and_invalid_values() {
        let pdf = b"1 0 obj\n<</MediaBox[0 0 10 10]/UserUnit 1>>\nendobj\n\
                    2 0 obj\n<</MediaBox[0 0 10 10]/UserUnit -5>>\nendobj\n\
                    3 0 obj\n<</MediaBox[0 0 10 10]/UserUnit 999999>>\nendobj\n";
        assert!(scan_user_units(pdf).is_empty());
    }

    #[test]
    fn ignores_longer_names() {
        let pdf = b"1 0 obj\n<</MediaBox[0 0 10 10]/UserUnitCustom 36>>\nendobj\n";
        assert!(scan_user_units(pdf).is_empty());
    }

    #[test]
    fn matches_rotated_dimensions() {
        let entries = [UserUnitEntry {
            media_width: 35.5,
            media_height: 14156.25,
            user_unit: 36.0,
        }];
        assert_eq!(match_user_unit(&entries, 35.5, 14156.25), 36.0);
        assert_eq!(match_user_unit(&entries, 14156.25, 35.5), 36.0);
        assert_eq!(match_user_unit(&entries, 612.0, 792.0), 1.0);
    }

    #[test]
    fn parses_media_box_with_offsets_and_floats() {
        let pdf = b"1 0 obj\n<</MediaBox [ -10.5 20 100.5 320 ] /UserUnit 2.5>>\nendobj\n";
        let entries = scan_user_units(pdf);
        assert_eq!(entries.len(), 1);
        assert!((entries[0].media_width - 111.0).abs() < 1e-3);
        assert!((entries[0].media_height - 300.0).abs() < 1e-3);
        assert!((entries[0].user_unit - 2.5).abs() < 1e-6);
    }
}
