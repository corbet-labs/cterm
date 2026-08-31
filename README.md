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
- **Split Panes**: Native nested panes with draggable dividers, directional focus and resize, and temporary zoom
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
- **Shell Integration**: OSC 133 prompt navigation and last-command output boundaries
- **Desktop Notifications**: Native OSC 9/777 and Kitty OSC 99 notifications
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
| FreeBSD 14.4 | Experimental GTK4/Wayland source build | Native client/daemon build, library and daemon-integration tests, plus a compositor-backed Wayland UI smoke test in FreeBSD VMs; packages still pending |
| Android/iOS | Not currently supported | Distant local-terminal targets; no release or CI contract yet |

The three production desktop renderers display terminal text, selections,
cursor shapes, Sixel images, and text attributes natively. FreeBSD reuses the
GTK4/Wayland frontend and has hard native build, test, and compositor-backed UI
contracts, but is not a packaged release target yet. Linux builds do not
include or test an X11 fallback.

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

With Nix, the repository's pinned development shell supplies Rust, `protoc`,
and the native Linux UI dependencies:

```bash
nix develop
```

Otherwise, install the platform prerequisites above, then build normally:

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

The fail-closed command-plugin package, ABI, isolated runner, deterministic
discovery, machine-local grants, and shared authorization/execution bridge are
documented in [docs/plugins.md](docs/plugins.md). GTK/Wayland, macOS, and
Windows expose installed commands under **Tools → Plugins**, request exact
permissions in native dialogs, and execute them outside the UI thread. Managed
mode keeps the complete plugin surface disabled.

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
| Previous Shell Prompt | Ctrl+Shift+Z | Ctrl+Shift+Z |
| Next Shell Prompt | Ctrl+Shift+X | Ctrl+Shift+X |

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
| 25 | DECTCEM | Show/hide cursor |
| 45 | — | Reverse-wrap at the left margin |
| 80 | DECSDM | Sixel display mode (scrolling control) |
| 1000 | — | Normal mouse tracking (button press/release) |
| 1002 | — | Button-event mouse tracking (press/release/motion with button) |
| 1003 | — | Any-event mouse tracking (all motion events) |
| 1004 | — | Focus event reporting |
| 1006 | — | SGR extended mouse coordinates |
| 1007 | — | Alternate-screen wheel-to-cursor-key translation |
| 1015 | — | URXVT decimal mouse coordinates |
| 1016 | — | SGR pixel mouse coordinates |
| 1047 | — | Alternate screen buffer |
| 1048 | — | Save/restore cursor |
| 1049 | — | Alternate screen buffer with cursor save/restore |
| 1070 | — | Use a private palette for each Sixel image |
| 2004 | — | Bracketed paste mode |
| 2026 | — | Synchronized application updates |
| 2031 | — | Native theme change reports |
| 2033 | — | Native window visibility reports |
| 8452 | — | Place the cursor to the right of Sixel graphics |

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
| 4 | Query/set indexed palette colors |
| 7 | Track the shell-reported working directory (`file:` URI) |
| 8 | Hyperlinks |
| 9 | iTerm2/Foot desktop notifications |
| 10 | Query/set foreground color |
| 11 | Query/set background color |
| 12 | Query/set cursor color |
| 52 | Clipboard operations |
| 99 | Kitty desktop notifications |
| 104 | Reset indexed palette colors |
| 110 | Reset foreground color |
| 111 | Reset background color |
| 112 | Reset cursor color |
| 133 | Shell prompt and command-output markers |
| 777 | URxvt `notify` desktop notifications |
| 1337 | iTerm2 inline images and file transfer |

OSC 4/10–12 replies are generated by the daemon from the active frontend palette,
so startup queries cannot be lost while a native window is still attaching.

OSC 133 A/C/D markers survive scrollback, terminal resize/reflow, and daemon
reconnects. Ctrl+Shift+Z/X navigate to the previous/next marked prompt; the core
also exposes the most recently completed command output delimited by C/D.

### Desktop Notifications

OSC 9 and `OSC 777;notify` accept a UTF-8 title and optional body, matching
Foot's first-semicolon split and its rejection of the unrelated numeric OSC 9
Windows/ConEmu forms. Kitty OSC 99 supports bounded title/body chunking, UTF-8
and base64 payloads, stable replacement and close IDs, urgency, and the default
focus action. Its capability reply advertises exactly that common subset:
`p=title,body,?,close`, `a=focus`, `o=always`, `u=0,1,2`, and `c=0`.

Delivery uses GApplication notifications on Linux, UserNotifications.framework
on macOS, and native shell notification balloons on Windows. Identified
notifications can be replaced or closed on all three frontends, and activating
a notification focuses cterm when requested. macOS asks for notification
permission on first application startup.

### Supported Xterm Palette Stack Sequences

| Sequence | Description |
|----------|-------------|
| XTPUSHCOLORS (`CSI # P`) | Save the current dynamic foreground, background, cursor, and indexed palette |
| XTPOPCOLORS (`CSI # Q`) | Restore a saved dynamic palette |
| XTREPORTCOLORS (`CSI # R`) | Report the current palette-stack slot and allocated stack size |

### Supported Theme and Visibility Reports

cterm follows foot's native-state reporting extension. `CSI ? 996 n` reports a
dark or light frontend as `CSI ? 997 ; 1 n` or `CSI ? 997 ; 2 n`; `CSI ? 998 n`
reports a visible or hidden/minimized window as `CSI ? 999 ; 1 n` or
`CSI ? 999 ; 2 n`. DEC private modes 2031 and 2033 enable change reports and
support DECRQM, XTSAVE, and XTRESTORE. GTK/Wayland, Cocoa, and Win32 feed their
native window state to the daemon, which remains the single PTY reply authority.

### Supported DEC Rectangular Editing

| Sequence | Description |
|----------|-------------|
| DECCARA | Change bold, underline, blink, and inverse attributes in a rectangle |
| DECRARA | Invert those attributes in a rectangle |
| DECCRA | Copy a rectangle, including overlap-safe copies |
| DECFRA | Fill a rectangle using the current SGR style |
| DECERA | Erase a rectangle using the current background color |

Coordinates honor origin mode and scrolling margins, are clipped to the active
page, and preserve the cursor position.

### Sixel Graphics

cterm supports DEC Sixel graphics for inline image display:
- Full color palette support (up to 1024 colors)
- RGB and DEC HLS color definitions
- DEC pixel-aspect ratios and raster attributes (`Pan`, `Pad`, `Ph`, `Pv`)
- Private and shared color registers via DEC mode 1070
- Foot-compatible color and geometry management replies (`CSI ? … S`)
- Configurable dimensions up to 10,000×10,000 pixels, with a separate 64 MiB
  default allocation budget for untrusted output
- DECSDM mode for controlling image placement and scrolling
- Images scroll with terminal content
- Grid cells under images are cleared (xterm-compatible behavior)
- DA1 reports Sixel, ANSI color, rectangular editing, and OSC 52 capabilities (`CSI ? 62 ; 4 ; 22 ; 28 ; 52 c`)
- `CSI 16 t` reports the renderer's current character-cell height and width

### Enhanced keyboard events

cterm implements all five kitty progressive keyboard flags: disambiguated keys,
press/repeat/release events, alternate keys, all-key reporting, and associated
text. GTK/Wayland, AppKit, and Win32 provide native layout, physical-key,
modifier, dead-key, and IME data to the shared Rust encoder. Main and alternate
screens keep independent flag stacks.

### Kitty Graphics Protocol

cterm accepts bounded Kitty APC graphics commands through the same shared RGBA
pipeline used by Sixel and iTerm2 images. The first implementation tranche
supports direct, regular-file, and secure temporary-file transfers; raw RGB,
RGBA, and PNG images; zlib compression; chunked uploads; support queries;
transmit/display/place/delete actions; cropping, cell scaling, pixel offsets,
quiet replies, storage quotas, cursor suppression, and the complete placement
delete selectors. Signed z-ordering is shared by Cocoa, GTK, and Direct2D,
including Kitty's below-background and below-text layers; image identifiers and
z-indices also survive daemon snapshots. POSIX and Windows named shared-memory
transport supports bounded size/offset reads and protocol-required cleanup.
Animation frame loading, partial-frame edits, alpha/overwrite composition,
client-driven frame selection, and terminal-driven loading/loop playback share
one quota-aware Rust frame store and the existing native event-loop clocks.
Transient `N=1` usage hints propagate through frame composition and prioritize
unplaced short-lived images for eviction under quota pressure.
Unicode placeholders use invisible virtual placements and Kitty's complete
297-diacritic coordinate encoding. Their aspect-preserving RGBA fragments move
with text and scrollback, are cached per viewport, and are rendered consistently
below text on Cocoa, GTK/Wayland, and Direct2D. Relative `P/Q/H/V` placements
follow normal, virtual, or relative parents through named-placement updates;
the bounded graph rejects missing parents, self-reference, cycles, and chains
beyond 32 ancestors, and cascades parent lifetimes as required by Kitty.

### Kitty miscellaneous extensions

Kitty's independent bold/faint resets (`SGR 221`/`222`) and `CSI 22 J`
viewport-to-scrollback operation are supported. Native Cocoa, GTK/Wayland, and
Win32 pointer-leave hooks also emit Kitty's SGR-pixel mouse-leave report.

### Kitty Multiple Cursors

Kitty's multiple-cursors protocol supports block, beam, underline, and
follow-main shapes; main-cursor, point, and rectangular coordinate forms;
full-screen and selective clearing; capability, cursor-state, and color-state
queries; and indexed, RGB, inherited, and reverse-video colors. Extra cursors
share the main cursor's blink phase but remain visible independently of
DECTCEM. GTK/Wayland, Cocoa, and Direct2D render the same overlay state, which
also survives daemon snapshots and incremental screen updates.

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
- **cterm-ui**: UI-agnostic traits plus the deterministic split-pane layout core
- **cterm-app**: Shared application logic between UI implementations
- **cterm-cocoa**: Native macOS implementation using AppKit and CoreGraphics
- **cterm-gtk**: GTK4-specific rendering and widgets (Linux)
- **cterm-win32**: Native Windows implementation using Win32 and Direct2D
- **cterm-headless**: Headless terminal daemon for remote access (ctermd)

Parser, full-screen redraw, Unicode, resize, and scrollback-reflow performance
can be measured locally with `cargo bench -p cterm-core`. GitHub Actions runs
the same deterministic Criterion suite on changes to the benchmarks and on a
weekly schedule, retaining its HTML report as an artifact.

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
- [x] Native split panes (macOS/Linux/Windows)
- [x] Isolated command plugins with native permission prompts

### Future

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
