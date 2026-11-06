#!/bin/sh
# Fails when the image's root filesystem exceeds its architecture's line in
# container-size-budget. Debian's arm64 packages are ~10% larger than amd64's,
# hence one budget per architecture.
#
# `docker image inspect .Size` differs between storage drivers (compressed vs
# unpacked); the unpacked root filesystem does not.
#
# Usage: scripts/size-budget.sh <image>
set -eu
img="${1:?usage: size-budget.sh <image>}"
root="$(git rev-parse --show-toplevel)"
arch="$(docker run --rm --entrypoint dpkg "$img" --print-architecture)"
size="$(docker run --rm --entrypoint du "$img" -sxb / | cut -f1)"
budget="$(awk -v a="$arch" '$1 == a { print $2 }' "$root/container-size-budget")"
[ -n "$budget" ] || { echo "no budget for $arch in container-size-budget"; exit 1; }
echo "$arch root filesystem ${size} bytes, budget ${budget}"
[ "$size" -le "$budget" ]
