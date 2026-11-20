#!/usr/bin/env bash
# Build the guest kernel: upstream Linux with kernel/airdroid.config merged
# over the architecture's defconfig and the KVM guest fragment, no modules.
#   ARCH=x86_64 tools/build-kernel.sh      -> build/ARCH/vmlinuz
set -euo pipefail
. "$(dirname "$0")/common.sh"
src=$BUILD/linux-$LINUX_VERSION
# airDroid's kernel patches (kernel/patches); the tree is re-extracted when
# they change
stamp=$(cat "$ROOT"/kernel/patches/*.patch 2>/dev/null | sha256sum | cut -c1-16)
if [[ ! -f $src/.airdroid-$stamp ]]; then
	rm -rf "$src"
	tar -C "$BUILD" -xf "$(fetch "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$LINUX_VERSION.tar.xz" "linux-$LINUX_VERSION.tar.xz")"
	for p in "$ROOT"/kernel/patches/*.patch; do
		[[ -f $p ]] && patch -d "$src" -p1 -s < "$p"
	done
	touch "$src/.airdroid-$stamp"
fi
# objtool needs libelf's headers; take them from the distribution's packages
# without installing anything.
hostelf=$CACHE/hostlibelf
if [[ ! -f /usr/include/gelf.h && ! -f $hostelf/usr/include/gelf.h ]]; then
	mkdir -p "$hostelf/debs"
	(cd "$hostelf/debs" && apt-get download libelf-dev libelf1t64 zlib1g-dev libzstd-dev >/dev/null)
	for d in "$hostelf"/debs/*.deb; do dpkg-deb -x "$d" "$hostelf"; done
fi
make_args=(-C "$src" O="$BUILD/kernel" -j"$JOBS"
	HOSTCFLAGS="-I$hostelf/usr/include" HOSTLDFLAGS="-L$hostelf/usr/lib/x86_64-linux-gnu -Wl,-rpath,$hostelf/usr/lib/x86_64-linux-gnu")
image=arch/x86/boot/bzImage defconfig=x86_64_defconfig
if [[ $ARCH == arm64 ]]; then
	make_args+=(ARCH=arm64 CROSS_COMPILE=${CROSS_COMPILE:-/data1/airos/toolchain/cubie-host/bin/aarch64-linux-gnu-})
	image=arch/arm64/boot/Image defconfig=defconfig
fi
mkdir -p "$BUILD/kernel"
make "${make_args[@]}" "$defconfig" >/dev/null
(cd "$BUILD/kernel" && "$src/scripts/kconfig/merge_config.sh" -m .config \
	"$src/kernel/configs/kvm_guest.config" "$ROOT/kernel/airdroid.config" >/dev/null)
make "${make_args[@]}" olddefconfig >/dev/null
# report fragment options the merge dropped (missing dependencies)
while IFS= read -r line; do
	[[ $line =~ ^(CONFIG_[A-Z0-9_]+)=(.*)$ ]] || continue
	opt=${BASH_REMATCH[1]} want=${BASH_REMATCH[2]}
	got=$(grep -E "^$opt=|^# $opt is not set" "$BUILD/kernel/.config" | head -1 || true)
	if [[ $want == n ]]; then
		[[ -z $got || $got == "# $opt is not set" ]] || echo "kernel config: $opt wanted n, got $got" >&2
	elif [[ $got != "$opt=$want" ]]; then
		echo "kernel config: $opt wanted $want, got ${got:-unset}" >&2
	fi
done < "$ROOT/kernel/airdroid.config"
make "${make_args[@]}" "$(basename "$image")" >/dev/null
cp "$BUILD/kernel/$image" "$BUILD/vmlinuz"
ls -la "$BUILD/vmlinuz"
