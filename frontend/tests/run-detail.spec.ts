import { test, expect, type Page, type Route } from "@playwright/test";

const run = (id: string, name: string) => ({
  id,
  task_id: id,
  status: "completed",
  progress: { stage: "finished", title: name },
  tokens: 123,
  cost: 0,
  created_at: "2026-09-13 12:00:00",
});

const file = (id: number, prefix: string) => ({
  id,
  path: `/source/${prefix}-${id}.ts`,
  language: "typescript",
  status: "indexed",
  detail: "{}",
});

async function mockWorkspace(
  page: Page,
  runs: ReturnType<typeof run>[],
  files: (route: Route, endpoint: string) => Promise<void>,
  eventBody = "retry: 60000\n\n",
) {
  await page.route("**/api/v1/**", async (route) => {
    const endpoint = new URL(route.request().url()).pathname.replace(
      "/api/v1",
      "",
    );
    if (endpoint.endsWith("/events")) {
      await route.fulfill({
        status: 200,
        headers: { "content-type": "text/event-stream" },
        body: eventBody,
      });
      return;
    }
    if (endpoint.includes("/files")) {
      await files(route, endpoint);
      return;
    }
    const json =
      endpoint === "/settings"
        ? { llm: { model: "test" }, source_roots: ["/source"] }
        : endpoint === "/runs"
          ? runs
          : endpoint === "/tasks"
            ? runs.map((item) => ({
                id: item.task_id,
                name: item.progress.title,
                sources: ["/source"],
                target: `/output/${item.id}.md`,
                direction: "테스트 문서",
                language: "한국어",
                include: [],
                exclude: [],
                max_iterations: 1,
                max_diagrams: 0,
                max_seconds: 60,
                max_tokens: 10000,
                max_cost: 0,
              }))
            : endpoint === "/artifacts"
              ? { artifacts: [] }
              : endpoint === "/diagnostics"
                ? { database: true }
                : {};
    await route.fulfill({ json });
  });
}

async function openFiles(page: Page) {
  await page.goto("/");
  await page.getByRole("button", { name: "실행 모니터", exact: true }).click();
  await page
    .getByRole("button", { name: "파일 분석 내역", exact: true })
    .click();
}

test("task cards stay inside a narrow mobile workspace", async ({ page }) => {
  await mockWorkspace(page, [run("run-a", "모바일 작업")], async (route) =>
    route.fulfill({ json: { files: [], has_more: false } }),
  );
  await page.setViewportSize({ width: 320, height: 800 });
  await page.goto("/");
  await expect(page.locator(".task-card")).toHaveCount(1);
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
  const card = await page.locator(".task-card").boundingBox();
  expect(card).not.toBeNull();
  expect(card!.x + card!.width).toBeLessThanOrEqual(320);

  await page.getByRole("button", { name: "새 작업", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "새 문서화 작업", exact: true }),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
});

test("terminal event closes the browser event stream", async ({ page }) => {
  let connections = 0;
  page.on("request", (request) => {
    if (new URL(request.url()).pathname.endsWith("/events")) connections++;
  });
  await mockWorkspace(
    page,
    [{ ...run("run-a", "실행 중인 작업"), status: "running" }],
    async (route) => route.fulfill({ json: { files: [], has_more: false } }),
    "retry: 50\n\nevent: stream-end\ndata: terminal\n\n",
  );
  await page.goto("/");
  await page.getByRole("button", { name: "실행 모니터", exact: true }).click();
  await expect(page.locator(".run-detail")).toBeVisible();
  await page.waitForTimeout(300);
  expect(connections).toBe(1);
});

test("terminal runs do not open an event stream", async ({ page }) => {
  let connections = 0;
  page.on("request", (request) => {
    if (new URL(request.url()).pathname.endsWith("/events")) connections++;
  });
  await mockWorkspace(page, [run("run-a", "종료된 실행")], async (route) =>
    route.fulfill({ json: { files: [], has_more: false } }),
  );
  await page.goto("/");
  await page.getByRole("button", { name: "실행 모니터", exact: true }).click();
  await expect(page.locator(".run-detail")).toBeVisible();
  await page.waitForTimeout(100);
  expect(connections).toBe(0);
});

test("warning-completed runs can continue with a higher UI review limit", async ({
  page,
}) => {
  await mockWorkspace(
    page,
    [
      {
        ...run("run-a", "추가 검토"),
        status: "completed_with_warnings",
        progress: { title: "추가 검토", warnings: ["minor issue"] },
      },
    ],
    async (route) => route.fulfill({ json: { files: [], has_more: false } }),
  );
  await page.goto("/");
  await page.getByRole("button", { name: "실행 모니터", exact: true }).click();
  await page
    .getByRole("checkbox", { name: "현재 작업의 검토 회차 적용" })
    .check();
  const resumed = page.waitForRequest(
    (request) =>
      request.method() === "POST" &&
      new URL(request.url()).pathname.endsWith("/runs/run-a/resume"),
  );
  await page.getByRole("button", { name: "체크포인트 재개" }).click();
  const url = new URL((await resumed).url());
  expect(url.searchParams.get("current_review_limit")).toBe("true");
});

test("UTC database timestamps are displayed in the browser timezone", async ({
  page,
}) => {
  await mockWorkspace(page, [run("run-a", "시간 표시")], async (route) =>
    route.fulfill({ json: { files: [], has_more: false } }),
  );
  await page.goto("/");
  await page.getByRole("button", { name: "실행 모니터", exact: true }).click();
  const expected = await page.evaluate(() =>
    new Intl.DateTimeFormat("ko-KR", {
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      hourCycle: "h23",
    }).format(new Date("2026-09-13T12:00:00Z")),
  );
  await expect(page.locator(".run-row small")).toHaveText(expected);
  await expect(page.locator(".run-row small")).not.toHaveText(
    "2026-09-13 12:00:00",
  );
});

test("file history pagination ends on the last page", async ({ page }) => {
  const firstPage = Array.from({ length: 200 }, (_, index) =>
    file(index + 1, "first"),
  );
  await mockWorkspace(page, [run("run-a", "실행 A")], async (route) => {
    const after = new URL(route.request().url()).searchParams.get("after");
    await route.fulfill({
      json: after
        ? { files: [file(201, "last")], has_more: false }
        : { files: firstPage, has_more: true },
    });
  });
  await openFiles(page);
  await expect(page.locator(".file-row")).toHaveCount(200);
  await page.getByRole("button", { name: "더 보기", exact: true }).click();
  await expect(page.locator(".file-row")).toHaveCount(201);
  await expect(page.getByRole("button", { name: "더 보기" })).toHaveCount(0);
  await expect(page.locator(".list-footer")).toContainText("201개 파일");
  await page.setViewportSize({ width: 320, height: 800 });
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
  await expect(
    page.getByRole("heading", { name: "실행 모니터", exact: true }),
  ).toBeVisible();
});

test("late file response does not overwrite another run", async ({ page }) => {
  await mockWorkspace(
    page,
    [run("run-a", "실행 A"), run("run-b", "실행 B")],
    async (route, endpoint) => {
      if (endpoint.startsWith("/runs/run-a/"))
        await new Promise((resolve) => setTimeout(resolve, 300));
      try {
        await route.fulfill({
          json: {
            files: [file(1, endpoint.startsWith("/runs/run-a/") ? "a" : "b")],
            has_more: false,
          },
        });
      } catch {
        // The old request may already be aborted when the selected run changes.
      }
    },
  );
  await openFiles(page);
  await page.locator(".run-row").filter({ hasText: "실행 B" }).click();
  await expect(page.locator(".file-list")).toContainText("/source/b-1.ts");
  await page.waitForTimeout(400);
  await expect(page.locator(".file-list")).not.toContainText("/source/a-1.ts");
});

test("malformed progress event is skipped without breaking later events", async ({
  page,
}) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await mockWorkspace(
    page,
    [{ ...run("run-a", "실행 A"), status: "running" }],
    async (route) => route.fulfill({ json: { files: [], has_more: false } }),
    [
      "retry: 60000",
      "event: progress",
      "data: not-json",
      "",
      "event: progress",
      'data: {"id":2,"kind":"stage","data":{"message":"healthy"}}',
      "",
      "",
    ].join("\n"),
  );
  await page.goto("/");
  await page.getByRole("button", { name: "실행 모니터", exact: true }).click();
  await expect(page.locator(".event-list")).toContainText("healthy");
  expect(errors).toEqual([]);
});

test("source understanding shows exact chunk progress", async ({ page }) => {
  await mockWorkspace(
    page,
    [{ ...run("run-a", "소스 이해"), status: "running" }],
    async (route) => route.fulfill({ json: { files: [], has_more: false } }),
    [
      "retry: 60000",
      "event: progress",
      'data: {"id":2,"kind":"source_batch","data":{"read_chunks":25,"total_chunks":100}}',
      "",
      "",
    ].join("\n"),
  );
  await page.goto("/");
  await page.getByRole("button", { name: "실행 모니터", exact: true }).click();
  await expect(
    page.getByRole("progressbar", { name: "전체 소스 읽기" }),
  ).toHaveAttribute("aria-valuenow", "25");
  await expect(page.locator(".source-progress")).toContainText("25.0%");
  await expect(page.locator(".source-progress")).toContainText("25 / 100 청크");
});
