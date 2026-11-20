#!/usr/bin/env bash
# Everything, in order, for an airDroid package (what CI runs):
#   ARCH=x86_64 LINUXVM=/path/to/linuxvm tools/build-all.sh
#     -> artifacts/airdroid-VERSION-ARCH.hpkg
# LINUXVM: a checkout of airos-works/linuxvm (its QEMU build is reused).
set -euo pipefail
here=$(dirname "$0")
"$here/build-kernel.sh"
"$here/build-guest.sh"
"$here/build-qemu.sh"   # QEMU with OpenGL and virglrenderer (build-gpu.sh inside)
"$here/haiku/package.sh"
