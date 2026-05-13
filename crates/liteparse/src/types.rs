use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Supported output formats for parsed documents.
/// - `"json"` — Structured JSON with per-page text items, bounding boxes, and metadata.
/// - `"text"` — Plain text with spatial layout preserved.
#[derive(Debug, Serialize, Deserialize)]
pub enum OutputFormat {
    Json,
    Text,
}

/// Accepted input types for input documents.
/// - `FilePath(String)` — A file path to a local PDF document.
/// - `Buffer(Vec<u8>)` — A byte buffer containing the PDF data.
#[derive(Debug, Serialize, Deserialize)]
pub enum InputType {
    FilePath(String),
    Buffer(Vec<u8>),
}

/// Represents a single text item extracted from a PDF page,
/// including its content, position, size, rotation, and font metadata.
#[derive(Debug, Clone, Serialize)]
pub struct TextItem {
    pub text: String,
    /// Viewport-space coordinates (top-left origin, 72 DPI).
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Rotation in degrees (counter-clockwise, adjusted for page rotation).
    pub rotation: f32,
    pub font_name: Option<String>,
    pub font_size: Option<f32>,
    /// Font size * scale_y from the text matrix — accounts for CTM scaling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_height: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_ascent: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_descent: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_weight: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_flags: Option<i32>,
    /// Sum of glyph widths (using charcode-based lookup when possible).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_width: Option<f32>,
    /// Whether the font has buggy encoding (private-use codepoints, TT subset, etc.)
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub font_is_buggy: bool,
    /// Marked content ID from the PDF structure tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcid: Option<i32>,
    /// Fill color as ARGB hex string (e.g. "ff000000").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill_color: Option<String>,
    /// Stroke color as ARGB hex string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stroke_color: Option<String>,
    /// OCR confidence score (0.0–1.0). None for native PDF text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

/// Represents a single page in a PDF document, including its dimensions and extracted text items.
#[derive(Debug, Serialize)]
pub struct Page {
    pub page_number: usize,
    pub page_width: f32,
    pub page_height: f32,
    pub text_items: Vec<TextItem>,
}

/// Represents a fully parsed page with projected text layout.
#[derive(Debug, Serialize)]
pub struct ParsedPage {
    pub page_number: usize,
    pub page_width: f32,
    pub page_height: f32,
    pub text: String,
    pub text_items: Vec<TextItem>,
}

#[derive(Debug, Serialize)]
pub enum Snap {
    Left,
    Right,
    Center,
}

#[derive(Debug, Serialize)]
pub enum Anchor {
    Left,
    Right,
    Center,
}

/// Represents a Projected piece of text, responsible for keeping track of projection related data
#[derive(Debug, Serialize)]
pub struct ProjectedTextItem {
    pub item: TextItem,
    pub snap: Snap,
    pub anchor: Anchor,
    pub is_dup: bool,
    pub rendered: bool,
    pub num_spaces: usize,
    pub force_unsnapped: bool,
    pub is_margin_line_number: bool,
    pub rotated: bool,
    pub d: f32,
}

pub type AnchorMap = HashMap<i32, Vec<(usize, usize)>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_item() -> TextItem {
        TextItem {
            text: "hi".into(),
            x: 1.0,
            y: 2.0,
            width: 10.0,
            height: 4.0,
            rotation: 0.0,
            font_name: Some("Arial".into()),
            font_size: Some(12.0),
            font_height: None,
            font_ascent: None,
            font_descent: None,
            font_weight: None,
            font_flags: None,
            text_width: None,
            font_is_buggy: false,
            mcid: None,
            fill_color: None,
            stroke_color: None,
            confidence: None,
        }
    }

    #[test]
    fn output_format_serializes() {
        assert_eq!(
            serde_json::to_string(&OutputFormat::Json).unwrap(),
            "\"Json\""
        );
        assert_eq!(
            serde_json::to_string(&OutputFormat::Text).unwrap(),
            "\"Text\""
        );
    }

    #[test]
    fn input_type_roundtrip() {
        let v = InputType::FilePath("/tmp/x.pdf".into());
        let s = serde_json::to_string(&v).unwrap();
        let back: InputType = serde_json::from_str(&s).unwrap();
        match back {
            InputType::FilePath(p) => assert_eq!(p, "/tmp/x.pdf"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn text_item_skips_none_fields() {
        let item = sample_item();
        let s = serde_json::to_string(&item).unwrap();
        assert!(!s.contains("font_height"));
        assert!(!s.contains("confidence"));
        assert!(!s.contains("font_is_buggy"));
        assert!(s.contains("\"text\":\"hi\""));
    }

    #[test]
    fn text_item_includes_buggy_flag_when_true() {
        let mut item = sample_item();
        item.font_is_buggy = true;
        let s = serde_json::to_string(&item).unwrap();
        assert!(s.contains("font_is_buggy"));
    }

    #[test]
    fn page_serializes() {
        let p = Page {
            page_number: 1,
            page_width: 100.0,
            page_height: 200.0,
            text_items: vec![sample_item()],
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"page_number\":1"));
    }

    #[test]
    fn anchor_map_basic() {
        let mut m: AnchorMap = HashMap::new();
        m.entry(5).or_default().push((1, 2));
        assert_eq!(m.get(&5).unwrap()[0], (1, 2));
    }
}
