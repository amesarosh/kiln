#!/usr/bin/env bash
# Build wayland-time, its settings app, task manager, launcher, lock screen, password prompt, file picker,
# file manager and Bluetooth app from this checkout, then install them system-wide.
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
PORTALS_DIR=/usr/share/xdg-desktop-portal
DBUS_DIR=/usr/share/dbus-1/services

echo "==> Building ($PROFILE)"
cargo build --profile "$PROFILE" --workspace

echo "==> Installing (needs sudo)"
sudo install -Dm755 target/$PROFILE/wayland-time "$BIN_DIR/wayland-time"
sudo install -Dm755 target/$PROFILE/wayland-time-settings "$BIN_DIR/wayland-time-settings"
sudo install -Dm755 target/$PROFILE/wayland-time-tasks "$BIN_DIR/wayland-time-tasks"
sudo install -Dm755 target/$PROFILE/wayland-time-launcher "$BIN_DIR/wayland-time-launcher"
sudo install -Dm755 target/$PROFILE/wayland-time-lock "$BIN_DIR/wayland-time-lock"
sudo install -Dm755 target/$PROFILE/wayland-time-polkit "$BIN_DIR/wayland-time-polkit"
sudo install -Dm755 target/$PROFILE/wayland-time-files "$BIN_DIR/wayland-time-files"
sudo install -Dm755 target/$PROFILE/wayland-time-file-manager "$BIN_DIR/wayland-time-file-manager"
sudo install -Dm755 target/$PROFILE/wayland-time-bluetooth "$BIN_DIR/wayland-time-bluetooth"
# The file picker is a desktop portal backend: the portal finds it through these files, and
# D-Bus starts it when an app first asks for a file.
sudo install -Dm644 resources/portal/wayland-time.portal "$PORTALS_DIR/portals/wayland-time.portal"
sudo install -Dm644 resources/portal/wayland-time-portals.conf "$PORTALS_DIR/wayland-time-portals.conf"
sudo mkdir -p "$DBUS_DIR"
printf '[D-BUS Service]\nName=org.freedesktop.impl.portal.desktop.wayland-time\nExec=%s --portal\n' \
    "$BIN_DIR/wayland-time-files" |
    sudo tee "$DBUS_DIR/org.freedesktop.impl.portal.desktop.wayland-time.service" >/dev/null
# A portal config of your own overrides the one above; make sure it picks our file picker.
USER_PORTALS="${XDG_CONFIG_HOME:-$HOME/.config}/xdg-desktop-portal/wayland-time-portals.conf"
if [ -f "$USER_PORTALS" ] && ! grep -q 'FileChooser' "$USER_PORTALS"; then
    echo 'org.freedesktop.impl.portal.FileChooser=wayland-time;gtk' >>"$USER_PORTALS"
    echo "==> Added the file picker to $USER_PORTALS"
fi
# The lock screen checks passwords with these PAM rules.
sudo install -Dm644 resources/pam.d/wayland-time-lock /etc/pam.d/wayland-time-lock
sudo install -Dm644 resources/wayland-time.desktop "$SESSIONS_DIR/wayland-time.desktop"
sudo install -Dm644 resources/wayland-time-settings.desktop "$APPS_DIR/wayland-time-settings.desktop"
sudo install -Dm644 resources/wayland-time-tasks.desktop "$APPS_DIR/wayland-time-tasks.desktop"
sudo install -Dm644 resources/wayland-time-file-manager.desktop "$APPS_DIR/wayland-time-file-manager.desktop"
sudo install -Dm644 resources/wayland-time-bluetooth.desktop "$APPS_DIR/wayland-time-bluetooth.desktop"

commit="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
dirty="$(git status --porcelain 2>/dev/null | head -1)"
echo "==> Installed $commit${dirty:+ (plus uncommitted changes)}:"
echo "    $BIN_DIR/wayland-time"
echo "    $BIN_DIR/wayland-time-settings"
echo "    $BIN_DIR/wayland-time-tasks"
echo "    $BIN_DIR/wayland-time-launcher"
echo "    $BIN_DIR/wayland-time-lock"
echo "    $BIN_DIR/wayland-time-polkit"
echo "    $BIN_DIR/wayland-time-files"
echo "    $BIN_DIR/wayland-time-file-manager"
echo "    $BIN_DIR/wayland-time-bluetooth"
echo "    $PORTALS_DIR/portals/wayland-time.portal"
echo "    $PORTALS_DIR/wayland-time-portals.conf"
echo "    $DBUS_DIR/org.freedesktop.impl.portal.desktop.wayland-time.service"
echo "    /etc/pam.d/wayland-time-lock"
echo "    $SESSIONS_DIR/wayland-time.desktop"
echo "    $APPS_DIR/wayland-time-settings.desktop"
echo "    $APPS_DIR/wayland-time-tasks.desktop"
echo "    $APPS_DIR/wayland-time-file-manager.desktop"
echo "    $APPS_DIR/wayland-time-bluetooth.desktop"
if pgrep -x wayland-time >/dev/null; then
    echo "==> wayland-time is running: log out and back in to use the new version."
fi
