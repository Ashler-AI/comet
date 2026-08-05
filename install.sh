#!/bin/sh
# Ashler Comet headless installer.
#
#   export COMET_RELEASES_URL=https://storage.googleapis.com/<private-bucket>/releases
#   export COMET_RELEASES_AUTHORIZATION="Bearer $(gcloud auth print-access-token)"
#   gcloud storage cat gs://<private-bucket>/releases/install.sh | sh
#
# `COMET_RELEASES_URL` must select the matching private immutable GCS feed.
# `COMET_RELEASES_AUTHORIZATION` is consumed immediately and removed from the
# environment before curl starts; it is never placed in argv or written to disk.
# The release workflow publishes SHA256SUMS; missing or mismatched files fail.
#
# Installs the self-contained native binary to ~/.comet-native/app, puts
# `comet` on PATH, and — once signed in — runs it as a systemd user service.
set -eu

RELEASES="${COMET_RELEASES_URL:?set COMET_RELEASES_URL to the private Ashler Comet GCS HTTPS prefix}"
RELEASE_AUTHORIZATION="${COMET_RELEASES_AUTHORIZATION:-}"
unset COMET_RELEASES_AUTHORIZATION

release_fetch() {
  if [ -z "$RELEASE_AUTHORIZATION" ]; then
    echo "ashler comet install: COMET_RELEASES_AUTHORIZATION is required for the private release feed" >&2
    exit 1
  fi
  case "$RELEASE_AUTHORIZATION" in
    Bearer\ *) release_token="${RELEASE_AUTHORIZATION#Bearer }" ;;
    *) release_token= ;;
  esac
  case "$release_token" in
    ""|*[!A-Za-z0-9._~+/=:-]*)
      echo "ashler comet install: invalid release authorization header" >&2
      exit 1
      ;;
  esac
  # Curl reads the private header from stdin. The bearer is neither an argument
  # nor an inherited environment variable in the curl process.
  printf 'Authorization: %s\n' "$RELEASE_AUTHORIZATION" | curl --header @- "$@"
}

# --- platform ---------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) plat=linux ;;
  Darwin)
    echo "comet install: on macOS, download the desktop app instead:" >&2
    echo "  $RELEASES/latest.txt → $RELEASES/comet-<version>-macos-arm64.dmg" >&2
    exit 1
    ;;
  *)
    echo "comet install: unsupported OS '$os' — only Linux for now." >&2
    exit 1
    ;;
esac
case "$arch" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *)
    echo "comet install: unsupported architecture '$arch'." >&2
    exit 1
    ;;
esac

# --- download ----------------------------------------------------------------
ver="$(release_fetch -fsSL "$RELEASES/latest.txt" | tr -d '[:space:]')"
[ -n "$ver" ] || { echo "ashler comet install: could not resolve latest version" >&2; exit 1; }
file="comet-$ver-$plat-$arch.tar.gz"
data_root="$HOME/.comet-native"
app_root="$data_root/app"
dest="$app_root/$ver"

if [ -x "$dest/comet" ]; then
  echo "comet $ver already downloaded — relinking."
else
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  echo "downloading Ashler Comet $ver ($plat-$arch)…"
  release_fetch -fSL --progress-bar "$RELEASES/$file" -o "$tmp/$file"
  release_fetch -fsSL "$RELEASES/SHA256SUMS" -o "$tmp/SHA256SUMS"
  expected="$(awk -v file="$file" '$2 == file || $2 == "*" file { print $1; exit }' "$tmp/SHA256SUMS")"
  [ -n "$expected" ] || { echo "ashler comet install: $file is missing from SHA256SUMS" >&2; exit 1; }
  if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp/$file" | awk '{print $1}')"
  elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$tmp/$file" | awk '{print $1}')"
  else
    echo "ashler comet install: sha256sum or shasum is required" >&2
    exit 1
  fi
  [ "$actual" = "$expected" ] || { echo "ashler comet install: checksum mismatch for $file" >&2; exit 1; }
  mkdir -p "$dest"
  tar -xzf "$tmp/$file" -C "$dest" --strip-components=1
fi

ln -sfn "$dest" "$app_root/current"
mkdir -p "$HOME/.local/bin"
ln -sf "$app_root/current/comet" "$HOME/.local/bin/comet"

# --- service -----------------------------------------------------------------
# Auth is decoupled from the daemon: `comet login` persists the session and a
# service-managed `comet headless` loads it (exiting with "run comet login
# first" otherwise) — so the service starts only after first sign-in.
signed_in=no
[ -f "$data_root/session.json" ] && signed_in=yes

service=manual
if command -v systemctl >/dev/null 2>&1 && [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  mkdir -p "$HOME/.config/systemd/user"
  cat >"$HOME/.config/systemd/user/comet-native.service" <<'UNIT'
[Unit]
Description=Ashler Comet headless engine
After=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
ExecStart=%h/.comet-native/app/current/comet headless
Restart=on-failure
RestartSec=5


[Install]
WantedBy=default.target
UNIT
  systemctl --user daemon-reload
  systemctl --user enable comet-native >/dev/null 2>&1 || true
  if [ "$signed_in" = yes ]; then
    systemctl --user restart comet-native
    service=running
  else
    service=ready
  fi
  # Keep the user manager (and the engine) running without an active login.
  loginctl enable-linger "$USER" 2>/dev/null \
    || sudo -n loginctl enable-linger "$USER" 2>/dev/null \
    || echo "warn: could not enable linger — the engine stops when you log out (run: sudo loginctl enable-linger $USER)"
else
  echo "warn: systemd user session not available — run the engine manually with: comet headless"
fi

# --- agent CLIs ---------------------------------------------------------------
command -v claude >/dev/null 2>&1 || \
  echo "note: Claude Code CLI is optional. Install it through Ashler-approved tooling."

case ":$PATH:" in
  *":$HOME/.local/bin:"*) path_hint="" ;;
  *) path_hint=' (add ~/.local/bin to your PATH)' ;;
esac

echo ""
echo "Ashler Comet $ver installed$path_hint"
echo ""
case "$service" in
  running)
    echo "the engine restarted with the new version."
    echo "  systemctl --user status comet-native    check the service"
    ;;
  ready)
    echo "next steps:"
    echo "  comet login                              sign in (paste-code) and exit"
    echo "  systemctl --user start comet-native      then start the engine"
    ;;
  manual)
    echo "next: \`comet login\` to sign in, then run the engine with \`comet headless\`."
    ;;
esac
