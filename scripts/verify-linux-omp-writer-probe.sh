#!/usr/bin/env bash
# Every packaged Linux executable that hosts OMP sessions must answer the
# hidden writer probe before CLI parsing: Linux resume runs its own executable
# with it, and an executable that rejects it makes every saved session
# unresumable ("Could not verify that this Crew session is inactive").
# Usage: scripts/verify-linux-omp-writer-probe.sh EXECUTABLE...
set -euo pipefail

[[ $# -gt 0 ]] || { echo "usage: $0 EXECUTABLE..." >&2; exit 2; }
dir="$(mktemp -d)"
trap 'rm -rf "$dir"' EXIT
mkdir "$dir/home"
journal="$dir/session.jsonl"
printf '{"type":"session"}\n' > "$journal"

probe() {
  local executable="$1" expected="$2" state
  # A build without the dispatch treats the marker as an ordinary launch:
  # bound it and keep it away from any real profile.
  if ! state="$(HOME="$dir/home" timeout 20 "$executable" __comet-omp-writer-probe "$journal" </dev/null)"; then
    echo "::error::$executable rejected the OMP writer probe" >&2
    return 1
  fi
  if [[ "$state" != "$expected" ]]; then
    echo "::error::$executable reported '$state' instead of '$expected' for the OMP writer probe" >&2
    return 1
  fi
}

for executable in "$@"; do
  probe "$executable" I
  # A held write descriptor is an active writer, never an inactive journal.
  exec 9>>"$journal"
  probe "$executable" A
  exec 9>&-
  echo "$executable answers the OMP writer probe"
done
