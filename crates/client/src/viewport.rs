//! Where the remote surface is drawn inside a local window, and the inverse mapping used for
//! pointer input. The frame is always drawn 1:1 from the top-left corner: any resampling
//! blurs text, and a size mismatch is short lived because the window snaps to the frame size.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Viewport {
    pub fn fit(surface_w: u32, surface_h: u32, _window_w: u32, _window_h: u32) -> Self {
        Self { x: 0.0, y: 0.0, w: surface_w.max(1) as f32, h: surface_h.max(1) as f32 }
    }

    /// Window pixel position to remote surface pixels, clamped to the surface.
    pub fn to_surface(&self, wx: f64, wy: f64) -> (f32, f32) {
        ((wx as f32 - self.x).clamp(0.0, self.w - 1.0), (wy as f32 - self.y).clamp(0.0, self.h - 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_pixel_exact_from_top_left() {
        for (ww, wh) in [(1280, 720), (2000, 500), (500, 250)] {
            let v = Viewport::fit(1280, 720, ww, wh);
            assert_eq!((v.x, v.y, v.w, v.h), (0.0, 0.0, 1280.0, 720.0));
        }
        let v = Viewport::fit(1000, 500, 600, 300);
        assert_eq!(v.to_surface(100.0, 50.0), (100.0, 50.0));
        assert_eq!(v.to_surface(-3.0, 900.0), (0.0, 499.0));
    }
}
