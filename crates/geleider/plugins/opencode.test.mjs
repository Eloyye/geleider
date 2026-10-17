import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, writeFile, chmod, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

async function until(check) {
  const deadline = Date.now() + 5000;
  while (!(await check())) {
    assert.ok(Date.now() < deadline, "timed out");
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
}

test("OpenCode 2 hooks annotate the root Session and queue only acknowledged results", async () => {
  const dir = await mkdtemp(join(tmpdir(), "geleider-plugin-"));
  let cleanup;
  try {
    const callsPath = join(dir, "calls");
    const resultsPath = join(dir, "results");
    const exe = join(dir, "geleider");
    await writeFile(resultsPath, JSON.stringify([{ run_id: 12, message: "Run #12 passed" }]));
    await writeFile(exe, `#!${process.execPath}
const fs = require('node:fs');
const args = process.argv.slice(2);
const input = fs.readFileSync(0, 'utf8');
fs.appendFileSync(${JSON.stringify(callsPath)}, JSON.stringify({ args, input: input ? JSON.parse(input) : null }) + '\\n');
if (args[0] === 'session-results') {
  if (args.includes('--ack')) fs.writeFileSync(${JSON.stringify(resultsPath)}, '[]');
  console.log(fs.readFileSync(${JSON.stringify(resultsPath)}, 'utf8'));
}
`);
    await chmod(exe, 0o700);
    const source = (await readFile(new URL("./opencode.ts", import.meta.url), "utf8"))
      .replace("__GELEIDER_EXE__", JSON.stringify(exe));
    const modulePath = join(dir, "plugin.ts");
    await writeFile(modulePath, source);
    const { default: plugin } = await import(pathToFileURL(modulePath));
    const hooks = new Map();
    const events = [];
    const prompts = [];
    let rejectDelivery = true;
    let deliveryAttempts = 0;
    const ctx = {
      session: {
        async get({ sessionID }) {
          return { id: sessionID, parentID: sessionID === "child" ? "root" : undefined,
            location: { directory: sessionID === "other" ? "/other-worktree" : "/root-worktree" } };
        },
        async prompt(input) {
          deliveryAttempts++;
          if (rejectDelivery) throw Error("temporarily unavailable");
          prompts.push(input);
        },
      },
      tool: { async hook(name, handler) { hooks.set(name, handler); } },
      event: { async *subscribe({ signal }) {
        while (!signal.aborted) {
          if (events.length) yield events.shift();
          else await new Promise((resolve) => setTimeout(resolve, 5));
        }
      } },
    };
    cleanup = await plugin.setup(ctx);
    const call = { id: "call", sessionID: "child", tool: "geleider_submit_run", input: { job: "test" } };
    await hooks.get("execute.before")(call);
    assert.deepEqual(call.input.session, { harness: "opencode", id: "root" });
    assert.equal(call.input.cwd, "/root-worktree");
    const other = { id: "call-2", sessionID: "other", tool: "geleider_submit_run", input: { job: "test" } };
    await hooks.get("execute.before")(other);
    assert.deepEqual(other.input.session, { harness: "opencode", id: "other" });
    assert.equal(other.input.cwd, "/other-worktree");
    events.push({ type: "permission.asked", data: { sessionID: "root", id: "permission" } });
    events.push({ type: "session.idle", data: { sessionID: "child" } });
    events.push({ type: "form.created", data: { form: { sessionID: "root", id: "question" } } });
    const calls = async () => (await readFile(callsPath, "utf8")).trim().split("\n").map(JSON.parse);
    await until(async () => (await calls()).some((c) => c.args[2] === "form.created"));
    assert.ok(!(await calls()).some((c) => c.args[2] === "session.idle"));
    await until(() => deliveryAttempts > 0);
    assert.equal(prompts.length, 0);
    assert.ok(!(await calls()).some((c) => c.args.includes("--ack")));
    rejectDelivery = false;
    await until(() => prompts.length > 0);
    assert.deepEqual(prompts[0], { sessionID: "root", text: "Run #12 passed", delivery: "queue" });
    await until(async () => (await calls()).some((c) => c.args.includes("--ack")));
    await cleanup();
    cleanup = undefined;
    const count = (await calls()).length;
    await new Promise((resolve) => setTimeout(resolve, 100));
    assert.equal((await calls()).length, count, "cleanup stops callbacks and polling");
  } finally {
    await cleanup?.();
    await rm(dir, { recursive: true, force: true });
  }
});
