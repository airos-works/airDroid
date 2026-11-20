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
# GPU=1 (default): OpenGL and virglrenderer. Their libraries (pixman,
# libepoxy, virglrenderer: tools/build-gpu.sh) go into QEMU's dependency
# sysroot after linuxvm's script stages GLib and slirp, before QEMU's
# configure.
GPU=${GPU:-1}
if [[ $GPU == 1 ]]; then
	sed -i 's/--enable-slirp /--enable-slirp --enable-opengl --enable-virglrenderer --enable-pixman /' "$copy/tools/build-qemu.sh"
	grep -q '^src=$BUILD/qemu-$QEMU_VERSION$' "$copy/tools/build-qemu.sh" \
		|| { echo "linuxvm's build-qemu.sh changed: no place for the GPU step" >&2; exit 1; }
	sed -i 's|^src=$BUILD/qemu-$QEMU_VERSION$|[[ -z ${AIRDROID_AFTER_DEPS:-} ]] \|\| bash "$AIRDROID_AFTER_DEPS" \|\| exit 1\n&|' "$copy/tools/build-qemu.sh"
	export AIRDROID_AFTER_DEPS=$ROOT/tools/build-gpu.sh AIRDROID_BUILD=$BUILD
fi
out=$BUILD/qemu
# QEMU's tree is rebuilt when the patch set changes
stamp=$( (cat "$copy"/qemu/patches/*.patch "$copy/tools/build-qemu.sh"; echo "gpu=$GPU") | sha256sum | cut -c1-16)
mkdir -p "$out"
if [[ ! -f $out/.patchset-$stamp ]]; then
	rm -rf "$out"/qemu-[0-9]*/ "$out"/.patchset-*
fi
ARCH=$ARCH LINUXVM_BUILD=$out LINUXVM_CACHE=${LINUXVM_CACHE:-/data2/airos/cache/linuxvm} "$copy/tools/build-qemu.sh"
touch "$out/.patchset-$stamp"
if [[ $GPU == 1 ]]; then
	# QEMU's GL libraries ship next to it (lib/airDroid/lib)
	. "${AIROS_SDK:-/data1/airos/sdk}/$ARCH/env.sh"
	rm -rf "$out/lib"
	mkdir -p "$out/lib"
	for lib in libepoxy.so.0 libvirglrenderer.so.1 libpixman-1.so.0; do
		"${CROSS}strip" -o "$out/lib/$lib" "$(readlink -f "$out/qemu-deps/boot/system/lib/$lib")"
	done
	patchelf --force-rpath --set-rpath '$ORIGIN/lib' "$out/qemu-system-$( [[ $ARCH == arm64 ]] && echo aarch64 || echo x86_64)"
fi
