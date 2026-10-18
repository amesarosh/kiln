# wayland-time

A lightweight, fast, configurable tiling Wayland compositor written in Rust, built on [Smithay](https://github.com/Smithay/smithay).

- **Dynamic tiling** (master/stack and monocle), toggleable floating, fullscreen, 1–32 workspaces
- **One TOML file** for keybinds, gaps, borders, colors, keyboard layout and window rules, **reloaded on save**
- **Idle costs nothing:** frames are only drawn when something changes, and only damaged regions are repainted
- Works with standard desktop tools: bars (waybar), launchers (fuzzel, rofi, wofi), wallpaper tools (swaybg) via `wlr-layer-shell`
- Runs nested for development or **on real hardware from a TTY**, with hardware cursor planes, direct scanout and GPU buffer sharing (linux-dmabuf)
- Server-side decorations requested by default, so apps don't draw title bars; clipboard and primary selection supported

## Status

- **Nested** inside an existing X11 or Wayland session (development setup): working.
- **TTY / real hardware** (DRM/KMS, libinput, libseat): implemented, needs testing on more machines.
  Single GPU only; extra monitors light up but tiling happens on the first one.

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
    --check-config     validate the config and exit
```

Logging uses `RUST_LOG`, e.g. `RUST_LOG=debug`.

### Running from a TTY

Build dependencies: `libinput-dev`, `libseat-dev`, `libgbm-dev`, `libdrm-dev`, `libudev-dev`, `libxkbcommon-dev`.

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

`Mod` is **Alt when nested** (so the host desktop doesn't steal Super) and **Super on hardware**. Set `general.mod` to override.
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
| `Mod+1..9` / `Mod+Shift+1..9` | go to / send to workspace |
| `Mod+Shift+q` | close window |
| `Mod+Shift+r` | reload config |
| `Mod+Shift+e` | quit |
| `Mod+left drag` / `Mod+right drag` | move / resize (floats the window) |

## Configuration

Any section or key you leave out uses the default, so a config can be as small as:

```toml
[general]
autostart = ["waybar", "swaybg -i ~/wall.png"]

[layout]
gaps_inner = 10
smart_gaps = true

[decoration]
border_active = "#f38ba8"

[[rules]]
app_id = "pavucontrol"
floating = true
```

If you define `[binds]`, it replaces the default binds entirely. See [`resources/default-config.toml`](resources/default-config.toml) for every option and action.
A config with errors is rejected as a whole and the last working config stays active; all problems are logged at once.

## Code map

| File | Purpose |
|---|---|
| `src/main.rs` | CLI, startup, event loop, redraw scheduling |
| `src/state.rs` | compositor state and window management: arrange, focus, actions, config reload |
| `src/layout.rs` | pure tiling math (unit tested); add new layouts here |
| `src/workspace.rs` | per-workspace window order, focus, floating (unit tested) |
| `src/config.rs` | TOML parsing, validation, keybind/action parsing (unit tested) |
| `src/input.rs` | keyboard binds, pointer focus, Mod+drag |
| `src/grabs.rs` | interactive move/resize |
| `src/render.rs` | builds the frame: layers, windows, borders |
| `src/handlers/` | Wayland protocol handlers (xdg-shell, layer-shell, decorations, seat, clipboard) |
| `src/backend/winit.rs` | nested backend |
| `src/backend/udev.rs` | hardware backend: DRM/KMS outputs, libinput, session/VT handling |
| `src/backend/mod.rs` | backend switch, linux-dmabuf (GPU buffer sharing) |
| `src/cursor.rs` | XCursor theme loading for the hardware backend |

`cargo test` runs the unit tests.

## Roadmap

1. Multiple monitors (workspaces per output) and output config (mode, scale, position, 10-bit), multi-GPU.
2. Session lock (screen locker) and idle handling, which a real session needs.
3. XWayland for X11-only apps.
4. IPC socket for status bars and scripting (workspace state, focused window, run actions).
5. More protocols: xdg-activation, fractional scale, viewporter, idle-inhibit, screencopy (screenshots/screen sharing), presentation-time.
6. Small optional animations (window open/close, workspace switch).
