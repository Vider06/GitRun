# Third-party notices

This file credits third-party tools GitRun can optionally invoke. It does not
cover Rust crate dependencies declared in each crate's `Cargo.toml`, which are
subject to their own published licenses via crates.io.

## zizmor

GitRun's GSR workflow validation can optionally shell out to **zizmor**, a
static analysis tool for GitHub Actions workflows, for deeper checks than
GitRun's own built-in scan covers.

- **Project:** zizmor
- **Author:** William Woodruff and the zizmor project (zizmorcore)
- **Homepage:** https://docs.zizmor.sh
- **Repository:** https://github.com/zizmorcore/zizmor
- **License:** MIT License — https://github.com/zizmorcore/zizmor/blob/main/LICENSE

zizmor is not bundled with GitRun and its source is not modified or
redistributed by this project. It is an entirely optional, off-by-default
integration: GitRun only installs it (via `cargo install zizmor` from
crates.io) after an operator explicitly enables it and accepts zizmor's
license/terms through a consent dialog in the GitRun dashboard. See the
"GSR (GitSecureRun)" section of [README.md](README.md) for what the
integration does.

MIT License terms (for zizmor, reproduced per the license's own
requirements — this is zizmor's license, not GitRun's; GitRun's own license
is in [LICENSE](LICENSE)):

> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to
> deal in the Software without restriction, including without limitation the
> rights to use, copy, modify, merge, publish, distribute, sublicense, and/or
> sell copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in
> all copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
> FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
> DEALINGS IN THE SOFTWARE.

For the authoritative, current license text, always refer to
https://github.com/zizmorcore/zizmor/blob/main/LICENSE rather than this copy.
