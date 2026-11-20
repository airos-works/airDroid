# airDroid

Android apps on air/OS. airDroid does what [Waydroid](https://github.com/waydroid/waydroid)
does on Linux (a full LineageOS Android, apps installed from `.apk` files,
each app in its own desktop window or the whole Android UI in one), built for
air/OS:

* **Rust throughout**: the air/OS app, the VM's init and its compositor.
* **Be API, not Wayland**: Android's windows become BWindows; input comes
  from BViews.
* **No copies over a wire**: guest RAM is a shared file, so the app reads
  Android's frame buffers in place.
* **GPU acceleration**: Android's OpenGL ES runs on the host GPU through
  virgl (virglrenderer on air/OS's Mesa); Vulkan (Venus) is next.

What works, as in Waydroid: installing apps from `.apk` files (open one,
or `airdroid app install`), each app in its own resizable window or the
whole Android UI in one, Deskbar entries for Android apps, keyboard, mouse
and touch, the clipboard both ways, sound and the microphone, Android's
notifications on the desktop, networking, a shared folder (`~/Android` is
`/sdcard/airOS`), and Waydroid's command line as `airdroid` (`shell`,
`logcat`, `app install|remove|launch|list|intent`, `prop`, `show-full-ui`,
`upgrade`, `status`, `stop`).

Haiku has no Linux kernel to share, so where Waydroid starts an LXC container,
airDroid starts a small Linux VM (QEMU with NVMM, the same stack as
air/OS's Docker `linuxvm`) whose only job is that container. It uses
Waydroid's own system and vendor images unchanged.

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how it fits together,
[docs/BUILDING.md](docs/BUILDING.md) to build and test, and
[docs/STATUS.md](docs/STATUS.md) for where it stands.

<!-- airos-ci:latest-builds:start -->
## Latest builds

Built automatically by air/OS CI from commit `5d9e51b` on 2026-11-20 ([all files](https://github.com/airos-works/airDroid/releases/tag/latest)).

| Architecture | Package |
|---|---|
| x86_64 | [airdroid-0.1.0-35-x86_64.hpkg](https://github.com/airos-works/airDroid/releases/download/latest/airdroid-0.1.0-35-x86_64.hpkg) |

Install with `pkgman install <file>`, or copy the file into `/boot/system/packages`. Haiku SDK: x86_64 hrev60207-2798-gd5494a639a, arm64 hrev60207-2798-gd5494a639a.
<!-- airos-ci:latest-builds:end -->

## Layout

| Path | What |
|---|---|
| `crates/proto` | host ↔ guest wire protocol |
| `crates/guest` | `airdroid-guest`: the VM's PID 1 (container, compositor, app actions) |
| `crates/core` | host side: QEMU, qcow2 disks, guest memory, session, layer composition |
| `crates/devtool` | `airdroid-dev`: headless runner for Linux/KVM development |
| `apps/airdroid-haiku` | the air/OS app (Be API) |
| `kernel/` | guest kernel config fragment |
| `tools/` | build scripts (kernel, guest initramfs, Haiku cross build, packaging) |

License: GPL-3.0-or-later (Waydroid is GPL-3.0).
