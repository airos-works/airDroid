# airDroid architecture

```
 air/OS                                           Linux VM (QEMU + NVMM)
┌──────────────────────────────┐                 ┌─────────────────────────────────────┐
│ airDroid.app (Rust + Be API) │ virtio-serial   │ airdroid-guest (PID 1, Rust, musl)  │
│  • session, app list, menus  │◄──────────────►│  • binderfs, disks, waydroid.prop   │
│  • BWindow per Android task  │  (control,     │  • Wayland compositor  ◄── hwcomposer│
│  • composes layers from ─────┼── frames by id,│  • exec/setns, pm/am app actions    │
│    guest RAM (mmap)          │   input)       │  ┌───────────────────────────────┐  │
│                              │                 │  │ Android (LineageOS 20, Waydroid│  │
│  /var/shared_memory/…ram ◄───┼── guest RAM ───►│  │ images) in mnt/pid/uts/ipc ns │  │
└──────────────────────────────┘   (ramfs file)  │  └───────────────────────────────┘  │
                                                 └─────────────────────────────────────┘
```

## What Waydroid does, and where airDroid does it

| Waydroid on Linux | airDroid |
|---|---|
| LXC container on the host kernel | the same container, in a dedicated VM (Haiku has no Linux kernel) |
| `waydroid container` service (Python, root) | `airdroid-guest`, PID 1 of the VM |
| binder/ashmem modules, binderfs | guest kernel with binderfs; `sys.use_memfd=true` |
| `lxc.mount.entry` node list, tmpfs dev/run/tmp/var | `container::prepare_rootfs`, `populate_dev` |
| `waydroid.prop` bound over `/vendor/waydroid.prop` | generated from the kernel command line + `props.override` |
| system/vendor overlays | overlayfs with the upper layer on the data disk |
| `~/.local/share/waydroid/data` bound at `/data` | ext4 on a qcow2 disk (BFS has no sparse files) |
| host's Wayland compositor | a minimal compositor in the VM that forwards layers |
| host's PulseAudio socket | (planned) a pulse-protocol server in the VM → Media Kit |
| `waydroid shell`, `lxc-attach` | `ToGuest::Exec` with `setns` |
| `waydroid app install/launch`, IPlatform over binder | `pm`/`am` in the container (IPlatform planned) |
| `.desktop` files + `/data/icons` | Deskbar entries + the same icons |

## The display path

Waydroid's `hwcomposer.waydroid` is a Wayland client. In multi-window mode it
groups SurfaceFlinger's layers by task and makes one `xdg_toplevel` per task,
with a subsurface per layer. Without a usable GPU (`ro.hardware.gralloc=default`)
it copies each gralloc buffer into its own memfd and hands that to `wl_shm`.

airDroid's compositor runs in the VM and composites nothing. For each shm
buffer it `mlock`s the pages, reads their physical addresses from
`/proc/self/pagemap` and sends them to the host once (`BufferNew`). A commit
becomes a `Frame`: the window's layers (buffer id, source crop, destination),
bottom first. QEMU backs guest RAM with a shared file, which on air/OS lives
in `/var/shared_memory` (a ramfs), and the host maps it too. The host
therefore reads the pixels where Android left them, converts and blends them
into the window's bitmap, and answers `FrameDone`. That releases replaced
buffers and fires frame callbacks and presentation feedback, so Android
renders at the host's pace with one frame in flight per window.

Pages must not move while the host may read them: the guest kernel has no
KSM or THP, and the daemon sets `vm.compact_unevictable_allowed=0`.

## Input

Host events (pointer, touch, keys as Linux key codes, scroll) go to the
compositor, which hit-tests the window's surface tree and delivers
`wl_pointer` / `wl_touch` / `wl_keyboard` events to the surface underneath,
in surface coordinates. The HWC converts them into evdev events on its
`/dev/input` FIFOs. The daemon holds those FIFOs open for writing, because
Android's init opens new input nodes and would otherwise block forever on a
FIFO without a writer.

## Boot

The VM boots straight into the kernel and initramfs (no bootloader). The
daemon mounts binderfs, the images and the data disk (formatting it on the
first start), writes `waydroid.prop`, starts the compositor, and then
`clone`s Android's `/init` into new mount/PID/UTS/IPC namespaces with
`container=lxc`, the variable Waydroid's patched init looks for. On a
development host with KVM, Android reaches `sys.boot_completed` in about
27 s on the first start.
