# cterm

A high-performance, customizable terminal emulator built in Rust. cterm uses native AppKit/CoreGraphics on macOS, Win32/Direct2D on Windows, and GTK4 on Wayland for Linux. X11 is intentionally not a supported Linux backend.

## Features

### Terminal Emulation
- **High Performance**: Custom VT100/ANSI terminal emulator with efficient screen buffer management
- **True Color Support**: Full 24-bit RGB color with 256-color palette fallback
- **Unicode Support**: Extended grapheme clusters, combining characters, ZWJ emoji, flags, and wide cells
- **Scrollback Buffer**: Configurable history with grapheme-safe resize reflow
- **Find in Scrollback**: Search through terminal history with regex support

### User Interface
- **Tabs**: Multiple terminal tabs with keyboard shortcuts
- **Tab Customization**: Custom colors and names for tabs
- **Tab Templates**: Persistent tab configurations for frequently-used commands (great for Claude sessions)
- **Quick Launch**: VS Code-style fuzzy search overlay to instantly open or switch to tabs (Cmd+G / Ctrl+Shift+G)
- **Themes**: Built-in themes (Tokyo Night, Dracula, Nord, and more) plus custom TOML themes
- **Keyboard Shortcuts**: Fully configurable shortcuts for all actions
- **Zoom**: Adjustable font size with Ctrl+/Ctrl-
- **Copy as HTML**: Copy terminal content with colors and formatting preserved (macOS)
- **Send Signal**: Send Unix signals (SIGHUP, SIGINT, SIGTERM, etc.) to terminal processes (macOS/Linux)

### Terminal Features
- **Hyperlinks**: Clickable URLs with OSC 8 support
- **Clipboard**: OSC 52 clipboard integration for remote copy/paste
- **Dynamic Colors**: Theme-aware OSC 10/11/12 query, set, and reset support
- **Alternate Screen**: Full alternate screen buffer support (for vim, less, etc.)
- **Sixel Graphics**: Inline image display with DEC Sixel protocol support
- **iTerm2 Graphics**: Inline images via OSC 1337 protocol (PNG, JPEG, GIF)
- **iTerm2 File Transfer**: Receive files via OSC 1337 with streaming support for large files
- **DRCS Fonts**: Soft font support via DECDLD for custom character sets

### System Integration
- **Native PTY**: Cross-platform PTY implementation (Unix openpty, Windows ConPTY)
- **Daemon Architecture**: Terminal sessions live in the `ctermd` daemon and survive UI restarts, upgrades, and crashes
- **Seamless Upgrades**: Update cterm without losing terminal sessions - daemon keeps sessions alive across restarts
- **Auto-Update**: Built-in update checker with GitHub releases integration and release notes display
- **Debug Log Viewer**: In-app log viewer for troubleshooting (Windows)

## Platform status

| Platform | Local terminal UI | CI contract |
|---|---|---|
| macOS (Intel/Apple Silicon) | AppKit/CoreGraphics | Native build, unit/integration tests, and UI automation |
| Windows x64 | Win32/Direct2D + ConPTY | Native build, unit/integration tests, and UI automation |
| Linux x86_64/ARM64 | GTK4 on Wayland only | Native build, unit/integration tests, and a headless-Wayland UI smoke test |
| FreeBSD 14.4 | No desktop UI yet | `cterm-core` and `cterm-ui` tests in a FreeBSD VM |
| Android/iOS | Not currently supported | Distant local-terminal targets; no release or CI contract yet |

The three desktop renderers display terminal text, selections, cursor shapes,
Sixel images, and text attributes natively. FreeBSD currently validates the
portable Rust core only. Linux builds do not include or test an X11 fallback.

## Installation

### Pre-built Binaries

| Platform | Download |
|----------|----------|
| **macOS** (Universal) | [DMG Installer](https://github.com/corbet-labs/cterm/releases/latest/download/cterm-macos-universal.dmg) |
| **Windows** (x64) | [Installer](https://github.com/corbet-labs/cterm/releases/latest/download/cterm-windows-x86_64-setup.exe) · [ZIP](https://github.com/corbet-labs/cterm/releases/latest/download/cterm-windows-x86_64.zip) |
| **Linux** (x64) | [tar.gz](https://github.com/corbet-labs/cterm/releases/latest/download/cterm-linux-x86_64.tar.gz) |
| **Linux** (ARM64) | [tar.gz](https://github.com/corbet-labs/cterm/releases/latest/download/cterm-linux-arm64.tar.gz) |

Or browse all releases on the [GitHub Releases](https://github.com/corbet-labs/cterm/releases) page.

### Building from Source

#### Prerequisites

- Rust 1.88 or later
- Protocol Buffers compiler (`protoc`)

**Linux only** - GTK4, libadwaita, Pango, and Cairo development libraries. A
Wayland compositor is required at runtime; X11-only sessions are unsupported.

**Debian/Ubuntu:**
```bash
sudo apt install libgtk-4-dev libadwaita-1-dev libpango1.0-dev libcairo2-dev protobuf-compiler
```

**Fedora:**
```bash
sudo dnf install gtk4-devel libadwaita-devel pango-devel cairo-devel protobuf-compiler
```

**Arch Linux:**
```bash
sudo pacman -S gtk4 libadwaita pango cairo protobuf
```

**macOS:**
Uses native AppKit/CoreGraphics. Install `protobuf` for source builds (for
example, `brew install protobuf`).

**Windows:**
Uses native Win32/Direct2D. Install `protoc` for source builds (the public CI
uses Chocolatey).

#### Build

```bash
# Development build
cargo build

# Release build (optimized)
cargo build --release

# Run
cargo run --release
```

The binary will be at `target/release/cterm`.

### Launching a command

`--execute` starts an executable directly. Arguments after the command are
passed as a trailing argument vector; cterm does not join them into a shell
command line. The command contract is UTF-8; each accepted value remains a
distinct argv element.

```bash
cterm --execute my-tui -- --profile "Jane Doe" --literal-flag
cterm --directory ./workspace --env MODE=review --env COLOR=always \
  --execute my-tui -- input.json
```

`--env NAME=VALUE` is repeatable. Command-line values override `[general.env]`
values, and the last repeated name wins. An explicit command, directory,
environment value, or title creates a fresh session instead of reconnecting to
an unrelated existing daemon session. `--maximized` and `--fullscreen` control
the initial native window.

### Managed product mode

Embedding packages can opt into a fail-closed, isolated runtime contract:

```bash
cterm --managed \
  --config-dir /absolute/product/config \
  --daemon-socket /absolute/product/run/ctermd.sock \
  --daemon-identity product-alpha \
  --daemon-executable ctermd \
  --execute product-tui -- --profile default
```

`--daemon-executable` is resolved relative to the cterm UI executable and must
stay inside that directory tree. Managed mode never searches `PATH`, requires
the daemon identity, protocol, and package version to match exactly, always
creates a fresh session, and removes cterm's upstream update actions. On
Windows, `--daemon-socket` is an exact named-pipe path such as
`\\.\pipe\product-ctermd-user`; ctermd rejects remote pipe clients. A per-user
pipe name prevents accidental collisions but is not an authorization secret;
Windows still applies the launching process token's default pipe DACL.

## Configuration

Configuration files are stored in platform-specific locations:
- **Linux**: `~/.config/cterm/`
- **macOS**: `~/Library/Application Support/com.cterm.terminal/`
- **Windows**: `%APPDATA%\cterm\`

See [docs/configuration.md](docs/configuration.md) for detailed configuration options.

## Keyboard Shortcuts

| Action | macOS | Linux/Windows |
|--------|-------|---------------|
| New Tab | Cmd+T | Ctrl+Shift+T |
| Close Tab | Cmd+W | Ctrl+Shift+W |
| Next Tab | Cmd+Shift+] | Ctrl+Tab |
| Previous Tab | Cmd+Shift+[ | Ctrl+Shift+Tab |
| Switch to Tab 1-9 | Cmd+1-9 | Ctrl+1-9 |
| Quick Launch | Cmd+G | Ctrl+Shift+G |
| Copy | Cmd+C | Ctrl+Shift+C |
| Copy as HTML | Cmd+Shift+C | — |
| Paste | Cmd+V | Ctrl+Shift+V |
| Find | Cmd+F | Ctrl+Shift+F |
| Zoom In | Cmd++ | Ctrl++ |
| Zoom Out | Cmd+- | Ctrl+- |
| Reset Zoom | Cmd+0 | Ctrl+0 |

**Scrollback:** Use mouse wheel or trackpad to scroll through terminal history.

## Quick Launch

Press **Cmd+G** (macOS) or **Ctrl+Shift+G** (Linux/Windows) to open the Quick Launch overlay. It provides a fuzzy search over your tab templates, letting you instantly open a new tab or switch to an existing one.

- Type to filter templates by name
- Use **Arrow keys** or **Tab**/**Shift+Tab** to navigate results
- Press **Enter** to launch, **Escape** to dismiss

## Tab Templates

Tab templates are defined in `sticky_tabs.toml` in your [configuration directory](#configuration). Each template pre-configures a tab with a command, working directory, color, and other settings.

```toml
[[tabs]]
name = "Claude"
command = "claude"
color = "#7c3aed"
unique = true
keep_open = true

[[tabs]]
name = "Project"
working_directory = "~/projects/myapp"
color = "#22c55e"

[[tabs]]
name = "Dev Server"
command = "npm"
args = ["run", "dev"]
working_directory = "~/projects/myapp"
keep_open = true
```

### Template options

| Field | Description |
|-------|-------------|
| `name` | Display name shown in Quick Launch and as the tab title |
| `command` | Command to run (omit for default shell) |
| `args` | Command arguments (array) |
| `working_directory` | Initial working directory |
| `git_remote` | Git URL to clone if `working_directory` doesn't exist |
| `color` | Tab color in hex (`#RRGGBB`) |
| `theme` | Theme override for this tab |
| `background_color` | Lock the background color (overrides theme, hex `#RRGGBB`) |
| `keep_open` | Keep the tab open after the process exits |
| `unique` | Singleton mode — only one instance of this tab can exist at a time |
| `env` | Extra environment variables (table) |
| `docker` | Docker container config (see below) |
| `ssh` | SSH remote config (see below) |

### Singleton tabs (`unique = true`)

When a template has `unique = true`, launching it via Quick Launch will **switch to the existing tab** if one is already open, instead of creating a duplicate. This is ideal for tools that should only run once, like AI assistants or long-running servers.

Combined with Quick Launch, this creates a "go to or open" workflow: press **Cmd+G**, type a few characters, hit **Enter**, and you're either switched to your running session or a new one is started — no need to hunt through tabs.

### Docker templates

Templates can launch shells inside Docker containers. Set the `docker` field with a mode (`exec`, `run`, or `devcontainer`):

```toml
[[tabs]]
name = "Dev Container"
color = "#0db7ed"
unique = true
[tabs.docker]
mode = "devcontainer"
image = "ubuntu:24.04"
project_dir = "~/projects/myapp"
shell = "/bin/bash"
mount_ssh = true
```

| Field | Description |
|-------|-------------|
| `mode` | `exec` (attach to running container), `run` (start new), or `devcontainer` |
| `container` | Container name/ID (exec mode) |
| `image` | Image name with tag (run/devcontainer mode) |
| `shell` | Shell to use inside the container |
| `project_dir` | Project directory to mount (devcontainer mode) |
| `docker_args` | Additional docker arguments (array) |
| `auto_remove` | Remove container on exit (default: true) |
| `mount_claude_config` | Mount Claude config into container (default: true) |
| `mount_ssh` | Mount SSH keys into container (default: false) |
| `mount_gitconfig` | Mount git config into container (default: true) |
| `workdir` | Working directory inside container |

### SSH templates

Templates can open SSH connections with port forwarding, jump hosts, and more:

```toml
[[tabs]]
name = "Production"
color = "#22c55e"
unique = true
[tabs.ssh]
host = "prod.example.com"
username = "deploy"
identity_file = "~/.ssh/prod_key"
```

| Field | Description |
|-------|-------------|
| `host` | Remote host (required) |
| `port` | SSH port (default: 22) |
| `username` | SSH username |
| `identity_file` | Path to private key |
| `remote_command` | Command to execute on the remote host |
| `local_forwards` | Local port forwards (array, format: `"local:host:remote"`) |
| `remote_forwards` | Remote port forwards (array) |
| `dynamic_forward` | SOCKS proxy port |
| `jump_host` | Bastion/jump host |
| `agent_forward` | Forward SSH agent (default: false) |
| `x11_forward` | Forward X11 (default: false) |
| `options` | Extra SSH options as key-value pairs (table, passed as `-o`) |
| `extra_args` | Additional raw SSH arguments (array) |

## Terminal Compatibility

### Supported DEC Private Modes (DECSET/DECRST)

| Mode | Name | Description |
|------|------|-------------|
| 1 | DECCKM | Application cursor keys |
| 6 | DECOM | Origin mode (cursor addressing relative to scroll region) |
| 7 | DECAWM | Auto-wrap mode |
| 9 | X10 Mouse | X10 mouse reporting (button press only) |
| 25 | DECTCEM | Show/hide cursor |
| 80 | DECSDM | Sixel display mode (scrolling control) |
| 1000 | — | Normal mouse tracking (button press/release) |
| 1002 | — | Button-event mouse tracking (press/release/motion with button) |
| 1003 | — | Any-event mouse tracking (all motion events) |
| 1004 | — | Focus event reporting |
| 1006 | — | SGR extended mouse coordinates |
| 1047 | — | Alternate screen buffer |
| 1048 | — | Save/restore cursor |
| 1049 | — | Alternate screen buffer with cursor save/restore |
| 2004 | — | Bracketed paste mode |

### Supported ANSI Modes (SM/RM)

| Mode | Name | Description |
|------|------|-------------|
| 4 | IRM | Insert mode |
| 20 | LNM | Line feed/new line mode |

### Supported OSC Sequences

| OSC | Description |
|-----|-------------|
| 0 | Set window title and icon name |
| 1 | Set icon name |
| 2 | Set window title |
| 8 | Hyperlinks |
| 10 | Query/set foreground color |
| 11 | Query/set background color |
| 12 | Query/set cursor color |
| 52 | Clipboard operations |
| 110 | Reset foreground color |
| 111 | Reset background color |
| 112 | Reset cursor color |
| 1337 | iTerm2 inline images and file transfer |

### Sixel Graphics

cterm supports DEC Sixel graphics for inline image display:
- Full color palette support (up to 256 colors)
- RGB and HLS color definitions
- DECSDM mode for controlling image placement and scrolling
- Images scroll with terminal content
- Grid cells under images are cleared (xterm-compatible behavior)
- DA1 reports Sixel, ANSI color, and OSC 52 capabilities (`CSI ? 62 ; 4 ; 22 ; 52 c`)
- `CSI 16 t` reports the renderer's current character-cell height and width

### Enhanced keyboard events

cterm implements the kitty progressive keyboard protocol's disambiguation and
event-type flags. Applications can request flags `1|2` to receive unambiguous
press, repeat, and release events for functional keys and disambiguated modified
keys. Main and alternate screens keep independent flag stacks.

All-key, alternate-key, and associated-text reporting are currently masked out
when requested. Those modes require layout- and IME-accurate physical key data
that is not available consistently across GTK, AppKit, and Win32; cterm reports
only the flags it can honor on every backend.

Test with:
```bash
# Using ImageMagick
convert image.png -resize 200x200 sixel:-

# Using libsixel
img2sixel image.png
```

### iTerm2 Graphics Protocol (OSC 1337)

cterm supports iTerm2's inline image protocol for displaying PNG, JPEG, and GIF images:
- Inline image display with `inline=1`
- File transfer with `inline=0` (shows notification bar with Save/Save As/Discard)
- Streaming file transfer support for large files (spills to disk when >1MB)
- Configurable width/height in pixels, cells, or percentages
- Aspect ratio preservation

Test with:
```bash
# Using imgcat (from iTerm2 utilities)
imgcat image.png

# Manual test (inline image)
printf '\033]1337;File=inline=1:'$(base64 < image.png)'\a'

# Manual test (file transfer)
printf '\033]1337;File=name='$(echo -n "test.bin" | base64)':'$(base64 < file.bin)'\a'
```

### DRCS (Soft Fonts)

cterm supports DECDLD (DEC Download) for custom character sets:
- Define custom glyphs via escape sequences
- Multiple font sizes supported
- Designate fonts to G0/G1 character sets

## Architecture

```
cterm/
├── crates/
│   ├── cterm-core/      # Core terminal emulation (parser, screen, PTY)
│   ├── cterm-ui/        # UI abstraction traits
│   ├── cterm-app/       # Application logic (config, sessions, upgrades, crash recovery)
│   ├── cterm-cocoa/     # Native macOS UI using AppKit/CoreGraphics
│   ├── cterm-gtk/       # GTK4 UI implementation (Linux)
│   ├── cterm-win32/     # Native Windows UI using Win32/Direct2D
│   └── cterm-headless/  # Headless terminal daemon (ctermd)
└── docs/                # Documentation
```

The modular architecture enables:
- **cterm-core**: Pure Rust terminal emulation, reusable in other projects
- **cterm-ui**: UI-agnostic traits for toolkit abstraction
- **cterm-app**: Shared application logic between UI implementations
- **cterm-cocoa**: Native macOS implementation using AppKit and CoreGraphics
- **cterm-gtk**: GTK4-specific rendering and widgets (Linux)
- **cterm-win32**: Native Windows implementation using Win32 and Direct2D
- **cterm-headless**: Headless terminal daemon for remote access (ctermd)

## Built-in Themes

- Default Dark
- Default Light
- Tokyo Night
- Dracula
- Nord

Custom themes can be added as TOML files in the `themes/` configuration subdirectory.

## Roadmap

- [x] Text selection and copy/paste
- [x] Crash recovery (macOS/Linux)
- [x] Sixel graphics support
- [x] iTerm2 graphics protocol (OSC 1337)
- [x] DRCS soft font support
- [x] Windows native UI (Win32/Direct2D)
- [x] Seamless upgrades (macOS/Linux/Windows)
- [x] Copy as HTML with formatting
- [x] Tab templates with Quick Launch
- [x] Docker and SSH templates
- [x] Auto-update with release notes

### Future

- Split panes
- Plugin system
- Additional foot-compatible behavior and performance work
- Android and iOS local-terminal frontends (distant targets)

## License

cterm is licensed under
[FSL-1.1-ALv2](https://fsl.software/FSL-1.1-ALv2.template.md). Each version
automatically becomes available under Apache-2.0 two years after it is made
available.

Source inherited from KarpelesLab/cterm and Rio/Sugarloaf retains its MIT
grants and notices; see [THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md) and
[LICENSES](LICENSES/).

## Contributing

Contributions are welcome! Please open an issue or pull request on GitHub.
