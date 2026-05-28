use std::sync::Arc;

use crate::error::LiteParseError;
use crate::ocr::{OcrEngine, OcrOptions, OcrResult};
use crate::types::{Page, TextItem};
use image::{ImageBuffer, Rgba};
use pdfium::Document;

/// Owned page bitmap prepared for OCR. Indices refer to positions in the `pages` slice.
pub(crate) struct RenderedPage {
    pub idx: usize,
    pub rgb_bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Render pages that need OCR from an already-open document.
///
/// The pdfium `Document` holds raw pointers that are not `Send`, so callers must
/// drop it before awaiting the OCR engine.
pub(crate) fn render_pages_for_ocr(
    document: &Document,
    pages: &[Page],
    dpi: f32,
) -> Result<Vec<RenderedPage>, LiteParseError> {
    let mut rendered = Vec::new();
    for (idx, page) in pages.iter().enumerate() {
        let text_length: usize = page.text_items.iter().map(|item| item.text.len()).sum();
        let page_obj = document.page((page.page_number - 1) as i32)?;
        let has_images = !page_obj.image_bounds(25.0, 0.9).is_empty();

        let page_area = page.page_width * page.page_height;
        let text_bbox_area: f32 = page
            .text_items
            .iter()
            .map(|item| item.width * item.height)
            .sum();
        let text_coverage = if page_area > 0.0 {
            text_bbox_area / page_area
        } else {
            0.0
        };

        let needs_ocr = text_length < 20 || text_coverage < 0.15 || has_images;
        if !needs_ocr {
            continue;
        }

        let bitmap = page_obj.render(dpi)?;
        let width = bitmap.width() as u32;
        let height = bitmap.height() as u32;
        let rgba = bitmap.to_rgba();

        let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_raw(width, height, rgba)
            .ok_or(LiteParseError::Other(
                "failed to create image buffer".into(),
            ))?;
        let rgb_img = image::DynamicImage::ImageRgba8(img).to_rgb8();
        let rgb_bytes = rgb_img.into_raw();

        rendered.push(RenderedPage {
            idx,
            rgb_bytes,
            width,
            height,
        });
    }
    Ok(rendered)
}

/// Run OCR on pre-rendered page bitmaps and merge results into `pages`.
pub(crate) async fn ocr_and_merge_rendered(
    pages: &mut [Page],
    rendered: Vec<RenderedPage>,
    dpi: f32,
    ocr_engine: Arc<dyn OcrEngine>,
    ocr_language: &str,
    num_workers: usize,
) -> Result<(), LiteParseError> {
    // Phase 1: spawn OCR tasks onto the tokio runtime so they run on
    // separate threads. A semaphore limits concurrency to `num_workers`.
    let num_workers = num_workers.max(1);
    let semaphore = Arc::new(tokio::sync::Semaphore::new(num_workers));
    let mut handles = Vec::with_capacity(rendered.len());

    let handle = tokio::runtime::Handle::current();

    for r in rendered {
        let engine = ocr_engine.clone();
        let sem = semaphore.clone();
        let language = ocr_language.to_string();
        let page_number = pages[r.idx].page_number;
        let rt_handle = handle.clone();

        handles.push((
            r.idx,
            page_number,
            tokio::task::spawn_blocking(move || {
                // Block this thread until a permit is available.
                let _permit = rt_handle
                    .block_on(sem.acquire_owned())
                    .expect("semaphore closed");
                let options = OcrOptions { language };
                rt_handle.block_on(engine.recognize(&r.rgb_bytes, r.width, r.height, &options))
            }),
        ));
    }

    // Phase 3: collect results and merge into pages.
    let scale_factor = 72.0 / dpi;

    for (idx, page_number, handle) in handles {
        let ocr_results: Vec<OcrResult> = match handle.await {
            Ok(Ok(results)) => results,
            Ok(Err(e)) => {
                eprintln!("[ocr] failed for page {}: {}", page_number, e);
                continue;
            }
            Err(e) => {
                eprintln!("[ocr] task panicked for page {}: {}", page_number, e);
                continue;
            }
        };

        if ocr_results.is_empty() {
            continue;
        }

        let page = &mut pages[idx];
        for r in &ocr_results {
            if r.confidence <= 0.1 {
                continue;
            }

            let ocr_x = r.bbox[0] * scale_factor;
            let ocr_y = r.bbox[1] * scale_factor;
            let ocr_w = (r.bbox[2] - r.bbox[0]) * scale_factor;
            let ocr_h = (r.bbox[3] - r.bbox[1]) * scale_factor;

            if overlaps_existing_text(&page.text_items, ocr_x, ocr_y, ocr_w, ocr_h, 2.0) {
                continue;
            }

            let cleaned = clean_ocr_table_artifacts(&r.text);
            if cleaned.is_empty() {
                continue;
            }

            page.text_items.push(TextItem {
                text: cleaned,
                x: ocr_x,
                y: ocr_y,
                width: ocr_w,
                height: ocr_h,
                font_name: Some("OCR".to_string()),
                font_size: Some(ocr_h),
                confidence: Some((r.confidence * 1000.0).round() / 1000.0),
                ..Default::default()
            });
        }
    }

    Ok(())
}

/// Check if an OCR bounding box overlaps with any existing text item.
fn overlaps_existing_text(
    items: &[TextItem],
    ocr_x: f32,
    ocr_y: f32,
    ocr_w: f32,
    ocr_h: f32,
    tolerance: f32,
) -> bool {
    for item in items {
        let item_right = item.x + item.width;
        let item_bottom = item.y + item.height;

        let overlap_x = ocr_x < item_right + tolerance && ocr_x + ocr_w > item.x - tolerance;
        let overlap_y = ocr_y < item_bottom + tolerance && ocr_y + ocr_h > item.y - tolerance;

        if overlap_x && overlap_y {
            return true;
        }
    }
    false
}

/// Clean common OCR artifacts from table border misreads.
/// OCR often misreads vertical table border lines as bracket-like characters.
fn clean_ocr_table_artifacts(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // Strip leading/trailing border artifact characters: | [ ] ( ) { }
    let without_artifacts: &str = trimmed
        .trim_start_matches(['|', '[', ']', '(', ')', '{', '}'])
        .trim_end_matches(['|', '[', ']', '(', ')', '{', '}'])
        .trim();

    if without_artifacts.is_empty() {
        return trimmed.to_string();
    }

    // Only use cleaned version if core content looks numeric-ish
    // This avoids incorrectly stripping brackets from content like "(note)"
    let is_numeric_ish = without_artifacts
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, ',' | '.' | ' ' | '%' | '-' | '+' | '*' | '/'))
        || without_artifacts == "N/A"
        || without_artifacts == "Z"
        || without_artifacts == "-";

    if is_numeric_ish {
        without_artifacts.to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_ocr_table_artifacts() {
        assert_eq!(clean_ocr_table_artifacts("44520]"), "44520");
        assert_eq!(clean_ocr_table_artifacts("|123"), "123");
        assert_eq!(clean_ocr_table_artifacts("0.3|"), "0.3");
        assert_eq!(clean_ocr_table_artifacts("(note)"), "(note)");
        assert_eq!(clean_ocr_table_artifacts("|hello|"), "|hello|");
        assert_eq!(clean_ocr_table_artifacts("N/A"), "N/A");
        assert_eq!(clean_ocr_table_artifacts(""), "");
        assert_eq!(clean_ocr_table_artifacts("|||"), "|||");
    }

    fn make_item(x: f32, y: f32, w: f32, h: f32) -> TextItem {
        TextItem {
            text: "x".into(),
            x,
            y,
            width: w,
            height: h,
            ..Default::default()
        }
    }

    #[test]
    fn test_overlaps_existing_text_inside() {
        let items = vec![make_item(10.0, 10.0, 20.0, 5.0)];
        assert!(overlaps_existing_text(&items, 12.0, 11.0, 5.0, 2.0, 2.0));
    }

    #[test]
    fn test_overlaps_existing_text_disjoint() {
        let items = vec![make_item(10.0, 10.0, 20.0, 5.0)];
        assert!(!overlaps_existing_text(&items, 100.0, 100.0, 5.0, 5.0, 2.0));
    }

    #[test]
    fn test_overlaps_existing_text_tolerance() {
        let items = vec![make_item(10.0, 10.0, 20.0, 5.0)];
        // Just outside but within tolerance
        assert!(overlaps_existing_text(&items, 31.0, 10.0, 5.0, 5.0, 2.0));
        // Beyond tolerance
        assert!(!overlaps_existing_text(&items, 35.0, 10.0, 5.0, 5.0, 2.0));
    }

    #[test]
    fn test_overlaps_empty() {
        assert!(!overlaps_existing_text(&[], 0.0, 0.0, 1.0, 1.0, 0.0));
    }

    #[test]
    fn test_clean_ocr_keeps_whitespace_trimmed() {
        assert_eq!(clean_ocr_table_artifacts("   "), "");
        assert_eq!(clean_ocr_table_artifacts(" 123 "), "123");
    }
}
