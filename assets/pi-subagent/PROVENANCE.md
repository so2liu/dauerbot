# Managed Pi subagent

`index.ts` and `agents.ts` originate in the MIT-licensed official Pi example at
badlogic/pi-mono commit `1f6f319bb28992a40b99c147fffb23b65a23129d`
(`packages/coding-agent/examples/extensions/subagent`). The upstream index is byte identical to the example shipped with the locally
verified Pi 1.5.1 package. The vendored index only renames the tool and its result
hook to `dauerbot_subagent` to preserve existing user tools.
`LICENSE` is the upstream license. `agents.ts` adds a local fallback worker;
user and project agent definitions override it. The worker does not pin a model.
`managed.ts` adds automatic, execution-scoped evidence and a dispatch gate.

No detached children: upstream awaits single, parallel, and chained subprocesses.
The wrapper counts the entire invocation, including preparation and queue gaps.
Dauerbot explicitly loads this extension only in its managed parent Pi sessions.
The normal Pi discovery of global plugins/settings/auth remains enabled.

`coverage.ts` allows only the exact byte hash of HERDR 0.9.5 embedded Pi v9
observer from `herdrdev/herdr` commit `2ecf19a92fc3a7fff75b6ce3655ef04f4625ec01`,
`src/integration/assets/pi/herdr-agent-state.ts`; it reports lifecycle via a
Unix socket and never dispatches children. No HERDR source is copied here.
