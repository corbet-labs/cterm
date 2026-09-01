# Third-party source notices

cterm uses dependencies distributed through Cargo as well as the source-derived
components listed here. The original notices are preserved in the derived files.

| Component | Upstream revision | cterm use | License |
|---|---|---|---|
| KarpelesLab/cterm | `35ceaeccf3401de02b15037fc6e04a7e8a26aa83` | Fork base for the terminal runtime and native frontends | MIT |
| Rio / Sugarloaf | `357281638216876a2406c46e50033e7143256175`; `932c1a7d9e07b4db5924f7a0dd689e823c3a1442` | Adapted Rust rasterizers for box drawing, block elements, braille, sextants, and octants in `cterm-ui`; adapted XTGETTCAP parsing, DEC Special Graphics, modifyOtherKeys encoding, and tests in `cterm-core`; SGR 5/6 slow/rapid blink distinction informed the shared blink policy | MIT |
| rio-vt-benchmark | `a49a7062c964034d5192032fe8e18fb7e262dbec` | Workload taxonomy and comparison methodology used to inform original Criterion benchmarks in `cterm-core`; no source code copied | MIT (declared in upstream README) |
| Alacritty VTE | `89c12df969145ffb5084d1122627d7292c2c638f` (`vte` 0.13.1) | Adapted tested SGR parameter grouping and extended-color validation in `cterm-core` | MIT |
| foot | `765ca4070bb6f095fc58c030f2154ed03857701d` (1.27.0) | Behavioral reference and adapted DEC rectangular-editing, xterm palette-stack, Sixel aspect/palette/resource-management, theme/visibility-reporting, and cursor/cell blink source, timing and rearm semantics, validation and tests in `cterm-core` and `cterm-ui` | MIT |
| Zellij | `e839bfffa586992364309a685b2c71f3b23c247e` | Adapted all-or-none bounded control-string interception and split/recovery test structure for streamed OSC 1337, plus the Kitty graphics command vocabulary, chunk state, and error model in `cterm-core` | MIT |
| Noa | `8d843ce352e2f10ef1c130bcf7f94198f1ccaca6` | Adapted the tested Rust full-canvas `Arc` frame-store, quota-accounting, monotonic animation-tick structure, Unicode-placeholder scanner, and 297-entry diacritic table; cterm's command mapping, playback, placeholder inference, aspect ratio, and layer behavior are corrected against Kitty's specification and reference implementation | MIT |
| Qwertty Term / Ghostty | `9021f511bf053ec4155298e43a65de4365a13f80`; `2da015cd6` | Adapted Qwertty Term's Rust port of Ghostty's complete Kitty OSC 72 command vocabulary and parser test matrix in `cterm-core`; cterm adds strict integer, duplicate-key, terminator, chunk-chain, and payload bounds | MIT |
| Yazi | `a73d235678db3a070b7e013ccf9573bf45a5324f` | Adapted Kitty OSC 72 chunk-state, bounded wire-framing, and MIME-payload test cases in `cterm-core` and `cterm-app` | MIT |
| Elio | `3a39678609e927ea4f248a3cf40d7dbf353260fe` | Behavioral reference for tested Kitty OSC 72 client sequencing, local URI-list negotiation, and chunk boundaries; no source copied | MIT |
| Tao | `2f9eecf236f4f6a8acfa03329c57039224a3ce99` | Adapted the tested Rust `IDropTarget`, `CF_HDROP`, dynamically sized path extraction, and OLE registration lifecycle for the Win32 Kitty OSC 72 adapter | Apache-2.0 |
| Baseview | `c36ca154f882353f04684973dfe683c1b3d6abb3` | Behavioral reference for client-coordinate drag movement and retaining parsed file data across Win32 drag callbacks; no source copied | MIT OR Apache-2.0 |
| `stretch` | `20e0748c15ceb0695bd2ebb821a8eee7364f3c8d` (`0.3.2`) | Transitive Rust flexbox dependency of `native-windows-gui`; the published crate omits its declared license file, so cterm pins a reviewed cargo-deny clarification and ships the upstream notice | MIT |
| `fs_at` | `e8b58a0682496a0c6ddc9eae80942a2f29a5a7e4` (`0.2.1`) | Cargo dependency providing tested handle-relative file creation and cleanup through `openat` on Unix and `NtCreateFile` on Windows for OSC 5113 staging; cterm adds only the final platform rename operation | Apache-2.0 |
| Microsoft OpenVMM | `ef54fd16f6449c51efe62ea46ddcfabd9e9cd589` | Adapted the tested Rust `NtSetInformationFile` buffer construction, retained-root rename, and NTSTATUS conversion for atomic OSC 5113 commits on Windows | MIT |
| `shared_memory-rs` | `68563b3aa82b832dfb73b18a59f4db34ff182df2` (`0.12.4`) | Cargo dependency providing tested Windows named-mapping lifecycle for Kitty shared-memory transfers; no source copied | MIT OR Apache-2.0 |
| `nix` | `9cd968a1af35b46b05ed41e05acfcca5d02a5645` (`0.31.3`) | Cargo dependency providing safe POSIX `shm_open`, `shm_unlink`, descriptor ownership, staged-file identity checks, and handle-relative `renameat` commit on Linux, macOS, and FreeBSD; no source copied | MIT |
| `memmap2` | `7710019665fec7bdac1dc18cf6661fbe215a1ad2` (`0.9.11`) | Cargo dependency providing tested read-only POSIX mapping for Kitty shared-memory payload snapshots; no source copied | MIT OR Apache-2.0 |
| Wasmi / `wasmi_wasi` | `c517895c2db09f660d6eae0bc4549861ab8fd88f` (`v1.1.0`) | Pinned Cargo runtime and WASIp1 adapter used only by the isolated `cterm-plugin-host`; no source copied | MIT OR Apache-2.0 (cterm distributes under the MIT option) |
| Bytecode Alliance `wasi-common` | `3d0ec7e7c5ae4cd3f9b99d915276926d799b9a2b` (`v36.0.14`, constrained to the patched 36.x line) | Directly constrained transitive WASIp1 context and bounded virtual pipes for `cterm-plugin-host`; no source copied | Apache-2.0 WITH LLVM-exception |
| `process-wrap` | `c8d6b1faa1dc54723e11df5f2026e61e05e93950` (`v10.0.0`) | Pinned Cargo process-group and Windows Job Object lifecycle used by the application plugin broker; no source copied | Apache-2.0 OR MIT, with identified Windows routines Apache-2.0-only |

The KarpelesLab/cterm source incorporated at the fork base retains its original
MIT grant and notice in `LICENSES/KARPELESLAB-CTERM-MIT.txt`. Subsequent cterm
contributions are provided under FSL-1.1-ALv2; see `LICENSE`.

The selected Wasmi MIT notice is preserved in `LICENSES/WASMI-MIT.txt`.
The standard Apache-2.0 terms covering Tao and `fs_at` are preserved in
`LICENSES/WASI-COMMON-APACHE-2.0-WITH-LLVM-EXCEPTION.txt`; the exception at the
end applies only to `wasi-common`.
The `process-wrap` provenance notice and MIT option are preserved in
`LICENSES/PROCESS-WRAP-COPYRIGHT.txt` and `LICENSES/PROCESS-WRAP-MIT.txt`; the
full Apache-2.0 terms are also present in the `wasi-common` license file.
The MIT notice omitted from the published `stretch` 0.3.2 crate is preserved in
`LICENSES/STRETCH-MIT.txt`.

## Rio / Sugarloaf MIT license

Copyright (c) 2022-present Raphael Amorim

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## foot MIT license

Copyright (c) 2019 Daniel Eklöf

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## Alacritty VTE MIT license

Copyright (c) 2016 Joe Wilm

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## Zellij MIT license

Copyright (c) 2020 Zellij contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## Qwertty Term MIT license

Copyright (c) 2026 Josh McKinney
Copyright (c) 2024 Mitchell Hashimoto, Ghostty contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## Yazi MIT license

Copyright (c) 2023 - sxyazi

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## Elio MIT license

Copyright (c) 2026 Miguel Regueiro

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## Microsoft OpenVMM MIT license

Copyright (c) Microsoft Corporation.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
