use crate::error::PdfiumError;

pub struct Bitmap {
    handle: pdfium_sys::FPDF_BITMAP,
}

impl Bitmap {
    /// Wrap an existing FPDF_BITMAP handle (takes ownership, will destroy on drop).
    ///
    /// # Safety
    /// The handle must be a valid, non-null bitmap that the caller owns.
    pub unsafe fn from_handle(handle: pdfium_sys::FPDF_BITMAP) -> Self {
        Bitmap { handle }
    }

    /// Create a new BGRA bitmap with the given dimensions.
    pub fn new(width: i32, height: i32) -> Result<Self, PdfiumError> {
        let handle = unsafe {
            pdfium_sys::FPDFBitmap_CreateEx(
                width,
                height,
                pdfium_sys::FPDFBitmap_BGRA as i32,
                std::ptr::null_mut(),
                0, // stride=0 lets pdfium choose
            )
        };
        if handle.is_null() {
            return Err(PdfiumError::OperationFailed);
        }
        Ok(Bitmap { handle })
    }

    pub fn handle(&self) -> pdfium_sys::FPDF_BITMAP {
        self.handle
    }

    pub fn width(&self) -> i32 {
        unsafe { pdfium_sys::FPDFBitmap_GetWidth(self.handle) }
    }

    pub fn height(&self) -> i32 {
        unsafe { pdfium_sys::FPDFBitmap_GetHeight(self.handle) }
    }

    pub fn stride(&self) -> i32 {
        unsafe { pdfium_sys::FPDFBitmap_GetStride(self.handle) }
    }

    /// Fill a rectangle with an ARGB color (0xAARRGGBB).
    pub fn fill_rect(&self, left: i32, top: i32, width: i32, height: i32, color: u64) {
        unsafe {
            pdfium_sys::FPDFBitmap_FillRect(
                self.handle,
                left,
                top,
                width,
                height,
                // necessary for windows -> expected `u32`, found `u64`
                #[allow(clippy::useless_conversion)]
                color.try_into().unwrap(),
            );
        }
    }

    /// Get the raw pixel buffer as a byte slice.
    /// Format is BGRA, row-major, with `stride()` bytes per row.
    pub fn buffer(&self) -> &[u8] {
        let ptr = unsafe { pdfium_sys::FPDFBitmap_GetBuffer(self.handle) };
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
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe { pdfium_sys::FPDFBitmap_Destroy(self.handle) };
    }
}
