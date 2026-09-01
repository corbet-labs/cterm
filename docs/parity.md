# Compatibility and parity inventory

This is the living acceptance ledger for cterm's declared compatibility scope.
A feature is not complete merely because its parser or model exists:

- **Implemented** means the behavior exists in every applicable production backend.
- **Partial** means a useful subset exists but protocol behavior or a backend is missing.
- **Missing** means no production implementation exists.
- **Verified** requires automated behavioral evidence at the layer where users observe it.

Kitty and foot are behavioral references. Their non-Rust implementations are
not copied into cterm. Source adaptations must remain bounded, license-compatible,
and recorded in `THIRD_PARTY_LICENSES.md` at an exact upstream revision.

## Kitty protocol extensions

The authoritative catalog is Kitty's current
[terminal protocol extensions](https://sw.kovidgoyal.net/kitty/protocol-extensions/).

| Extension | Implementation | Current evidence | Outstanding acceptance work |
|---|---|---|---|
| Colored and styled underlines | Implemented | `CellAttributes`, SGR parsing, and native Cocoa/GTK/Direct2D underline renderers | Add pixel-level native visual assertions for style, color, and position |
| Graphics protocol | Implemented, conformance partial | Bounded APC protocol, quota-aware image/placement/animation store, 45 focused tests, and native shared-RGBA rendering | Differential protocol suite and semantic native image assertions |
| Keyboard protocol | Implemented | All five progressive flags, independent screen stacks, and native layout/physical-key inputs | Cross-platform differential fixtures for dead keys, IME, alternate keys, and release events |
| Text sizing | Implemented | OSC 66 model, atomic multicell editing/reflow behavior, snapshots, and native rendering | Pixel-level alignment and fractional-scaling assertions |
| Drag and drop | Partial | Complete OSC 72 vocabulary and local destination drops on GTK/Wayland, Cocoa, and Win32/OLE | Source-side drags, move actions, and remote filesystem requests |
| Multiple cursors | Implemented | Shared shape/color model, queries, snapshots, and native overlays | Pixel-level native overlay assertions |
| File transfer over the TTY | Partial | Kitty-compatible unpadded codec, lossless bounded daemon actor, exact replayable consent tokens, owned deny-default Cocoa/GTK/Win32 prompts with expiry and lifecycle cancellation, relaunch-safe cleanup, and private handle-relative regular-file staging with strict zlib, cumulative limits, metadata, missing-parent creation, and per-file atomic commit | Native prompt interaction automation, receive sessions, directory/link trees, rsync/XXH3, bypass policy, and Kitty differential tests |
| Desktop notifications | Partial | Native delivery and the advertised title/body/focus/close common subset | Activation/close reporting, icons, buttons, sounds, expiry, filtering, and complete capability queries |
| Mouse pointer shapes | Missing | Existing pointer changes are local hyperlink/divider UI behavior, not the protocol | OSC 22 model, stack/query behavior, and native cursor mappings |
| Unscrolling the screen | Missing | `CSI 22 J` viewport-to-scrollback exists, but it is a different extension | Implement and test Kitty `CSI Ps + T` without conflating it with ordinary `CSI Ps T` |
| Color control | Partial | Xterm palette stack plus OSC 4/10-12 dynamic colors | Kitty OSC 21 and OSC 30001/30101, including selection, cursor-text, visual-bell, and transparency keys |
| Arbitrary-region styles/colors | Partial | DEC rectangle operations support a bounded attribute subset | Kitty all-SGR DECCARA semantics, including colors |
| Rich clipboard | Missing | Text-oriented OSC 52 and native text clipboards | OSC 5522 typed MIME data, reads/writes, paste events, consent reuse, queries, and native adapters |
| Miscellaneous extensions | Partial | Independent bold/faint reset, mouse-leave report, and viewport-to-scrollback | No-argument save/restore of safe modes and Kitty private DCS commands |

## foot and terminal-core parity

The audited foot baseline is release `1.27.0` plus current master revision
`765ca4070bb6f095fc58c030f2154ed03857701d`; that exact behavioral-reference
revision is also recorded in `THIRD_PARTY_LICENSES.md`.

| Area | Implementation | Current evidence | Outstanding acceptance work |
|---|---|---|---|
| VT/DEC/xterm core | Substantial | Parser/screen unit tests, real Neovim and tmux sessions, and a byte-for-byte foot probe in CI | Expand the differential corpus across editing, modes, reports, resize/reflow, Unicode, and malformed input |
| Sixel | Implemented | Bounded decoder tests and the shared image store render through Cocoa/CoreGraphics, GTK/Cairo, and Win32/Direct2D | Inject known Sixel frames and compare semantic pixels on every native backend |
| Dynamic colors and palette stack | Implemented | Daemon-owned replies and native palette state tests | Broaden differential color encodings and renderer assertions |
| Rectangular editing | Partial relative to Kitty, implemented for the documented DEC/foot subset | Core behavioral tests cover erase, fill, copy, and bounded attribute changes | Complete the Kitty all-SGR superset tracked above |
| Shell and TUI behavior | Substantial | Hard real-session Neovim/tmux CI and OSC 133 integration | Add more shells, multiplexers, TUIs, keyboard modes, and long-running fuzz/differential workloads |
| Performance | Measured, not gated | Criterion parser/render/reflow/scrollback reports run on relevant changes and weekly | Establish stable hardware-normalized regression thresholds before making performance a hard gate |

## Platform and delivery contract

| Target | Current hard evidence | Open work |
|---|---|---|
| macOS Intel/Apple Silicon | Native builds, library/integration tests, Cocoa UI automation, universal packages | Broader protocol-specific semantic rendering and signed/notarized release evidence |
| Windows x64 | Native builds, library/integration tests, Win32 UI automation, ZIP and NSIS packages | Broader protocol-specific semantic rendering and ARM64 evaluation |
| Linux x86_64/ARM64 | Wayland-only builds, tests, compositor-backed GTK UI smoke, release archives | Distribution-native packages and broader semantic rendering |
| FreeBSD 14.4 | Native library/daemon tests and compositor-backed GTK/Wayland UI smoke | Reproducible release package |
| Software/headless | Headless daemon and core behavioral suites | Keep protocol parity independent of a GPU or window system |
| Android/iOS | Missing, distant target | Keyboard-driven local terminal frontends after desktop parity |

The broader network stack remains intentionally behind local-terminal parity.
It stays in scope, but it is not allowed to delay completion of the rows above.
