# Project conventions

Dauerbot is an open-source project for a global audience. English is the project language. Use English for repository descriptions, README and other documentation, code comments, user-facing UI and CLI text, release notes, commit messages, issue titles, and pull request titles and descriptions. This project-specific rule overrides the parent's default of Chinese commits and PRs. Preserve user content, proper names, and multilingual test fixtures in their original language. Do not rewrite published Git history to translate old commits.

The user-facing client is the native Rust/GPUI macOS application in `desktop/`. Do not add a web interface, WebView, Svelte, or a JavaScript build toolchain. Desktop, CLI, and MCP share the independent Rust daemon's API. Network requests and file access checks must never block GPUI's main thread.

Check macOS access against paths that the user explicitly selects. Do not scan private databases, call private TCC APIs, or treat one accessible file as proof of Full Disk Access. Explain that the desktop, daemon, HERDR, and Agent can have different permissions.

Do not change a user's real Core/App configuration or terminate their terminals. HERDR tests must use an isolated `HC_SESSION` and a short `XDG_RUNTIME_DIR`. Keep the default bundle ID and data directory compatible across renames and upgrades; clearly label windows that use independent data directories.

Keep credentials out of Git, logs, and screenshots. Live Feishu tests must name the intended bot and chat and must not compete with an existing bot's event listener. Message submission does not prove completion; preserve `uncertain` results rather than retrying automatically.

Remove obsolete code and dependencies with the feature they served. Preserve storage migrations and compatibility required to read existing user data. Add tests only for meaningful behavioral regressions. Run the relevant Rust tests and clippy; use the isolated protocol smoke test when changing daemon, CLI, MCP, or message handling.

Package the native app with `scripts/build-desktop.sh`; it must build without Node.js or pnpm. Local ad-hoc signatures are for development, not proof of Developer ID signing, notarization, or Gatekeeper acceptance. Public macOS releases must use Developer ID Application signing, Apple notarization, a stapled ticket, and Gatekeeper acceptance before packaging. Publish version tags only from `main`. Never overwrite a public release or silently change the update signing key.

Keep GitHub Actions usage low. Routine pushes and pull requests must not start native builds. Run the relevant build, tests, and clippy locally; cloud native checks are manual only. Releases require a locally built and tested staged package. Validate the tag and staging metadata on Ubuntu before allocating macOS for signing and notarization. Never add an automatic cloud compilation fallback.
