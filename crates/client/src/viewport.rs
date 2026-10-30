//! Where the remote surface is drawn inside a local window, and the inverse mapping used for
//! pointer input. The frame is scaled to fit with its aspect ratio kept (letterboxed).

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    surface_w: f32,
    surface_h: f32,
}

impl Viewport {
    pub fn fit(surface_w: u32, surface_h: u32, window_w: u32, window_h: u32) -> Self {
        let (sw, sh) = (surface_w.max(1) as f32, surface_h.max(1) as f32);
        let (ww, wh) = (window_w.max(1) as f32, window_h.max(1) as f32);
        let scale = (ww / sw).min(wh / sh);
        // Pixel exact when sizes match, so no resampling blur in the common case.
        let (w, h) = if surface_w == window_w && surface_h == window_h { (sw, sh) } else { (sw * scale, sh * scale) };
        Self { x: ((ww - w) / 2.0).floor(), y: ((wh - h) / 2.0).floor(), w, h, surface_w: sw, surface_h: sh }
    }

    /// Window pixel position to remote surface pixels, clamped to the surface.
    pub fn to_surface(&self, wx: f64, wy: f64) -> (f32, f32) {
        let sx = (wx as f32 - self.x) * self.surface_w / self.w;
        let sy = (wy as f32 - self.y) * self.surface_h / self.h;
        (sx.clamp(0.0, self.surface_w - 1.0), sy.clamp(0.0, self.surface_h - 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_when_sizes_match() {
        let v = Viewport::fit(1280, 720, 1280, 720);
        assert_eq!((v.x, v.y, v.w, v.h), (0.0, 0.0, 1280.0, 720.0));
        assert_eq!(v.to_surface(100.0, 50.0), (100.0, 50.0));
    }

    #[test]
    fn letterbox_and_inverse() {
        // Window twice as wide: pillarboxed, scale 1.
        let v = Viewport::fit(1000, 500, 2000, 500);
        assert_eq!((v.x, v.w), (500.0, 1000.0));
        assert_eq!(v.to_surface(600.0, 100.0), (100.0, 100.0));
        // Half size window: scale 0.5.
        let v = Viewport::fit(1000, 500, 500, 250);
        assert_eq!(v.to_surface(250.0, 125.0), (500.0, 250.0));
        // Outside the picture clamps to the edge.
        let v = Viewport::fit(1000, 500, 2000, 500);
        assert_eq!(v.to_surface(0.0, 900.0), (0.0, 499.0));
    }
}
