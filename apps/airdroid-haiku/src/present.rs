//! Reading Android's GPU frames straight into window bitmaps.
//!
//! With gpu=virgl, Android's buffers are virtio-gpu resources in host GPU
//! memory. Our QEMU (qemu/patches/0103) listens on `VmConfig::present_socket`
//! and reads a resource into an area of ours on request, with one GPU read
//! and no copy through guest RAM: when a window shows just one GPU layer,
//! unscaled, the frame goes straight into the window's back bitmap. Anything
//! else (several layers, scaling) has QEMU fill the resources' guest backing
//! and is composed from there as before. The guest then only waits for
//! Android's rendering (airdroid.present=host).

use airdroid_core::compose::Buffer;
use airdroid_core::proto::{format, Layer, Rect};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// struct airdroid_present_req in the QEMU patch.
#[repr(C)]
struct Request {
    res: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    area: i32,
    offset: u32,
    stride: u32,
    flags: u32,
}

pub struct Present {
    path: PathBuf,
    stream: Option<UnixStream>,
    retry_at: Instant,
    /// (requests, total time) since the last report
    stats: (u32, Duration),
}

/// What one window frame needs: a resource read straight into the bitmap.
#[derive(Clone, Copy, Debug)]
pub struct Direct {
    res: u32,
    x: u32,
    y: u32,
    swap: bool,
}

impl Present {
    pub fn new(path: PathBuf) -> Present {
        Present { path, stream: None, retry_at: Instant::now(), stats: (0, Duration::ZERO) }
    }

    fn request(&mut self, req: Request) -> Result<(), String> {
        if self.stream.is_none() {
            if Instant::now() < self.retry_at {
                return Err("not connected".into());
            }
            match UnixStream::connect(&self.path) {
                Ok(s) => self.stream = Some(s),
                Err(e) => {
                    self.retry_at = Instant::now() + Duration::from_millis(500);
                    return Err(format!("{}: {e}", self.path.display()));
                }
            }
        }
        let t0 = Instant::now();
        let s = self.stream.as_mut().unwrap();
        // SAFETY: Request is plain old data with no padding
        let bytes = unsafe { std::slice::from_raw_parts((&req as *const Request).cast::<u8>(), std::mem::size_of::<Request>()) };
        let mut status = [0u8; 4];
        if let Err(e) = s.write_all(bytes).and_then(|_| s.read_exact(&mut status)) {
            self.stream = None;
            return Err(format!("present socket: {e}"));
        }
        self.stats.0 += 1;
        self.stats.1 += t0.elapsed();
        if crate::engine::debug() && self.stats.0 == 300 {
            eprintln!("airDroid: present: {} reads, {:.2} ms average", self.stats.0, self.stats.1.as_secs_f64() * 1e3 / self.stats.0 as f64);
            self.stats = (0, Duration::ZERO);
        }
        match i32::from_le_bytes(status) {
            0 => Ok(()),
            e => Err(format!("QEMU read resource {}: error {}", req.res, -e)),
        }
    }

    /// Reads `d` into `area` (a frame of `w` x `h` packed 32-bit rows).
    pub fn read_direct(&mut self, d: Direct, area: i32, w: u32, h: u32) -> Result<(), String> {
        self.request(Request { res: d.res, x: d.x, y: d.y, w, h, area, offset: 0, stride: w * 4, flags: d.swap as u32 })
    }

    /// Fills the guest backing of every GPU buffer among `layers` that shows
    /// in `area`.
    pub fn read_backings(&mut self, layers: &[Layer], buffers: &HashMap<u32, Buffer>, area: Rect) {
        for l in layers {
            let Some(b) = buffers.get(&l.buffer) else { continue };
            if b.resource == 0 || l.dst.intersect(&area).is_empty() {
                continue;
            }
            if let Err(e) = self.request(Request { res: b.resource, x: 0, y: 0, w: b.width, h: b.height, area: -1, offset: 0, stride: 0, flags: 0 }) {
                if crate::engine::debug() {
                    eprintln!("airDroid: {e}");
                }
            }
        }
    }
}

/// Whether the part of the frame in `area` is one GPU layer, unscaled and
/// 32-bit: then QEMU can read it straight into the window's bitmap. Under
/// it there may only be solid colour layers (the display's background), or
/// anything if it is opaque. (Android's premultiplied pixels over black:
/// translucent edges, which the window does not show, lose the background
/// colour.)
pub fn direct(layers: &[Layer], buffers: &HashMap<u32, Buffer>, area: Rect) -> Option<Direct> {
    let mut shown = layers.iter().filter(|l| !l.dst.intersect(&area).is_empty());
    let l = shown.next_back()?;
    let b = buffers.get(&l.buffer)?;
    if !l.opaque && shown.any(|u| buffers.contains_key(&u.buffer)) {
        return None;
    }
    let swap = match b.format {
        format::ARGB8888 | format::XRGB8888 => false,
        format::ABGR8888 | format::XBGR8888 => true,
        _ => return None,
    };
    let covers = l.dst.intersect(&area) == area;
    let unscaled = l.src.w == l.dst.w && l.src.h == l.dst.h;
    if b.resource == 0 || !covers || !unscaled {
        return None;
    }
    let x = l.src.x + area.x - l.dst.x;
    let y = l.src.y + area.y - l.dst.y;
    let fits = x >= 0 && y >= 0 && x + area.w <= b.width as i32 && y + area.h <= b.height as i32;
    fits.then_some(Direct { res: b.resource, x: x as u32, y: y as u32, swap })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(w: u32, h: u32, resource: u32) -> Buffer {
        let data: &'static [u8] = Box::leak(vec![0u8; (w * h * 4) as usize].into_boxed_slice());
        let mut b = Buffer::from_slice(data, w, h, w * 4, format::ABGR8888);
        b.resource = resource;
        b
    }
    fn layer(buffer: u32, src: Rect, dst: Rect, opaque: bool) -> Layer {
        Layer { buffer, src, dst, opaque }
    }

    #[test]
    fn one_app_window_over_the_background_is_direct() {
        let buffers = HashMap::from([(7, gpu(488, 848, 42)), (8, gpu(100, 100, 0))]);
        let screen = Rect::new(0, 0, 1920, 1080);
        let app = layer(7, Rect::new(0, 0, 488, 848), Rect::new(716, 100, 488, 848), false);
        let solid = layer(u32::MAX, Rect::new(0x202020, 0, 0, 0), screen, false);
        // the window shows the task without its 8 px shadow
        let area = Rect::new(724, 108, 472, 832);
        let d = direct(&[solid.clone(), app.clone()], &buffers, area).expect("direct");
        assert_eq!((d.res, d.x, d.y, d.swap), (42, 8, 8, true));
        // a translucent shm layer under it: compose
        let under = layer(8, Rect::new(0, 0, 100, 100), Rect::new(700, 100, 100, 100), false);
        assert!(direct(&[under.clone(), app.clone()], &buffers, area).is_none());
        // ... unless the app layer is opaque
        let opaque = Layer { opaque: true, ..app.clone() };
        assert!(direct(&[under, opaque], &buffers, area).is_some());
        // scaled, an shm buffer, or not covering the window: compose
        let scaled = layer(7, Rect::new(0, 0, 244, 424), Rect::new(716, 100, 488, 848), false);
        assert!(direct(&[solid.clone(), scaled], &buffers, area).is_none());
        let shm = layer(8, Rect::new(0, 0, 100, 100), Rect::new(724, 108, 100, 100), false);
        assert!(direct(&[solid.clone(), shm], &buffers, Rect::new(724, 108, 100, 100)).is_none());
        assert!(direct(&[solid, app], &buffers, Rect::new(700, 100, 488, 848)).is_none());
    }
}
