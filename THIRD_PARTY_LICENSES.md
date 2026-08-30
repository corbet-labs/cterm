# Third-party source notices

cterm uses dependencies distributed through Cargo as well as the source-derived
components listed here. The original notices are preserved in the derived files.

| Component | Upstream revision | cterm use | License |
|---|---|---|---|
| KarpelesLab/cterm | `35ceaeccf3401de02b15037fc6e04a7e8a26aa83` | Fork base for the terminal runtime and native frontends | MIT |
| Rio / Sugarloaf | `357281638216876a2406c46e50033e7143256175` | Adapted Rust rasterizers for box drawing, block elements, braille, sextants, and octants in `cterm-ui`; adapted XTGETTCAP parsing, DEC Special Graphics and modifyOtherKeys encoding, and tests in `cterm-core` | MIT |
| Alacritty VTE | `89c12df969145ffb5084d1122627d7292c2c638f` (`vte` 0.13.1) | Adapted tested SGR parameter grouping and extended-color validation in `cterm-core` | MIT |
| foot | `765ca4070bb6f095fc58c030f2154ed03857701d` (1.27.0) | Behavioral reference and adapted DEC rectangular-editing and xterm palette-stack semantics, validation and tests in `cterm-core` | MIT |

The KarpelesLab/cterm source incorporated at the fork base retains its original
MIT grant and notice in `LICENSES/KARPELESLAB-CTERM-MIT.txt`. Subsequent cterm
contributions are provided under FSL-1.1-ALv2; see `LICENSE`.

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
