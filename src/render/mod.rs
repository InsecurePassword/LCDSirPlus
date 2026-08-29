//! Deterministic 160x43 monochrome framebuffer and dashboard renderer.

pub mod font;
pub mod renderer;

use crate::model::{HEIGHT, WIDTH};
use crate::sha256::Sha256;

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct Frame {
    pub pixels: [u8; WIDTH * HEIGHT],
}

impl Default for Frame {
    fn default() -> Self {
        Frame {
            pixels: [0u8; WIDTH * HEIGHT],
        }
    }
}

#[allow(dead_code)]
impl Frame {
    pub fn new() -> Self {
        Frame::default()
    }

    pub fn set(&mut self, x: i32, y: i32, on: bool) {
        if x < 0 || x >= WIDTH as i32 || y < 0 || y >= HEIGHT as i32 {
            return;
        }
        let (x, y) = (x as usize, y as usize);
        self.pixels[y * WIDTH + x] = if on { 1 } else { 0 };
    }

    pub fn get(&self, x: i32, y: i32) -> bool {
        if x < 0 || x >= WIDTH as i32 || y < 0 || y >= HEIGHT as i32 {
            return false;
        }
        let (x, y) = (x as usize, y as usize);
        self.pixels[y * WIDTH + x] != 0
    }

    pub fn h_line(&mut self, x0: i32, x1: i32, y: i32, on: bool) {
        let (x0, x1) = if x0 > x1 { (x1, x0) } else { (x0, x1) };
        for x in x0..=x1 {
            self.set(x, y, on);
        }
    }

    pub fn v_line(&mut self, x: i32, y0: i32, y1: i32, on: bool) {
        let (y0, y1) = if y0 > y1 { (y1, y0) } else { (y0, y1) };
        for y in y0..=y1 {
            self.set(x, y, on);
        }
    }

    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, on: bool) {
        if w <= 0 || h <= 0 {
            return;
        }
        self.h_line(x, x + w - 1, y, on);
        self.h_line(x, x + w - 1, y + h - 1, on);
        self.v_line(x, y, y + h - 1, on);
        self.v_line(x + w - 1, y, y + h - 1, on);
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, on: bool) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.set(xx, yy, on);
            }
        }
    }

    pub fn invert_rect(&mut self, x: i32, y: i32, w: i32, h: i32) {
        for yy in y..y + h {
            for xx in x..x + w {
                let on = self.get(xx, yy);
                self.set(xx, yy, !on);
            }
        }
    }

    /// SDK-boundary byte form: zero off, 255 on.
    pub fn logitech_bytes(&self) -> Vec<u8> {
        self.pixels
            .iter()
            .map(|&v| if v != 0 { 255 } else { 0 })
            .collect()
    }

    pub fn hash_hex(&self) -> String {
        let mut h = Sha256::new();
        h.update(&self.pixels);
        crate::sha256::hex(&h.finish())
    }

    pub fn equal(&self, other: &Frame) -> bool {
        self.pixels == other.pixels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_checked_access() {
        let mut f = Frame::new();
        f.set(-1, 0, true);
        f.set(0, -1, true);
        f.set(160, 0, true);
        f.set(0, 43, true);
        assert_eq!(f.pixels, [0u8; WIDTH * HEIGHT]);
        f.set(159, 42, true);
        assert!(f.get(159, 42));
        assert!(!f.get(160, 42));
    }

    #[test]
    fn rect_draws_border_only() {
        let mut f = Frame::new();
        f.rect(2, 2, 4, 3, true);
        assert!(f.get(2, 2) && f.get(5, 2) && f.get(2, 4) && f.get(5, 4));
        assert!(!f.get(3, 3));
    }

    #[test]
    fn logitech_sdk_bitmap_is_exact_row_major_bytes() {
        let mut frame = Frame::new();
        frame.set(0, 0, true);
        frame.set(159, 42, true);
        let bytes = frame.logitech_bytes();
        assert_eq!(bytes.len(), 6880);
        assert_eq!(bytes[0], 255);
        assert_eq!(bytes[159 + 42 * 160], 255);
        assert_eq!(bytes[1], 0);
    }
}
