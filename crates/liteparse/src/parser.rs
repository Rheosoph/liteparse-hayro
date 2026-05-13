use crate::config::{LiteParseConfig, OutputFormat, parse_target_pages};
use crate::conversion;
use crate::extract;
use crate::ocr::OcrEngine;
use crate::ocr::http_simple::HttpOcrEngine;
use crate::ocr::tesseract::TesseractOcrEngine;
use crate::ocr_merge;
use crate::output::{json, text};
use crate::projection;
use crate::types::ParsedPage;

/// Result of parsing a document.
pub struct ParseResult {
    /// Parsed pages with projected text layout.
    pub pages: Vec<ParsedPage>,
    /// Full document text, concatenated from all pages.
    pub text: String,
}

/// Main LiteParse orchestrator.
pub struct LiteParse {
    config: LiteParseConfig,
}

impl LiteParse {
    pub fn new(config: LiteParseConfig) -> Self {
        Self { config }
    }

    /// Parse a document file, returning structured results.
    pub async fn parse(&self, input: &str) -> Result<ParseResult, Box<dyn std::error::Error>> {
        let log = |msg: &str| {
            if !self.config.quiet {
                eprintln!("{}", msg);
            }
        };

        // Resolve input to a PDF path (convert if needed)
        let pdf_path = if conversion::is_pdf(input) {
            input.to_string()
        } else {
            conversion::convert_to_pdf(input, self.config.password.as_deref())
                .await?
                .pdf_path
        };

        let t0 = std::time::Instant::now();

        // Determine which pages to extract
        let target_pages = self
            .config
            .target_pages
            .as_ref()
            .map(|s| parse_target_pages(s))
            .transpose()
            .map_err(|e| format!("invalid --target-pages: {}", e))?;

        // Extract text items from PDF pages
        let mut pages = extract::extract_pages_filtered(
            &pdf_path,
            target_pages.as_deref(),
            self.config.max_pages,
            self.config.password.as_deref(),
        )?;
        let t1 = std::time::Instant::now();
        log(&format!(
            "[liteparse] extract: {:.1}ms ({} pages)",
            t1.duration_since(t0).as_secs_f64() * 1000.0,
            pages.len()
        ));

        // OCR pass
        if self.config.ocr_enabled {
            let engine: Box<dyn OcrEngine> = if self.config.ocr_server_url.is_none() {
                Box::new(TesseractOcrEngine::new(self.config.tessdata_path.clone()))
            } else {
                Box::new(HttpOcrEngine::new(
                    self.config.ocr_server_url.clone().unwrap(),
                ))
            };
            ocr_merge::ocr_and_merge_pages(
                &mut pages,
                &pdf_path,
                self.config.dpi,
                engine.as_ref(),
                &self.config.ocr_language,
            )?;
        }
        let t_ocr = std::time::Instant::now();
        log(&format!(
            "[liteparse] ocr: {:.1}ms",
            t_ocr.duration_since(t1).as_secs_f64() * 1000.0
        ));

        // Grid projection
        let parsed_pages = projection::project_pages_to_grid(pages);
        let t2 = std::time::Instant::now();
        log(&format!(
            "[liteparse] project: {:.1}ms",
            t2.duration_since(t_ocr).as_secs_f64() * 1000.0
        ));

        let full_text = parsed_pages
            .iter()
            .map(|p| p.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        let total = t2.duration_since(t0).as_secs_f64() * 1000.0;
        log(&format!("[liteparse] total: {:.1}ms", total));

        Ok(ParseResult {
            pages: parsed_pages,
            text: full_text,
        })
    }

    /// Format a parse result according to the configured output format.
    pub fn format(&self, result: &ParseResult) -> Result<String, Box<dyn std::error::Error>> {
        match self.config.output_format {
            OutputFormat::Json => Ok(json::format_json(&result.pages)?),
            OutputFormat::Text => Ok(text::format_text(&result.pages)),
        }
    }

    pub fn config(&self) -> &LiteParseConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ParsedPage;

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn test_new_stores_config() {
        let mut cfg = LiteParseConfig::default();
        cfg.ocr_enabled = false;
        cfg.max_pages = 7;
        let lp = LiteParse::new(cfg);
        assert!(!lp.config().ocr_enabled);
        assert_eq!(lp.config().max_pages, 7);
    }

    fn fake_result() -> ParseResult {
        ParseResult {
            pages: vec![ParsedPage {
                page_number: 1,
                page_width: 100.0,
                page_height: 200.0,
                text: "hello".into(),
                text_items: vec![],
            }],
            text: "hello".into(),
        }
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn test_format_json() {
        let mut cfg = LiteParseConfig::default();
        cfg.output_format = OutputFormat::Json;
        let lp = LiteParse::new(cfg);
        let s = lp.format(&fake_result()).unwrap();
        assert!(s.contains("\"page\""));
        assert!(s.contains("\"hello\""));
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn test_format_text() {
        let mut cfg = LiteParseConfig::default();
        cfg.output_format = OutputFormat::Text;
        let lp = LiteParse::new(cfg);
        let s = lp.format(&fake_result()).unwrap();
        assert!(s.contains("hello"));
    }
}
