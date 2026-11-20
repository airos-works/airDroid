#!/usr/bin/env bash
# GPU pieces for airDroid's QEMU on air/OS: libepoxy and virglrenderer,
# cross-built and installed into the QEMU build's dependency sysroot
# (build/ARCH/qemu/qemu-deps), so tools/build-qemu.sh (GPU=1) can enable
# OpenGL and virglrenderer. Run after one plain tools/build-qemu.sh.
#   ARCH=x86_64 tools/build-gpu.sh
set -euo pipefail
. "$(dirname "$0")/common.sh"
. "${AIROS_SDK:-/data1/airos/sdk}/$ARCH/env.sh"
EPOXY_VERSION=1.5.10
VIRGL_VERSION=1.2.0
Q=$BUILD/qemu
deps=$Q/qemu-deps
sys=$deps/boot/system
bin=$Q/qemu-bin
[[ -x $bin/$TRIPLET-gcc ]] || { echo "run tools/build-qemu.sh first" >&2; exit 1; }
export PATH=$bin:$PATH
W=$BUILD/gpu
mkdir -p "$W"
cross=$W/haiku-$ARCH.cross
cat > "$cross" <<CROSSFILE
[binaries]
c = '$bin/$TRIPLET-gcc'
cpp = '$bin/$TRIPLET-g++'
ar = '$bin/$TRIPLET-ar'
strip = '$bin/$TRIPLET-strip'
pkg-config = '$bin/$TRIPLET-pkg-config'

[host_machine]
system = 'haiku'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'

[built-in options]
c_args = ['-D_BSD_SOURCE', '-I$SYSROOT/boot/system/develop/headers/os/opengl']
c_link_args = ['-lbsd', '-lnetwork']
CROSSFILE
meson_args=(--cross-file "$cross" --prefix=/boot/system --libdir=lib --includedir=develop/headers --buildtype=release)

# libepoxy: GL/EGL function loading (virglrenderer and QEMU's OpenGL)
e=$W/libepoxy-$EPOXY_VERSION
if [[ ! -f $sys/lib/pkgconfig/epoxy.pc && ! -f $sys/develop/lib/pkgconfig/epoxy.pc ]]; then
	rm -rf "$e"
	tar -C "$W" -xf "$(fetch "https://github.com/anholt/libepoxy/archive/refs/tags/$EPOXY_VERSION.tar.gz" "libepoxy-$EPOXY_VERSION.tar.gz")"
	meson setup "$e/build" "$e" "${meson_args[@]}" -Degl=yes -Dglx=no -Dx11=false -Dtests=false -Ddocs=false > "$W/epoxy-setup.log"
	ninja -C "$e/build" > "$W/epoxy-build.log"
	DESTDIR=$deps meson install -C "$e/build" --no-rebuild > /dev/null
fi

# virglrenderer: the host side of virtio-gpu 3D (OpenGL through EGL)
v=$W/virglrenderer-$VIRGL_VERSION
if [[ ! -f $v/.patched ]]; then
	rm -rf "$v"
	tar -C "$W" -xf "$(fetch "https://gitlab.freedesktop.org/virgl/virglrenderer/-/archive/$VIRGL_VERSION/virglrenderer-$VIRGL_VERSION.tar.gz" "virglrenderer-$VIRGL_VERSION.tar.gz")"
	for p in "$ROOT"/gpu/patches/virglrenderer-$VIRGL_VERSION-*.patch; do
		[[ -f $p ]] && patch -d "$v" -p1 -s < "$p"
	done
	touch "$v/.patched"
fi
[[ -f $v/build/build.ninja ]] || meson setup "$v/build" "$v" "${meson_args[@]}" -Dplatforms=egl -Dminigbm_allocation=false \
	-Dvenus=false -Dtests=false -Dfuzzer=false -Dvalgrind=false -Dtracing=none -Dcheck-gl-errors=false > "$W/virgl-setup.log"
ninja -C "$v/build" > "$W/virgl-build.log"
DESTDIR=$deps meson install -C "$v/build" --no-rebuild > /dev/null
ls -la "$sys"/lib/libvirglrenderer.so* "$sys"/lib/libepoxy.so*
