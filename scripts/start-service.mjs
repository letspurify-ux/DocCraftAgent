import fs from "node:fs";
import http from "node:http";
import net from "node:net";
import path from "node:path";
import { once } from "node:events";
import { killTree, root, run, windows } from "./all-processes.mjs";
import {
  owned,
  readServiceState,
  removeServiceState,
  serviceName,
  serviceStopRequested,
  writeServiceState,
} from "./individual-processes.mjs";

const service = serviceName(process.argv[2]);
const scriptName = `start_${service}`;
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
  if (state) removeServiceState(service, state);
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

function port(value) {
  const number = Number(value);
  if (!Number.isInteger(number) || number < 1 || number > 65535)
    throw new Error("포트는 1~65535 정수여야 합니다.");
  return number;
}

async function available(value) {
  const server = net.createServer();
  await new Promise((resolve, reject) => {
    server.once("error", (error) =>
      reject(
        new Error(
          `${value}번 포트를 사용할 수 없습니다 (${error.code}). 기존 서버를 종료하거나 포트 환경변수를 지정하세요.`,
        ),
      ),
    );
    server.listen(value, "127.0.0.1", resolve);
  });
  await new Promise((resolve) => server.close(resolve));
}

async function ready(url) {
  const deadline = Date.now() + 30000;
  while (!stopping && Date.now() < deadline) {
    try {
      const ok = await new Promise((resolve, reject) => {
        const request = http.get(
          url,
          { signal: AbortSignal.timeout(1000) },
          (response) => {
            response.resume();
            resolve(response.statusCode >= 200 && response.statusCode < 300);
          },
        );
        request.on("error", reject);
      });
      if (ok) return;
    } catch {}
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
  throw new Error(`서버 준비 실패: ${url}`);
}

try {
  const previous = readServiceState(service);
  if (owned(previous)) throw new Error(`이미 ${scriptName}가 실행 중입니다.`);
  if (previous) removeServiceState(service, previous);
  state = writeServiceState(service);
  const stopWatcher = setInterval(() => {
    if (serviceStopRequested(service, state)) shutdown();
  }, 200);
  stopWatcher.unref();

  if (service === "backend") {
    const backendPort = port(process.env.DOCCRAFT_PORT ?? 8765);
    await available(backendPort);
    await build("cargo", ["build", "--locked"], root);
    const server = launch(
      path.join(
        root,
        "target",
        "debug",
        windows ? "doccraft-agent.exe" : "doccraft-agent",
      ),
      [],
    );
    server.once("exit", () => {
      if (!stopping) {
        console.error("백엔드가 종료됐습니다.");
        shutdown(1);
      }
    });
    server.once("error", () => shutdown(1));
    await ready(`http://127.0.0.1:${backendPort}/health`);
    console.log(
      `\nBackend: http://127.0.0.1:${backendPort}\nCtrl+C 또는 다른 터미널에서 ${windows ? "stop_backend.bat" : "./stop_backend.sh"} 로 중지합니다.`,
    );
  } else {
    const frontendPort = port(process.env.DOCCRAFT_FRONTEND_PORT ?? 6001);
    const backendPort = port(process.env.DOCCRAFT_PORT ?? 8765);
    await available(frontendPort);
    const frontend = path.join(root, "frontend");
    if (
      !fs.existsSync(
        path.join(frontend, "node_modules", "vite", "bin", "vite.js"),
      )
    ) {
      await build("npm", ["ci"], frontend);
    }
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
        console.error("프론트엔드가 종료됐습니다.");
        shutdown(1);
      }
    });
    ui.once("error", () => shutdown(1));
    await ready(`http://127.0.0.1:${frontendPort}/`);
    console.log(
      `\nFrontend: http://127.0.0.1:${frontendPort}\nAPI proxy: http://127.0.0.1:${backendPort}\nCtrl+C 또는 다른 터미널에서 ${windows ? "stop_frontend.bat" : "./stop_frontend.sh"} 로 중지합니다.`,
    );
  }
} catch (error) {
  console.error(error.message);
  shutdown(1);
}
