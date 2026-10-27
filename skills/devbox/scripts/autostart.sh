#!/usr/bin/env bash
# Shared boot and explicit-wake entry point; never provision or authenticate.
set -euo pipefail
channel="${1:-production}"
[[ $# -le 1 ]] || { echo 'Expected one release channel.' >&2; exit 1; }
case "$channel" in staging|production) ;; *) echo 'Expected staging or production.' >&2; exit 1 ;; esac
exec env CREW_CHANNEL="$channel" python3 "$HOME/.local/share/crew-devbox/$channel/bootstrap.py" start
