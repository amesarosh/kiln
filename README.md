# wayland-time

A lightweight, fast, configurable tiling Wayland compositor written in Rust, built on [Smithay](https://github.com/Smithay/smithay).

- **Dynamic tiling** (master/stack side by side or stacked vertically, and monocle), toggleable floating, fullscreen, 1–32 workspaces
- **Multiple monitors**, each with its own set of workspaces (like dwm), placed left to right
- **Built-in clickable bar** per monitor (top, bottom, left or right): workspaces (with optional names and icons), one tab per window with its app icon, status widgets (tray icons, what's playing, network, battery, brightness, a notifications bell), layout switcher, a settings button and a power button (lock, suspend, log out, restart, shut down)
- **Overview** (`Mod+Tab`): every workspace of every monitor as live thumbnails; click a window or workspace to go there, drag windows between workspaces
- **App launcher** (`wayland-time-launcher`): fuzzy search over installed apps, with the ones you use most first
- **Desktop notifications** built in: pop-ups with actions, do not disturb, and a list of missed ones behind the bell (or leave it to mako/dunst)
- **Animations**: workspaces slide or fade, tiles glide into place; touchpad swipes change workspace and open the overview
- **One TOML file** for keybinds, gaps, borders, colors, keyboard layout and window rules, **reloaded on save**
- **Settings app** (`wayland-time-settings`) that edits that file for you, with every change applied as you make it
- **Task manager** (`wayland-time-tasks`, `Ctrl+Shift+Escape`): the running processes, to search, sort and end, and graphs of CPU, memory, disk and network use
- **Idle costs nothing:** frames are only drawn when something changes, and only damaged regions are repainted
- Works with standard desktop tools: bars (waybar), launchers (fuzzel, rofi, wofi), wallpaper tools (swaybg) via `wlr-layer-shell`
- Runs nested for development or **on real hardware from a TTY**, with hardware cursor planes, direct scanout and GPU buffer sharing (linux-dmabuf)
- Server-side decorations requested by default, so apps don't draw title bars; clipboard and primary selection supported, including clipboard managers (cliphist, `wl-paste --watch`); what you copy stays available after the app you copied from closes (`keep_clipboard`)
- **X11 apps through Xwayland** (Steam, games, the VS Code snap, older programs): tiled and focused like any window, with copy and paste working between X11 and Wayland apps
- **Window list for other programs**: taskbars (waybar's `wlr/taskbar`), window switchers and scripts (`wlrctl`, `lswt`) can list, focus, fullscreen and close windows
- **Screenshots and recording** (grim, slurp, wf-recorder) and screen sharing through xdg-desktop-portal-wlr; recorders and screen sharing can take frames as GPU buffers, so nothing is copied through the CPU
- **HDR** per monitor (`hdr = true` under `[[outputs]]`, or the settings app): HDR10 signalling with 10-bit output, ordinary windows at an adjustable SDR brightness, and HDR video and games shown in HDR through the color-management protocol (also tone-mapped onto SDR monitors)
- **Games**: fullscreen games that ask can skip vsync for lower input lag (tearing control), and virtual machines, remote desktops and games can take every key, binds included (`Mod+Ctrl+Escape` takes them back)
- **Sessions come back**: apps open when you quit, log out or shut down are started again at the next login, with each window on the same monitor and workspace, in the same tiling order, size and floating position
- **Wallpaper** built in (`[theme] wallpaper`, or one per monitor): fill, fit, stretch, center or tile, picked in the settings app
- **File picker** (`wayland-time-files`) in the theme's look, given to apps through the desktop portal (Vivaldi, Firefox, Chrome, Flatpaks...): places, path bar, search, filters, open/save/folders. Dialogs always float, centered over the window that opened them, sized to fit the screen
- **File manager** (`wayland-time-file-manager`, "Files" in the launcher) in the theme's look, built on the file picker: open files in their default app or pick one, copy, cut and paste (in the background, with progress), rename, new folders, a trash that works with other file managers', search, drag and drop with other apps, folders added to the sidebar, and live updates when files change
- **Bluetooth** (`wayland-time-bluetooth`, and a bar widget: click to open it, right-click to turn Bluetooth on or off): pair (with passkey and PIN prompts), connect, disconnect and forget devices, with battery levels
- **Sound** (`wayland-time-sound`, and a bar widget showing the volume: click to open it, right-click to mute, scroll to change it): pick the output and microphone, set their volumes, set or mute each app's volume, send an app to another output, and choose sound card profiles
- **Password prompt for admin rights** (`wayland-time-polkit`, a polkit agent in the theme's look), started at login, so installing software, mounting disks and `pkexec` can ask for your password
- **Screen locking** with its own lock screen (`wayland-time-lock`, in the theme's colors and font) or any standard locker (swaylock, hyprlock), idle handling (swayidle) and monitors turning off when idle, with video players able to keep the screen awake
- **Monitor tools** (kanshi, wdisplays, wlr-randr) can change modes, scale and arrangement live, and the settings app has a monitor map you drag; frame timing for video players and games (presentation-time)
- **Scripting**: `wayland-time msg "<action>"` runs any action from a shell

## Status

- **Nested** inside an existing X11 or Wayland session (development setup): working.
- **TTY / real hardware** (DRM/KMS, libinput, libseat): implemented, needs testing on more machines.
  Single GPU only. Mode, scale, position, rotation and HDR are set per monitor with `[[outputs]]`.

`vendor/smithay` is Smithay 0.7.0 with a few small additions (tearing page flips, per-monitor buffer formats); see `vendor/smithay/PATCHES.md`.

## Build and run

```sh
cargo build --release        # the compositor and its apps
./target/release/wayland-time -e alacritty
```

On first start it writes a fully commented default config to `~/.config/wayland-time/config.toml`.

```
-c, --config <PATH>    use a different config file
-e, --exec <CMD>       run a command once ready (repeatable)
-b, --backend <NAME>   auto (default), winit, tty or headless (no screen, for tests)
-o, --output <NAME>    tty only: light up just this monitor, e.g. DP-4 (repeatable)
    --check-config     validate the config and exit

wayland-time msg <ACTION>   run an action in the running compositor, e.g. "workspace 2"
```

Logging uses `RUST_LOG`, e.g. `RUST_LOG=debug`.

### Running from a TTY

Build dependencies: `libinput-dev`, `libseat-dev`, `libgbm-dev`, `libdrm-dev`, `libudev-dev`, `libxkbcommon-dev`, `libpam0g-dev` (for the lock screen).
For X11 apps, install `xwayland` (turn it off with `xwayland = false` under `[general]`).

1. Switch to a free text console with `Ctrl+Alt+F3` and log in.
2. For a first test, use a timeout so it exits on its own even if input doesn't work:
   `timeout 60 ./target/release/wayland-time -e alacritty`
3. Quit with `Super+Shift+e`. `Ctrl+Alt+F1..F12` switch consoles at any time.

When run from a TTY, logs go to `~/.local/state/wayland-time/wayland-time.log` (previous run: `.log.old`).

To pick it from a login screen (GDM, SDDM…), install the binary and session file.
`./install.sh` builds everything and does all of this for you (it asks for sudo); by hand:

```sh
sudo install -m755 target/release/wayland-time /usr/local/bin/
sudo install -m644 resources/wayland-time.desktop /usr/share/wayland-sessions/
# The settings app, and its launcher entry:
sudo install -m755 target/release/wayland-time-settings /usr/local/bin/
sudo install -m644 resources/wayland-time-settings.desktop /usr/share/applications/
# The task manager, and its launcher entry:
sudo install -m755 target/release/wayland-time-tasks /usr/local/bin/
sudo install -m644 resources/wayland-time-tasks.desktop /usr/share/applications/
# The app launcher:
sudo install -m755 target/release/wayland-time-launcher /usr/local/bin/
# The lock screen, and the PAM rules it checks passwords with:
sudo install -m755 target/release/wayland-time-lock /usr/local/bin/
# The password prompt for apps that need admin rights:
sudo install -m755 target/release/wayland-time-polkit /usr/local/bin/
sudo install -m644 resources/pam.d/wayland-time-lock /etc/pam.d/
# The Bluetooth app, and its launcher entry:
sudo install -m755 target/release/wayland-time-bluetooth /usr/local/bin/
sudo install -m644 resources/wayland-time-bluetooth.desktop /usr/share/applications/
# The Sound app, and its launcher entry:
sudo install -m755 target/release/wayland-time-sound /usr/local/bin/
sudo install -m644 resources/wayland-time-sound.desktop /usr/share/applications/
```

## Default keys

`Mod` is **Alt when nested** (so the host desktop doesn't steal Super) and **Super on hardware**. Set `general.mod` to override. A bare `"Mod"` bind fires when you tap Mod by itself.
`Ctrl+Alt+F1..F12` always switch consoles on hardware.

| Keys | Action |
|---|---|
| `Mod+Return` | terminal (alacritty) |
| `Mod+d` | app launcher: type, then Enter (text matching no app runs as a command) |
| `Mod+Tab` | overview: every workspace at once (arrows and Enter, 1-9, or click; Escape goes back) |
| `Mod+j` / `Mod+k` | focus next / previous |
| `Mod+Shift+j` / `Mod+Shift+k` | move window down / up the stack |
| `Mod+Shift+Return` | swap with master |
| `Mod+h` / `Mod+l` | shrink / grow master |
| `Mod+i` / `Mod+o` | more / fewer master windows |
| `Mod+t` / `Mod+Shift+t` / `Mod+m` / `Mod+space` | tile / vertical (master on top) / monocle / next layout |
| `Mod+Shift+space` | toggle floating |
| `Mod+f` | toggle fullscreen |
| `Mod+1..9` / `Mod+Shift+1..9` | go to / send to workspace (on the focused monitor) |
| `Mod+.` / `Mod+,` | focus next / previous monitor |
| `Mod+Shift+.` / `Mod+Shift+,` | send window to next / previous monitor |
| `Mod+Shift+q` | close window |
| `Mod+Shift+r` | reload config |
| `Mod+Escape` | lock the screen |
| `Pause` / `XF86AudioPlay` / `XF86AudioNext` / `XF86AudioPrev` | play/pause / next / previous track in the media player (Spotify, browsers, mpv) |
| `Print` / `Shift+Print` | screenshot a region to the clipboard / all monitors to a file (grim, slurp, wl-copy) |
| `Alt+Tab` / `Alt+Shift+Tab` | window switcher: all windows, most recent first; let go of Alt to switch (Escape cancels) |
| `Mod+v` | pick from clipboard history (cliphist and fuzzel; start `wl-paste --watch cliphist store` in autostart) |
| `Mod+Shift+e` | quit |
| `Mod+left drag` | move: a tiled window swaps places with the tile under the pointer and stays tiled (drag it to the top or bottom edge of a tile in the other column to stack it above or below that tile), a floating one moves freely; drop on another monitor to move it there |
| `Mod+right drag` | resize: a tiled window moves the borders it shares with its neighbours, a floating one changes size |
| `Mod+middle click` | float or re-tile the window under the pointer |
| 3 or 4 finger swipe | left / right: next / previous workspace; up: overview; down: close it (`[input] gestures`) |

## Configuration

### Monitors and the bar

Each monitor has its own workspaces 1–9. Workspace keys, new windows and the launcher act on the
focused monitor, which follows the mouse (with `focus_follows_mouse`) or the last click.
When a monitor is unplugged, its windows move to the same-numbered workspaces of the focused one.

Each monitor also gets the built-in bar (`[bar]` in the config). Click a workspace or tab to switch
to it, middle-click a tab to close that window, right-click a tab for its window menu (float,
fullscreen, move to a workspace or monitor, close), click the layout name to cycle layouts, and scroll
over the bar or empty desktop to change workspace. It works alongside waybar; set
`bar.enabled = false` to use waybar alone.

### Monitor settings

Each `[[outputs]]` entry sets up one monitor by its connector name. The log shows the names and
modes of connected monitors.

```toml
[[outputs]]
name = "DP-1"
mode = "2560x1440@144"
scale = 1.5
position = [0, 0]

[[outputs]]
name = "HDMI-A-1"
position = [1707, 0]   # right of DP-1: 2560 / 1.5 ≈ 1707
transform = "90"       # rotated to portrait

[[outputs]]
name = "eDP-1"
enabled = false
```

Positions are in scaled pixels, so a 2560-wide monitor at scale 1.5 is 1707 wide. Monitors
without a `position` are placed left to right after the ones that have one. Apps that support
fractional scaling draw at the exact scale. Others draw at the scale rounded up and the
compositor shrinks them. Nested, only
`scale` applies (the fake monitors are named `winit` or `winit-1`, `winit-2`…).

### Config file

Any section or key you leave out uses the default, so a config can be as small as:

```toml
[general]
autostart = ["waybar", "swaybg -i ~/wall.png"]

[layout]
gaps_inner = 10
smart_gaps = true

[theme.colors]
accent = "#f38ba8"   # focused border, active workspace, tab underline...

[[rules]]
app_id = "pavucontrol"
floating = true

[[rules]]
app_id = "firefox"
output = "DP-5"   # monitor names are in the log
workspace = 2
```

The wallpaper is `[theme] wallpaper = "~/Pictures/x.jpg"` (PNG, JPEG or WebP), fitted with
`wallpaper_fit`: `fill` (cover the monitor, cropping the edges), `fit` (all of it, with the
`base` color around it), `stretch`, `center` or `tile`. A monitor can have its own with
`wallpaper` under its `[[outputs]]` entry (`""` for none). Pictures are scaled once per monitor
on a background thread, so drawing them costs nothing; saving the config or changing a
monitor's mode remakes them. Wallpaper tools like swaybg still work and draw on top.

Everything the compositor draws (the bar, window borders, the lock screen) takes its font and
colors from `[theme]`. Colors are named once in `[theme.colors]` and referred to by name, and
shades like `hover = "mix(surface, text, 0.1)"` or `drag_box = "alpha(accent, 0.18)"` (a
see-through accent) are built from them, so changing `surface` or `accent` updates every part
that uses it. A section can override colors for itself alone with
its own table, e.g. `[bar.colors]`.

Themes can be saved and switched. A saved theme is a file in `~/.config/wayland-time/themes/`
holding the same keys as `[theme]` (`font`, `font_size`, `radius`, `icon_theme`, `wallpaper` and a `[colors]`
table), and `[theme] name = "Nord"` uses `themes/Nord.toml`. Its values go on top of the built-in
theme, and anything else set in `[theme]` goes on top of the saved theme. Editing the theme file
in use reloads it like the config. The settings app's Appearance page can pick a theme, save the
current look as a new one, and delete one.

If you define `[binds]`, it replaces the default binds entirely. See [`resources/default-config.toml`](resources/default-config.toml) for every option and action.
A config with errors is rejected as a whole and the last working config stays active; all problems are logged at once.

### Bar widgets, notifications and the tray

`[bar] widgets` picks the status widgets and their order. They sit next to the clock (on
every bar when no connected monitor has the clock):

- **tray**: icons apps put in the tray (StatusNotifierItem: Steam, Discord, nm-applet...).
  Click to open the app, right-click for its menu, middle-click for its second action.
- **media**: what's playing in any MPRIS player. Click to pause or play, middle- or right-click
  for the next track, scroll to skip back and forward.
- **network**, **battery** and **brightness** (scroll to change it, through logind). Battery
  and brightness only show on machines that have them.
- **notifications**: a bell with a dot for unread notifications. Click for the list,
  right-click for do not disturb, middle-click to clear them all.

Notifications pop up in a corner (`[notifications]`). Click one to open it, click its buttons
for their actions, right-click to dismiss it. With another notification daemon running (mako,
dunst) the built-in one stays out of the way; `enabled = false` turns it off for good.

### Launcher, overview and animations

`wayland-time-launcher` (the `launcher` action) lists installed apps from their `.desktop`
files. Type to search names, keywords and descriptions; apps you start often come first. It
opens floating in the middle of the focused monitor and closes when you press Escape or click
elsewhere.

The `overview` action (`Mod+Tab`, or swipe up with three fingers) zooms out to every workspace
of every monitor. Click a window to go to it, click a workspace to switch to it, or drag a
window onto another workspace (on any monitor) to move it there.

```toml
[animations]
enabled = true
workspace = "slide"   # "slide", "fade" or "none"
duration = 220        # milliseconds
```

### Scripting

`wayland-time msg <action>` runs any action from `[binds]` in the running compositor, e.g.
`wayland-time msg "workspace 3"` or `wayland-time msg overview`. It talks to a socket in
`$XDG_RUNTIME_DIR` (programs the compositor starts get its path in `$WAYLAND_TIME_SOCKET`).

### Task manager

`wayland-time-tasks` (also `Ctrl+Shift+Escape`) lists your processes with their CPU and memory
use; "All users and system" adds other users' processes and kernel threads. Click a column to
sort by it, type to search by name, command line or PID, and select a process to end it ("End
process" asks it to quit, like closing it; "Force quit" stops it at once). Delete ends the
selected process; right-click for more. Ending the desktop itself, Xwayland or the system's first
process asks first. The Performance page graphs the last minute of CPU (and each core), memory and
swap, disk and network use. CPU is a share of the whole machine, so all processes add up to 100%.

### Settings app

`wayland-time-settings` edits the config file with a window instead of a text editor: general
behavior, appearance, animations and every theme color, tiling, gaps and workspace names, the
bar and its widgets, notifications, each monitor (a map to drag them into place, plus mode,
scale, rotation, bar and layout, picked from the monitors that are connected), keyboard layout
and repeat, mouse and touchpad, keybinds, window rules and sessions. A search box finds
settings by name.

Changes apply as you make them: shortly after each edit the file is checked with the
compositor's own config loader and saved, and the compositor reloads it. A change that doesn't
load (a color typo, a keyboard layout that doesn't exist) is shown in the window and isn't saved,
so the desktop never picks up a broken config. Comments and layout in your file are kept, and
edits made in a text editor at the same time are picked up. The few settings that only apply at
the next login (autostart, Xwayland, session restore) are marked as such. The app follows
`[theme]` itself, so color and font changes show up in it right away.

```
-c, --config <PATH>   edit a different config file
-p, --page <NAME>     open at a page, e.g. monitors or keybinds
```

### Sessions

Open windows are saved to `~/.local/state/wayland-time/session.toml` a few seconds after anything
changes, and right away when you quit. At the next start each app is launched again (from its
`.desktop` file, or the command line it ran with) and its windows go back where they were, along
with each workspace's layout and the workspace each monitor was showing. A change is only saved
once it has lasted a few seconds, so apps closing during shutdown aren't remembered as closed.

An app with several windows in one process (a browser) is started once and reopens its windows
itself. Windows an app opened on its own (a game from Steam, a viewer from a terminal) aren't
started separately. Apps in `autostart` are left to autostart, but their windows are still put
back. Terminals come back as fresh shells.

```toml
[session]
restore = true                  # false: don't save or restore
exclude = ["steam_app_*", "pavucontrol"]   # never saved or started again
```

Nested runs use `session-nested.toml`, so testing never touches the real session.

### Screen lock and idle

`wayland-time-lock` is the built-in lock screen: the time, the date, your name and a password
field, drawn in the theme's colors and font. It checks your password with PAM, like logging in
(`install.sh` adds `/etc/pam.d/wayland-time-lock` for this; without it, the `login` rules are
used). Escape clears what you typed. `Mod+Escape` runs the `lock` action, which starts the
locker set in `[general] locker`, or else the first one installed of `wayland-time-lock`,
hyprlock, swaylock, gtklock and waylock. Any locker that uses the standard lock protocol works.

To lock after a while and before suspend, use `swayidle` (`-f` returns once the screen is
locked, so the machine doesn't sleep with the desktop showing):

```toml
[general]
autostart = ["swayidle -w timeout 300 'wayland-time-lock -f' before-sleep 'wayland-time-lock -f'"]
```

While locked, only the locker is drawn and every key goes to it. No binds work, including
Ctrl+Alt+Backspace (quitting would drop you into the shell that started the compositor), but
Ctrl+Alt+F1..F12 still switch VTs. If the locker crashes, the screen stays locked and turns dark
red; press any key to start the locker again. From another VT you can also start one yourself:
`WAYLAND_DISPLAY=wayland-1 wayland-time-lock` (the socket name is in the log).

To turn monitors off when idle, set `monitors_off_after` (seconds) under `[general]`, or bind
the `power-off-monitors` action. Any key press, click or pointer movement turns them back on.
The screen locks as they turn off; set `lock_when_monitors_off = false` to stop that.
Tools like `wlopm` can also switch them through the standard output power protocol, e.g. from
swayidle: `timeout 600 'wlopm --off \*' resume 'wlopm --on \*'`.

Apps like video players can ask to keep the screen awake. That only counts while their window
is on screen.

### File picker

Apps that ask the desktop portal for a file (browsers, Flatpaks, most GTK and Qt apps) get
`wayland-time-files`: places on the left (Recent, Home, your folders, bookmarks, drives), the
path with back, forward and up, a sortable list, search below the folder, the app's file
type filters and options, and a name field when saving (replacing a file asks first).
Keys: arrows and Enter, Backspace or Alt+Up for the parent folder, Alt+Left/Right for back and
forward, Ctrl+L to type a path, Ctrl+F or just typing to search, Ctrl+H for hidden files,
Escape to cancel. `install.sh` registers it with the portal (`wayland-time-portals.conf` picks
it for `FileChooser`, with GTK's as the fallback); log in again to use it. Run by hand,
`wayland-time-files [--open-many | --folder | --save NAME]` prints the paths picked.

Dialogs float rather than tile, and open over the window they belong to: windows with a parent,
X11 dialogs, and anything a portal opens, including when the app couldn't link the dialog to
its window (then it goes over the window that was focused). Portal dialogs from Wayland apps are
linked through xdg-foreign. A dialog bigger than the screen is asked to fit.

### File manager

`wayland-time-file-manager [FOLDER]` ("Files" in the launcher) shows the same places, path bar,
sortable list and search as the file picker, plus a Trash place. Double-click or Enter opens a
folder in place and a file in its default app (from `mimeapps.list`); right-click a file for
Open With, Cut, Copy, Rename, Move to Trash and Delete Permanently, or the empty space for New
Folder, Paste, Open Terminal Here and Show Hidden Files. Copies and moves run in the background
with progress in the status line, and never overwrite: a name that's taken gets " (2)". The
trash follows the freedesktop layout, so it's shared with other file managers; files on other
drives go to that drive's own trash folder. Copying also puts the paths on the clipboard as
text, and pasting accepts paths copied elsewhere. Keys: Enter, F2 rename, Delete trash,
Shift+Delete delete for good, Ctrl+C/X/V, Ctrl+Shift+N new folder, Ctrl+A, F5 reload, and the
picker's navigation keys; the mouse's side buttons go back and forward. It doesn't take over
opening folders from other apps unless you make it the default for `inode/directory`
(`xdg-mime default wayland-time-file-manager.desktop inode/directory`).

Files can be dragged in from other apps and out to them. Dropped on the list they're copied
into the folder shown; on a folder row or a sidebar place they go into that folder (dragged
within the window they're moved), and on Trash they're trashed. Folders can be added to the
sidebar (right-click › Add to Sidebar, Ctrl+D, or dropped on the sidebar's empty space) and
removed with a right-click; they're GTK bookmarks (`~/.config/gtk-3.0/bookmarks`), so other file
managers and file dialogs show them too.

### Bluetooth

`wayland-time-bluetooth` ("Bluetooth" in the launcher, or click the bar's `bluetooth` widget)
turns Bluetooth on and off, lists your devices (connect, disconnect, forget, battery level when
the device reports it), and searches for new ones to pair; a search stops by itself after two
minutes, as searching slows other Bluetooth traffic. Pairing asks to confirm a code or type a
PIN when the device needs it (the app is BlueZ's pairing agent while it's open), and paired
devices are trusted so they reconnect by themselves. Right-click the bar widget to turn
Bluetooth on or off without opening the app. It talks to BlueZ (`bluetoothd`) on the system bus.

### Sound

`wayland-time-sound` ("Sound" in the launcher, or click the bar's `sound` widget) picks the
output (speakers, headphones, HDMI) and input (microphone) and sets their volumes, lists the
apps playing or recording with a volume and mute button for each, and sends an app to a
different output from its ⋯ menu ("Default" makes it follow the default again). Sound cards
can be switched between profiles (for example stereo or surround, or a Bluetooth headset's
music and call modes). On the bar widget, right-click mutes and scrolling changes the volume,
with the same pop-up as the volume keys. It needs PipeWire with WirePlumber: it reads the
state from `pw-dump` and makes changes with `wpctl` and `pw-metadata`.

### Password prompts for admin rights

When an app needs admin rights (installing software, mounting a disk, `pkexec`), polkit asks
the session's authentication agent for a password. The compositor starts one at login:
`[general] polkit_agent`, or when that's empty the first one installed of
`wayland-time-polkit` (our own), polkit-gnome, polkit-kde, polkit-mate and lxpolkit; `"none"`
starts none. `wayland-time-polkit` dims the screen and shows what is being asked, whose password
it wants (Tab or "Switch user" when several people could answer) and a password field, in the
theme's colors. The password goes to polkit's own `polkit-agent-helper-1`, which checks it, so
the prompt never decides anything itself. Nested and headless runs don't start an agent.

## Code map

The repository is a Cargo workspace:

| Directory | Purpose |
|---|---|
| `src/` | the compositor (`wayland-time`) |
| `crates/wt-config/` | the config file and theme, shared by the compositor and the apps: defaults, parsing, validation, keybind and action parsing, color palette, font lookup (unit tested) |
| `crates/wt-ui/` | the shared look for apps: turns `[theme]` into egui styling and fonts, and opens app windows |
| `crates/wt-draw/` | CPU text and shape drawing (hinted, subpixel text): the bar, menus and the lock screen |
| `crates/wt-icons/` | icons from the icon themes: app icons via .desktop files, symbolic icons tinted one color, PNG/SVG loading |
| `apps/settings/` | the settings app (`wayland-time-settings`) |
| `apps/launcher/` | the app launcher (`wayland-time-launcher`) |
| `apps/files/` | the file picker (`wayland-time-files`): desktop portal FileChooser backend on the session bus, and the egui picker window it runs per request; and the file manager (`wayland-time-file-manager`, in `src/bin/`), sharing its listing, places, icons and drawing (`src/lib.rs`) |
| `apps/bluetooth/` | the Bluetooth app (`wayland-time-bluetooth`): BlueZ on the system bus, and its pairing agent |
| `apps/sound/` | the Sound app (`wayland-time-sound`) |
| `crates/wt-pipewire/` | sound devices, app streams and volumes from PipeWire (`pw-dump --monitor`), and changing them (`wpctl`, `pw-metadata`): the Sound app and the bar's sound widget |
| `apps/polkit/` | the password prompt (`wayland-time-polkit`): polkit agent on the system bus, talks to `polkit-agent-helper-1`, drawn with `wt-draw` on a layer surface |
| `apps/lock/` | the lock screen (`wayland-time-lock`): session lock protocol, PAM, drawn with `wt-draw` since egui can't make lock surfaces |

New apps go in `apps/`, use `wt-ui` for their window and look, and `wt-config` for anything
in the config file.

| File | Purpose |
|---|---|
| `src/main.rs` | CLI, startup, event loop, redraw scheduling |
| `src/state.rs` | compositor state and window management: monitors, arrange, focus, actions, config reload |
| `src/session.rs` | saving open windows and restoring them at the next start |
| `src/bar.rs` | the built-in bar: layout, painting, hit testing, clicks |
| `src/layout.rs` | pure tiling math (unit tested); add new layouts here |
| `src/workspace.rs` | per-workspace window order, focus, floating (unit tested) |
| `src/config.rs` | re-exports `wt-config`, plus conversions to smithay types |
| `crates/wt-config/src/config.rs` | TOML parsing, validation, keybind/action parsing (unit tested) |
| `crates/wt-config/src/theme.rs` | shared color palette; resolves color names and `mix(...)` |
| `apps/settings/src/doc.rs` | the config file as an editable document: comment-keeping edits, checking, saving |
| `apps/settings/src/pages/` | the settings pages; plain settings are listed as data in `fields.rs` |
| `src/handlers/xwayland.rs` | Xwayland: starting it, managing X11 windows, sharing the clipboard with X11 |
| `src/focus.rs` | keyboard focus target: a Wayland surface or an X11 window |
| `src/overview.rs` | the overview: workspace cards, live thumbnails, zoom animation, dragging windows between workspaces |
| `src/anim.rs` | animations: tiles sliding into place, the dragged tile, workspace switches |
| `src/notifications.rs` | notification pop-ups, their timeouts and clicks, do not disturb, the bell's list |
| `src/status.rs` | battery, network and backlight from sysfs, for the bar |
| `src/dbus/` | the session bus thread: notification daemon, tray watcher and items, media players |
| `src/ipc.rs` | `wayland-time msg`: the command socket |
| `src/input.rs` | keyboard binds, pointer focus, Mod+drag |
| `src/grabs.rs` | interactive move/resize |
| `src/render.rs` | builds the frame: layers, windows, borders, lock screen |
| `src/wallpaper.rs` | the wallpaper: decoded and fitted to each monitor on a worker thread |
| `src/handlers/` | Wayland protocol handlers (xdg-shell, layer-shell, decorations, seat, clipboard, viewporter, fractional scale) |
| `src/handlers/lock.rs` | screen locking: lock state, lock surfaces, who may unlock |
| `src/handlers/idle.rs` | idle notifications and idle inhibit |
| `src/handlers/output_power.rs` | turning monitors off and on (DPMS), idle power-off timer |
| `src/handlers/output_management.rs` | monitor setup tools (wlr-output-management): heads, modes, applying arrangements |
| `src/backend/winit.rs` | nested backend |
| `src/backend/udev.rs` | hardware backend: DRM/KMS outputs, libinput, session/VT handling |
| `src/backend/headless.rs` | headless backend for tests: draws offscreen at 60 Hz |
| `src/backend/mod.rs` | backend switch, linux-dmabuf (GPU buffer sharing) |
| `src/cursor.rs` | XCursor theme loading for the hardware backend |

`cargo test --workspace` runs the unit tests.

To try multi-monitor behaviour without extra screens, split the nested window into several fake
monitors: `WAYLAND_TIME_NESTED_OUTPUTS=2 cargo run -- -b winit`.

For tests that shouldn't need a visible window, `-b headless` (with
`WAYLAND_TIME_HEADLESS_OUTPUTS=N`) draws offscreen; take screenshots with `grim` and drive it
with `wayland-time msg`, which also has fake input for this: `test-pointer X Y`, `test-click
[BUTTON]`, `test-key CODE` and `test-bar` (where each part of the bar is).

## Roadmap

1. 10-bit output, multi-GPU.
2. Workspace and window state over the `msg` socket, for status bars and scripts.
3. Window open and close animations; workspace swipes that follow your fingers.
