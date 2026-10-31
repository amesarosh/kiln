#!/usr/bin/env bash
# Build wayland-time, its settings app, task manager, launcher, lock screen and password prompt from this checkout,
# then install them
# system-wide.
#
#   ./install.sh            build and install
#   PREFIX=/usr ./install.sh  install binaries under /usr/bin instead of /usr/local/bin
#   PROFILE=release ./install.sh  fully optimized build (slower to build) instead of the fast profile
#
# The build runs as you; only the copy into system folders uses sudo. Installing while
# wayland-time is running is safe, but the new version only takes effect after you log
# out and back in (the settings app: after you reopen it).
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")"

# fast rebuilds in seconds; release takes ~1.5 minutes but runs a little faster.
PROFILE="${PROFILE:-fast}"
PREFIX="${PREFIX:-/usr/local}"
BIN_DIR="$PREFIX/bin"
SESSIONS_DIR=/usr/share/wayland-sessions
APPS_DIR=/usr/share/applications

echo "==> Building ($PROFILE)"
cargo build --profile "$PROFILE" --workspace

echo "==> Installing (needs sudo)"
sudo install -Dm755 target/$PROFILE/wayland-time "$BIN_DIR/wayland-time"
sudo install -Dm755 target/$PROFILE/wayland-time-settings "$BIN_DIR/wayland-time-settings"
sudo install -Dm755 target/$PROFILE/wayland-time-tasks "$BIN_DIR/wayland-time-tasks"
sudo install -Dm755 target/$PROFILE/wayland-time-launcher "$BIN_DIR/wayland-time-launcher"
sudo install -Dm755 target/$PROFILE/wayland-time-lock "$BIN_DIR/wayland-time-lock"
sudo install -Dm755 target/$PROFILE/wayland-time-polkit "$BIN_DIR/wayland-time-polkit"
# The lock screen checks passwords with these PAM rules.
sudo install -Dm644 resources/pam.d/wayland-time-lock /etc/pam.d/wayland-time-lock
sudo install -Dm644 resources/wayland-time.desktop "$SESSIONS_DIR/wayland-time.desktop"
sudo install -Dm644 resources/wayland-time-settings.desktop "$APPS_DIR/wayland-time-settings.desktop"
sudo install -Dm644 resources/wayland-time-tasks.desktop "$APPS_DIR/wayland-time-tasks.desktop"

commit="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
dirty="$(git status --porcelain 2>/dev/null | head -1)"
echo "==> Installed $commit${dirty:+ (plus uncommitted changes)}:"
echo "    $BIN_DIR/wayland-time"
echo "    $BIN_DIR/wayland-time-settings"
echo "    $BIN_DIR/wayland-time-tasks"
echo "    $BIN_DIR/wayland-time-launcher"
echo "    $BIN_DIR/wayland-time-lock"
echo "    $BIN_DIR/wayland-time-polkit"
echo "    /etc/pam.d/wayland-time-lock"
echo "    $SESSIONS_DIR/wayland-time.desktop"
echo "    $APPS_DIR/wayland-time-settings.desktop"
echo "    $APPS_DIR/wayland-time-tasks.desktop"
if pgrep -x wayland-time >/dev/null; then
    echo "==> wayland-time is running: log out and back in to use the new version."
fi
