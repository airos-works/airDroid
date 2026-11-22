#!/bin/sh
# race.sh NAME [HOGS] [SECONDS]: airDroid under host CPU contention, so that
# its vCPU threads are preempted and moved between CPUs all the time (what
# the os_preempt_disable() race in the NVMM driver needs). A line every two
# seconds, synced, says how far it got.
export PATH=/bin:/boot/system/bin
NAME=$1; HOGS=${2:-48}; SECS=${3:-360}
mkdir -p /boot/home/airdroid-test; cd /boot/home/airdroid-test
C=/boot/home/config/cache/airDroid/console.log
O=race-$NAME.txt
rm -f $O race-$NAME.done $C
kill $(ps | grep "[s]creen_blanker" | awk '{print $(NF-3)}') 2>/dev/null
echo "$(date '+%H:%M:%S') start: $(uname -a | grep -o 'hrev[0-9+]*'), nvmm $(grep -a 'loaded driver.*nvmm' /var/log/syslog | tail -1 | grep -o '/boot/.*')" >> $O
(/boot/system/apps/airDroid > /boot/home/airdroid-race.log 2>&1 &)
for i in $(seq 1 120); do grep -qaE "airdroid: (Booted|Failed)" $C 2>/dev/null && break; sleep 1; done
echo "$(date '+%H:%M:%S') $(grep -aE 'airdroid: (Booted|Failed)' $C | sed 's/\r//' | cut -c1-60)" >> $O; sync
sleep 8
airdroid app launch com.android.settings > /dev/null 2>&1
sleep 6
(while [ ! -f race-$NAME.done ]; do airdroid shell input swipe 960 800 960 300 300; airdroid shell input swipe 960 300 960 800 300; done > /dev/null 2>&1 &)
rm -f hogs.pid
for i in $(seq 1 $HOGS); do sh -c 'while :; do :; done' & echo $! >> hogs.pid; done
echo "$(date '+%H:%M:%S') $HOGS hogs running" >> $O; sync
T0=$(date +%s)
while [ $(( $(date +%s) - T0 )) -lt $SECS ]; do
	echo "$(date '+%H:%M:%S') alive $(( $(date +%s) - T0 )) s; android $(airdroid status 2>&1 | head -1 | cut -c1-40)" >> $O
	sync
	sleep 2
done
kill $(cat hogs.pid) 2>/dev/null; rm -f hogs.pid
echo done > race-$NAME.done
sleep 2
hey airDroid quit >/dev/null 2>&1
for i in $(seq 1 40); do ps | grep -qE "[a]pps/airDroid|[l]ib/airDroid/qemu" || break; sleep 1; done
echo "$(date '+%H:%M:%S') finished; left running $(ps | grep -cE '[a]pps/airDroid|[l]ib/airDroid/qemu')" >> $O; sync
