#!/usr/bin/env bash
# The 10 GB streaming suite (T5.14; NFR-2, NFR-4, NFR-11, NFR-12), driven through tmux.
# Usage: scripts/suite-10g.sh <dir> <file>   (spill files go to <dir> via TMPDIR)
set -euo pipefail
dir=${1:?usage: suite-10g.sh <dir> <file>}
file=${2:?usage: suite-10g.sh <dir> <file>}
dv=$PWD/target/release/dv
session=dv-suite-$$
export TMPDIR=$dir

now() { perl -MTime::HiRes=time -e 'printf "%.0f\n", time * 1000'; }
screen() { tmux capture-pane -p -t "$session"; }
rss() { ps -o rss= -p "$pid" | awk '{ printf "%.0f", $1 / 1024 }'; }

# Milliseconds from sending keys until the screen changes.
latency() {
    local before t
    before=$(screen)
    t=$(now)
    tmux send-keys -t "$session" "$@"
    while [ "$(screen)" = "$before" ]; do sleep 0.002; done
    echo $(($(now) - t))
}

trap 'tmux kill-session -t "$session" 2>/dev/null || true' EXIT
start=$(now)
tmux new-session -d -s "$session" -x 120 -y 40 "$dv --mode stream '$file'"
until screen | grep -q '▼'; do sleep 0.005; done
echo "first screen: $(($(now) - start)) ms"
pid=$(pgrep -n -f "$dv --mode stream")
echo "expand root while indexing: $(latency l) ms, RSS $(rss) MB"
until screen | tail -1 | grep -Eq '[0-9] values'; do sleep 0.25; done
echo "indexing done: $(($(now) - start)) ms, RSS $(rss) MB"

samples=()
for keys in G l G l G gg G PageUp PageUp gg; do
    # shellcheck disable=SC2086 # `gg` is sent as two keys
    ms=$(latency $(echo "$keys" | sed 's/^gg$/g g/'))
    samples+=("$(rss)")
    echo "  $keys: ${ms} ms, RSS ${samples[-1]} MB"
done
printf 'RSS while browsing: min %s MB, max %s MB\n' \
    "$(printf '%s\n' "${samples[@]}" | sort -n | head -1)" \
    "$(printf '%s\n' "${samples[@]}" | sort -n | tail -1)"
tmux send-keys -t "$session" q
