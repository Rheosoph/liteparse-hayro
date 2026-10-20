//! pdfium reports a hyphen at the end of a line as U+0002; the extracted text keeps the drawn '-'.
use liteparse::extract_raw_text_items;
use liteparse::pdfium_compat as pdfium;
use pdfium::Library;

fn pdf(content: &str, forms: &[&str], rotation: i32) -> Vec<u8> {
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Rotate {rotation} /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>"
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Courier >>".to_string(),
        stream("", content),
    ];
    objects.extend(forms.iter().map(|s| s.to_string()));
    let mut data = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(data.len());
        data.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
    }
    let xref = data.len();
    data.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        data.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    data.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    data
}

fn stream(dict: &str, content: &str) -> String {
    format!(
        "<< {dict} /Length {} >>\nstream\n{content}\nendstream",
        content.len()
    )
}

fn raw_items(bytes: &[u8]) -> Vec<String> {
    let lib = Library::init();
    let doc = lib.load_document_from_bytes(bytes, None).unwrap();
    let page = doc.page(0).unwrap();
    let text = page.text().unwrap();
    extract_raw_text_items(&page, &text, &page.view_box().unwrap(), None)
        .into_iter()
        .map(|item| item.text)
        .collect()
}

#[test]
fn line_end_hyphen_keeps_its_dash() {
    let content = "BT /F1 10 Tf 50 700 Td (Used improperly, technolo-) Tj 0 -12 Td (gies can and do result) Tj ET";
    let items = raw_items(&pdf(content, &[], 0));
    let joined = items.join("|");
    assert!(!joined.contains('\u{2}'), "control code leaked: {joined:?}");
    assert!(
        items
            .iter()
            .any(|item| item.trim_end().ends_with("technolo-")),
        "hyphenated word lost its dash: {joined:?}"
    );
}

/// A font whose ToUnicode maps `X` to U+0002, written mid-line.
fn stx_font_pdf() -> Vec<u8> {
    let cmap = "/CIDInit /ProcSet findresource begin 12 dict begin begincmap /CMapName /T def 1 begincodespacerange <00> <FF> endcodespacerange 1 beginbfchar <58> <0002> endbfchar endcmap CMapName currentdict /CMap defineresource pop end end";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Courier /ToUnicode 6 0 R >>".to_string(),
        stream("", "BT /F1 10 Tf 50 700 Td (abXcd efgh ijkl) Tj ET"),
        stream("", cmap),
    ];
    let mut data = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(data.len());
        data.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
    }
    let xref = data.len();
    data.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        data.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    data.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    data
}

#[test]
fn mapped_stx_that_is_not_a_hyphen_is_left_alone() {
    let items = raw_items(&stx_font_pdf());
    let joined = items.join("|");
    assert!(
        joined.contains("ab\u{2}cd"),
        "the mapped code changed: {joined:?}"
    );
    assert!(
        !joined.contains("ab-cd"),
        "a non-hyphen 0x02 became a dash: {joined:?}"
    );
}
