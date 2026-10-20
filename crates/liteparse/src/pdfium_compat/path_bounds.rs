//! `CPDF_PathObject::CalcBoundingBox`: the bounds PDFium reports for a path
//! object, computed in the path's own space and then transformed.
//!
//! Unstroked paths use the box of all their points (control points
//! included). Stroked paths use PDFium's outline estimate, which widens
//! every segment end and join by the *full* line width; a zero-width
//! (hairline) stroke instead pads the transformed box by half a unit.

use hayro::kurbo::{Affine, BezPath, PathEl, Point, Rect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Move,
    Line,
    Bezier,
}

/// A `CFX_Path::Point`.
#[derive(Debug, Clone, Copy)]
struct PathPoint {
    x: f32,
    y: f32,
    kind: Kind,
    close: bool,
}

/// A `CFX_FloatRect` being grown by `UpdateRect`.
struct Bounds {
    left: f32,
    bottom: f32,
    right: f32,
    top: f32,
}

impl Bounds {
    fn update(&mut self, x: f32, y: f32) {
        self.left = self.left.min(x);
        self.bottom = self.bottom.min(y);
        self.right = self.right.max(x);
        self.top = self.top.max(y);
    }

    /// `CFX_Matrix::TransformRect`, in single precision.
    fn transform(&self, m: Affine) -> Bounds {
        let [a, b, c, d, e, f] = m.as_coeffs().map(|v| v as f32);
        let corners = [
            (self.left, self.top),
            (self.left, self.bottom),
            (self.right, self.top),
            (self.right, self.bottom),
        ]
        .map(|(x, y)| (a * x + c * y + e, b * x + d * y + f));
        let mut out = Bounds {
            left: corners[0].0,
            bottom: corners[0].1,
            right: corners[0].0,
            top: corners[0].1,
        };
        for (x, y) in &corners[1..] {
            out.update(*x, *y);
        }
        out
    }

    fn to_rect(&self) -> Rect {
        Rect::new(
            self.left as f64,
            self.bottom as f64,
            self.right as f64,
            self.top as f64,
        )
    }
}

/// The bounds of `path` (in the space it was drawn in) painted with `ctm`,
/// filled and/or stroked with `line_width` (in that same space).
pub(crate) fn path_object_bounds(
    path: &BezPath,
    ctm: Affine,
    stroked: bool,
    line_width: f32,
) -> Rect {
    let points = path_points(path);
    if points.is_empty() {
        return Rect::ZERO;
    }
    let local = if stroked && line_width != 0.0 {
        stroke_bounds(&points, line_width)
    } else {
        let mut bounds = Bounds {
            left: points[0].x,
            bottom: points[0].y,
            right: points[0].x,
            top: points[0].y,
        };
        for p in &points[1..] {
            bounds.update(p.x, p.y);
        }
        bounds
    };
    let mut rect = local.transform(ctm);
    if stroked && line_width == 0.0 {
        rect.left -= 0.5;
        rect.bottom -= 0.5;
        rect.right += 0.5;
        rect.top += 0.5;
    }
    rect.to_rect()
}

/// PDFium's point list for a path: curves as three points, a closing
/// `h` as a line back to the subpath start when not already there.
fn path_points(path: &BezPath) -> Vec<PathPoint> {
    let point = |p: Point, kind| PathPoint {
        x: p.x as f32,
        y: p.y as f32,
        kind,
        close: false,
    };
    let mut points: Vec<PathPoint> = Vec::new();
    let mut start = Point::ZERO;
    let mut current = Point::ZERO;
    for el in path.elements() {
        match *el {
            PathEl::MoveTo(p) => {
                points.push(point(p, Kind::Move));
                start = p;
                current = p;
            }
            PathEl::LineTo(p) => {
                points.push(point(p, Kind::Line));
                current = p;
            }
            PathEl::QuadTo(c, p) => {
                let c1 = current + (c - current) * (2.0 / 3.0);
                let c2 = p + (c - p) * (2.0 / 3.0);
                points.extend([c1, c2, p].map(|q| point(q, Kind::Bezier)));
                current = p;
            }
            PathEl::CurveTo(c1, c2, p) => {
                points.extend([c1, c2, p].map(|q| point(q, Kind::Bezier)));
                current = p;
            }
            PathEl::ClosePath => {
                if current != start {
                    points.push(point(start, Kind::Line));
                    current = start;
                }
                if let Some(last) = points.last_mut() {
                    last.close = true;
                }
            }
        }
    }
    points
}

/// `CFX_Path::GetBoundingBoxForStrokePath`.
fn stroke_bounds(points: &[PathPoint], line_width: f32) -> Bounds {
    let mut rect = Bounds {
        left: 100000.0,
        bottom: 100000.0,
        right: -100000.0,
        top: -100000.0,
    };
    let half_width = line_width;
    let mut i = 0;
    while i < points.len() {
        // (start, middle, end), the middle only for a join.
        let (start, middle, end) = if points[i].kind == Kind::Move {
            if i + 1 == points.len() {
                if points[i].close {
                    rect.update(points[i].x, points[i].y);
                }
                break;
            }
            (i + 1, None, i)
        } else {
            if points[i].kind == Kind::Bezier && !points[i].close {
                if i + 2 >= points.len() {
                    break;
                }
                rect.update(points[i].x, points[i].y);
                rect.update(points[i + 1].x, points[i + 1].y);
                i += 2;
            }
            if i + 1 == points.len() || points[i + 1].kind == Kind::Move {
                (i - 1, None, i)
            } else {
                (i - 1, Some(i), i + 1)
            }
        };
        let p = |k: usize| (points[k].x, points[k].y);
        match middle {
            Some(middle) => {
                update_line_join_points(&mut rect, p(start), p(middle), p(end), half_width)
            }
            None => update_line_end_points(&mut rect, p(start), p(end), half_width),
        }
        i += 1;
    }
    rect
}

fn update_line_end_points(rect: &mut Bounds, start: (f32, f32), end: (f32, f32), hw: f32) {
    if start.0 == end.0 {
        if start.1 == end.1 {
            rect.update(end.0 + hw, end.1 + hw);
            rect.update(end.0 - hw, end.1 - hw);
            return;
        }
        let y = if end.1 < start.1 {
            end.1 - hw
        } else {
            end.1 + hw
        };
        rect.update(end.0 + hw, y);
        rect.update(end.0 - hw, y);
        return;
    }
    if start.1 == end.1 {
        let x = if end.0 < start.0 {
            end.0 - hw
        } else {
            end.0 + hw
        };
        rect.update(x, end.1 + hw);
        rect.update(x, end.1 - hw);
        return;
    }
    let (dx, dy) = (end.0 - start.0, end.1 - start.1);
    let ll = dx.hypot(dy);
    let mx = end.0 + hw * dx / ll;
    let my = end.1 + hw * dy / ll;
    let dx1 = hw * dy / ll;
    let dy1 = hw * dx / ll;
    rect.update(mx - dx1, my + dy1);
    rect.update(mx + dx1, my - dy1);
}

fn update_line_join_points(
    rect: &mut Bounds,
    start: (f32, f32),
    mid: (f32, f32),
    end: (f32, f32),
    half_width: f32,
) {
    let one_twentieth = 1.0_f32 / 20.0;
    let (mut start_k, mut start_c, mut start_dc) = (0.0_f32, 0.0_f32, 0.0_f32);
    let (mut end_k, mut end_c, mut end_dc) = (0.0_f32, 0.0_f32, 0.0_f32);
    let start_vert = (start.0 - mid.0).abs() < one_twentieth;
    let end_vert = (mid.0 - end.0).abs() < one_twentieth;
    if start_vert && end_vert {
        let dir = if mid.1 > start.1 { 1.0 } else { -1.0 };
        let y = mid.1 + half_width * dir;
        rect.update(mid.0 + half_width, y);
        rect.update(mid.0 - half_width, y);
        return;
    }
    if !start_vert {
        let (sx, sy) = (start.0 - mid.0, start.1 - mid.1);
        start_k = (mid.1 - start.1) / (mid.0 - start.0);
        start_c = mid.1 - start_k * mid.0;
        let start_len = sx.hypot(sy);
        start_dc = (half_width * start_len / sx).abs();
    }
    if !end_vert {
        let (ex, ey) = (end.0 - mid.0, end.1 - mid.1);
        end_k = ey / ex;
        end_c = mid.1 - end_k * mid.0;
        let end_len = ex.hypot(ey);
        end_dc = (half_width * end_len / ex).abs();
    }
    if start_vert {
        let mut x = start.0;
        if end.0 < start.0 {
            x += half_width;
        } else {
            x -= half_width;
        }
        let y = if start.1 < end_k * start.0 + end_c {
            end_k * x + end_c + end_dc
        } else {
            end_k * x + end_c - end_dc
        };
        rect.update(x, y);
        return;
    }
    if end_vert {
        let mut x = end.0;
        if start.0 < end.0 {
            x += half_width;
        } else {
            x -= half_width;
        }
        let y = if end.1 < start_k * end.0 + start_c {
            start_k * x + start_c + start_dc
        } else {
            start_k * x + start_c - start_dc
        };
        rect.update(x, y);
        return;
    }
    if (start_k - end_k).abs() < one_twentieth {
        let start_dir = if mid.0 > start.0 { 1 } else { -1 };
        let end_dir = if end.0 > mid.0 { 1 } else { -1 };
        if start_dir == end_dir {
            update_line_end_points(rect, mid, end, half_width);
        } else {
            update_line_end_points(rect, start, mid, half_width);
        }
        return;
    }
    let start_outside_c = if end.1 < start_k * end.0 + start_c {
        start_c + start_dc
    } else {
        start_c - start_dc
    };
    let end_outside_c = if start.1 < end_k * start.0 + end_c {
        end_c + end_dc
    } else {
        end_c - end_dc
    };
    let join_x = (end_outside_c - start_outside_c) / (start_k - end_k);
    let join_y = start_k * join_x + start_outside_c;
    rect.update(join_x, join_y);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hairline_strokes_pad_half_a_unit() {
        let mut path = BezPath::new();
        path.move_to((10.0, 20.0));
        path.line_to((110.0, 20.0));
        let rect = path_object_bounds(&path, Affine::IDENTITY, true, 0.0);
        assert_eq!(rect, Rect::new(9.5, 19.5, 110.5, 20.5));
    }

    #[test]
    fn strokes_widen_by_the_full_line_width() {
        let mut path = BezPath::new();
        path.move_to((10.0, 20.0));
        path.line_to((110.0, 20.0));
        let rect = path_object_bounds(&path, Affine::IDENTITY, true, 2.0);
        assert_eq!(rect, Rect::new(8.0, 18.0, 112.0, 22.0));
    }

    #[test]
    fn fills_use_the_control_box() {
        let mut path = BezPath::new();
        path.move_to((0.0, 0.0));
        path.curve_to((0.0, 10.0), (10.0, 10.0), (10.0, 0.0));
        path.close_path();
        let rect = path_object_bounds(&path, Affine::scale(2.0), false, 1.0);
        assert_eq!(rect, Rect::new(0.0, 0.0, 20.0, 20.0));
    }
}
