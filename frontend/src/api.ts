export async function api<T = any>(
  path: string,
  options: RequestInit = {},
): Promise<T> {
  const response = await fetch("/api/v1" + path, {
    ...options,
    credentials: "same-origin",
    headers: { "Content-Type": "application/json", ...options.headers },
  });
  const text = await response.text();
  let value: any;
  try {
    value = text.trim() ? JSON.parse(text) : undefined;
  } catch {
    /* Proxy and framework errors may not be JSON. */
  }
  if (!response.ok) {
    const message =
      typeof value?.error === "string"
        ? value.error
        : response.status === 403
          ? "요청이 거절되었습니다. 프론트엔드·백엔드 포트 설정을 확인하세요."
          : response.status === 401
            ? "세션이 만료되었습니다. 화면을 새로고침하세요."
            : response.status >= 500
              ? "서버 응답에 문제가 있습니다. 백엔드 실행 상태와 프록시 설정을 확인하세요."
              : "요청을 처리하지 못했습니다. 입력값과 서버 상태를 확인하세요.";
    throw new Error(`${message} (HTTP ${response.status})`);
  }
  if (response.status === 204) return undefined as T;
  if (value === undefined)
    throw new Error(
      `서버가 유효한 JSON 응답을 반환하지 않았습니다. (HTTP ${response.status})`,
    );
  return value;
}
export const send = (method: string, value?: unknown): RequestInit => ({
  method,
  body: value === undefined ? undefined : JSON.stringify(value),
});
import type { components } from "./api.generated";
export type Task = components["schemas"]["TaskConfig"];
export type Run = Omit<components["schemas"]["RunView"], "progress"> & {
  progress: Record<string, any>;
};
export type Artifact = {
  id: string;
  run_id: string;
  task_id: string;
  path: string;
  warnings: string[];
  created_at: string;
};
export const newTask = (): Task => ({
  id: "",
  name: "",
  sources: [],
  target: "",
  direction: "",
  language: "한국어",
  include: [],
  exclude: [],
  max_iterations: 3,
  max_diagrams: null,
  max_seconds: 7200,
  max_tokens: 2000000,
  max_cost: 0,
});
