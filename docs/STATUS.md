# Status

## Works (verified on air/OS in a VM and on Linux/KVM)

* Android 13 (LineageOS 20, Waydroid's images) boots under NVMM; first-run
  setup downloads and checks the images like `waydroid init`.
* Display: frames read in place from guest RAM; full UI in one window, or
  each app in its own borderless window at the position Android gives it
  (Waydroid's multi-window mode, the default).
* Input: mouse (buttons, wheel), keyboard (Linux key codes, modifiers),
  optional mouse-as-touch.
* Apps: install `.apk` files (Tracker, the file panel, `airdroid app
  install`), launch, list with names and icons, Deskbar launchers
  (Applications › Android apps), remove.
* Audio output (PulseAudio protocol in the guest, Media Kit on air/OS).
* Networking (QEMU user networking; Android sees Ethernet).
* Time zone: Android follows air/OS's (BTimeZone, `airdroid.tz=` on the
  guest command line; `cmd alarm set-timezone` after boot).
* Clipboard both ways (implemented; not yet verified on air/OS).
* `airdroid` command: status, shell, logcat, app …, prop, stop.

* GPU rendering, phase 1 (virgl): Android's GL goes through Mesa's virgl
  driver to virglrenderer in airDroid's QEMU, on the host's GPU through
  EGL (AMD: radeonsi; NVIDIA: Zink on NVK; no GPU: llvmpipe). On air/OS
  QEMU reads finished frames straight into the app's window bitmaps
  (qemu/patches/0103, below); elsewhere the guest reads them back into
  its pages. `gpu` setting: auto (virgl when there is a GPU), virgl,
  none (SwiftShader). Verified on the X399 (GTX 1080 Ti, Zink on NVK)
  and in the test VM with llvmpipe.

* Settings window (Android > Settings…): memory, processors, graphics
  (automatic, the GPU, software), screen density, mouse clicks as
  touches and the shared folder; they apply when Android starts next.
* Shared folder: `~/Android` on air/OS is `/sdcard/airOS` in Android
  (virtio-9p, mounted in Android's namespace over its storage, so every
  app and the Files app see it); setting `shared_folder` ("" to turn it
  off). Waydroid's data is a host folder; airDroid's is a disk image, so
  this is how files go back and forth.
* Microphone: Android's record streams (PulseAudio, through Waydroid's
  audio HAL) get the system's audio input (BMediaRecorder on the default
  input node, converted to the stream's rate, channels and format), or
  silence when there is none, so recording apps keep working. Verified on
  KVM with a test tone (recorded 440 Hz, level as sent) and on the air/OS
  test VM (no input there: silence); not yet with a real microphone.
* Multi-window resizing: the edges of an app's (borderless) window resize
  it on the desktop, and Android gives the app's task the new bounds
  (`am task resize`); moving works through Android's own caption bar.
* Notifications: Android's notifications show on the air/OS desktop
  (BNotification with the app's name and icon; clicking one starts the
  app). airDroid serves Waydroid's `waydroidnotifications` binder service
  from the VM, which LineageOS's Waydroid build posts to.
* `airdroid upgrade`: newer Android images from the OTA channels, then a
  restart (Android's data stays).

## On real hardware (X399, 2026-11-21, hrev60207+2797)

Threadripper (AMD, NVMM's SVM path) with a GTX 1080 Ti:

* Android booted in 32 s (about 300 s in the nested test VM).
* GPU: QEMU's GL was Summit's Mesa, Zink on NVK; virgl rendered Android
  correctly. Scrolling Settings: 330 frames in about 5 s, 0.3 % janky,
  median frame 9 ms (GPU 6 ms), 99th percentile 25 ms; the host presented
  about 65 frames a second.
* Quit with Android booted: 2 s, nothing left behind.
* The first session left Android's data disk (ext4 in the qcow2) corrupt,
  and the next test boot hung the whole computer (no ping, no syslog).
* The corruption was BFS, not NVMM: the same boot under TCG (no NVMM)
  corrupted the disk too, and a native test (bfs_concurrent_grow_test)
  lost 113 of 257 blocks written by concurrent writers to a growing file.
  BFS zeroed a growing write's gap after releasing the inode lock, over
  data other writers had put there meanwhile (QEMU's I/O threads on a
  growing qcow2). Fixed in haiku 4879942cdc; with it Android's first boot
  leaves a clean disk (e2fsck) in the x86_64 test VM. linuxvm (Docker)
  runs the same QEMU and was exposed to it too.
* Retested on hrev60207+2800 (with the BFS fix): four headless NVMM runs
  (8 vCPUs, Android's first boot, 35-40 s each) and two runs of the app
  (virgl, Settings scrolled: 333 frames, 0.3-0.6 % janky, median 10-19 ms;
  boot 28 s on a new disk, 19.8 s on the next start) all left a clean disk
  (e2fsck) and the computer never hung; the hang came with the corruption.
  airDroid runs on AMD again and is back in the images.

## Host frame path (X399, 2026-11-21)

The app's CPU time while Android scrolls Settings (multi-window, virgl,
~325 frames over 5-6 s, screen on; teamcpu = user + kernel time):

| build | app CPU | per frame |
|---|---|---|
| before (per-pixel compose, canvas copy) | 595-616 ms | ~1.9 ms |
| fast compose paths (29a5d23) | 327-339 ms | ~1.0 ms |
| composing into the window's bitmap (24971f0) | 244-246 ms | ~0.75 ms |
| QEMU reads GPU frames into the bitmap (present=host) | 84-99 ms | ~0.28 ms |

The last row also removes the guest's read-back: it took 2.93 ms per
frame (average, 7.3 ms at most; a TRANSFER_FROM_HOST and waiting for
it); the guest now only waits for Android's rendering (0.00-0.01 ms).
QEMU reads a frame into the window in 1.4-1.6 ms, and its own CPU time
did not grow (7.8-8.5 s over the run, mostly Android's vCPUs, as before).

On the build host (Xeon E5-2650 v4) composing a 1920x1080 frame: opaque
XRGB + translucent bar 7.9 -> 1.4 ms; an RGBA app window that Android
does not mark opaque 6.7 -> 1.7 ms. Android's own
frame times (median 10-20 ms, 0.3-1.2 % janky) come from Android and
virgl, not the host path.

## Against Waydroid (X399, 2026-11-21)

airDroid's side, default mode (virgl, 8 processors and 4 GB for
Android, LineageOS 20.0-20260927 VANILLA, 1920x1080, GTX 1070 through
Zink on NVK on NVIDIA's resource manager):

| | airDroid on air/OS |
|---|---|
| Android boot (start to `sys.boot_completed`) | 19.2 s (21 s from starting the app) |
| Settings cold start (`am start -W`, TotalTime) | 471-486 ms |
| Clock cold start | 580 ms (WaitTime 587-818 ms) |
| Scroll test (6 swipe pairs in Settings) | 305-331 frames, 0.3-0.7 % janky, every one on screen (302-326 host frames) |
| Frame time, median / 90th / 99th percentile | 5-11 / 11-13 / 16-19 ms (8-26 / 20-29 / 22-48 ms before the fence fix below) |
| airDroid's own CPU time for the test | 94-107 ms |

Measure with the app's window on screen: start it once with
`airdroid app launch` (a bare `am start` leaves `waydroid.active_apps`
unset, the hardware composer then shows nothing, and frame times come
out at 5-6 ms because nothing is presented; a first version of this
table had that mistake).

Waydroid's side on the same machine: not measured yet. The attempt
(2026-11-21) was to boot a Linux live image from the lab KVM's virtual
media; the KVM's USB link to the X399 is not connected (its gadget
reads "not attached", the X399 lists no such USB device), so the image
never reached the firmware. With that cable in, mount the rescue image
as a CD and repeat the same three measurements under Waydroid. Expect
the graphics numbers to say more about nouveau than about Waydroid:
on Linux this GTX 1070 has only nouveau, which cannot raise the card's
clocks; boot and app start are the fair part.

## Where a frame's time went (X399, 2026-11-22)

Android's per-frame timestamps (`dumpsys gfxinfo <app> framestats`)
for the Settings scroll test, median / 90th percentile in ms:

| | before | after |
|---|---|---|
| wait for the render thread | 10.4 / 11.8 | 0.1 / 0.7 |
| waiting for a free buffer (dequeue) | 14.3 / 15.4 | 0.01 / 0.01 |
| swap until the frame is done | 9.98 / 10.7 | 1.6 / 2.2 |
| vsync to frame done | 37.2 / 39.1 | 4.3 / 5.9 |
| frame interval | 16.67 | 16.67 |

Frames always came at the display's rate; each was 37 ms old when it
was done. A frame is done for Android when its virgl fence signals,
and QEMU looked for signalled fences on a 10 ms timer: every swap
waited for the next tick (9.98 ms at the median), the render thread
was still in that wait when the next frame arrived, and the app ran
out of buffers behind it. QEMU patch 0105 polls every millisecond
while fences are pending. The same in Vulkan mode (4.6 / 5.9 ms);
QEMU's CPU time for the test did not go up (7.5 s before, 6.6-7.2 s).

## "Show Android" mode paints what changed (X399, 2026-11-22)

A window of several layers (the whole of "Show Android" mode; an app
window with a video surface or a menu) was read out of the GPU and
blended whole for every frame: the Settings scroll test put 114 of
Android's 232 frames on screen, with 48 % of them janky and 24 ms of
airDroid's CPU time each.

* The window's two bitmaps take turns, so a frame repaints what the
  last two frames changed, and QEMU copies only that part of a layer
  out of the GPU (patch 0106: a read of a part of a resource goes to
  its place in the guest's backing).
* The guest passes each layer's damage on (`Frame.damage`, one
  rectangle a layer). "All of it" for the buffer a surface shows
  already counts as nothing: Android's composer says that with every
  commit of a layer it has no damage for.
* A layer that says "all of it" with a new buffer is checked: the new
  buffer is compared with the one before (0.7 ms for a screen). In this
  mode SurfaceFlinger composes each task's "Background" layer itself
  and hands its target over in another buffer every frame, with the
  same picture.
* What is left to blend goes in bands, a thread each, when it is large.

After: 262-292 of Android's 271-305 frames on screen, 30-39 % janky, 8.7-9 ms
of airDroid's CPU time a frame; a tenth of the screen repainted on
average, 3.8 ms to read and compose it. The window is the same picture
as Android's own screenshot (no pixel more than 5/255 off). What is
left is SurfaceFlinger's composition target: 8 MB out of the GPU for
every frame (6 ms in QEMU) to find that it has not changed. App windows
of one layer (the usual case) never took this path: 323 of 326 frames,
61 ms of CPU time for the whole test.

## "Show Android" mode and app starts (X399, 2026-11-21)

Starting an app in "Show Android" mode took Android's UI down, in both
gpu modes (found with the Godot editor): the hardware composer makes a
new wl_buffer of the same dma-buf for every layer on every frame there,
the guest compositor imported each one in full (map it, touch every
page, read the page map, describe it to the host: 3 ms; 2672 imports in
13 s with 9 buffers alive), fell behind, and stopped reading the
composer's requests; the composer's libwayland found its socket full
after about a second and the composer aborted.

* One import per dma-buf, kept a second after its last wl_buffer; GEM
  handles are counted (the kernel gives one handle per dma-buf, and the
  first release used to close it under a second import); a destroyed
  wl_buffer's contents stay until its surface gets another buffer.
* The guest writes to the host through a thread (a write waits for
  QEMU, which can be busy for seconds), and Android's sockets get 4 MiB
  of send buffer, so a stall is no longer fatal.
* The compositor says so on the VM console when its loop takes more
  than 200 ms ("compositor held up ...").

After: the same sequence (Show Android, start the app, recents, back,
start it again, home) with the Godot editor on GL and on Vulkan and
with Settings: no abort, nothing held up, frames at every step. The
scroll test in the default mode is unchanged (325-327 frames on screen,
median 19-20 ms) and airDroid's own CPU time for it went from 94-107 ms
to 79-81 ms.

Android's settings: when the host goes down while Android writes its
settings files, Android starts them afresh and is no longer
provisioned (Settings exits at once; this happened to the test
machine's Android after the hang above). airDroid now sets
`device_provisioned` again when a boot after the first finds it gone.

## Needs

* air/OS with haiku 353344194f (NVMM page-protection fix), i.e. images from
  2026-11-20 23:48 on. Older kernels panic as soon as Android boots.

## Not yet

* Vulkan on the GPU (Venus) by default: it is a choice in the settings
  window, experimental (see the plan below; verified on one GPU, a
  GTX 1070 on NVK); otherwise Android's Vulkan is lavapipe (CPU) under
  virgl, SwiftShader without a GPU.
* Camera and sensors (Waydroid has neither either).
* Toasts and system dialogs in multi-window mode: they belong to no app's
  task, so (as in Waydroid) no window shows them; crash and "not
  responding" dialogs are turned off there so they cannot hold the focus.
* ARM64 (Waydroid's arm64 images and QEMU under TCG would work but slowly;
  no NVMM there).
* AMD hosts: see "On real hardware" above.
* Not yet verified on hardware: a real microphone, and any Intel machine
  (the VMX/EPT path is verified only in nested VMs).

## GPU plan

1. Done: virgl (GL) through virglrenderer on EGL.
2. Venus (Vulkan), experimental: "The GPU, Vulkan too (experimental)"
   under Graphics in the settings window (`"gpu": "venus"` in
   settings.json); Vulkan apps use lavapipe on the CPU otherwise, and
   a build without the render server falls back to virgl. Android lists the host's GPU as its Vulkan device
   ("Virtio-GPU Venus (NVIDIA GeForce GTX 1070 (NVK GP104-A))" on the
   X399, "... (llvmpipe ...)" in the nested VM), with GL next to it as
   before, but Vulkan apps cannot show windows yet (layer 4). In layers:
   1. Done (Haiku master 14478e9792): NVMM maps memory QEMU already
      has into the guest, up to 1024 pieces per machine
      (`nvmm_hva_map_existing()`; `nvmm_hva_map()` always replaces the
      range with new anonymous memory, as NetBSD's does). Test:
      `libnvmm_hva_existing`.
   2. Done: QEMU (patch 0104) registers blob memory through
      `nvmm_hva_map_existing` and never lets NVMM replace memory it was
      handed, tells the guest NVMM's address width (37 bits, so the blob
      window lands where NVMM maps), and does not offer GL blob buffers
      (they map the host GL driver's own memory).
   3. Done: virglrenderer's Venus (vkr) and render server on air/OS:
      thread workers (no signalfd there), shared memory as locked areas
      passed between the processes (`virgl_shm.h`), host visible Vulkan
      memory as such areas imported with VK_EXT_external_memory_host
      (the drivers cannot export memory). The app starts the render
      server and QEMU takes its socket (`get_server_fd`): forking it
      from QEMU, which has a GPU device by then, lost QEMU's GL device
      and reset the X399. `tools/dev/vkprobe.c` reproduces that without
      airDroid (a process with a live NVK device and imported host
      memory forks a child that uses the GPU): a bug in air/OS or its
      NVIDIA driver glue, not found yet.
   4. Android's window buffers: work on the X399. With
      `debug.hwui.renderer=skiavk` (a line in props.override on the
      data disk; Android's UI stays on GL otherwise, and Vulkan apps
      use Venus either way with `gpu: "venus"`) Android's UI renders
      through
      Venus on NVK: Settings shows and scrolls (328 frames, 0.9 % janky,
      median 13 ms, 99th percentile 26 ms; the host reads a frame in
      0.7 ms, GL frames take 1.1-1.6 ms). How:
      * Mesa's Venus driver in the guest offers Android's window buffer
        support only if the renderer has dma-buf import, DRM format
        modifiers, a foreign queue family and sync fds. NVK on NVIDIA's
        RM lists them all and has neither dma-bufs nor sync files: vkr
        answers the sync fd queries itself and does what the three
        resource commands are for with the fences and semaphores.
      * A gralloc buffer is a GL (virgl) resource that cannot be
        exported: when Venus attaches one, virglrenderer gives it a
        shadow, a shared locked area with the resource's rows (at a
        pitch of whole 32 bytes: NVIDIA samples a linear image only at
        such a pitch), which vkr imports as host memory for the image.
        The GPU renders straight into it (`tools/dev/vkimage.c`: 5.3 ms
        a 1280x720 frame with submit and wait), and reads of the
        resource (the present path) come from it: no GPU read-back.
      * Also needed: host memory types shown to the guest as device
        local (Skia takes a hardware buffer only then), device creation
        retried at medium queue priority (NVK refuses Android's high),
        no sync fd export on fences and semaphores, and 8192 file
        descriptors for QEMU and the render server (NVK opens one per
        memory mapping; air/OS gives 256).
      Six sessions in a row on the X399 (boot, Settings, scroll, quit):
      each on the Vulkan pipeline with about 370 frames, nothing left
      behind on quit. Two hangs on the way there were ours: a wait for
      an exported semaphore that stalled the host queue, and the
      render server deleting a window buffer's area while the driver
      still had its pages pinned for a Vulkan import (delete_area()
      waits; the worker that waited also delivers the context's
      fences). Each import now maps the area by itself.
      GL users of a Vulkan-drawn buffer (SurfaceFlinger's own
      composition, Android's screenshots, the recents view): a texture
      whose resource has a shadow is written from the shadow before a
      GL draw, blit or copy reads it (vrend_sync_shadow; one upload per
      draw that samples it). Before, `screencap` showed a Vulkan app's
      window black; now it shows what the window shows.
      Which hosts: the guest's driver offers Android a swapchain only
      when the host's Vulkan driver lists external memory as dma-buf,
      image DRM format modifiers, foreign queue families and external
      fence and semaphore fds (it need not be able to export any of
      them: see above), and the buffers are host memory imports.
      `virgl_render_server --vk-check` says whether a device has all
      of that, and the app asks it before it turns Venus on; without,
      it does what "Automatic" does (virgl and Android's lavapipe on a GPU) (a line on the app's
      stderr says why). NVK on the GTX 1070 passes; lavapipe of Mesa
      22 (the test VM) does not: there Vulkan apps found no
      VK_KHR_swapchain and failed to start before the check existed.
      Open: one render server crash early on was not seen again and
      not explained.
      A third-party app: the Godot 4.3 editor (its Android APK has
      x86_64 code) started with `--rendering-method mobile
      --rendering-driver vulkan` reports "Vulkan 1.3.325 - Forward
      Mobile - Using Device #0: NVIDIA - Virtio-GPU Venus (NVIDIA
      GeForce GTX 1070 (NVK GP104-A))" and shows its project manager
      (a VkSwapchain through Android's loader, where Android's UI
      wraps buffers itself). With its default renderer it runs on the
      GL path ("OpenGL ES 3.2 Mesa - virgl (zink Vulkan 1.3 (NVIDIA
      GeForce GTX 1070 ...))").
      Host bug met on the way (air/OS, not airDroid): the X399 reset or
      hung several times while a render server was stuck like that; a
      stuck process should not take the machine down. See also the
      fork bug above. Once more on 2026-11-21 22:02 without any render
      server: the machine stopped a second after airDroid started a VM
      in the default mode (nothing in the syslog after the start) and
      was back by itself three minutes later. That was a kernel panic
      (the machine has `panic_reboot_timeout 120`: two minutes in the
      kernel debugger, then a restart), but its text did not reach the
      next start (no /var/log/previous_syslog). 62 further start and
      quit cycles, at four points of the guest's start and 14 of them
      right after a package reinstall as that one was, went through
      without it. Again on 2026-11-22 09:10, as a Vulkan-mode session
      that had run the scroll test was quit and the next one started;
      again no text (the loader does reserve the fixed log buffer on
      this machine, the restart does not keep it). Both times a session
      that had done real GPU work was ending and the next beginning. The default mode is unchanged by all this (X399:
      19.7 s to boot, 94 ms for the scroll test).
   Tried earlier on Linux/KVM with lavapipe (QEMU on Xvfb, SDL GL):
   Android enumerates the Venus device and the render server starts,
   but rendering into gralloc buffers ends in "aborting on ring fatal
   error" (layer 4: no GBM/dma-buf path on that host either).
3. Done on air/OS: frames do not go through guest RAM. QEMU
   (qemu/patches/0103) listens on `present.sock` (AIRDROID_PRESENT_SOCKET)
   and reads a virgl resource with one GPU read straight into an area of
   the app's: each window's two frame bitmaps live in cloneable areas
   that app_server and QEMU both map. A window whose frame is one
   unscaled GPU layer (over the background colour, or opaque) is read
   into its back bitmap and nothing composes it; other frames have QEMU
   fill the layers' guest backing and are composed as before. The guest
   (airdroid.present=host on its command line) only waits for Android's
   rendering. AIRDROID_PRESENT=guest goes back to the guest's read-back.
   What is left is the one read from GPU memory per frame: app_server
   draws from main memory, so showing a frame without it would need the
   window's pixels on the GPU (as Summit's and the Crafts' direct
   scanout path does for Vulkan).
