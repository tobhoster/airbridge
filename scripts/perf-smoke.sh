#!/bin/sh
# Resource baselines of the release binary, measured in an isolated network
# namespace (a dummy "LAN" interface, no real network):
#   - startup to first mDNS announcement < 2 s (fake backend: no SANE probe)
#   - idle RSS < 20 MiB
#   - idle CPU ~0: at most 5 clock ticks (50 ms) over 10 s
#
# Usage: scripts/perf-smoke.sh [path/to/airbridge]   (default: target/release/airbridge)
set -eu
bin="${1:-target/release/airbridge}"
[ -x "$bin" ] || { echo "build first: cargo build --release"; exit 2; }
bin="$(realpath "$bin")"
exec unshare --user --map-root-user --net -- sh -eu -c '
bin="$1"
ip link set lo up
ip link add lan0 type dummy
ip addr add 10.77.0.2/24 dev lan0
ip link set lan0 multicast on up
ip route add default via 10.77.0.1 dev lan0
log="$(mktemp)"
t0=$(date +%s%N)
AIRBRIDGE_BACKEND=fake AIRBRIDGE_PRINTING=off AIRBRIDGE_BRSCAN4_DIR=/nonexistent RUST_LOG=info "$bin" >"$log" 2>&1 &
pid=$!
while ! grep -q "announced" "$log"; do
    if ! kill -0 "$pid" 2>/dev/null; then cat "$log"; echo "airbridge exited"; exit 1; fi
    if [ $(( ($(date +%s%N) - t0) / 1000000 )) -gt 5000 ]; then cat "$log"; echo "no announcement within 5 s"; exit 1; fi
    sleep 0.01
done
startup_ms=$(( ($(date +%s%N) - t0) / 1000000 ))
sleep 5
ticks() { awk "{print \$14 + \$15}" /proc/$pid/stat; }
c0=$(ticks); sleep 10; c1=$(ticks)
rss_kb=$(awk "/^VmRSS/ {print \$2}" /proc/$pid/status)
kill "$pid"; wait "$pid" || true
idle_ticks=$((c1 - c0))
echo "startup_to_announced_ms=$startup_ms idle_rss_kib=$rss_kb idle_cpu_ticks_10s=$idle_ticks"
fail=0
[ "$startup_ms" -lt 2000 ] || { echo "FAIL: startup ${startup_ms} ms >= 2000"; fail=1; }
[ "$rss_kb" -lt 20480 ] || { echo "FAIL: idle RSS ${rss_kb} KiB >= 20 MiB"; fail=1; }
[ "$idle_ticks" -le 5 ] || { echo "FAIL: idle CPU ${idle_ticks} ticks in 10 s"; fail=1; }
exit $fail
' perf-smoke "$bin"
