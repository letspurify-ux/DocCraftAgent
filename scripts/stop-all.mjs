import {
  readState,
  owned as allOwned,
  removeState,
  requestStop,
  discoverManagers,
} from "./all-processes.mjs";
import {
  owned as serviceOwned,
  readServiceState,
  removeServiceState,
  requestServiceStop,
} from "./individual-processes.mjs";

try {
  const targets = [
    {
      label: "start_all",
      state: readState(),
      owned: allOwned,
      remove: removeState,
      request: requestStop,
    },
    ...["frontend", "backend"].map((service) => ({
      label: `start_${service}`,
      state: readServiceState(service),
      owned: (state) => serviceOwned(state) && state.service === service,
      remove: (state) => removeServiceState(service, state),
      request: (state) => requestServiceStop(service, state),
    })),
  ];
  const trackedPids = new Set(
    targets.flatMap((target) =>
      target.owned(target.state) && Number.isSafeInteger(target.state.pid)
        ? [target.state.pid]
        : [],
    ),
  );
  for (const manager of discoverManagers()) {
    if (trackedPids.has(manager.pid)) continue;
    targets.push({
      label: manager.label,
      state: manager,
      owned: allOwned,
      remove: () => {},
      request: (state) => process.kill(state.pid, "SIGINT"),
    });
  }
  const running = [];
  let stale = false;
  for (const target of targets) {
    if (!target.state) continue;
    if (!target.owned(target.state)) {
      target.remove(target.state);
      stale = true;
      continue;
    }
    target.request(target.state);
    running.push(target);
  }

  if (running.length === 0) {
    console.log(
      stale
        ? "이전 실행 기록을 정리했습니다. 실행 중인 서버가 없습니다."
        : "실행 중인 서버가 없습니다.",
    );
  } else {
    const deadline = Date.now() + 30000;
    while (
      running.some((target) => target.owned(target.state)) &&
      Date.now() < deadline
    )
      await new Promise((resolve) => setTimeout(resolve, 100));

    const remaining = running.filter((target) => target.owned(target.state));
    for (const target of running) {
      if (!target.owned(target.state)) target.remove(target.state);
    }
    if (remaining.length > 0)
      throw new Error(
        `종료 대기 시간이 초과되었습니다. ${remaining
          .map((target) => target.label)
          .join(", ")} 터미널을 확인하세요.`,
      );
    console.log("백엔드와 프론트엔드를 모두 중지했습니다.");
  }
} catch (e) {
  console.error(e.message);
  process.exitCode = 1;
}
