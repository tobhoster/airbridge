#!/bin/sh
# Fails when any criterion benchmark is more than 20% slower than on BASE.
# Both sides run on the same machine, back to back, so CI runners of
# different speed do not matter.
#
# Usage: scripts/bench-gate.sh <base-git-ref> [max-regression-percent]
set -eu
base="${1:?usage: bench-gate.sh <base-ref> [percent]}"
limit="${2:-20}"
root="$(git rev-parse --show-toplevel)"
cd "$root"
tree="$(mktemp -d)"
trap 'git worktree remove --force "$tree" >/dev/null 2>&1 || true' EXIT
git worktree add --detach "$tree" "$base" >/dev/null
if [ -f "$tree/benches/perf.rs" ]; then
    (cd "$tree" && CARGO_TARGET_DIR="$root/target" cargo bench --bench perf -- --save-baseline base --noplot)
else
    echo "base $base has no benchmarks; recording only"
    cargo bench --bench perf -- --noplot
    exit 0
fi
cargo bench --bench perf -- --baseline base --noplot
fail=0
for change in target/criterion/*/*/change/estimates.json target/criterion/*/change/estimates.json; do
    [ -f "$change" ] || continue
    name="${change#target/criterion/}"
    name="${name%/change/estimates.json}"
    pct=$(python3 -c 'import json,sys; print(round(json.load(open(sys.argv[1]))["mean"]["point_estimate"]*100, 1))' "$change")
    echo "$name: ${pct}% vs $base"
    if python3 -c 'import sys; sys.exit(0 if float(sys.argv[1]) > float(sys.argv[2]) else 1)' "$pct" "$limit"; then
        echo "FAIL: $name regressed by ${pct}% (limit ${limit}%)"
        fail=1
    fi
done
exit $fail
