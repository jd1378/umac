//! The Mac cursor shape to show over streamed windows: standard shapes map to the local
//! cursor theme, anything else is drawn from the image the host sent.

use std::{collections::VecDeque, sync::Arc};

use cursor_icon::CursorIcon;
use umac_proto::{CursorShape, MAX_CURSOR_SIDE, NamedCursor};

/// Images kept for `CursorKnown`; matches the host's view of what we have.
const CACHE: usize = 32;

#[derive(Debug, PartialEq)]
pub struct Image {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    /// Unpremultiplied RGBA as sent by the host.
    pub rgba: Vec<u8>,
}

impl Image {
    /// wl_shm ARGB8888: premultiplied, little-endian, so bytes are B, G, R, A.
    pub fn bgra_premultiplied(&self) -> Vec<u8> {
        self.rgba
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| {
                let a = p[3] as u32;
                let pm = |c: u8| ((c as u32 * a + 127) / 255) as u8;
                [pm(p[2]), pm(p[1]), pm(p[0]), p[3]]
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Named(CursorIcon),
    Image(Arc<Image>),
}

pub fn icon(name: NamedCursor) -> CursorIcon {
    use CursorIcon as C;
    use NamedCursor as N;
    match name {
        N::Default => C::Default,
        N::Text => C::Text,
        N::VerticalText => C::VerticalText,
        N::Pointer => C::Pointer,
        N::Grab => C::Grab,
        N::Grabbing => C::Grabbing,
        N::Crosshair => C::Crosshair,
        N::NotAllowed => C::NotAllowed,
        N::Copy => C::Copy,
        N::Alias => C::Alias,
        N::ContextMenu => C::ContextMenu,
        N::ColResize => C::ColResize,
        N::RowResize => C::RowResize,
        N::EwResize => C::EwResize,
        N::NsResize => C::NsResize,
        N::EResize => C::EResize,
        N::WResize => C::WResize,
        N::NResize => C::NResize,
        N::SResize => C::SResize,
        N::NeResize => C::NeResize,
        N::NwResize => C::NwResize,
        N::SeResize => C::SeResize,
        N::SwResize => C::SwResize,
        N::NeswResize => C::NeswResize,
        N::NwseResize => C::NwseResize,
        N::ZoomIn => C::ZoomIn,
        N::ZoomOut => C::ZoomOut,
    }
}

/// Checks an image from the network; `None` if it is malformed or too large.
fn image(id: u64, shape: CursorShape) -> Option<Image> {
    let CursorShape::Image { width, height, hot_x, hot_y, rgba } = shape else { return None };
    let ok = (1..=MAX_CURSOR_SIDE).contains(&width)
        && (1..=MAX_CURSOR_SIDE).contains(&height)
        && rgba.len() == (width * height * 4) as usize
        && hot_x < width
        && hot_y < height;
    ok.then_some(Image { id, width, height, hot_x, hot_y, rgba })
}

/// Current shape plus a small cache of recent images.
pub struct Cursors {
    current: Shape,
    cache: VecDeque<Arc<Image>>,
}

impl Default for Cursors {
    fn default() -> Self {
        Self { current: Shape::Named(CursorIcon::Default), cache: VecDeque::new() }
    }
}

impl Cursors {
    pub fn current(&self) -> &Shape {
        &self.current
    }

    /// A new cursor from the host. Returns true if the shape changed.
    pub fn set(&mut self, id: u64, shape: CursorShape) -> bool {
        let next = match shape {
            CursorShape::Named(n) => Shape::Named(icon(n)),
            shape => match image(id, shape) {
                Some(img) => {
                    let img = Arc::new(img);
                    self.cache.retain(|c| c.id != id);
                    if self.cache.len() >= CACHE {
                        self.cache.pop_front();
                    }
                    self.cache.push_back(img.clone());
                    Shape::Image(img)
                }
                None => {
                    tracing::debug!("ignoring malformed cursor image");
                    Shape::Named(CursorIcon::Default)
                }
            },
        };
        self.replace(next)
    }

    /// The host switched back to an image it sent before. Unknown ids fall back to the arrow.
    pub fn known(&mut self, id: u64) -> bool {
        let next = match self.cache.iter().position(|c| c.id == id) {
            Some(i) => {
                let img = self.cache.remove(i).unwrap();
                self.cache.push_back(img.clone());
                Shape::Image(img)
            }
            None => Shape::Named(CursorIcon::Default),
        };
        self.replace(next)
    }

    fn replace(&mut self, next: Shape) -> bool {
        let changed = next != self.current;
        self.current = next;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: u32, h: u32) -> CursorShape {
        CursorShape::Image { width: w, height: h, hot_x: 0, hot_y: 0, rgba: vec![255; (w * h * 4) as usize] }
    }

    #[test]
    fn named_shapes_map_to_theme_icons() {
        assert_eq!(icon(NamedCursor::Text), CursorIcon::Text);
        assert_eq!(icon(NamedCursor::Pointer), CursorIcon::Pointer);
        assert_eq!(icon(NamedCursor::NwseResize), CursorIcon::NwseResize);
    }

    #[test]
    fn premultiplies_into_bgra() {
        let i = Image { id: 1, width: 1, height: 2, hot_x: 0, hot_y: 0, rgba: vec![200, 100, 50, 128, 10, 20, 30, 0] };
        assert_eq!(i.bgra_premultiplied(), vec![25, 50, 100, 128, 0, 0, 0, 0]);
    }

    #[test]
    fn rejects_malformed_images() {
        let mut c = Cursors::default();
        assert!(c.set(1, img(2, 2)));
        assert!(matches!(c.current(), Shape::Image(_)));
        // Wrong length, too large, hotspot outside: all fall back to the arrow.
        assert!(c.set(2, CursorShape::Image { width: 2, height: 2, hot_x: 0, hot_y: 0, rgba: vec![0; 3] }));
        assert_eq!(c.current(), &Shape::Named(CursorIcon::Default));
        c.set(3, img(MAX_CURSOR_SIDE + 1, 1));
        assert_eq!(c.current(), &Shape::Named(CursorIcon::Default));
        c.set(4, CursorShape::Image { width: 1, height: 1, hot_x: 1, hot_y: 0, rgba: vec![0; 4] });
        assert_eq!(c.current(), &Shape::Named(CursorIcon::Default));
    }

    #[test]
    fn cache_serves_known_ids_and_evicts_oldest() {
        let mut c = Cursors::default();
        for id in 0..CACHE as u64 + 1 {
            c.set(id, img(1, 1));
        }
        c.set(999, CursorShape::Named(NamedCursor::Text));
        assert!(c.known(CACHE as u64));
        assert!(matches!(c.current(), Shape::Image(i) if i.id == CACHE as u64));
        // Id 0 was evicted.
        c.known(0);
        assert_eq!(c.current(), &Shape::Named(CursorIcon::Default));
        assert!(!c.known(0));
    }
}
