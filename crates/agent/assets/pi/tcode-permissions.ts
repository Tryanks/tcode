/**
 * tcode's permission bridge for pi RPC mode.
 *
 * pi deliberately has no built-in permission gate. This extension keeps tool
 * execution fail-closed and translates decisions into RPC extension-UI
 * confirmations that the Rust adapter surfaces as native tcode approvals.
 */
import pathModule from "node:path";
import fsModule from "node:fs";

export default function tcodePermissions(pi: any) {
	const mode = process.env.TCODE_PI_APPROVAL_MODE ?? "supervised";
	const cwd = canonical(process.env.TCODE_PI_CWD ?? process.cwd());
	const safe = new Set(["read", "grep", "find", "ls"]);
	const writes = new Set(["edit", "write"]);
	const known = new Set([...safe, ...writes, "bash"]);

	pi.on("tool_call", async (event: any, ctx: any) => {
		const toolName = String(event.toolName ?? "");
		const input = event.input ?? {};

		// Unknown extension tools never inherit blanket access. This is the
		// fail-closed boundary if pi adds a new built-in tool in the future.
		if (!known.has(toolName)) {
			return { block: true, reason: `tcode blocked unknown tool: ${toolName || "(unnamed)"}` };
		}
		if (safe.has(toolName)) return undefined;

		if (mode === "read_only") {
			return { block: true, reason: `tcode read-only mode blocked ${toolName}` };
		}
		if (mode === "full_access") return undefined;

		let reason = "requires confirmation";
		if (writes.has(toolName) && mode === "auto_accept_edits") {
			const target = typeof input.path === "string" ? canonical(input.path) : "";
			if (target && isInside(target, cwd)) return undefined;
			reason = "writes outside the project require confirmation";
		} else if (toolName === "bash") {
			reason = "shell commands require confirmation";
		}

		const payload = JSON.stringify({ toolName, input, cwd, reason });
		const confirmed = await ctx.ui.confirm(`tcode:${toolName}`, payload);
		if (!confirmed) return { block: true, reason: `tcode denied ${toolName}` };
		return undefined;
	});
}

function normalize(path: string): string {
	const resolved = pathModule.resolve(path);
	return process.platform === "win32" ? resolved.toLowerCase() : resolved;
}

// Resolve symlinks as far as the filesystem currently exists. For a new file,
// canonicalizing its parent closes the common "workspace symlink points outside"
// escape while still allowing the not-yet-created leaf.
function canonical(path: string): string {
	const resolved = pathModule.resolve(path);
	try {
		return normalize(fsModule.realpathSync(resolved));
	} catch {
		try {
			return normalize(pathModule.join(fsModule.realpathSync(pathModule.dirname(resolved)), pathModule.basename(resolved)));
		} catch {
			return normalize(resolved);
		}
	}
}

function isInside(path: string, cwd: string): boolean {
	const separator = pathModule.sep;
	return path === cwd || path.startsWith(cwd + separator);
}
