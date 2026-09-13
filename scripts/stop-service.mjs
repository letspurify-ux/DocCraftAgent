import {
  owned,
  readServiceState,
  removeServiceState,
  requestServiceStop,
  serviceName,
} from "./individual-processes.mjs";

const service = serviceName(process.argv[2]);
const label = service === "backend" ? "백엔드" : "프론트엔드";
const scriptName = `start_${service}`;

try {
  const state = readServiceState(service);
  if (!state) console.log(`${scriptName}로 실행 중인 ${label}가 없습니다.`);
  else if (!owned(state) || state.service !== service) {
    removeServiceState(service, state);
    console.log(
      "이전 실행 기록을 정리했습니다. 실행 중인 다른 프로세스는 중지하지 않았습니다.",
    );
  } else {
    requestServiceStop(service, state);
    const deadline = Date.now() + 30000;
    while (owned(state) && Date.now() < deadline)
      await new Promise((resolve) => setTimeout(resolve, 100));
    if (owned(state))
      throw new Error(
        `종료 대기 시간이 초과되었습니다. ${scriptName} 터미널을 확인하세요.`,
      );
    removeServiceState(service, state);
    console.log(`${label}를 중지했습니다.`);
  }
} catch (error) {
  console.error(error.message);
  process.exitCode = 1;
}
