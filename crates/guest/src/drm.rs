//! The guest side of virtio-gpu buffers (gpu=virgl): Android's gralloc
//! (minigbm on Mesa's virgl) hands the hwcomposer dma-bufs of virtio-gpu
//! resources, which live in host GPU memory. Each one has guest pages as
//! its transfer backing; after Android renders, the compositor asks the
//! host to copy the resource into those pages (TRANSFER_FROM_HOST) and the
//! airDroid host reads them in place like any shm buffer.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const fn iowr(nr: u64, size: u64) -> u64 {
    (3 << 30) | (size << 16) | ((b'd' as u64) << 8) | nr
}
const fn iow(nr: u64, size: u64) -> u64 {
    (1 << 30) | (size << 16) | ((b'd' as u64) << 8) | nr
}
const DRM_COMMAND_BASE: u64 = 0x40;
const DRM_IOCTL_GEM_CLOSE: u64 = iow(0x09, 8);
const DRM_IOCTL_PRIME_FD_TO_HANDLE: u64 = iowr(0x2e, 12);
const DRM_IOCTL_VIRTGPU_MAP: u64 = iowr(DRM_COMMAND_BASE + 0x01, 16);
const DRM_IOCTL_VIRTGPU_RESOURCE_INFO: u64 = iowr(DRM_COMMAND_BASE + 0x05, 16);
const DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST: u64 = iowr(DRM_COMMAND_BASE + 0x06, 44);
const DRM_IOCTL_VIRTGPU_WAIT: u64 = iowr(DRM_COMMAND_BASE + 0x08, 8);
const DRM_IOCTL_VIRTGPU_CONTEXT_INIT: u64 = iowr(DRM_COMMAND_BASE + 0x0b, 16);
const VIRTGPU_CONTEXT_PARAM_CAPSET_ID: u64 = 1;
const VIRTIO_GPU_CAPSET_VIRGL2: u64 = 2;

pub struct Drm {
    fd: OwnedFd,
    /// GEM handles in use and how many imports hold each: the kernel
    /// gives one handle per dma-buf however often it is imported, and one
    /// GEM_CLOSE drops it for all of them.
    handles: std::sync::Mutex<std::collections::HashMap<u32, u32>>,
}

/// A virtio-gpu resource imported from a dma-buf, mapped (its guest
/// backing) and pinned.
pub struct GpuBuffer {
    pub handle: u32,
    /// the host's resource id
    pub res: u32,
    pub ptr: *mut u8,
    pub len: usize,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}
unsafe impl Send for GpuBuffer {}
unsafe impl Sync for GpuBuffer {}

fn ioctl<T>(fd: i32, req: u64, arg: &mut T) -> io::Result<()> {
    loop {
        let r = unsafe { libc::ioctl(fd, req as _, arg as *mut T) };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) && e.raw_os_error() != Some(libc::EAGAIN) {
            return Err(e);
        }
    }
}

impl Drm {
    pub fn open() -> io::Result<Drm> {
        for n in 128..136 {
            let p = crate::sys::cstr(format!("/dev/dri/renderD{n}"));
            let fd = unsafe { libc::open(p.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
            if fd >= 0 {
                let drm = Drm { fd: unsafe { OwnedFd::from_raw_fd(fd) }, handles: Default::default() };
                // Our own virgl context, before any import: with
                // context_init, importing a dma-buf does not create one,
                // and resources are attached only to an existing context
                // (else the host refuses our transfers: "Illegal resource").
                let params = [VIRTGPU_CONTEXT_PARAM_CAPSET_ID, VIRTIO_GPU_CAPSET_VIRGL2];
                // struct drm_virtgpu_context_init { u32 num_params; u32 pad; u64 ctx_set_params; }
                let mut init = [1u32, 0, 0, 0];
                let ptr = params.as_ptr() as u64;
                init[2] = ptr as u32;
                init[3] = (ptr >> 32) as u32;
                ioctl(drm.fd.as_raw_fd(), DRM_IOCTL_VIRTGPU_CONTEXT_INIT, &mut init)?;
                return Ok(drm);
            }
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "no virtio-gpu render node"))
    }

    /// The GEM handle of `dmabuf` (the same one for the same buffer), held
    /// until `unref_handle`.
    pub fn handle_of(&self, dmabuf: &OwnedFd) -> io::Result<u32> {
        // struct drm_prime_handle { u32 handle; u32 flags; s32 fd; }
        let mut prime = [0u32; 3];
        prime[2] = dmabuf.as_raw_fd() as u32;
        let mut handles = self.handles.lock().unwrap();
        ioctl(self.fd.as_raw_fd(), DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut prime)?;
        *handles.entry(prime[0]).or_insert(0) += 1;
        Ok(prime[0])
    }

    pub fn unref_handle(&self, handle: u32) {
        let mut handles = self.handles.lock().unwrap();
        match handles.get_mut(&handle) {
            Some(n) if *n > 1 => *n -= 1,
            Some(_) => {
                handles.remove(&handle);
                self.close(handle);
            }
            None => {}
        }
    }

    /// Maps the backing of a held handle (plane 0 of a linear buffer); the
    /// buffer takes the caller's hold on the handle over (`release`).
    pub fn map(&self, handle: u32, width: u32, height: u32, stride: u32) -> io::Result<GpuBuffer> {
        let fd = self.fd.as_raw_fd();
        // struct drm_virtgpu_resource_info { bo_handle, res_handle, size, blob_mem }
        let mut info = [0u32; 4];
        info[0] = handle;
        ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_INFO, &mut info)?;
        let len = (info[2] as usize).max(stride as usize * height as usize);
        // struct drm_virtgpu_map { u64 offset; u32 handle; u32 pad; }
        let mut map = [0u64; 2];
        map[1] = handle as u64;
        ioctl(fd, DRM_IOCTL_VIRTGPU_MAP, &mut map)?;
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd, map[0] as libc::off_t) };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // the host reads these pages by physical address: fault every page
        // in (mlock skips this kind of driver mapping); virtio-gpu keeps
        // the backing pinned while the resource exists
        for off in (0..len).step_by(4096) {
            unsafe { std::ptr::read_volatile((ptr as *const u8).add(off)) };
        }
        Ok(GpuBuffer { handle, res: info[1], ptr: ptr as *mut u8, len, width, height, stride })
    }

    /// Copies the resource's current contents (host GPU memory) into its
    /// guest backing and waits for it.
    pub fn read_back(&self, b: &GpuBuffer) -> io::Result<()> {
        let fd = self.fd.as_raw_fd();
        // struct drm_virtgpu_3d_transfer_from_host { u32 bo_handle;
        //   struct { u32 x, y, z, w, h, d; } box; u32 level, offset, stride, layer_stride; }
        let mut t = [0u32; 11];
        t[0] = b.handle;
        t[4] = b.width;
        t[5] = b.height;
        t[6] = 1;
        // stride and layer_stride stay 0: the kernel refuses them for
        // non-blob resources (the backing's layout is the resource's)
        ioctl(fd, DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST, &mut t)?;
        // struct drm_virtgpu_3d_wait { u32 handle; u32 flags; }
        let mut w = [b.handle, 0u32];
        ioctl(fd, DRM_IOCTL_VIRTGPU_WAIT, &mut w)
    }

    /// Waits until the GPU work Android submitted on the resource is done
    /// (the host reads it itself after that).
    pub fn wait(&self, b: &GpuBuffer) -> io::Result<()> {
        let mut w = [b.handle, 0u32];
        ioctl(self.fd.as_raw_fd(), DRM_IOCTL_VIRTGPU_WAIT, &mut w)
    }

    pub fn release(&self, b: &GpuBuffer) {
        unsafe {
            libc::munmap(b.ptr as *mut libc::c_void, b.len);
        }
        self.unref_handle(b.handle);
    }

    fn close(&self, handle: u32) {
        let mut c = [handle, 0u32];
        let _ = ioctl(self.fd.as_raw_fd(), DRM_IOCTL_GEM_CLOSE, &mut c);
    }
}
