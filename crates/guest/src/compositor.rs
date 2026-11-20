//! The Wayland compositor Waydroid's hwcomposer talks to, inside the VM.
//!
//! It implements what `hwcomposer.waydroid` binds (wl_compositor,
//! wl_subcompositor, wl_shm, xdg_wm_base, wl_seat, wl_output, wp_viewporter,
//! wp_presentation, wl_data_device_manager) and composites nothing itself:
//! every window's surface tree becomes a list of layers that the host draws
//! straight from guest memory. A shm buffer is pinned (mlock) and announced
//! once with its guest-physical pages; frames then only carry buffer ids.
//!
//! Pacing: one frame per window is in flight; the host's FrameDone releases
//! replaced buffers and fires frame callbacks / presentation feedback, so
//! Android renders at the rate the host displays.

use crate::hostlink::HostLink;
use crate::logf;
use crate::sys;
use airdroid_proto::{format, GpaRange, Layer, Rect, ToGuest, ToHost, TouchKind};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use wayland_protocols::wp::linux_dmabuf::zv1::server::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1};
use wayland_protocols::wp::presentation_time::server::{wp_presentation, wp_presentation_feedback};
use wayland_protocols::wp::viewporter::server::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::server::{xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_server::backend::{ClientData, ClientId, DisconnectReason, ObjectId};
use wayland_server::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
    wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_shm_pool, wl_subcompositor, wl_subsurface,
    wl_surface, wl_touch,
};
use wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, ListeningSocket, New, Resource, WEnum};

const KEYMAP: &str = include_str!("../data/keymap-us.xkb");

// ---- shared memory ---------------------------------------------------------

/// A client's shm pool, mapped and pinned in the daemon.
struct Pool {
    ptr: *mut u8,
    size: usize,
    _fd: OwnedFd,
}
unsafe impl Send for Pool {}
unsafe impl Sync for Pool {}

impl Pool {
    fn map(fd: OwnedFd, size: usize) -> std::io::Result<Pool> {
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), size.max(1), libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0)
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        // populate and pin: the host reads these pages by physical address
        if unsafe { libc::mlock(ptr, size) } != 0 {
            logf!("mlock of a {size}-byte pool failed: {}", std::io::Error::last_os_error());
        }
        Ok(Pool { ptr: ptr as *mut u8, size, _fd: fd })
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        unsafe {
            libc::munlock(self.ptr as *const libc::c_void, self.size);
            libc::munmap(self.ptr as *mut libc::c_void, self.size.max(1));
        }
    }
}

/// Guest-physical ranges backing [addr, addr+len) of our address space.
fn gpa_ranges(pagemap: &mut std::fs::File, addr: usize, len: usize) -> std::io::Result<Vec<GpaRange>> {
    const PAGE: usize = 4096;
    let mut out: Vec<GpaRange> = Vec::new();
    let end = addr + len;
    let mut va = addr;
    let first_page = addr / PAGE;
    let npages = (end + PAGE - 1) / PAGE - first_page;
    let mut entries = vec![0u8; npages * 8];
    pagemap.seek(SeekFrom::Start((first_page * 8) as u64))?;
    pagemap.read_exact(&mut entries)?;
    let mut i = 0;
    while va < end {
        let e = u64::from_le_bytes(entries[i * 8..i * 8 + 8].try_into().unwrap());
        if e & (1 << 63) == 0 {
            return Err(std::io::Error::other(format!("page at {va:#x} not present")));
        }
        let pfn = e & ((1 << 55) - 1);
        if pfn == 0 {
            return Err(std::io::Error::other("pagemap hides PFNs (needs CAP_SYS_ADMIN)"));
        }
        let off = va % PAGE;
        let chunk = (PAGE - off).min(end - va);
        let gpa = pfn * PAGE as u64 + off as u64;
        match out.last_mut() {
            Some(last) if last.gpa + last.len as u64 == gpa && (last.len as usize) + chunk <= u32::MAX as usize => {
                last.len += chunk as u32
            }
            _ => out.push(GpaRange { gpa, len: chunk as u32 }),
        }
        va += chunk;
        i += 1;
    }
    Ok(out)
}

// ---- state -----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
struct SrcRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

#[derive(Default)]
struct SurfState {
    /// Some(None) = detach, Some(Some(b)) = attach, None = unchanged.
    buffer: Option<Option<wl_buffer::WlBuffer>>,
    damage: bool,
    callbacks: Vec<wl_callback::WlCallback>,
    feedbacks: Vec<wp_presentation_feedback::WpPresentationFeedback>,
    src: Option<Option<SrcRect>>,
    dst: Option<Option<(i32, i32)>>,
    scale: Option<i32>,
    input: Option<Option<Vec<(bool, Rect)>>>,
    position: Option<(i32, i32)>,
}

impl SurfState {
    /// Fold a newer state into this cached one.
    fn merge(&mut self, n: SurfState) {
        if n.buffer.is_some() {
            self.buffer = n.buffer;
        }
        self.damage |= n.damage;
        self.callbacks.extend(n.callbacks);
        self.feedbacks.extend(n.feedbacks);
        if n.src.is_some() {
            self.src = n.src;
        }
        if n.dst.is_some() {
            self.dst = n.dst;
        }
        if n.scale.is_some() {
            self.scale = n.scale;
        }
        if n.input.is_some() {
            self.input = n.input;
        }
        if n.position.is_some() {
            self.position = n.position;
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    None,
    Toplevel(u32),
    Subsurface { parent_sync: bool },
    Cursor,
}

struct Surface {
    res: wl_surface::WlSurface,
    pending: SurfState,
    cached: Option<SurfState>,
    buffer: Option<wl_buffer::WlBuffer>,
    src: Option<SrcRect>,
    dst: Option<(i32, i32)>,
    scale: i32,
    /// None = infinite input region.
    input: Option<Vec<(bool, Rect)>>,
    role: Role,
    parent: Option<ObjectId>,
    /// position relative to the parent (subsurfaces)
    pos: (i32, i32),
    sync: bool,
    /// subsurfaces, bottom to top; `self` sits at index `self_index`
    children: Vec<ObjectId>,
    self_index: usize,
    /// frame callbacks / feedback waiting for the next frame of the window
    callbacks: Vec<wl_callback::WlCallback>,
    feedbacks: Vec<wp_presentation_feedback::WpPresentationFeedback>,
}

struct BufferInfo {
    host_id: u32,
    width: u32,
    height: u32,
    format: u32,
    _pool: Option<Arc<Pool>>,
    /// a virtio-gpu resource (dma-buf): read back before each frame
    gpu: Option<Arc<crate::drm::GpuBuffer>>,
    /// a 1x1 pool buffer: its colour, read at commit (backgrounds)
    solid: Option<*const u32>,
}
unsafe impl Send for BufferInfo {}

struct Window {
    id: u32,
    surface: ObjectId,
    xdg_surface: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    title: String,
    app_id: String,
    geometry: Option<Rect>,
    announced: bool,
    configured: bool,
    maximized: bool,
    dirty: bool,
    serial: u32,
    in_flight: Option<InFlight>,
    pending_release: Vec<wl_buffer::WlBuffer>,
}

struct InFlight {
    serial: u32,
    sent_ns: u64,
    callbacks: Vec<wl_callback::WlCallback>,
    feedbacks: Vec<wp_presentation_feedback::WpPresentationFeedback>,
    release: Vec<wl_buffer::WlBuffer>,
}

#[derive(Default)]
struct SeatClient {
    client: Option<Client>,
    pointers: Vec<wl_pointer::WlPointer>,
    keyboards: Vec<wl_keyboard::WlKeyboard>,
    touches: Vec<wl_touch::WlTouch>,
    data_devices: Vec<wl_data_device::WlDataDevice>,
}

pub struct OutputCfg {
    pub width: u32,
    pub height: u32,
    pub refresh_mhz: u32,
    pub dpi: u32,
}

pub struct State {
    link: HostLink,
    drm: Option<crate::drm::Drm>,
    /// GPU buffers attached since the last frames went out
    readback: Vec<ObjectId>,
    out: OutputCfg,
    pagemap: std::fs::File,
    surfaces: HashMap<ObjectId, Surface>,
    buffers: HashMap<ObjectId, BufferInfo>,
    pools: HashMap<ObjectId, Arc<Pool>>,
    regions: HashMap<ObjectId, Vec<(bool, Rect)>>,
    viewports: HashMap<ObjectId, ObjectId>,
    xdg_surfaces: HashMap<ObjectId, ObjectId>,
    windows: HashMap<u32, Window>,
    toplevels: HashMap<ObjectId, u32>,
    seats: HashMap<ClientId, SeatClient>,
    outputs: Vec<wl_output::WlOutput>,
    next_buffer: u32,
    next_window: u32,
    serial: u32,
    /// pointer: (window, surface under it)
    pointer_focus: Option<(u32, ObjectId)>,
    keyboard_focus: Option<u32>,
    touch_focus: HashMap<i32, (u32, ObjectId, (f64, f64))>,
    pressed_keys: Vec<u32>,
    cursor_surface: Option<ObjectId>,
    selection: Option<wl_data_source::WlDataSource>,
    host_clipboard: Option<String>,
    epoch: std::time::Instant,
}

impl State {
    fn next_serial(&mut self) -> u32 {
        self.serial = self.serial.wrapping_add(1);
        self.serial
    }

    fn time_ms(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    fn root_window(&self, mut id: ObjectId) -> Option<u32> {
        loop {
            let s = self.surfaces.get(&id)?;
            match s.role {
                Role::Toplevel(w) => return Some(w),
                Role::Subsurface { .. } => id = s.parent.clone()?,
                _ => return None,
            }
        }
    }

    /// Whether `id` is a sync subsurface (itself or through a parent).
    fn effectively_sync(&self, id: &ObjectId) -> bool {
        let mut cur = id.clone();
        loop {
            let Some(s) = self.surfaces.get(&cur) else { return false };
            match s.role {
                Role::Subsurface { .. } => {
                    if s.sync {
                        return true;
                    }
                    match &s.parent {
                        Some(p) => cur = p.clone(),
                        None => return false,
                    }
                }
                _ => return false,
            }
        }
    }

    fn commit(&mut self, id: &ObjectId) {
        let sync = self.effectively_sync(id);
        let Some(s) = self.surfaces.get_mut(id) else { return };
        let pending = std::mem::take(&mut s.pending);
        if sync {
            match &mut s.cached {
                Some(c) => c.merge(pending),
                None => s.cached = Some(pending),
            }
            return;
        }
        let mut st = pending;
        if let Some(mut c) = s.cached.take() {
            c.merge(st);
            st = c;
        }
        self.apply(id, st);
        // children in sync mode apply their cached state with the parent
        self.apply_cached_children(id);
        self.after_commit(id);
    }

    fn apply_cached_children(&mut self, id: &ObjectId) {
        let children = self.surfaces.get(id).map(|s| s.children.clone()).unwrap_or_default();
        for c in children {
            if &c == id {
                continue;
            }
            if let Some(cs) = self.surfaces.get_mut(&c) {
                if let Some(st) = cs.cached.take() {
                    self.apply(&c, st);
                }
            }
            self.apply_cached_children(&c);
        }
    }

    fn apply(&mut self, id: &ObjectId, st: SurfState) {
        let window = self.root_window(id.clone());
        let Some(s) = self.surfaces.get_mut(id) else { return };
        if let Some(b) = st.buffer {
            if let Some(nb) = b.as_ref() {
                if self.buffers.get(&nb.id()).is_some_and(|i| i.gpu.is_some()) {
                    self.readback.push(nb.id());
                }
            }
            let Some(s) = self.surfaces.get_mut(id) else { return };
            let old = std::mem::replace(&mut s.buffer, b);
            if let Some(old) = old {
                if s.buffer.as_ref() != Some(&old) {
                    match window.and_then(|w| self.windows.get_mut(&w)) {
                        Some(w) => w.pending_release.push(old),
                        None => old.release(),
                    }
                }
            }
        }
        let Some(s) = self.surfaces.get_mut(id) else { return };
        if let Some(src) = st.src {
            s.src = src;
        }
        if let Some(dst) = st.dst {
            s.dst = dst;
        }
        if let Some(scale) = st.scale {
            s.scale = scale.max(1);
        }
        if let Some(input) = st.input {
            s.input = input;
        }
        if let Some(p) = st.position {
            s.pos = p;
        }
        s.callbacks.extend(st.callbacks);
        s.feedbacks.extend(st.feedbacks);
        if let Some(w) = window.and_then(|w| self.windows.get_mut(&w)) {
            w.dirty = true;
        }
    }

    fn after_commit(&mut self, id: &ObjectId) {
        let Some(s) = self.surfaces.get(id) else { return };
        match s.role {
            Role::Toplevel(wid) => {
                let has_buffer = s.buffer.is_some();
                let Some(w) = self.windows.get_mut(&wid) else { return };
                if !w.configured {
                    w.configured = true;
                    self.configure_window(wid);
                } else if has_buffer && !w.announced {
                    w.announced = true;
                    let (title, app_id) = (w.title.clone(), w.app_id.clone());
                    let (ww, wh) = self.window_size(wid);
                    self.link.send(&ToHost::WindowNew { id: wid, title, app_id, width: ww, height: wh });
                }
            }
            Role::Cursor => {
                let buf = s.buffer.as_ref().and_then(|b| self.buffers.get(&b.id())).map(|b| b.host_id);
                let cbs = std::mem::take(&mut self.surfaces.get_mut(id).unwrap().callbacks);
                let t = self.time_ms();
                for cb in cbs {
                    cb.done(t);
                }
                if let Some((wid, _)) = self.pointer_focus.clone() {
                    self.link.send(&ToHost::Cursor { window: wid, buffer: buf, hot_x: 0, hot_y: 0 });
                }
            }
            _ => {}
        }
    }

    fn configure_window(&mut self, wid: u32) {
        let (w, h) = (self.out.width as i32, self.out.height as i32);
        let serial = self.next_serial();
        let Some(win) = self.windows.get(&wid) else { return };
        let mut states: Vec<u8> = Vec::new();
        let mut push = |s: xdg_toplevel::State| states.extend_from_slice(&(s as u32).to_ne_bytes());
        push(xdg_toplevel::State::Activated);
        if win.maximized {
            push(xdg_toplevel::State::Maximized);
        }
        win.toplevel.configure(w, h, states);
        win.xdg_surface.configure(serial);
    }

    fn surface_size(&self, s: &Surface) -> Option<(i32, i32)> {
        if let Some(d) = s.dst {
            return Some(d);
        }
        if let Some(src) = s.src {
            return Some((src.w.round() as i32, src.h.round() as i32));
        }
        let b = self.buffers.get(&s.buffer.as_ref()?.id())?;
        Some((b.width as i32 / s.scale, b.height as i32 / s.scale))
    }

    fn window_size(&self, wid: u32) -> (u32, u32) {
        let Some(w) = self.windows.get(&wid) else { return (1, 1) };
        if let Some(g) = w.geometry {
            if !g.is_empty() {
                return (g.w as u32, g.h as u32);
            }
        }
        let mut layers = Vec::new();
        self.collect_layers(&w.surface, 0, 0, &mut layers, &mut Vec::new());
        let mut r = Rect::default();
        for l in &layers {
            r = r.union(&l.0.dst);
        }
        ((r.x + r.w).max(1) as u32, (r.y + r.h).max(1) as u32)
    }

    /// Depth-first, bottom to top: (layer, surface).
    fn collect_layers(&self, id: &ObjectId, x: i32, y: i32, out: &mut Vec<(Layer, ObjectId)>, solid: &mut Vec<(usize, u32)>) {
        let Some(s) = self.surfaces.get(id) else { return };
        for (i, c) in s.children.iter().enumerate() {
            if i == s.self_index && c == id {
                if let Some(b) = s.buffer.as_ref().and_then(|b| self.buffers.get(&b.id())) {
                    let (w, h) = self.surface_size(s).unwrap_or((b.width as i32, b.height as i32));
                    let src = match s.src {
                        Some(r) => Rect::new(r.x as i32, r.y as i32, r.w.round() as i32, r.h.round() as i32),
                        None => Rect::new(0, 0, b.width as i32, b.height as i32),
                    };
                    if let Some(p) = b.solid {
                        solid.push((out.len(), unsafe { std::ptr::read_volatile(p) }));
                    }
                    out.push((
                        Layer { buffer: b.host_id, src, dst: Rect::new(x, y, w, h), opaque: !format::has_alpha(b.format) },
                        id.clone(),
                    ));
                }
                continue;
            }
            if let Some(cs) = self.surfaces.get(c) {
                self.collect_layers(c, x + cs.pos.0, y + cs.pos.1, out, solid);
            }
        }
    }

    /// Sends frames for dirty windows that have none in flight.
    fn flush_frames(&mut self) {
        // GPU buffers: copy what Android rendered into their guest pages
        if !self.readback.is_empty() {
            if let Some(drm) = &self.drm {
                for id in std::mem::take(&mut self.readback) {
                    if let Some(g) = self.buffers.get(&id).and_then(|b| b.gpu.clone()) {
                        if let Err(e) = drm.read_back(&g) {
                            logf!("gpu read back: {e}");
                        }
                    }
                }
            }
        }
        let now = sys::monotonic_ns();
        let ids: Vec<u32> = self.windows.keys().copied().collect();
        for wid in ids {
            // a host that stopped answering (window hidden) must not stall
            // Android: give up on a frame after a second
            if let Some(w) = self.windows.get(&wid) {
                if let Some(f) = &w.in_flight {
                    if now.saturating_sub(f.sent_ns) > 1_000_000_000 {
                        let serial = f.serial;
                        self.frame_done(wid, serial, now);
                    }
                }
            }
            let Some(w) = self.windows.get(&wid) else { continue };
            if !w.dirty || w.in_flight.is_some() || !w.announced {
                continue;
            }
            let surface = w.surface.clone();
            let mut layers = Vec::new();
            let mut solid = Vec::new();
            self.collect_layers(&surface, 0, 0, &mut layers, &mut solid);
            let (width, height) = self.window_size(wid);
            // gather callbacks of every surface in the tree
            let mut callbacks = Vec::new();
            let mut feedbacks = Vec::new();
            for (_, sid) in &layers {
                if let Some(s) = self.surfaces.get_mut(sid) {
                    callbacks.append(&mut s.callbacks);
                    feedbacks.append(&mut s.feedbacks);
                }
            }
            if let Some(s) = self.surfaces.get_mut(&surface) {
                callbacks.append(&mut s.callbacks);
                feedbacks.append(&mut s.feedbacks);
            }
            let w = self.windows.get_mut(&wid).unwrap();
            w.dirty = false;
            w.serial = w.serial.wrapping_add(1);
            let serial = w.serial;
            let release = std::mem::take(&mut w.pending_release);
            w.in_flight = Some(InFlight { serial, sent_ns: now, callbacks, feedbacks, release });
            let mut out_layers: Vec<Layer> = layers.into_iter().map(|(l, _)| l).collect();
            // 1x1 solid buffers: the host fills; encode the colour in src.x
            for (i, color) in solid {
                out_layers[i].src = Rect::new(color as i32, 0, 0, 0);
                out_layers[i].buffer = u32::MAX;
            }
            self.link.send(&ToHost::Frame { window: wid, serial, width, height, layers: out_layers, damage: Vec::new() });
        }
    }

    fn frame_done(&mut self, wid: u32, serial: u32, _host_ns: u64) {
        let t = self.time_ms();
        let now = sys::monotonic_ns();
        let refresh_ns = if self.out.refresh_mhz > 0 { (1_000_000_000_000u64 / self.out.refresh_mhz as u64) as u32 } else { 16_666_666 };
        let output = self.outputs.first().cloned();
        let Some(w) = self.windows.get_mut(&wid) else { return };
        let Some(f) = w.in_flight.take_if(|f| f.serial == serial) else { return };
        for b in f.release {
            b.release();
        }
        for cb in f.callbacks {
            cb.done(t);
        }
        let secs = now / 1_000_000_000;
        for fb in f.feedbacks {
            if let Some(o) = output.as_ref().filter(|o| o.id().same_client_as(&fb.id())) {
                fb.sync_output(o);
            }
            fb.presented(
                (secs >> 32) as u32,
                secs as u32,
                (now % 1_000_000_000) as u32,
                refresh_ns,
                0,
                serial,
                wp_presentation_feedback::Kind::Vsync,
            );
        }
    }

    // ---- input ----------------------------------------------------------

    fn surface_abs_pos(&self, id: &ObjectId) -> (i32, i32) {
        let mut p = (0, 0);
        let mut cur = id.clone();
        while let Some(s) = self.surfaces.get(&cur) {
            if let Role::Subsurface { .. } = s.role {
                p.0 += s.pos.0;
                p.1 += s.pos.1;
                match &s.parent {
                    Some(par) => cur = par.clone(),
                    None => break,
                }
            } else {
                break;
            }
        }
        p
    }

    fn accepts_input(&self, s: &Surface, lx: f64, ly: f64) -> bool {
        let Some((w, h)) = self.surface_size(s) else { return false };
        if lx < 0.0 || ly < 0.0 || lx >= w as f64 || ly >= h as f64 {
            return false;
        }
        match &s.input {
            None => true,
            Some(ops) => {
                let mut inside = false;
                for (add, r) in ops {
                    if lx >= r.x as f64 && ly >= r.y as f64 && lx < (r.x + r.w) as f64 && ly < (r.y + r.h) as f64 {
                        inside = *add;
                    }
                }
                inside
            }
        }
    }

    /// Topmost surface of window `wid` accepting input at (x, y).
    fn hit(&self, wid: u32, x: f64, y: f64) -> Option<(ObjectId, f64, f64)> {
        let w = self.windows.get(&wid)?;
        let mut all = Vec::new();
        self.collect_tree(&w.surface, &mut all);
        for id in all.iter().rev() {
            let s = self.surfaces.get(id)?;
            let (px, py) = self.surface_abs_pos(id);
            let (lx, ly) = (x - px as f64, y - py as f64);
            if s.buffer.is_some() && self.accepts_input(s, lx, ly) {
                return Some((id.clone(), lx, ly));
            }
        }
        // nothing claims it: the toplevel surface, in window coordinates
        Some((w.surface.clone(), x, y))
    }

    fn collect_tree(&self, id: &ObjectId, out: &mut Vec<ObjectId>) {
        let Some(s) = self.surfaces.get(id) else { return };
        for (i, c) in s.children.iter().enumerate() {
            if i == s.self_index && c == id {
                out.push(id.clone());
            } else {
                self.collect_tree(c, out);
            }
        }
    }

    fn surface_res(&self, id: &ObjectId) -> Option<wl_surface::WlSurface> {
        self.surfaces.get(id).map(|s| s.res.clone())
    }

    fn pointers_for(&self, surf: &wl_surface::WlSurface) -> Vec<wl_pointer::WlPointer> {
        let Some(c) = surf.client() else { return Vec::new() };
        self.seats.get(&c.id()).map(|s| s.pointers.clone()).unwrap_or_default()
    }

    fn pointer_motion(&mut self, wid: u32, x: f64, y: f64) {
        let Some((sid, lx, ly)) = self.hit(wid, x, y) else { return };
        let t = self.time_ms();
        if self.pointer_focus.as_ref().map(|(_, s)| s) != Some(&sid) {
            if let Some((_, old)) = self.pointer_focus.take() {
                if let Some(os) = self.surface_res(&old) {
                    let serial = self.next_serial();
                    for p in self.pointers_for(&os) {
                        p.leave(serial, &os);
                        if p.version() >= 5 {
                            p.frame();
                        }
                    }
                }
            }
            let Some(ns) = self.surface_res(&sid) else { return };
            let serial = self.next_serial();
            for p in self.pointers_for(&ns) {
                p.enter(serial, &ns, lx, ly);
                if p.version() >= 5 {
                    p.frame();
                }
            }
            self.pointer_focus = Some((wid, sid));
        } else if let Some(ns) = self.surface_res(&sid) {
            for p in self.pointers_for(&ns) {
                p.motion(t, lx, ly);
                if p.version() >= 5 {
                    p.frame();
                }
            }
        }
    }

    fn pointer_leave(&mut self) {
        if let Some((_, old)) = self.pointer_focus.take() {
            if let Some(os) = self.surface_res(&old) {
                let serial = self.next_serial();
                for p in self.pointers_for(&os) {
                    p.leave(serial, &os);
                    if p.version() >= 5 {
                        p.frame();
                    }
                }
            }
        }
    }

    fn pointer_button(&mut self, button: u32, pressed: bool) {
        let Some((_, sid)) = self.pointer_focus.clone() else { return };
        let Some(s) = self.surface_res(&sid) else { return };
        let serial = self.next_serial();
        let t = self.time_ms();
        let state = if pressed { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
        for p in self.pointers_for(&s) {
            p.button(serial, t, button, state);
            if p.version() >= 5 {
                p.frame();
            }
        }
    }

    fn pointer_axis(&mut self, dx: f64, dy: f64, disc_x: i32, disc_y: i32) {
        let Some((_, sid)) = self.pointer_focus.clone() else { return };
        let Some(s) = self.surface_res(&sid) else { return };
        let t = self.time_ms();
        for p in self.pointers_for(&s) {
            if p.version() >= 5 {
                p.axis_source(wl_pointer::AxisSource::Wheel);
            }
            if dy != 0.0 {
                if p.version() >= 5 && disc_y != 0 {
                    p.axis_discrete(wl_pointer::Axis::VerticalScroll, disc_y);
                }
                p.axis(t, wl_pointer::Axis::VerticalScroll, dy);
            }
            if dx != 0.0 {
                if p.version() >= 5 && disc_x != 0 {
                    p.axis_discrete(wl_pointer::Axis::HorizontalScroll, disc_x);
                }
                p.axis(t, wl_pointer::Axis::HorizontalScroll, dx);
            }
            if p.version() >= 5 {
                p.frame();
            }
        }
    }

    fn touch(&mut self, wid: u32, id: i32, kind: TouchKind, x: f64, y: f64) {
        let t = self.time_ms();
        match kind {
            TouchKind::Down => {
                let Some((sid, lx, ly)) = self.hit(wid, x, y) else { return };
                let Some(s) = self.surface_res(&sid) else { return };
                let (px, py) = self.surface_abs_pos(&sid);
                self.touch_focus.insert(id, (wid, sid, (px as f64, py as f64)));
                let serial = self.next_serial();
                for tch in self.touches_for(&s) {
                    tch.down(serial, t, &s, id, lx, ly);
                    tch.frame();
                }
            }
            TouchKind::Motion => {
                let Some((_, sid, (px, py))) = self.touch_focus.get(&id).cloned() else { return };
                let Some(s) = self.surface_res(&sid) else { return };
                for tch in self.touches_for(&s) {
                    tch.motion(t, id, x - px, y - py);
                    tch.frame();
                }
            }
            TouchKind::Up => {
                let Some((_, sid, _)) = self.touch_focus.remove(&id) else { return };
                let Some(s) = self.surface_res(&sid) else { return };
                let serial = self.next_serial();
                for tch in self.touches_for(&s) {
                    tch.up(serial, t, id);
                    tch.frame();
                }
            }
            TouchKind::Cancel => {
                let ids: Vec<(i32, ObjectId)> = self.touch_focus.drain().map(|(k, v)| (k, v.1)).collect();
                for (_, sid) in ids {
                    if let Some(s) = self.surface_res(&sid) {
                        for tch in self.touches_for(&s) {
                            tch.cancel();
                        }
                    }
                }
            }
        }
    }

    fn touches_for(&self, surf: &wl_surface::WlSurface) -> Vec<wl_touch::WlTouch> {
        let Some(c) = surf.client() else { return Vec::new() };
        self.seats.get(&c.id()).map(|s| s.touches.clone()).unwrap_or_default()
    }

    fn keyboards_for(&self, surf: &wl_surface::WlSurface) -> Vec<wl_keyboard::WlKeyboard> {
        let Some(c) = surf.client() else { return Vec::new() };
        self.seats.get(&c.id()).map(|s| s.keyboards.clone()).unwrap_or_default()
    }

    fn focus(&mut self, wid: Option<u32>) {
        if self.keyboard_focus == wid {
            return;
        }
        if let Some(old) = self.keyboard_focus.take() {
            if let Some(s) = self.windows.get(&old).and_then(|w| self.surface_res(&w.surface)) {
                let serial = self.next_serial();
                for k in self.keyboards_for(&s) {
                    k.leave(serial, &s);
                }
            }
        }
        if let Some(new) = wid {
            if let Some(s) = self.windows.get(&new).and_then(|w| self.surface_res(&w.surface)) {
                let serial = self.next_serial();
                for k in self.keyboards_for(&s) {
                    k.enter(serial, &s, Vec::new());
                    k.modifiers(serial, 0, 0, 0, 0);
                }
                self.keyboard_focus = Some(new);
            }
        }
        self.pressed_keys.clear();
    }

    fn key(&mut self, wid: u32, code: u32, pressed: bool) {
        if self.keyboard_focus != Some(wid) {
            self.focus(Some(wid));
        }
        let Some(s) = self.windows.get(&wid).and_then(|w| self.surface_res(&w.surface)) else { return };
        if pressed {
            self.pressed_keys.push(code);
        } else {
            self.pressed_keys.retain(|k| *k != code);
        }
        let serial = self.next_serial();
        let t = self.time_ms();
        let state = if pressed { wl_keyboard::KeyState::Pressed } else { wl_keyboard::KeyState::Released };
        for k in self.keyboards_for(&s) {
            k.key(serial, t, code, state);
        }
    }

    fn send_keymap(k: &wl_keyboard::WlKeyboard) {
        let bytes = KEYMAP.as_bytes();
        let name = sys::cstr("airdroid-keymap");
        let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return;
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut f = std::fs::File::from(fd);
        use std::io::Write;
        let _ = f.write_all(bytes);
        let _ = f.write_all(&[0]);
        k.keymap(wl_keyboard::KeymapFormat::XkbV1, f.as_fd(), (bytes.len() + 1) as u32);
        if k.version() >= 4 {
            k.repeat_info(0, 0); // Android repeats keys itself
        }
    }

    fn window_close(&mut self, wid: u32) {
        if let Some(w) = self.windows.get(&wid) {
            w.toplevel.close();
        }
    }

    fn window_resize(&mut self, wid: u32, width: u32, height: u32) {
        let serial = self.next_serial();
        if let Some(w) = self.windows.get(&wid) {
            let mut states: Vec<u8> = Vec::new();
            states.extend_from_slice(&(xdg_toplevel::State::Activated as u32).to_ne_bytes());
            w.toplevel.configure(width as i32, height as i32, states);
            w.xdg_surface.configure(serial);
        }
    }

    fn set_output(&mut self, o: OutputCfg) {
        self.out = o;
        for out in self.outputs.clone() {
            Self::send_output(&self.out, &out);
        }
    }

    fn send_output(cfg: &OutputCfg, o: &wl_output::WlOutput) {
        let dpi = if cfg.dpi == 0 { 160 } else { cfg.dpi };
        let mm = |px: u32| (px as f64 * 25.4 / dpi as f64) as i32;
        o.geometry(0, 0, mm(cfg.width), mm(cfg.height), wl_output::Subpixel::Unknown, "airOS".into(), "airDroid".into(), wl_output::Transform::Normal);
        o.mode(wl_output::Mode::Current | wl_output::Mode::Preferred, cfg.width as i32, cfg.height as i32, cfg.refresh_mhz as i32);
        if o.version() >= 2 {
            o.scale(1);
            o.done();
        }
    }

    // ---- clipboard ---------------------------------------------------------

    /// Android set the selection: read its text and hand it to the host.
    fn take_selection(&mut self, source: wl_data_source::WlDataSource, mimes: Vec<String>) {
        let Some(mime) = mimes.iter().find(|m| m.starts_with("text/plain")).cloned() else { return };
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return;
        }
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        source.send(mime, w.as_fd());
        drop(w);
        let link = self.link.clone();
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::fs::File::from(r).read_to_string(&mut text);
            link.send(&ToHost::Clipboard { text });
        });
        self.selection = Some(source);
    }

    /// The host clipboard changed: offer it to Android.
    fn offer_clipboard(&mut self, dh: &DisplayHandle, text: String) {
        self.host_clipboard = Some(text);
        for seat in self.seats.values() {
            let Some(client) = seat.client.clone() else { continue };
            for dev in &seat.data_devices {
                let Ok(offer) = client.create_resource::<wl_data_offer::WlDataOffer, (), State>(dh, dev.version(), ()) else { continue };
                dev.data_offer(&offer);
                offer.offer("text/plain;charset=utf-8".into());
                offer.offer("text/plain".into());
                dev.selection(Some(&offer));
            }
        }
    }
}

// ---- protocol dispatch -------------------------------------------------------

struct ClientState;
impl ClientData for ClientState {
    fn initialized(&self, _: ClientId) {}
    fn disconnected(&self, _: ClientId, reason: DisconnectReason) {
        logf!("wayland client disconnected: {reason:?}");
    }
}

macro_rules! global_simple {
    ($iface:ty) => {
        impl GlobalDispatch<$iface, ()> for State {
            fn bind(_: &mut State, _: &DisplayHandle, _: &Client, r: New<$iface>, _: &(), di: &mut DataInit<'_, State>) {
                di.init(r, ());
            }
        }
    };
}

global_simple!(wl_compositor::WlCompositor);
global_simple!(wl_subcompositor::WlSubcompositor);
global_simple!(wp_viewporter::WpViewporter);
global_simple!(wl_data_device_manager::WlDataDeviceManager);

impl GlobalDispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, r: New<xdg_wm_base::XdgWmBase>, _: &(), di: &mut DataInit<'_, State>) {
        di.init(r, ());
    }
}

impl GlobalDispatch<wl_shm::WlShm, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, r: New<wl_shm::WlShm>, _: &(), di: &mut DataInit<'_, State>) {
        let shm = di.init(r, ());
        for f in [wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888, wl_shm::Format::Abgr8888, wl_shm::Format::Xbgr8888, wl_shm::Format::Rgb565] {
            shm.format(f);
        }
    }
}

impl GlobalDispatch<wl_seat::WlSeat, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, r: New<wl_seat::WlSeat>, _: &(), di: &mut DataInit<'_, State>) {
        let seat = di.init(r, ());
        seat.capabilities(wl_seat::Capability::Pointer | wl_seat::Capability::Keyboard | wl_seat::Capability::Touch);
        if seat.version() >= 2 {
            seat.name("airDroid".into());
        }
    }
}

impl GlobalDispatch<wl_output::WlOutput, ()> for State {
    fn bind(st: &mut State, _: &DisplayHandle, _: &Client, r: New<wl_output::WlOutput>, _: &(), di: &mut DataInit<'_, State>) {
        let o = di.init(r, ());
        State::send_output(&st.out, &o);
        st.outputs.push(o);
    }
}

impl GlobalDispatch<wp_presentation::WpPresentation, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, r: New<wp_presentation::WpPresentation>, _: &(), di: &mut DataInit<'_, State>) {
        let p = di.init(r, ());
        p.clock_id(libc::CLOCK_MONOTONIC as u32);
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for State {
    fn request(st: &mut State, _: &Client, _: &wl_compositor::WlCompositor, req: wl_compositor::Request, _: &(), _: &DisplayHandle, di: &mut DataInit<'_, State>) {
        match req {
            wl_compositor::Request::CreateSurface { id } => {
                let s = di.init(id, ());
                let oid = s.id();
                st.surfaces.insert(
                    oid.clone(),
                    Surface {
                        res: s,
                        pending: SurfState::default(),
                        cached: None,
                        buffer: None,
                        src: None,
                        dst: None,
                        scale: 1,
                        input: None,
                        role: Role::None,
                        parent: None,
                        pos: (0, 0),
                        sync: true,
                        children: vec![oid],
                        self_index: 0,
                        callbacks: Vec::new(),
                        feedbacks: Vec::new(),
                    },
                );
            }
            wl_compositor::Request::CreateRegion { id } => {
                let r = di.init(id, ());
                st.regions.insert(r.id(), Vec::new());
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_region::WlRegion, ()> for State {
    fn request(st: &mut State, _: &Client, r: &wl_region::WlRegion, req: wl_region::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {
        match req {
            wl_region::Request::Add { x, y, width, height } => {
                st.regions.entry(r.id()).or_default().push((true, Rect::new(x, y, width, height)))
            }
            wl_region::Request::Subtract { x, y, width, height } => {
                st.regions.entry(r.id()).or_default().push((false, Rect::new(x, y, width, height)))
            }
            _ => {}
        }
    }
    fn destroyed(st: &mut State, _: ClientId, r: &wl_region::WlRegion, _: &()) {
        st.regions.remove(&r.id());
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn request(st: &mut State, _: &Client, s: &wl_surface::WlSurface, req: wl_surface::Request, _: &(), _: &DisplayHandle, di: &mut DataInit<'_, State>) {
        let oid = s.id();
        match req {
            wl_surface::Request::Attach { buffer, .. } => {
                if let Some(sf) = st.surfaces.get_mut(&oid) {
                    sf.pending.buffer = Some(buffer);
                }
            }
            wl_surface::Request::Damage { .. } | wl_surface::Request::DamageBuffer { .. } => {
                if let Some(sf) = st.surfaces.get_mut(&oid) {
                    sf.pending.damage = true;
                }
            }
            wl_surface::Request::Frame { callback } => {
                let cb = di.init(callback, ());
                if let Some(sf) = st.surfaces.get_mut(&oid) {
                    sf.pending.callbacks.push(cb);
                }
            }
            wl_surface::Request::SetInputRegion { region } => {
                let ops = region.map(|r| st.regions.get(&r.id()).cloned().unwrap_or_default());
                if let Some(sf) = st.surfaces.get_mut(&oid) {
                    sf.pending.input = Some(ops);
                }
            }
            wl_surface::Request::SetBufferScale { scale } => {
                if let Some(sf) = st.surfaces.get_mut(&oid) {
                    sf.pending.scale = Some(scale);
                }
            }
            wl_surface::Request::Commit => st.commit(&oid),
            _ => {}
        }
    }
    fn destroyed(st: &mut State, _: ClientId, s: &wl_surface::WlSurface, _: &()) {
        let oid = s.id();
        if let Some(sf) = st.surfaces.remove(&oid) {
            if let Some(p) = sf.parent {
                if let Some(ps) = st.surfaces.get_mut(&p) {
                    if let Some(i) = ps.children.iter().position(|c| c == &oid) {
                        ps.children.remove(i);
                        if i < ps.self_index {
                            ps.self_index -= 1;
                        }
                    }
                }
                if let Some(w) = st.root_window(p) {
                    if let Some(w) = st.windows.get_mut(&w) {
                        w.dirty = true;
                    }
                }
            }
            if let Some(b) = sf.buffer {
                b.release();
            }
        }
        if st.pointer_focus.as_ref().map(|(_, s)| s) == Some(&oid) {
            st.pointer_focus = None;
        }
        st.touch_focus.retain(|_, v| v.1 != oid);
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn request(_: &mut State, _: &Client, _: &wl_callback::WlCallback, _: wl_callback::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn request(st: &mut State, _: &Client, _: &wl_shm::WlShm, req: wl_shm::Request, _: &(), _: &DisplayHandle, di: &mut DataInit<'_, State>) {
        if let wl_shm::Request::CreatePool { id, fd, size } = req {
            let pool = di.init(id, ());
            match Pool::map(fd, size.max(0) as usize) {
                Ok(p) => {
                    st.pools.insert(pool.id(), Arc::new(p));
                }
                Err(e) => {
                    logf!("shm pool map failed: {e}");
                    pool.post_error(wl_shm::Error::InvalidFd, "mmap failed");
                }
            }
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn request(st: &mut State, _: &Client, pool: &wl_shm_pool::WlShmPool, req: wl_shm_pool::Request, _: &(), _: &DisplayHandle, di: &mut DataInit<'_, State>) {
        match req {
            wl_shm_pool::Request::CreateBuffer { id, offset, width, height, stride, format } => {
                let buffer = di.init(id, ());
                let fmt: u32 = match format {
                    WEnum::Value(f) => f.into(),
                    WEnum::Unknown(u) => u,
                };
                let Some(p) = st.pools.get(&pool.id()).cloned() else { return };
                let len = stride as usize * height as usize;
                if offset < 0 || width <= 0 || height <= 0 || offset as usize + len > p.size {
                    pool.post_error(wl_shm::Error::InvalidStride, "buffer outside the pool");
                    return;
                }
                let addr = p.ptr as usize + offset as usize;
                st.next_buffer += 1;
                let host_id = st.next_buffer;
                let solid = (width == 1 && height == 1).then_some(addr as *const u32);
                match gpa_ranges(&mut st.pagemap, addr, len) {
                    Ok(ranges) => st.link.send(&ToHost::BufferNew {
                        id: host_id,
                        width: width as u32,
                        height: height as u32,
                        stride: stride as u32,
                        format: fmt,
                        ranges,
                    }),
                    Err(e) => logf!("buffer {host_id}: no guest-physical pages: {e}"),
                }
                st.buffers.insert(
                    buffer.id(),
                    BufferInfo { host_id, width: width as u32, height: height as u32, format: fmt, _pool: Some(p), gpu: None, solid },
                );
            }
            wl_shm_pool::Request::Resize { size } => {
                // remap: buffers made earlier keep their own mapping
                if let Some(old) = st.pools.get(&pool.id()) {
                    let fd = unsafe { libc::dup(old._fd.as_raw_fd()) };
                    if fd >= 0 {
                        if let Ok(p) = Pool::map(unsafe { OwnedFd::from_raw_fd(fd) }, size as usize) {
                            st.pools.insert(pool.id(), Arc::new(p));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    fn destroyed(st: &mut State, _: ClientId, pool: &wl_shm_pool::WlShmPool, _: &()) {
        st.pools.remove(&pool.id());
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn request(_: &mut State, _: &Client, _: &wl_buffer::WlBuffer, _: wl_buffer::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}
    fn destroyed(st: &mut State, _: ClientId, b: &wl_buffer::WlBuffer, _: &()) {
        if let Some(info) = st.buffers.remove(&b.id()) {
            st.link.send(&ToHost::BufferDestroy { id: info.host_id });
            if let (Some(g), Some(drm)) = (info.gpu, &st.drm) {
                if let Ok(g) = Arc::try_unwrap(g) {
                    drm.release(&g);
                }
            }
        }
        st.readback.retain(|x| x != &b.id());
    }
}

impl Dispatch<wl_subcompositor::WlSubcompositor, ()> for State {
    fn request(st: &mut State, _: &Client, _: &wl_subcompositor::WlSubcompositor, req: wl_subcompositor::Request, _: &(), _: &DisplayHandle, di: &mut DataInit<'_, State>) {
        if let wl_subcompositor::Request::GetSubsurface { id, surface, parent } = req {
            let sub = di.init(id, surface.id());
            let _ = sub;
            let (sid, pid) = (surface.id(), parent.id());
            if let Some(s) = st.surfaces.get_mut(&sid) {
                s.role = Role::Subsurface { parent_sync: true };
                s.parent = Some(pid.clone());
                s.sync = true;
            }
            if let Some(p) = st.surfaces.get_mut(&pid) {
                p.children.push(sid); // new subsurfaces go on top
            }
        }
    }
}

impl Dispatch<wl_subsurface::WlSubsurface, ObjectId> for State {
    fn request(st: &mut State, _: &Client, _: &wl_subsurface::WlSubsurface, req: wl_subsurface::Request, sid: &ObjectId, _: &DisplayHandle, _: &mut DataInit<'_, State>) {
        let above = matches!(req, wl_subsurface::Request::PlaceAbove { .. });
        match req {
            wl_subsurface::Request::SetPosition { x, y } => {
                if let Some(s) = st.surfaces.get_mut(sid) {
                    s.pending.position = Some((x, y));
                }
            }
            wl_subsurface::Request::SetSync => {
                if let Some(s) = st.surfaces.get_mut(sid) {
                    s.sync = true;
                }
            }
            wl_subsurface::Request::SetDesync => {
                if let Some(s) = st.surfaces.get_mut(sid) {
                    s.sync = false;
                }
                // cached state applies when leaving sync mode
                if !st.effectively_sync(sid) {
                    if let Some(c) = st.surfaces.get_mut(sid).and_then(|s| s.cached.take()) {
                        st.apply(sid, c);
                        st.after_commit(sid);
                    }
                }
            }
            wl_subsurface::Request::PlaceAbove { sibling } | wl_subsurface::Request::PlaceBelow { sibling } => {
                let Some(parent) = st.surfaces.get(sid).and_then(|s| s.parent.clone()) else { return };
                let Some(p) = st.surfaces.get_mut(&parent) else { return };
                let me_self = p.children[p.self_index].clone();
                p.children.retain(|c| c != sid);
                let target = sibling.id();
                let idx = p.children.iter().position(|c| *c == target).unwrap_or(p.children.len().saturating_sub(1));
                let at = if above { idx + 1 } else { idx };
                p.children.insert(at.min(p.children.len()), sid.clone());
                p.self_index = p.children.iter().position(|c| *c == me_self).unwrap_or(0);
            }
            _ => {}
        }
    }
    fn destroyed(st: &mut State, _: ClientId, _: &wl_subsurface::WlSubsurface, sid: &ObjectId) {
        let parent = st.surfaces.get(sid).and_then(|s| s.parent.clone());
        if let Some(p) = parent.and_then(|p| st.surfaces.get_mut(&p)) {
            if let Some(i) = p.children.iter().position(|c| c == sid) {
                p.children.remove(i);
                if i < p.self_index {
                    p.self_index -= 1;
                }
            }
        }
        if let Some(w) = st.root_window(sid.clone()) {
            if let Some(w) = st.windows.get_mut(&w) {
                w.dirty = true;
            }
        }
        if let Some(s) = st.surfaces.get_mut(sid) {
            s.role = Role::None;
            s.parent = None;
        }
    }
}

impl Dispatch<wp_viewporter::WpViewporter, ()> for State {
    fn request(st: &mut State, _: &Client, _: &wp_viewporter::WpViewporter, req: wp_viewporter::Request, _: &(), _: &DisplayHandle, di: &mut DataInit<'_, State>) {
        if let wp_viewporter::Request::GetViewport { id, surface } = req {
            let v = di.init(id, ());
            st.viewports.insert(v.id(), surface.id());
        }
    }
}

impl Dispatch<wp_viewport::WpViewport, ()> for State {
    fn request(st: &mut State, _: &Client, v: &wp_viewport::WpViewport, req: wp_viewport::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {
        let Some(sid) = st.viewports.get(&v.id()).cloned() else { return };
        let Some(s) = st.surfaces.get_mut(&sid) else { return };
        match req {
            wp_viewport::Request::SetSource { x, y, width, height } => {
                s.pending.src = Some(if width < 0.0 { None } else { Some(SrcRect { x, y, w: width, h: height }) });
            }
            wp_viewport::Request::SetDestination { width, height } => {
                s.pending.dst = Some(if width <= 0 { None } else { Some((width, height)) });
            }
            _ => {}
        }
    }
    fn destroyed(st: &mut State, _: ClientId, v: &wp_viewport::WpViewport, _: &()) {
        if let Some(sid) = st.viewports.remove(&v.id()) {
            if let Some(s) = st.surfaces.get_mut(&sid) {
                s.pending.src = Some(None);
                s.pending.dst = Some(None);
            }
        }
    }
}

impl Dispatch<wp_presentation::WpPresentation, ()> for State {
    fn request(st: &mut State, _: &Client, _: &wp_presentation::WpPresentation, req: wp_presentation::Request, _: &(), _: &DisplayHandle, di: &mut DataInit<'_, State>) {
        if let wp_presentation::Request::Feedback { surface, callback } = req {
            let fb = di.init(callback, ());
            if let Some(s) = st.surfaces.get_mut(&surface.id()) {
                s.pending.feedbacks.push(fb);
            }
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, ()> for State {
    fn request(_: &mut State, _: &Client, _: &wp_presentation_feedback::WpPresentationFeedback, _: wp_presentation_feedback::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn request(st: &mut State, _: &Client, _: &xdg_wm_base::XdgWmBase, req: xdg_wm_base::Request, _: &(), _: &DisplayHandle, di: &mut DataInit<'_, State>) {
        match req {
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                let x = di.init(id, ());
                st.xdg_surfaces.insert(x.id(), surface.id());
            }
            xdg_wm_base::Request::CreatePositioner { id } => {
                di.init(id, ());
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_positioner::XdgPositioner, ()> for State {
    fn request(_: &mut State, _: &Client, _: &xdg_positioner::XdgPositioner, _: xdg_positioner::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}
}

impl Dispatch<xdg_popup::XdgPopup, ()> for State {
    fn request(_: &mut State, _: &Client, _: &xdg_popup::XdgPopup, _: xdg_popup::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn request(st: &mut State, _: &Client, x: &xdg_surface::XdgSurface, req: xdg_surface::Request, _: &(), _: &DisplayHandle, di: &mut DataInit<'_, State>) {
        match req {
            xdg_surface::Request::GetToplevel { id } => {
                let tl = di.init(id, ());
                let Some(sid) = st.xdg_surfaces.get(&x.id()).cloned() else { return };
                st.next_window += 1;
                let wid = st.next_window;
                if let Some(s) = st.surfaces.get_mut(&sid) {
                    s.role = Role::Toplevel(wid);
                }
                st.toplevels.insert(tl.id(), wid);
                st.windows.insert(
                    wid,
                    Window {
                        id: wid,
                        surface: sid,
                        xdg_surface: x.clone(),
                        toplevel: tl,
                        title: String::new(),
                        app_id: String::new(),
                        geometry: None,
                        announced: false,
                        configured: false,
                        maximized: false,
                        dirty: false,
                        serial: 0,
                        in_flight: None,
                        pending_release: Vec::new(),
                    },
                );
            }
            xdg_surface::Request::GetPopup { id, .. } => {
                di.init(id, ());
            }
            xdg_surface::Request::SetWindowGeometry { x: gx, y: gy, width, height } => {
                let sid = st.xdg_surfaces.get(&x.id()).cloned();
                if let Some(wid) = sid.and_then(|s| st.root_window(s)) {
                    if let Some(w) = st.windows.get_mut(&wid) {
                        w.geometry = Some(Rect::new(gx, gy, width, height));
                    }
                }
            }
            _ => {}
        }
    }
    fn destroyed(st: &mut State, _: ClientId, x: &xdg_surface::XdgSurface, _: &()) {
        st.xdg_surfaces.remove(&x.id());
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn request(st: &mut State, _: &Client, tl: &xdg_toplevel::XdgToplevel, req: xdg_toplevel::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {
        let Some(wid) = st.toplevels.get(&tl.id()).copied() else { return };
        let mut update = false;
        match req {
            xdg_toplevel::Request::SetTitle { title } => {
                if let Some(w) = st.windows.get_mut(&wid) {
                    w.title = title;
                    update = w.announced;
                }
            }
            xdg_toplevel::Request::SetAppId { app_id } => {
                if let Some(w) = st.windows.get_mut(&wid) {
                    w.app_id = app_id;
                    update = w.announced;
                }
            }
            xdg_toplevel::Request::SetMaximized | xdg_toplevel::Request::UnsetMaximized => {
                let max = matches!(req, xdg_toplevel::Request::SetMaximized);
                if let Some(w) = st.windows.get_mut(&wid) {
                    w.maximized = max;
                    update = w.announced;
                    if w.configured {
                        st.configure_window(wid);
                    }
                }
            }
            _ => {}
        }
        if update {
            if let Some(w) = st.windows.get(&wid) {
                st.link.send(&ToHost::WindowUpdate { id: wid, title: w.title.clone(), app_id: w.app_id.clone(), maximized: w.maximized });
            }
        }
    }
    fn destroyed(st: &mut State, _: ClientId, tl: &xdg_toplevel::XdgToplevel, _: &()) {
        if let Some(wid) = st.toplevels.remove(&tl.id()) {
            if let Some(w) = st.windows.remove(&wid) {
                if let Some(s) = st.surfaces.get_mut(&w.surface) {
                    s.role = Role::None;
                }
                if let Some(f) = w.in_flight {
                    for b in f.release {
                        b.release();
                    }
                }
                for b in w.pending_release {
                    b.release();
                }
                if w.announced {
                    st.link.send(&ToHost::WindowDestroy { id: wid });
                }
            }
            if st.keyboard_focus == Some(wid) {
                st.keyboard_focus = None;
            }
            if st.pointer_focus.as_ref().map(|p| p.0) == Some(wid) {
                st.pointer_focus = None;
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn request(st: &mut State, client: &Client, _: &wl_seat::WlSeat, req: wl_seat::Request, _: &(), _: &DisplayHandle, di: &mut DataInit<'_, State>) {
        let seat = st.seats.entry(client.id()).or_default();
        seat.client.get_or_insert_with(|| client.clone());
        match req {
            wl_seat::Request::GetPointer { id } => seat.pointers.push(di.init(id, ())),
            wl_seat::Request::GetKeyboard { id } => {
                let k = di.init(id, ());
                State::send_keymap(&k);
                seat.keyboards.push(k);
            }
            wl_seat::Request::GetTouch { id } => seat.touches.push(di.init(id, ())),
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn request(st: &mut State, _: &Client, _: &wl_pointer::WlPointer, req: wl_pointer::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {
        if let wl_pointer::Request::SetCursor { surface, hotspot_x, hotspot_y, .. } = req {
            match surface {
                Some(s) => {
                    if let Some(sf) = st.surfaces.get_mut(&s.id()) {
                        sf.role = Role::Cursor;
                    }
                    st.cursor_surface = Some(s.id());
                    let buf = st
                        .surfaces
                        .get(&s.id())
                        .and_then(|sf| sf.buffer.as_ref())
                        .and_then(|b| st.buffers.get(&b.id()))
                        .map(|b| b.host_id);
                    if let Some((wid, _)) = st.pointer_focus.clone() {
                        st.link.send(&ToHost::Cursor { window: wid, buffer: buf, hot_x: hotspot_x, hot_y: hotspot_y });
                    }
                }
                None => {
                    if let Some((wid, _)) = st.pointer_focus.clone() {
                        st.link.send(&ToHost::Cursor { window: wid, buffer: None, hot_x: 0, hot_y: 0 });
                    }
                }
            }
        }
    }
    fn destroyed(st: &mut State, client: ClientId, p: &wl_pointer::WlPointer, _: &()) {
        if let Some(s) = st.seats.get_mut(&client) {
            s.pointers.retain(|x| x != p);
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn request(_: &mut State, _: &Client, _: &wl_keyboard::WlKeyboard, _: wl_keyboard::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}
    fn destroyed(st: &mut State, client: ClientId, k: &wl_keyboard::WlKeyboard, _: &()) {
        if let Some(s) = st.seats.get_mut(&client) {
            s.keyboards.retain(|x| x != k);
        }
    }
}

impl Dispatch<wl_touch::WlTouch, ()> for State {
    fn request(_: &mut State, _: &Client, _: &wl_touch::WlTouch, _: wl_touch::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}
    fn destroyed(st: &mut State, client: ClientId, t: &wl_touch::WlTouch, _: &()) {
        if let Some(s) = st.seats.get_mut(&client) {
            s.touches.retain(|x| x != t);
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn request(_: &mut State, _: &Client, _: &wl_output::WlOutput, _: wl_output::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {}
    fn destroyed(st: &mut State, _: ClientId, o: &wl_output::WlOutput, _: &()) {
        st.outputs.retain(|x| x != o);
    }
}

/// Mime types a data source offers.
#[derive(Default)]
struct SourceMimes(Mutex<Vec<String>>);

impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for State {
    fn request(st: &mut State, client: &Client, _: &wl_data_device_manager::WlDataDeviceManager, req: wl_data_device_manager::Request, _: &(), dh: &DisplayHandle, di: &mut DataInit<'_, State>) {
        match req {
            wl_data_device_manager::Request::CreateDataSource { id } => {
                di.init(id, SourceMimes::default());
            }
            wl_data_device_manager::Request::GetDataDevice { id, .. } => {
                let dev = di.init(id, ());
                let seat = st.seats.entry(client.id()).or_default();
                seat.client.get_or_insert_with(|| client.clone());
                seat.data_devices.push(dev);
                if let Some(text) = st.host_clipboard.clone() {
                    st.offer_clipboard(dh, text);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, SourceMimes> for State {
    fn request(_: &mut State, _: &Client, _: &wl_data_source::WlDataSource, req: wl_data_source::Request, mimes: &SourceMimes, _: &DisplayHandle, _: &mut DataInit<'_, State>) {
        if let wl_data_source::Request::Offer { mime_type } = req {
            mimes.0.lock().unwrap().push(mime_type);
        }
    }
    fn destroyed(st: &mut State, _: ClientId, s: &wl_data_source::WlDataSource, _: &SourceMimes) {
        if st.selection.as_ref() == Some(s) {
            st.selection = None;
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn request(st: &mut State, _: &Client, _: &wl_data_device::WlDataDevice, req: wl_data_device::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {
        if let wl_data_device::Request::SetSelection { source: Some(source), .. } = req {
            let mimes = source.data::<SourceMimes>().map(|m| m.0.lock().unwrap().clone()).unwrap_or_default();
            st.take_selection(source, mimes);
        }
    }
    fn destroyed(st: &mut State, client: ClientId, d: &wl_data_device::WlDataDevice, _: &()) {
        if let Some(s) = st.seats.get_mut(&client) {
            s.data_devices.retain(|x| x != d);
        }
    }
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn request(st: &mut State, _: &Client, _: &wl_data_offer::WlDataOffer, req: wl_data_offer::Request, _: &(), _: &DisplayHandle, _: &mut DataInit<'_, State>) {
        if let wl_data_offer::Request::Receive { fd, .. } = req {
            let text = st.host_clipboard.clone().unwrap_or_default();
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = std::fs::File::from(fd).write_all(text.as_bytes());
            });
        }
    }
}

// ---- linux-dmabuf (gpu=virgl) ------------------------------------------------------

// DRM fourccs
const DRM_ARGB8888: u32 = 0x3432_5241;
const DRM_XRGB8888: u32 = 0x3432_5258;
const DRM_ABGR8888: u32 = 0x3432_4241;
const DRM_XBGR8888: u32 = 0x3432_4258;

/// DRM fourcc -> the protocol's (wl_shm) format code.
fn drm_to_proto(f: u32) -> Option<u32> {
    Some(match f {
        DRM_ARGB8888 => format::ARGB8888,
        DRM_XRGB8888 => format::XRGB8888,
        DRM_ABGR8888 => format::ABGR8888,
        DRM_XBGR8888 => format::XBGR8888,
        _ => return None,
    })
}

impl GlobalDispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for State {
    fn bind(_: &mut State, _: &DisplayHandle, _: &Client, r: New<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>, _: &(), di: &mut DataInit<'_, State>) {
        let d = di.init(r, ());
        for f in [DRM_ARGB8888, DRM_XRGB8888, DRM_ABGR8888, DRM_XBGR8888] {
            if d.version() >= 3 {
                // linear only: what a transfer back into guest pages is laid out as
                d.modifier(f, 0, 0);
            } else {
                d.format(f);
            }
        }
    }
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for State {
    fn request(_: &mut State, _: &Client, _: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, req: zwp_linux_dmabuf_v1::Request, _: &(), _: &DisplayHandle, di: &mut DataInit<'_, State>) {
        if let zwp_linux_dmabuf_v1::Request::CreateParams { params_id } = req {
            di.init(params_id, DmabufParams::default());
        }
    }
}

#[derive(Default)]
struct DmabufParams(Mutex<Vec<(OwnedFd, u32, u32, u32)>>); // fd, plane, offset, stride

impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, DmabufParams> for State {
    fn request(
        st: &mut State,
        _: &Client,
        params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        req: zwp_linux_buffer_params_v1::Request,
        planes: &DmabufParams,
        _: &DisplayHandle,
        di: &mut DataInit<'_, State>,
    ) {
        match req {
            zwp_linux_buffer_params_v1::Request::Add { fd, plane_idx, offset, stride, .. } => {
                planes.0.lock().unwrap().push((fd, plane_idx, offset, stride));
            }
            zwp_linux_buffer_params_v1::Request::CreateImmed { buffer_id, width, height, format: fourcc, .. } => {
                let buffer = di.init(buffer_id, ());
                let planes = std::mem::take(&mut *planes.0.lock().unwrap());
                let Some((fd, _, offset, stride)) = planes.into_iter().find(|p| p.1 == 0) else {
                    params.post_error(zwp_linux_buffer_params_v1::Error::Incomplete, "no plane 0");
                    return;
                };
                let Some(fmt) = drm_to_proto(fourcc) else {
                    params.post_error(zwp_linux_buffer_params_v1::Error::InvalidFormat, "format not supported");
                    return;
                };
                let Some(drm) = &st.drm else { return };
                let g = match drm.import(&fd, width as u32, height as u32, stride) {
                    Ok(g) => Arc::new(g),
                    Err(e) => {
                        logf!("dma-buf import: {e}");
                        params.post_error(zwp_linux_buffer_params_v1::Error::InvalidWlBuffer, "import failed");
                        return;
                    }
                };
                st.next_buffer += 1;
                let host_id = st.next_buffer;
                let addr = g.ptr as usize + offset as usize;
                let len = (stride as usize * height as usize).min(g.len.saturating_sub(offset as usize));
                match gpa_ranges(&mut st.pagemap, addr, len) {
                    Ok(ranges) => st.link.send(&ToHost::BufferNew { id: host_id, width: width as u32, height: height as u32, stride, format: fmt, ranges }),
                    Err(e) => logf!("gpu buffer {host_id}: no guest-physical pages: {e}"),
                }
                st.buffers.insert(
                    buffer.id(),
                    BufferInfo { host_id, width: width as u32, height: height as u32, format: fmt, _pool: None, gpu: Some(g), solid: None },
                );
            }
            zwp_linux_buffer_params_v1::Request::Create { .. } => {
                // only create_immed (what the hwcomposer uses)
                params.failed();
            }
            _ => {}
        }
    }
}

// ---- event loop ----------------------------------------------------------------

/// Host events for the compositor thread.
pub struct Inbox {
    queue: Mutex<Vec<ToGuest>>,
    wake: OwnedFd,
}

impl Inbox {
    pub fn push(&self, m: ToGuest) {
        self.queue.lock().unwrap().push(m);
        let one: u64 = 1;
        unsafe { libc::write(self.wake.as_raw_fd(), &one as *const u64 as *const libc::c_void, 8) };
    }
}

pub fn new_inbox() -> Arc<Inbox> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    assert!(fd >= 0, "eventfd");
    Arc::new(Inbox { queue: Mutex::new(Vec::new()), wake: unsafe { OwnedFd::from_raw_fd(fd) } })
}

/// Runs the compositor on the calling thread; `socket` is the listening path.
pub fn run(link: HostLink, inbox: Arc<Inbox>, socket: &str, out: OutputCfg) -> std::io::Result<()> {
    let mut display: Display<State> = Display::new().map_err(|e| std::io::Error::other(format!("wayland display: {e}")))?;
    let dh = display.handle();
    dh.create_global::<State, wl_compositor::WlCompositor, ()>(5, ());
    dh.create_global::<State, wl_subcompositor::WlSubcompositor, ()>(1, ());
    dh.create_global::<State, wl_shm::WlShm, ()>(1, ());
    dh.create_global::<State, xdg_wm_base::XdgWmBase, ()>(2, ());
    dh.create_global::<State, wl_seat::WlSeat, ()>(5, ());
    dh.create_global::<State, wl_output::WlOutput, ()>(3, ());
    dh.create_global::<State, wp_viewporter::WpViewporter, ()>(1, ());
    dh.create_global::<State, wp_presentation::WpPresentation, ()>(1, ());
    dh.create_global::<State, wl_data_device_manager::WlDataDeviceManager, ()>(3, ());
    // GPU buffers only with a virtio-gpu (gpu=virgl); else Android uses shm
    let drm = crate::drm::Drm::open().ok();
    if drm.is_some() {
        dh.create_global::<State, zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()>(3, ());
        logf!("virtio-gpu present: dma-buf buffers enabled");
    }

    let _ = std::fs::remove_file(socket);
    let listener = ListeningSocket::bind_absolute(socket.into()).map_err(|e| std::io::Error::other(format!("bind {socket}: {e}")))?;
    let _ = sys::chmod(socket, 0o777);

    let mut st = State {
        link,
        drm,
        readback: Vec::new(),
        out,
        pagemap: std::fs::File::open("/proc/self/pagemap")?,
        surfaces: HashMap::new(),
        buffers: HashMap::new(),
        pools: HashMap::new(),
        regions: HashMap::new(),
        viewports: HashMap::new(),
        xdg_surfaces: HashMap::new(),
        windows: HashMap::new(),
        toplevels: HashMap::new(),
        seats: HashMap::new(),
        outputs: Vec::new(),
        next_buffer: 0,
        next_window: 0,
        serial: 0,
        pointer_focus: None,
        keyboard_focus: None,
        touch_focus: HashMap::new(),
        pressed_keys: Vec::new(),
        cursor_surface: None,
        selection: None,
        host_clipboard: None,
        epoch: std::time::Instant::now(),
    };
    logf!("wayland compositor listening on {socket}");

    loop {
        let mut fds = [
            libc::pollfd { fd: listener.as_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: display.backend().poll_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: inbox.wake.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        // wake at least every 100 ms for the frame watchdog
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, 100) };
        if fds[0].revents & libc::POLLIN != 0 {
            while let Ok(Some(stream)) = listener.accept() {
                match display.handle().insert_client(stream, Arc::new(ClientState)) {
                    Ok(_) => logf!("wayland client connected"),
                    Err(e) => logf!("wayland client rejected: {e}"),
                }
            }
        }
        if fds[2].revents & libc::POLLIN != 0 {
            let mut v = 0u64;
            unsafe { libc::read(inbox.wake.as_raw_fd(), &mut v as *mut u64 as *mut libc::c_void, 8) };
        }
        let msgs = std::mem::take(&mut *inbox.queue.lock().unwrap());
        for m in msgs {
            match m {
                ToGuest::FrameDone { window, serial, time_ns } => st.frame_done(window, serial, time_ns),
                ToGuest::PointerMotion { window, x, y } => st.pointer_motion(window, x as f64, y as f64),
                ToGuest::PointerButton { button, pressed, .. } => st.pointer_button(button, pressed),
                ToGuest::PointerAxis { dx, dy, discrete_x, discrete_y, .. } => st.pointer_axis(dx as f64, dy as f64, discrete_x, discrete_y),
                ToGuest::PointerLeave { .. } => st.pointer_leave(),
                ToGuest::Touch { window, id, kind, x, y } => st.touch(window, id, kind, x as f64, y as f64),
                ToGuest::Key { window, code, pressed } => st.key(window, code, pressed),
                ToGuest::Focus { window } => st.focus(window),
                ToGuest::WindowResize { id, width, height } => st.window_resize(id, width, height),
                ToGuest::WindowClose { id } => st.window_close(id),
                ToGuest::TaskBounds { id, x, y, width, height } => {
                    if let Some(w) = st.windows.get(&id) {
                        if let Some(package) = w.app_id.strip_prefix("waydroid.") {
                            crate::apps::resize_task(package.to_string(), x, y, width, height);
                        }
                    }
                }
                ToGuest::Output { width, height, refresh_mhz, dpi } => st.set_output(OutputCfg { width, height, refresh_mhz, dpi }),
                ToGuest::Clipboard { text } => st.offer_clipboard(&dh, text),
                _ => {}
            }
        }
        if let Err(e) = display.dispatch_clients(&mut st) {
            logf!("wayland dispatch: {e}");
        }
        st.flush_frames();
        let _ = display.flush_clients();
    }
}
