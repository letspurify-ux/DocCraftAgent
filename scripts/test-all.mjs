import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import net from "node:net";
import path from "node:path";
import { spawn } from "node:child_process";
import { once } from "node:events";
const managerData = await fs.mkdtemp(
  path.join(os.tmpdir(), "doccraft-manager-test-"),
);
process.env.DOCCRAFT_PROCESS_STATE_DIR = managerData;
const { root, readState, owned, windows, requestStop } =
  await import("./all-processes.mjs");
assert(
  !owned(readState()),
  "Stop the managed servers before running launcher tests",
);
async function freePort() {
  const s = net.createServer();
  s.listen(0, "127.0.0.1");
  await once(s, "listening");
  const p = s.address().port;
  await new Promise((r) => s.close(r));
  return p;
}
const data = await fs.mkdtemp(path.join(os.tmpdir(), "doccraft-launcher-"));
const backend = await freePort(),
  frontend = await freePort();
const env = {
  ...process.env,
  DOCCRAFT_PORT: String(backend),
  DOCCRAFT_FRONTEND_PORT: String(frontend),
  DOCCRAFT_DATA_DIR: data,
};
function start() {
  const child = spawn(
    process.execPath,
    [path.join(root, "scripts", "start-all.mjs")],
    {
      cwd: os.tmpdir(),
      env,
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  let output = "";
  child.stdout.on("data", (b) => (output += b));
  child.stderr.on("data", (b) => (output += b));
  child.finished = once(child, "exit");
  child.output = () => output;
  return child;
}
async function ready(child) {
  const deadline = Date.now() + 60000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(child.output());
    if (child.output().includes("Frontend:")) return;
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error("Startup timeout: " + child.output());
}
async function closed(port) {
  await assert.rejects(
    fetch(`http://127.0.0.1:${port}/health`, {
      signal: AbortSignal.timeout(1000),
    }),
  );
}
let child;
try {
  child = start();
  await ready(child);
  assert.equal((await fetch(`http://127.0.0.1:${frontend}/`)).status, 200);
  assert.equal(
    (await fetch(`http://127.0.0.1:${frontend}/health`)).status,
    200,
  );
  // A cross-port browser POST must pass Origin validation through the Vite proxy.
  const session = await fetch(`http://127.0.0.1:${frontend}/api/v1/session`);
  const cookie = session.headers.get("set-cookie").split(";")[0];
  const post = await fetch(`http://127.0.0.1:${frontend}/api/v1/tasks`, {
    method: "POST",
    headers: {
      Origin: `http://127.0.0.1:${frontend}`,
      Cookie: cookie,
      "Content-Type": "application/json",
    },
    body: "{}",
  });
  assert.equal(
    post.status,
    400,
    "Configured frontend Origin must reach the task handler (invalid fixture expects 400)",
  );
  const duplicate = start();
  assert.equal((await duplicate.finished)[0], 1);
  assert(duplicate.output().includes("이미 start_all"));
  const stop = spawn(
    process.execPath,
    [path.join(root, "scripts", "stop-all.mjs")],
    {
      cwd: os.tmpdir(),
      stdio: "inherit",
    },
  );
  assert.equal((await once(stop, "exit"))[0], 0);
  assert.equal((await child.finished)[0], 0);
  await closed(backend);
  await closed(frontend);
  assert.equal(readState(), null);
  console.log(
    "PASS start, custom proxy, duplicate protection, stop_all, state cleanup and ports released",
  );
  child = start();
  await ready(child);
  if (windows) requestStop(readState());
  else child.kill("SIGINT");
  assert.equal((await child.finished)[0], 0);
  await closed(backend);
  await closed(frontend);
  assert.equal(readState(), null);
  console.log(
    windows
      ? "PASS repeated start/stop stops both servers"
      : "PASS Ctrl+C stops both servers",
  );
} finally {
  if (child?.exitCode === null) {
    if (windows && owned(readState())) requestStop(readState());
    else child.kill("SIGTERM");
    await child.finished;
  }
  await fs.rm(data, { recursive: true, force: true });
  await fs.rm(managerData, { recursive: true, force: true });
}
