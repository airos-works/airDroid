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

## Needs

* air/OS with haiku 353344194f (NVMM page-protection fix), i.e. images from
  2026-11-20 23:48 on. Older kernels panic as soon as Android boots.

## Not yet

* Vulkan on the GPU (Venus): see the plan below; Android's Vulkan is
  lavapipe (CPU) under virgl, SwiftShader otherwise.
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
2. Venus (Vulkan), experimental: `"gpu": "venus"` in settings.json (not
   in the settings window; Vulkan apps use lavapipe on the CPU
   otherwise). Android lists the host's GPU as its Vulkan device
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
   4. Android's window buffers: works on the X399 on branch
      `venus-wsi`, not stable yet. With `debug.hwui.renderer=skiavk`
      (props.override on the data disk) Android's UI renders through
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
      Open: one run crashed the render server and one hung right after
      boot (not reproduced since; debug_server now writes reports for
      it); GL consumers of a Vulkan-rendered buffer (SurfaceFlinger's
      own composition, screenshots) see the GL texture, not the shadow;
      a shadow is filled from GL once, when Venus attaches the buffer.
      Host bugs met on the way (air/OS, not airDroid): a render server
      that dies with host memory imported cannot be reaped (its team
      stays, the pages pinned), and the machine has reset or hung with
      such teams around; see also the fork bug above.
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
