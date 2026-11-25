//! Plain-data geometry shared with the frontend.
//!
//! All rectangles are "logical points" unless the name says otherwise. Global
//! coordinates use the CoreGraphics convention: origin at the top-left corner
//! of the main display, y grows downwards.

use objc2_core_foundation::{CGRect, CGPoint, CGSize};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn from_cg(r: CGRect) -> Self {
        Self::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
    }

    pub fn to_cg(&self) -> CGRect {
        cg_rect(self.x, self.y, self.w, self.h)
    }

    pub fn right(&self) -> f64 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    pub fn intersection(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        (right > x && bottom > y).then(|| Rect::new(x, y, right - x, bottom - y))
    }

    pub fn is_empty(&self) -> bool {
        self.w < 1.0 || self.h < 1.0
    }

    /// Same rect with positive width/height (for drags that go right-to-left).
    pub fn normalized(self) -> Self {
        let (x, w) = if self.w < 0.0 { (self.x + self.w, -self.w) } else { (self.x, self.w) };
        let (y, h) = if self.h < 0.0 { (self.y + self.h, -self.h) } else { (self.y, self.h) };
        Rect::new(x, y, w, h)
    }
}

/// Build a CoreGraphics rect without depending on constructor helpers.
pub fn cg_rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect {
        origin: CGPoint { x, y },
        size: CGSize { width: w, height: h },
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorInfo {
    pub id: u32,
    /// Logical bounds in global coordinates.
    pub rect: Rect,
    /// Physical pixels per logical point.
    pub scale: f64,
    /// Native framebuffer size reported by the current display mode.
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub is_main: bool,
}

/// An on-screen window, in z-order (front first), local to a monitor.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WinRect {
    pub id: u32,
    pub layer: i32,
    pub pid: i32,
    pub rect: Rect,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersection_and_contains() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 5.0, 10.0, 10.0);
        assert_eq!(a.intersection(&b), Some(Rect::new(5.0, 5.0, 5.0, 5.0)));
        assert_eq!(a.intersection(&Rect::new(20.0, 20.0, 1.0, 1.0)), None);
        assert!(a.contains(0.0, 0.0) && !a.contains(10.0, 5.0));
    }

    #[test]
    fn normalizes_negative_drags() {
        let r = Rect::new(10.0, 10.0, -4.0, -6.0).normalized();
        assert_eq!(r, Rect::new(6.0, 4.0, 4.0, 6.0));
    }
}
