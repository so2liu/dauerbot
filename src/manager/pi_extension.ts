import { readFileSync } from "node:fs";
import { createConnection } from "node:net";
import { join } from "node:path";

export default function (pi: any) {
    const tools = JSON.parse(readFileSync(join(process.env.DAUERBOT_PI_BRIDGE!, "tools.json"), "utf8"));
    const names = new Set(tools.map((tool: any) => tool.name));
    pi.on("tool_call", (event: any) => {
        if (!names.has(event.toolName)) return { block: true, reason: "Tool is outside the Manager allowlist" };
    });
    pi.on("before_provider_request", (event: any) => {
        const { max_tokens, max_completion_tokens, ...payload } = event.payload;
        return payload;
    });
    for (const { name, description, parameters } of tools) {
        pi.registerTool({
            name,
            label: name,
            description,
            parameters,
            executionMode: "sequential",
            async execute(occurrence: string, args: unknown, signal: AbortSignal) {
                const value = await new Promise<unknown>((resolve, reject) => {
                    const socket = createConnection(join(process.env.DAUERBOT_PI_BRIDGE!, "tools.sock"));
                    socket.setEncoding("utf8");
                    const abort = () => socket.destroy(new Error("Manager tool cancelled"));
                    signal?.addEventListener("abort", abort, { once: true });
                    if (signal?.aborted) abort();
                    socket.on("connect", () => socket.write(JSON.stringify({ occurrence, name, arguments: args }) + "\n"));
                    let data = "";
                    socket.on("data", (chunk) => {
                        data += chunk;
                        if (data.length > 1_048_576) socket.destroy(new Error("Manager tool reply is oversized"));
                    });
                    socket.on("error", reject);
                    socket.on("close", () => signal?.removeEventListener("abort", abort));
                    socket.on("end", () => {
                        try { resolve(JSON.parse(data)); } catch { reject(new Error("Invalid Manager tool reply")); }
                    });
                });
                return { content: [{ type: "text", text: JSON.stringify(value) }], details: {} };
            },
        });
    }
}
