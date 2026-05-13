use image::{ImageBuffer, Rgba};
use pdfium::Library;
use serde::Serialize;

/// Render a single page to a PNG file.
pub fn screenshot(
    pdf_path: &str,
    page_num: u32,
    dpi: f32,
    output_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let lib = Library::init();
    let document = lib.load_document(pdf_path, None)?;
    let page = document.page((page_num - 1) as i32)?;
    let bitmap = page.render(dpi)?;

    let width = bitmap.width() as u32;
    let height = bitmap.height() as u32;
    let rgba = bitmap.to_rgba();

    let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width, height, rgba).ok_or("failed to create image buffer")?;

    img.save(output_path)?;
    eprintln!(
        "[rust-bin] rendered page {} at {dpi} DPI → {output_path} ({width}×{height})",
        page_num
    );

    Ok(())
}

/// Render a single page and write raw PNG bytes to stdout.
pub fn screenshot_to_stdout(
    pdf_path: &str,
    page_num: u32,
    dpi: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    let lib = Library::init();
    let document = lib.load_document(pdf_path, None)?;
    let page = document.page((page_num - 1) as i32)?;
    let bitmap = page.render(dpi)?;

    let width = bitmap.width() as u32;
    let height = bitmap.height() as u32;
    let rgba = bitmap.to_rgba();

    let encoder = image::codecs::png::PngEncoder::new(std::io::stdout().lock());
    use image::ImageEncoder;
    encoder.write_image(&rgba, width, height, image::ColorType::Rgba8.into())?;

    Ok(())
}

#[derive(Debug, Serialize)]
struct ImageBoundsOutput {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

/// Extract image bounding boxes and print as JSON to stdout.
pub fn image_bounds(
    pdf_path: &str,
    page_num: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    let lib = Library::init();
    let document = lib.load_document(pdf_path, None)?;
    let page_count = document.page_count();

    for page_index in 0..page_count {
        if let Some(target) = page_num
            && page_index as u32 + 1 != target
        {
            continue;
        }

        let page = document.page(page_index)?;
        let bounds = page.image_bounds(25.0, 0.9);

        let output: Vec<ImageBoundsOutput> = bounds
            .iter()
            .map(|b| ImageBoundsOutput {
                x: b.x,
                y: b.y,
                width: b.width,
                height: b.height,
            })
            .collect();

        let json = serde_json::json!({
            "page_number": page_index + 1,
            "images": output,
        });
        println!("{}", serde_json::to_string(&json)?);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_bounds_output_serializes() {
        let b = ImageBoundsOutput {
            x: 1.0,
            y: 2.0,
            width: 3.0,
            height: 4.0,
        };
        let s = serde_json::to_string(&b).unwrap();
        assert!(s.contains("\"x\":1"));
        assert!(s.contains("\"width\":3"));
    }

    #[test]
    fn test_screenshot_missing_file_errors() {
        let r = screenshot(
            "/nonexistent/path/does_not_exist.pdf",
            1,
            72.0,
            "/tmp/out.png",
        );
        assert!(r.is_err());
    }

    #[test]
    fn test_image_bounds_missing_file_errors() {
        let r = image_bounds("/nonexistent/path/does_not_exist.pdf", None);
        assert!(r.is_err());
    }
}
