// Installed by geleider: Harness Session hooks v2
// OpenCode 2 Promise plugin API. All persistence and state mapping live in Rust.
import { execFile } from "node:child_process";

const executable = __GELEIDER_EXE__;

type Info = { id: string; parentID?: string; location: { directory: string }; subpath?: string };
type ToolEvent = { id: string; sessionID: string; tool: string; input: unknown };
type Context = {
  session: {
    get(input: { sessionID: string }, options?: { signal: AbortSignal }): Promise<Info>;
    prompt(input: { sessionID: string; text: string; delivery: "queue" }): Promise<unknown>;
  };
  tool: { hook(name: string, callback: (event: ToolEvent) => Promise<void>): Promise<unknown> };
  event: { subscribe(options: { signal: AbortSignal }): AsyncIterable<{ type: string; data: Record<string, unknown> }> };
};

// The installed file contains an absolute executable path; no shell interpolation.
function call(args: string[], input?: unknown): Promise<string | undefined> {
  return new Promise((resolve) => {
    const child = execFile(executable, args, { timeout: 500, maxBuffer: 256 * 1024 },
      (error, stdout) => resolve(error ? undefined : stdout));
    child.on("error", () => resolve(undefined));
    child.stdin?.on("error", () => {});
    child.stdin?.end(input === undefined ? "" : JSON.stringify(input));
  });
}

export default {
  id: "geleider",
  async setup(ctx: Context) {
    const controller = new AbortController();
    const known = new Map<string, Info>();
    const roots = new Map<string, Info>();
    let stopped = false;
    let polling = false;

    async function root(id: string): Promise<Info | undefined> {
      const visited = new Set<string>();
      const signal = AbortSignal.timeout(300);
      while (!visited.has(id) && visited.size < 32) {
        visited.add(id);
        // Refresh locations: Sessions can move between worktrees.
        const info = await ctx.session.get({ sessionID: id }, { signal })
          .catch(() => known.get(id));
        if (!info) return;
        known.set(id, info);
        if (!info.parentID) {
          roots.set(id, info);
          return info;
        }
        id = info.parentID;
      }
    }

    function report(info: Info, event: string, fields: Record<string, unknown> = {}) {
      return call(["hook", "opencode", event], {
        ...fields, session_id: info.id, cwd: info.location.directory, owner_pid: process.pid,
      });
    }

    await ctx.tool.hook("execute.before", async (event) => {
      // submit_run, claim and release name the calling Session.
      const attested = /^(geleider_|mcp__geleider__)(submit_run|claim|release)$/.test(event.tool);
      // A Session the model wrote itself is never passed on, even when the
      // Session cannot be resolved: it could name another Session.
      const args = attested ? { ...((event.input ?? {}) as Record<string, unknown>) } : undefined;
      if (args) {
        delete args.session;
        event.input = args;
      }
      try {
        const info = await root(event.sessionID);
        if (!info || stopped) return;
        // A child belongs to its parent Session, but can't overwrite its state.
        const child = info.id !== event.sessionID;
        const fields = { tool_name: event.tool, tool_use_id: event.id, ...(child ? { agent_id: event.sessionID } : {}) };
        if (!args) {
          if (!child) await report(info, "tool.before", fields);
          return;
        }
        args.cwd ??= info.location.directory;
        // geleider attests the root Session to its daemon and returns the
        // arguments carrying that attestation's nonce.
        const output = await report(info, "tool.before", { ...fields, tool_input: args });
        const updated = output && JSON.parse(output)?.hookSpecificOutput?.updatedInput;
        if (updated && typeof updated === "object" && !Array.isArray(updated)) event.input = updated;
      } catch { /* Hooks must never fail a Harness operation. */ }
    });
    await ctx.tool.hook("execute.after", async (event) => {
      try {
        const info = await root(event.sessionID);
        if (info && !stopped && info.id === event.sessionID) await report(info, "tool.after", { tool_use_id: event.id });
      } catch { /* Best effort. */ }
    });

    const supported = new Set([
      "session.created", "session.deleted", "session.status", "session.idle", "session.error",
      "permission.asked", "permission.replied", "form.created", "form.replied", "form.cancelled",
      "session.step.started", "message.updated",
    ]);
    void (async () => {
      try {
        for await (const event of ctx.event.subscribe({ signal: controller.signal })) {
          if (stopped) break;
          if (!supported.has(event.type)) continue;
          const fields = event.data;
          const form = fields.form as { sessionID?: string } | undefined;
          const message = fields.info as { sessionID?: string; role?: string; providerID?: string } | undefined;
          const id = (fields.sessionID ?? form?.sessionID ?? message?.sessionID) as string | undefined;
          if (!id) continue;
          const info = await root(id);
          if (!info) continue;
          if (info.id !== id && event.type !== "permission.asked" && event.type !== "form.created") continue;
          if (event.type === "session.step.started" || event.type === "message.updated") {
            if (event.type === "message.updated" && message?.role !== "assistant") continue;
            const model = fields.model as { providerID?: string } | undefined;
            const provider = model?.providerID ?? message?.providerID;
            // Send only provider identity, never message text or model output.
            if (typeof provider === "string") await report(info, event.type, { provider_id: provider });
          } else await report(info, event.type, fields);
          if (event.type === "session.deleted") {
            roots.delete(id);
            known.delete(id);
          }
        }
      } catch { /* Subscription disposal or unavailable server. */ }
    })();

    async function poll() {
      if (stopped || polling) return;
      polling = true;
      try {
        for (const id of roots.keys()) {
          if (stopped) break;
          const output = await call(["session-results", "opencode", id]);
          if (!output) continue;
          const results = JSON.parse(output) as { run_id: number; message: string }[];
          for (const result of results) {
            if (stopped) break;
            await ctx.session.prompt({ sessionID: id, text: result.message, delivery: "queue" });
            await call(["session-results", "opencode", id, "--ack", String(result.run_id)]);
          }
        }
      } catch { /* Leave unacknowledged results in geleider for retry. */ }
      finally { polling = false; }
    }
    const timer = setInterval(() => void poll(), 2000);
    timer.unref?.();
    return async () => {
      stopped = true;
      clearInterval(timer);
      controller.abort();
      await Promise.all([...roots.values()].map((info) => report(info, "SessionEnd")));
    };
  },
};
