import { test } from "node:test";
import assert from "node:assert/strict";
import { api } from "../src/api.ts";

test("API reports empty/non-JSON HTTP errors and preserves structured server errors", async () => {
  const original = globalThis.fetch;
  try {
    for (const [status, body, expected] of [
      [403, "", /포트 설정.*HTTP 403/],
      [502, "<html>Bad Gateway</html>", /백엔드 실행 상태.*HTTP 502/],
      [
        400,
        JSON.stringify({ error: "Invalid source path" }),
        /Invalid source path.*HTTP 400/,
      ],
      [200, "", /유효한 JSON/],
    ]) {
      globalThis.fetch = async () => new Response(body, { status });
      await assert.rejects(api("/tasks"), expected);
    }
    globalThis.fetch = async () => new Response(null, { status: 204 });
    assert.equal(await api("/tasks"), undefined);
    globalThis.fetch = async () => Response.json({ id: "saved" });
    assert.deepEqual(await api("/tasks"), { id: "saved" });
  } finally {
    globalThis.fetch = original;
  }
});
