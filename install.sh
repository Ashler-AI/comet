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
# After the comet install it bootstraps any missing agent CLIs (OMP, Claude
# Code, Codex). Bootstrap failures never abort the comet install; set
# COMET_SKIP_AGENT_BOOTSTRAP=1 to skip the phase in managed environments.
set -eu

# OMP is an independently released ACP runtime. Its bootstrap downloads an
# executable from the official upstream GitHub release, pinned by sha256.
OMP_VERSION=17.2.9
OMP_RELEASE_BASE="https://github.com/can1357/oh-my-pi/releases/download/v$OMP_VERSION"

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    echo "ashler comet install: sha256sum or shasum is required" >&2
    exit 1
  fi
}

install_omp() {
  omp_arch="$(uname -m)"
  case "$omp_arch" in
    arm64 | aarch64) omp_arch=arm64 ;;
    x86_64 | amd64) omp_arch=x64 ;;
    *)
      echo "ashler comet install: unsupported architecture '$omp_arch' for OMP" >&2
      exit 1
      ;;
  esac
  case "$(uname -s)" in
    Darwin) asset="omp-darwin-$omp_arch" ;;
    Linux)
      # Alpine and friends ship musl; everything else gets the glibc build.
      if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
        asset="omp-linux-musl-$omp_arch"
      else
        asset="omp-linux-$omp_arch"
      fi
      ;;
    *)
      echo "ashler comet install: unsupported OS '$(uname -s)' for OMP" >&2
      exit 1
      ;;
  esac
  case "$asset" in
    omp-darwin-arm64)     expected=3f9c44c465da8428b5a81a0c9cdac22ced982319fe93d534914cb61838a63118 ;;
    omp-darwin-x64)       expected=35c36f893a68feb6df3a61ff9359bb6ad13a5534687bb0396508aabc69c5f347 ;;
    omp-linux-arm64)      expected=e3c4b0a96dbe14f68a65aa4158bdc15252a0fc352691517fb2a07bf85e97e283 ;;
    omp-linux-x64)        expected=4f7aeb33b2f347c11a5ac8c73630e31d02c0a3eef3693468880b9f5e8f02a02b ;;
    omp-linux-musl-arm64) expected=e2606e23c422849668e9927b0d9e952818dca145c09cc327b66f17522923258d ;;
    omp-linux-musl-x64)   expected=f08e14ec39d92774e3080e6c32038ed8d0d8ab9daa396991a08ae52128789933 ;;
  esac

  destination="${OMP_INSTALL_PATH:-$HOME/.local/bin/omp}"
  if [ -e "$destination" ]; then
    actual="$(sha256_file "$destination")"
    [ "$actual" = "$expected" ] || {
      echo "ashler comet install: existing $destination is not the pinned official OMP $OMP_VERSION artifact" >&2
      echo "remove or relocate it, then rerun this explicit bootstrap" >&2
      exit 1
    }
    echo "OMP $OMP_VERSION is already installed and verified at $destination"
    return
  fi

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT HUP INT TERM
  echo "downloading official OMP $OMP_VERSION ($asset)…"
  curl -fL --proto '=https' --tlsv1.2 "$OMP_RELEASE_BASE/$asset" -o "$tmp/omp" || {
    echo "ashler comet install: download failed for official OMP $OMP_VERSION $asset" >&2
    exit 1
  }
  actual="$(sha256_file "$tmp/omp")"
  [ "$actual" = "$expected" ] || {
    echo "ashler comet install: checksum mismatch for official OMP $OMP_VERSION $asset" >&2
    exit 1
  }
  mkdir -p "$(dirname "$destination")"
  chmod 0755 "$tmp/omp"
  mv "$tmp/omp" "$destination"
  echo "OMP $OMP_VERSION installed and verified at $destination"
}

case "${1:-}" in
  --install-omp)
    [ "$#" -eq 1 ] || { echo "usage: install.sh --install-omp" >&2; exit 2; }
    install_omp
    exit 0
    ;;
  --help|-h)
    echo "usage: install.sh [--install-omp]"
    echo "  --install-omp  explicitly install/validate the pinned official OMP release"
    exit 0
    ;;
  "") ;;
  *) echo "usage: install.sh [--install-omp]" >&2; exit 2 ;;
esac

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
  actual="$(sha256_file "$tmp/$file")"
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
# Bootstrap missing agent CLIs. Anything already present is left alone —
# updates are handled in-app now (`omp update` etc.). Failures here must not
# abort a successful comet install, so every step is guarded.
if [ "${COMET_SKIP_AGENT_BOOTSTRAP:-0}" = 1 ]; then
  echo "note: COMET_SKIP_AGENT_BOOTSTRAP=1 — skipping agent CLI bootstrap."
else
  omp_destination="${OMP_INSTALL_PATH:-$HOME/.local/bin/omp}"
  if ! command -v omp >/dev/null 2>&1 && [ ! -e "$omp_destination" ]; then
    echo "bootstrapping OMP ${OMP_VERSION}…"
    # Subshell keeps install_omp's exit/trap out of the main install.
    ( install_omp ) \
      || echo "warn: OMP bootstrap failed — rerun later with: install.sh --install-omp" >&2
  fi

  if ! command -v claude >/dev/null 2>&1; then
    echo "bootstrapping Claude Code…"
    # Staged to a file so a failed download can't masquerade as an empty
    # script, and </dev/null so the installer can't consume our stdin when
    # this script itself arrives over a pipe.
    claude_sh="$(mktemp)"
    { curl -fsSL https://claude.ai/install.sh -o "$claude_sh" && bash "$claude_sh" </dev/null; } \
      || echo "warn: Claude Code bootstrap failed — rerun later with: curl -fsSL https://claude.ai/install.sh | bash" >&2
    rm -f "$claude_sh"
  fi

  if ! command -v codex >/dev/null 2>&1; then
    if command -v npm >/dev/null 2>&1; then
      echo "bootstrapping Codex…"
      npm install -g @openai/codex \
        || echo "warn: Codex bootstrap failed — rerun later with: npm install -g @openai/codex" >&2
    else
      echo "note: codex needs npm and was skipped — install Node.js, then: npm install -g @openai/codex"
    fi
  fi
fi

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
