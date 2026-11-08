//! Points and rectangles in pixel coordinates (`i32`: no casts in user code, no overflow for any
//! display this crate can drive).

/// A pixel position: `x` grows to the right, `y` downwards, `(0, 0)` is the top left corner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// An axis-aligned rectangle: top left corner `(x, y)`, `w` pixels wide and `h` pixels tall. A
/// rectangle with `w <= 0` or `h <= 0` is empty (covers no pixel).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    /// The rectangle spanning two corners, both inclusive, in any order.
    pub const fn from_corners(a: Point, b: Point) -> Self {
        let (x0, x1) = if a.x <= b.x { (a.x, b.x) } else { (b.x, a.x) };
        let (y0, y1) = if a.y <= b.y { (a.y, b.y) } else { (b.y, a.y) };
        Self::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1)
    }

    /// One past the rightmost column.
    pub const fn right(&self) -> i32 {
        self.x + self.w
    }

    /// One past the bottom row.
    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub const fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    pub const fn contains(&self, p: Point) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }

    /// The overlap of two rectangles (empty when they don't overlap).
    pub fn intersect(&self, other: &Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        Rect::new(x, y, (r - x).max(0), (b - y).max(0))
    }

    /// Shrink by `d` pixels on every side (grow with a negative `d`).
    pub const fn inset(&self, d: i32) -> Rect {
        Rect::new(self.x + d, self.y + d, self.w - 2 * d, self.h - 2 * d)
    }

    /// The same size, moved by `(dx, dy)`.
    pub const fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    /// The center pixel (rounded towards the top left).
    pub const fn center(&self) -> Point {
        Point::new(self.x + self.w / 2, self.y + self.h / 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_and_contains() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, -3, 10, 6);
        assert_eq!(a.intersect(&b), Rect::new(5, 0, 5, 3));
        assert!(a.intersect(&Rect::new(20, 20, 2, 2)).is_empty());
        assert!(a.contains(Point::new(9, 9)));
        assert!(!a.contains(Point::new(10, 9)));
        assert_eq!(
            Rect::from_corners(Point::new(4, 7), Point::new(1, 2)),
            Rect::new(1, 2, 4, 6)
        );
    }
}
