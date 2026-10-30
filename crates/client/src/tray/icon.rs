//! Tray icon pixmaps. The artwork is line art; its lines are stored as alpha masks at a few
//! sizes (generated from assets/tray.png) and tinted to suit the panel.

use crate::config::TrayIconColor;

const MASKS: [(u32, &[u8]); 5] = [
    (22, include_bytes!("../../assets/tray-22.mask")),
    (32, include_bytes!("../../assets/tray-32.mask")),
    (48, include_bytes!("../../assets/tray-48.mask")),
    (64, include_bytes!("../../assets/tray-64.mask")),
    (128, include_bytes!("../../assets/tray-128.mask")),
];

/// Square icon, ARGB32 in network byte order (A, R, G, B per pixel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pixmap {
    pub size: u32,
    pub argb: Vec<u8>,
}

pub fn pixmaps(rgb: [u8; 3]) -> Vec<Pixmap> {
    MASKS
        .iter()
        .map(|&(size, mask)| Pixmap { size, argb: mask.iter().flat_map(|&a| [a, rgb[0], rgb[1], rgb[2]]).collect() })
        .collect()
}

/// Line color for the tray: light lines on a dark panel and the other way round. `kdeglobals`
/// is the KDE color scheme file; without it, white suits the common dark panels.
pub fn line_color(choice: TrayIconColor, kdeglobals: Option<&str>) -> [u8; 3] {
    const WHITE: [u8; 3] = [255, 255, 255];
    const BLACK: [u8; 3] = [0, 0, 0];
    match choice {
        TrayIconColor::Black => BLACK,
        TrayIconColor::White => WHITE,
        TrayIconColor::Auto => match kdeglobals.and_then(panel_is_dark) {
            Some(false) => BLACK,
            _ => WHITE,
        },
    }
}

/// Reads `[Colors:Window] BackgroundNormal=r,g,b`, which Plasma panels follow.
fn panel_is_dark(kdeglobals: &str) -> Option<bool> {
    let mut in_section = false;
    for line in kdeglobals.lines().map(str::trim) {
        if line.starts_with('[') {
            in_section = line == "[Colors:Window]";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some(value) = line.strip_prefix("BackgroundNormal=") else { continue };
        let rgb: Vec<f32> = value.split(',').filter_map(|c| c.trim().parse().ok()).collect();
        let [r, g, b] = rgb[..] else { return None };
        return Some(0.2126 * r + 0.7152 * g + 0.0722 * b < 128.0);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_match_sizes() {
        for (size, mask) in MASKS {
            assert_eq!(mask.len(), (size * size) as usize);
        }
        let p = &pixmaps([1, 2, 3])[0];
        assert_eq!(p.argb.len(), 22 * 22 * 4);
        assert_eq!(&p.argb[1..4], &[1, 2, 3]);
    }

    #[test]
    fn color_follows_scheme() {
        let dark = "[General]\nBackgroundNormal=255,255,255\n[Colors:Window]\nBackgroundNormal=32,35,38\n";
        let light = "[Colors:Window]\nForegroundNormal=0,0,0\nBackgroundNormal=239,240,241\n";
        assert_eq!(line_color(TrayIconColor::Auto, Some(dark)), [255, 255, 255]);
        assert_eq!(line_color(TrayIconColor::Auto, Some(light)), [0, 0, 0]);
        assert_eq!(line_color(TrayIconColor::Auto, None), [255, 255, 255]);
        assert_eq!(line_color(TrayIconColor::Auto, Some("[Colors:Window]\nBackgroundNormal=x\n")), [255, 255, 255]);
        assert_eq!(line_color(TrayIconColor::Black, Some(dark)), [0, 0, 0]);
    }
}
