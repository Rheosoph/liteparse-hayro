use std::fmt;

use crate::ffi;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfiumError {
    Unknown,
    FileNotFound,
    InvalidFormat,
    PasswordRequired,
    UnsupportedSecurity,
    PageNotFound,
    OperationFailed,
    /// The pdfium shared library could not be located or loaded.
    LibraryUnavailable,
}

impl PdfiumError {
    pub(crate) fn from_last_error() -> Self {
        let code = unsafe { ffi!(FPDF_GetLastError()) };
        match code as u32 {
            pdfium_sys::FPDF_ERR_FILE => PdfiumError::FileNotFound,
            pdfium_sys::FPDF_ERR_FORMAT => PdfiumError::InvalidFormat,
            pdfium_sys::FPDF_ERR_PASSWORD => PdfiumError::PasswordRequired,
            pdfium_sys::FPDF_ERR_SECURITY => PdfiumError::UnsupportedSecurity,
            pdfium_sys::FPDF_ERR_PAGE => PdfiumError::PageNotFound,
            _ => PdfiumError::Unknown,
        }
    }
}

impl fmt::Display for PdfiumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PdfiumError::Unknown => write!(f, "unknown pdfium error"),
            PdfiumError::FileNotFound => write!(f, "file not found"),
            PdfiumError::InvalidFormat => write!(f, "invalid PDF format"),
            PdfiumError::PasswordRequired => write!(f, "password required"),
            PdfiumError::UnsupportedSecurity => write!(f, "unsupported security handler"),
            PdfiumError::PageNotFound => write!(f, "page not found"),
            PdfiumError::OperationFailed => write!(f, "operation failed"),
            PdfiumError::LibraryUnavailable => write!(f, "pdfium shared library unavailable"),
        }
    }
}

impl std::error::Error for PdfiumError {}
