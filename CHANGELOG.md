# Changelog

## 0.3.2 — 2026-11-15

### Fixed

- Sign macOS releases with Developer ID Application and complete Apple notarization so downloaded apps pass Gatekeeper.
- Staple and validate the notarization ticket before packaging; stop publication if Apple acceptance or Gatekeeper verification fails.
- Preserve the existing update signing key, application identifier, local project data, and HERDR sessions.

### Added

- Stage a locally built and tested macOS package for CI signing and notarization, avoiding a second Rust compilation without exporting signing credentials.

## 0.3.1 — 2026-11-15

### Changed

- Adopt English for project documentation, repository metadata, native desktop copy, CLI help, errors, MCP instructions, commit messages, and pull requests.
- Remove the legacy Svelte interface, browser tests, embedded web assets, unused web-only endpoints and refresh notifications, and JavaScript build dependencies. The desktop and release builds now use Rust directly.
- Make `hfb ui` open the native desktop application.
- Remove the unused group-discovery permission from onboarding; incoming private and group messages still reach the bot's fixed project.

### Fixed

- Resolve draft releases by numeric ID before validating uploaded asset digests. GitHub's release-by-tag endpoint returned 404 during the first signing run, so 0.3.0 remained a draft.

## 0.3.0 — 2026-11-15

### Added

- Native App Updates page and `hfb update` commands for checks, downloads, installation, channels, restart, and rollback.
- Automatic checks at startup and every six hours while the desktop is open. Background replacement takes effect on the next launch.
- Ed25519 manifest signatures, SHA-256, Apple Team, bundle ID, version, and architecture checks, with restricted extraction and same-volume replacement.
- Tag-triggered releases for commits on main. Publish only after complete assets pass checksum validation.

### Fixed

- Clear stale local-service errors after reconnection and restart an exited daemon automatically.
- Bypass external proxies for local desktop, CLI, and MCP control requests; support existing proxies for external update downloads.
- Preserve project data, HERDR terminals, and MCP sessions when changing daemon versions.
- Label independent data directories in the window title and page so test instances cannot be mistaken for missing user data.

Version 0.2.0 requires one manual installation of an updater-enabled release. Packages use Apple Development signing and have not completed Apple notarization. The first 0.3.0 CI run built and signed successfully but stopped at draft publication; 0.3.1 fixes that step.

## 0.2.0 — 2026-11-15

Initial open-source preview under the MIT license.

- Native Rust/GPUI macOS app with Feishu bot setup, credential validation, and selected-path permission checks.
- Independent Rust daemon, CLI, and MCP server; the daemon keeps working when the desktop closes.
- One Feishu bot bound to one project with a switchable active CLI. Private and group messages share the same main session.
- HERDR-hosted Codex, Claude Code, Pi, and Antigravity sessions, visible in Herdr App.
- Markdown and file tools with explicit recipients, delivery deduplication, and preserved uncertain results.
- Fixed initial message routing, inherited proxy settings, group-only XML metadata in direct messages, and incompatible native session settings after CLI switches.

The visible name changed to Dauerbot while the internal executable names, bundle ID, and data directory stayed compatible. Apple Silicon preview only; Intel and minimum macOS compatibility remain unverified.
