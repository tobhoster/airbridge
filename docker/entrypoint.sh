#!/bin/sh
# Container entrypoint.
#
# Images built with BRSCAN4=runtime (the published image) contain no Brother
# code. On start, brscan4 is installed from the cache in $BRSCAN4_CACHE; the
# first start downloads it from Brother and checks its SHA-256, exactly as a
# local build does. Any failure leaves airbridge running without brscan4
# (WSD and the other SANE backends still work) and is retried next start.
#
# AIRBRIDGE_BRSCAN4=off skips brscan4 entirely.
set -eu

log() { echo "airbridge-entrypoint: $*" >&2; }

verify() { echo "$BRSCAN4_SHA256  $1" | sha256sum -c --status - 2>/dev/null; }

brscan4_runtime() {
    [ "${BRSCAN4_MODE:-}" = runtime ] || return 0
    # Already installed by an earlier start of this container.
    [ ! -x /opt/brother/scanner/brscan4/brsaneconfig4 ] || return 0
    case "${AIRBRIDGE_BRSCAN4:-auto}" in
        auto | on | true | 1 | "") ;;
        off | false | 0)
            log "brscan4 disabled (AIRBRIDGE_BRSCAN4=off)"
            return 0
            ;;
        *)
            log "AIRBRIDGE_BRSCAN4=$AIRBRIDGE_BRSCAN4: expected auto or off; starting without brscan4"
            return 0
            ;;
    esac
    arch="$(dpkg --print-architecture)"
    if [ "$arch" != amd64 ]; then
        log "brscan4 exists only for amd64 (this is $arch); Brother scanners are reached through WSD"
        return 0
    fi
    deb="$BRSCAN4_CACHE/brscan4-$BRSCAN4_SHA256.deb"
    if [ -f "$deb" ] && ! verify "$deb"; then
        log "cached brscan4 does not match its SHA-256; downloading it again"
        rm -f "$deb"
    fi
    if [ ! -f "$deb" ]; then
        if ! mkdir -p "$BRSCAN4_CACHE" 2>/dev/null || [ ! -w "$BRSCAN4_CACHE" ]; then
            log "$BRSCAN4_CACHE is not writable; starting without brscan4"
            return 0
        fi
        log "downloading brscan4 from $BRSCAN4_URL"
        part="$deb.part"
        # Redirects must stay on HTTPS.
        if ! curl -fsSL --proto-redir '=https' --retry 2 --connect-timeout 10 --max-time 120 \
            -o "$part" "$BRSCAN4_URL"; then
            rm -f "$part"
            log "brscan4 download failed; starting without it (retried on next start)"
            return 0
        fi
        if ! verify "$part"; then
            rm -f "$part"
            log "brscan4 download does not match SHA-256 $BRSCAN4_SHA256; refused, starting without it"
            return 0
        fi
        mv "$part" "$deb"
        # Drop debs of other brscan4 versions (an older image's pin).
        for old in "$BRSCAN4_CACHE"/brscan4-*.deb; do
            [ "$old" = "$deb" ] || rm -f "$old"
        done
    fi
    if /usr/local/lib/airbridge/brscan4-install.sh "$deb"; then
        log "brscan4 installed from $deb"
    else
        log "brscan4 install failed; starting without it"
    fi
}

# `airbridge healthcheck` (Docker's HEALTHCHECK) must not install anything.
[ "${1:-}" = healthcheck ] || brscan4_runtime
exec /usr/local/bin/airbridge "$@"
