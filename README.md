# Dauerbot

A native macOS workspace that connects Feishu, durable tasks, and schedules to native CLI agents running on demand in HERDR. Built with Rust and GPUI, with an independent daemon shared by the desktop app, CLI, and MCP server.

One bot belongs to one project with one active agent. Direct messages and group messages reach the same main session; the agent chooses where to send its replies. Dauerbot supports Codex, Claude Code, Pi, and Antigravity through their installed CLIs and existing account and model settings. Watch and take over the real terminal directly in Dauerbot. The lightweight daemon stays online; an idle Agent sleeps after ten minutes and wakes for new work. Dauerbot does not proxy model APIs.

## Install

Download the Apple Silicon package from [GitHub Releases](https://github.com/so2liu/dauerbot/releases), extract it, and move `Dauerbot.app` to Applications. Version 0.4.0 requires **`herdr` 0.9.5** and embeds its terminal directly. Earlier Dauerbot releases used HERDR Core (`hc`) and Herdr App; that execution backend is no longer supported. Sign in to the native Agent CLI you want to use; Dauerbot keeps its existing login and model settings.

Starting with 0.3.2, releases require Developer ID Application signing, Apple notarization, a stapled ticket, and Gatekeeper acceptance before publication. Intel builds and the minimum supported macOS version have not been verified.

Dauerbot uses the MIT license. It is an independent project with no official affiliation with HERDR, Feishu, or the CLI vendors.

## Connect your bot

The native app guides you through creating an enterprise custom app in the Feishu developer console, enabling its bot capability, and saving its App ID and App Secret locally. A group Webhook bot cannot provide the required two-way messaging.

Import the permission JSON supplied by the app, select long connections for event delivery, subscribe to `im.message.receive_v1`, and publish the app. The full permission set receives all group messages; the basic set receives group mentions. Add the bot to a group, or message it directly. Credential validation proves the app's identity; it does not prove that event permissions work.

Create a project by choosing a directory and an Agent CLI. The first project becomes the bot's fixed owner. You do not need a chat ID or a separate chat-binding step. Messages received before project creation stay queued. Additional local projects do not share this bot's messages or sending permissions.

Switch the active CLI in the workspace or with `hfb session switch`. Switching requires the current task to finish or be resolved, then closes the old execution and preserves the project, bot binding, progress and queues. The next task starts the selected CLI with continuity summaries. You can also send local tasks through the CLI before connecting Feishu.

Avoid running another long-connection listener for the same Feishu app: the platform distributes events between listeners, so another program may receive the message instead.

## Native desktop and local data

The desktop renders native GPUI controls. There is no browser interface, WebView, Node.js runtime, or frontend build step. Closing the app leaves its daemon and HERDR agent running.

The workspace has project and task controls on the left and the selected Agent's native terminal on the right. Observe is read-only; **Take control** enables direct input, login, model selection, and native prompts. **Release control** returns the session to automatic dispatch. Viewing does not prevent idle sleep. Explicit control pins the execution until released, protecting unsent input even if the desktop crashes. Closing a view disconnects its terminal bridge and leaves the Agent running. Create a task or schedule through its dedicated form; there is no duplicate chat history or message inspector.

Switch between **English** and **简体中文** in the app. The choice takes effect immediately and stays saved on this Mac. Existing projects open directly in the workspace.

Dauerbot checks only the project folders or SQLite files you select. Database checks read schema metadata rather than business records. Full Disk Access is optional and must be granted in System Settings to the application that actually performs the task. Granting access to Dauerbot does not automatically grant it to an existing HERDR or Agent process. Normal operation does not require Accessibility or Screen Recording.

The internal desktop executable remains `HerdrBridge`, the CLI is `hfb`, and the bundle ID remains `dev.herdr.feishu-bridge` so upgrades preserve existing settings and messages. On macOS, the default data directory is `~/Library/Application Support/dev.herdr.feishu-bridge`. `hfb init` prints the platform's actual directory. Unix directories use mode 0700 and configuration files use 0600. Credentials currently use local files protected by these permissions, not Keychain.

The desktop chooses an available loopback port on first setup; CLI commands read the same configuration. The daemon's management API uses a random bearer token and binds only to IPv4 loopback. Do not expose it directly to the public Internet.

Use `--data-dir` for an independent instance. Its window shows an independent-instance label and the actual path:

```sh
open "dist/Dauerbot.app" --args --data-dir /absolute/path/to/private-data
```

Each data directory derives its own dedicated HERDR session. A one-time transition preserves existing projects, credentials, native history references and messages, and clears old Core execution addresses. Projects with a previous live Core execution stay stopped until you explicitly start or resume them. This transition does not provide ongoing support for the former backend.

## Build and develop

Install Rust and Xcode on macOS. The dependency lockfiles define the exact Rust dependencies. The execution and terminal-stream adapter targets legacy HERDR 0.9.5 / protocol 22. The terminal component uses a licensed Rust fork of gpui-terminal with the Alacritty emulator. See [ARCHITECTURE.md](ARCHITECTURE.md) for ownership and lifecycle contracts.

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

The smoke test uses real SDK WebSockets, an isolated HERDR terminal, MCP stdio, and local Feishu fixtures. It checks delivery, files, deduplication, reconnection, daemon restarts, embedded terminal connections, warm retirement and task/timer wakeup. It uses private XDG configuration and HERDR sessions, with no real bot or model account. Test artifacts stay in ignored directories.

Live sending is opt-in: `tests/live_feishu.rs` reads `.local/live.json` with `app_id`, `app_secret`, and `chat_id`. Run `cargo test --test live_feishu -- --ignored` only for an explicitly selected bot and recipient, then remove the temporary credentials. It sends a test Markdown message and a small file without opening a competing event listener.

## CLI and HERDR sessions

```sh
hfb project add my-project --path /absolute/path/to/repo --agent codex
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

`--data-dir` is a global argument. For an isolated daemon, use `hfb --data-dir /absolute/private-data serve --port 49322 --herdr-session my-bridge`.

Dauerbot preserves native CLI account, model, provider, and reasoning settings. It does not invent model flags. On macOS, native agents start through a login shell inside HERDR, loading the same profile exports and proxy settings as your terminal. CLI arguments are passed literally, including prompts and paths with spaces. As configured for this project, Codex uses `--dangerously-bypass-approvals-and-sandbox`, Claude Code and Antigravity use `--dangerously-skip-permissions`, and Pi uses `--approve` for project files. Pi extensions can still enforce their own approval rules.

A main session is a native CLI process owned by the legacy HERDR server. Desktop terminal observation connects to that exact execution; CLI attach remains available for terminal users. Login and permission prompts remain in place, with an actionable attention notice. Dauerbot confirms recognized Claude Code and Antigravity project-trust prompts after verifying the exact project and execution identity. It attempts this once per execution and leaves other prompts for the user.

Use the same CLI's verified native session ID to resume history. Native hooks supply session identity where supported. Cross-CLI switching carries task summaries and files rather than claiming native transcript portability. Model selection stays native unless you explicitly supply an override; the terminal shows native model information, while unconfirmed metadata stays labeled unconfirmed.

| CLI | MCP configuration | Native resume |
| --- | --- | --- |
| Codex | Dedicated launch configuration; enables `--no-daemon` and `--dangerously-bypass-hook-trust` when the installed CLI advertises each flag, and forwards HERDR terminal identity | `codex resume`, using the session registered through MCP metadata or a supplied ID |
| Claude Code | Project-specific `--mcp-config` | A new UUID for a new session; `--resume` for recovery |
| Pi | Merges `herdr_feishu` into `.pi/mcp.json` | `--session` |
| Antigravity (`agy`) | Registers `herdr_feishu` in `~/.gemini/config/mcp_config.json`; the server resolves the current execution from verified HERDR identity. `-i` submits the initial interactive prompt | `--conversation` |

Configuration merges preserve other MCP entries and reject invalid JSON or an unrelated server using the same name. Ordinary agy sessions receive no Dauerbot project tools. Ignore generated local configuration in the target project. Custom CLI argument arrays run without a shell and can use `{mcp_config}`, `{credentials}`, `{bridge_exe}`, and `{instructions}`. Per-session credentials live under `sessions/<project>/<generation>/` and cannot administer other projects.

## Messages and MCP

The managed CLI starts `hfb mcp --credentials <private-file>` automatically.

| Tool | Arguments | Purpose |
| --- | --- | --- |
| `session_ready` | Optional `native_session` | Verify the terminal and process generation, then register readiness |
| `ack_message` | `message_id` | Confirm receipt of the nested Feishu message and its linked event |
| `send_markdown` | `to`, `text`, `request_id` | Send Markdown to the chosen chat or user |
| `send_file` | `to`, `path`, `request_id` | Stream a local file, up to 100 MB |

`to` is mandatory for MCP sends: `oc_…` identifies a chat and `ou_…` a user Open ID. The agent follows user intent and may reply somewhere other than the source chat. Reuse `request_id` only when retrying the same content and destination. Only `status=sent` means the API confirmed the send. Files must resolve inside the project or Dauerbot cache; symlinks cannot escape those directories.

Incoming messages use an escaped XML envelope with source IDs, sender, body, attachments, mentions, and thread context. Group messages include a bounded member snapshot when available. Direct messages omit group-only `<chat>` and `<members>` elements; they do not emit `not_applicable` placeholders. Missing group permissions produce `status="unavailable"` without blocking the message. User text remains untrusted input, even inside XML.

SQLite stores incoming and outgoing messages. Incoming events persist before SDK acknowledgment and deduplicate by project and platform message ID. Each project serializes runs. Acknowledgment confirms receipt; `finish_run` ends the current run before the next task is dispatched.

| Status | Meaning |
| --- | --- |
| `queued` | Saved and waiting for a ready, idle main session |
| `submitting` / `submitted` | Writing to the terminal / waiting for acknowledgment |
| `acknowledged` | The agent confirmed receipt; the task may still be running |
| `pending` / `sending` / `sent` | Outbound waiting / sending / API-confirmed |
| `failed` | A definite failure allows an informed retry |
| `uncertain` | Verify the result before retrying |
| `dismissed` | Manually excluded from blocking delivery |

Restarts preserve uncertain operations instead of blindly resending. PTY input and Feishu have no atomic transaction, so Dauerbot does not promise exactly-once delivery. It checks legacy metadata, root PID, generation, and foreground program identity before typing; exited agents and replacement shells do not receive queued tasks.

## Tasks, schedules, and Agent tools

Tasks, events, runs and timers live in SQLite independently of Agent processes. Once a native run has finished, the supervisor verifies idle state, queue emptiness, background jobs, session continuity and human takeover before beginning a ten-minute warm period. New work reuses a warm Agent or resumes a sleeping one. Stopping a project disables automatic wakeup until started again.

```sh
hfb task add my-project "Review dependencies" "Check for useful dependency updates"
hfb task list my-project
hfb timer add my-project "Daily review" "Review open tasks" --kind cron --cron '0 9 * * 1-5' --timezone Asia/Shanghai
hfb timer add my-project "Follow up" "Check the build result" --kind once --at '2026-12-01T09:00:00+08:00' --timezone Asia/Shanghai
hfb timer list my-project
hfb work my-project
hfb decision DECISION_ID "Proceed with the proposed approach"
hfb event dismiss EVENT_ID
# Retry only after checking whether the previous attempt already performed work.
hfb event retry EVENT_ID
```

Schedules support once, interval and UNIX five-field cron expressions with IANA timezones. Recurring work coalesces missed occurrences after downtime and avoids piling up another occurrence while one remains pending. Editing or cancelling a timer removes unsubmitted occurrences of its old revision. Active work requires execution cancellation or explicit uncertain-result recovery.

The MCP server exposes `session_ready`, `ack_message`, `ack_event`, `send_markdown`, `send_file`, `create_task`, `list_work`, `checkpoint`, `finish_run`, `schedule_task`, `request_decision`, `answer_decision`, `register_job`, and `complete_job`. Creation tools require stable request IDs for safe retries. Markdown/file recipients remain mandatory and Agent-selected. A checkpoint also writes readable task progress under the selected project's `.dauerbot/tasks/` directory.

Register background commands and subagents before launching them; report their completion so retirement can proceed. `hfb job register` and `hfb job complete` support external child reporting. Native hidden background activity still needs adapter evidence: silence and an answer on screen never prove quiescence. If an idle Agent omits `finish_run`, Dauerbot shows an unfinished-task notice after a grace period; review the result before retrying or dismissing its event. A deferred business decision can release the process; a native login or permission prompt retains its live terminal. Decisions and resolved child jobs create events through the same inbox.

An interrupted child can keep a task uncertain. Stop the project's execution, review the child result, and record your decision with `hfb job abandon JOB_ID "Reason for abandoning the reviewed work"`. Then retry or dismiss the parent event. Abandonment records the review without inventing a successful result; late completion reports cannot overwrite it.

## Updates and releases

The native app checks GitHub Releases at startup and every six hours while open. The stable channel accepts stable versions; preview also accepts prereleases. Selection uses semantic version order, not upload time or the Latest badge. Automatic updates download and replace the app in the background; the current desktop and HERDR agent keep running until you restart the desktop. The new desktop safely reconnects to a daemon using the same data directory.

App Updates supports manual checks, installation, disabling automatic updates, channel selection, and rollback. Rollback pauses automatic updates so the reverted version stays installed. Version 0.2.0 requires one manual installation of an updater-enabled version.

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

Release tags must reference commits on `main`. Update both crate versions, lockfiles, Info.plist, and CHANGELOG before staging. Build, test, and lint locally. Release automation first validates the tag and staged package metadata on Ubuntu, then starts macOS to verify the package, sign with Developer ID Application, submit to Apple notarization, staple the accepted ticket, and verify Gatekeeper acceptance. It uploads the zip, `SHA256SUMS`, `signing.txt`, `update.json`, and `update.json.sig` to a draft release and publishes only after GitHub's uploaded digests match. Public versions cannot be overwritten, and an older release cannot take Latest from a newer version.

The repository's Actions secrets hold `APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_ID`, `APPLE_APP_SPECIFIC_PASSWORD`, `APPLE_TEAM_ID`, and the Base64 32-byte Ed25519 seed `UPDATE_SIGNING_KEY`. The certificate must include the Developer ID Application private key for Team `366ADQ28F6`. Notarization credentials are stored only in the temporary runner keychain, which CI deletes after the run. A notarization failure prevents publication. The public update key lives in `packaging/update-public-key.txt`. Do not rotate it without a transition release that existing clients can verify. Manifest signing does not replace Apple notarization.

Merge the release source to main and run `python3 scripts/prebuilt-macos.py stage --tag vVERSION` from a clean checkout matching main, then push the version tag pointing to that staged main commit. The script builds and runs the existing Rust checks locally before uploading an ad-hoc package to a private draft release. Automation verifies its source commit, tree, version, architecture, size, and SHA-256, then signs and notarizes it without compiling again. The certificate and private key stay in GitHub Secrets. Staging assets are removed before publication. Missing or mismatched staging inputs stop the release; there is no cloud compilation fallback.

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
