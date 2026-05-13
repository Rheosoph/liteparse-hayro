use std::time::Duration;

use reqwest::blocking::{
    Client,
    multipart::{Form, Part},
};
use serde::{Deserialize, Serialize};

use crate::ocr::{OcrEngine, OcrOptions, OcrResult};

#[derive(Debug, Serialize, Deserialize)]
pub struct HttpOcrResponseItem {
    text: String,
    bbox: [f32; 4],
    confidence: f32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HttpOcrResponse {
    pub results: Vec<HttpOcrResponseItem>,
}

/// HTTP-based OCR engine that conforms to LiteParse OCR API specification.
/// The server must implement the API defined in OCR_API_SPEC.md:
///     - POST /ocr endpoint
///     - Accepts multipart/form-data with 'file' and 'language' fields
///     - Returns JSON: { results: [{ text, bbox: [x1,y1,x2,y2], confidence }] }
/// See ocr/easyocr/ and ocr/paddleocr/ for example server implementations.
pub struct HttpOcrEngine {
    pub name: String,
    server_url: String,
}

impl HttpOcrEngine {
    pub fn new(server_url: String) -> Self {
        Self {
            name: "http-ocr".to_string(),
            server_url,
        }
    }

    fn _recognize_batch(
        &self,
        images: Vec<&[u8]>,
        options: OcrOptions,
    ) -> Result<Vec<Vec<OcrResult>>, Box<dyn std::error::Error>> {
        let mut results: Vec<Vec<OcrResult>> = vec![];
        for i in images {
            let result = self.recognize(i, 0, 0, &options)?;
            results.push(result);
        }
        Ok(results)
    }
}

impl OcrEngine for HttpOcrEngine {
    fn name(&self) -> &str {
        &self.name
    }

    fn recognize(
        &self,
        image_data: &[u8],
        _width: u32,
        _height: u32,
        options: &OcrOptions,
    ) -> Result<Vec<OcrResult>, Box<dyn std::error::Error>> {
        let client = Client::new();
        let form = Form::new()
            .part(
                "file",
                Part::bytes(image_data.to_vec())
                    .file_name("image.png")
                    .mime_str("image/png")?,
            )
            .text("language", options.language.clone());
        let response: HttpOcrResponse = client
            .post(&self.server_url)
            .multipart(form)
            .timeout(Duration::from_millis(60000))
            .send()?
            .json()?;

        let results: Vec<OcrResult> = response
            .results
            .iter()
            .map(|i| OcrResult {
                text: i.text.clone(),
                bbox: i.bbox,
                confidence: i.confidence,
            })
            .collect();

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_sets_name_and_url() {
        let e = HttpOcrEngine::new("http://example.com/ocr".into());
        assert_eq!(e.name(), "http-ocr");
        assert_eq!(e.server_url, "http://example.com/ocr");
    }

    #[test]
    fn test_response_deserializes() {
        let raw = r#"{"results":[{"text":"hi","bbox":[1.0,2.0,3.0,4.0],"confidence":0.85}]}"#;
        let parsed: HttpOcrResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.results.len(), 1);
        assert_eq!(parsed.results[0].text, "hi");
        assert_eq!(parsed.results[0].bbox, [1.0, 2.0, 3.0, 4.0]);
        assert!((parsed.results[0].confidence - 0.85).abs() < 1e-6);
    }

    #[test]
    fn test_response_deserializes_empty() {
        let raw = r#"{"results":[]}"#;
        let parsed: HttpOcrResponse = serde_json::from_str(raw).unwrap();
        assert!(parsed.results.is_empty());
    }

    #[test]
    fn test_recognize_network_error() {
        let e = HttpOcrEngine::new("http://127.0.0.1:1/ocr".into());
        let opts = OcrOptions {
            language: "eng".into(),
        };
        let r = e.recognize(&[0u8; 4], 1, 1, &opts);
        assert!(r.is_err());
    }
}
