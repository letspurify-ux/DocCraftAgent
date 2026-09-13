import fs from "node:fs";
import path from "node:path";
import { identity, owned, root } from "./all-processes.mjs";

const stateDir =
  process.env.DOCCRAFT_PROCESS_STATE_DIR ?? path.join(root, ".local");

export function serviceName(value) {
  if (value !== "backend" && value !== "frontend")
    throw new Error("서비스는 backend 또는 frontend여야 합니다.");
  return value;
}

export function serviceStatePath(service) {
  return path.join(stateDir, `${service}-process.json`);
}

function serviceStopPath(service, state) {
  return path.join(stateDir, `stop-${service}-${state.pid}.json`);
}

export function readServiceState(service) {
  try {
    return JSON.parse(fs.readFileSync(serviceStatePath(service), "utf8"));
  } catch (error) {
    if (error.code === "ENOENT") return null;
    throw error;
  }
}

export function writeServiceState(service) {
  fs.mkdirSync(stateDir, { recursive: true, mode: 0o700 });
  const state = {
    service,
    pid: process.pid,
    identity: identity(process.pid),
  };
  if (!state.identity)
    throw new Error("프로세스 식별 정보를 읽을 수 없어 시작을 중단합니다.");
  fs.writeFileSync(serviceStatePath(service), JSON.stringify(state), {
    flag: "wx",
    mode: 0o600,
  });
  return state;
}

export function removeServiceState(service, state) {
  const current = readServiceState(service);
  if (
    current?.pid === state.pid &&
    current?.identity === state.identity &&
    current?.service === service
  ) {
    fs.unlinkSync(serviceStatePath(service));
    fs.rmSync(serviceStopPath(service, state), { force: true });
  }
}

export function requestServiceStop(service, state) {
  if (!owned(state) || state.service !== service)
    throw new Error("실행 프로세스 소유권을 확인할 수 없습니다.");
  fs.writeFileSync(
    serviceStopPath(service, state),
    JSON.stringify({ identity: state.identity }),
    { mode: 0o600 },
  );
}

export function serviceStopRequested(service, state) {
  try {
    return (
      JSON.parse(fs.readFileSync(serviceStopPath(service, state), "utf8"))
        .identity === state.identity
    );
  } catch {
    return false;
  }
}

export { owned };
