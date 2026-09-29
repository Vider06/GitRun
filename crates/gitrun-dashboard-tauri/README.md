# GitRun Dashboard (Tauri)

Provides the current GitRun graphical dashboard as a Tauri 2 app: native
Rust backend (`src-tauri/`, commands in `src-tauri/src/lib.rs`) + a small
vanilla HTML/CSS/JS frontend (`dist/`, no framework, no build step).

**Not yet built or run in this environment** — the sandbox this was written
in has no Rust toolchain and no `cargo`/`tauri-cli` available, so this has
been written carefully against Tauri 2's documented API shape but never
actually compiled. Treat the first build as a real first build, not a
formality — see the handoff notes for what to check first.

## Prerequisites

- Rust toolchain (this workspace targets edition 2021)
- Node.js + npm (for `@tauri-apps/cli`, already in `package.json`)
- Tauri's own system dependencies for your OS — see
  <https://tauri.app/start/prerequisites/> (on Debian/Ubuntu this is
  `libwebkit2gtk-4.1-dev`, `build-essential`, `libssl-dev`, and a few
  others; the exact list has changed across Tauri versions, so check the
  current docs rather than trusting a list written here from memory)

## Development

```bash
cd crates/gitrun-dashboard-tauri
npm install
npm run tauri dev
```

## Building

```bash
npm run tauri build
```

Produces a `.deb` and AppImage per `tauri.conf.json`'s `bundle.targets` —
add other targets there if you need `.rpm`, Windows, or macOS bundles later.

## Runtime configuration

Like the rest of GitRun, this reads `GITRUN_CONFIG_FILE` (or falls back to
`Config::from_env()`) — set it before launching so the dashboard points at
the same config file the scheduler and CLI use:

```bash
GITRUN_CONFIG_FILE=/etc/gitrun/gitrun.env ./gitrun-dashboard-tauri
```

## What's real vs. what's a known gap

See the session handoff document for the full list — in short: every
`#[tauri::command]` in `src-tauri/src/lib.rs` reads/writes real GitRun state
(no mocked data), but this has never been run, so expect a first-build pass
to surface at least minor issues (exact Tauri 2 API signatures can shift
between minor versions; verify against whatever version `npm install`
actually resolves).
