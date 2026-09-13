import {
  readState,
  owned,
  removeState,
  requestStop,
} from "./all-processes.mjs";
try {
  const state = readState();
  if (!state) console.log("start_all로 실행 중인 서버가 없습니다.");
  else if (!owned(state)) {
    removeState(state);
    console.log(
      "이전 실행 기록을 정리했습니다. 실행 중인 다른 프로세스는 중지하지 않았습니다.",
    );
  } else {
    requestStop(state);
    const deadline = Date.now() + 30000;
    while (owned(state) && Date.now() < deadline)
      await new Promise((resolve) => setTimeout(resolve, 100));
    if (owned(state))
      throw new Error(
        "종료 대기 시간이 초과되었습니다. start_all 터미널을 확인하세요.",
      );
    removeState(state);
    console.log("백엔드와 프론트엔드를 모두 중지했습니다.");
  }
} catch (e) {
  console.error(e.message);
  process.exitCode = 1;
}
