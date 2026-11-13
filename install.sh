#!/usr/bin/env bash
# Build kiln, its settings app, task manager, launcher, lock screen, password prompt, file picker,
# file manager, Bluetooth and Sound apps and image viewer from this checkout, then install them system-wide.
#
#   ./install.sh            build and install
#   PREFIX=/usr ./install.sh  install binaries under /usr/bin instead of /usr/local/bin
#   PROFILE=release ./install.sh  fully optimized build (slower to build) instead of the fast profile
#
# The build runs as you; only the copy into system folders uses sudo. Installing while
# kiln is running is safe, but the new version only takes effect after you log
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
sudo install -Dm755 target/$PROFILE/kiln "$BIN_DIR/kiln"
sudo install -Dm755 target/$PROFILE/kiln-settings "$BIN_DIR/kiln-settings"
sudo install -Dm755 target/$PROFILE/kiln-tasks "$BIN_DIR/kiln-tasks"
sudo install -Dm755 target/$PROFILE/kiln-launcher "$BIN_DIR/kiln-launcher"
sudo install -Dm755 target/$PROFILE/kiln-lock "$BIN_DIR/kiln-lock"
sudo install -Dm755 target/$PROFILE/kiln-polkit "$BIN_DIR/kiln-polkit"
sudo install -Dm755 target/$PROFILE/kiln-files "$BIN_DIR/kiln-files"
sudo install -Dm755 target/$PROFILE/kiln-file-manager "$BIN_DIR/kiln-file-manager"
sudo install -Dm755 target/$PROFILE/kiln-bluetooth "$BIN_DIR/kiln-bluetooth"
sudo install -Dm755 target/$PROFILE/kiln-sound "$BIN_DIR/kiln-sound"
sudo install -Dm755 target/$PROFILE/kiln-image-viewer "$BIN_DIR/kiln-image-viewer"
# The file picker is a desktop portal backend: the portal finds it through these files, and
# D-Bus starts it when an app first asks for a file.
sudo install -Dm644 resources/portal/kiln.portal "$PORTALS_DIR/portals/kiln.portal"
sudo install -Dm644 resources/portal/kiln-portals.conf "$PORTALS_DIR/kiln-portals.conf"
sudo mkdir -p "$DBUS_DIR"
printf '[D-BUS Service]\nName=org.freedesktop.impl.portal.desktop.kiln\nExec=%s --portal\n' \
    "$BIN_DIR/kiln-files" |
    sudo tee "$DBUS_DIR/org.freedesktop.impl.portal.desktop.kiln.service" >/dev/null
# A portal config of your own overrides the one above; make sure it picks our file picker.
USER_PORTALS="${XDG_CONFIG_HOME:-$HOME/.config}/xdg-desktop-portal/kiln-portals.conf"
if [ -f "$USER_PORTALS" ] && ! grep -q 'FileChooser' "$USER_PORTALS"; then
    echo 'org.freedesktop.impl.portal.FileChooser=kiln;gtk' >>"$USER_PORTALS"
    echo "==> Added the file picker to $USER_PORTALS"
fi
# The lock screen checks passwords with these PAM rules.
sudo install -Dm644 resources/pam.d/kiln-lock /etc/pam.d/kiln-lock
sudo install -Dm644 resources/kiln.desktop "$SESSIONS_DIR/kiln.desktop"
sudo install -Dm644 resources/kiln-settings.desktop "$APPS_DIR/kiln-settings.desktop"
sudo install -Dm644 resources/kiln-tasks.desktop "$APPS_DIR/kiln-tasks.desktop"
sudo install -Dm644 resources/kiln-file-manager.desktop "$APPS_DIR/kiln-file-manager.desktop"
sudo install -Dm644 resources/kiln-bluetooth.desktop "$APPS_DIR/kiln-bluetooth.desktop"
sudo install -Dm644 resources/kiln-sound.desktop "$APPS_DIR/kiln-sound.desktop"
sudo install -Dm644 resources/kiln-image-viewer.desktop "$APPS_DIR/kiln-image-viewer.desktop"
# Default apps for a kiln session (images open in our viewer).
sudo install -Dm644 resources/kiln-mimeapps.list "$APPS_DIR/kiln-mimeapps.list"

commit="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
dirty="$(git status --porcelain 2>/dev/null | head -1)"
echo "==> Installed $commit${dirty:+ (plus uncommitted changes)}:"
echo "    $BIN_DIR/kiln"
echo "    $BIN_DIR/kiln-settings"
echo "    $BIN_DIR/kiln-tasks"
echo "    $BIN_DIR/kiln-launcher"
echo "    $BIN_DIR/kiln-lock"
echo "    $BIN_DIR/kiln-polkit"
echo "    $BIN_DIR/kiln-files"
echo "    $BIN_DIR/kiln-file-manager"
echo "    $BIN_DIR/kiln-bluetooth"
echo "    $BIN_DIR/kiln-sound"
echo "    $BIN_DIR/kiln-image-viewer"
echo "    $PORTALS_DIR/portals/kiln.portal"
echo "    $PORTALS_DIR/kiln-portals.conf"
echo "    $DBUS_DIR/org.freedesktop.impl.portal.desktop.kiln.service"
echo "    /etc/pam.d/kiln-lock"
echo "    $SESSIONS_DIR/kiln.desktop"
echo "    $APPS_DIR/kiln-settings.desktop"
echo "    $APPS_DIR/kiln-tasks.desktop"
echo "    $APPS_DIR/kiln-file-manager.desktop"
echo "    $APPS_DIR/kiln-bluetooth.desktop"
echo "    $APPS_DIR/kiln-sound.desktop"
echo "    $APPS_DIR/kiln-image-viewer.desktop"
echo "    $APPS_DIR/kiln-mimeapps.list"
if pgrep -x kiln >/dev/null; then
    echo "==> kiln is running: log out and back in to use the new version."
fi
