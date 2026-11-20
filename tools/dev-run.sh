#!/usr/bin/env bash
# Boot the VM headless with airdroid-dev on a Linux host (KVM); commands go
# to build/state/cmd (echo "shot 1 /tmp/x.png" > build/state/cmd), logs to
# build/state/dev.log. IMAGES defaults to /mnt/HaikuWork/build/airdroid-images.
set -euo pipefail
. "$(dirname "$0")/common.sh"
state=$ROOT/build/state
mkdir -p "$state"
pkill -x airdroid-dev 2>/dev/null || true
if [[ -f $state/qemu.pid ]]; then kill "$(cat "$state/qemu.pid")" 2>/dev/null || true; fi
sleep 1
rm -f "$state/cmd"; mkfifo "$state/cmd"
(cd "$ROOT" && cargo build --release -q -p airdroid-dev)
exec 3<>"$state/cmd"
nohup "$ROOT/target/release/airdroid-dev" --images "${IMAGES:-/mnt/HaikuWork/build/airdroid-images}" \
	--guest "$BUILD" --state "$state" --ram "${RAM:-4096}" --cpus "${CPUS:-8}" --size "${SIZE:-1280x800}" \
	--multiwin "${MULTIWIN:-0}" --cmd "$state/cmd" "$@" > "$state/dev.out" 2> "$state/dev.log" < /dev/null &
echo "airdroid-dev pid $!"
