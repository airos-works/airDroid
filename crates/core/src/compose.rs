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
    /// The virtio-gpu resource behind it, or 0 (an shm buffer, or the guest
    /// copies the resource into guest RAM itself).
    pub resource: u32,
}
unsafe impl Send for Buffer {}
// (composing in bands only reads through the pointers)
unsafe impl Sync for Buffer {}

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
        Buffer { width, height, stride, format, segs, valid, resource: 0 }
    }

    /// A buffer over host memory (tests and benchmarks).
    pub fn from_slice(data: &[u8], width: u32, height: u32, stride: u32, format: u32) -> Buffer {
        Buffer { width, height, stride, format, segs: vec![(0, data.as_ptr(), data.len())], valid: true, resource: 0 }
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
    // premultiplied "over": src + dst * (255 - a) / 255, two channels at a
    // time (red and blue, then green and alpha), rounding as x / 255 does
    // within one step
    let inv = 255 - a;
    let scale = |v: u32| -> u32 {
        let t = v * inv + 0x0080_0080;
        ((t + ((t >> 8) & 0x00ff_00ff)) >> 8) & 0x00ff_00ff
    };
    let rb = scale(dst & 0x00ff_00ff);
    let ag = scale((dst >> 8) & 0x00ff_00ff);
    let sum_rb = (src & 0x00ff_00ff) + rb;
    let sum_ag = ((src >> 8) & 0x00ff_00ff) + ag;
    // saturate each 8-bit lane (premultiplied input keeps them in range;
    // this only guards against bad data)
    let sat = |v: u32| -> u32 {
        let over = v & 0x0100_0100;
        (v | (over - (over >> 8))) & 0x00ff_00ff
    };
    sat(sum_rb) | (sat(sum_ag) << 8)
}

/// A window's frame on the host.
pub struct Canvas {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u32>,
    row: Vec<u8>,
    conv: Vec<u32>,
    /// the generic path only (tests compare the fast paths with it)
    slow: bool,
    /// the window row `out` starts at (a band of a frame; else 0)
    top: i32,
    /// threads for one frame at most (0: as many as make sense)
    pub bands: usize,
}

/// Areas smaller than this are composed by the calling thread alone.
const BAND_PIXELS: i64 = 300_000;

impl Canvas {
    pub fn new() -> Canvas {
        Canvas { width: 0, height: 0, pixels: Vec::new(), row: Vec::new(), conv: Vec::new(), slow: false, top: 0, bands: 0 }
    }

    pub fn resize(&mut self, w: u32, h: u32) {
        // (compose_into() sets the size without the pixels)
        if (w, h) != (self.width, self.height) || self.pixels.len() < w as usize * h as usize {
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
        let mut pixels = std::mem::take(&mut self.pixels);
        let r = self.compose_into(&mut pixels, self.width, self.height, layers, buffers, ox, oy);
        self.pixels = pixels;
        r
    }

    /// Like `compose_at`, into `out` (`width` x `height` pixels, rows packed:
    /// a window's bitmap), instead of the canvas's own pixels.
    pub fn compose_into(
        &mut self,
        out: &mut [u32],
        width: u32,
        height: u32,
        layers: &[Layer],
        buffers: &HashMap<u32, Buffer>,
        ox: i32,
        oy: i32,
    ) -> Rect {
        self.compose_region(out, width, height, layers, buffers, ox, oy, None)
    }

    /// Like `compose_into`, for the part `clip` of `out` only (in `out`'s
    /// coordinates; None: all of it): the rest of `out` keeps what it has.
    /// Returns the area painted.
    #[allow(clippy::too_many_arguments)]
    pub fn compose_region(
        &mut self,
        out: &mut [u32],
        width: u32,
        height: u32,
        layers: &[Layer],
        buffers: &HashMap<u32, Buffer>,
        ox: i32,
        oy: i32,
        clip: Option<Rect>,
    ) -> Rect {
        assert!(out.len() >= width as usize * height as usize);
        self.width = width;
        self.height = height;
        let bounds = Rect::new(0, 0, self.width as i32, self.height as i32);
        let full = clip.map_or(bounds, |c| c.intersect(&bounds));
        if full.is_empty() {
            return full;
        }
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
        // layers under the topmost opaque one that covers everything are
        // hidden; without one, start from black
        let first = if self.slow { None } else { first_needed(layers, buffers, &full) };
        let clear = first.is_none();
        let layers = &layers[first.unwrap_or(0)..];
        // A large area in bands, a thread each: blending several layers of
        // a whole screen is 20 ms on one.
        let wanted = match self.bands {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()).min(8),
            n => n,
        };
        let bands = wanted.min((full.w as i64 * full.h as i64 / BAND_PIXELS) as usize).min(full.h as usize);
        if bands < 2 || self.slow {
            self.top = 0;
            self.paint(out, layers, buffers, &full, clear);
            return full;
        }
        let w = width as usize;
        let slow = self.slow;
        let mut rest = &mut out[full.y as usize * w..(full.y + full.h) as usize * w];
        let mut y = full.y;
        std::thread::scope(|scope| {
            for i in 0..bands {
                let h = (full.y + full.h - y) / (bands - i) as i32;
                let (band, tail) = std::mem::take(&mut rest).split_at_mut(h as usize * w);
                rest = tail;
                let clip = Rect::new(full.x, y, full.w, h);
                let mut part = Canvas { width, height, top: y, slow, ..Canvas::new() };
                let mut work = move || part.paint(band, layers, buffers, &clip, clear);
                if i + 1 == bands {
                    work();
                } else {
                    scope.spawn(work);
                }
                y += h;
            }
        });
        full
    }

    /// Paints `layers` into the part `clip` of the frame; `out` starts at
    /// row `self.top` of it.
    fn paint(&mut self, out: &mut [u32], layers: &[Layer], buffers: &HashMap<u32, Buffer>, clip: &Rect, clear: bool) {
        if clear {
            self.fill(out, *clip, 0xff00_0000);
        }
        for l in layers {
            if l.buffer == u32::MAX {
                // solid colour (src.x carries ARGB, premultiplied)
                self.fill(out, l.dst.intersect(clip), l.src.x as u32);
                continue;
            }
            let Some(b) = buffers.get(&l.buffer) else { continue };
            if !b.valid {
                continue;
            }
            self.draw(out, l, b, clip);
        }
    }

    fn fill(&mut self, out: &mut [u32], r: Rect, color: u32) {
        if r.is_empty() {
            return;
        }
        let w = self.width as usize;
        for y in r.y..r.y + r.h {
            let at = (y - self.top) as usize * w;
            let row = &mut out[at + r.x as usize..at + (r.x + r.w) as usize];
            if color >> 24 == 0xff {
                row.fill(color);
            } else {
                for p in row.iter_mut() {
                    *p = blend(color, *p);
                }
            }
        }
    }

    fn draw(&mut self, out: &mut [u32], l: &Layer, b: &Buffer, full: &Rect) {
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
        let swap_rb = b.format == format::ABGR8888 || b.format == format::XBGR8888;
        if !scaled && opaque && !self.slow && (swap_rb || b.format == format::ARGB8888 || b.format == format::XRGB8888) {
            // memory B,G,R,A rows are the canvas's layout: copy them (R,G,B,A
            // ones, Android's usual RGBA_8888, with red and blue swapped)
            let x0 = src.x as usize + (clip.x - l.dst.x) as usize;
            let n = clip.w as usize;
            for y in clip.y..clip.y + clip.h {
                let sy = (src.y + (y - l.dst.y)) as usize;
                let start = (y - self.top) as usize * w + clip.x as usize;
                let dst_row = &mut out[start..start + n];
                let bytes = unsafe { std::slice::from_raw_parts_mut(dst_row.as_mut_ptr() as *mut u8, n * 4) };
                b.read(sy * b.stride as usize + x0 * 4, bytes);
                if swap_rb {
                    for p in dst_row.iter_mut() {
                        let v = *p;
                        *p = (v & 0x0000_ff00) | ((v >> 16) & 0xff) | ((v & 0xff) << 16) | 0xff00_0000;
                    }
                } else {
                    for p in dst_row.iter_mut() {
                        *p |= 0xff00_0000;
                    }
                }
            }
            return;
        }
        if !scaled && !self.slow && (b.format == format::ARGB8888 || b.format == format::ABGR8888) {
            // Translucent 32-bit layers, unscaled (Android's hwcomposer marks
            // nothing opaque, so this is every app window): one read of the
            // row, then one pass that copies alpha-255 pixels and blends the
            // rest
            let x0 = src.x as usize + (clip.x - l.dst.x) as usize;
            let n = clip.w as usize;
            let mut conv = std::mem::take(&mut self.conv);
            if conv.len() < n {
                conv.resize(n, 0);
            }
            for y in clip.y..clip.y + clip.h {
                let sy = (src.y + (y - l.dst.y)) as usize;
                let bytes = unsafe { std::slice::from_raw_parts_mut(conv.as_mut_ptr() as *mut u8, n * 4) };
                b.read(sy * b.stride as usize + x0 * 4, bytes);
                let start = (y - self.top) as usize * w + clip.x as usize;
                let dst_row = &mut out[start..start + n];
                let row = &conv[..n];
                // rows that are opaque throughout (all of a window's but its
                // shadow's) are copied; this check and that copy vectorize
                let opaque_row = row.iter().fold(0xffff_ffffu32, |m, &v| m & v) >> 24 == 0xff;
                if opaque_row {
                    if swap_rb {
                        for (d, &v) in dst_row.iter_mut().zip(row) {
                            *d = (v & 0xff00_ff00) | ((v >> 16) & 0xff) | ((v & 0xff) << 16);
                        }
                    } else {
                        dst_row.copy_from_slice(row);
                    }
                } else {
                    for (d, &v) in dst_row.iter_mut().zip(row) {
                        let v = if swap_rb { (v & 0xff00_ff00) | ((v >> 16) & 0xff) | ((v & 0xff) << 16) } else { v };
                        *d = if v >> 24 == 0xff { v } else { blend(v, *d) };
                    }
                }
            }
            self.conv = conv;
            return;
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
            let dst_row = &mut out[(y - self.top) as usize * w..((y - self.top) as usize + 1) * w];
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

impl Buffer {
    fn alpha_row(&self, y: u32, x0: u32, x1: u32, row: &mut Vec<u8>) -> Option<Vec<u8>> {
        if self.format != format::ARGB8888 && self.format != format::ABGR8888 {
            return None; // no alpha: opaque
        }
        let n = ((x1 - x0) * 4) as usize;
        row.resize(n, 0);
        self.read(y as usize * self.stride as usize + x0 as usize * 4, &mut row[..n]);
        Some(row.chunks(4).map(|p| p[3]).collect())
    }

    /// The part of `src` whose pixels are fully opaque, in buffer
    /// coordinates (Android's freeform windows carry their shadow as
    /// translucent margins in the same buffer).
    pub fn opaque_extent(&self, src: Rect) -> Rect {
        let src = src.intersect(&Rect::new(0, 0, self.width as i32, self.height as i32));
        if src.is_empty() || !self.valid || !format::has_alpha(self.format) {
            return src;
        }
        let (x0, x1) = (src.x as u32, (src.x + src.w) as u32);
        let mut row = Vec::new();
        let opaque_row = |y: i32, row: &mut Vec<u8>| -> Option<(u32, u32)> {
            let a = self.alpha_row(y as u32, x0, x1, row)?;
            let first = a.iter().position(|&v| v == 255)?;
            let last = a.iter().rposition(|&v| v == 255)?;
            Some((x0 + first as u32, x0 + last as u32 + 1))
        };
        let Some(top) = (src.y..src.y + src.h).find(|&y| opaque_row(y, &mut row).is_some()) else { return src };
        let bottom = (src.y..src.y + src.h).rev().find(|&y| opaque_row(y, &mut row).is_some()).unwrap_or(top);
        // left/right from a handful of rows (shadows are uniform)
        let (mut left, mut right) = (x1, x0);
        let step = ((bottom - top) / 16).max(1);
        let mut y = top;
        while y <= bottom {
            if let Some((l, r)) = opaque_row(y, &mut row) {
                left = left.min(l);
                right = right.max(r);
            }
            y += step;
        }
        if right <= left {
            return src;
        }
        Rect::new(left as i32, top, (right - left) as i32, bottom - top + 1)
    }
}

/// Where two buffers of one layout differ within `area` (buffer
/// coordinates, four-byte pixels): the bounds, or an empty rectangle if
/// nowhere. None if they cannot be compared.
///
/// For a client that says "all of it changed" with every buffer: Android's
/// SurfaceFlinger hands its own composition target over like that, one
/// buffer after the other with the same picture.
pub fn buffer_changes(a: &Buffer, b: &Buffer, area: &Rect) -> Option<Rect> {
    let four = |f| matches!(f, format::ARGB8888 | format::XRGB8888 | format::ABGR8888 | format::XBGR8888);
    if !a.valid || !b.valid || (a.width, a.height, a.stride, a.format) != (b.width, b.height, b.stride, b.format) || !four(a.format) {
        return None;
    }
    let area = area.intersect(&Rect::new(0, 0, a.width as i32, a.height as i32));
    if area.is_empty() {
        return Some(Rect::new(0, 0, 0, 0));
    }
    let rows = |y0: i32, y1: i32| -> Option<(i32, i32, i32, i32)> {
        let n = area.w as usize * 4;
        let (mut ra, mut rb) = (vec![0u8; n], vec![0u8; n]);
        let mut found: Option<(i32, i32, i32, i32)> = None; // x0, y0, x1, y1 (inclusive)
        for y in y0..y1 {
            let off = y as usize * a.stride as usize + area.x as usize * 4;
            a.read(off, &mut ra);
            b.read(off, &mut rb);
            if ra == rb {
                continue;
            }
            let first = ra.chunks_exact(4).zip(rb.chunks_exact(4)).position(|(p, q)| p != q).unwrap() as i32;
            let last = ra.chunks_exact(4).zip(rb.chunks_exact(4)).rposition(|(p, q)| p != q).unwrap() as i32;
            found = Some(match found {
                None => (first, y, last, y),
                Some((x0, top, x1, _)) => (x0.min(first), top, x1.max(last), y),
            });
        }
        found
    };
    // in bands, like composing: 8 MB of a whole screen each
    let bands = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8).min((area.w as i64 * area.h as i64 / BAND_PIXELS) as usize).max(1);
    let rows = &rows;
    let found: Vec<Option<(i32, i32, i32, i32)>> = std::thread::scope(|scope| {
        let step = (area.h as usize).div_ceil(bands) as i32;
        let handles: Vec<_> = (0..bands as i32)
            .map(|i| {
                let (y0, y1) = (area.y + i * step, (area.y + (i + 1) * step).min(area.y + area.h));
                scope.spawn(move || if y0 < y1 { rows(y0, y1) } else { None })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or(None)).collect()
    });
    let mut all: Option<(i32, i32, i32, i32)> = None;
    for f in found.into_iter().flatten() {
        all = Some(match all {
            None => f,
            Some((x0, y0, x1, y1)) => (x0.min(f.0), y0.min(f.1), x1.max(f.2), y1.max(f.3)),
        });
    }
    Some(match all {
        None => Rect::new(0, 0, 0, 0),
        Some((x0, y0, x1, y1)) => Rect::new(area.x + x0, y0, x1 - x0 + 1, y1 - y0 + 1),
    })
}

/// The topmost layer that hides everything under it in `region` (opaque,
/// covering it, all of its source inside its buffer): composing `region`
/// starts there. None: start from black with the first layer.
pub fn first_needed(layers: &[Layer], buffers: &HashMap<u32, Buffer>, region: &Rect) -> Option<usize> {
    layers.iter().rposition(|l| {
        l.dst.x <= region.x
            && l.dst.y <= region.y
            && l.dst.x + l.dst.w >= region.x + region.w
            && l.dst.y + l.dst.h >= region.y + region.h
            && if l.buffer == u32::MAX {
                (l.src.x as u32) >> 24 == 0xff
            } else {
                buffers.get(&l.buffer).is_some_and(|b| {
                    b.valid
                        && (l.opaque || !format::has_alpha(b.format))
                        && l.src.x >= 0
                        && l.src.y >= 0
                        && l.src.x + l.src.w <= b.width as i32
                        && l.src.y + l.src.h <= b.height as i32
                })
            }
    })
}

/// What a window's frame changed against the frame before it: the bounds of
/// the layers that are not the same in both (their old and new places).
/// None: everything (the layers were added, removed or reordered); an empty
/// rectangle: nothing.
///
/// A layer with the same buffer in both frames shows the same pixels: a
/// client cannot draw into the buffer a surface shows (Android's buffer
/// queues never hand out the front buffer), so new content comes in another
/// buffer.
pub fn frame_changes(before: &[Layer], now: &[Layer]) -> Option<Rect> {
    if before.len() != now.len() {
        return None;
    }
    let mut changed = Rect::new(0, 0, 0, 0);
    for (b, n) in before.iter().zip(now) {
        if b != n {
            changed = changed.union(&b.dst).union(&n.dst);
        }
    }
    Some(changed)
}

/// Like `content_bounds`, without the translucent margins (shadows) of the
/// layers: what a window without per-pixel transparency should show.
pub fn opaque_bounds(layers: &[Layer], buffers: &HashMap<u32, Buffer>) -> Option<Rect> {
    let mut r = Rect::default();
    for l in layers {
        if l.buffer == u32::MAX {
            if (l.src.x as u32) >> 24 == 0xff {
                r = r.union(&l.dst);
            }
            continue;
        }
        let Some(b) = buffers.get(&l.buffer) else { continue };
        let e = b.opaque_extent(l.src);
        if e.is_empty() || l.src.w <= 0 || l.src.h <= 0 {
            continue;
        }
        // buffer -> window coordinates (layers may be scaled)
        let sx = |x: i32| l.dst.x + ((x - l.src.x) as i64 * l.dst.w as i64 / l.src.w as i64) as i32;
        let sy = |y: i32| l.dst.y + ((y - l.src.y) as i64 * l.dst.h as i64 / l.src.h as i64) as i32;
        let d = Rect::new(sx(e.x), sy(e.y), sx(e.x + e.w) - sx(e.x), sy(e.y + e.h) - sy(e.y));
        r = r.union(&d);
    }
    (!r.is_empty()).then_some(r)
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
    fn a_region_composes_like_the_whole() {
        let (w, h) = (64u32, 40u32);
        let (app, bar, layers) = frame(w, h);
        // a third layer in the middle, translucent
        let mut dot = vec![0u8; 16 * 16 * 4];
        for (i, b) in dot.iter_mut().enumerate() {
            *b = if i % 4 == 3 { 0x60 } else { (i % 61) as u8 / 3 };
        }
        let mut layers = layers;
        layers.push(Layer { buffer: 3, src: Rect::new(0, 0, 16, 16), dst: Rect::new(20, 12, 16, 16), opaque: false });
        let buffers = HashMap::from([
            (1, Buffer::from_slice(&app, w, h, w * 4, format::XRGB8888)),
            (2, Buffer::from_slice(&bar, w, 48, w * 4, format::ARGB8888)),
            (3, Buffer::from_slice(&dot, 16, 16, 64, format::ARGB8888)),
        ]);
        let mut whole = vec![0u32; (w * h) as usize];
        Canvas::new().compose_into(&mut whole, w, h, &layers, &buffers, 0, 0);
        for clip in [Rect::new(18, 10, 20, 20), Rect::new(0, 0, 64, 5), Rect::new(60, 30, 40, 40), Rect::new(5, 5, 1, 1)] {
            let mut part = vec![0x1234_5678u32; (w * h) as usize];
            let painted = Canvas::new().compose_region(&mut part, w, h, &layers, &buffers, 0, 0, Some(clip));
            assert_eq!(painted, clip.intersect(&Rect::new(0, 0, w as i32, h as i32)));
            for y in 0..h as i32 {
                for x in 0..w as i32 {
                    let i = (y as u32 * w + x as u32) as usize;
                    let inside = x >= painted.x && x < painted.x + painted.w && y >= painted.y && y < painted.y + painted.h;
                    assert_eq!(part[i], if inside { whole[i] } else { 0x1234_5678 }, "at {x},{y} for {clip:?}");
                }
            }
        }
    }

    #[test]
    fn bands_compose_like_one_thread() {
        let (w, h) = (900u32, 700u32);
        let (app, bar, mut layers) = frame(w, h);
        let mut veil = vec![0u8; (w * h * 4) as usize];
        for (i, b) in veil.iter_mut().enumerate() {
            *b = if i % 4 == 3 { 0x40 } else { (i % 53) as u8 / 4 };
        }
        // a translucent layer over everything, scaled, and one that is not
        layers.push(Layer { buffer: 3, src: Rect::new(0, 0, w as i32, h as i32), dst: Rect::new(0, 0, w as i32, h as i32), opaque: false });
        layers.push(Layer { buffer: 3, src: Rect::new(0, 0, 300, 300), dst: Rect::new(100, 50, 450, 600), opaque: false });
        let buffers = HashMap::from([
            (1, Buffer::from_slice(&app, w, h, w * 4, format::XRGB8888)),
            (2, Buffer::from_slice(&bar, w, 48, w * 4, format::ARGB8888)),
            (3, Buffer::from_slice(&veil, w, h, w * 4, format::ABGR8888)),
        ]);
        let mut one = vec![0u32; (w * h) as usize];
        let mut c = Canvas::new();
        c.bands = 1;
        c.compose_into(&mut one, w, h, &layers, &buffers, 0, 0);
        for bands in [2, 3, 7] {
            let mut many = vec![0x1111_1111u32; (w * h) as usize];
            let mut c = Canvas::new();
            c.bands = bands;
            c.compose_into(&mut many, w, h, &layers, &buffers, 0, 0);
            assert!(one == many, "{bands} bands differ from one thread");
            // and a region of it
            let clip = Rect::new(37, 91, 801, 577);
            let mut part = vec![0x1111_1111u32; (w * h) as usize];
            c.compose_region(&mut part, w, h, &layers, &buffers, 0, 0, Some(clip));
            for y in 0..h as i32 {
                for x in 0..w as i32 {
                    let i = (y as u32 * w + x as u32) as usize;
                    let inside = x >= clip.x && x < clip.x + clip.w && y >= clip.y && y < clip.y + clip.h;
                    assert_eq!(part[i], if inside { one[i] } else { 0x1111_1111 }, "at {x},{y} with {bands} bands");
                }
            }
        }
    }

    #[test]
    fn buffer_changes_are_where_the_pixels_differ() {
        let (w, h) = (1000u32, 700u32);
        let one = vec![7u8; (w * h * 4) as usize];
        let mut two = one.clone();
        let a = Buffer::from_slice(&one, w, h, w * 4, format::ARGB8888);
        let whole = Rect::new(0, 0, w as i32, h as i32);
        assert_eq!(buffer_changes(&a, &Buffer::from_slice(&two, w, h, w * 4, format::ARGB8888), &whole), Some(Rect::new(0, 0, 0, 0)));
        // two pixels, far apart (in different bands)
        two[(30 * w as usize + 100) * 4 + 2] = 9;
        two[(650 * w as usize + 900) * 4] = 9;
        let b = Buffer::from_slice(&two, w, h, w * 4, format::ARGB8888);
        assert_eq!(buffer_changes(&a, &b, &whole), Some(Rect::new(100, 30, 801, 621)));
        // within an area that has only the first
        assert_eq!(buffer_changes(&a, &b, &Rect::new(50, 10, 200, 100)), Some(Rect::new(100, 30, 1, 1)));
        // and one that has neither
        assert_eq!(buffer_changes(&a, &b, &Rect::new(200, 100, 300, 300)), Some(Rect::new(0, 0, 0, 0)));
        // another layout: no answer
        assert_eq!(buffer_changes(&a, &Buffer::from_slice(&two, w, h - 1, w * 4, format::ARGB8888), &whole), None);
    }

    #[test]
    fn frame_changes_are_the_layers_that_differ() {
        let l = |buffer, x, w| Layer { buffer, src: Rect::new(0, 0, w, 10), dst: Rect::new(x, 0, w, 10), opaque: true };
        let before = vec![l(1, 0, 100), l(2, 10, 20), l(3, 50, 20)];
        assert_eq!(frame_changes(&before, &before), Some(Rect::new(0, 0, 0, 0)));
        // another buffer in the second layer
        let now = vec![l(1, 0, 100), l(7, 10, 20), l(3, 50, 20)];
        assert_eq!(frame_changes(&before, &now), Some(Rect::new(10, 0, 20, 10)));
        // the third layer moved: where it was and where it is
        let now = vec![l(1, 0, 100), l(2, 10, 20), l(3, 60, 20)];
        assert_eq!(frame_changes(&before, &now), Some(Rect::new(50, 0, 30, 10)));
        // a layer more: everything
        assert_eq!(frame_changes(&before, &before[..2]), None);
    }

    /// A typical Android frame: a full-screen opaque XRGB layer (the app)
    /// and a translucent ARGB status bar over it.
    fn frame(w: u32, h: u32) -> (Vec<u8>, Vec<u8>, Vec<Layer>) {
        let mut app = vec![0u8; (w * h * 4) as usize];
        for (i, b) in app.iter_mut().enumerate() {
            *b = (i * 7 % 251) as u8;
        }
        let mut bar = vec![0u8; (w * 48 * 4) as usize];
        for (i, b) in bar.iter_mut().enumerate() {
            *b = if i % 4 == 3 { 0x80 } else { (i % 97) as u8 / 2 };
        }
        let layers = vec![
            Layer { buffer: 1, src: Rect::new(0, 0, w as i32, h as i32), dst: Rect::new(0, 0, w as i32, h as i32), opaque: true },
            Layer { buffer: 2, src: Rect::new(0, 0, w as i32, 48), dst: Rect::new(0, 0, w as i32, 48), opaque: false },
        ];
        (app, bar, layers)
    }

    fn old_blend(src: u32, dst: u32) -> u32 {
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
            out |= ((s + (d * inv + 127) / 255).min(255)) << shift;
        }
        out
    }

    fn close(a: u32, b: u32) -> bool {
        (0..4).all(|i| (((a >> (i * 8)) & 0xff) as i32 - ((b >> (i * 8)) & 0xff) as i32).abs() <= 1)
    }

    #[test]
    fn blend_matches_division() {
        let mut seed = 12345u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            seed
        };
        for _ in 0..200_000 {
            // premultiplied source: channels <= alpha
            let a = rnd() >> 24;
            let c = |v: u32| if a == 0 { 0 } else { v % (a + 1) };
            let src = (a << 24) | (c(rnd() & 0xff) << 16) | (c(rnd() & 0xff) << 8) | c(rnd() & 0xff);
            let dst = rnd();
            assert!(close(blend(src, dst), old_blend(src, dst)), "{src:08x} over {dst:08x}");
        }
    }

    #[test]
    fn fast_paths_match_generic() {
        let (w, h) = (97u32, 61u32);
        let (app, bar, _) = frame(w, h);
        let mut abgr = app.clone();
        abgr.reverse();
        // RGBA with every pixel opaque but some rows translucent (a window
        // and its shadow)
        let mut solid = app.clone();
        for (i, p) in solid.chunks_mut(4).enumerate() {
            p[3] = if (i / w as usize) % 7 == 3 { 0x60 } else { 0xff };
            if p[3] != 0xff {
                for c in &mut p[..3] {
                    *c = (*c as u32 * 0x60 / 255) as u8;
                }
            }
        }
        let mut bufs = HashMap::new();
        bufs.insert(4, Buffer::from_slice(&solid, w, h, w * 4, format::ABGR8888));
        bufs.insert(5, Buffer::from_slice(&solid, w, h, w * 4, format::ARGB8888));
        bufs.insert(1, Buffer::from_slice(&app, w, h, w * 4, format::XRGB8888));
        bufs.insert(2, Buffer::from_slice(&bar, w, 48, w * 4, format::ARGB8888));
        bufs.insert(3, Buffer::from_slice(&abgr, w, h, w * 4, format::ABGR8888));
        let full = Rect::new(0, 0, w as i32, h as i32);
        let cases: Vec<Vec<Layer>> = vec![
            vec![Layer { buffer: 1, src: full, dst: full, opaque: true }],
            vec![
                Layer { buffer: 1, src: full, dst: full, opaque: true },
                Layer { buffer: 2, src: Rect::new(0, 0, w as i32, 48), dst: Rect::new(0, 0, w as i32, 48), opaque: false },
            ],
            // partly covered: black around it, then translucent on top
            vec![
                Layer { buffer: 1, src: Rect::new(5, 3, 40, 30), dst: Rect::new(10, 7, 40, 30), opaque: true },
                Layer { buffer: 2, src: Rect::new(0, 0, 50, 20), dst: Rect::new(30, 20, 50, 20), opaque: false },
            ],
            // scaled, other formats, solid colours, layers under an opaque one
            vec![
                Layer { buffer: u32::MAX, src: Rect::new(0x80402010u32 as i32, 0, 0, 0), dst: full, opaque: false },
                Layer { buffer: 3, src: Rect::new(0, 0, 40, 30), dst: Rect::new(0, 0, 80, 60), opaque: false },
                Layer { buffer: 1, src: full, dst: full, opaque: true },
                Layer { buffer: 3, src: Rect::new(0, 0, 40, 30), dst: Rect::new(20, 10, 60, 45), opaque: false },
                Layer { buffer: u32::MAX, src: Rect::new(0x40202020u32 as i32, 0, 0, 0), dst: Rect::new(3, 3, 20, 20), opaque: false },
            ],
            // shifted origin (multi-window crop)
            vec![Layer { buffer: 1, src: full, dst: Rect::new(-7, -5, w as i32, h as i32), opaque: true }],
            // not marked opaque, rows opaque or translucent, over a colour
            vec![
                Layer { buffer: u32::MAX, src: Rect::new(0xff336699u32 as i32, 0, 0, 0), dst: full, opaque: true },
                Layer { buffer: 4, src: full, dst: Rect::new(-2, -3, w as i32, h as i32), opaque: false },
            ],
            vec![
                Layer { buffer: 1, src: full, dst: full, opaque: true },
                Layer { buffer: 5, src: Rect::new(3, 2, 60, 50), dst: Rect::new(10, 5, 60, 50), opaque: false },
            ],
            // Android's RGBA_8888 (ABGR8888), opaque, cropped and partly covering
            vec![Layer { buffer: 3, src: full, dst: Rect::new(-3, -2, w as i32, h as i32), opaque: true }],
            vec![Layer { buffer: 3, src: Rect::new(4, 4, 50, 40), dst: Rect::new(9, 9, 50, 40), opaque: true }],
        ];
        for (i, layers) in cases.iter().enumerate() {
            let mut fast = Canvas::new();
            fast.resize(w, h);
            let mut slow = Canvas::new();
            slow.slow = true;
            slow.resize(w, h);
            fast.pixels.fill(0x12345678);
            slow.pixels.fill(0x12345678);
            fast.compose_at(layers, &bufs, 0, 0);
            slow.compose_at(layers, &bufs, 0, 0);
            for (j, (a, b)) in fast.pixels.iter().zip(&slow.pixels).enumerate() {
                assert!(close(*a, *b), "case {i} pixel {j}: {a:08x} vs {b:08x}");
            }
        }
    }

    /// `cargo test -p airdroid-core --release compose_bench -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn compose_bench() {
        let (w, h) = (1920u32, 1080u32);
        let (app, bar, layers) = frame(w, h);
        let mut bufs = HashMap::new();
        bufs.insert(1, Buffer::from_slice(&app, w, h, w * 4, format::XRGB8888));
        bufs.insert(2, Buffer::from_slice(&bar, w, 48, w * 4, format::ARGB8888));
        let mut c = Canvas::new();
        c.resize(w, h);
        let n = 200;
        let t = std::time::Instant::now();
        for _ in 0..n {
            c.compose(&layers, &bufs);
        }
        let per = t.elapsed() / n;
        eprintln!("compose 1920x1080 (opaque XRGB + ARGB bar): {per:?} per frame");
        let mut bufs = HashMap::new();
        bufs.insert(1, Buffer::from_slice(&app, w, h, w * 4, format::ABGR8888));
        bufs.insert(2, Buffer::from_slice(&bar, w, 48, w * 4, format::ABGR8888));
        let t = std::time::Instant::now();
        for _ in 0..n {
            c.compose(&layers, &bufs);
        }
        eprintln!("compose 1920x1080 (opaque ABGR + ABGR bar): {:?} per frame", t.elapsed() / n);
        // Android's hwcomposer marks nothing opaque: an RGBA app layer with
        // every pixel at alpha 255, as multi-window shows it
        let mut solid = app.clone();
        for p in solid.chunks_mut(4) {
            p[3] = 0xff;
        }
        let mut bufs = HashMap::new();
        bufs.insert(1, Buffer::from_slice(&solid, w, h, w * 4, format::ABGR8888));
        let unmarked = vec![Layer { opaque: false, ..layers[0].clone() }];
        let t = std::time::Instant::now();
        for _ in 0..n {
            c.compose(&unmarked, &bufs);
        }
        eprintln!("compose 1920x1080 (ABGR not marked opaque, alpha 255): {:?} per frame", t.elapsed() / n);
        let t = std::time::Instant::now();
        for _ in 0..n {
            c.pixels.fill(0xff00_0000);
        }
        eprintln!("  clear alone: {:?}", t.elapsed() / n);
        let marked = vec![Layer { opaque: true, ..layers[0].clone() }];
        let t = std::time::Instant::now();
        for _ in 0..n {
            c.compose(&marked, &bufs);
        }
        eprintln!("  same buffer marked opaque: {:?}", t.elapsed() / n);
    }

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
