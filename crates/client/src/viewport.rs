//! Where the remote surface is drawn inside a local window, and the inverse mapping used for
//! pointer input. Windows are drawn 1:1 from the top-left corner: any resampling blurs text,
//! and a size mismatch is short lived because the window snaps to the frame size. A desktop
//! cannot snap the window to a whole Mac display, so it is scaled to fit instead.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Window pixels per surface pixel.
    scale: f32,
    surface_w: f32,
    surface_h: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// Pixel exact from the top-left; crops or pads.
    #[default]
    Exact,
    /// Scaled to fit with aspect kept, centered; 1:1 when sizes match.
    Scale,
}

impl Viewport {
    pub fn new(fit: Fit, surface_w: u32, surface_h: u32, window_w: u32, window_h: u32) -> Self {
        let (sw, sh) = (surface_w.max(1) as f32, surface_h.max(1) as f32);
        let (ww, wh) = (window_w.max(1) as f32, window_h.max(1) as f32);
        let scale = match fit {
            Fit::Exact => 1.0,
            Fit::Scale if (surface_w, surface_h) == (window_w, window_h) => 1.0,
            Fit::Scale => (ww / sw).min(wh / sh),
        };
        let (w, h) = (sw * scale, sh * scale);
        let (x, y) = match fit {
            Fit::Exact => (0.0, 0.0),
            Fit::Scale => (((ww - w) / 2.0).floor(), ((wh - h) / 2.0).floor()),
        };
        Self {
            x,
            y,
            w,
            h,
            scale,
            surface_w: sw,
            surface_h: sh,
        }
    }

    /// Window pixel position to remote surface pixels, clamped to the surface.
    pub fn to_surface(&self, wx: f64, wy: f64) -> (f32, f32) {
        let sx = (wx as f32 - self.x) / self.scale;
        let sy = (wy as f32 - self.y) / self.scale;
        (
            sx.clamp(0.0, self.surface_w - 1.0),
            sy.clamp(0.0, self.surface_h - 1.0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_is_pixel_exact_from_top_left() {
        for (ww, wh) in [(1280, 720), (2000, 500), (500, 250)] {
            let v = Viewport::new(Fit::Exact, 1280, 720, ww, wh);
            assert_eq!((v.x, v.y, v.w, v.h), (0.0, 0.0, 1280.0, 720.0));
        }
        let v = Viewport::new(Fit::Exact, 1000, 500, 600, 300);
        assert_eq!(v.to_surface(100.0, 50.0), (100.0, 50.0));
        assert_eq!(v.to_surface(-3.0, 900.0), (0.0, 499.0));
    }

    #[test]
    fn scale_fits_with_aspect_and_inverts() {
        // Same size: identity.
        let v = Viewport::new(Fit::Scale, 1920, 1080, 1920, 1080);
        assert_eq!((v.x, v.y, v.w, v.h), (0.0, 0.0, 1920.0, 1080.0));
        // 4K into 1920x1200: half size, letterboxed vertically.
        let v = Viewport::new(Fit::Scale, 3840, 2160, 1920, 1200);
        assert_eq!((v.x, v.y, v.w, v.h), (0.0, 60.0, 1920.0, 1080.0));
        assert_eq!(v.to_surface(960.0, 600.0), (1920.0, 1080.0));
        assert_eq!(v.to_surface(0.0, 0.0), (0.0, 0.0));
        assert_eq!(v.to_surface(5000.0, 5000.0), (3839.0, 2159.0));
        // Smaller display in a larger window: scaled up and pillarboxed.
        let v = Viewport::new(Fit::Scale, 1000, 500, 3000, 1000);
        assert_eq!((v.x, v.w, v.h), (500.0, 2000.0, 1000.0));
        assert_eq!(v.to_surface(1500.0, 500.0), (500.0, 250.0));
    }
}
