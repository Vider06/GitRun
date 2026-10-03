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

The zizmor license text is not reproduced here because GitRun does not
redistribute zizmor. For the authoritative, current license text and copyright
notice, refer to the upstream license:

https://github.com/zizmorcore/zizmor/blob/main/LICENSE
