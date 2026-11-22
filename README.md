# Dauerbot

A native macOS workspace that connects Feishu messages and scheduled runs to native CLI agents running on demand in HERDR. Built with Rust and GPUI, with an independent daemon shared by the desktop app, CLI, and MCP server.

One bot belongs to one user project and its conversation. Direct messages and group messages reach the same main session; the agent chooses where to send its replies. Dauerbot supports Codex, Claude Code, Pi, and Antigravity through their installed CLIs and existing account and model settings. Watch and take over the real terminal directly in Dauerbot. The lightweight daemon stays online; sessions with verified native-child coverage can sleep after ten idle minutes and wake for new work. Sessions with unknown child coverage stay available. Native mode uses the selected CLI directly. An optional built-in management Agent uses a separately configured model API for coordination while the native CLI continues to perform coding work.

## Install

Download the Apple Silicon package from [GitHub Releases](https://github.com/so2liu/dauerbot/releases), extract it, and move `Dauerbot.app` to Applications. The app includes the supported **HERDR 0.9.5** runtime and provisions it offline on first use when HERDR is missing. No separate HERDR installation is needed. Compatible existing installations continue to work; explicit `herdr` paths and `herdr_config` settings are preserved. An incompatible configured runtime produces a diagnostic instead of restarting an existing server. Sign in to the native Agent CLI you want to use; Dauerbot keeps its existing login and model settings.

Starting with 0.3.2, releases require Developer ID Application signing, Apple notarization, a stapled ticket, and Gatekeeper acceptance before publication. Intel builds and the minimum supported macOS version have not been verified.

Dauerbot uses the MIT license. Bundled HERDR uses Apache-2.0; its attribution and applicable vendored licenses are included in `Contents/Resources/herdr/`. It is an independent project with no official affiliation with HERDR, Feishu, or the CLI vendors.

## Connect your bot

Create a project by choosing a directory and an Agent CLI, then connect its dedicated bot. You can also keep the project local until you are ready. The native app guides you through creating an enterprise custom app in the Feishu developer console, enabling its bot capability, and saving its App ID and App Secret locally. A group Webhook bot cannot provide the required two-way messaging.

Import the permission JSON supplied by the app, select long connections for event delivery, subscribe to `im.message.receive_v1`, and publish the app. For topic replies without a new bot mention, use Feishu's existing Agent permission preset or the full permission JSON containing `im:message.group_msg`; obtain approval and publish if that scope is not already effective. No JSON re-import is needed when the required scope is already granted. The basic set only receives group mentions; reading an individual message through the API does not prove permission to receive unmentioned group events. Add the bot to a group, or message it directly. Credential validation proves the app's identity; it does not prove that event permissions work.

Each project has its own App ID, App Secret, event connection and connection status. The same bot cannot belong to two projects. New projects stay unbound until you connect their bot; they never inherit another project's credentials. Direct messages and explicit bot mentions enter the receiving bot's project without a chat-binding step. With broader group events authorized, unmentioned topic replies enter only a project identified by existing bot reply receipts or verified native mentions. Exact reply parents take precedence; an ambiguous shared topic needs an explicit bot mention. Unrelated group chatter is ignored. Native mention-all remains a broadcast. Replies mentioning a person within a known discussion remain follow-ups. The Agent still chooses each outbound reply anchor; there is no latest-topic pointer or discussion map. Bot identity lookup retries in the background; undecided events stay in a separate intake journal and never enter Agent history, handoff revisions or normal work until admitted. Direct messages and plain replies in known valid topics do not wait for that lookup. Native mention types distinguish people from other bots within known discussions; app-scoped IDs are compared only with the receiving bot. Legacy non-self mentions without a reliable type are ignored once that bot identity is known. A verified user Stop cancels undecided intake and retains deduplication, while runtime/update recovery preserves it. Ordinary pending and uncertain work keep their existing behavior.

From the CLI, bind and validate a project's bot with `hfb feishu set <project> --app-id cli_...` (the secret is read from stdin) and `hfb feishu check <project>`. `hfb feishu disconnect <project>` disconnects only that project. Existing singleton installations keep their bot on its recorded project when upgrading; other projects remain unbound.

Choose Codex, Claude Code, Pi or Antigravity in the project view to switch now. A running Agent first settles its children and ongoing work and writes a request-specific `HANDOFF.md`; Dauerbot starts the chosen CLI in the same workspace after that cooperative handoff and a later native idle transition. Once the switch is accepted, all unclaimed messages and fired schedules wait for the selected CLI; new input does not interrupt or invalidate the old Agent’s handoff. Authenticated MCP registration and native readiness release that input to the exact selected execution. Known active children prevent switching; supported managed Pi also supplies a native dispatch fence. Other CLIs rely on the Agent's explicit ready-to-switch acknowledgement rather than complete native child coverage. Automatic idle retirement keeps its stricter child-coverage requirement. A sleeping project without a resumable native conversation starts the selected CLI immediately, reading its existing project files (or beginning a fresh workspace if no handoff exists). A paused project or terminal takeover must be resumed/released first. Failed handoff keeps the old CLI and holds pending input; failed selected-CLI startup preserves the selection, handoff and pending input for retry. Pause or terminal takeover does not release the hold. Explicit Stop abandons the switch only after the native stop succeeds and pauses the project, keeping messages pending until Resume. Changing CLI clears the previous CLI's native identity without replaying completed messages. The advanced `hfb session switch` command rejects a held transition; stop it explicitly before changing CLI by force.

Avoid running another long-connection listener for the same Feishu app: the platform distributes events between listeners, so another program may receive the message instead.

## Native desktop and local data

The desktop renders native GPUI controls. There is no browser interface, WebView, Node.js runtime, or frontend build step. Dauerbot starts in the macOS menu bar: click its icon to see project states, add a project, or open the full app. Selecting a project opens its details. Closing the window returns to the menu bar and leaves the daemon and HERDR Agent running; Quit exits the desktop.

The full window has projects on the left and scheduled runs in the center. The selected Agent's native terminal is a debugging tool on the right, collapsed by default. Expand it to observe: the real PTY follows the available panel size, including window and font-size changes. Viewing owns geometry independently of keyboard input; a newer terminal view replaces the old viewport connection. **Take control** enables direct input, login, model selection, and native prompts. **Release control** returns the session to automatic dispatch. Viewing does not prevent idle sleep. Explicit control pins the execution until released, protecting unsent input even if the desktop crashes. Hiding the terminal or closing the window releases control and disconnects the terminal bridge. Send instructions and ask the Agent to schedule work in Feishu; the desktop lists schedules and lets you pause or enable them. Agent execution steps and manual schedule forms are not part of the main view. Settings contains bot setup, permissions and updates.

Open `HANDOFF.md` or `AGENTS.md` directly in the project view to read rendered Markdown without opening Terminal or an editor. Views are read-only and accept regular UTF-8 files up to 256 KiB. Embedded images and raw HTML are omitted; links open only when clicked. Missing documents have an explicit empty state.

Choose **Take control** before scrolling application history. Fullscreen Codex owns its transcript, so wheel gestures must reach Codex through the native terminal; read-only observation cannot reveal messages Codex has not rendered. Wheel input preserves the pointer cell and modifiers, and HERDR routes it according to the application's mouse and alternate-scroll modes. Observation never silently takes control or sends wheel events.

Switch between **English** and **简体中文** in the app. The choice takes effect immediately and stays saved on this Mac. Project state changes also appear in the menu-bar panel.

Expand a project in the sidebar to see observed native child Agents. Codex 0.169.1 exposes recorded parent/child relationships and native names through its scoped metadata API; their current activity remains unknown. Other Codex versions and CLIs without a verified hierarchy source show unavailable observations. Managed Pi shows named invocation states from a separate bounded presentation sample. Missing, stale or incomplete metadata is labeled accordingly. This tree never establishes child completeness or retirement safety, and children without a known terminal target remain read-only.

Dauerbot checks only the project folders or SQLite files you select. Database checks read schema metadata rather than business records. Full Disk Access is optional and must be granted in System Settings to the application that actually performs the task. Granting access to Dauerbot does not automatically grant it to an existing HERDR or Agent process. Normal operation does not require Accessibility or Screen Recording.

The internal desktop executable remains `HerdrBridge`, the CLI is `hfb`, and the bundle ID remains `dev.herdr.feishu-bridge` so upgrades preserve existing settings and messages. On macOS, the default data directory is `~/Library/Application Support/dev.herdr.feishu-bridge`. `hfb init` prints the platform's actual directory. Unix directories use mode 0700 and configuration files use 0600. Bot credentials are stored in the private SQLite database, with file mode 0600. They are never returned in API responses. Native CLI accounts continue to use their own credential storage.

The desktop chooses an available loopback port on first setup; CLI commands read the same configuration. The daemon's management API uses a random bearer token and binds only to IPv4 loopback. Do not expose it directly to the public Internet.

Use `--data-dir` for an independent instance. Its window shows an independent-instance label and the actual path:

```sh
open "dist/Dauerbot.app" --args --data-dir /absolute/path/to/private-data
```

Each data directory derives its own dedicated HERDR session. App-managed runtime copies live under `runtime/herdr/0.9.5/<signed-payload-sha256>/herdr` in that directory. Installation uses a process lock and atomic rename, preserves older runtime paths across app updates/moves, and does not edit shell profiles or global PATH. Each Agent execution receives the resolved runtime directory in its own PATH after login-shell startup. Missing or corrupt owned copies are repaired from the verified bundle; symlink targets are rejected. User-owned executables and configuration files are never replaced. Normal schema upgrades preserve existing projects, credentials, native history references, messages and explicit pauses. Product execution uses only legacy herdr 0.9.5.

## Advanced Manager observability

Manager tracing is disabled by default. To opt in, create a separate Langfuse v4 project and save a private `langfuse.json` in Dauerbot's data directory (file mode `0600` and data directory mode `0700` on Unix). Use the Langfuse project keys, independently of the Manager provider login.

```json
{
  "enabled": true,
  "include_content": false,
  "host": "https://cloud.langfuse.com",
  "public_key": "<project public key>",
  "secret_key": "<project secret key>"
}
```

Omit `host` to use the default. For a different region or self-hosted installation, use its HTTPS origin without the API path. Plain HTTP is restricted to loopback. The daemon reads the file at startup. The integration has no project view controls. Set `enabled` to false or remove the file to disable it on the next daemon start.

Manager sends OTLP/HTTP JSON trace metadata using Langfuse Basic authentication and the v4 ingestion header. For self-hosted Langfuse, upgrade the server to v4 before using v4 ingestion. Each Manager turn has one pseudonymous trace, including turns triggered by worker updates, timers, and approved proposals. The trace records its admitted input IDs and a child span for each input. Model generations, compaction calls, and tool spans appear once per turn. All observations in a trace carry the same pseudonymous Manager channel session ID. The trace includes timings, the configured model name, available token counts, operation types and bounded outcome categories. Content capture is off unless you set `include_content` to true. Enable it only when you consent to send debugging content to the configured host. This includes stored user event envelopes, the full model message history with system instructions and attached image data, assistant text and tool calls, and tool arguments and results. The root observation contains the admitted events and any eligible final assistant text for a completed turn. Exports redact configured credentials, known credential patterns, and protected paths. Tool results and later model content are suppressed when a protected file is involved. Arbitrary unmarked secrets in text or images can still be disclosed, so keep them out of inputs and ordinary workspace files when debugging is enabled. The exporter sends best-effort in the background with a three-second request deadline and bounded capacity. Export errors only produce local diagnostics.

## Build and develop

Install Rust 1.95 or newer and Xcode on macOS. The dependency lockfiles define the exact Rust dependencies. The execution and terminal-stream adapter targets legacy HERDR 0.9.5 / protocol 22. The terminal component uses a licensed Rust fork of gpui-terminal with the Alacritty emulator. See [ARCHITECTURE.md](ARCHITECTURE.md) for ownership and lifecycle contracts.

```sh
./scripts/build-desktop.sh
open "dist/Dauerbot.app"

# Build only the standalone daemon and CLI.
./scripts/build.sh
./target/release/hfb serve
```

The desktop package includes `hfb` beside `HerdrBridge`. The bundled `hfb ui` opens the native desktop; a standalone CLI reports a missing desktop executable instead of opening a browser. `hfb serve` runs in the foreground and handles SIGINT/SIGTERM. `hfb service` prints a launchd or systemd user-service template without installing it. Regenerate that template if you move the executable or data directory.

Local builds use an ad-hoc signature. To sign with an existing certificate, run `./scripts/build-desktop.sh "Signing Identity"`; `scripts/sign-macos.sh` also accepts an optional keychain path. These scripts do not export private keys.

```sh
cargo test --locked --release --target-dir desktop/target --tests
cargo test --locked --release --manifest-path desktop/Cargo.toml
cargo clippy --locked --release --target-dir desktop/target --all-targets -- -D warnings
cargo clippy --locked --release --manifest-path desktop/Cargo.toml --all-targets -- -D warnings
python3 scripts/smoke.py --binary "$PWD/desktop/target/release/hfb"
```

`python3 scripts/terminal-viewport-smoke.py --binary /path/to/hfb` checks actual private PTY dimensions, competing views and normal Agent/MCP delivery while the desktop owns geometry without keyboard takeover.

The smoke test uses real SDK WebSockets, an isolated HERDR terminal, MCP stdio, and local Feishu fixtures. It checks delivery, files, deduplication, reconnection, daemon restarts, embedded terminal connections, unknown-child retirement vetoes, native continuity and message/timer wakeup. Focused Rust tests cover known-quiescent handoff retirement and its dispatch fence. It uses private XDG configuration and HERDR sessions, with no real bot or model account. The custom fixture records fresh launches and explicit native resume as test-only continuity metadata; it does not prove native CLI hooks or real-model acceptance. Test artifacts stay in ignored directories.

The cross-version regression uses the published, signed 0.4.4 app as the old daemon, replaces its bundle executable with the new build, and exercises the actual old and new MCP contracts:

```sh
python3 scripts/upgrade-smoke.py --old-app /path/to/extracted-0.4.4/Dauerbot.app --binary "$PWD/desktop/target/release/hfb"
```

It covers historical companion dispatch, missing-backup failure, a live old MCP across an incompatible daemon handoff, explicit native recovery, queued input delivered once, uncertain work retained, unrelated panes preserved, and compatible live-session reuse. Its private fixture Agent establishes native identity through HERDR and uses each published MCP contract normally; the harness does not acknowledge messages, patch the database, or replay effects.

Live sending is opt-in: `tests/live_feishu.rs` reads `.local/live.json` with `app_id`, `app_secret`, and `chat_id`. Run `cargo test --test live_feishu -- --ignored` only for an explicitly selected bot and recipient, then remove the temporary credentials. It sends a test Markdown message and a small file without opening a competing event listener.

## Built-in project manager

The built-in Rust Manager works with the project's native coding CLI. In **Settings**, choose OpenAI Chat Completions, OpenAI Responses, or Anthropic Messages, then enter the provider endpoint, model, and API key. The provider is shared across projects. You can set a context window in tokens or leave it blank for automatic detection. Dauerbot keeps the Manager key in its private local database. Coding CLI accounts retain their own login and settings.

The Manager stores an append-only conversation per channel and automatically summarizes older history as the context fills. Changes to the workspace's `AGENTS.md` reach each channel at its next model call. New user input can cancel a model request in that channel. An admitted tool can finish in the background and return its result to the originating channel.

The Manager has [11 restricted tools](src/manager_prompt.md) for workspace files, worker delegation and steering, scheduled runs, Feishu messages and files, and authorized terminal controls. The daemon enforces Pause, Stop, and terminal takeover, and preserves uncertain effects for review. Managed input stays queued when the provider is unavailable. Coding work runs in each project's native CLI, including Pi when selected as a worker.

## CLI and HERDR sessions

```sh
hfb project add my-project --agent codex
# Or use an existing project folder:
hfb project add existing-project --path /absolute/path/to/repo --agent codex
hfb project list
hfb session start my-project
hfb session screen my-project
hfb session attach my-project
hfb message inject my-project 'Review this project and send the result to Feishu'
hfb session switch my-project --agent agy
hfb project pause my-project
hfb project unpause my-project
hfb session stop my-project
hfb session resume my-project
```

Without `--path`, project creation allocates a private folder under `~/.dauerbot/agents/<project-name>-<four-random-letters>`. The desktop uses this default too; choosing an existing folder remains available. App-created folders receive a concise `AGENTS.md` for durable rules. Keep current progress in `HANDOFF.md`.

New workspaces and normal session launches install a project-local Herdr delegation skill at `.dauerbot/skills/herdr-cli-subagents/SKILL.md`. A small marked block in `AGENTS.md` points to it and asks the main Agent to delegate substantial work while staying responsive. Existing user instructions outside that block remain unchanged, including in selected existing folders. Native responsive subagents are preferred for same-CLI work; Herdr serves cross-CLI/model collaboration or cases native delegation cannot cover. This private skill needs no global installation or native discovery registration: the project instructions and session bootstrap explicitly point to it. User-edited or unowned skill collisions stop preparation with an actionable error instead of being overwritten.

The skill does not add child-observation coverage or an idle-retirement pin for external Herdr workers. The parent remains responsible for their results, cleanup and handoff records; parent idle alone cannot prove those panes have settled.

`--data-dir` is a global argument. For an isolated daemon, use `hfb --data-dir /absolute/private-data serve --port 49322 --herdr-session my-bridge`.

Dauerbot preserves native CLI account, model, provider, and reasoning settings. It does not invent model flags. On macOS, native agents start through a login shell inside HERDR, loading the same profile exports and proxy settings as your terminal. CLI arguments are passed literally, including prompts and paths with spaces. As configured for this project, Codex uses `--dangerously-bypass-approvals-and-sandbox`, Claude Code and Antigravity use `--dangerously-skip-permissions`, and Pi uses `--approve` for project files. Pi extensions can still enforce their own approval rules.

A main session is a native CLI process owned by the legacy HERDR server. Desktop terminal observation connects to that exact execution; CLI attach remains available for terminal users. Login and permission prompts remain in place, with an actionable attention notice. Dauerbot confirms recognized Claude Code and Antigravity project-trust prompts after verifying the exact project and execution identity. It attempts this once per execution and leaves other prompts for the user.

Use the same CLI's verified native session ID to resume history. Native hooks supply session identity where supported. Cross-CLI switching preserves project files rather than claiming native transcript portability. Model selection stays native unless you explicitly supply an override; the terminal shows native model information, while unconfirmed metadata stays labeled unconfirmed.

| CLI | MCP configuration | Native resume |
| --- | --- | --- |
| Codex | Dedicated launch configuration; enables `--no-daemon` and `--dangerously-bypass-hook-trust` when the installed CLI advertises each flag, and forwards HERDR terminal identity | `codex resume`, using the session observed through native HERDR metadata or a supplied ID |
| Claude Code | Project-specific `--mcp-config` | A new UUID for a new session; `--resume` for recovery |
| Pi | Merges `herdr_feishu` into `.pi/mcp.json` | `--session` |
| Antigravity (`agy`) | Registers `herdr_feishu` in `~/.gemini/config/mcp_config.json`; the server resolves the current execution from verified HERDR identity. `-i` submits the initial interactive prompt | `--conversation` |

Configuration merges preserve other MCP entries and reject invalid JSON or an unrelated server using the same name. Ordinary agy sessions receive no Dauerbot project tools. Ignore generated local configuration in the target project. Custom CLI argument arrays run without a shell and can use `{mcp_config}`, `{credentials}`, `{bridge_exe}`, and `{instructions}`. Per-session credentials live under `sessions/<project>/<generation>/` and cannot administer other projects.

## Messages and MCP

The managed CLI starts `hfb mcp --credentials <private-file>` automatically.

| Tool | Arguments | Purpose |
| --- | --- | --- |
| `send_md` | `to`, `text`, `request_id`, optional `reply_to_message_id` | Reply with Markdown in the chosen discussion |
| `send_file` | `to`, `path`, `request_id`, optional `reply_to_message_id` | Reply with a local file in the chosen discussion, up to 100 MB |
| `schedule_run` | `request_id`, `name`, `prompt`, `kind`, `timezone`, schedule fields, `purpose` for new schedules, optional `to` | Persist a once, interval, or cron run; classify user work or internal follow-ups |

`to` is mandatory for MCP sends: `oc_…` identifies a chat and `ou_…` a user Open ID. The agent follows user intent and may reply somewhere other than the source chat. Reuse `request_id` only when retrying the same content and destination. Only `status=sent` means the API confirmed the send. Files must resolve inside the project or Dauerbot cache; symlinks cannot escape those directories.

Incoming messages use an escaped XML envelope with source IDs, sender, body, attachments, mentions, and thread context. Rich posts select one localized document (Chinese, then English, then the first available locale) and retain embedded image/media references. Preparation runs outside the project delivery lock, so later plain steering can pass while an attachment downloads. Up to 16 resources and 100 MB in total are downloaded per message into the Dauerbot cache within an 80-second total deadline; unavailable or over-limit resources remain visible with an error. The envelope provides local file paths for the CLI's own image/file tools; it does not imply that the native model has viewed the image. Group messages include a bounded member snapshot when available. Direct messages omit group-only `<chat>` and `<members>` elements; they do not emit `not_applicable` placeholders. Missing group permissions produce `status="unavailable"` without blocking the message. User text remains untrusted input, even inside XML.

SQLite stores incoming and outgoing messages. Incoming messages persist before SDK acknowledgment and deduplicate by project and platform message ID. Conversation messages immediately steer the same main Agent, including while it is working. Dauerbot submits them through the native CLI's paste-and-Enter path. The Agent calls no readiness, acknowledgment, progress, decision, job, or completion tools. Questions and replies are ordinary Feishu messages. Native HERDR working/idle state drives session activity; terminal output alone never sends a Feishu reply.

| Status | Meaning |
| --- | --- |
| `queued` | Persisted but not yet delivered (starting, paused, taken over, or blocked); live conversation messages do not wait for idle |
| `submitting` / `submitted` | Writing to the terminal / submitted to its input stream |
| `acknowledged` | Receipt recorded by an earlier version; never automatically replayed |
| `pending` / `sending` / `sent` | Outbound waiting / sending / API-confirmed |
| `failed` | A definite failure allows an informed retry |
| `uncertain` | Verify the result before retrying |
| `dismissed` | Manually excluded from blocking delivery |

Restarts preserve uncertain operations instead of blindly resending. PTY input and Feishu have no atomic transaction, so Dauerbot does not promise exactly-once delivery. It checks legacy metadata, root PID, generation, and foreground program identity before typing; exited agents and replacement shells do not receive queued input.

All text/file replies use Feishu topics when the Agent supplies their anchor. For a normal reply, pass the trusted incoming `message_id` as `reply_to_message_id`; reuse that original discussion anchor for related updates and attachments even when another message steers the Agent. The daemon validates the anchor against the project, bot and explicit `to`. A P2P user `ou_` recipient can match a trusted private-chat anchor; a group message never becomes permission to reply to its sender privately. Previously sent messages can also anchor follow-ups. Incoming envelopes retain `thread_id`, `root_id` and `parent_id`.

Ordinary work sends one concise result. A requested result or a decision requiring user action can start a discussion when there is no trusted topic. Supply the trusted work occurrence as `source_event_id` and set `intent=result` or `intent=decision`. Dauerbot stores the first confirmed root and reuses it for the same work occurrence, bot and recipient. A decision can mention the trusted originating requester or explicit direct recipient. For user-requested progress in an existing topic, use `intent=progress` with the trusted work occurrence. Later progress updates edit the same reply. If Feishu definitively prohibits editing that message, Dauerbot may send one replacement inside the original topic. Unsupported topic replies and uncertain delivery stay visible for review.

Topics group Feishu's visible conversation; they don't remove existing native Agent context. Dauerbot doesn't reload the entire thread on each incoming message. Bot P2P support for Markdown and uploaded files was verified against an owner-selected private chat on November 17, 2026. Other tenants and chat modes remain subject to platform permissions, bot membership and topic visibility. Group @-only event permissions don't guarantee delivery of subsequent unmentioned replies; natural follow-ups need the corresponding group-message permission or another explicit @.

## Sessions and scheduled runs

The supervisor observes native HERDR identity, activity, foreground processes, manual control and explicit native-child coverage. Parent idle alone cannot start the ten-minute warm period: covered children must also be quiescent. Active children, incomplete or stale coverage, and changes to the child invocation revision reset the full idle period and cancel pending handoff. After ten idle minutes, Dauerbot asks the Agent to update `HANDOFF.md` with its goal, completed work, decisions, relevant files/commits, pending issues and next actions. Retirement requires an updated artifact bearing that request’s marker and a later verified native idle transition. New input cancels a pending handoff and steers the live Agent; input arriving after the retirement boundary wakes a fresh session immediately. The fresh session must read `AGENTS.md` and `HANDOFF.md` before new work. A failed or timed-out handoff preserves native recovery and never retries automatically; cancelled requests can try again after another idle period, at most three times per execution. Stopping a session ends its current process; later messages wake a fresh execution of the same native conversation. Explicitly pausing automatic activity holds new input. An uncertain delivery stays recorded without blocking fresh messages or independent schedules. Uncertainty in the current execution prevents automatic handoff retirement; uncertainty from an older execution does not pin a fresh one. Native idle describes execution state, not proof of successful business work or a completed external effect.

Prefer native subagents for same-CLI delegation; use Herdr mainly for cross-CLI/model collaboration or when native delegation is insufficient. Managed Pi **1.5.1** explicitly loads the bundled MIT-licensed official subprocess extension, pinned at Pi commit `1f6f319bb28992a40b99c147fffb23b65a23129d`. Its tool is named `dauerbot_subagent`, preserving existing user `subagent` tools. The fallback `worker` inherits the parent's selected model and thinking level; user/project worker definitions retain their precedence. Other Pi versions keep their selected runtime and normal configuration, with unknown child coverage and no injected adapter. Existing sessions launched before this adapter also remain unknown until their next normal launch.

The extension counts the complete single, parallel or chained tool invocation, including pending slots and gaps between processes. It publishes bounded automatic evidence tied to the execution generation and native parent session, with a fresh heartbeat and child activity revision. Missing, malformed, stale or mismatched evidence means **unknown**, which retains the session. Codex, Claude Code, Antigravity and custom commands currently have unknown child coverage: HERDR 0.9.5 has no native child census, and Antigravity explicitly excludes background children from parent status. Their normal steering, schedules, explicit stop and native recovery still work.

Global Pi settings, plugins, packages and authentication continue to load normally. Their child lifecycle is not covered by this adapter. Any configured or auto-discovered external extension/package, additional extension launch flag, or unreadable discovery input makes coverage unknown, even if the extension registers no tools. The sole allowed observer is the exact regular-file HERDR 0.9.5 Pi v9 hook (SHA-256 `2c5272d732b475bbf91a027203b1f98d25fe43d2c1402530a442b288aeaca1e4`), audited as socket-only lifecycle reporting with no child dispatch. A modified hook remains unknown. Extra MCP servers outside the generation-scoped Dauerbot entry also make coverage unknown. Once incomplete, coverage stays unknown for the whole execution generation, including extension reloads, even if its configuration is later removed. A ledger left by an unclean adapter restart also starts with unknown coverage. The daemon treats the durable unknown marker as authoritative even if a late old adapter callback replaces the child snapshot. No global files are installed, removed or rewritten by this subagent integration.

The final retirement claim uses the same atomic directory-creation gate as managed subagent dispatch, rechecks the revision and coverage, and leaves a generation-specific retirement marker before native stop. New managed child dispatch cannot pass that fence. Once native stop is attempted, an error also preserves the marker because a delayed close may still take effect; explicit native recovery resolves that uncertainty. A pre-stop veto or cancelled claim releases its temporary fence. A crash leaves the gate/marker conservative; Dauerbot never guesses that a stale native gate can be broken. The database transaction separately coordinates incoming input. These boundaries cover this adapter's dispatch, not arbitrary detached shell jobs or uninstrumented plugins. The upstream foreground tool waits for its children: Feishu input is delivered as steering immediately, but the root model's reply can wait for the active tool. It does not provide asynchronous parent replies while a child tool is unresolved.

The offline fixture `node scripts/pi-subagent-fixture.mjs /path/to/pi-coding-agent` uses the installed Pi 1.5.1 extension loader with deterministic subprocess protocol fixtures to verify awaiting, parallel/chain gaps, abort, model inheritance, dispatch fences and global plugin preservation. The separate `python3 scripts/pi-native-subagent-fixture.py /absolute/path/to/pi [pinned-herdr-hook]` check starts the actual installed TUI parent and headless child Pi with private HOME/settings/models, offline startup and a deterministic localhost model endpoint. It verifies parent identity, active/quiescent evidence, selected-model inheritance, the foreground reply barrier, native dispatch veto and shutdown invalidation. Its finite server reports its PID/port and shuts down in the fixture cleanup. Neither check establishes live-provider latency or account compatibility. This runtime/retirement contract uses API protocol 7. Automatic activation checks each live MCP contract before transferring ownership; unsupported executions defer the update while preserving native continuity and queued/uncertain input.

```sh
hfb timer add my-project "Daily review" "Review project changes" --kind cron --cron '0 9 * * 1-5' --timezone Asia/Shanghai
hfb timer add my-project "Follow up" "Check the build result" --kind once --at '2026-12-01T09:00:00+08:00' --timezone Asia/Shanghai
hfb timer list my-project
hfb timer pause TIMER_ID
hfb timer resume TIMER_ID
hfb timer remove TIMER_ID
# Retry only after checking whether the previous input already performed work.
hfb event retry EVENT_ID
hfb event dismiss EVENT_ID
```

Schedules support once, interval and UNIX five-field cron expressions with IANA timezones. Recurring schedules coalesce missed occurrences after downtime and avoid overlap while their own input remains pending. Editing, disabling or deleting a timer cancels unsubmitted occurrences of its old revision.

For new `schedule_run` requests, set `purpose` to `user` for work the user asked to schedule or `internal` for the Agent's own follow-up checks. Internal schedules execute and deduplicate normally. The desktop schedule rows and count show user work and unclassified legacy reminders that have a next occurrence; they omit internal follow-ups and exhausted one-shots. Paused future reminders remain visible with an Enable action. API/CLI listings and execution history retain all records.

Legacy schedules have an empty, unclassified purpose. Migration leaves their names, enabled state, next occurrence, revisions and history intact; it never guesses purpose from titles, language or schedule kind, so existing future one-shots remain visible. Missing or empty purpose is accepted for compatible clients and retries and omitted from the canonical creation payload. Reuse the original payload and `request_id` on retries, including after an upgrade; adding an explicit purpose to a previous request is a different payload. Edits that omit purpose preserve the existing classification.

Managed MCP exposes exactly `send_md`, `send_file`, and `schedule_run`; unmanaged sessions expose no project tools. MCP privately authenticates the project, generation and HERDR pane at startup. Stable request IDs deduplicate sends and schedule creation. Background execution belongs to HERDR and the Agent. There is no separate Dauerbot task or job registry.

Normal database upgrades preserve messages, schedule IDs, occurrence history and uncertain outcomes. Legacy journal containers remain internal for their foreign-key relationships. Previously submitted or uncertain input is never requeued by this upgrade. API protocol 6 and application version 0.4.6 identify private workspace creation and verified handoff retirement; incompatible desktop/daemon combinations must reconnect using matching builds.

## Updates and releases

The native app checks GitHub Releases at startup and every six hours while open. Stable accepts stable versions; preview also accepts prereleases. Selection uses semantic version order. Signed updates prepare the canonical app bundle while the current app keeps serving, then start a hidden native window and a standby service. After GUI readiness, supported live Agent contracts, real transport readiness and exclusive service ownership are confirmed, the new window takes over and the old processes drain. The desktop shows update progress and results; users do not manage desktop/service restarts or Agent recovery as part of an update.

`updates/activation.json` is the private durable installation receipt. The update lock spans bundle preparation, service handover and GUI publication. Before ownership commits, failure restores the exact original bundle and previous installation record. After ownership commits, recovery uses the successor and the latest journal; it never restores an old database snapshot. Interrupted preparation is reconciled at startup, and committed GUI publication has bounded retries. An automatic retry does not repeatedly activate a failed release. Manual update checks can retry a failed publication. The previous signed bundle stays available until publication succeeds.

Published 0.4.7 cannot acquire candidate-launch or standby hooks retroactively. The first launch of a capable desktop performs a separately bounded bootstrap: authenticate the existing service, preflight the signed successor and a read-only snapshot, verify active MCP contracts, then transfer to the new service. API7 native sessions keep their generations and native IDs. Unsupported contracts defer the update while the current service continues. A durable startup ownership receipt separates failures that can reopen the exact prior executable from failures that must recover the successor. This initial bootstrap can reconnect transports; the capable-to-capable handover is the path that prepares the successor before traffic moves.

Agent tools use retained, immutable hfb companions. The stable three-tool MCP contract is separate from the desktop API. Supported active MCP processes keep running across service handover; unsupported contracts veto activation before ownership changes. The installer never silently restarts an Agent or replays an uncertain effect.

Before accepting requests, the daemon retains its executable at `runtime/hfb/<version>-api<protocol>/<sha256>/hfb`. MCP configurations and custom `{bridge_exe}` commands use this immutable path.

The daemon collects obsolete companions at startup and hourly on a blocking background worker. Each sweep examines at most 512 entries and checks at most eight deletion candidates. It retains its own companion lease, every terminal-bearing project's recorded companion and current-generation MCP command (including paused, taken-over and update-required executions), persistent managed Pi and Antigravity commands, literal companion paths in saved launch arguments (including stopped projects), and the exact SHA-256 of the updater's validated signed rollback executable. Older executions without a recorded companion retain their matching protocol until retirement; an unknown execution protocol skips collection. Pi configuration locations are recorded in `runtime/hfb/integrations.json` before publication and during collection, so known project configurations remain roots after project deletion. Only exact known project `.pi/mcp.json` paths and the global `~/.gemini/config/mcp_config.json` are read; no home-directory search is performed.

Retired `sessions/<project>/<generation>/mcp.json` and credentials remain on disk but stop retaining executables once they are no longer the project's terminal generation. New native recovery commands use the current companion and explicit generation configuration; Pi and Antigravity's persistent commands remain independent roots until removed or replaced. Native session history does not itself relaunch archived bootstrap commands.

A filesystem lock serializes companion publication and collection. Shared per-entry leases cover publication through durable reference creation; the daemon holds its lease for its lifetime. Historical dispatch explicitly inherits its lease across `exec`, including into older executables that do not implement leases. Before deletion the collector requires an exclusive lease and a successful OS no-holder check; legacy protocol roots also protect future children of existing old Agents. Tool errors, permission warnings, timeouts and uncertain reference state prevent deletion. Only validated, owned, regular content-addressed files are removed individually; symlinks, unknown files, bundles, native HERDR sessions, credentials and user files are preserved. The separate bundled HERDR runtime is outside this collector.

Already-running older daemons cannot acquire pinning logic retroactively. When one launches the new hfb through a replaced bundle path, the new MCP authenticates the local daemon, verifies its version and generation, and dispatches to the matching original executable from `updates/installed.json`'s preserved signed bundle. The historical executable must pass Apple Team, bundle and exact-version checks before retention. The new MCP does not implement retired lifecycle tools. If neither that backup nor a valid retained historical companion survives, startup reports `update_required` and preserves the native execution and saved input. Automatic activation refuses an unverified live contract instead of silently replacing its execution. Historical desktop binaries retain their original behavior.

The desktop captures authenticated prior execution metadata before handover. Compatibility is checked against the successor's supported MCP contracts before switching ownership. Unknown or unsupported live contracts preserve the current service and defer activation.

App Updates supports manual checks, automatic signed installation, disabling automatic updates and channel selection. CLI-only `update install` prepares the bundle for desktop activation. The legacy CLI rollback path rejects committed ownership and requires the rollback target to pass current schema/MCP admission before changing files or services.

```sh
hfb update status
hfb update check
hfb update install --app "/Applications/Dauerbot.app"
hfb update auto off
hfb update auto on
hfb update channel preview
hfb update channel stable
hfb update rollback
```

The updater verifies the Ed25519-signed manifest, size, SHA-256, Apple Team, bundle ID, version, and architecture. It rejects unsafe archive paths and symlinks. Same-volume replacement retains one old bundle and restores it if replacement or CLI validation fails. Ad-hoc and development builds cannot silently replace themselves; the installed bundle must already have a trusted signature and a writable parent directory.

Release tags must reference commits on `main`. Update both crate versions, lockfiles, Info.plist, and CHANGELOG before staging. Build, test, and lint locally. Release automation first validates the tag and staged package metadata on Ubuntu, then starts macOS to verify the package, sign with Developer ID Application, submit to Apple notarization, staple the accepted ticket, and verify Gatekeeper acceptance. It uploads the zip, `SHA256SUMS`, `signing.txt`, `update.json`, and `update.json.sig` to the existing private draft and verifies GitHub's uploaded digests. The workflow leaves that signed draft unpublished for local verification against the actual signed predecessor. Public versions cannot be overwritten, and an older release cannot take Latest from a newer version.

The repository's Actions secrets hold `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_ID`, `APPLE_APP_SPECIFIC_PASSWORD`, `APPLE_TEAM_ID`, and the Base64 32-byte Ed25519 seed `UPDATE_SIGNING_KEY`. The certificate must include the Developer ID Application private key for Team `366ADQ28F6`. Notarization credentials are stored only in the temporary runner keychain, which CI deletes after the run. A notarization failure prevents publication. The public update key lives in `packaging/update-public-key.txt`. Do not rotate it without a transition release that existing clients can verify. Manifest signing does not replace Apple notarization.

Merge the release source to main and run `python3 scripts/prebuilt-macos.py stage --tag vVERSION` from a clean checkout matching main, then push the version tag pointing to that staged main commit. The script builds and runs the existing Rust checks locally before uploading an ad-hoc package to a private draft release. Automation verifies its source commit, tree, version, architecture, size, and SHA-256, then signs and notarizes it without compiling again. The certificate and private key stay in GitHub Secrets. Staging assets are removed before publication. Missing or mismatched staging inputs stop the release; there is no cloud compilation fallback.

After the signing workflow succeeds, download the signed assets from the private draft and independently verify their manifest signature, hashes, Developer ID signature, notarization ticket, and Gatekeeper acceptance. Before publication, run isolated cross-version activation against the actual published signed predecessor, including configuration-fingerprint mutation rejection, and verify native Agent/MCP continuity. Same-version candidate tests do not establish predecessor compatibility. Keep the unsigned staging inputs in the draft until verification completes so the signing workflow remains retryable.

Copy the verified signed assets into `dist/release/` and save the draft body as `dist/release/release-notes.md` (`gh release view vVERSION --repo so2liu/dauerbot --json body --jq .body`). Then run `python3 scripts/publish-release.py --tag vVERSION` to publish. This explicit final step reuploads and verifies the signed assets, removes private unsigned staging inputs, publishes the release, and updates Latest according to semantic version order. `--stage-only` only uploads and verifies signed assets in an existing private draft and updates its notes; it never publishes, deletes staging inputs, or changes Latest. Do not publish or install a failed compatibility candidate.

Routine pushes and pull requests do not trigger Actions runners. The full native build, tests, and clippy run locally during release staging. For an explicit cloud verification, use Actions → Manual native checks → Run workflow. This manual workflow has a 30-minute limit and cancels an older manual run for the same ref. Release notarization receipts are retained for seven days.

## Repository layout

```text
src/          Rust daemon, API, CLI, MCP, storage, and connectors
desktop/      Native GPUI macOS client
packaging/    Bundle metadata, icon, and public update key
scripts/      Rust builds, signing, release automation, and protocol fixtures
tests/        Behavioral regressions and optional live sending
test-results/ Ignored local verification artifacts
```
