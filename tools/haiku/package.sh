#!/usr/bin/env bash
# Builds airDroid's hpkg on the Linux build host with the air/OS SDK's host
# tools: the app, the VM (kernel, initramfs, airDroid's QEMU and firmware),
# resources, Deskbar link and post-install script.
#   tools/haiku/package.sh            -> artifacts/airdroid-VERSION-x86_64.hpkg
# Needs tools/build-kernel.sh, tools/build-guest.sh and tools/build-qemu.sh
# done first (build/x86_64/...).
# For airos-ci (build_airdroid): AIRDROID_BUILD (the build tree),
# AIRDROID_STAGE (where to stage) and AIRDROID_STAGE_ONLY=1 (no hpkg: CI
# adds attributes and creates the package itself); the resources are left
# in $AIRDROID_BUILD/airDroid.rsrc.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
. "$ROOT/tools/haiku/env.sh"
RES=$ROOT/apps/airdroid-haiku/resources
B=${AIRDROID_BUILD:-$ROOT/build/$ARCH}
STAGE=${AIRDROID_STAGE:-$ROOT/build/package-stage}
T=${CARGO_TARGET_DIR:-$ROOT/target}
(cd "$ROOT" && cargo build --release --target "$RUST_TARGET" -Zbuild-std=std,panic_abort -p airdroid-haiku -p airdroid-cli)
rm -rf "$STAGE"
mkdir -p "$STAGE"/{apps,bin,data/airDroid/firmware,lib/airDroid,boot/post-install,data/deskbar/menu/Applications,documentation/packages/airdroid}
# strip before xres: GNU strip drops resources
"${CROSS}strip" -o "$STAGE/apps/airDroid" "$T/$RUST_TARGET/release/airDroid"
(cd "$RES" && "$TOOLS/rc/rc" -o "$B/airDroid.rsrc" airDroid.rdef)
"$TOOLS/xres" -o "$STAGE/apps/airDroid" "$B/airDroid.rsrc"
# BEOS:TYPE, BEOS:APP_SIG etc. as attributes, or Tracker's first open is lost
[[ ${AIRDROID_STAGE_ONLY:-0} == 1 ]] || "$TOOLS/resattr/resattr" -O -o "$STAGE/apps/airDroid" "$B/airDroid.rsrc"
"${CROSS}strip" -o "$STAGE/bin/airdroid" "$T/$RUST_TARGET/release/airdroid"
cp "$B/vmlinuz" "$B/initramfs.img" "$STAGE/data/airDroid/"
cp "$B"/qemu/firmware/* "$STAGE/data/airDroid/firmware/"
cp "$B/qemu/qemu-system-x86_64" "$STAGE/lib/airDroid/"
# QEMU's GL libraries (GPU=1 build): libepoxy, virglrenderer, pixman
if [[ -d $B/qemu/lib ]]; then
	mkdir -p "$STAGE/lib/airDroid/lib"
	cp "$B"/qemu/lib/* "$STAGE/lib/airDroid/lib/"
fi
install -m 755 "$RES/airdroid-post-install.sh" "$STAGE/boot/post-install/airdroid.sh"
ln -s ../../../../apps/airDroid "$STAGE/data/deskbar/menu/Applications/airDroid"
cp "$ROOT/README.md" "$ROOT/docs/ARCHITECTURE.md" "$STAGE/documentation/packages/airdroid/"
cp "$RES/airDroid.PackageInfo" "$STAGE/.PackageInfo"
[[ ${AIRDROID_STAGE_ONLY:-0} != 1 ]] || exit 0
version=$(awk '$1 == "version" { print $2 }' "$STAGE/.PackageInfo")
mkdir -p "$ROOT/artifacts"
out=$ROOT/artifacts/airdroid-$version-$ARCH.hpkg
rm -f "$out"
"$TOOLS/package/package" create -C "$STAGE" "$out"
ls -la "$out"
