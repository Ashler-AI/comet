#!/bin/sh
set -eu
install=${1:-install.sh}
grep -F 'OMP_VERSION=17.2.9' "$install" >/dev/null
grep -F 'omp-darwin-arm64' "$install" >/dev/null
grep -F '3f9c44c465da8428b5a81a0c9cdac22ced982319fe93d534914cb61838a63118' "$install" >/dev/null
grep -F 'omp-darwin-x64' "$install" >/dev/null
grep -F '35c36f893a68feb6df3a61ff9359bb6ad13a5534687bb0396508aabc69c5f347' "$install" >/dev/null
sh -n "$install"
