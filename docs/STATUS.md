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
* Clipboard both ways (implemented; not yet verified on air/OS).
* `airdroid` command: status, shell, logcat, app …, prop, stop.

* GPU rendering, phase 1 (virgl): Android's GL goes through Mesa's virgl
  driver to virglrenderer in airDroid's QEMU, on the host's GPU through
  EGL (AMD: radeonsi; NVIDIA: Zink on NVK; no GPU: llvmpipe). Frames are
  read back into guest pages and shown as before. `gpu` setting:
  auto (virgl when there is a GPU), virgl, none (SwiftShader).
  Verified in the test VM with llvmpipe; not yet on GPU hardware.

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
* **Blocker:** the first session left Android's data disk (ext4 in the
  qcow2) corrupt (an inode table and a directory block lost; qcow2 itself
  consistent), and the next test boot hung the whole computer (no ping, no
  syslog). Every earlier test ran NVMM's VMX/EPT path (Intel hosts); the
  suspect is NVMM's SVM/RVI path (nested page invalidation). Until it is
  fixed airDroid refuses to start on AMD processors (settings
  `allow_amd: true` overrides it, for testing), and it is not in the
  images.

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

1. Done: virgl (GL) through virglrenderer on EGL, frames read back.
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
3. Zero-copy display: present virtio-gpu resources straight from the
   host's GL/Vulkan instead of reading them back (needs the resources'
   textures shared between QEMU and the airDroid app, or QEMU presenting).
