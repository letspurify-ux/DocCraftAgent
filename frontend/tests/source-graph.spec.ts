import { test, expect } from "@playwright/test";

test("source graph and omission ledger retain paged records and explain unresolved coverage", async ({
  page,
}) => {
  const run = {
    id: "graph-run",
    task_id: "task",
    status: "partial",
    progress: { title: "원본 대조 미완료" },
    tokens: 500,
    cost: 0,
    created_at: "2026-09-15 12:00:00",
    updated_at: "2026-09-15 12:00:00",
  };
  const symbol = (name: string) => ({
    id: name,
    qualified_name: name,
    kind: "method",
    span: { start: 4, end: 12 },
  });
  await page.route("**/api/v1/**", async (route) => {
    const url = new URL(route.request().url());
    const path = url.pathname;
    let json: unknown = {};
    if (path.endsWith("/tasks"))
      json = [
        {
          id: "task",
          name: "그래프 검증",
          sources: ["/source"],
          target: "/output/doc.md",
          direction: "취소 흐름 문서",
        },
      ];
    else if (path.endsWith("/runs")) json = [run];
    else if (path.endsWith("/settings")) json = { llm: { model: "test" } };
    else if (path.endsWith("/artifacts")) json = { artifacts: [] };
    else if (path.endsWith("/understanding"))
      json = {
        total_files: 2,
        coverage: {
          complete: true,
          reading_complete: true,
          read_files: 2,
          read_chunks: 3,
          read_batches: 1,
        },
        batches: [],
        has_more: false,
        next_cursor: "",
        graph: {
          symbols: 3,
          edges: 4,
          unsupported_files: 1,
          parse_error_files: 0,
        },
        document_coverage: {
          scope: "scope-a",
          complete: true,
          checked: 7,
          covered: 4,
          out_of_scope: 2,
          missing: 1,
        },
      };
    else if (path.endsWith("/graph")) {
      if (!url.searchParams.has("file_id"))
        json = {
          files: [{ id: 1, path: "/source/worker.py" }],
          next_cursor: 1,
          has_more: false,
        };
      else if (url.searchParams.get("after") === "0")
        json = {
          path: "/source/worker.py",
          supported: true,
          parse_errors: false,
          symbols: [symbol("Worker::run")],
          edges: [
            {
              source_name: "Worker::run",
              edge: { kind: "calls", target: "store", span: { start: 8 } },
            },
          ],
          next_cursor: 50,
          has_more: true,
        };
      else
        json = {
          path: "/source/worker.py",
          supported: true,
          parse_errors: false,
          symbols: [symbol("Worker::cancel")],
          edges: [],
          next_cursor: 100,
          has_more: false,
        };
    } else if (path.endsWith("/coverage"))
      json = {
        items: [
          {
            path: "/source/worker.py",
            obligations: [
              { id: "c1", subject: "Worker::cancel" },
              { id: "c2", subject: "debug" },
            ],
            assessments: [
              {
                id: "c1",
                status: "missing",
                reason: "저장 전에 취소되는 조건을 설명해야 합니다.",
                quote: "",
              },
              {
                id: "c2",
                status: "out_of_scope",
                reason: "디버그 로그는 취소 흐름 안내의 범위 밖입니다.",
                quote: "",
              },
            ],
          },
        ],
        next_cursor: "end",
        has_more: false,
      };
    await route.fulfill({ json });
  });
  await page.goto("/");
  await page.getByRole("button", { name: "실행 모니터", exact: true }).click();
  await page.getByRole("button", { name: "소스 이해", exact: true }).click();
  await page
    .getByText("코드 그래프 · 심볼 3개 / 관계·분기 4개", { exact: true })
    .click();
  await expect(page.getByText(/원본 대조 완료: 7개 확인/)).toBeVisible();
  await expect(page.getByText(/구조 분석 미지원 1개/)).toBeVisible();
  await page.getByRole("button", { name: "파일별 그래프 보기" }).click();
  await page
    .getByRole("button", { name: "/source/worker.py", exact: true })
    .click();
  await expect(
    page
      .getByRole("region", { name: "선택한 파일의 코드 그래프" })
      .getByText("Worker::run", { exact: true })
      .first(),
  ).toBeVisible();
  await page.getByRole("button", { name: "관계 더 보기" }).click();
  await expect(page.getByText("Worker::cancel", { exact: true })).toBeVisible();
  await expect(
    page.getByText("Worker::run", { exact: true }).first(),
  ).toBeVisible();
  await page.getByRole("button", { name: "설명 확인·제외 이유 보기" }).click();
  await page
    .locator("summary")
    .filter({ hasText: "/source/worker.py" })
    .click();
  await expect(
    page.getByText("저장 전에 취소되는 조건을 설명해야 합니다."),
  ).toBeVisible();
  await expect(
    page.getByText("디버그 로그는 취소 흐름 안내의 범위 밖입니다."),
  ).toBeVisible();
});
