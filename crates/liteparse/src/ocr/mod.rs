pub mod http_simple;
pub mod tesseract;

/// A single word-level OCR result with bounding box and confidence.
#[derive(Debug, Clone)]
pub struct OcrResult {
    pub text: String,
    /// Bounding box in pixel coordinates: [x1, y1, x2, y2] (left, top, right, bottom).
    pub bbox: [f32; 4],
    /// Confidence score in 0.0–1.0 range.
    pub confidence: f32,
}

pub struct OcrOptions {
    pub language: String,
}

pub trait OcrEngine {
    fn name(&self) -> &str;
    fn recognize(
        &self,
        image_data: &[u8],
        width: u32,
        height: u32,
        options: &OcrOptions,
    ) -> Result<Vec<OcrResult>, Box<dyn std::error::Error>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyEngine;
    impl OcrEngine for DummyEngine {
        fn name(&self) -> &str {
            "dummy"
        }
        fn recognize(
            &self,
            _image_data: &[u8],
            _width: u32,
            _height: u32,
            options: &OcrOptions,
        ) -> Result<Vec<OcrResult>, Box<dyn std::error::Error>> {
            Ok(vec![OcrResult {
                text: format!("lang={}", options.language),
                bbox: [0.0, 0.0, 10.0, 10.0],
                confidence: 0.9,
            }])
        }
    }

    #[test]
    fn test_engine_trait_object() {
        let engine: Box<dyn OcrEngine> = Box::new(DummyEngine);
        assert_eq!(engine.name(), "dummy");
        let opts = OcrOptions {
            language: "eng".into(),
        };
        let r = engine.recognize(&[], 1, 1, &opts).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].text, "lang=eng");
        assert_eq!(r[0].bbox, [0.0, 0.0, 10.0, 10.0]);
        assert!((r[0].confidence - 0.9).abs() < 1e-6);
    }
}
