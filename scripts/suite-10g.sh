#!/usr/bin/env bash
# The 10 GB streaming suite (T5.14; NFR-2, NFR-4, NFR-11, NFR-12), driven through tmux.
# Usage: [DV_MODE=auto|memory|stream] scripts/suite-10g.sh <dir> <file>   (spill files go to
# <dir> via TMPDIR; the mode defaults to stream)
set -euo pipefail
dir=${1:?usage: suite-10g.sh <dir> <file>}
file=${2:?usage: suite-10g.sh <dir> <file>}
dv=$PWD/target/release/dv
session=dv-suite-$$
mode=${DV_MODE:-stream}
export TMPDIR=$dir

now() { perl -MTime::HiRes=time -e 'printf "%.0f\n", time * 1000'; }
screen() { tmux capture-pane -p -t "$session"; }
rss() { ps -o rss= -p "$pid" | awk '{ printf "%.0f", $1 / 1024 }'; }

# Waits until `cmd` succeeds, failing the suite after `ms` milliseconds.
wait_for() {
    local ms=$1 what=$2 deadline
    shift 2
    deadline=$(($(now) + ms))
    until "$@"; do
        if [ "$(now)" -gt "$deadline" ]; then
            echo "timed out waiting for $what" >&2
            exit 1
        fi
        sleep 0.005
    done
}

# Milliseconds from sending keys until the screen changes ("no change" after 2 s).
latency() {
    local before t deadline
    before=$(screen)
    t=$(now)
    deadline=$((t + 2000))
    tmux send-keys -t "$session" "$@"
    while [ "$(screen)" = "$before" ]; do
        if [ "$(now)" -gt "$deadline" ]; then
            echo "no change"
            return
        fi
        sleep 0.002
    done
    echo "$(($(now) - t)) ms"
}

has_tree() { screen | grep -q '▼'; }
indexed() { screen | tail -1 | grep -Eq '[0-9] values'; }

trap 'tmux kill-session -t "$session" 2>/dev/null || true' EXIT
start=$(now)
tmux new-session -d -s "$session" -x 120 -y 40 "$dv --mode $mode '$file'"
wait_for 120000 "the first screen" has_tree
echo "first screen: $(($(now) - start)) ms"
pid=$(pgrep -n -f "$dv --mode $mode")
echo "expand root while indexing: $(latency l), RSS $(rss) MB"
wait_for 1800000 "indexing to finish" indexed
echo "indexing done: $(($(now) - start)) ms, RSS $(rss) MB"

samples=()
for keys in G l G l G gg G PageUp PageUp gg; do
    # shellcheck disable=SC2086 # `gg` is sent as two keys
    took=$(latency $(echo "$keys" | sed 's/^gg$/g g/'))
    samples+=("$(rss)")
    echo "  $keys: ${took}, RSS ${samples[-1]} MB"
done
printf 'RSS while browsing: min %s MB, max %s MB\n' \
    "$(printf '%s\n' "${samples[@]}" | sort -n | head -1)" \
    "$(printf '%s\n' "${samples[@]}" | sort -n | tail -1)"
tmux send-keys -t "$session" q
