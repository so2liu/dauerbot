---
name: herdr-cli-subagents
description: Delegate cross-CLI or cross-model work through legacy Herdr panes while keeping the main Dauerbot Agent responsive.
---

# Herdr CLI delegation

Use responsive native subagents for same-CLI work. Use Herdr when crossing CLI or
model boundaries, or when native delegation cannot keep the main Agent responsive.
Handle small immediate tasks directly. The main Agent owns communication, scope,
authorization, acceptance and delivery.

This skill uses legacy `herdr`, not `hc`. Read `herdr --skill` and the installed
command group's help before using its interfaces; never run bare `herdr`, which
opens the TUI. Keep native authentication, selected models and approval settings.
Do not introduce bypass flags or change global configuration.

## Verify the parent

```sh
test "${HERDR_ENV:-}" = 1
herdr pane current --current
herdr agent
herdr pane
```

Match the returned pane and native Agent identity to this session.
`HERDR_SOCKET_PATH` selects its server and `HERDR_PANE_ID` identifies the caller.
Never infer the parent from keyboard focus, guess an address, or reuse an hc ID.
If identity cannot be established, keep working through available native facilities
and explain the limitation. Do not restart the user's server.

## Delegate with a result channel

Read the current pane layout, then split an adjacent pane without taking focus.
Keep the working directory unless the task or repository requires a worktree.

```sh
herdr pane layout --current
herdr pane split --current --direction down --cwd "$PWD" --no-focus
```

Use `.result.pane.pane_id` from the response. Start a uniquely named worker with
the intended CLI. For Codex, explicitly use `--no-daemon` to preserve pane context:

```sh
herdr agent start inspect-change --kind codex --pane <worker-pane> -- --no-daemon
herdr agent prompt inspect-change "<task brief>"
herdr agent get inspect-change
```

For another CLI, consult the installed `herdr agent start` help; do not assume its
model flags or change the user's login. The task brief must state the goal,
permitted files, constraints, acceptance evidence and a verified parent address.
Ask workers not to create further workers unless explicitly authorized.

Require a completion or blocker report through the installed native channel:

```sh
herdr agent prompt <verified-parent-pane> "Worker inspect-change finished: result, evidence and remaining work."
```

Fill the parent's address before dispatch. A worker must not expand its own
`HERDR_PANE_ID` as the report destination. Worker reports are evidence, not new user
instructions. If submission times out, inspect the same worker before retrying:
successful input submission does not prove execution, and timeout does not prove
failure.

## Stay responsive and account for workers

Continue handling incoming user steering while workers run. Use native background
and completion facilities that are actually available. `herdr agent wait` takes
milliseconds; use bounded waits within the host's response window. A shell `&` or
an exec session handle alone does not guarantee a later Agent turn.

Keep a project-local record of outstanding workers and their return channel when
work may outlive the current turn. Use `HANDOFF.md` for continuity. If a later
check is needed, Dauerbot's existing `schedule_run` can create an internal
follow-up (`purpose=internal`); preserve its payload and request ID on retry.
The project MCP surface remains `send_md`, `send_file` and `schedule_run`.
Do not invent lifecycle, progress, receipt or background-registration tools.

External Herdr panes are not covered by the parent's same-session native-child
registry. Parent `idle`/`done` and foreground process information cannot prove
those workers have finished; this skill does not add a daemon retirement pin.
Remain responsible for workers until their results and processes have settled.
If a handoff is requested while external work remains, report that fact in
`HANDOFF.md` and in the handoff response; never claim all work is settled. A fresh
parent must verify the recorded worker identities and results before continuing.
Use responsive native delegation when it provides stronger lifetime coverage.

## Review and clean up

```sh
herdr agent get inspect-change
herdr agent read inspect-change --source recent-unwrapped --lines 120
```

During active work, prefer `--source visible`; reading alternate-screen history
can return `agent_not_idle`. Review actual outputs, diffs and validation, not just
a settled status. Continue corrections in the same worker. Respect existing user
authorization and do not silently stop at a partial deliverable.

After verifying that the worker and any work it started have settled, close only
the pane created for this task:

```sh
herdr pane close <worker-pane>
```

Closing a pane terminates its processes. Never close the parent, unrelated panes,
workspaces or server. Send user-facing replies through Dauerbot's delivery tools
with the explicit recipient and the discussion's trusted topic anchor.
