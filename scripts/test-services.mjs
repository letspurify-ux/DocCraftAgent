import assert from "node:assert/strict";
import fs from "node:fs/promises";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import { once } from "node:events";

const managerData = await fs.mkdtemp(
  path.join(os.tmpdir(), "doccraft-service-manager-test-"),
);
process.env.DOCCRAFT_PROCESS_STATE_DIR = managerData;
const { root, windows } = await import("./all-processes.mjs");
const { owned, readServiceState, requestServiceStop } =
  await import("./individual-processes.mjs");

async function freePort() {
  const server = net.createServer();
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const value = server.address().port;
  await new Promise((resolve) => server.close(resolve));
  return value;
}

const data = await fs.mkdtemp(path.join(os.tmpdir(), "doccraft-service-test-"));
const backendPort = await freePort();
let frontendPort = await freePort();
while (frontendPort === backendPort) frontendPort = await freePort();
const env = {
  ...process.env,
  DOCCRAFT_PORT: String(backendPort),
  DOCCRAFT_FRONTEND_PORT: String(frontendPort),
  DOCCRAFT_DATA_DIR: data,
};

function start(service) {
  const child = spawn(
    process.execPath,
    [path.join(root, "scripts", "start-service.mjs"), service],
    {
      cwd: os.tmpdir(),
      env,
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  let output = "";
  child.stdout.on("data", (chunk) => (output += chunk));
  child.stderr.on("data", (chunk) => (output += chunk));
  child.finished = once(child, "exit");
  child.output = () => output;
  return child;
}

async function ready(child, marker) {
  const deadline = Date.now() + 60000;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) throw new Error(child.output());
    if (child.output().includes(marker)) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`Startup timeout: ${child.output()}`);
}

async function stop(service) {
  const child = spawn(
    process.execPath,
    [path.join(root, "scripts", "stop-service.mjs"), service],
    { cwd: os.tmpdir(), env, stdio: "inherit" },
  );
  assert.equal((await once(child, "exit"))[0], 0);
}

async function assertClosed(port, pathname) {
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    try {
      await fetch(`http://127.0.0.1:${port}${pathname}`, {
        signal: AbortSignal.timeout(500),
      });
    } catch {
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  assert.fail(`${port}번 포트가 해제되지 않았습니다.`);
}

async function cleanup(service, child) {
  if (child?.exitCode !== null) return;
  const state = readServiceState(service);
  if (owned(state)) requestServiceStop(service, state);
  else child.kill("SIGTERM");
  await child.finished;
}

let backend;
let frontend;
try {
  backend = start("backend");
  await ready(backend, "Backend:");
  assert.equal(
    (await fetch(`http://127.0.0.1:${backendPort}/health`)).status,
    200,
  );

  frontend = start("frontend");
  await ready(frontend, "Frontend:");
  assert.equal((await fetch(`http://127.0.0.1:${frontendPort}/`)).status, 200);
  assert.equal(
    (await fetch(`http://127.0.0.1:${frontendPort}/health`)).status,
    200,
  );

  const duplicate = start("frontend");
  assert.equal((await duplicate.finished)[0], 1);
  assert(duplicate.output().includes("이미 start_frontend"));

  await stop("frontend");
  assert.equal((await frontend.finished)[0], 0);
  await assertClosed(frontendPort, "/");
  assert.equal(
    (await fetch(`http://127.0.0.1:${backendPort}/health`)).status,
    200,
    "stop_frontend must leave the backend running",
  );

  frontend = start("frontend");
  await ready(frontend, "Frontend:");
  await stop("backend");
  assert.equal((await backend.finished)[0], 0);
  await assertClosed(backendPort, "/health");
  assert.equal(
    (await fetch(`http://127.0.0.1:${frontendPort}/`)).status,
    200,
    "stop_backend must leave the frontend running",
  );

  await stop("frontend");
  assert.equal((await frontend.finished)[0], 0);
  await assertClosed(frontendPort, "/");
  assert.equal(readServiceState("backend"), null);
  assert.equal(readServiceState("frontend"), null);
  console.log(
    "PASS individual start, proxy, duplicate protection, independent stop, state cleanup and ports released",
  );
} finally {
  await cleanup("frontend", frontend);
  await cleanup("backend", backend);
  await fs.rm(data, { recursive: true, force: true });
  await fs.rm(managerData, { recursive: true, force: true });
}
