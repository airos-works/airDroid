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
# GPU=1 (default): OpenGL and virglrenderer (tools/build-gpu.sh installs
# libepoxy and virglrenderer into the dependency sysroot first)
GPU=${GPU:-1}
if [[ $GPU == 1 ]]; then
	sed -i 's/--enable-slirp /--enable-slirp --enable-opengl --enable-virglrenderer --enable-pixman /' "$copy/tools/build-qemu.sh"
	# egl-headless and virtio-gpu draw with pixman: HaikuPorts' (x86_64)
	if [[ $ARCH == x86_64 ]]; then
		sys=$BUILD/qemu/qemu-deps/boot/system
		if [[ -d $sys && ! -f $sys/develop/lib/pkgconfig/pixman-1.pc && ! -f $sys/lib/pkgconfig/pixman-1.pc ]]; then
			AIROS_CACHE=${AIROS_CACHE:-/data2/airos/cache} AIROS_SDK=${AIROS_SDK:-/data1/airos/sdk} \
				python3 "${AIROS_CI:-/mnt/HaikuWork/src/airos-ci}/deps/haikuports.py" stage "$ARCH" "$sys" pixman_devel >/dev/null
			while read -r p; do
				mkdir -p "$BUILD/qemu/qemu-deps$(dirname "$p")"
				ln -sfn ../../boot/system "$BUILD/qemu/qemu-deps$p"
			done < <(grep -ho "/packages/[^/]*/.self" "$sys"/develop/lib/pkgconfig/*.pc | sort -u)
		fi
	fi
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
