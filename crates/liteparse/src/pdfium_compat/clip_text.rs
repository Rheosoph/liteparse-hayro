//! Text in clip-only render mode (`7 Tr`).
//!
//! hayro turns such text into a clip path without handing the glyphs to the
//! device, while PDFium keeps it as an ordinary text object (reporting mode
//! 7). For the object pass, page content is rewritten to show that text in
//! invisible mode (`3 Tr`) instead, which hayro does deliver; the content
//! scan then reports the runs as clip text. Rendering keeps the original
//! content.

/// `content` with every `7 Tr` operator turned into `3 Tr`, or `None` when
/// there is none.
pub(crate) fn reveal_clip_text(content: &[u8]) -> Option<Vec<u8>> {
    if !content.windows(2).any(|w| w == b"Tr") {
        return None;
    }
    let mut sevens: Vec<(usize, usize)> = Vec::new();
    let mut previous_seven: Option<(usize, usize)> = None;
    let n = content.len();
    let mut i = 0;
    while i < n {
        match content[i] {
            b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'\0' => {
                i += 1;
                continue;
            }
            b'%' => {
                while i < n && !matches!(content[i], b'\n' | b'\r') {
                    i += 1;
                }
            }
            b'(' => i = skip_string(content, i),
            b'<' if content.get(i + 1) == Some(&b'<') => i += 2,
            b'<' => {
                while i < n && content[i] != b'>' {
                    i += 1;
                }
                i += 1;
            }
            b'/' => {
                i += 1;
                while i < n && is_regular(content[i]) {
                    i += 1;
                }
            }
            b'>' | b'[' | b']' | b'{' | b'}' | b')' => i += 1,
            _ => {
                let start = i;
                while i < n && is_regular(content[i]) {
                    i += 1;
                }
                let token = &content[start..i];
                if token == b"Tr"
                    && let Some(seven) = previous_seven
                {
                    sevens.push(seven);
                } else if token == b"ID" {
                    i = skip_inline_image(content, i);
                }
                previous_seven = is_seven(token).then_some((start, i));
                continue;
            }
        }
        previous_seven = None;
    }
    if sevens.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(content.len());
    let mut copied = 0;
    for (start, end) in sevens {
        out.extend_from_slice(&content[copied..start]);
        out.push(b'3');
        copied = end;
    }
    out.extend_from_slice(&content[copied..]);
    Some(out)
}

fn is_regular(b: u8) -> bool {
    !matches!(
        b,
        b' ' | b'\t'
            | b'\n'
            | b'\r'
            | b'\x0c'
            | b'\0'
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

fn is_seven(token: &[u8]) -> bool {
    std::str::from_utf8(token)
        .ok()
        .and_then(|t| t.parse::<f64>().ok())
        == Some(7.0)
}

/// The index after the literal string starting at `start`.
fn skip_string(content: &[u8], start: usize) -> usize {
    let mut depth = 0;
    let mut i = start;
    while i < content.len() {
        match content[i] {
            b'\\' => i += 1,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    content.len()
}

/// The index after the `EI` that ends inline image data starting at `start`
/// (just after `ID`).
fn skip_inline_image(content: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i + 1 < content.len() {
        if &content[i..i + 2] == b"EI"
            && content[i - 1].is_ascii_whitespace()
            && content.get(i + 2).is_none_or(|b| !is_regular(*b))
        {
            return i + 2;
        }
        i += 1;
    }
    content.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_clip_only_render_mode() {
        let content = b"BT 7 Tr (7 Tr) Tj 0 Tr /F1 7 Tf ET q 7.0 Tr Q";
        assert_eq!(
            reveal_clip_text(content).unwrap(),
            b"BT 3 Tr (7 Tr) Tj 0 Tr /F1 7 Tf ET q 3 Tr Q".to_vec()
        );
        assert!(reveal_clip_text(b"BT 0 Tr (a) Tj ET").is_none());
    }

    #[test]
    fn skips_inline_image_data() {
        let content = b"BI /W 1 /H 1 ID 7 Tr EI 7 Tr";
        assert_eq!(
            reveal_clip_text(content).unwrap(),
            b"BI /W 1 /H 1 ID 7 Tr EI 3 Tr".to_vec()
        );
    }
}
