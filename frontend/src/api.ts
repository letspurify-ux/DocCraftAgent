export async function api<T = any>(
  path: string,
  options: RequestInit = {},
): Promise<T> {
  const response = await fetch("/api/v1" + path, {
    ...options,
    credentials: "same-origin",
    headers: { "Content-Type": "application/json", ...options.headers },
  });
  const value = await response.json();
  if (!response.ok) throw new Error(value.error ?? `HTTP ${response.status}`);
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
  max_seconds: 7200,
  max_tokens: 2000000,
  max_cost: 0,
});
