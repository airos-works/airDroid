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

## Needs

* air/OS with haiku 353344194f (NVMM page-protection fix), i.e. images from
  2026-11-20 23:48 on. Older kernels panic as soon as Android boots.

## Not yet

* Vulkan on the GPU (Venus): see the plan below; Android's Vulkan is
  lavapipe (CPU) under virgl, SwiftShader otherwise.
* Audio input (microphone), camera, sensors, notifications to the desktop.
* ARM64 (Waydroid's arm64 images and QEMU under TCG would work but slowly;
  no NVMM there).
* Window resizing in multi-window mode (Android's caption moves windows;
  resizing them from air/OS is not wired).
* Performance on real hardware is unmeasured (the X399 went down with the
  NVMM kernel bug this work found; see haiku 353344194f).

## GPU plan

1. Done: virgl (GL) through virglrenderer on EGL, frames read back.
2. Venus (Vulkan): Waydroid's vendor image has `vulkan.virtio`; the host
   side is virglrenderer's Venus on RADV/NVK. Blocker: Venus maps host
   GPU memory into the guest (virtio-gpu blob/hostmem), and NVMM on
   air/OS replaces a mapped region with its own memory object instead of
   mapping the existing pages. Needs the NVMM driver to map an existing
   area's cache into the guest.
3. Zero-copy display: present virtio-gpu resources straight from the
   host's GL/Vulkan instead of reading them back (needs the resources'
   textures shared between QEMU and the airDroid app, or QEMU presenting).
