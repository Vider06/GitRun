# Accessibility

GitRun aims to make its operator interfaces usable by people with different access needs. Accessibility is an ongoing engineering responsibility, not a property that can be inferred from a successful build or from the presence of a few ARIA attributes.

This page describes the current interface design, intended environments, known limitations and the preferred way to report barriers. It is not an accessibility conformance report. GitRun does not currently claim conformance to WCAG 2.2 AA or another formal accessibility standard.

## Interface scope

GitRun provides several operator interfaces:

- **Desktop Dashboard:** a Tauri application whose interface is rendered by the operating system's webview. Native accessibility integration and behavior therefore depend partly on the packaged webview and host operating system.
- **Terminal management console:** `gitrun manage`, a keyboard-driven terminal interface built with Ratatui and Crossterm.
- **Command-line interface:** non-interactive and command-oriented workflows that expose text output and can be used without opening the Dashboard or management console.
- **Documentation and repository workflows:** Markdown documentation, GitHub issues and pull requests.

The interfaces provide alternative ways to inspect and operate GitRun, but they are not yet equivalent in every feature or accessibility behavior.

## Current implementation

### Desktop Dashboard

The current Dashboard includes:

- HTML landmarks and named navigation regions for the primary application areas.
- Native buttons, links, form controls and labels in many views.
- Visible keyboard focus styling for buttons, inputs, selects, text areas and links.
- Live announcement regions for parts of navigation, loading feedback and notifications.
- Dialog markup with accessible names in several flows, with focus handling in the shared confirmation dialog.
- Dark and light themes.
- A `prefers-reduced-motion` rule that reduces CSS animation and transition effects when the operating system requests reduced motion.

These are implementation details, not proof that every control or interaction is accessible. Dashboard views render parts of their content dynamically; accessible names, field associations, focus order, announcements and dialog behavior are not yet audited consistently across every view and state.

### Terminal management console

The management console is designed for keyboard operation. The current controls include:

- Arrow keys or `j`/`k` to move through the interface.
- `Tab` to change focus between sections and controls.
- `Enter` to run the selected operation.
- `r` to refresh, `Esc` to go back, and `?` to open help.
- `q` or `Ctrl+C` to exit; number shortcuts jump directly to sections.

See the [CLI management-console guide](crates/gitrun-cli/README.md#terminal-management-console) for the full interaction model.

The console is text-based, but its presentation uses terminal capabilities, Unicode symbols and color. How this is exposed to a screen reader depends on the terminal emulator, operating system and assistive technology combination.

### CLI and documentation

The non-interactive CLI provides an alternative to the graphical and terminal management interfaces for supported operations. Documentation is published as Markdown and should remain navigable using headings, links and ordinary text.

The availability of a CLI command does not mean that every Dashboard or TUI operation has an equivalent command. Consult the command reference and subsystem guides for actual feature boundaries.

## Supported environments

Accessibility behavior is expected to be evaluated only on environments where the corresponding GitRun interface is supported:

- **Dashboard:** use a published desktop package and its supported host environment. Native accessibility APIs, webview behavior, scaling and assistive-technology integration vary by operating system and webview version.
- **Terminal management console:** use a terminal emulator that supports standard keyboard input and the text output required by the console. Screen-reader behavior depends on the terminal and its accessibility integration.
- **CLI:** use a supported GitRun host and a shell/terminal capable of displaying the command output. The CLI can also be used in scripts where an interactive terminal is not appropriate.
- **Documentation:** use GitHub's Markdown renderer or another Markdown reader.

See [Linux platform compatibility](docs/PLATFORM_SUPPORT.md) and the published release notes for the supported host and package boundaries. Platform detection alone does not establish that a desktop package or a particular assistive-technology combination is supported.

## Known limitations and validation status

The following limitations are known and should be considered before relying on a specific accessibility workflow:

- GitRun has not published a complete, repeatable screen-reader test matrix for NVDA, JAWS, VoiceOver, Orca or other assistive technologies.
- Dashboard accessibility has not been audited end to end across every dynamically rendered view, form, table, status update and modal. Some views expose labels and ARIA metadata more consistently than others.
- The current terminal console is a Ratatui interface, not a semantic native widget tree. Screen-reader navigation and announcement quality depend on terminal integration and have not been established for every environment.
- Color contrast has not been measured systematically across both themes, all status colors and every component state. A light theme is available, but it is not a claim of high-contrast conformance.
- Keyboard focus, zoom/reflow behavior and keyboard-only completion of every destructive or privileged workflow have not been formally validated as a complete test suite.
- Reduced-motion styling exists in the Dashboard stylesheet, but all motion and timing behavior across every interface has not been separately audited.

These gaps are not a statement that the interfaces are unusable. They describe what has and has not been verified so that users and maintainers can make informed decisions.

## Reporting an accessibility barrier

Please report accessibility problems through the GitHub issue templates:

- Use **Support / Configuration** for a barrier affecting use of a command, Dashboard view, terminal console or setup flow.
- Use **Repository maintenance** for documentation, issue-template or contributor-workflow accessibility problems.

Include, where possible:

1. The GitRun source or installed release version and the interface involved.
2. Operating system, desktop/webview or terminal emulator, and relevant display settings.
3. Assistive technology and version, if one is being used.
4. The steps that reproduce the barrier, the observed result and the expected result.
5. A sanitized screenshot or short text excerpt when it helps explain the problem.

Do not attach credentials, tokens, private keys, secret values or unredacted runtime configuration. Do not report security vulnerabilities in public issues; follow [SECURITY.md](SECURITY.md) for private vulnerability reporting.

## Accessibility-oriented contribution checks

Changes to user-facing interfaces should consider the following during development and review:

- Complete the relevant flow using only a keyboard, with a visible and logical focus order.
- Give every interactive control a stable accessible name and associate form fields with their labels and help text.
- Ensure dialogs identify their purpose, constrain and restore focus appropriately, and expose validation errors.
- Do not communicate status or meaning through color alone.
- Preserve readable text, meaningful headings, semantic tables and descriptive link text.
- Respect reduced-motion preferences and avoid time-limited interactions where they are not essential.
- Check dynamic loading, success and failure announcements, including behavior when the backend is unavailable.
- State which operating system, webview, terminal and assistive-technology combinations were actually tested. Do not imply broader coverage than was performed.

Accessibility fixes should be scoped to the affected interaction and include regression coverage where practical.
