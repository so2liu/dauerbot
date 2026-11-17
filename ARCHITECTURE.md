# Dauerbot architecture

Current contract: application 0.4.5, API protocol 5, store schema 7. Source review, isolated protocol checks and real CLI acceptance provide separate evidence; see TESTING.md for recorded results.

Each project connects its own Feishu bot to its main native Agent session. The desktop manages projects and scheduled runs on the left, with the selected Agent's interactive terminal on the right. An independent Rust daemon keeps receiving events and managing schedules while Agent processes sleep.

## Ownership

Dauerbot owns projects, the message delivery journal, schedules, attention records and outbound delivery. Legacy `herdr` 0.9.5 owns Agent PTYs and process lifetimes in a dedicated named session. Native CLIs own model authentication, model settings and native conversation history. The desktop owns terminal presentation and view/controller bridge processes, never a second Agent process.

The legacy `herdr` backend and the newer `hc` / herdr-core / Herdr App backend are different systems. Product execution uses the legacy backend. External development subagents may use hc without changing the product backend.

## Project and bot identity

`project_bots` stores one private credential pair per project and enforces a unique App ID across projects. A new project has no bot until its owner explicitly connects one. The desktop opens that connection step after project creation, with an option to continue locally. Native Agent selection is independent of bot identity.

Each binding owns its SDK event listener, reconnect state and access-token cache. Binding changes restart only the affected listener. The receiving listener supplies the project and App ID to a transactional inbox insert; chat IDs never choose the destination project. Conversations and cached group snapshots are scoped by `(project_id, chat_id)`, so two bots can participate in the same group without mixing their data. Outgoing messages retain their bot App ID and cannot silently send through a replacement bot.

Schema 5 moves the former singleton credentials to their recorded project owner and removes the singleton binding. Existing unbound projects remain local. Credentials are stored in the private SQLite database, never returned through the desktop/API, and retained safely across daemon restarts.

## Input and delivery journal

Feishu messages and fired schedules enter a SQLite inbox. A per-project lock serializes terminal writes and execution transitions, while projects progress independently. An event records input provenance and delivery state; an internal run records one submission attempt and its execution generation. These records do not represent Agent objectives, progress or successful business outcomes. Conversation records retain their meaning as Feishu chat metadata.

Incoming conversation messages have priority and steer the current native Agent immediately, including while it is working. Native paste-and-Enter submission preserves ongoing tools and uses the CLI's steering behavior. Explicit pause, takeover and native login/permission prompts suspend automatic input. Scheduled runs dispatch when the native Agent is idle. Uncertain input stays recorded for explicit review and never blocks fresh user direction or independent schedules.

Outbound sends use stable request IDs. A repeated request must match its original content and destination; uncertain sends never receive an automatic retry. The managed MCP surface contains exactly `send_md`, `send_file` and `schedule_run`. Questions and their answers travel as ordinary Feishu messages. Agents manage their own background execution through HERDR and native facilities.

## Wakeup and retirement

Incoming messages and eligible schedules automatically wake or reuse the project's Agent. Stop ends the current execution; later input resumes the native conversation without replaying interrupted uncertain input. Only explicit pause or manual terminal takeover holds new input. Timers persist their occurrences even while no Agent exists. The same project lock coordinates retirement with dispatch so new input cannot disappear in a concurrent shutdown.

Native HERDR identity and working/idle observations drive activity. MCP startup registers its authenticated generation privately; the Agent performs no readiness handshake. The daemon also recognizes native readiness without waiting for an MCP tool. Native hooks supply session references where supported, and the daemon verifies project, generation, managed name and process ownership before dispatch.

After observing native idle, the daemon settles submitted journal attempts whose native transition follows submission. A matching old state sequence cannot settle a new prompt. Older journal records without sequence metadata settle on verified native idle. When an observation omits a previously available sequence, a two-second guard prevents immediate settlement from the submission snapshot. Neither path changes uncertain delivery into success. Settled journal entries release a timer's pending-occurrence gate without claiming its business objective succeeded.

The Agent stays warm for ten idle minutes. Retirement requires an empty eligible input queue, no native blocked prompt, no manual takeover, a verified resumable native session and foreground-process evidence that the CLI is quiescent. Uncertain history and removed task/job records don't pin the process. Unknown foreground processes keep it warm. Native history resumes only within the same CLI; switching CLI preserves project files, bot binding, schedules and message history, closes the old execution and starts a new native conversation when needed.

## CLI compatibility

Installation identity includes executable, machine, version and transport. Runtime-specific adapters prepare argv, discover native session references, interpret observations and expose capability support. Preserve native login/model configuration and existing proxy variables. A requested model and a confirmed model are separate values; starting successfully does not confirm a model.

The legacy managed integrations can report session IDs for Codex, Claude Code, Pi and agy. Their installation and lifecycle evidence differ. Preserve unrelated user configuration and verify hooks in isolated tests. Native readiness, input submission, native idle and foreground quiescence remain distinct observations.

Headless app-server, stream-json, RPC and ACP implementations in Raft, Multica and OAR provide useful reference behavior. They do not automatically preserve interactive TUI takeover. The selected transport keeps native interactive CLIs in legacy HERDR.

## Embedded native terminal

The desktop consumes legacy terminal session observe/control streams. ANSI frames feed a reusable terminal emulator with a GPUI renderer. The control path handles keyboard input, Chinese IME, paste, resizing, mouse and explicit release. No WebView, browser frontend or JavaScript build toolchain is used.

Viewing and control have separate semantics. Viewing does not hold the Agent awake indefinitely. Explicit control obtains an execution-generation lease; queued daemon work remains durable. Closing the panel releases only the connection. Reconnecting validates project, backend, terminal and generation before sending any input. Sleeping projects display their state without waking merely to render a page.

## Scheduled runs

Support once, interval and cron schedules with an IANA timezone. Persist the schedule, revision and next occurrence. Transactional firing records an occurrence and event before advancing the timer. After downtime, recurring work coalesces missed occurrences into one check, and a still-pending check prevents overlapping repeats. Users and Agents use the same schedule operations; users can inspect, pause, edit and cancel them.

Agents may schedule future checks through `schedule_run`; the native timer view and CLI retain pause, enable, edit and delete operations. No daemon task registry or progress synchronization is required.

## Attention and questions

Native login, permission and startup problems create daemon-owned attention records before MCP is ready. Present a short reason, affected work and the appropriate action in the desktop; notify the responsible Feishu recipient when known. Repeated observations update the same item. Reading a notice does not resolve its underlying problem.

An Agent asks the user through `send_md` and incorporates the ordinary Feishu reply as steering. A native CLI prompt retains its live interactive process. Existing authorized automatic workspace-trust handling remains available.

## Compatibility and delivery

The product supports fresh stores and normal upgrades from schema 4 onward. Schema 7 keeps existing messages, schedule IDs, request identities, revisions, occurrence history and next-fire times. Compatibility tables retain internal journal foreign-key relationships; obsolete decision and job tables and their API/CLI/UI surfaces are removed. Upgrades never requeue previously submitted or uncertain input. Existing explicit pauses, bot credentials and native conversation references survive.

API protocol 5 and application version 0.4.5 mark the incompatible Agent/daemon contract. Desktop and CLI check protocol compatibility. The private MCP registration endpoint also checks protocol before exposing project tools, so an old daemon cannot silently accept the new lifecycle. Older desktops see the newer version and must not replace its daemon with an older build; the schema guard rejects attempts to open a newer store. Bundle identity, data directory and update signing keys stay stable.

Product execution uses legacy `herdr` 0.9.5. There is no hc/Core runtime adapter or Core archive. Source, documentation, default UI and commits use English; the desktop supports persistent English and Simplified Chinese preferences.

## Agent wire compatibility during activation

The Agent MCP wire contract is independent of the desktop/service API. New
executions and their private credentials carry `mcp_contract: 1`. Contract 1
provides exactly `send_md`, `send_file`, and `schedule_run`, including topic
anchors, schedule purpose, generation-scoped authentication, native registration,
request deduplication, and uncertain-effect preservation. Registration must return
the negotiated `mcp_contract`; matching tool names alone do not establish support.
The service advertises supported identities through `mcp_contracts` in `/api/state`.

`mcp::check_activation(projects, target_api)` checks the authenticated owner's
project snapshot before ownership changes. It accepts explicit contract 1 across
engine revisions. Published 0.4.7 generations without explicit metadata are also
supported with public API 7: their original startup probe requires that exact API
before private registration. Published API 5/6 and unknown contracts are not
admitted merely because their tool names overlap. Unsupported active generations
keep the old service serving. Sleeping records do not hold an active MCP process.

A supported activation preserves the original pane, native conversation,
generation, project token, credentials, and immutable companion. It does not stop
and resume the native CLI or hot-swap an existing stdio MCP child. Legacy
registration receives its generation's API value; modern registration negotiates
the stable wire identity independently and reports the current service API
separately. Existing execution metadata is never relabeled to claim compatibility.

An unmodified 0.4.7 service does not return `mcp_contract` and cannot host a newly
created explicit-contract MCP client. A post-commit rollback must therefore check
all current generations before switching ownership back, or retain a compatible
engine. Restoring an old bundle is not sufficient proof of runtime compatibility.
Pre-commit rollback leaves the original legacy generations and old service intact.

## Automatic service handover

Handover-capable services expose authenticated operator endpoints under
`/api/service/activation/{prepare,commit,status,abort}`. These are desktop/update
coordination endpoints, not additional Agent MCP tools. The public API remains 7;
`/state.service_activation.capability` advertises the independent handover contract.

Preparation starts the verified candidate with inherited duplicates of the bound
loopback listener and daemon lock, plus an anonymous control socketpair. Descriptor
inheritance is resealed before the candidate can start children. The candidate
validates/migrates a private SQLite snapshot and checks native MCP compatibility;
it never constructs the live App or performs crash recovery during preparation.
The configuration fence comes from that exact snapshot, not a later live read.

Feishu officially supports clustered long connections: one connected client is
chosen for each event. Candidate transports may warm while both sets of callbacks
commit through the old single journal owner. A small exact-version OpenLark patch
reports the actual WebSocket upgrade handshake. Its event/ACK behavior is unchanged:
ACK200 follows synchronous journal ingestion; relay failure withholds success.
After cutover, warmed connections remain supervised and reconnect normally.

The old owner closes effect admission and drains existing operations. Mutating
HTTP work runs in owned tasks so losing its response waiter cannot release an
in-flight effect permit. Scheduler, native reconciliation, outbox and collection
work share the same outer barrier. Read-only MCP probes remain available; newly
arriving mutations wait, then proxy with their original IDs and authority to the
new owner. At the 95-second admission deadline an unadmitted request receives a
retryable error. A forwarded operation with a lost response remains uncertain;
that case must not be described as never accepted.

`service-activation.json` atomically records the transaction and a separate nested
`serving_owner` fact. New preparations preserve the previous owner; commit replaces
it with a fresh epoch. The candidate opens the latest live journal only after that
boundary. Abandoned transient records become uncertain after old effects drain,
not during preparation. A public request must confirm the expected new owner PID
before the old process exits. Listener address, token, project generations, native
sessions, timer occurrence keys and immutable MCP commands remain unchanged.

Failure before commit leaves the old owner serving. Failure after commit never
restores an old snapshot or reopens old writers: the retained descriptors supervise
bounded restarts of the verified new executable with a fresh epoch and latest-store
crash recovery. A normal later restart must match the recorded owner executable.
The GUI coordinator owns signed bundle rollback and the explicit legacy bootstrap
claim. Published 0.4.7 lacks this transfer protocol; its first upgrade has a bounded
reconnect boundary. Unsupported future service/MCP/schema contracts are refused
before cutover rather than being treated as automatically compatible.
