//! Draws a window's layers, read straight from guest memory, into a
//! 32-bit BGRA frame (air/OS B_RGB32 / wl_shm ARGB8888 byte order).
//!
//! Android's buffers are premultiplied; opaque layers are copied, others
//! blended with `src + dst * (1 - a)`. Layers are scaled nearest-neighbour
//! when their source and destination sizes differ (rare: video, the 1x1
//! window backgrounds, which the guest sends as solid colours instead).

use crate::guestmem::GuestMem;
use airdroid_proto::{format, GpaRange, Layer, Rect};
use std::collections::HashMap;

/// A buffer announced by the guest.
pub struct Buffer {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: u32,
    /// (byte offset in the buffer, host pointer, length), in order
    segs: Vec<(usize, *const u8, usize)>,
    pub valid: bool,
}
unsafe impl Send for Buffer {}

impl Buffer {
    pub fn new(mem: &GuestMem, width: u32, height: u32, stride: u32, format: u32, ranges: &[GpaRange]) -> Buffer {
        let mut segs = Vec::with_capacity(ranges.len());
        let mut off = 0usize;
        let mut valid = true;
        for r in ranges {
            match mem.slice(r) {
                Some(s) => segs.push((off, s.as_ptr(), s.len())),
                None => valid = false,
            }
            off += r.len as usize;
        }
        let need = stride as usize * height as usize;
        if off < need || format::bytes_per_pixel(format).is_none() {
            valid = false;
        }
        Buffer { width, height, stride, format, segs, valid }
    }

    /// Copies `len` bytes at buffer offset `off` into `out`.
    fn read(&self, mut off: usize, out: &mut [u8]) {
        let mut done = 0;
        // segments are sorted by offset: binary search the first
        let mut i = match self.segs.binary_search_by(|s| s.0.cmp(&off)) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        while done < out.len() && i < self.segs.len() {
            let (start, ptr, len) = self.segs[i];
            if off >= start + len {
                i += 1;
                continue;
            }
            let within = off - start;
            let n = (len - within).min(out.len() - done);
            unsafe { std::ptr::copy_nonoverlapping(ptr.add(within), out.as_mut_ptr().add(done), n) };
            done += n;
            off += n;
            i += 1;
        }
    }
}

/// Converts one row of a buffer format into BGRA (premultiplied).
fn row_to_bgra(format: u32, src: &[u8], dst: &mut [u32], width: usize) {
    match format {
        format::ARGB8888 | format::XRGB8888 => {
            for x in 0..width {
                let p = u32::from_le_bytes([src[x * 4], src[x * 4 + 1], src[x * 4 + 2], src[x * 4 + 3]]);
                dst[x] = if format == format::XRGB8888 { p | 0xff00_0000 } else { p };
            }
        }
        format::ABGR8888 | format::XBGR8888 => {
            // memory R,G,B,A -> B,G,R,A
            for x in 0..width {
                let (r, g, b, a) = (src[x * 4] as u32, src[x * 4 + 1] as u32, src[x * 4 + 2] as u32, src[x * 4 + 3] as u32);
                let a = if format == format::XBGR8888 { 0xff } else { a };
                dst[x] = (a << 24) | (r << 16) | (g << 8) | b;
            }
        }
        format::RGB565 => {
            for x in 0..width {
                let p = u16::from_le_bytes([src[x * 2], src[x * 2 + 1]]) as u32;
                let r = ((p >> 11) & 0x1f) * 255 / 31;
                let g = ((p >> 5) & 0x3f) * 255 / 63;
                let b = (p & 0x1f) * 255 / 31;
                dst[x] = 0xff00_0000 | (r << 16) | (g << 8) | b;
            }
        }
        _ => {}
    }
}

#[inline]
fn blend(src: u32, dst: u32) -> u32 {
    let a = src >> 24;
    if a == 0xff {
        return src;
    }
    if src == 0 {
        return dst;
    }
    let inv = 255 - a;
    let mut out = 0u32;
    for shift in [0, 8, 16, 24] {
        let s = (src >> shift) & 0xff;
        let d = (dst >> shift) & 0xff;
        let v = (s + (d * inv + 127) / 255).min(255);
        out |= v << shift;
    }
    out
}

/// A window's frame on the host.
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
    row: Vec<u8>,
    conv: Vec<u32>,
}

impl Canvas {
    pub fn new() -> Canvas {
        Canvas { width: 0, height: 0, pixels: Vec::new(), row: Vec::new(), conv: Vec::new() }
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        if (w, h) != (self.width, self.height) {
            self.width = w;
            self.height = h;
            self.pixels = vec![0xff00_0000; w as usize * h as usize];
        }
    }

    /// Paints `layers` (bottom first) over the whole canvas; returns the
    /// area painted.
    pub fn compose(&mut self, layers: &[Layer], buffers: &HashMap<u32, Buffer>) -> Rect {
        self.compose_at(layers, buffers, 0, 0)
    }

    /// Like `compose`, for the part of the window whose top left corner is
    /// at (`ox`, `oy`) in window coordinates (the canvas is that part).
    pub fn compose_at(&mut self, layers: &[Layer], buffers: &HashMap<u32, Buffer>, ox: i32, oy: i32) -> Rect {
        let full = Rect::new(0, 0, self.width as i32, self.height as i32);
        let shifted: Vec<Layer>;
        let layers = if ox != 0 || oy != 0 {
            shifted = layers
                .iter()
                .map(|l| {
                    let mut l = l.clone();
                    l.dst.x -= ox;
                    l.dst.y -= oy;
                    l
                })
                .collect();
            &shifted[..]
        } else {
            layers
        };
        for p in self.pixels.iter_mut() {
            *p = 0xff00_0000;
        }
        for l in layers {
            if l.buffer == u32::MAX {
                // solid colour (src.x carries ARGB, premultiplied)
                self.fill(l.dst.intersect(&full), l.src.x as u32);
                continue;
            }
            let Some(b) = buffers.get(&l.buffer) else { continue };
            if !b.valid {
                continue;
            }
            self.draw(l, b, &full);
        }
        full
    }

    fn fill(&mut self, r: Rect, color: u32) {
        if r.is_empty() {
            return;
        }
        let w = self.width as usize;
        for y in r.y..r.y + r.h {
            let row = &mut self.pixels[y as usize * w + r.x as usize..y as usize * w + (r.x + r.w) as usize];
            if color >> 24 == 0xff {
                row.fill(color);
            } else {
                for p in row.iter_mut() {
                    *p = blend(color, *p);
                }
            }
        }
    }

    fn draw(&mut self, l: &Layer, b: &Buffer, full: &Rect) {
        let clip = l.dst.intersect(full);
        if clip.is_empty() || l.src.w <= 0 || l.src.h <= 0 {
            return;
        }
        let bpp = format::bytes_per_pixel(b.format).unwrap() as usize;
        let src = l.src.intersect(&Rect::new(0, 0, b.width as i32, b.height as i32));
        if src.is_empty() {
            return;
        }
        let scaled = src.w != l.dst.w || src.h != l.dst.h;
        let opaque = l.opaque || !format::has_alpha(b.format);
        let w = self.width as usize;
        let bw = b.width as usize;
        if self.row.len() < bw * bpp {
            self.row.resize(bw * bpp, 0);
        }
        if self.conv.len() < bw {
            self.conv.resize(bw, 0);
        }
        let mut last_sy = usize::MAX;
        for y in clip.y..clip.y + clip.h {
            let sy = if scaled {
                src.y as usize + ((y - l.dst.y) as usize * src.h as usize) / l.dst.h as usize
            } else {
                (src.y + (y - l.dst.y)) as usize
            };
            if sy != last_sy {
                // read only the needed span of the source row
                let (x0, x1) = if scaled { (src.x as usize, (src.x + src.w) as usize) } else {
                    let x0 = src.x as usize + (clip.x - l.dst.x) as usize;
                    (x0, x0 + clip.w as usize)
                };
                let off = sy * b.stride as usize + x0 * bpp;
                let n = (x1 - x0) * bpp;
                let mut row = std::mem::take(&mut self.row);
                b.read(off, &mut row[..n]);
                let mut conv = std::mem::take(&mut self.conv);
                row_to_bgra(b.format, &row[..n], &mut conv[..x1 - x0], x1 - x0);
                self.row = row;
                self.conv = conv;
                last_sy = sy;
            }
            let dst_row = &mut self.pixels[y as usize * w..(y as usize + 1) * w];
            for x in clip.x..clip.x + clip.w {
                let si = if scaled {
                    ((x - l.dst.x) as usize * src.w as usize) / l.dst.w as usize
                } else {
                    (x - clip.x) as usize
                };
                let s = self.conv[si];
                let d = &mut dst_row[x as usize];
                *d = if opaque { s | 0xff00_0000 } else { blend(s, *d) };
            }
        }
    }
}

impl Default for Canvas {
    fn default() -> Self {
        Self::new()
    }
}

/// The part of a window Android draws in: the union of its layers, leaving
/// out fully transparent solid ones (the hwcomposer's window background in
/// multi-window mode is the whole display, transparent, with the app's
/// task somewhere in it). None if nothing is visible.
pub fn content_bounds(layers: &[Layer]) -> Option<Rect> {
    let mut r = Rect::default();
    for l in layers {
        if l.buffer == u32::MAX && (l.src.x as u32) >> 24 == 0 {
            continue;
        }
        r = r.union(&l.dst);
    }
    (!r.is_empty()).then_some(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_skip_transparent_background() {
        let bg = Layer { buffer: u32::MAX, src: Rect::new(0, 0, 0, 0), dst: Rect::new(0, 0, 1280, 800), opaque: false };
        let app = Layer { buffer: 3, src: Rect::new(0, 0, 360, 640), dst: Rect::new(458, 62, 364, 654), opaque: true };
        assert_eq!(content_bounds(&[bg.clone(), app.clone()]), Some(Rect::new(458, 62, 364, 654)));
        assert_eq!(content_bounds(&[bg]), None);
        let opaque_bg = Layer { buffer: u32::MAX, src: Rect::new(0xff000000u32 as i32, 0, 0, 0), dst: Rect::new(0, 0, 1280, 800), opaque: true };
        assert_eq!(content_bounds(&[opaque_bg, app]), Some(Rect::new(0, 0, 1280, 800)));
    }
}
