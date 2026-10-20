use std::fmt;

/// Errors from the PDF backend, with PDFium's error vocabulary so callers
/// that match on it keep working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfiumError {
    Unknown,
    FileNotFound,
    InvalidFormat,
    PasswordRequired,
    UnsupportedSecurity,
    PageNotFound,
    OperationFailed,
    /// Never produced by the hayro backend (there is no shared library to
    /// load); kept so exhaustive matches written for PDFium still compile.
    LibraryUnavailable,
}

impl fmt::Display for PdfiumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PdfiumError::Unknown => write!(f, "unknown PDF error"),
            PdfiumError::FileNotFound => write!(f, "file not found"),
            PdfiumError::InvalidFormat => write!(f, "invalid PDF format"),
            PdfiumError::PasswordRequired => write!(f, "password required"),
            PdfiumError::UnsupportedSecurity => write!(f, "unsupported security handler"),
            PdfiumError::PageNotFound => write!(f, "page not found"),
            PdfiumError::OperationFailed => write!(f, "operation failed"),
            PdfiumError::LibraryUnavailable => write!(f, "PDF backend unavailable"),
        }
    }
}

impl std::error::Error for PdfiumError {}

impl From<hayro::hayro_syntax::LoadPdfError> for PdfiumError {
    fn from(error: hayro::hayro_syntax::LoadPdfError) -> Self {
        use hayro::hayro_syntax::{DecryptionError, LoadPdfError};
        match error {
            LoadPdfError::Invalid => PdfiumError::InvalidFormat,
            LoadPdfError::Decryption(DecryptionError::PasswordProtected) => {
                PdfiumError::PasswordRequired
            }
            LoadPdfError::Decryption(_) => PdfiumError::UnsupportedSecurity,
        }
    }
}
