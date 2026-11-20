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

## Not yet

* GPU acceleration: Android renders with SwiftShader (CPU). Plan below.
* Audio input (microphone), camera, sensors, notifications to the desktop.
* ARM64 (Waydroid's arm64 images and QEMU under TCG would work but slowly;
  no NVMM there).
* Window resizing in multi-window mode (Android's caption moves windows;
  resizing them from air/OS is not wired).
* Performance on real hardware is unmeasured (the X399 went down with the
  NVMM kernel bug this work found; see haiku 353344194f).

## GPU plan

1. Venus: Android's `vulkan.virtio` (in Waydroid's vendor image) with a
   virtio-gpu device whose host side is virglrenderer's Venus renderer on
   air/OS's RADV/NVK. Needs virglrenderer ported to air/OS (Vulkan only,
   no EGL/GBM) and blob resources mapped into guest memory.
2. GLES over Venus with Mesa's Zink in the guest, or ANGLE (Android's
   `ro.hardware.egl=angle` on Vulkan).
3. Display: gralloc buffers become virtio-gpu blobs; the host imports them
   into Vulkan and presents with the Craft apps' readback/scanout path,
   instead of the CPU copy.
