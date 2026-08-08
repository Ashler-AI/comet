#!/usr/bin/env bash
# Stable entrypoint for the deterministic Ashler Comet collaboration smoke.
# It uses two in-memory headless devices and requires no cloud credentials,
# installed agent CLI, network listener, or persistent state.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec node "$ROOT/scripts/headless-collaboration-smoke.mjs"
