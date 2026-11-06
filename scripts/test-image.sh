#!/bin/sh
# Edge cases of the container entrypoint and the runtime brscan4 download
# (images built with BRSCAN4=runtime). Needs Docker and, on amd64, access to
# download.brother.com.
#
# Usage: scripts/test-image.sh <image>
set -eu
img="${1:?usage: test-image.sh <image>}"
vol="airbridge-test-$$"
names=""
failed=0

cleanup() {
    for n in $names; do docker rm -f "$n" >/dev/null 2>&1 || true; done
    docker volume rm -f "$vol" >/dev/null 2>&1 || true
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*"
    failed=1
}

# start <name> [docker run args...]: run with the fake backend, wait for "ready".
start() {
    n="$1"
    shift
    names="$names $n"
    docker run -d --name "$n" -e AIRBRIDGE_BACKEND=fake -e AIRBRIDGE_PRINTING=off "$@" "$img" >/dev/null
    i=0
    until docker logs "$n" 2>&1 | grep -q 'airbridge: ready'; do
        i=$((i + 1))
        if [ "$i" -gt 240 ] || [ "$(docker inspect -f '{{.State.Running}}' "$n")" != true ]; then
            docker logs "$n" 2>&1 | sed 's/^/    /'
            fail "$n: never became ready"
            return 1
        fi
        sleep 0.5
    done
}

has() {
    docker logs "$1" 2>&1 | grep -qF -- "$2" || {
        docker logs "$1" 2>&1 | sed 's/^/    /'
        fail "$1: log lacks '$2'"
    }
}

lacks() {
    if docker logs "$1" 2>&1 | grep -qF -- "$2"; then
        docker logs "$1" 2>&1 | sed 's/^/    /'
        fail "$1: log has '$2'"
    fi
}

cached() {
    docker run --rm -v "$vol:/c" --entrypoint sh "$img" -c 'ls /c/*.deb 2>/dev/null | wc -l'
}

arch="$(docker run --rm --entrypoint dpkg "$img" --print-architecture)"
mode="$(docker run --rm --entrypoint printenv "$img" BRSCAN4_MODE)"
echo "image $img: arch=$arch BRSCAN4=$mode"
[ "$mode" = runtime ] || { echo "not a BRSCAN4=runtime image"; exit 2; }

echo "- the image holds no Brother code"
if docker run --rm --entrypoint sh "$img" -c 'test -e /opt/brother || find / -xdev -name "libsane-brother4*" | grep -q .'; then
    fail "Brother files found in the image"
fi

echo "- serves eSCL and passes its own healthcheck"
if start ab-escl -p 127.0.0.1:18090:8090 -e AIRBRIDGE_ALLOW=0.0.0.0/0 -e AIRBRIDGE_BRSCAN4=off; then
    caps="$(mktemp)"
    i=0
    until curl -sf -o "$caps" http://127.0.0.1:18090/eSCL/ScannerCapabilities || [ "$i" -gt 60 ]; do
        i=$((i + 1))
        sleep 0.5
    done
    grep -q '<pwg:MakeAndModel>Airbridge Demo Scanner</pwg:MakeAndModel>' "$caps" || fail "no demo scanner in ScannerCapabilities"
    rm -f "$caps"
    docker exec ab-escl airbridge healthcheck >/dev/null || fail "healthcheck failed"
fi

if [ "$arch" != amd64 ]; then
    echo "- $arch: brscan4 is skipped, airbridge starts"
    start ab-arch || true
    has ab-arch "brscan4 exists only for amd64"
    has ab-arch "brscan4=false"
    [ "$failed" = 0 ] && echo "all checks passed" || exit 1
    exit 0
fi

echo "- AIRBRIDGE_BRSCAN4=off skips the download"
start ab-off -e AIRBRIDGE_BRSCAN4=off || true
has ab-off "brscan4 disabled"
has ab-off "brscan4=false"
lacks ab-off "unknown setting"
lacks ab-off "downloading"

echo "- an invalid AIRBRIDGE_BRSCAN4 starts without brscan4"
start ab-bad-setting -e AIRBRIDGE_BRSCAN4=maybe || true
has ab-bad-setting "expected auto or off"
has ab-bad-setting "brscan4=false"

echo "- an unreachable download starts without brscan4"
start ab-unreachable -v "$vol:/var/lib/airbridge" -e BRSCAN4_URL=https://127.0.0.1:9/brscan4.deb || true
has ab-unreachable "brscan4 download failed"
has ab-unreachable "brscan4=false"
[ "$(cached)" = 0 ] || fail "a failed download left a file in the cache"

echo "- a download with the wrong SHA-256 is refused"
start ab-badsum -v "$vol:/var/lib/airbridge" -e BRSCAN4_SHA256=0000000000000000000000000000000000000000000000000000000000000000 || true
has ab-badsum "does not match SHA-256"
has ab-badsum "brscan4=false"
[ "$(cached)" = 0 ] || fail "a refused download was kept"

echo "- a read-only cache starts without brscan4"
start ab-readonly -v "$vol:/var/lib/airbridge:ro" || true
has ab-readonly "is not writable"
has ab-readonly "brscan4=false"

echo "- first start downloads from Brother, verifies and installs"
start ab-fetch -v "$vol:/var/lib/airbridge" || true
has ab-fetch "downloading brscan4"
has ab-fetch "brscan4 installed"
has ab-fetch "brscan4 found models="
has ab-fetch "brscan4=true"
[ "$(cached)" = 1 ] || fail "the download was not cached"

echo "- a restart of the same container reuses the installed driver"
docker restart ab-fetch >/dev/null
i=0
until [ "$(docker logs ab-fetch 2>&1 | grep -c 'airbridge: ready')" = 2 ] || [ "$i" -gt 60 ]; do
    i=$((i + 1))
    sleep 0.5
done
[ "$(docker logs ab-fetch 2>&1 | grep -c 'downloading brscan4')" = 1 ] || fail "restart downloaded again"
[ "$(docker logs ab-fetch 2>&1 | grep -c 'brscan4=true')" = 2 ] || fail "restart lost brscan4"
docker rm -f ab-fetch >/dev/null

echo "- a new container installs from the cache without downloading"
start ab-cache -v "$vol:/var/lib/airbridge" -e BRSCAN4_URL=https://127.0.0.1:9/brscan4.deb || true
lacks ab-cache "downloading"
has ab-cache "brscan4 installed"
has ab-cache "brscan4=true"
docker rm -f ab-cache >/dev/null

echo "- a corrupt cache is downloaded again"
docker run --rm -v "$vol:/c" --entrypoint sh "$img" -c 'for f in /c/*.deb; do echo garbage >"$f"; done'
start ab-corrupt -v "$vol:/var/lib/airbridge" || true
has ab-corrupt "does not match its SHA-256; downloading it again"
has ab-corrupt "brscan4 installed"
has ab-corrupt "brscan4=true"

echo "- the healthcheck command never installs anything"
out="$(docker run --rm -e AIRBRIDGE_BACKEND=fake "$img" healthcheck 2>&1 || true)"
case "$out" in *airbridge-entrypoint*) fail "healthcheck ran the brscan4 setup: $out" ;; esac

[ "$failed" = 0 ] && echo "all checks passed" || exit 1
