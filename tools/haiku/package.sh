#!/usr/bin/env bash
# Builds airDroid's hpkg on the Linux build host with the air/OS SDK's host
# tools: the app, the VM (kernel, initramfs, airDroid's QEMU and firmware),
# resources, Deskbar link and post-install script.
#   tools/haiku/package.sh            -> artifacts/airdroid-VERSION-x86_64.hpkg
# Needs tools/build-kernel.sh, tools/build-guest.sh and tools/build-qemu.sh
# done first (build/x86_64/...).
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
. "$ROOT/tools/haiku/env.sh"
RES=$ROOT/apps/airdroid-haiku/resources
B=$ROOT/build/$ARCH
STAGE=$ROOT/build/package-stage
(cd "$ROOT" && cargo build --release --target "$RUST_TARGET" -Zbuild-std=std,panic_abort -p airdroid-haiku -p airdroid-cli)
rm -rf "$STAGE"
mkdir -p "$STAGE"/{apps,bin,data/airDroid/firmware,lib/airDroid,boot/post-install,data/deskbar/menu/Applications,documentation/packages/airdroid}
# strip before xres: GNU strip drops resources
"${CROSS}strip" -o "$STAGE/apps/airDroid" "$ROOT/target/$RUST_TARGET/release/airDroid"
(cd "$RES" && "$TOOLS/rc/rc" -o "$STAGE/airDroid.rsrc" airDroid.rdef)
"$TOOLS/xres" -o "$STAGE/apps/airDroid" "$STAGE/airDroid.rsrc"
# BEOS:TYPE, BEOS:APP_SIG etc. as attributes, or Tracker's first open is lost
"$TOOLS/resattr/resattr" -O -o "$STAGE/apps/airDroid" "$STAGE/airDroid.rsrc"
rm "$STAGE/airDroid.rsrc"
"${CROSS}strip" -o "$STAGE/bin/airdroid" "$ROOT/target/$RUST_TARGET/release/airdroid"
cp "$B/vmlinuz" "$B/initramfs.img" "$STAGE/data/airDroid/"
cp "$B"/qemu/firmware/* "$STAGE/data/airDroid/firmware/"
cp "$B/qemu/qemu-system-x86_64" "$STAGE/lib/airDroid/"
install -m 755 "$RES/airdroid-post-install.sh" "$STAGE/boot/post-install/airdroid.sh"
ln -s ../../../../apps/airDroid "$STAGE/data/deskbar/menu/Applications/airDroid"
cp "$ROOT/README.md" "$ROOT/docs/ARCHITECTURE.md" "$STAGE/documentation/packages/airdroid/"
cp "$RES/airDroid.PackageInfo" "$STAGE/.PackageInfo"
version=$(awk '$1 == "version" { print $2 }' "$STAGE/.PackageInfo")
mkdir -p "$ROOT/artifacts"
out=$ROOT/artifacts/airdroid-$version-$ARCH.hpkg
rm -f "$out"
"$TOOLS/package/package" create -C "$STAGE" "$out"
ls -la "$out"
