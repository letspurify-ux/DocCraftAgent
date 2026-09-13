import { test, expect } from "@playwright/test";

test("diagrams survive polling and update when selecting another document", async ({
  page,
}) => {
  const docs = [
    {
      id: "diagram-a",
      task_id: "task",
      path: "/test/a.md",
      created_at: "2026-09-12",
      warnings: [],
    },
    {
      id: "diagram-b",
      task_id: "task",
      path: "/test/b.md",
      created_at: "2026-09-12",
      warnings: [],
    },
  ];
  let polls = 0;
  await page.route("**/api/v1/**", async (route) => {
    const url = new URL(route.request().url());
    const endpoint = url.pathname.replace("/api/v1", "");
    let json: unknown = {};
    if (endpoint === "/settings")
      json = { llm: { model: "test" }, source_roots: ["/test"] };
    if (endpoint === "/artifacts") {
      polls++;
      json = { artifacts: docs };
    } else if (endpoint === "/tasks" || endpoint === "/runs") json = [];
    else if (endpoint === "/diagnostics") json = { database: true };
    else if (endpoint.startsWith("/artifacts/")) {
      const a = endpoint.endsWith("diagram-a");
      json = {
        path: a ? "/test/a.md" : "/test/b.md",
        markdown: a
          ? '# First\n\n```mermaid\nflowchart LR\n A["Original"] --> B["Done"]\n```\n\n```mermaid\nflowchart LR\n C["Second"] --> D["Done"]\n```'
          : '# Changed\n\n```mermaid\nflowchart LR\n A["Updated"] --> B["Different"]\n```',
      };
    }
    await route.fulfill({ json });
  });
  await page.goto("/");
  await page
    .getByRole("button", { name: "문서 라이브러리", exact: true })
    .click();
  await page.getByLabel("문서 선택").selectOption("diagram-a");
  const svgs = page.locator(".mermaid svg");
  await expect(svgs).toHaveCount(2);
  const original = await svgs.evaluateAll((nodes) => nodes.map((n) => n.id));
  const before = polls;
  await expect
    .poll(() => polls, { timeout: 12000 })
    .toBeGreaterThanOrEqual(before + 3);
  await expect(svgs).toHaveCount(2);
  expect(await svgs.evaluateAll((nodes) => nodes.map((n) => n.id))).toEqual(
    original,
  );
  await page.getByLabel("문서 선택").selectOption("diagram-b");
  await expect(page.locator(".markdown h1")).toHaveText("Changed");
  await expect(svgs).toHaveCount(1);
  await expect(svgs).toContainText("Updated");
});
