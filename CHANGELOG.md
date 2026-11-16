# Changelog

## 0.4.1 — 2026-11-16

### Fixed

- Start the background HERDR server directly instead of loading an interactive login shell. A Mac mini migration exposed shell initialization blocking server startup indefinitely. Native Agent sessions still load their login shell and existing settings.
- Keep task instructions on separate readable lines in Agent prompts, with formatted metadata and ordinary quotation marks instead of escaped JSON strings.
- Exclude imported message history from everyday task controls.

## 0.4.0 — 2026-11-16

### Changed

- Run native Agents on demand through HERDR 0.9.5. Messages, tasks and timers wake the Agent; verified completion starts a ten-minute warm period before the process retires. Later work resumes the same CLI's native conversation.
- Replace the former hc/Core execution backend. Existing projects, bot credentials, binding and history migrate once; previous Core executions remain stopped until explicitly started or resumed.
- Embed the real Agent terminal in the native GPUI workspace, with read-only observation and explicit control. Taking control pauses automatic input and retirement until release.
- Keep everyday project, task and schedule controls separate from bot setup, permissions and updates, with persistent English and Simplified Chinese.

### Added

- Durable tasks, events, runs, executions, progress checkpoints, deferred decisions and tracked child jobs shared by the desktop, CLI and MCP.
- Once, interval and timezone-aware cron schedules that survive daemon restarts and wake sleeping Agents without overlapping pending occurrences.
- Actionable login, permission and unfinished-work notices, plus explicit recovery for uncertain operations.

### Fixed

- Use public HERDR pane targets for native prompt delivery and verified native session references for resume.
- Handle current Claude Code workspace trust and Codex hook trust without manual startup confirmation, using only supported native flags and verified project prompts.
- Wait for Pi's direct MCP tools before the first task, preserve proxy/login-shell settings, and distinguish unsent prompts from uncertain submissions.
- Preserve an execution during transient process observations and restart the warm period after blocked or disconnected observations.

### Validation

- Local backend and desktop tests, clippy, and 16 isolated protocol/lifecycle smoke checks pass.
- Real Codex, Claude Code and Pi task/retirement/same-session resume flows pass. Authenticated agy end-to-end acceptance and visible desktop IME/selection/clipboard acceptance remain open in #23 and #25.

## 0.3.4 — 2026-11-16

### Changed

- Open existing projects directly in a compact native workspace, with setup separated from everyday project actions.
- Add an immediate, persistent English and Simplified Chinese language switch without resetting setup progress or form input.
- Remove the local task composer, message inspector, message history, and repeated message-detail polling.
- Keep scrolling independent between pages so navigation does not clip the next view.

### Fixed

- Keep persistent main sessions visible in the local Herdr App when the agent is still starting, the app opens later, or a mirrored machine is selected.
- Reuse and focus the correct local pane on repeated monitoring requests, preserving the existing agent process and session.
- Reconcile late adoption and reconnect events without restoring an unrelated shell into the owned main session.

## 0.3.3 — 2026-11-16

### Fixed

- Start native agents through a login shell inside HERDR, loading the same environment as a user's terminal. Finder-launched sessions previously missed shell exports, leaving Antigravity with an empty model list and an unspecified plan-model error on networks that require a proxy.
- Preserve CLI arguments without shell interpolation, native model defaults, and HERDR terminal identity. Refuse delivery while the foreground process is still a shell.

### Changed

- Stop automatic Actions runs on pushes and pull requests; retain full native checks as an explicit manual workflow.
- Require locally built and tested release packages. Reject missing or mismatched staging metadata on Ubuntu before starting macOS signing, with no cloud Rust compilation fallback.
- Limit manual native checks to 30 minutes, cancel superseded manual runs for the same ref, and retain notarization receipts for seven days.

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
