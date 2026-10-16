#!/bin/sh
# Cargo runner (see .cargo/config.toml): every test, bench and `cargo run`
# binary executes in a fresh, empty network namespace with only loopback.
# This is what makes the default test suite hermetic: a test that tried to
# reach the LAN would fail with "network unreachable" instead of sending a
# packet.
#
# AIRBRIDGE_ALLOW_NETWORK=1 opts out (hardware suite, running the daemon).
set -eu
if [ "${AIRBRIDGE_ALLOW_NETWORK:-0}" = 1 ]; then
    exec "$@"
fi
exec unshare --user --map-root-user --net -- \
    sh -c 'ip link set lo up && exec "$@"' airbridge-netns "$@"
