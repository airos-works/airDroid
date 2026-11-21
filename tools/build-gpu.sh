#!/usr/bin/env bash
# GPU pieces for airDroid's QEMU on air/OS: pixman (HaikuPorts), libepoxy
# and virglrenderer, cross-built and installed into the QEMU build's
# dependency sysroot (build/ARCH/qemu/qemu-deps), so QEMU can enable OpenGL
# and virglrenderer. tools/build-qemu.sh runs it between linuxvm's
# dependency staging and QEMU's configure.
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

# pixman: egl-headless and virtio-gpu draw with it (HaikuPorts' on x86_64)
if [[ $ARCH == x86_64 && ! -f $sys/develop/lib/pkgconfig/pixman-1.pc && ! -f $sys/lib/pkgconfig/pixman-1.pc ]]; then
	AIROS_CACHE=${AIROS_CACHE:-/data2/airos/cache} AIROS_SDK=${AIROS_SDK:-/data1/airos/sdk} \
		python3 "${AIROS_CI:-/mnt/HaikuWork/src/airos-ci}/deps/haikuports.py" stage "$ARCH" "$sys" pixman_devel >/dev/null
	while read -r p; do
		mkdir -p "$deps$(dirname "$p")"
		ln -sfn ../../boot/system "$deps$p"
	done < <(grep -ho "/packages/[^/]*/.self" "$sys"/develop/lib/pkgconfig/*.pc | sort -u)
fi
# Venus (Vulkan): the loader and headers virglrenderer's vkr builds against
if [[ $ARCH == x86_64 && ! -f $sys/develop/lib/pkgconfig/vulkan.pc ]]; then
	AIROS_CACHE=${AIROS_CACHE:-/data2/airos/cache} AIROS_SDK=${AIROS_SDK:-/data1/airos/sdk} \
		python3 "${AIROS_CI:-/mnt/HaikuWork/src/airos-ci}/deps/haikuports.py" stage --no-deps "$ARCH" "$sys" \
		vulkan_loader vulkan_loader_devel vulkan_headers >/dev/null
fi
VENUS=false
[[ ! -f $sys/develop/lib/pkgconfig/vulkan.pc ]] || VENUS=true
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
# A tree from other patches or options is not this build's: a kept one
# once shipped a virglrenderer without Venus next to a QEMU that had it.
virgl_opts=(-Dplatforms=egl -Dminigbm_allocation=false -Dvenus=$VENUS -Drender-server-worker=thread
	-Dtests=false -Dfuzzer=false -Dvalgrind=false -Dtracing=none -Dcheck-gl-errors=false)
stamp=$(cat "$ROOT"/gpu/patches/virglrenderer-$VIRGL_VERSION-*.patch 2>/dev/null \
	| { cat; echo "${virgl_opts[*]}"; } | sha256sum | cut -c1-16)
if [[ ! -f $v/.patched-$stamp ]]; then
	rm -rf "$v"
	tar -C "$W" -xf "$(fetch "https://gitlab.freedesktop.org/virgl/virglrenderer/-/archive/$VIRGL_VERSION/virglrenderer-$VIRGL_VERSION.tar.gz" "virglrenderer-$VIRGL_VERSION.tar.gz")"
	for p in "$ROOT"/gpu/patches/virglrenderer-$VIRGL_VERSION-*.patch; do
		[[ -f $p ]] && patch -d "$v" -p1 -s < "$p"
	done
	touch "$v/.patched-$stamp"
fi
# Venus runs in virglrenderer's render server (thread workers: no
# signalfd/clone on air/OS), which QEMU starts (RENDER_SERVER_EXEC_PATH)
[[ -f $v/build/build.ninja ]] || meson setup "$v/build" "$v" "${meson_args[@]}" "${virgl_opts[@]}" > "$W/virgl-setup.log"
ninja -C "$v/build" > "$W/virgl-build.log"
DESTDIR=$deps meson install -C "$v/build" --no-rebuild > /dev/null
rm -f "$Q/virgl_render_server"
if [[ $VENUS == true && ! -f $v/build/server/virgl_render_server ]]; then
	echo "build-gpu: Venus is on but virgl_render_server was not built" >&2
	exit 1
fi
if [[ -f $v/build/server/virgl_render_server ]]; then
	"${CROSS}strip" -o "$Q/virgl_render_server" "$v/build/server/virgl_render_server"
	patchelf --force-rpath --set-rpath '$ORIGIN/lib' "$Q/virgl_render_server"
fi
ls -la "$sys"/lib/libvirglrenderer.so* "$sys"/lib/libepoxy.so*
