import { test, expect } from "@playwright/test";

test("outline preview edits stay local until submitted with their base revision", async ({
  page,
}) => {
  let submitted: any;
  const run = {
    id: "outline-run",
    task_id: "task",
    status: "awaiting_outline",
    progress: { title: "목차 확인" },
    tokens: 200,
    cost: 0,
    created_at: "2026-09-14 12:00:00",
    updated_at: "2026-09-14 12:00:00",
  };
  const outline = {
    revision: 1,
    reader_goal: "입력과 결과 이해",
    storyline: "입력을 검증한 뒤 결과를 확인한다",
    sections: [
      {
        id: "s1",
        title: "처리 결과",
        query: "service.py",
        handoff: "",
        depends_on: [],
        evidence_ids: ["a".repeat(64)],
        key_points: ["정상 결과 확인"],
        out_of_scope: [],
        diagrams: [],
      },
    ],
    terminology: [],
  };
  await page.route("**/api/v1/**", async (route) => {
    const path = new URL(route.request().url()).pathname;
    if (path.endsWith("/revisions")) {
      submitted = route.request().postDataJSON();
      await route.fulfill({ json: { id: run.id, revision: 2 } });
      return;
    }
    const json = path.endsWith("/tasks")
      ? [
          {
            id: "task",
            name: "미리보기 작업",
            direction: "문서",
            sources: ["/source"],
            target: "/output/a.md",
          },
        ]
      : path.endsWith("/runs")
        ? [run]
        : path.endsWith("/outline")
          ? { outline, review: { issues: [] } }
          : path.endsWith("/settings")
            ? { llm: { model: "test" } }
            : path.endsWith("/artifacts")
              ? { artifacts: [] }
              : {};
    await route.fulfill({ json });
  });
  await page.goto("/");
  await page.getByRole("button", { name: "실행 모니터", exact: true }).click();
  await page.getByRole("button", { name: "문서 구성", exact: true }).click();
  await expect(
    page.getByRole("button", { name: "목차 확정·본문 작성" }),
  ).toBeEnabled();
  await expect(page.getByLabel("독자가 해결할 질문")).toHaveCount(0);
  await page
    .getByLabel("핵심 설명 · 한 줄에 하나")
    .fill("처리한 결과를 반환한다");
  await page.getByLabel("제목", { exact: true }).fill("결과 확인하기");
  await expect(
    page.getByRole("button", { name: "목차 확정·본문 작성" }),
  ).toBeDisabled();
  expect(submitted).toBeUndefined();
  await page
    .getByRole("button", { name: "수정 목차 검토", exact: true })
    .click();
  await expect
    .poll(() => submitted?.outline?.sections[0].title)
    .toBe("결과 확인하기");
  expect(submitted.outline.sections[0].key_points).toEqual([
    "처리한 결과를 반환한다",
  ]);
  expect(submitted.base_revision).toBe(1);
  expect(submitted.outline.sections[0].id).toBe("s1");
});
