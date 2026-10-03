# GitRun Dashboard (Tauri)

Provides the current GitRun graphical dashboard as a Tauri 2 app: native
Rust backend (`src-tauri/`, commands in `src-tauri/src/lib.rs`) + a small
vanilla HTML/CSS/JS frontend (`dist/`, no framework, no build step).

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

## First-run setup

When no GitRun configuration exists, the dashboard opens its graphical first-run
wizard. The wizard supports both GitHub PAT and GitHub App authentication, lets
you configure the repository list, and asks PolicyKit to run the existing privileged
GitRun setup path. When a PAT is used, it is passed through a mode-0600 temporary
request file and removed after setup completes. For GitHub App authentication, the
wizard stores only the App ID, installation ID, and PEM file path; on Linux the PEM
file must be a regular, non-symlink file with owner-only permissions (0600). The
private key itself is never copied into the dashboard setup request. GitHub App
authentication is also available through `gitrun setup --terminal`.

## Runtime

All dashboard commands read and write live GitRun state. There is no mock data or
HTTP server; the frontend uses Tauri IPC directly against the Rust backend.
