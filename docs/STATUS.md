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
2. Venus (Vulkan), experimental (`gpu: "venus"`, not offered in the
   settings window): QEMU gets blob resources, a host memory window and
   venus=on, Android `ro.hardware.vulkan=virtio` (Waydroid's vendor image
   has Mesa's Venus driver). Tried on Linux/KVM with lavapipe (QEMU on
   Xvfb, SDL GL): Android enumerates the Venus device and the host render
   server starts, but rendering into gralloc buffers ends in "aborting on
   ring fatal error": virglrenderer's Venus shares memory with GL through
   GBM and dma-bufs, which a host without a render node lacks. On air/OS
   it needs (a) that external memory path ported to air/OS's GPU stack
   (virglrenderer there is built without GBM) and (b) NVMM mapping an
   existing area into the guest (blob/hostmem): NVMM's hva_map always
   replaces the range with new anonymous memory (as NetBSD's does). Both
   need GPU hardware to develop.
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
