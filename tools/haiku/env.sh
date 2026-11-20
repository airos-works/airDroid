# Cross-compilation environment for air/OS, the same toolchain airos-ci
# uses (its SDK and pinned Rust nightly). Source it:  . tools/haiku/env.sh
# ARCH=x86_64 (default) or arm64.
AD_HAIKU_TOOLS=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ARCH=${ARCH:-x86_64}
AIROS_SDK=${AIROS_SDK:-/data1/airos/sdk}
AIROS_TOOLCHAIN=${AIROS_TOOLCHAIN:-/data1/airos/toolchain}
. "$AIROS_SDK/$ARCH/env.sh"
. "$AIROS_TOOLCHAIN/rust/env-$ARCH.sh"
export RUST_TARGET
# the CI cargo home belongs to the runner: keep its toolchain, own registry
[[ -w $CARGO_HOME/registry ]] || export CARGO_HOME=${AIRDROID_CARGO_HOME:-/mnt/HaikuWork/toolchains/rust/cargo}
export AIROS_SYSROOT=$SYSROOT AIROS_CROSS=$CROSS AIROS_UNWIND_SPECS=$UNWIND_SPECS AIROS_TOOLS=$TOOLS
_triple=${RUST_TARGET//-/_}
export "CARGO_TARGET_${_triple^^}_LINKER=$AD_HAIKU_TOOLS/haiku-cc"
export "CC_$_triple=$AD_HAIKU_TOOLS/haiku-cc" "CXX_$_triple=$AD_HAIKU_TOOLS/haiku-c++"
export "AR_$_triple=${CROSS}ar" "RANLIB_$_triple=${CROSS}ranlib"
unset _triple
