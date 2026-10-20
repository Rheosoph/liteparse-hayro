use std::ffi::CString;
use std::sync::Once;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::document::Document;
use crate::error::PdfiumError;
use crate::ffi;

static INIT: Once = Once::new();

/// Process-global PDFium serialization lock.
///
/// PDFium's FFI is **not thread-safe**: concurrent calls (even across distinct
/// documents) corrupt internal state and cause heap UB (double-free / heap
/// corruption). Every [`Library`] handle holds this mutex for its entire
/// lifetime, and the owning PDFium resources ([`Document`], `Page`,
/// `TextPage`, `Bitmap`) borrow from a [`Library`] via their `'lib` lifetime,
/// so the borrow checker statically prevents PDFium work outside the lock.
/// (`Font` is a borrowed, non-owning handle constructed through an `unsafe`
/// fn; its lock discipline is the caller's responsibility, not statically
/// enforced.)
#[cfg(not(target_arch = "wasm32"))]
fn pdfium_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// A live, locked PDFium session.
///
/// Holding a `Library` proves the current thread has exclusive,
/// process-wide access to PDFium. All PDFium resources ([`Document`] etc.)
/// borrow from this handle, which makes it impossible to call into PDFium
/// without first acquiring the lock.
///
/// `Library` is intentionally **not `Clone`**. To use PDFium from a
/// different scope, call [`Library::init`] again — this will block until
/// any other in-flight PDFium work has finished.
///
/// On `wasm32` there is no threading, so the lock is elided.
///
/// The snippet below must fail to compile — a `Document` cannot outlive
/// the `Library` that opened it:
///
/// ```compile_fail
/// use liteparse_pdfium::{Library, Document};
/// let doc: Document<'static> = {
///     let lib = Library::init();
///     lib.load_document("x.pdf", None).unwrap()
/// };
/// // `lib` was dropped above — using `doc` here is a use-after-unlock.
/// let _ = doc.page_count();
/// ```
pub struct Library {
    #[cfg(not(target_arch = "wasm32"))]
    _guard: MutexGuard<'static, ()>,
    #[cfg(target_arch = "wasm32")]
    _private: (),
}

impl Library {
    /// Acquire the process-wide PDFium lock, blocking the current thread
    /// until any other in-flight PDFium work has finished. Initializes the
    /// library on first call.
    ///
    /// Multiple concurrent callers are serialized; only one `Library`
    /// instance exists at a time.
    pub fn init() -> Library {
        Self::try_init().expect("failed to load pdfium shared library")
    }

    /// [`Library::init`] that reports a missing or unloadable pdfium shared
    /// library as [`PdfiumError::LibraryUnavailable`] instead of panicking.
    /// Hosts that cannot afford a panic across an FFI boundary (a Node addon,
    /// where an escaping panic aborts the process) should call this first;
    /// the search path is described on `pdfium_sys::dynamic::load_default`.
    pub fn try_init() -> Result<Library, PdfiumError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            pdfium_sys::dynamic::load_default().map_err(|_| PdfiumError::LibraryUnavailable)?;
            // Recover from poisoning: a panic mid-FFI may leave PDFium in
            // an odd state, but subsequent calls should still be allowed
            // (the worst case is that the next parse also fails cleanly).
            let guard = pdfium_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            INIT.call_once(|| unsafe { ffi!(FPDF_InitLibrary()) });
            Ok(Library { _guard: guard })
        }
        #[cfg(target_arch = "wasm32")]
        {
            INIT.call_once(|| unsafe { ffi!(FPDF_InitLibrary()) });
            Ok(Library { _private: () })
        }
    }

    pub fn load_document(
        &self,
        path: &str,
        password: Option<&str>,
    ) -> Result<Document<'_>, PdfiumError> {
        let c_path = CString::new(path).map_err(|_| PdfiumError::FileNotFound)?;
        let c_password = password
            .map(|p| CString::new(p).map_err(|_| PdfiumError::OperationFailed))
            .transpose()?;

        let handle = unsafe {
            ffi!(FPDF_LoadDocument(
                c_path.as_ptr(),
                c_password.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
            ))
        };

        if handle.is_null() {
            return Err(PdfiumError::from_last_error());
        }

        // PDFium ignores `/UserUnit`, so recover it from the raw bytes (see
        // `crate::user_unit`) when the fork's dict-reading export isn't
        // available. The chunked containment probe keeps the common
        // no-UserUnit case to a single streaming pass with no whole-file
        // allocation.
        let page_user_units = if !Self::user_unit_api_available()
            && crate::user_unit::file_mentions_user_unit(path)
        {
            std::fs::read(path)
                .map(|bytes| Self::page_user_units(handle, &bytes))
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        Ok(Document {
            handle,
            page_user_units,
            _lib: std::marker::PhantomData,
        })
    }

    pub fn load_document_from_bytes(
        &self,
        data: &[u8],
        password: Option<&str>,
    ) -> Result<Document<'_>, PdfiumError> {
        let c_password = password
            .map(|p| CString::new(p).map_err(|_| PdfiumError::OperationFailed))
            .transpose()?;

        let handle = unsafe {
            ffi!(FPDF_LoadMemDocument(
                data.as_ptr() as *const std::ffi::c_void,
                data.len() as i32,
                c_password.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
            ))
        };

        if handle.is_null() {
            return Err(PdfiumError::from_last_error());
        }

        // SAFETY: pdfium requires the data buffer to outlive the document.
        // The caller must ensure `data` lives long enough. For owned data,
        // consider passing a Vec and having the Document hold it.
        // For now, this is the caller's responsibility.
        Ok(Document {
            handle,
            page_user_units: if Self::user_unit_api_available() {
                Vec::new()
            } else {
                Self::page_user_units(handle, data)
            },
            _lib: std::marker::PhantomData,
        })
    }

    /// Whether the loaded pdfium binary exports the fork's
    /// `FPDFPage_GetUserUnit`. When it does, `Document::page` reads
    /// `/UserUnit` through it and the byte-scan table is skipped entirely.
    fn user_unit_api_available() -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            pdfium_sys::dynamic::pdfium().FPDFPage_GetUserUnit.is_some()
        }
        // Statically linked on wasm; a pinned release without the export
        // would fail at link time, so present-at-runtime is guaranteed.
        #[cfg(target_arch = "wasm32")]
        {
            true
        }
    }

    /// Build the per-page `/UserUnit` table by matching scanned page objects
    /// against PDFium's reported page sizes (see `crate::user_unit`).
    fn page_user_units(handle: pdfium_sys::FPDF_DOCUMENT, data: &[u8]) -> Vec<f32> {
        let entries = crate::user_unit::scan_user_units(data);
        if entries.is_empty() {
            return Vec::new();
        }
        let page_count = unsafe { ffi!(FPDF_GetPageCount(handle)) };
        (0..page_count.max(0))
            .map(|index| {
                let mut size = pdfium_sys::FS_SIZEF {
                    width: 0.0,
                    height: 0.0,
                };
                let ok = unsafe { ffi!(FPDF_GetPageSizeByIndexF(handle, index, &mut size)) };
                if ok != 0 {
                    crate::user_unit::match_user_unit(&entries, size.width, size.height)
                } else {
                    1.0
                }
            })
            .collect()
    }
}
