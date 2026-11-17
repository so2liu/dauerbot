/** Conservative census of the verified Pi 1.5.1 extension discovery inputs. */
import * as fs from "node:fs";
import * as path from "node:path";
import { createHash } from "node:crypto";

// Exact HERDR 0.9.5 embedded Pi v9 observer, frozen commit 2ecf19a.
// Audited as socket-only lifecycle reporting, with no child dispatch.
const HERDR_PI_V9 = "2c5272d732b475bbf91a027203b1f98d25fe43d2c1402530a442b288aeaca1e4";
function knownObserver(file: string): boolean {
	if (path.basename(file) !== "herdr-agent-state.ts") return false;
	const meta = fs.lstatSync(file);
	return meta.isFile() && meta.size < 64_000 && createHash("sha256").update(fs.readFileSync(file)).digest("hex") === HERDR_PI_V9;
}
export function externalExtensions(agentDir: string, cwd: string, settings: any, managedMcp?: any): boolean {
	// Do not load/execute these plugins a second time or inspect authentication.
	// Even an extension without tools can launch children from a lifecycle hook.
	if (!settings || (settings.packages?.length ?? 0) || (settings.extensions?.length ?? 0)) return true;
	const dirs = [path.join(agentDir, "extensions")];
	const configs = [path.join(agentDir, "settings.json")];
	const mcps = [path.join(agentDir, "mcp.json")];
	let current = cwd;
	while (true) {
		dirs.push(path.join(current, ".pi", "extensions"));
		configs.push(path.join(current, ".pi", "settings.json"));
		mcps.push(path.join(current, ".pi", "mcp.json"));
		const parent = path.dirname(current);
		if (parent === current) break;
		current = parent;
	}
	try {
		// Inspect the individual discovery inputs as merged project arrays can hide
		// globally enabled packages. Malformed/JSONC inputs conservatively veto.
		for (const file of configs) {
			let raw;
			try { raw = fs.readFileSync(file, "utf8"); }
			catch (error: any) { if (error.code === "ENOENT") continue; throw error; }
			const config = JSON.parse(raw);
			if ((config.extensions?.length ?? 0) || (config.packages?.length ?? 0)) return true;
		}
		for (const file of mcps) {
			let raw;
			try { raw = fs.readFileSync(file, "utf8"); }
			catch (error: any) { if (error.code === "ENOENT") continue; throw error; }
			const config = JSON.parse(raw);
			for (const [name, server] of Object.entries(config.mcpServers ?? {})) {
				const entry = server as any;
				if (name !== "herdr_feishu" || !managedMcp || !entry || typeof entry !== "object" || Object.keys(entry).some((key) => !["command", "args", "exposure"].includes(key)) || entry.command !== managedMcp.command || JSON.stringify(entry.args) !== JSON.stringify(managedMcp.args)) return true;
			}
		}
		return dirs.some((dir) => {
			try { return fs.readdirSync(dir).some((name) => !knownObserver(path.join(dir, name))); }
			catch (error: any) { if (error.code === "ENOENT") return false; throw error; }
		});
	} catch { return true; }
}
