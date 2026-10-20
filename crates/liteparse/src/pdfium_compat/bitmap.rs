use std::marker::PhantomData;

use super::library::Library;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitmapFormat {
    Unknown,
    Gray,
    Bgr,
    Bgrx,
    Bgra,
    BgraPremul,
}

impl BitmapFormat {
    pub fn bytes_per_pixel(self) -> Option<usize> {
        match self {
            BitmapFormat::Unknown => None,
            BitmapFormat::Gray => Some(1),
            BitmapFormat::Bgr => Some(3),
            BitmapFormat::Bgrx | BitmapFormat::Bgra | BitmapFormat::BgraPremul => Some(4),
        }
    }
}

/// An owned raster, stored as straight-alpha BGRA rows like PDFium's
/// `FPDFBitmap_BGRA` so [`Bitmap::buffer`] readers see the same layout.
pub struct Bitmap<'lib> {
    width: i32,
    height: i32,
    bgra: Vec<u8>,
    _lib: PhantomData<&'lib Library>,
}

impl<'lib> Bitmap<'lib> {
    pub(crate) fn from_rgba(width: i32, height: i32, rgba: Vec<u8>) -> Self {
        let mut bgra = rgba;
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        Self {
            width,
            height,
            bgra,
            _lib: PhantomData,
        }
    }

    /// From premultiplied RGBA (vello's pixmap layout).
    pub(crate) fn from_premultiplied(width: i32, height: i32, rgba: &[u8]) -> Self {
        let mut bgra = Vec::with_capacity(rgba.len());
        for px in rgba.chunks_exact(4) {
            let a = px[3];
            let unpremul = |c: u8| -> u8 {
                if a == 0 || a == 255 {
                    c
                } else {
                    ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8
                }
            };
            bgra.extend_from_slice(&[unpremul(px[2]), unpremul(px[1]), unpremul(px[0]), a]);
        }
        Self {
            width,
            height,
            bgra,
            _lib: PhantomData,
        }
    }

    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn stride(&self) -> i32 {
        self.width * 4
    }

    pub fn format(&self) -> BitmapFormat {
        BitmapFormat::Bgra
    }

    /// Fill a rectangle with an ARGB color.
    pub fn fill_rect(&mut self, left: i32, top: i32, width: i32, height: i32, color: u64) {
        let [_, _, _, _, a, r, g, b] = color.to_be_bytes();
        for y in top.max(0)..(top + height).min(self.height) {
            for x in left.max(0)..(left + width).min(self.width) {
                let i = ((y * self.width + x) * 4) as usize;
                self.bgra[i..i + 4].copy_from_slice(&[b, g, r, a]);
            }
        }
    }

    pub fn buffer(&self) -> &[u8] {
        &self.bgra
    }

    pub fn to_rgba(&self) -> Vec<u8> {
        let mut rgba = self.bgra.clone();
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        rgba
    }

    pub fn to_rgb(&self) -> Vec<u8> {
        let mut rgb = Vec::with_capacity(self.bgra.len() / 4 * 3);
        for px in self.bgra.chunks_exact(4) {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        rgb
    }

    pub fn to_luma(&self) -> Vec<u8> {
        self.bgra
            .chunks_exact(4)
            .map(|px| {
                let (b, g, r) = (px[0] as u32, px[1] as u32, px[2] as u32);
                ((77 * r + 150 * g + 29 * b) >> 8) as u8
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiplied_round_trip_for_opaque_pixels() {
        let bitmap = Bitmap::from_premultiplied(1, 1, &[10, 20, 30, 255]);
        assert_eq!(bitmap.to_rgba(), vec![10, 20, 30, 255]);
        assert_eq!(bitmap.to_rgb(), vec![10, 20, 30]);
        assert_eq!(bitmap.buffer(), &[30, 20, 10, 255]);
    }
}
