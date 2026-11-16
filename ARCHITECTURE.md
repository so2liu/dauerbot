# Dauerbot architecture

Status: implementation approved following the initial independent agy source review, 2026-11-16. Final review and acceptance checks continue during implementation.

Dauerbot connects one Feishu bot to one project and its main native Agent session. The desktop manages projects and tasks on the left, with the selected Agent's interactive terminal on the right. An independent Rust daemon keeps receiving events and managing schedules while Agent processes sleep.

## Ownership

Dauerbot owns durable task/event/run state, schedules, user decisions, attention records and outbound delivery. Legacy `herdr` 0.9.5 owns Agent PTYs and process lifetimes in a dedicated named session. Native CLIs own model authentication, model settings and native conversation history. The desktop owns terminal presentation and view/controller bridge processes, never a second Agent process.

The legacy `herdr` backend and the newer `hc` / herdr-core / Herdr App backend are different systems. Product execution uses the legacy backend. External development subagents may use hc without changing the product backend.

## Durable work

All task triggers enter a SQLite inbox: Feishu messages, explicit tasks, fired timers, decision answers and tracked child results. Administrative operations such as agent selection, delivery pause and schedule edits update state directly. A per-project supervisor serializes dispatch and execution transitions; projects progress independently.

A task stores its objective, progress, destination and waiting condition. An event records what made work runnable. A run records one processing attempt. An execution records a particular native CLI process generation and its legacy terminal identity. Existing conversations retain their meaning as Feishu chat metadata. Outbound messages retain stable request IDs and uncertain sends never receive an automatic blind retry.

Initially all tasks use the project main session in order. Native history is resumed only within the same CLI. Switching CLI preserves durable progress and project memory, then begins a new native session with explicit continuity context.

## Wakeup and retirement

The supervisor wakes or reuses an Agent for eligible work. Timers create durable inbox events, even while no Agent exists. Queue insertion and project state transitions share a coordination boundary so retirement cannot lose a concurrent message.

After a finished run, keep the Agent warm for ten minutes. Retirement requires no active or uncertain turn, no eligible event backlog, no active tracked child work, no native blocked prompt, no human takeover, and a verified native session reference for future resume. Unknown execution quiescence is a state to resolve; silence, output delivery and acknowledgement do not establish completion.

A run completion report records the outcome and progress. Native turn boundaries and explicit background-job tracking provide separate execution evidence. Long-running background work registers with the daemon and reports completion. A user takeover lease prevents retirement and automatic task input until control returns.

## CLI compatibility

Installation identity includes executable, machine, version and transport. Runtime-specific adapters prepare argv, discover native session references, interpret observations and expose capability support. Preserve native login/model configuration and existing proxy variables. A requested model and a confirmed model are separate values; starting successfully does not confirm a model.

The legacy managed integrations can report session IDs for Codex, Claude Code, Pi and agy. Their installation and lifecycle evidence differ. Preserve unrelated user configuration and verify hooks in isolated tests. Startup/readiness, input submission, receipt, turn completion and background quiescence remain distinct.

Headless app-server, stream-json, RPC and ACP implementations in Raft, Multica and OAR provide useful reference behavior. They do not automatically preserve interactive TUI takeover. The selected transport keeps native interactive CLIs in legacy HERDR.

## Embedded native terminal

The desktop consumes legacy terminal session observe/control streams. ANSI frames feed a reusable terminal emulator with a GPUI renderer. The control path handles keyboard input, Chinese IME, paste, resizing, mouse and explicit release. No WebView, browser frontend or JavaScript build toolchain is used.

Viewing and control have separate semantics. Viewing does not hold the Agent awake indefinitely. Explicit control obtains an execution-generation lease; queued daemon work remains durable. Closing the panel releases only the connection. Reconnecting validates project, backend, terminal and generation before sending any input. Sleeping projects display their state without waking merely to render a page.

## Schedules and proactive work

Support once, interval and cron schedules with an IANA timezone. Persist the schedule, revision and next occurrence. Transactional firing records an occurrence and event before advancing the timer. After downtime, recurring work coalesces missed occurrences into one check, and a still-pending check prevents overlapping repeats. Users and Agents use the same schedule operations; users can inspect, pause, edit and cancel them.

Readable project memory stores instructions and useful progress. SQLite owns transactional execution state. Future memory curation, goals and follow-up checks use ordinary tasks and timers, without adding a second scheduler or model proxy.

## Attention and deferred decisions

Native login, permission and startup problems create daemon-owned attention records before MCP is ready. Present a short reason, affected work and the appropriate action in the desktop; notify the responsible Feishu recipient when known. Repeated observations update the same item. Reading a notice does not resolve its underlying problem.

A business decision persists its question and pauses the task. Answering it inserts a new event that wakes the Agent. This differs from a native CLI prompt, which retains its live interactive process. Existing authorized automatic workspace-trust handling remains available.

## Compatibility and delivery

Keep the bundle identifier, data directory, update signing key, credentials and bot/project binding. Archive old Core execution references during migration and never use them to control a legacy terminal. Preserve message history and uncertain operations. Source, documentation, default UI and commits use English; desktop preference supports persistent English and Simplified Chinese.

Implementation issues: #23 legacy execution and adapters, #24 durable tasks/scheduling/supervision, #25 native embedded workspace. Parent #22 closes only after all acceptance checks pass. Version 0.3.4 publishes the pre-refactor main; this implementation does not publish another version automatically.
