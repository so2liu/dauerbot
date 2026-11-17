# Changelog

## Unreleased

### Changed

- Advance the daemon contract to API protocol 7 so the desktop cannot silently reuse an API 6 service without managed runtime and child-aware retirement semantics. Existing incompatible native executions require the established explicit recovery action.

### Fixed

- Require fresh, generation- and native-session-matched child coverage before idle handoff retirement. Child activity, revision changes and unknown coverage reset eligibility and cancel pending handoff; a shared native dispatch gate fences the final stop. Unsupported CLI child observation retains sessions instead of inferring safety from parent idle (#57).

### Added

- Load the pinned official Pi subprocess subagent extension explicitly for managed Pi 1.5.1 sessions, preserving global plugins, authentication and selected models. Native same-CLI delegation is preferred, with Herdr for cross-CLI/model work. External plugins or incomplete discovery make retirement coverage unknown; the foreground child tool can delay the root model's response to steering (#57).

## 0.4.6 — 2026-11-17

### Fixed

- Keep Agent-internal follow-ups and exhausted one-shots out of the desktop schedule list and count. Preserve paused future reminders, legacy unclassified schedules, execution history and creation retry IDs; classify new scheduled work through explicit purpose metadata (#54).
- Automatically collect obsolete retained hfb companions in the background. Preserve active executions, persistent managed MCP commands, running binaries and the exact signed rollback companion; retired session archives no longer retain old executables (#52).
- Retain each daemon's hfb companion at a private version, protocol and SHA-256 pinned path. App replacement cannot change new or recovered Agent commands. Historical daemons can use their original signed companion from the updater's preserved bundle; missing backups report `update_required` (#46).
- Hold input for incompatible live Agent generations after a service handoff and offer an explicit project recovery action. Preserve native continuity, queued messages, uncertain effects and unrelated terminals; compatible generations remain running (#46).

### Added

- Bundle the supported HERDR 0.9.5 runtime in the native app and provision it offline when no supported runtime is installed. Preserve explicit runtime choices and expose the app-owned runtime to Agent tools (#47).
- Create private project folders under `~/.dauerbot/agents` by default, with concise `AGENTS.md` instructions. Keep existing-folder selection and preserve user instructions (#48).

### Changed

- Request an updated `HANDOFF.md` after ten verified native idle minutes. Require the current request marker, a changed artifact and a later idle transition before retirement, then wake into a fresh session that reads project instructions and the handoff. New input cancels a pending handoff and steers the live Agent (#48).
- Keep native recovery after handoff failures, interrupted execution and uncertain delivery. Bound housekeeping prompts and preserve terminal takeover, pauses and scheduling (#48).
- Advance the app and daemon to 0.4.6, bundle build 11 and API protocol 6. An API 5 background service cannot satisfy the new workspace/handoff contract; replacement retains the trusted-install and downgrade checks.

## 0.4.5 — 2026-11-16

### Changed

- Make the macOS menu bar the primary entry, with a project-status panel and Add Project, Open App and Settings actions. Closing the window keeps the utility resident. The workspace shows projects and scheduled runs with a collapsed terminal and explicit input takeover (#43).
- Reuse an authenticated daemon across desktop updates when its API protocol remains compatible. Show the service version and provide a separate trusted-install-only update action that rejects downgrades and preserves native Agent sessions (#42).
- Restore Enter and Tab handling in the native macOS terminal while preserving printable text and Chinese IME input (#40).
- Reduce managed Agent MCP to `send_md`, `send_file` and `schedule_run`; remove task, progress, decision and background-job bookkeeping from bootstrap, API, CLI and desktop. Questions use ordinary Feishu messages and replies.
- Drive dispatch and ten-minute idle retirement through native HERDR state and verified execution identity. Preserve uncertain input and outbound results without automatic replay.
- Keep stored schedules, messages and timer controls through schema 7 upgrades. Mark the incompatible contract with API protocol 5 so desktop, CLI and MCP reject incompatible services.

## 0.4.4 — 2026-11-16

### Fixed

- Automatically wake projects for incoming messages after their previous Agent process stops. Remove the persistent stopped-project switch and migrate existing stopped projects without losing bot bindings, native conversations or explicit pauses.
- Remove routine Start/Resume/Stop buttons from the everyday workspace. Show standby and automatic wakeup clearly in English and Simplified Chinese; retain an explicit pause control.
- Explain that interrupted old work does not block new conversation messages. Stop and remove projects under the same wakeup lock so deletion cannot race automatic startup.
- Keep interrupted history recorded without pinning a new execution awake or blocking independent scheduled work.

## 0.4.3 — 2026-11-16

### Fixed

- Send incoming conversation messages to the active Agent immediately, including while it is working or an earlier run is unfinished. Use native steering submission instead of waiting for the next task.
- Keep fresh user direction deliverable after an uncertain earlier input without replaying that input. Preserve explicit delivery pause, terminal takeover, login prompts and permission decisions.
- Drain message bursts in order while retaining independent receipts and completion records. Standalone tasks and timers remain serialized.

## 0.4.2 — 2026-11-16

### Changed

- Give every project its own Feishu bot credentials, event connection, token cache and connection status. Reject attempts to bind the same App ID to two projects.
- Open bot setup after creating a project, with an explicit local-only option. Show Connect/Manage Bot in each project's workspace, and keep credential drafts and setup confirmations separate.
- Migrate the former global bot to its recorded project owner without assigning it to other projects. Scope chat context by project, including when two bots share a group.
- Complete the two-device Core transition and remove its execution archive, migration notices and unused configuration. Existing project data, tasks, schedules and native conversations remain in the current store.

### Fixed

- Register Antigravity CLI's MCP server in its supported global configuration. Each invocation resolves the active project and generation from verified HERDR identity; ordinary CLI sessions receive no project tools.
- Start managed native sessions without SSH connection markers that make agy ignore the Mac's existing keychain login. Keep native model, proxy and SSH agent settings.

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
