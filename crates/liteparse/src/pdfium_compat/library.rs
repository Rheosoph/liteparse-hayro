use super::document::Document;
use super::error::PdfiumError;

/// Entry point for opening documents.
///
/// PDFium is single-threaded behind a process-global lock, and its wrapper's
/// `Library` is the guard for that lock. hayro has no global state, so this
/// handle is free to create and hold; it exists so the `'lib` lifetimes that
/// thread through upstream liteparse keep their shape.
#[derive(Debug, Default)]
pub struct Library {
    _private: (),
}

impl Library {
    pub fn init() -> Library {
        Library { _private: () }
    }

    pub fn try_init() -> Result<Library, PdfiumError> {
        Ok(Self::init())
    }

    pub fn load_document(
        &self,
        path: &str,
        password: Option<&str>,
    ) -> Result<Document<'_>, PdfiumError> {
        let bytes = std::fs::read(path).map_err(|_| PdfiumError::FileNotFound)?;
        Document::open(bytes, password)
    }

    pub fn load_document_from_bytes(
        &self,
        data: &[u8],
        password: Option<&str>,
    ) -> Result<Document<'_>, PdfiumError> {
        Document::open(data.to_vec(), password)
    }
}
