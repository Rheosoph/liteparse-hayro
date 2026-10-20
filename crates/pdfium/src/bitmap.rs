use std::marker::PhantomData;

use crate::error::PdfiumError;
use crate::ffi;
use crate::library::Library;

/// Pixel layout of a [`Bitmap`], as `FPDFBitmap_GetFormat` reports it. Bitmaps this
/// crate creates are BGRA unless [`Bitmap::new_with_format`] says otherwise; bitmaps
/// pdfium hands back (an embedded image's own pixels, for instance) can be any of
/// these, and `buffer()` has to be read with the matching bytes per pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitmapFormat {
    Unknown,
    /// One byte per pixel.
    Gray,
    /// Three bytes per pixel, blue first.
    Bgr,
    /// Four bytes per pixel, the fourth unused.
    Bgrx,
    /// Four bytes per pixel with straight alpha.
    Bgra,
    /// Four bytes per pixel with premultiplied alpha.
    BgraPremul,
}

impl BitmapFormat {
    /// Bytes each pixel occupies in the buffer, `None` for [`BitmapFormat::Unknown`].
    pub fn bytes_per_pixel(self) -> Option<usize> {
        match self {
            BitmapFormat::Unknown => None,
            BitmapFormat::Gray => Some(1),
            BitmapFormat::Bgr => Some(3),
            BitmapFormat::Bgrx | BitmapFormat::Bgra | BitmapFormat::BgraPremul => Some(4),
        }
    }
}

/// A BGRA pixel buffer owned by PDFium.
///
/// The `'lib` lifetime ties the bitmap to a held [`Library`] lock, so it
/// cannot be created (or destroyed via `Drop`) outside the PDFium critical
/// section.
pub struct Bitmap<'lib> {
    handle: pdfium_sys::FPDF_BITMAP,
    _lib: PhantomData<&'lib Library>,
}

impl<'lib> Bitmap<'lib> {
    /// Wrap an existing FPDF_BITMAP handle (takes ownership, will destroy on drop).
    ///
    /// # Safety
    /// The handle must be a valid, non-null bitmap that the caller owns,
    /// and the caller must hold a [`Library`] for at least `'lib`.
    pub unsafe fn from_handle(handle: pdfium_sys::FPDF_BITMAP) -> Self {
        Bitmap {
            handle,
            _lib: PhantomData,
        }
    }

    /// Create a new BGRA bitmap with the given dimensions.
    ///
    /// # Safety
    /// The caller must hold a [`Library`] for at least `'lib` (PDFium FFI is
    /// not thread-safe). `'lib` is not constrained by an argument, so callers
    /// must ensure it cannot outlive the held lock — usually by inferring it
    /// from the call site (e.g. returning a `Bitmap<'lib>` from a method on
    /// `Page<'_, 'lib>`, whose existence already proves the lock is held).
    pub unsafe fn new(width: i32, height: i32) -> Result<Self, PdfiumError> {
        unsafe { Self::new_with_format(width, height, pdfium_sys::FPDFBitmap_BGRA as i32) }
    }

    /// Like [`Self::new`] with an explicit pdfium pixel format (`FPDFBitmap_BGR`,
    /// `FPDFBitmap_BGRA`, `FPDFBitmap_BGRx`, `FPDFBitmap_Gray`). pdfium composites
    /// 24-bit and 32-bit targets through different paths, so a consumer matching
    /// another renderer's pixels byte for byte has to pick the same format it used.
    /// `buffer()`/`stride()` follow the format: 3 bytes per pixel for BGR.
    pub unsafe fn new_with_format(
        width: i32,
        height: i32,
        format: i32,
    ) -> Result<Self, PdfiumError> {
        let handle = unsafe {
            ffi!(FPDFBitmap_CreateEx(
                width,
                height,
                format,
                std::ptr::null_mut(),
                0, // stride=0 lets pdfium choose
            ))
        };
        if handle.is_null() {
            return Err(PdfiumError::OperationFailed);
        }
        Ok(Bitmap {
            handle,
            _lib: PhantomData,
        })
    }

    pub fn handle(&self) -> pdfium_sys::FPDF_BITMAP {
        self.handle
    }

    pub fn width(&self) -> i32 {
        unsafe { ffi!(FPDFBitmap_GetWidth(self.handle)) }
    }

    pub fn height(&self) -> i32 {
        unsafe { ffi!(FPDFBitmap_GetHeight(self.handle)) }
    }

    pub fn stride(&self) -> i32 {
        unsafe { ffi!(FPDFBitmap_GetStride(self.handle)) }
    }

    /// The pixel layout (`FPDFBitmap_GetFormat`).
    pub fn format(&self) -> BitmapFormat {
        match unsafe { ffi!(FPDFBitmap_GetFormat(self.handle)) } as u32 {
            pdfium_sys::FPDFBitmap_Gray => BitmapFormat::Gray,
            pdfium_sys::FPDFBitmap_BGR => BitmapFormat::Bgr,
            pdfium_sys::FPDFBitmap_BGRx => BitmapFormat::Bgrx,
            pdfium_sys::FPDFBitmap_BGRA => BitmapFormat::Bgra,
            pdfium_sys::FPDFBitmap_BGRA_Premul => BitmapFormat::BgraPremul,
            _ => BitmapFormat::Unknown,
        }
    }

    /// Fill a rectangle with an ARGB color (0xAARRGGBB).
    pub fn fill_rect(&self, left: i32, top: i32, width: i32, height: i32, color: u64) {
        unsafe {
            ffi!(FPDFBitmap_FillRect(
                self.handle,
                left,
                top,
                width,
                height,
                // necessary for windows -> expected `u32`, found `u64`
                #[allow(clippy::useless_conversion)]
                color.try_into().unwrap(),
            ));
        }
    }

    /// Get the raw pixel buffer as a byte slice.
    /// Format is BGRA, row-major, with `stride()` bytes per row.
    pub fn buffer(&self) -> &[u8] {
        let ptr = unsafe { ffi!(FPDFBitmap_GetBuffer(self.handle)) };
        let len = (self.stride() * self.height()) as usize;
        unsafe { std::slice::from_raw_parts(ptr as *const u8, len) }
    }

    /// Convert the BGRA buffer to RGBA in a new Vec.
    pub fn to_rgba(&self) -> Vec<u8> {
        let width = self.width() as usize;
        let height = self.height() as usize;
        let stride = self.stride() as usize;
        let src = self.buffer();
        let mut rgba = Vec::with_capacity(width * height * 4);

        for y in 0..height {
            let row = &src[y * stride..y * stride + width * 4];
            for pixel in row.chunks_exact(4) {
                // BGRA -> RGBA
                rgba.push(pixel[2]); // R
                rgba.push(pixel[1]); // G
                rgba.push(pixel[0]); // B
                rgba.push(pixel[3]); // A
            }
        }

        rgba
    }

    /// Convert the BGRA buffer to tightly-packed RGB in a new Vec, dropping the
    /// alpha channel (pages render onto opaque white, so alpha is constant 255).
    pub fn to_rgb(&self) -> Vec<u8> {
        let width = self.width() as usize;
        let height = self.height() as usize;
        let stride = self.stride() as usize;
        let src = self.buffer();
        let mut rgb = Vec::with_capacity(width * height * 3);

        for y in 0..height {
            let row = &src[y * stride..y * stride + width * 4];
            for pixel in row.chunks_exact(4) {
                // BGRA -> RGB (drop A)
                rgb.push(pixel[2]); // R
                rgb.push(pixel[1]); // G
                rgb.push(pixel[0]); // B
            }
        }

        rgb
    }

    /// Convert the BGRA buffer to tightly-packed 8-bit grayscale (1 byte/px)
    /// using Rec. 601 luma weights.
    pub fn to_luma(&self) -> Vec<u8> {
        let width = self.width() as usize;
        let height = self.height() as usize;
        let stride = self.stride() as usize;
        let src = self.buffer();
        let mut luma = Vec::with_capacity(width * height);

        for y in 0..height {
            let row = &src[y * stride..y * stride + width * 4];
            for pixel in row.chunks_exact(4) {
                let (b, g, r) = (pixel[0] as u32, pixel[1] as u32, pixel[2] as u32);
                luma.push(((77 * r + 150 * g + 29 * b) >> 8) as u8);
            }
        }

        luma
    }
}

impl Drop for Bitmap<'_> {
    fn drop(&mut self) {
        unsafe { ffi!(FPDFBitmap_Destroy(self.handle)) };
    }
}
