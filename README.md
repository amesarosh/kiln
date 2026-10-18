# wayland-time

A lightweight, fast, configurable tiling Wayland compositor written in Rust, built on [Smithay](https://github.com/Smithay/smithay).

- **Dynamic tiling** (master/stack and monocle), toggleable floating, fullscreen, 1–32 workspaces
- **Multiple monitors**, each with its own set of workspaces (like dwm), placed left to right
- **Built-in clickable bar** per monitor (top, bottom, left or right): workspaces, one tab per window with its app icon, layout switcher and a launcher button
- **One TOML file** for keybinds, gaps, borders, colors, keyboard layout and window rules, **reloaded on save**
- **Idle costs nothing:** frames are only drawn when something changes, and only damaged regions are repainted
- Works with standard desktop tools: bars (waybar), launchers (fuzzel, rofi, wofi), wallpaper tools (swaybg) via `wlr-layer-shell`
- Runs nested for development or **on real hardware from a TTY**, with hardware cursor planes, direct scanout and GPU buffer sharing (linux-dmabuf)
- Server-side decorations requested by default, so apps don't draw title bars; clipboard and primary selection supported
- **X11 apps through Xwayland** (Steam, games, the VS Code snap, older programs): tiled and focused like any window, with copy and paste working between X11 and Wayland apps
- **Screen locking** (swaylock, hyprlock), idle handling (swayidle) and monitors turning off when idle, with video players able to keep the screen awake

## Status

- **Nested** inside an existing X11 or Wayland session (development setup): working.
- **TTY / real hardware** (DRM/KMS, libinput, libseat): implemented, needs testing on more machines.
  Single GPU only. Mode, scale, position and rotation are set per monitor with `[[outputs]]`.

## Build and run

```sh
cargo build --release
./target/release/wayland-time -e alacritty
```

On first start it writes a fully commented default config to `~/.config/wayland-time/config.toml`.

```
-c, --config <PATH>    use a different config file
-e, --exec <CMD>       run a command once ready (repeatable)
-b, --backend <NAME>   auto (default), winit or tty
-o, --output <NAME>    tty only: light up just this monitor, e.g. DP-4 (repeatable)
    --check-config     validate the config and exit
```

Logging uses `RUST_LOG`, e.g. `RUST_LOG=debug`.

### Running from a TTY

Build dependencies: `libinput-dev`, `libseat-dev`, `libgbm-dev`, `libdrm-dev`, `libudev-dev`, `libxkbcommon-dev`.
For X11 apps, install `xwayland` (turn it off with `xwayland = false` under `[general]`).

1. Switch to a free text console with `Ctrl+Alt+F3` and log in.
2. For a first test, use a timeout so it exits on its own even if input doesn't work:
   `timeout 60 ./target/release/wayland-time -e alacritty`
3. Quit with `Super+Shift+e`. `Ctrl+Alt+F1..F12` switch consoles at any time.

When run from a TTY, logs go to `~/.local/state/wayland-time/wayland-time.log` (previous run: `.log.old`).

To pick it from a login screen (GDM, SDDM…), install the binary and session file:

```sh
sudo install -m755 target/release/wayland-time /usr/local/bin/
sudo install -m644 resources/wayland-time.desktop /usr/share/wayland-sessions/
```

## Default keys

`Mod` is **Alt when nested** (so the host desktop doesn't steal Super) and **Super on hardware**. Set `general.mod` to override. A bare `"Mod"` bind fires when you tap Mod by itself.
`Ctrl+Alt+F1..F12` always switch consoles on hardware.

| Keys | Action |
|---|---|
| `Mod+Return` | terminal (alacritty) |
| `Mod+d` | launcher (fuzzel) |
| `Mod+j` / `Mod+k` | focus next / previous |
| `Mod+Shift+j` / `Mod+Shift+k` | move window down / up the stack |
| `Mod+Shift+Return` | swap with master |
| `Mod+h` / `Mod+l` | shrink / grow master |
| `Mod+i` / `Mod+o` | more / fewer master windows |
| `Mod+t` / `Mod+m` / `Mod+space` | tile / monocle / next layout |
| `Mod+Shift+space` | toggle floating |
| `Mod+f` | toggle fullscreen |
| `Mod+1..9` / `Mod+Shift+1..9` | go to / send to workspace (on the focused monitor) |
| `Mod+.` / `Mod+,` | focus next / previous monitor |
| `Mod+Shift+.` / `Mod+Shift+,` | send window to next / previous monitor |
| `Mod+Shift+q` | close window |
| `Mod+Shift+r` | reload config |
| `Mod+Escape` | lock the screen (swaylock) |
| `Mod+Shift+e` | quit |
| `Mod+left drag` / `Mod+right drag` | move / resize (floats the window); drop on another monitor to move it there |

## Configuration

### Monitors and the bar

Each monitor has its own workspaces 1–9. Workspace keys, new windows and the launcher act on the
focused monitor, which follows the mouse (with `focus_follows_mouse`) or the last click.
When a monitor is unplugged, its windows move to the same-numbered workspaces of the focused one.

Each monitor also gets the built-in bar (`[bar]` in the config). Click a workspace or tab to switch
to it, middle-click a tab to close that window, click the layout name to cycle layouts, and scroll
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

Everything the compositor draws (the bar, window borders, the lock screen) takes its font and
colors from `[theme]`. Colors are named once in `[theme.colors]` and referred to by name, and
shades like `hover = "mix(surface, text, 0.1)"` are built from them, so changing `surface` or
`accent` updates every part that uses it. A section can override colors for itself alone with
its own table, e.g. `[bar.colors]`.

If you define `[binds]`, it replaces the default binds entirely. See [`resources/default-config.toml`](resources/default-config.toml) for every option and action.
A config with errors is rejected as a whole and the last working config stays active; all problems are logged at once.

### Screen lock and idle

Any locker that uses the standard lock protocol works, such as `swaylock` or `hyprlock`. Bind
one to a key, and use `swayidle` to lock after a while and before suspend:

```toml
[general]
autostart = ["swayidle -w timeout 300 'swaylock -f' before-sleep 'swaylock -f'"]

[binds]
"Mod+Escape" = "spawn swaylock"
```

While locked, only the locker is drawn and every key goes to it. No binds work, including
Ctrl+Alt+Backspace (quitting would drop you into the shell that started the compositor), but
Ctrl+Alt+F1..F12 still switch VTs. If the locker crashes, the screen stays locked and turns dark
red. To recover, switch to another VT, log in, and start a new locker there:
`WAYLAND_DISPLAY=wayland-1 swaylock` (the socket name is in the log).

To turn monitors off when idle, set `monitors_off_after` (seconds) under `[general]`, or bind
the `power-off-monitors` action. Any key press, click or pointer movement turns them back on.
Tools like `wlopm` can also switch them through the standard output power protocol, e.g. from
swayidle: `timeout 600 'wlopm --off \*' resume 'wlopm --on \*'`.

Apps like video players can ask to keep the screen awake. That only counts while their window
is on screen.

## Code map

| File | Purpose |
|---|---|
| `src/main.rs` | CLI, startup, event loop, redraw scheduling |
| `src/state.rs` | compositor state and window management: monitors, arrange, focus, actions, config reload |
| `src/bar.rs` | the built-in bar: layout, painting, hit testing, clicks |
| `src/text.rs` | CPU text and shape drawing for compositor-drawn UI |
| `src/layout.rs` | pure tiling math (unit tested); add new layouts here |
| `src/workspace.rs` | per-workspace window order, focus, floating (unit tested) |
| `src/config.rs` | TOML parsing, validation, keybind/action parsing (unit tested) |
| `src/theme.rs` | shared font and color palette; resolves color names and `mix(...)` |
| `src/handlers/xwayland.rs` | Xwayland: starting it, managing X11 windows, sharing the clipboard with X11 |
| `src/focus.rs` | keyboard focus target: a Wayland surface or an X11 window |
| `src/icons.rs` | app icons for the bar: .desktop lookup, icon theme search, PNG/SVG loading |
| `src/input.rs` | keyboard binds, pointer focus, Mod+drag |
| `src/grabs.rs` | interactive move/resize |
| `src/render.rs` | builds the frame: layers, windows, borders, lock screen |
| `src/handlers/` | Wayland protocol handlers (xdg-shell, layer-shell, decorations, seat, clipboard, viewporter, fractional scale) |
| `src/handlers/lock.rs` | screen locking: lock state, lock surfaces, who may unlock |
| `src/handlers/idle.rs` | idle notifications and idle inhibit |
| `src/handlers/output_power.rs` | turning monitors off and on (DPMS), idle power-off timer |
| `src/backend/winit.rs` | nested backend |
| `src/backend/udev.rs` | hardware backend: DRM/KMS outputs, libinput, session/VT handling |
| `src/backend/mod.rs` | backend switch, linux-dmabuf (GPU buffer sharing) |
| `src/cursor.rs` | XCursor theme loading for the hardware backend |

`cargo test` runs the unit tests.

To try multi-monitor behaviour without extra screens, split the nested window into several fake
monitors: `WAYLAND_TIME_NESTED_OUTPUTS=2 cargo run -- -b winit`.

## Roadmap

1. 10-bit output, multi-GPU.
2. IPC socket for status bars and scripting (workspace state, focused window, run actions).
3. More protocols: xdg-activation, screencopy (screenshots/screen sharing), presentation-time.
4. Small optional animations (window open/close, workspace switch).
