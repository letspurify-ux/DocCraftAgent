import { spawn, execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const windows = process.platform === "win32";
export const root = fileURLToPath(new URL("../", import.meta.url));
export const statePath = path.join(
  process.env.DOCCRAFT_PROCESS_STATE_DIR ?? path.join(root, ".local"),
  "all-processes.json",
);
const stableLocale = { ...process.env, LANG: "C", LC_ALL: "C" };
export function identity(pid) {
  if (!Number.isSafeInteger(pid) || pid <= 1) return null;
  try {
    if (windows) {
      const command = `$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[System.Text.UTF8Encoding]::new(); $p=Get-CimInstance Win32_Process -Filter "ProcessId = ${pid}"; if ($null -eq $p) { exit 1 }; if (!$p.CommandLine) { exit 1 }; [ordered]@{created=$p.CreationDate.ToUniversalTime().Ticks.ToString();command=$p.CommandLine} | ConvertTo-Json -Compress`;
      return (
        execFileSync(
          "powershell.exe",
          ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", command],
          {
            encoding: "utf8",
            windowsHide: true,
            timeout: 5000,
            stdio: ["ignore", "pipe", "ignore"],
          },
        ).trim() || null
      );
    }
    return (
      execFileSync("ps", ["-p", String(pid), "-o", "lstart=", "-o", "args="], {
        encoding: "utf8",
        env: stableLocale,
      }).trim() || null
    );
  } catch {
    return null;
  }
}

export function discoverManagers() {
  if (windows) return [];
  const scripts = [
    { label: "start_all", suffix: path.join(root, "scripts", "start-all.mjs") },
    ...["frontend", "backend"].map((service) => ({
      label: `start_${service}`,
      suffix: `${path.join(root, "scripts", "start-service.mjs")} ${service}`,
    })),
  ];
  try {
    const processes = execFileSync("ps", ["-axo", "pid=", "-o", "args="], {
      encoding: "utf8",
      env: stableLocale,
    });
    const found = [];
    for (const line of processes.split("\n")) {
      const match = line.match(/^\s*(\d+)\s+(.+)$/);
      if (!match) continue;
      const pid = Number(match[1]);
      const command = match[2].trim();
      const script = scripts.find(
        ({ suffix }) => command === suffix || command.endsWith(` ${suffix}`),
      );
      if (!script) continue;
      const processIdentity = identity(pid);
      if (processIdentity)
        found.push({ ...script, pid, identity: processIdentity });
    }
    return found;
  } catch {
    return [];
  }
}
export function readState() {
  try {
    return JSON.parse(fs.readFileSync(statePath, "utf8"));
  } catch (e) {
    if (e.code === "ENOENT") return null;
    throw e;
  }
}
export function owned(state) {
  return (
    !!state &&
    typeof state.identity === "string" &&
    state.identity.length > 0 &&
    identity(state.pid) === state.identity
  );
}
export function removeState(state) {
  const current = readState();
  if (current?.pid === state.pid && current?.identity === state.identity) {
    fs.unlinkSync(statePath);
    fs.rmSync(stopPath(state), { force: true });
  }
}
export function run(command, args, options = {}) {
  if (windows && command === "npm") {
    if (args.length !== 1 || args[0] !== "ci")
      throw new Error("Unsupported npm command");
    return spawn(
      process.env.ComSpec || "cmd.exe",
      ["/d", "/s", "/c", "npm ci"],
      { cwd: root, stdio: "inherit", windowsHide: true, ...options },
    );
  }
  return spawn(command, args, { cwd: root, stdio: "inherit", ...options });
}

export function stopPath(state) {
  return path.join(path.dirname(statePath), `stop-all-${state.pid}.json`);
}
export function requestStop(state) {
  if (!owned(state))
    throw new Error("실행 프로세스 소유권을 확인할 수 없습니다.");
  fs.writeFileSync(
    stopPath(state),
    JSON.stringify({ identity: state.identity }),
    { mode: 0o600 },
  );
}
export function stopRequested(state) {
  try {
    return (
      JSON.parse(fs.readFileSync(stopPath(state), "utf8")).identity ===
      state.identity
    );
  } catch {
    return false;
  }
}
export function killTree(pid, force = false) {
  if (!Number.isSafeInteger(pid) || pid <= 1) return;
  if (windows) {
    execFileSync("taskkill.exe", ["/PID", String(pid), "/T", "/F"], {
      windowsHide: true,
      stdio: "ignore",
      timeout: 5000,
    });
  } else process.kill(-pid, force ? "SIGKILL" : "SIGINT");
}
