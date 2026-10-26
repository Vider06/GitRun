# `scripts`

Installation, release-build and verification scripts for Linux, macOS and Windows. Scripts are operational tooling rather than part of the Rust runtime. Check installer and release behavior against the current source/product version in `version.txt` (currently <!-- gitrun-version:start -->1.8.4<!-- gitrun-version:end --> on this source branch). The 1.7.0 Kitty queue-evaluation work is documented in [the Smart Scheduler guide](../docs/SMART_SCHEDULER.md); the 1.6.2 dashboard settings recovery and earlier legacy runtime migration remain historical records in [the consolidated release history](../docs/RELEASE_HISTORY.md). Historical verification limits are summarized in [release history](../docs/RELEASE_HISTORY.md); they do not validate a new installer or release commit.

## Current Markdown version markers

`sync-markdown-version.py` keeps the visible current-source version labels synchronized with `version.txt`. GitHub renders Markdown but does not expand arbitrary environment variables, so the script updates values stored inside invisible HTML comments rather than leaving placeholders visible to readers.

- Run `python3 scripts/sync-markdown-version.py` to update the marked values from `version.txt`.
- Run `python3 scripts/sync-markdown-version.py --check` in CI to detect stale or missing markers.
- The release workflow refreshes the markers before packaging and persists the correction to `main` only when the tagged source commit is still the current main head. Historical release numbers outside the markers are intentionally preserved.
