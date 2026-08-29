# Third-party source notices

cterm uses dependencies distributed through Cargo as well as the source-derived
components listed here. The original notices are preserved in the derived files.

| Component | Upstream revision | cterm use | License |
|---|---|---|---|
| KarpelesLab/cterm | `35ceaeccf3401de02b15037fc6e04a7e8a26aa83` | Fork base for the terminal runtime and native frontends | MIT |
| Rio / Sugarloaf | `357281638216876a2406c46e50033e7143256175` | Adapted Rust rasterizers for box drawing, block elements, braille, sextants, and octants in `cterm-ui`; adapted XTGETTCAP parsing, DEC Special Graphics and modifyOtherKeys encoding, and tests in `cterm-core` | MIT |

The KarpelesLab/cterm source incorporated at the fork base retains its original
MIT grant and notice in `LICENSES/KARPELESLAB-CTERM-MIT.txt`. Subsequent cterm
contributions are provided under FSL-1.1-ALv2; see `LICENSE`.

foot (`85655c74a4ded119392ea8b632626c3920042807`, MIT) is the behavioral reference
for the Linux terminal experience. No foot source is included by this revision.

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
