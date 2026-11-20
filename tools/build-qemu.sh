#!/usr/bin/env bash
# airDroid's QEMU for air/OS: linuxvm's build (NVMM on Haiku, slirp, 9p;
# apps/linuxvm, airos-works/linuxvm) with airDroid's patches in qemu/patches
# added: the guest's RAM areas are cloneable and their guest-physical
# layout is published, so the airDroid app can read frame buffers in place.
#   ARCH=x86_64 LINUXVM=../linuxvm tools/build-qemu.sh
#     -> build/ARCH/qemu/qemu-system-x86_64, build/ARCH/qemu/firmware/
set -euo pipefail
. "$(dirname "$0")/common.sh"
LINUXVM=${LINUXVM:-$(cd "$ROOT/../linuxvm" 2>/dev/null && pwd)}
[[ -f $LINUXVM/tools/build-qemu.sh ]] || { echo "linuxvm checkout not found (set LINUXVM)" >&2; exit 1; }
copy=$BUILD/linuxvm-src
rm -rf "$copy"
mkdir -p "$copy"
cp -a "$LINUXVM/tools" "$LINUXVM/qemu" "$copy/"
cp "$ROOT"/qemu/patches/*.patch "$copy/qemu/patches/"
# QEMU's tree is rebuilt when the patch set changes
stamp=$(cat "$copy"/qemu/patches/*.patch | sha256sum | cut -c1-16)
out=$BUILD/qemu
mkdir -p "$out"
if [[ ! -f $out/.patchset-$stamp ]]; then
	rm -rf "$out/qemu-"*/ "$out"/.patchset-*
fi
ARCH=$ARCH LINUXVM_BUILD=$out LINUXVM_CACHE=${LINUXVM_CACHE:-/data2/airos/cache/linuxvm} "$copy/tools/build-qemu.sh"
touch "$out/.patchset-$stamp"
