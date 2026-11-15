# Verification record

## Native CLI environment, 0.3.3

On November 16, 2026, macmini's working Antigravity session had shell proxy exports that its Dauerbot-launched session lacked. Direct requests to Google's OAuth and model-service hosts failed; requests through the existing proxy returned HTTP responses. Under the GUI login context, `agy models` listed 22 models and a real model turn returned `DAUERBOT_AGY_OK` with the proxy. The corresponding turn without proxies timed out. SSH-only processes did not have the GUI keychain unlocked and were not used to infer the user's interactive sign-in state.

The final implementation starts native agents through a login shell in HERDR, with arguments passed positionally to `exec`. A macmini GUI-launched validation daemon inherited no proxy settings, used an independent data directory and Core session, and launched the actual Antigravity CLI 1.5.9. The agent inherited all six existing upper/lowercase HTTP, HTTPS, and bypass proxy settings, completed a real model turn, called the MCP `session_ready` tool, and reached `ready=true`. Neither the constructor error nor the empty model picker appeared. The isolated agent, daemon, and Core were stopped after the check.

Regression checks verify actual profile loading, literal argument forwarding with shell metacharacters, and rejection of an initializing or restored shell even when terminal labels match. The existing checks continue to verify native model defaults and explicit user model arguments.

All 22 backend tests and three desktop tests passed; the opt-in live Feishu test remained ignored. Both release clippy checks passed. The isolated protocol smoke passed all 12 checks, including message delivery and daemon/Core restarts. Evidence: `test-results/cli-network/macmini-agy.json`, `.local/smoke-6adb8f27`, and `test-results/smoke.json`. The package was built locally without starting native Actions checks.

Date: November 15, 2026. Local environment: macOS on Apple Silicon, Rust 1.96.0, HERDR Core 0.1.20. These are development checks, not a cross-platform or long-running production load test. Evidence lives in ignored `test-results/` and `.local/` directories and must not contain credentials.

## Actions budget policy

Routine push and pull-request triggers have been removed. Full native checks are manual only, with a 30-minute limit and cancellation of an older manual run for the same ref. Release automation requires staged local output; an Ubuntu preflight must succeed before macOS signing can start. The release workflow contains no Rust toolchain setup, cache restore, or compilation fallback.

Local validation passed 11 staging scenarios: missing draft, missing metadata, missing archive, wrong commit, wrong tree, wrong version, invalid size, invalid checksum metadata, valid metadata inspection, actual macOS archive restoration, and rejection of a tampered package checksum. The archive restoration used the existing locally built 0.3.2 package and real `ditto` and `codesign`. Workflow parsing confirmed manual-only native checks and the Ubuntu-to-macOS job dependency. Python and shell syntax checks passed. Evidence: `test-results/actions-budget/staging-checks.json`.

## Developer ID and notarization, 0.3.2

The existing ma-agent Developer ID Application certificate and notarization credentials were transferred directly between GitHub Actions secret stores using sealed-box encryption. Only destination-encrypted values left the source runner. The certificate belongs to the existing Apple Team, `366ADQ28F6`. The transfer artifact and temporary source branch were removed; ma-agent main and product source were unchanged.

The native 0.3.2 package builds without Node.js. All 20 backend tests and three desktop tests pass; the optional live Feishu test remains ignored. Both clippy checks pass. The release YAML and Python scripts parse successfully. A local preflight check confirms the notarization script rejects an ad-hoc app before attempting to access notarization credentials.

Release acceptance requires an actual Apple submission, a stapled ticket, and successful `codesign`, `stapler`, and `spctl` checks against the downloaded public package. CI retains the submission receipt for inspection. These checks must complete before closing the Gatekeeper issue.

Release run 43726215368, attempt 2, completed that acceptance after the account holder signed the pending Apple agreement. The notarization, stapling, and Gatekeeper step took 31 seconds. The downloaded public package passed checksums, `codesign`, `stapler`, `spctl` (`Notarized Developer ID`), and `syspolicy_check distribution`. The installed 0.3.1 updater successfully installed 0.3.2; both desktop and daemon run 0.3.2. The configuration digest, bot binding, native session, terminal t1x8, and Agent PID 17625 stayed unchanged. All 12 isolated HERDR/MCP/simulated-Feishu smoke checks passed, and an independent native UI instance created a local project successfully. Gatekeeper issue #8 is closed.

## Current native-only cleanup

The native desktop no longer depends on Svelte, Vite, Playwright, Node.js, pnpm, embedded HTML, or a web development server. The daemon retains its authenticated API for the desktop, CLI, and MCP. Browser-only group discovery, manual chat binding, private-default routing, and SSE refresh endpoints have been removed. Existing storage migrations and user data remain compatible.

English is the default project language. User-provided project names, messages, and multilingual protocol fixtures retain their original text. Existing published Git history has not been rewritten.

The native package builds without Node.js or pnpm. Twenty backend tests and three desktop tests passed; the opt-in live test stayed ignored. Root and desktop clippy passed. The isolated protocol smoke passed all 12 checks (`.local/smoke-39f0470d`, `test-results/native-only-smoke.log`). A real bundled `hfb ui` launch opened the English GPUI window and selected an available port rather than preempting the default port. API inspection confirmed that `/`, `/api/events`, and `/api/onboarding/chats` return 404. The independent preview was closed and its own daemon shut down after verification. A second native screenshot verified that the English welcome cards fit within the content area after removing their intrinsic minimum width.

## Configuration and independent instances

An update-page preview used an empty independent directory while its old window title still said Dauerbot. That made it look as though the real Feishu configuration had disappeared. The preview was closed and the installed app reopened with its original data directory. API inspection and a native screenshot confirmed the existing project, bot binding, ready agent, and original terminal t1x8/PID 17625. The shared Core and user Agent were not restarted during this recovery.

Independent directories now have a distinct window title, a visible banner, and their actual path. A native screenshot verified the marker before closing the preview and restoring the everyday workspace. The desktop build and clippy passed.

## Updates and signing

The updater checks signed manifests, semantic versions and channels, bounded downloads and extraction, Apple Team, bundle ID, architecture, and executable version. It retains a rollback bundle and restores the old app when replacement fails. Rollback pauses automatic updates and restarts only the daemon associated with the selected data directory. Project MCP credentials cannot invoke administrative shutdown.

Before the native-only cleanup, 21 backend tests and three desktop tests passed; the opt-in live sending test remained ignored. Root and desktop clippy passed. GPUI's upstream `block 0.1.6` dependency reports a future-compatibility warning. The native update page was opened through the macOS menu and visually checked. Coordinate clicks fail in the available computer-control interface, so update button clicks are not claimed as fully automated GUI acceptance.

The first 0.3.0 CI run built, tested, signed the app, signed the manifest, and uploaded all five assets. Publication stopped because GitHub's release-by-tag API did not resolve the draft. The publication script now resolves the draft by numeric release ID before checking digests. The replacement v0.3.1 Release run 43721151816 completed build, tests, Apple signing, manifest signing, upload digest checks, and public publication successfully. All five assets are public. The 16,545,752-byte package has SHA-256 `aba9e9f95c644df97bb0b16cceb05e3938cfa06f786884d59702508fcba8756e`, matching both the verified download and GitHub asset digest.

An isolated signed 0.2.0 app upgraded to 0.3.1 through the real updater, then the new desktop switched its daemon to 0.3.1. Rollback restored both app and daemon to 0.2.0 and paused automatic updates. Reinstalling 0.3.1 reused the verified cached package. At every stage, the project, fixed bot binding, queued message, and byte-for-byte configuration digest stayed intact. Evidence: `test-results/update-e2e/result.json` and `acceptance.log`.

The installed everyday app then received the verified signed 0.3.1 bundle at its existing path. Its daemon reports 0.3.1; the original project remains ready on terminal t1x8 with Agent PID 17625. The bot binding and configuration digest are unchanged. A native screenshot confirmed the English workspace, 0.3.1 version, and existing project. The old app bundle is retained locally for recovery. No user Agent, MCP process, or shared Core was terminated.

## Message and session checks

The isolated SDK/HERDR/MCP/Feishu fixture smoke passed 12 checks before this cleanup. It covered Markdown and file sends, explicit recipients, deduplication and ordered turns, newly discovered private and group chats reaching the fixed bot project, reconnection, daemon restart preserving the original terminal and queue, isolated Core restart, and messages remaining queued after stopping an agent.

Rust regression coverage includes queue fairness, ACK ownership, generation and PID verification, canonical attachment paths, symlink boundaries, local API authentication and Origin checks, project-scoped MCP access, uncertain recovery, and native model arguments. Private-message XML omits group-only metadata and empty mentions; group metadata failures remain explicit without blocking delivery.

Real local CLI checks previously completed one model turn each with Pi 1.5.1, Claude Code 2.1.319, and Codex CLI 0.167.2 against the local Feishu fixture. A separate explicitly authorized live test sent one Markdown message and one small file and received platform message IDs. The temporary credentials were removed. Those historical checks do not establish current account login or live event permissions.

## Service recovery and permissions

A prior isolated recovery test terminated its daemon while leaving the GUI running. The GUI started a replacement daemon and reconnected with HTTP 200. A native screenshot confirmed that the stale connection error cleared. Local control requests bypass external proxies; normal polling reloads changed local endpoint settings after failures.

Selected-directory access tests clean up temporary probe files. SQLite probes preserve database contents and distinguish missing files from an actual OS permission denial. No private databases were scanned and no Full Disk Access was granted automatically. The desktop, daemon, and HERDR Agent have separate OS access contexts; successful desktop checks do not establish Agent permissions.

Recognized Antigravity directory-trust prompts require the expected project path, PID, generation, structured trust state, and selected Yes option. Other prompts remain in HERDR for interactive handling. The initial Antigravity model error was reproduced with a bare CLI and resolved by forwarding existing proxy variables into the Core child process; no model/provider override was introduced.

## Historical memory sample

For one idle project with the desktop open, five RSS snapshots over four seconds were stable: GUI 108.5 MiB, daemon 26.4 MiB, MCP 9.6 MiB, Core connection helper 7.0 MiB, and Antigravity 250.8 MiB. The Dauerbot background processes totaled about 43.0 MiB, or 151.5 MiB including the GUI; including the agent gave 402.3 MiB. Sampled ps CPU values were 0.0%. Shared Herdr App/Core and unrelated agents were excluded. RSS sums can double-count shared pages and do not represent peak or GPU memory, or the current release's measured footprint. Evidence: `test-results/memory-current.json`.
