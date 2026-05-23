use std::path::Path;

use liteparse::conversion::convert_data_to_pdf;
use liteparse::types::PdfInput;
use liteparse::{LiteParse, LiteParseConfig};
use serial_test::serial;

#[tokio::test]
#[serial]
async fn test_convert_data_to_pdf_integration() {
    let env_var = std::env::var("SKIP_INTEGRATION_TESTS");
    if let Ok(v) = env_var
        && v == "yes"
    {
        return;
    }
    let fixture_path = "../../integration_tests_data/receipt.png";
    let data = tokio::fs::read(fixture_path)
        .await
        .expect("Should be able to read file");
    let (converted, _temps) = convert_data_to_pdf(data, None)
        .await
        .expect("Should be able to convert data to PDF");
    assert!(Path::new(&converted.pdf_path).exists());
}

#[tokio::test]
#[serial]
async fn test_parse_bytes_image_integration() {
    let env_var = std::env::var("SKIP_INTEGRATION_TESTS");
    if let Ok(v) = env_var
        && v == "yes"
    {
        return;
    }
    let fixture_path = "../../integration_tests_data/receipt.png";
    let lit = LiteParse::new(LiteParseConfig::default());
    let data = tokio::fs::read(fixture_path)
        .await
        .expect("Should be able to read file");
    let input = PdfInput::Bytes(data);
    let parsed = lit
        .parse_input(input)
        .await
        .expect("Should be able to parse");
    assert_eq!(parsed.pages.len(), 1);
}

#[tokio::test]
#[serial]
async fn test_parse_bytes_office_integration() {
    let env_var = std::env::var("SKIP_INTEGRATION_TESTS");
    if let Ok(v) = env_var
        && v == "yes"
    {
        return;
    }
    let fixture_path = "../../integration_tests_data/sample3.doc";
    let lit = LiteParse::new(LiteParseConfig::default());
    let data = tokio::fs::read(fixture_path)
        .await
        .expect("Should be able to read file");
    let input = PdfInput::Bytes(data);
    let parsed = lit
        .parse_input(input)
        .await
        .expect("Should be able to parse");
    assert_eq!(parsed.pages.len(), 2);
}

#[tokio::test]
#[serial]
async fn test_parse_image_integration() {
    let env_var = std::env::var("SKIP_INTEGRATION_TESTS");
    if let Ok(v) = env_var
        && v == "yes"
    {
        return;
    }
    let lit = LiteParse::new(LiteParseConfig::default());
    let parsed = lit
        .parse("../../integration_tests_data/receipt.png")
        .await
        .expect("Should be able to parse");
    assert_eq!(parsed.pages.len(), 1);
}

#[tokio::test]
#[serial]
async fn test_parse_office_doc_integration() {
    let env_var = std::env::var("SKIP_INTEGRATION_TESTS");
    if let Ok(v) = env_var
        && v == "yes"
    {
        return;
    }
    let lit = LiteParse::new(LiteParseConfig::default());
    let parsed = lit
        .parse("../../integration_tests_data/sample3.doc")
        .await
        .expect("Should be able to parse");
    assert_eq!(parsed.pages.len(), 2);
}

#[tokio::test]
#[serial]
async fn test_parse_pdf_integration() {
    let lit = LiteParse::new(LiteParseConfig::default());
    let parsed = lit
        .parse("../../integration_tests_data/sample.pdf")
        .await
        .expect("Should be able to parse");
    assert_eq!(parsed.pages.len(), 1);
}

#[tokio::test]
#[serial]
async fn test_parse_bytes_pdf_integration() {
    let fixture_path = "../../integration_tests_data/sample.pdf";
    let lit = LiteParse::new(LiteParseConfig::default());
    let data = tokio::fs::read(fixture_path)
        .await
        .expect("Should be able to read file");
    let input = PdfInput::Bytes(data);
    let parsed = lit
        .parse_input(input)
        .await
        .expect("Should be able to parse");
    assert_eq!(parsed.pages.len(), 1);
}
