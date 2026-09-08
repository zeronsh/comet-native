#!/bin/sh
# Zeron (native) headless installer.
#
#   curl -fsSL https://zeron.sh/install.sh | sh
#
# Installs the native binary to ~/.zeron/app, puts `zeron` on PATH, and runs it
# as a local-only systemd user service that survives reboots. Signing in is
# optional and enables sync after a restart. Re-running upgrades in place;
# ~/.zeron state is preserved.
#
# The binary ships with production endpoints baked in: no ZERON_EDGE_URL or
# client-id configuration needed. Overrides (if any) go in ~/.zeron/env.
#
# It does need a couple of system libraries (libxkbcommon-x11, libxcb) that
# minimal server images omit — see the preflight check below.
set -eu

BASE="${ZERON_BASE_URL:-https://zeron.sh}"

# --- platform ---------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) plat=linux ;;
  Darwin)
    echo "zeron install: on macOS, download the desktop app instead:" >&2
    echo "  $BASE/releases/latest.txt → $BASE/releases/zeron-<version>-macos-arm64.dmg" >&2
    exit 1
    ;;
  *)
    echo "zeron install: unsupported OS '$os' — only Linux for now." >&2
    exit 1
    ;;
esac
case "$arch" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *)
    echo "zeron install: unsupported architecture '$arch'." >&2
    exit 1
    ;;
esac

# --- download ----------------------------------------------------------------
ver="$(curl -fsSL "$BASE/releases/latest.txt" | tr -d '[:space:]')"
[ -n "$ver" ] || { echo "zeron install: could not resolve latest version" >&2; exit 1; }
file="zeron-$ver-$plat-$arch.tar.gz"
data_root="$HOME/.zeron"
app_root="$data_root/app"
dest="$app_root/$ver"

if [ -x "$dest/zeron" ]; then
  echo "zeron $ver already downloaded — relinking."
else
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  echo "downloading zeron $ver ($plat-$arch)…"
  curl -fSL --progress-bar "$BASE/releases/$file" -o "$tmp/$file"
  mkdir -p "$dest"
  tar -xzf "$tmp/$file" -C "$dest" --strip-components=1
fi

# --- preflight: system libraries ---------------------------------------------
# The binary is not fully self-contained: gpui links libxkbcommon-x11 and libxcb
# as hard DT_NEEDED entries, so *every* subcommand fails to load without them --
# including `zeron headless`, `login` and `status`, none of which open a window.
# Minimal server and cloud images generally don't ship them. Check here, before
# systemd is pointed at the binary, so the failure names the missing package
# instead of surfacing as a restart loop in `systemctl --user status zeron`.
#
# This runs before `current` is repointed below: on an upgrade, a binary that
# cannot load must not displace the version that is running fine.
if command -v ldd >/dev/null 2>&1; then
  ldd_out="$(ldd "$dest/zeron" 2>&1 || true)"

  # glibc older than the one the release was built against: the loader reports a
  # version it cannot satisfy, and nothing the user installs can fix it.
  glibc_want="$(printf '%s\n' "$ldd_out" \
    | sed -n "s/.*version .\(GLIBC_[0-9.]*\). not found.*/\1/p" | head -1)"
  if [ -n "$glibc_want" ]; then
    echo "" >&2
    echo "zeron install: this build requires $glibc_want, newer than the glibc on" >&2
    echo "  this system ($(ldd --version 2>/dev/null | head -1))." >&2
    echo "" >&2
    echo "  The published binary is too new for this distro -- this is a packaging" >&2
    echo "  bug, not something you can install your way out of. Please report it:" >&2
    echo "  https://github.com/zeronsh/comet/issues" >&2
    exit 1
  fi

  missing="$(printf '%s\n' "$ldd_out" | awk '/=> not found/ { print $1 }' | sort -u)"
  if [ -n "$missing" ]; then
    if command -v apt-get >/dev/null 2>&1; then
      hint="sudo apt-get install -y libxkbcommon-x11-0 libxcb1"
    elif command -v dnf >/dev/null 2>&1; then
      hint="sudo dnf install -y libxkbcommon-x11 libxcb"
    elif command -v pacman >/dev/null 2>&1; then
      hint="sudo pacman -S --needed libxkbcommon-x11 libxcb"
    elif command -v zypper >/dev/null 2>&1; then
      hint="sudo zypper install -y libxkbcommon-x11-0 libxcb1"
    elif command -v apk >/dev/null 2>&1; then
      hint="sudo apk add libxkbcommon libxcb"
    else
      hint=""
    fi
    echo "" >&2
    echo "zeron install: missing system libraries:" >&2
    printf '  %s\n' $missing >&2
    if [ -n "$hint" ]; then
      echo "" >&2
      echo "  install them with:" >&2
      echo "    $hint" >&2
      echo "" >&2
      echo "  then re-run this installer." >&2
    else
      echo "" >&2
      echo "  install the packages providing them, then re-run this installer." >&2
    fi
    exit 1
  fi
fi

ln -sfn "$dest" "$app_root/current"
mkdir -p "$HOME/.local/bin"
ln -sf "$app_root/current/zeron" "$HOME/.local/bin/zeron"

# --- service -----------------------------------------------------------------
# The daemon is useful before auth: without a saved session it serves the local
# profile. Login only changes which profile the next daemon start selects.

service=manual
if command -v systemctl >/dev/null 2>&1 && [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  mkdir -p "$HOME/.config/systemd/user"
  cat >"$HOME/.config/systemd/user/zeron.service" <<'UNIT'
[Unit]
Description=Zeron native headless engine
After=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
ExecStart=%h/.zeron/app/current/zeron headless
Restart=on-failure
RestartSec=5
EnvironmentFile=-%h/.zeron/env

[Install]
WantedBy=default.target
UNIT
  systemctl --user daemon-reload
  systemctl --user enable zeron
  systemctl --user restart zeron
  service=running
  # Keep the user manager (and the engine) running without an active login.
  loginctl enable-linger "$USER" 2>/dev/null \
    || sudo -n loginctl enable-linger "$USER" 2>/dev/null \
    || echo "warn: could not enable linger — the engine stops when you log out (run: sudo loginctl enable-linger $USER)"
else
  echo "warn: systemd user session not available — run the engine manually with: zeron headless"
fi

# --- agent CLIs ---------------------------------------------------------------
command -v claude >/dev/null 2>&1 || \
  echo "note: Claude Code CLI not found — install it with: curl -fsSL https://claude.ai/install.sh | bash"

case ":$PATH:" in
  *":$HOME/.local/bin:"*) path_hint="" ;;
  *) path_hint=' (add ~/.local/bin to your PATH)' ;;
esac

echo ""
echo "✓ zeron $ver installed$path_hint"
echo ""
case "$service" in
  running)
    echo "the engine is running with the new version (local-only unless sync is enabled)."
    echo "  systemctl --user status zeron    check the service"
    echo ""
    echo "optional sync (local sessions stay local):"
    echo "  systemctl --user stop zeron"
    echo "  zeron login"
    echo "  systemctl --user restart zeron"
    ;;
  manual)
    echo "next: run the local-only engine with \`zeron headless\`."
    echo "optional sync: run \`zeron login\` before starting the engine."
    ;;
esac
