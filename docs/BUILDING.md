# Building and testing airDroid

Everything builds on the air/OS build host (Linux) with the workspace's
toolchains; nothing builds on air/OS itself.

## Pieces

| Step | Script | Output |
|---|---|---|
| Guest kernel (Linux 6.18.52 + `kernel/airdroid.config` + `kernel/patches`) | `tools/build-kernel.sh` | `build/x86_64/vmlinuz` |
| Guest userland (`airdroid-guest` for musl as `/init`, Alpine busybox and e2fsprogs) | `tools/build-guest.sh` | `build/x86_64/initramfs.img` |
| QEMU for air/OS (linuxvm's build + `qemu/patches`; with `GPU=1`, the default, OpenGL and virglrenderer from `tools/build-gpu.sh`) | `LINUXVM=../linuxvm tools/build-qemu.sh` | `build/x86_64/qemu/` |
| The app and the `airdroid` command, packaged | `tools/haiku/package.sh` | `artifacts/airdroid-VERSION-x86_64.hpkg` |

`tools/build-all.sh` runs them all in order (about 5 minutes from an empty
tree on the build host).

CI: airos-ci's `build_airdroid` (`apps/build-app-packages.sh`) runs
`tools/build-all.sh` with `AIRDROID_BUILD`, `AIRDROID_STAGE` and
`AIRDROID_STAGE_ONLY=1`, then adds the attributes and makes the package;
`.github/workflows/airos.yml` runs it on every push to `main` and
publishes the package to the `latest` release and the package pool. The
package revision is the commit count.

`tools/haiku/env.sh` uses airos-ci's SDK (`/data1/airos/sdk`) and pinned Rust
nightly (`/data1/airos/toolchain/rust`) with a separate cargo home (the CI's
belongs to the runner account).

## Linux development loop (KVM)

`airdroid-dev` runs the VM headless on the build host with the same guest
and core code as the air/OS app; it is the fastest way to work on the guest:

```sh
tools/build-guest.sh
MULTIWIN=1 tools/dev-run.sh          # boots in ~30 s; log: build/state/dev.log
echo "launch com.android.settings" > build/state/cmd
echo "shot 2 /tmp/settings.png"     > build/state/cmd
echo "shsh cmd notification post -t hi tag hello" > build/state/cmd   # as Android's shell user
echo "install /path/to/app.apk"     > build/state/cmd
```

`AIRDROID_DEV_CROP=1` crops windows the way the app does in multi-window
mode; `AIRDROID_DEV_LAYERS=1` logs every frame's layers. Audio goes to
`build/state/audio-N.raw` (s16le, paced like a sound card). The images come
from `IMAGES` (default `/mnt/HaikuWork/build/airdroid-images`: Waydroid's
`system.img` and `vendor.img`).

## air/OS test VM

`/data3/airos/airdroid-work/vm` runs an air/OS image under KVM with nested
VMX, so NVMM works inside it:

* `IMG=../airos-C.img vm/vm-up.sh` boots it ("Try air/OS"), starts sshd and
  mounts the 60 GB data disk at `/Data` (SSH `vm/ssh.sh`, port 2242; VNC
  5922; `vm/qmp.py shot|click|type`). Sound goes to `vm/audio.wav`.
* `airos-C.img` carries the kernel with the NVMM fix (haiku 353344194f);
  older kernels panic in `vm_soft_fault` as soon as Android boots.
* `/Data/redeploy.sh` quits airDroid, reinstalls `/Data/airdroid-*.hpkg`
  and starts it with `AIRDROID_DEBUG=1` (log `/Data/airdroid.log`).
* Android boots in about 4 minutes there (nested virtualization); use the
  `airdroid` command (`airdroid status`, `airdroid shell ...`) over SSH.

## Debugging

* `AIRDROID_DEBUG=1 airDroid` logs frames, buffers and the guest's log.
* The guest's console (kernel, Android's init via `/dev/kmsg`, the daemon)
  is `~/config/cache/airDroid/console.log`; QEMU's is `qemu.log`.
* `airdroid logcat`, `airdroid shell dmesg`, `airdroid shell --vm CMD` (the
  VM's own busybox userland). `airdroid shell` runs its command as
  Android's shell user, as `adb shell` does (`am`, `pm`, `input`, `wm`,
  `dumpsys`, `settings`); `airdroid shell -u 0 CMD` runs it as root, for
  files under `/data`. As root, `am` and `pm uninstall` do not work: the
  Java services of this Android take a root caller from outside for the
  system user.
