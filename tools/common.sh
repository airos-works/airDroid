# Sourced by the build scripts: versions and paths.
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ARCH=${ARCH:-x86_64}
case $ARCH in
	x86_64) LINUX_ARCH=x86_64 RUST_GUEST_TARGET=x86_64-unknown-linux-musl ;;
	arm64) LINUX_ARCH=arm64 RUST_GUEST_TARGET=aarch64-unknown-linux-musl ;;
	*) echo "unknown architecture $ARCH" >&2; exit 1 ;;
esac
BUILD=${AIRDROID_BUILD:-$ROOT/build/$ARCH}
CACHE=${AIRDROID_CACHE:-${AIROS_CACHE:-$HOME/.cache}/airdroid}
JOBS=${JOBS:-$(nproc)}
mkdir -p "$BUILD" "$CACHE"

# The same kernel as linuxvm (apps/linuxvm), so one tarball serves both.
LINUX_VERSION=6.18.52

# Rust: the workspace toolchain when present (the air/OS build host).
if [[ -d /mnt/HaikuWork/toolchains/rust ]]; then
	export RUSTUP_HOME=${RUSTUP_HOME:-/mnt/HaikuWork/toolchains/rust/rustup}
	export CARGO_HOME=${CARGO_HOME:-/mnt/HaikuWork/toolchains/rust/cargo}
	export PATH=$CARGO_HOME/bin:$PATH
fi

fetch() { # fetch URL FILE: download into the cache once, print its path
	local url=$1 out=$CACHE/$2
	if [[ ! -s $out ]]; then
		for d in /data2/airos/cache/linuxvm /data3/airos/cache/linuxvm; do
			[[ -s $d/$2 ]] && { cp "$d/$2" "$out"; echo "$out"; return; }
		done
		curl -fsSL --retry 3 -o "$out.part" "$url" || { echo "download failed: $url" >&2; exit 1; }
		mv -f "$out.part" "$out"
	fi
	echo "$out"
}
