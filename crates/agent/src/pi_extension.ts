// Written by Tcode for one pi session: attaches Tcode's MCP servers to it and
// runs pi's own `bash` through Tcode's task tools, so a long command moves to
// the background and wakes the thread when it exits. Tcode fills in
// `servers` with the session's servers.
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createBashToolDefinition } from "@earendil-works/pi-coding-agent";

const servers = __TCODE_SERVERS__;
const TASK_RUN = "mcp__tcode_tasks__task_run";
const GUIDELINE =
  "bash runs through Tcode: a command still running after 30 s moves to the background, the result says so with its task id and the output so far, and Tcode wakes this thread with a [control] message when it exits. Then end your turn or do other work; do not poll, sleep or run it again.";

function text(result) {
  return (result?.content ?? [])
    .filter((part) => part.type === "text")
    .map((part) => part.text)
    .join("");
}

// pi's bash operations, with each command run by Tcode's task host: its
// output streams in as `delta` progress, and `state` hears of a command that
// moved to the background.
function tcodeOperations(ctx, callId, state) {
  return {
    exec: async (command, cwd, { onData, signal, timeout, env }) => {
      const args = { command, cwd, call_id: callId, output_progress: "delta" };
      if (timeout !== undefined) args.timeout_s = timeout;
      // The environment goes by a file only this user reads: pi keeps the
      // arguments of nested calls in the session, and it holds secrets.
      const dir = env ? mkdtempSync(join(tmpdir(), "tcode-pi-env-")) : undefined;
      let outcome;
      try {
        if (dir) {
          args.env_file = join(dir, "env.json");
          const strings = Object.entries(env).filter(([, value]) => typeof value === "string");
          writeFileSync(args.env_file, JSON.stringify(Object.fromEntries(strings)), { mode: 0o600 });
        }
        outcome = await ctx.executeTool(TASK_RUN, args, {
          signal,
          onUpdate: (update) => {
            const delta = text(update);
            if (delta) onData(Buffer.from(delta));
          },
        });
      } finally {
        if (dir) rmSync(dir, { recursive: true, force: true });
      }
      if (signal?.aborted) throw new Error("aborted");
      const answer = text(outcome.result);
      if (outcome.isError) throw new Error(answer || "Tcode could not run the command");
      const run = JSON.parse(answer);
      if (run.output_rest) onData(Buffer.from(run.output_rest));
      if (run.status === "backgrounded") {
        state.backgrounded = run;
        return { exitCode: 0 };
      }
      if (run.timed_out) throw new Error(`timeout:${timeout}`);
      if (typeof run.exit_code === "number") return { exitCode: run.exit_code };
      return { exitCode: typeof run.signal === "number" ? 128 + run.signal : 1 };
    },
  };
}

// The result of a command that goes on in the background, in bash's shape:
// what it printed so far, then where it went.
function backgrounded(result, run) {
  const by = { user: "the user", agent: "task_background", yield: "Tcode after 30 s" }[run.moved_by] ?? "Tcode";
  const note = `[Still running: moved to the background by ${by} as Tcode task ${run.task_id}. Tcode wakes this thread with a [control] message when it exits; task_read, task_wait and task_stop act on ${run.task_id}.]`;
  const output = text(result);
  const { structuredContent: _exit, isError: _error, ...rest } = result;
  return { ...rest, content: [{ type: "text", text: output ? `${output}\n${note}` : note }] };
}

export default function (pi) {
  for (const [name, config] of Object.entries(servers)) pi.registerMcpServer(name, config);
  if (!("tcode_tasks" in servers)) return;
  let replaced = false;
  pi.on("session_start", () => {
    // Only pi's own bash, and only where it is in use: another extension's
    // bash (ssh, a sandbox, the user's own) is left as it is.
    const bash = pi.getAllTools().find((tool) => tool.name === "bash");
    if (replaced || bash?.sourceInfo?.source !== "builtin" || !pi.getActiveTools().includes("bash")) {
      return;
    }
    replaced = true;
    const local = createBashToolDefinition(process.cwd());
    pi.registerTool({
      ...local,
      promptGuidelines: [...(local.promptGuidelines ?? []), GUIDELINE],
      // The model runs commands with bash; task_run is bash's to call.
      prepareLoadout: () => ({ hiddenDeclarations: [TASK_RUN] }),
      async execute(toolCallId, params, signal, onUpdate, ctx) {
        // Without Tcode's task host (not connected), the command runs as pi
        // runs it.
        if (!ctx.tools?.some((tool) => tool.name === TASK_RUN)) {
          return local.execute(toolCallId, params, signal, onUpdate, ctx);
        }
        const state = {};
        const tool = createBashToolDefinition(ctx.cwd, {
          operations: tcodeOperations(ctx, toolCallId, state),
        });
        const result = await tool.execute(toolCallId, params, signal, onUpdate, ctx);
        return state.backgrounded ? backgrounded(result, state.backgrounded) : result;
      },
    });
  });
}
