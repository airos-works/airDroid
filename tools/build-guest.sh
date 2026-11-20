#!/usr/bin/env bash
# The VM's userland: the guest daemon (static musl) as /init, with Alpine's
# busybox (debug shell) and e2fsprogs (formats the data disk) and the files
# under guest/overlay (layered over Android's system/vendor).
#   ARCH=x86_64 tools/build-guest.sh   -> build/ARCH/initramfs.img
set -euo pipefail
. "$(dirname "$0")/common.sh"
ALPINE_VERSION=3.22
ALPINE_ARCH=$([[ $ARCH == arm64 ]] && echo aarch64 || echo x86_64)
rustup target list --installed | grep -qx "$RUST_GUEST_TARGET" || rustup target add "$RUST_GUEST_TARGET"
(cd "$ROOT" && cargo build --release -p airdroid-guest --target "$RUST_GUEST_TARGET")
tree=$BUILD/guest-tree
rm -rf "$tree"
mkdir -p "$tree/usr/share/airdroid"
[[ -d $ROOT/guest/overlay ]] && cp -a "$ROOT/guest/overlay" "$tree/usr/share/airdroid/overlay"
python3 "$ROOT/tools/mkrootfs.py" --arch "$ALPINE_ARCH" --alpine "$ALPINE_VERSION" \
	--agent "${CARGO_TARGET_DIR:-$ROOT/target}/$RUST_GUEST_TARGET/release/airdroid-guest" --cache "$CACHE/alpine" \
	--tree "$tree" --out "$BUILD/initramfs.img" \
	alpine-baselayout busybox busybox-binsh musl e2fsprogs
