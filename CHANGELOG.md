# Changelog

## Unreleased

### Fixed

- Keep signed updates from getting stuck on a stale or incompatible Agent MCP record. After handover, automatically verify and recover only the affected Agent, preserving its saved session and pending input without touching unidentified terminals.

## 0.6.18 — 2026-11-23

### Changed

- Migrate Manager Langfuse OTLP export to ingestion v4 with per-observation session context, single-trace turn attribution across batched events, worker/timer/proposal wake and compaction telemetry, full opt-in debug content preserving verified base64 images, and narrower credential redaction that keeps benign prose and fail-closed `Bearer`/`Basic` scrubbing intact. (#183, #185)
- Wrap Manager inbound events and synced project memory in structured XML envelopes (`<dauerbot_event>` and `<project_memory>`) mirrored verbatim to Langfuse debug telemetry. (#183)
- Remove host-side Manager file path denylists, workspace containment gates, and artificial tool/turn ceilings in favor of prompt-guided context and chunked `read_file` pagination (`offset`/`limit`), while preserving the finite non-shell tool allowlist, separate Manager credentials, and host authority boundaries. (#183)
- Improve Manager context and autonomy: newest-first 16 MiB image hydration budget, symlinked `AGENTS.md` loading, head-memory-preserving compaction, structured proposal schema validation without keyword blocklists, strict per-tool input schemas, and legacy primary `steer_worker` support. (#185)

### Fixed

- Fence Manager admission, tool execution, and delivery against `auto`-project takeover, fix attachment-readiness steering interruption, scope `close_agent`/`cli_send_keys` uncertainty per target, recover stranded in-flight handover records on the authorized incoming daemon without schema changes, surface persistent Manager provider failures with backoff, release `project_lock` during file/CLI probes while serializing `write_file` authority checks, and classify Feishu delivery errors cleanly. (#184)
- Fix `send_file` topic routing metadata and false `"error": ""` tool error flags, clean up definitively failed proposal cards before retry, and preserve Desktop Manager settings inputs when Reload fails. (#183, #185)

## 0.6.17 — 2026-11-22

### Changed

- Replace the external Pi Manager bridge and stateless history truncation with a pure-Rust Manager built on `rig-core`, maintaining durable append-only per-channel transcripts for LLM prompt-cache stability and token-aware canonical compaction. (#181)
- Sync shared `AGENTS.md` project memory across channels on demand via content hash watermarks without waking idle channels or mutating earlier history. (#181)
- Support OpenAI Chat Completions, OpenAI Responses (`store: false`), and Anthropic Messages in Manager settings with optional context-window overrides, fast steer cancellation, gate-held detached tool settlement, and explicit host-validated `source_event_id` delegation. (#181)

## 0.6.16 — 2026-11-22

### Added

- Let managed projects opt in to a restricted Pi runtime with nine host-authorized tools and separate Manager provider credentials. (#178)
- Resume the Pi Manager conversation per trusted discussion and let Pi compact its native history. The daemon retains input ownership and effect receipts. (#179)

### Fixed

- Scope Manager history sampling to the active Feishu discussion while preserving fresh steering. (#177)

## 0.6.15 — 2026-11-22

### Fixed

- Keep source-backed user decisions available after recent conversation history rolls over. The Manager distinguishes current decisions, historical references, and superseded decisions by discussion and work branch. Delegations carry the selected branch's constraints and source references into the worker task. (#175)

## 0.6.14 — 2026-11-22

### Fixed

- Let Manager run multiple independent workers of the same CLI within a project. The default has no application concurrency cap. An optional per-project, per-CLI cap queues assignments until a worker can take them. Workers share the project directory. (#172)
- Give Agents readable Feishu attachment metadata, verified cache paths or failure reasons, and an optional lark-cli download command for an authorized profile. Cache files, audio, and video from the current or directly quoted message. With ffmpeg and ffprobe available, include a video preview frame at 20% of its duration. Image-capable Manager models receive it as an image, and native terminals receive its local path. (#173)

## 0.6.13 — 2026-11-22

### Added

- Include the quoted parent of a Feishu reply in the Agent context. A bounded background lookup reads the direct parent and marks its content as untrusted reference data. Failed lookups report the reference as unavailable and let delivery continue. The current message and the inputs used for admission remain separate from the quote. (#170)

### Fixed

- Prevent Manager telemetry from panicking when a historical tool call has no path argument. (#169)

## 0.6.12 — 2026-11-21

### Fixed

- Remove telemetry length cutoffs for eligible Manager conversation text sent to Langfuse. Keep credential filtering and omit structured input that cannot be safely inspected. Content export remains opt-in. (#166)

## 0.6.11 — 2026-11-21

### Added

- Add the advanced, opt-in `include_content` setting for Manager Langfuse traces. It exports redacted user messages, conversational request text and assistant replies. Tool spans include selected arguments and outcome categories. Content capture defaults to off. Known credential patterns are filtered, but arbitrary unmarked secrets cannot be reliably identified. (#164)

## 0.6.10 — 2026-11-21

### Added

- Add optional Langfuse observability for the built-in Manager. Configure it in the daemon data directory to inspect interaction, model and tool timing and usage. Telemetry is off by default and excludes conversation text. (#162)

## 0.6.9 — 2026-11-21

### Changed

- Instruct the built-in Manager to acknowledge each new user message with one short, separate reply before continuing work. The Manager can use its source-scoped `send_md` receipt intent for this reply. Existing projects keep their execution mode. (#159)

### Fixed

- Run the CLI-switch smoke fixture in native compatibility mode and verify that held messages and the fired timer are delivered only to the ready target. (#159)

## 0.6.8 — 2026-11-21

### Changed

- New projects use the built-in Manager when the shared provider is configured. It can assign separate tasks to different terminal CLI workers, inspect the detected CLIs, and honor an explicit per-task CLI choice. New workers need no Dauerbot MCP registration. (#157)
- Existing projects keep their execution mode. Native compatibility remains available for projects created without a configured Manager provider. API protocol 7 and the native three-tool MCP interface are unchanged.

### Fixed

- Bound Manager terminal polling and keep the project available after a turn budget is exhausted. Add supervised recovery for exhausted CLI transitions while preserving pending and uncertain work. (#156)

## 0.6.7 — 2026-11-20

### Added

- View observed Feishu conversations, message closure and activity trends in the desktop Activity panel. (#153)
- Let the manager propose scoped project improvements for owner approval. Proposals are disabled by default and enabled per project. Approve or reject in the proposal discussion. (#154)

### Fixed

- Keep accepted inputs through Stop, resume attachment preparation explicitly, and scope manager replies to their work and destination. Preserve group context and hold queued work when shared provider migration requires confirmation. (#152)

## 0.6.6 — 2026-11-20

### Changed

- Keep manager replies brief and conclusion-first for IM conversations, with detail when requested and honest reporting of blockers. (#149)
- Express desktop actions through icons and a compact project toolbar, schedule rows, Agent tree and Terminal controls. Retain native CLI brands and explicit input takeover. (#150)
- Advance both application crates to 0.6.6 and bundle build 24; keep API protocol 7 and the native three-tool interface.

### Fixed

- Bound long Agent names before the fixed sidebar status column, including nested read-only details and navigable child rows. (#150)
- Match menu status to the displayed manager or native Agent, visibly dim disabled icons, and label unsupported model selection accurately. (#150)

## 0.6.5 — 2026-11-19

### Added

- Select, search and navigate native model choices with the keyboard. Keep model-list scrolling inside the picker, including at its boundaries. (#137, #139)
- Resize the sidebar and retain its width; choose light, dark or system appearance, and use Cmd+K to find projects, sessions and settings. Align Agent tree disclosure and status columns. (#139, #140, #141)
- Let managers explicitly select a supported CLI for delegation or steering through the existing cooperative handoff and delivery lock. Omitting the target preserves the current CLI. (#142)
- Give managers scoped terminal text/key sequences and read-only screen inspection, while retaining generation, pause, Stop and takeover guards. (#143)
- Inspect recorded manager model context in a read-only desktop panel, with bounded output and redaction of sensitive content. (#144)

### Fixed

- Drain an active manager step to a checkpoint boundary before upgrade handover. Defer activation and reopen admission when the bounded precommit drain cannot finish, preserving the active service and native worker. (#135)
- Supervise native runtime observation failures after a completed CLI switch without replaying repair effects. Refresh model selection controls and distinguish requested models from observed models. (#136)
- Make the native metadata test fixture independent of PATH-provided interpreters. (#138)

### Changed

- Advance both application crates to 0.6.5 and bundle build 23; keep API protocol 7 and the native three-tool interface.

## 0.6.4 — 2026-11-19

### Added

- Declared image-capable manager models receive verified image content from Feishu attachments. Enable image support for the exact endpoint and model in manager settings. Unsupported inputs and native-session transports that cannot attach pixels retain explicit file references and an explanation. (#128, #130)
- Managers can explicitly close verified-exited obsolete owned executions after checking completion, runtime ownership, remaining processes, pending work and uncertain effects. Each decision is audited; no automatic reaper is added. (#125, #127)

### Fixed

- Persist input ownership when a manager claims work. Delivery receipts no longer restart the active batch or replay its input, and interrupted work resumes from its saved checkpoint. (#124, #129)
- Keep Stop and takeover responsive while historical runtime inspection stalls. Bound the entire inspection and reject stale observations. (#131)
- Preserve attachment preparation when manager routing acknowledges a message before its download completes. (#130)
- Preserve an image declaration when pressing Enter or focusing an unchanged provider field. (#130)
- Allow stopped projects to be removed after management close attempts, preserving audit records and notes throughout schema upgrades. (#132)
- Allow obsolete executions to become eligible after delivery retries or explicit dismissals are resolved, while keeping pending and uncertain work protected. (#132)
- Retain every live or ambiguous shell, including builtin-only work, and close only independently verified exited targets with no remaining attributable processes. (#133)

### Changed

- Acknowledge sustained work briefly, keep related tasks together by default, and report results or blockers without volunteering internal execution details. Explicit architecture/model questions remain answerable. (#122, #126)
- Require review/check closure and automated review evidence for the current PR head. (#131)
- Advance both application crates to 0.6.4 and bundle build 22; keep API protocol 7 and the native three-tool interface.

## 0.6.3 — 2026-11-19

### Fixed

- Omit output-token limit parameters entirely from management model requests, including follow-up and corrected requests. Ignore retired caps in saved checkpoints instead of applying or restoring them. Provider-ended length responses remain bounded failures and cannot execute partial or sibling tools.
- Classify incomplete or invalid management responses before tool execution; allow one corrective request for invalid JSON or tools, then retain pending input and effect records and prepare a bounded native repair assignment. Preserve the 120-second request timeout and immediate cancellation (#116).
- Keep sanitized failure and correction diagnostics, incident-scoped notices and exponential retry backoff. Clear stale errors only after a valid response, keep repair results reachable through one fresh probe, and honor pause, Stop, takeover and uncertain effects. Localize management errors and Chinese-source notices (#116).

### Changed

- Advance both application crates to 0.6.3 and bundle build 21; keep API protocol 7 and the existing native tool interfaces unchanged.

## 0.6.2 — 2026-11-19

### Fixed

- Restore automatic upgrade handover from 0.5.x by canonicalizing configuration fingerprints across JSON map-ordering changes; retain the 0.6.0 predecessor encoding and specific service failure diagnostics (#113). This includes the upgrade fixes from 0.6.1.
- Use an Intel macOS runner for signing and notarizing the locally staged Apple Silicon package when hosted ARM runners are unavailable. Keep native runtime execution in mandatory local ARM staging; signing CI verifies payload metadata, SHA-256, size, Mach-O architecture, attribution and code signatures without compiling or executing ARM binaries.

### Changed

- Advance both application crates to 0.6.2 and bundle build 20; keep API protocol 7.

## 0.6.1 — 2026-11-19

### Fixed

- Restore automatic upgrade handover from 0.5.x by canonicalizing the cross-version configuration fingerprint independently of dependency-enabled JSON map ordering. Preserve compatibility with the 0.6.0 predecessor encoding while keeping configuration and execution identity checks intact (#113).
- Show the service’s specific activation failure instead of replacing it with a generic healthy-ownership message (#113).

### Changed

- Advance both application crates to 0.6.1 and bundle build 19; keep API protocol 7.

## 0.6.0 — 2026-11-18

### Added

- Opt in to a restricted built-in project manager from native project settings, with a separate OpenAI-compatible endpoint, model and private API key. Existing projects continue using native coding CLIs by default; native login, model settings and saved conversations remain unchanged (#110).
- Give the manager exactly seven internal tools: `read_file`, `write_file`, `delegate`, `worker_status`, `steer_worker`, `send_md` and `send_file`. Workspace file access and structured delegation support coordination while native CLI workers do the coding; the manager has no Bash, arbitrary process execution, generic HTTP or arbitrary MCP tools (#110).
- Use a Chinese management prompt grounded in the actual user project, with scoped group/topic corrections, explicit discussion origins and evidence-based acceptance. Show the manager, its coding worker and observed native children in the project tree (#110).
- Persist manager inputs, checkpoints and external-effect records across interruptions. Preserve fresh user steering, held CLI-switch input and uncertain effects; respect pause, Stop and terminal takeover, and support an ordered handoff back to native mode without replaying completed effects (#110).

### Fixed

- Left-align child Agent names, including read-only detail buttons, for a consistent sidebar tree (#109).

### Changed

- Raise the minimum Rust version for source builds to 1.95 and pin Rig to 0.44.0 for the optional manager.
- Advance both application crates to 0.6.0 and bundle build 18. Keep API protocol 7, the three native Agent tools (`send_md`, `send_file` and `schedule_run`) and bundled HERDR 0.9.5 unchanged. Manager provider credentials are configured separately and are never copied from native CLI login.

## 0.5.2 — 2026-11-18

### Fixed

- Admit unmentioned replies in verified project topics as immediate steering, including while the Agent is busy, while ignoring unrelated group traffic. Use native bot mentions and confirmed reply receipts to distinguish project discussions; shared broadcast topics require an explicit bot mention or a reply to that bot's own message (#105).
- Retain ambiguous mentions durably while bot identities are unavailable, without blocking private messages or proven topic input. Preserve deferred-input deduplication, respect explicit Stop, and keep identity recovery running after an interrupted update handover (#105).

### Changed

- Clarify that receiving unmentioned topic replies requires the Feishu `im:message.group_msg` permission or the Feishu Agent permission preset, approved and published by the application administrator. Dauerbot does not grant platform permissions automatically.
- Advance both application crates to 0.5.2 and bundle build 17. Keep API protocol 7, the three Agent tools and bundled HERDR 0.9.5 unchanged.

## 0.5.1 — 2026-11-18

### Added

- Show observed child Agents in a compact, collapsible project sidebar tree with nested branch lines, CLI icons and read-only details. Codex 0.169.1 uses its official ancestor-scoped metadata API; managed Pi exposes named invocations. Recorded relationships do not prove live activity or complete child coverage, and unsupported sources remain unknown (#99).

### Fixed

- Repaint the terminal after native window resizing so a narrowed panel shows the latest PTY content without a click or reconnect. Refresh only its owning window on the existing coalesced output cadence (#103).
- Separate automatic terminal viewport sizing from keyboard takeover. Bind viewport ownership to the immutable terminal, preserve the single terminal stream across input control changes, and keep displaced views from reacquiring another view's viewport (#98).

### Changed

- Advance both application crates to 0.5.1 and bundle build 16. Keep API protocol 7, the three Agent tools and bundled HERDR 0.9.5 unchanged.

## 0.5.0 — 2026-11-18

### Added

- Select per-project models for Codex, Claude Code, Pi and Antigravity from the desktop or authenticated Feishu selection cards. Keep requested and observed models distinct, preserve native login/settings, and guide setup of Feishu shortcut buttons (#83).
- Refresh the native project workspace using the reviewed Magpie layout, including responsive terminal geometry, accessible recovery controls and a searchable model picker (#84).

### Fixed

- Keep a supervised repair Agent reachable after CLI switching or runtime failure. Hold ordinary pending input until the selected execution is ready, preserve uncertain effects and native continuity, and respect pause, explicit Stop and terminal takeover (#82).
- Recover verified vanished native executions after HERDR restarts without closing or sending input to a foreign terminal that reused the old pane address (#90).
- Retain one canonical rich-post caption and embedded image/media references. Prepare bounded cached resources outside the delivery lock so slow downloads do not block later steering; surface unavailable resources in the Agent envelope (#92).
- Recover saved Agent conversations after a computer reboot using verified prior-boot execution evidence and one supervised runtime restart. Preserve queued input, fired schedules and uncertain effects. If a native CLI repeatedly forks the saved conversation, keep a scoped repair-only Agent reachable through authenticated MCP and trusted Feishu repair instructions without adopting the fork as the original session or releasing ordinary work (#94, #96).

### Changed

- Adapt Herdr App terminal rendering and input mechanisms for the native legacy transport, including ANSI colors, fixed-column shaping, bounded frame ingestion, takeover focus and AGY Alt+M. Native checks verified control release, history scrolling, selection and clipboard copy. Input-method candidate composition and commit remains an unverified, nonblocking follow-up under #85.
- Advance both application crates to 0.5.0 and bundle build 15. Keep API protocol 7, the three Agent tools and bundled HERDR 0.9.5 unchanged.

## 0.4.9 — 2026-11-17

### Fixed

- Hold unclaimed messages and scheduled input for the selected CLI from switch acceptance through authenticated readiness. Keep input pending on failure, pause, takeover and restart; retry without leaking to the old Agent. Explicit Stop cancels the switch and pauses delivery after a verified stop (#79).

### Changed

- Advance both application crates to 0.4.9 and bundle build 14. Keep API protocol 7 and the three Agent tools unchanged.

## 0.4.8 — 2026-11-17

### Added

- Bundle a project-local Herdr delegation skill and maintain a small `AGENTS.md` delegation block for new workspaces and session launches. Preserve user instructions and edited or unowned resource collisions; prefer responsive native same-CLI delegation without changing global settings (#72).
- Switch Codex, Claude Code, Pi or Antigravity immediately from the project view. The current Agent settles its work and children, writes a fresh `HANDOFF.md`, then the selected CLI starts in the same workspace. Preserve files, schedules, bot settings and queued input; deliver input after the selected CLI establishes its authenticated MCP connection (#70, #76).
- Read rendered `HANDOFF.md` and `AGENTS.md` directly in the native project view, with clear states for missing or unreadable documents (#70).

### Fixed

- Activate signed updates automatically after the new native window and service pass readiness checks. Preserve the latest window state and drafts, transfer service ownership, then publish the new interface and retire the old processes. Precommit failures restore the previous app; committed updates recover through the successor and latest journal (#66).
- Keep supported Agent MCP processes and native conversations running across service updates. Separate the stable three-tool MCP contract from the desktop API, and defer activation when an active contract cannot be hosted safely (#67).
- Transfer the existing listener and exclusive journal/effect ownership to a prepared successor. Warm Feishu transports before capable handover, drain accepted work, and preserve queued input, deduplication and uncertain effects (#68).

### Changed

- Simplify Updates to progress, results and normal update actions. The first launch after a published 0.4.7 update performs a bounded automatic bootstrap; later capable versions use verified handover (#66).
- Keep explicit CLI switching cooperative: require a fresh ready acknowledgement and a later native idle transition, retain the supported managed Pi dispatch fence, and leave automatic idle retirement's stricter child-coverage checks unchanged (#76).
- Advance both application crates to 0.4.8 and bundle build 13. Keep API protocol 7 and the three Agent tools unchanged.

## 0.4.7 — 2026-11-17

### Fixed

- Prevent the macOS menu-bar popup and normal window from deadlocking during focus changes by releasing the GPUI window mutex before the synchronous AppKit callback (#56).
- Preserve fractional trackpad movement, discrete wheel ticks, pointer cells, modifiers and source dimensions when scrolling terminal applications. Bounded unit events restore legacy HERDR wheel magnitude; explicit takeover remains required for application-owned history, including Codex virtual transcripts (#60).
- Require fresh, generation- and native-session-matched child coverage before idle handoff retirement. Child activity, changed or unknown coverage resets eligibility and cancels pending handoff. A shared dispatch gate fences the final stop, and unknown coverage remains authoritative across adapter reloads (#57).

### Added

- Support explicit Feishu topic replies for text and files. The Agent receives trusted message/thread context and selects the discussion anchor; independent notifications start with one actual message. Preserve the existing delivery journal, destination validation, request IDs and uncertain outcomes without automatic discussion grouping or extra ID mappings (#59).
- Load the pinned official Pi subprocess subagent extension only for managed Pi 1.5.1 sessions, preserving global plugins, authentication and selected models. Native same-CLI delegation is preferred, with Herdr for cross-CLI/model work. External plugins and unsupported CLI child coverage retain sessions; the foreground child tool can delay the root model's response to steering (#57).

### Changed

- Advance the app and daemon to 0.4.7, bundle build 12 and API protocol 7. API 6 services cannot satisfy the child-aware retirement contract; incompatible native executions use the existing explicit recovery action.

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
