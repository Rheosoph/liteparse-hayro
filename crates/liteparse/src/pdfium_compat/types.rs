//! Plain geometry and color types shared by the compat API.

/// An axis-aligned rectangle. In page space `top > bottom` (y-up); in
/// viewport space `top < bottom` (y-down), exactly as PDFium reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RectF {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// A character's tight box in page space, in PDFium's double precision.
#[derive(Debug, Clone, Copy, Default)]
pub struct CharBox {
    pub left: f64,
    pub right: f64,
    pub bottom: f64,
    pub top: f64,
}

/// A 2D affine matrix: `x' = a·x + c·y + e`, `y' = b·x + d·y + f`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Matrix {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

/// An 8-bit RGBA color.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// A text-segment rectangle in page space.
#[derive(Debug, Clone, Copy, Default)]
pub struct TextRect {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

impl Matrix {
    pub(crate) fn from_affine(affine: hayro::kurbo::Affine) -> Self {
        let [a, b, c, d, e, f] = affine.as_coeffs();
        Self {
            a: a as f32,
            b: b as f32,
            c: c as f32,
            d: d as f32,
            e: e as f32,
            f: f as f32,
        }
    }
}

impl RectF {
    /// Page-space (y-up) rectangle from a kurbo rect.
    pub(crate) fn from_kurbo_page(rect: hayro::kurbo::Rect) -> Self {
        Self {
            left: rect.x0.min(rect.x1) as f32,
            right: rect.x0.max(rect.x1) as f32,
            bottom: rect.y0.min(rect.y1) as f32,
            top: rect.y0.max(rect.y1) as f32,
        }
    }
}
