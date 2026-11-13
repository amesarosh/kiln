#!/usr/bin/env bash
# Build kiln and its apps from this checkout, then install them system-wide.
#
#   ./install.sh                  build and install
#   PREFIX=/usr ./install.sh      install binaries under /usr/bin instead of /usr/local/bin
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
PORTAL_SERVICE=org.freedesktop.impl.portal.desktop.kiln.service

# Every program the workspace builds.
BINARIES=(
    kiln
    kiln-settings
    kiln-tasks
    kiln-launcher
    kiln-lock
    kiln-polkit
    kiln-files
    kiln-file-manager
    kiln-bluetooth
    kiln-sound
    kiln-image-viewer
)
# Launcher entries, and the default apps for a kiln session (images open in our viewer).
APP_FILES=(
    kiln-settings.desktop
    kiln-tasks.desktop
    kiln-file-manager.desktop
    kiln-bluetooth.desktop
    kiln-sound.desktop
    kiln-image-viewer.desktop
    kiln-mimeapps.list
)

installed=()
# install_file MODE SOURCE DESTINATION
install_file() {
    sudo install -D -m "$1" "$2" "$3"
    installed+=("$3")
}

echo "==> Building ($PROFILE)"
cargo build --profile "$PROFILE" --workspace

echo "==> Installing (needs sudo)"
for bin in "${BINARIES[@]}"; do
    install_file 755 "target/$PROFILE/$bin" "$BIN_DIR/$bin"
done
# The file picker is a desktop portal backend: the portal finds it through these files, and
# D-Bus starts it when an app first asks for a file.
install_file 644 resources/portal/kiln.portal "$PORTALS_DIR/portals/kiln.portal"
install_file 644 resources/portal/kiln-portals.conf "$PORTALS_DIR/kiln-portals.conf"
sudo mkdir -p "$DBUS_DIR"
printf '[D-BUS Service]\nName=org.freedesktop.impl.portal.desktop.kiln\nExec=%s --portal\n' \
    "$BIN_DIR/kiln-files" |
    sudo tee "$DBUS_DIR/$PORTAL_SERVICE" >/dev/null
installed+=("$DBUS_DIR/$PORTAL_SERVICE")
# A portal config of your own overrides the one above; make sure it picks our file picker.
USER_PORTALS="${XDG_CONFIG_HOME:-$HOME/.config}/xdg-desktop-portal/kiln-portals.conf"
if [ -f "$USER_PORTALS" ] && ! grep -q 'FileChooser' "$USER_PORTALS"; then
    echo 'org.freedesktop.impl.portal.FileChooser=kiln;gtk' >>"$USER_PORTALS"
    echo "==> Added the file picker to $USER_PORTALS"
fi
# The lock screen checks passwords with these PAM rules.
install_file 644 resources/pam.d/kiln-lock /etc/pam.d/kiln-lock
install_file 644 resources/kiln.desktop "$SESSIONS_DIR/kiln.desktop"
for file in "${APP_FILES[@]}"; do
    install_file 644 "resources/$file" "$APPS_DIR/$file"
done

commit="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
dirty="$(git status --porcelain 2>/dev/null | head -1)"
echo "==> Installed $commit${dirty:+ (plus uncommitted changes)}:"
printf '    %s\n' "${installed[@]}"
if pgrep -x kiln >/dev/null; then
    echo "==> kiln is running: log out and back in to use the new version."
fi
