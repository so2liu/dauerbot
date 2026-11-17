# Dauerbot

A native macOS workspace that connects Feishu messages and scheduled runs to native CLI agents running on demand in HERDR. Built with Rust and GPUI, with an independent daemon shared by the desktop app, CLI, and MCP server.

One bot belongs to one project with one active agent. Direct messages and group messages reach the same main session; the agent chooses where to send its replies. Dauerbot supports Codex, Claude Code, Pi, and Antigravity through their installed CLIs and existing account and model settings. Watch and take over the real terminal directly in Dauerbot. The lightweight daemon stays online; an idle Agent sleeps after ten minutes and wakes for new work. Dauerbot does not proxy model APIs.

## Install

Download the Apple Silicon package from [GitHub Releases](https://github.com/so2liu/dauerbot/releases), extract it, and move `Dauerbot.app` to Applications. The app includes the supported **HERDR 0.9.5** runtime and provisions it offline on first use when HERDR is missing. No separate HERDR installation is needed. Compatible existing installations continue to work; explicit `herdr` paths and `herdr_config` settings are preserved. An incompatible configured runtime produces a diagnostic instead of restarting an existing server. Sign in to the native Agent CLI you want to use; Dauerbot keeps its existing login and model settings.

Starting with 0.3.2, releases require Developer ID Application signing, Apple notarization, a stapled ticket, and Gatekeeper acceptance before publication. Intel builds and the minimum supported macOS version have not been verified.

Dauerbot uses the MIT license. Bundled HERDR uses Apache-2.0; its attribution and applicable vendored licenses are included in `Contents/Resources/herdr/`. It is an independent project with no official affiliation with HERDR, Feishu, or the CLI vendors.

## Connect your bot

Create a project by choosing a directory and an Agent CLI, then connect its dedicated bot. You can also keep the project local until you are ready. The native app guides you through creating an enterprise custom app in the Feishu developer console, enabling its bot capability, and saving its App ID and App Secret locally. A group Webhook bot cannot provide the required two-way messaging.

Import the permission JSON supplied by the app, select long connections for event delivery, subscribe to `im.message.receive_v1`, and publish the app. The full permission set receives all group messages; the basic set receives group mentions. Add the bot to a group, or message it directly. Credential validation proves the app's identity; it does not prove that event permissions work.

Each project has its own App ID, App Secret, event connection and connection status. The same bot cannot belong to two projects. New projects stay unbound until you connect their bot; they never inherit another project's credentials. Private and group messages are routed by the receiving bot, so you do not need a chat ID or a chat-binding step. Two bots can receive messages in the same group without sharing their project history or permissions.

From the CLI, bind and validate a project's bot with `hfb feishu set <project> --app-id cli_...` (the secret is read from stdin) and `hfb feishu check <project>`. `hfb feishu disconnect <project>` disconnects only that project. Existing singleton installations keep their bot on its recorded project when upgrading; other projects remain unbound.

Switch the active CLI in project settings or with `hfb session switch`. Switching closes the old execution and preserves the project files, bot binding, schedules and message history. The next input starts a new native conversation in the selected CLI. You can also inject a local message with `hfb message inject` before connecting Feishu.

Avoid running another long-connection listener for the same Feishu app: the platform distributes events between listeners, so another program may receive the message instead.

## Native desktop and local data

The desktop renders native GPUI controls. There is no browser interface, WebView, Node.js runtime, or frontend build step. Dauerbot starts in the macOS menu bar: click its icon to see project states, add a project, or open the full app. Selecting a project opens its details. Closing the window returns to the menu bar and leaves the daemon and HERDR Agent running; Quit exits the desktop.

The full window has projects on the left and scheduled runs in the center. The selected Agent's native terminal is a debugging tool on the right, collapsed by default. Expand it to observe; **Take control** enables direct input, login, model selection, and native prompts. **Release control** returns the session to automatic dispatch. Viewing does not prevent idle sleep. Explicit control pins the execution until released, protecting unsent input even if the desktop crashes. Hiding the terminal or closing the window releases control and disconnects the terminal bridge. Send instructions and ask the Agent to schedule work in Feishu; the desktop lists schedules and lets you pause or enable them. Agent execution steps and manual schedule forms are not part of the main view. Settings contains bot setup, permissions, updates, and occasional session/recovery controls.

Switch between **English** and **简体中文** in the app. The choice takes effect immediately and stays saved on this Mac. Project state changes also appear in the menu-bar panel.

Dauerbot checks only the project folders or SQLite files you select. Database checks read schema metadata rather than business records. Full Disk Access is optional and must be granted in System Settings to the application that actually performs the task. Granting access to Dauerbot does not automatically grant it to an existing HERDR or Agent process. Normal operation does not require Accessibility or Screen Recording.

The internal desktop executable remains `HerdrBridge`, the CLI is `hfb`, and the bundle ID remains `dev.herdr.feishu-bridge` so upgrades preserve existing settings and messages. On macOS, the default data directory is `~/Library/Application Support/dev.herdr.feishu-bridge`. `hfb init` prints the platform's actual directory. Unix directories use mode 0700 and configuration files use 0600. Bot credentials are stored in the private SQLite database, with file mode 0600. They are never returned in API responses. Native CLI accounts continue to use their own credential storage.

The desktop chooses an available loopback port on first setup; CLI commands read the same configuration. The daemon's management API uses a random bearer token and binds only to IPv4 loopback. Do not expose it directly to the public Internet.

Use `--data-dir` for an independent instance. Its window shows an independent-instance label and the actual path:

```sh
open "dist/Dauerbot.app" --args --data-dir /absolute/path/to/private-data
```

Each data directory derives its own dedicated HERDR session. App-managed runtime copies live under `runtime/herdr/0.9.5/<signed-payload-sha256>/herdr` in that directory. Installation uses a process lock and atomic rename, preserves older runtime paths across app updates/moves, and does not edit shell profiles or global PATH. Each Agent execution receives the resolved runtime directory in its own PATH after login-shell startup. Missing or corrupt owned copies are repaired from the verified bundle; symlink targets are rejected. User-owned executables and configuration files are never replaced. Normal schema upgrades preserve existing projects, credentials, native history references, messages and explicit pauses. Product execution uses only legacy herdr 0.9.5.

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

The smoke test uses real SDK WebSockets, an isolated HERDR terminal, MCP stdio, and local Feishu fixtures. It checks delivery, files, deduplication, reconnection, daemon restarts, embedded terminal connections, warm retirement and message/timer wakeup. It uses private XDG configuration and HERDR sessions, with no real bot or model account. The custom fixture records fresh launches and explicit native resume as test-only continuity metadata; it does not prove native CLI hooks or real-model acceptance. Test artifacts stay in ignored directories.

The cross-version regression uses the published, signed 0.4.4 app as the old daemon, replaces its bundle executable with the new build, and exercises the actual old and new MCP contracts:

```sh
python3 scripts/upgrade-smoke.py --old-app /path/to/extracted-0.4.4/Dauerbot.app --binary "$PWD/desktop/target/release/hfb"
```

It covers historical companion dispatch, missing-backup failure, a live old MCP across an incompatible daemon handoff, explicit native recovery, queued input delivered once, uncertain work retained, unrelated panes preserved, and compatible live-session reuse. Its private fixture Agent establishes native identity through HERDR and uses each published MCP contract normally; the harness does not acknowledge messages, patch the database, or replay effects.

Live sending is opt-in: `tests/live_feishu.rs` reads `.local/live.json` with `app_id`, `app_secret`, and `chat_id`. Run `cargo test --test live_feishu -- --ignored` only for an explicitly selected bot and recipient, then remove the temporary credentials. It sends a test Markdown message and a small file without opening a competing event listener.

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

Without `--path`, project creation allocates a private folder under `~/.dauerbot/agents/<project-name>-<four-random-letters>`. The desktop uses this default too; choosing an existing folder remains available. App-created folders receive a concise `AGENTS.md` for durable rules. Dauerbot never seeds or rewrites instructions in an existing folder. Keep current progress in `HANDOFF.md`.

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
| `send_md` | `to`, `text`, `request_id` | Send Markdown to the chosen chat or user |
| `send_file` | `to`, `path`, `request_id` | Stream a local file, up to 100 MB |
| `schedule_run` | `request_id`, `name`, `prompt`, `kind`, `timezone`, schedule fields, optional `to` | Persist a once, interval, or cron run |

`to` is mandatory for MCP sends: `oc_…` identifies a chat and `ou_…` a user Open ID. The agent follows user intent and may reply somewhere other than the source chat. Reuse `request_id` only when retrying the same content and destination. Only `status=sent` means the API confirmed the send. Files must resolve inside the project or Dauerbot cache; symlinks cannot escape those directories.

Incoming messages use an escaped XML envelope with source IDs, sender, body, attachments, mentions, and thread context. Group messages include a bounded member snapshot when available. Direct messages omit group-only `<chat>` and `<members>` elements; they do not emit `not_applicable` placeholders. Missing group permissions produce `status="unavailable"` without blocking the message. User text remains untrusted input, even inside XML.

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

## Sessions and scheduled runs

The supervisor observes native HERDR identity, activity, foreground processes and manual control. A verified idle session starts a ten-minute warm period. After ten idle minutes, Dauerbot asks the Agent to update `HANDOFF.md` with its goal, completed work, decisions, relevant files/commits, pending issues and next actions. Retirement requires an updated artifact bearing that request’s marker and a later verified native idle transition. New input cancels a pending handoff and steers the live Agent; input arriving after the retirement boundary wakes a fresh session immediately. The fresh session must read `AGENTS.md` and `HANDOFF.md` before new work. A failed or timed-out handoff preserves native recovery and never retries automatically; cancelled requests can try again after another idle period, at most three times per execution. Stopping a session ends its current process; later messages wake a fresh execution of the same native conversation. Explicitly pausing automatic activity holds new input. An uncertain delivery stays recorded without blocking fresh messages or independent schedules. Uncertainty in the current execution prevents automatic handoff retirement; uncertainty from an older execution does not pin a fresh one. Native idle describes execution state, not proof of successful business work or a completed external effect.

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

Schedules support once, interval and UNIX five-field cron expressions with IANA timezones. Recurring schedules coalesce missed occurrences after downtime and avoid overlap while their own input remains pending. Editing, disabling or deleting a timer cancels unsubmitted occurrences of its old revision. The native timer view retains enable/disable/delete controls.

Managed MCP exposes exactly `send_md`, `send_file`, and `schedule_run`; unmanaged sessions expose no project tools. MCP privately authenticates the project, generation and HERDR pane at startup. Stable request IDs deduplicate sends and schedule creation. Background execution belongs to HERDR and the Agent. There is no separate Dauerbot task or job registry.

Normal database upgrades preserve messages, schedule IDs, occurrence history and uncertain outcomes. Legacy journal containers remain internal for their foreign-key relationships. Previously submitted or uncertain input is never requeued by this upgrade. API protocol 6 and application version 0.4.6 identify private workspace creation and verified handoff retirement; incompatible desktop/daemon combinations must reconnect using matching builds.

## Updates and releases

The native app checks GitHub Releases at startup and every six hours while open. The stable channel accepts stable versions; preview also accepts prereleases. Selection uses semantic version order, not upload time or the Latest badge. Automatic updates download and replace the app in the background; the current desktop and HERDR agent keep running until you restart the desktop. The new desktop safely reconnects to a daemon using the same data directory.

Reopening or updating the desktop reuses an authenticated daemon with the same API protocol, even when its version differs. The Updates view shows both versions. Choose **Update background service** to switch the daemon to the bundled version when ready; this briefly reconnects Feishu and local tools. Compatible Agent sessions remain running. A protocol-incompatible daemon still requires a trusted installed app to replace it, and incompatible live Agent generations show **Agent update required**. Select the project and choose **Recover Agent after update** to replace only that managed execution and resume its saved native session. If no native session ID was saved, the notice explains that recovery starts fresh. Saved input continues automatically when delivery is enabled; interrupted or uncertain effects are retained for review.

Before accepting requests, the daemon retains its hfb executable under its private data directory at `runtime/hfb/<version>-api<protocol>/<sha256>/hfb`. MCP configurations, global managed MCP entries and custom `{bridge_exe}` commands use that path. Publication is atomic and never overwrites a retained inode; ownership, permissions, content hash and executable identity are checked.

The daemon collects obsolete companions at startup and hourly on a blocking background worker. Each sweep examines at most 512 entries and checks at most eight deletion candidates. It retains its own companion lease, every terminal-bearing project's recorded companion and current-generation MCP command (including paused, taken-over and update-required executions), persistent managed Pi and Antigravity commands, literal companion paths in saved launch arguments (including stopped projects), and the exact SHA-256 of the updater's validated signed rollback executable. Older executions without a recorded companion retain their matching protocol until retirement; an unknown execution protocol skips collection. Pi configuration locations are recorded in `runtime/hfb/integrations.json` before publication and during collection, so known project configurations remain roots after project deletion. Only exact known project `.pi/mcp.json` paths and the global `~/.gemini/config/mcp_config.json` are read; no home-directory search is performed.

Retired `sessions/<project>/<generation>/mcp.json` and credentials remain on disk but stop retaining executables once they are no longer the project's terminal generation. New native recovery commands use the current companion and explicit generation configuration; Pi and Antigravity's persistent commands remain independent roots until removed or replaced. Native session history does not itself relaunch archived bootstrap commands.

A filesystem lock serializes companion publication and collection. Shared per-entry leases cover publication through durable reference creation; the daemon holds its lease for its lifetime. Historical dispatch explicitly inherits its lease across `exec`, including into older executables that do not implement leases. Before deletion the collector requires an exclusive lease and a successful OS no-holder check; legacy protocol roots also protect future children of existing old Agents. Tool errors, permission warnings, timeouts and uncertain reference state prevent deletion. Only validated, owned, regular content-addressed files are removed individually; symlinks, unknown files, bundles, native HERDR sessions, credentials and user files are preserved. The separate bundled HERDR runtime is outside this collector.

Already-running older daemons cannot acquire pinning logic retroactively. When one launches the new hfb through a replaced bundle path, the new MCP authenticates the local daemon, verifies its version and generation, and dispatches to the matching original executable from `updates/installed.json`'s preserved signed bundle. The historical executable must pass Apple Team, bundle and exact-version checks before retention. The new MCP does not implement retired lifecycle tools. If neither that backup nor a valid retained historical companion survives, startup fails with actionable `update_required`; open the installed app and recover the affected project after its service handoff. Historical desktop binaries cannot be retroactively given the new recovery UI.

The desktop captures the authenticated prior service protocol before a handoff. This lets generations without protocol metadata remain usable when that known protocol matches; records are applied only to the captured project, generation and terminal. A generation with neither its own protocol nor a matching prior record requires explicit recovery because compatibility cannot be verified safely. Service replacement never silently converts an old live MCP child or restarts unrelated terminals.

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
