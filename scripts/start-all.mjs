import fs from "node:fs";
import net from "node:net";
import http from "node:http";
import path from "node:path";
import { once } from "node:events";
import {
  root,
  windows,
  killTree,
  stopRequested,
  statePath,
  identity,
  readState,
  owned,
  removeState,
  run,
} from "./all-processes.mjs";

const children = new Set();
let stopping = false;
let state;
function shutdown(code = 0) {
  if (stopping) return;
  stopping = true;
  process.exitCode = code;
  for (const child of children) {
    try {
      killTree(child.pid);
    } catch {}
  }
  const timer = setTimeout(() => {
    for (const child of children) {
      try {
        killTree(child.pid, true);
      } catch {}
    }
  }, 8000);
  timer.unref();
}
process.on("SIGINT", () => shutdown());
process.on("SIGTERM", () => shutdown());
process.on("exit", () => {
  if (state) removeState(state);
});
function launch(command, args, cwd = root) {
  if (stopping) throw new Error("시작이 중단됐습니다.");
  const child = run(command, args, { cwd, detached: !windows });
  children.add(child);
  child.once("exit", () => children.delete(child));
  child.once("error", () => children.delete(child));
  return child;
}
async function build(command, args, cwd) {
  const child = launch(command, args, cwd);
  const [code] = await once(child, "exit");
  if (code !== 0 || stopping)
    throw new Error(`${command} 실행이 완료되지 않았습니다.`);
}
async function available(port) {
  const server = net.createServer();
  await new Promise((resolve, reject) => {
    server.once("error", (error) =>
      reject(
        new Error(
          `${port}번 포트를 사용할 수 없습니다 (${error.code}). 기존 서버를 종료하거나 포트 환경변수를 지정하세요.`,
        ),
      ),
    );
    server.listen(port, "127.0.0.1", resolve);
  });
  await new Promise((resolve) => server.close(resolve));
}
function port(value) {
  const n = Number(value);
  if (!Number.isInteger(n) || n < 1 || n > 65535)
    throw new Error("포트는 1~65535 정수여야 합니다.");
  return n;
}
async function ready(url) {
  const deadline = Date.now() + 30000;
  while (!stopping && Date.now() < deadline) {
    try {
      const ok = await new Promise((resolve, reject) => {
        const req = http.get(
          url,
          { signal: AbortSignal.timeout(1000) },
          (res) => {
            res.resume();
            resolve(res.statusCode >= 200 && res.statusCode < 300);
          },
        );
        req.on("error", reject);
      });
      if (ok) return;
    } catch {}
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
  throw new Error(`서버 준비 실패: ${url}`);
}
try {
  const backendPort = port(process.env.DOCCRAFT_PORT ?? 8765);
  const frontendPort = port(process.env.DOCCRAFT_FRONTEND_PORT ?? 6001);
  if (backendPort === frontendPort)
    throw new Error("백엔드와 프론트엔드 포트가 같을 수 없습니다.");
  fs.mkdirSync(path.dirname(statePath), { recursive: true, mode: 0o700 });
  const previous = readState();
  if (owned(previous)) throw new Error("이미 start_all이 실행 중입니다.");
  if (previous) removeState(previous);
  const candidate = { pid: process.pid, identity: identity(process.pid) };
  if (!candidate.identity)
    throw new Error("프로세스 식별 정보를 읽을 수 없어 시작을 중단합니다.");
  fs.writeFileSync(statePath, JSON.stringify(candidate), {
    flag: "wx",
    mode: 0o600,
  });
  state = candidate;
  const stopWatcher = setInterval(() => {
    if (stopRequested(state)) shutdown();
  }, 200);
  stopWatcher.unref();
  await available(backendPort);
  await available(frontendPort);
  const frontend = path.join(root, "frontend");
  if (
    !fs.existsSync(
      path.join(frontend, "node_modules", "vite", "bin", "vite.js"),
    )
  ) {
    await build("npm", ["ci"], frontend);
  }
  await build("cargo", ["build", "--locked"], root);
  const backend = launch(
    path.join(
      root,
      "target",
      "debug",
      windows ? "doccraft-agent.exe" : "doccraft-agent",
    ),
    [],
  );
  backend.once("exit", () => {
    if (!stopping) {
      console.error("백엔드가 종료되어 전체 서버를 중지합니다.");
      shutdown(1);
    }
  });
  backend.once("error", () => shutdown(1));
  await ready(`http://127.0.0.1:${backendPort}/health`);
  const ui = launch(
    process.execPath,
    [
      path.join(frontend, "node_modules", "vite", "bin", "vite.js"),
      "--host",
      "127.0.0.1",
      "--port",
      String(frontendPort),
      "--strictPort",
    ],
    frontend,
  );
  ui.once("exit", () => {
    if (!stopping) {
      console.error("프론트엔드가 종료되어 전체 서버를 중지합니다.");
      shutdown(1);
    }
  });
  ui.once("error", () => shutdown(1));
  await ready(`http://127.0.0.1:${frontendPort}/`);
  console.log(
    `\nFrontend: http://127.0.0.1:${frontendPort}\nBackend: http://127.0.0.1:${backendPort}\nCtrl+C 또는 다른 터미널에서 ${windows ? "stop_all.bat" : "./stop_all.sh"} 로 모두 중지합니다.`,
  );
} catch (e) {
  console.error(e.message);
  shutdown(1);
}
