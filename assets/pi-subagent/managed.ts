/** Dauerbot's managed-only wrapper. No global Pi settings are changed. */
import * as fs from "node:fs";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { getAgentDir, VERSION, type ExtensionAPI } from "@earendil-works/pi-coding-agent";
import official from "./index.ts";
import { externalExtensions } from "./coverage.ts";

export default function managed(pi: ExtensionAPI) {
	const dir = path.dirname(fileURLToPath(import.meta.url));
	const config = JSON.parse(fs.readFileSync(path.join(dir, "managed.json"), "utf8"));
	const gate = path.join(dir, "dispatch.lock");
	const retired = path.join(dir, "retiring");
	const state = path.join(dir, "children.json");
	const unknown = path.join(dir, "coverage.unknown");
	let session = "", sessionFile = "", revision = 0, active = 0, cwd = "";
	// An old ledger without clean shutdown can have live orphaned children.
	// Unknown coverage survives /reload for this whole execution generation.
	let incomplete = config.incomplete || VERSION !== "1.5.1" || fs.existsSync(unknown) || fs.existsSync(state);
	const epoch = `${process.pid}-${Date.now()}`;
	let heartbeat: ReturnType<typeof setInterval> | undefined;
	function publish() {
		if (!session) return;
		// Removing configuration does not unload a running plugin or its children.
		// Once coverage is incomplete it stays so for this execution generation.
		incomplete ||= fs.existsSync(unknown) || externalExtensions(getAgentDir(), cwd, pi.getSettings(), config.mcp);
		if (incomplete && !fs.existsSync(unknown)) fs.writeFileSync(unknown, "Incomplete native child coverage\n", { mode: 0o600 });
		const tmp = `${state}.${process.pid}.tmp`;
		fs.writeFileSync(tmp, JSON.stringify({ schema: 1, generation: config.generation,
			session, sessionFile, epoch, revision, state: active ? "active" : (incomplete ? "unknown" : "quiescent"), updated: Date.now() }), { mode: 0o600 });
		fs.renameSync(tmp, state);
	}
	function enter() {
		// A held gate or persistent retirement marker fails closed. Never dispatch
		// between the daemon's last observation and its native stop request.
		fs.mkdirSync(gate, { mode: 0o700 });
		try { if (fs.existsSync(retired)) throw new Error("This managed session is retiring"); }
		catch (error) { fs.rmdirSync(gate); throw error; }
	}
	pi.on("session_start", (_event, ctx) => {
		if (ctx.mode !== "tui") return;
		cwd = ctx.cwd;
		session = ctx.sessionManager.getSessionId();
		sessionFile = ctx.sessionManager.getSessionFile() ?? "";
		publish();
		if (heartbeat) clearInterval(heartbeat);
		heartbeat = setInterval(() => { try { publish(); } catch { /* Missing evidence retains the session. */ } }, 1500);
		heartbeat.unref();
	});
	pi.on("session_shutdown", () => {
		if (heartbeat) clearInterval(heartbeat);
		if (active) {
			try { fs.writeFileSync(unknown, "Adapter stopped with active child invocations\n", { mode: 0o600 }); } catch { return; }
		}
		try { fs.unlinkSync(state); } catch { /* Already absent. */ }
	});
	const wrapped = new Proxy(pi, {
		get(target, prop) {
			if (prop !== "registerTool") return Reflect.get(target, prop);
			return (tool: any) => target.registerTool({ ...tool, async execute(...args: any[]) {
				if (!session) throw new Error("Managed subagents require a native parent session");
				enter();
				try { active++; revision++; publish(); } finally { fs.rmdirSync(gate); }
				try { return await tool.execute(...args); }
				finally { active--; revision++; publish(); }
			} });
		},
	});
	official(wrapped);
}
