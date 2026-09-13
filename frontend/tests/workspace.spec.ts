import { test, expect } from "@playwright/test";
import path from "node:path";

test("workspace, settings, responsive layout and safe origin", async ({
  page,
  request,
}) => {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto("/");
  await expect(
    page.getByRole("heading", { name: "코드에서 문서까지" }),
  ).toBeVisible();
  await page.screenshot({ path: "test-results/workspace.png", fullPage: true });
  await page.getByRole("button", { name: "설정", exact: true }).click();
  await expect(page.getByRole("heading", { name: "공통 설정" })).toBeVisible();
  await page
    .getByRole("button", { name: "컨텍스트 · Reasoning", exact: true })
    .click();
  await expect(page.getByLabel("앱 컨텍스트 한도 (최대 200,000)")).toHaveValue(
    "200000",
  );
  await page.getByRole("button", { name: "MariaDB", exact: true }).click();
  await expect(page.getByLabel("비밀번호", { exact: true })).toHaveValue(
    "********",
  );
  await page.getByRole("button", { name: "DB 연결 테스트" }).click();
  await expect(page.getByRole("status")).toContainText('"ok":true');
  await page.setViewportSize({ width: 760, height: 900 });
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= innerWidth,
    ),
  ).toBe(true);
  expect(errors).toEqual([]);
  const denied = await request.post("/api/v1/batch", {
    headers: { Origin: "https://untrusted.example" },
    data: { ids: [] },
  });
  expect(denied.status()).toBe(403);
});

test("create, run, inspect and render a document through the UI", async ({
  page,
}) => {
  test.skip(
    !process.env.UI_SOURCE_ROOT || !process.env.UI_OUTPUT_ROOT,
    "Requires isolated integration fixture",
  );
  const title = "UI 검증 문서 " + Date.now();
  await page.goto("/");
  await page.getByRole("button", { name: "새 작업", exact: true }).click();
  await page.getByLabel("작업 이름", { exact: true }).fill(title);
  await page
    .getByLabel("소스 경로", { exact: true })
    .fill(process.env.UI_SOURCE_ROOT!);
  await page
    .getByLabel("대상 Markdown 경로", { exact: true })
    .fill(path.join(process.env.UI_OUTPUT_ROOT!, "ui-guide.md"));
  await page
    .getByRole("button", { name: "개발자 흐름도", exact: true })
    .click();
  await page.getByLabel("전체 다이어그램 상한 (빈칸=자동, 0=없음)").fill("3");
  await page.getByRole("button", { name: "작업 저장", exact: true }).click();
  const savedTasks = await (await page.request.get("/api/v1/tasks")).json();
  expect(
    savedTasks.find((task: { name: string }) => task.name === title)
      .max_diagrams,
  ).toBe(3);
  const card = page
    .locator(".task-card")
    .filter({ has: page.getByRole("heading", { name: title, exact: true }) });
  await expect(card).toBeVisible();
  await card.getByRole("button", { name: "실행", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "실행 모니터", exact: true }),
  ).toBeVisible();
  await expect(page.locator(".run-detail .badge").first()).toHaveText("완료", {
    timeout: 40000,
  });
  await expect(page.locator(".event-list")).toContainText("terminal");
  await page
    .getByRole("button", { name: "문서 라이브러리", exact: true })
    .click();
  const option = await page
    .getByLabel("문서 선택")
    .locator("option")
    .filter({ hasText: "ui-guide.md" })
    .first()
    .getAttribute("value");
  await page.getByLabel("문서 선택").selectOption(option!);
  await expect(page.locator(".markdown h1")).toHaveText(title);
  await expect(page.locator(".mermaid svg").first()).toBeVisible();
  const reference = page.locator(".markdown [data-footnote-ref]").first();
  await expect(reference).toBeVisible();
  const footnote = (await reference.getAttribute("href"))!.slice(1);
  await reference.click();
  await expect(page.locator(`[id="${footnote}"]`)).toBeVisible();
  await page.screenshot({ path: "test-results/document.png", fullPage: true });
});

test("partial artifact shows review warning before document", async ({
  page,
}) => {
  test.skip(
    !process.env.UI_SOURCE_ROOT,
    "Requires isolated integration fixture",
  );
  await page.goto("/");
  await page
    .getByRole("button", { name: "문서 라이브러리", exact: true })
    .click();
  const option = page
    .getByLabel("문서 선택")
    .locator("option")
    .filter({ hasText: "quota-partial.md" })
    .first();
  await expect(option).toContainText("부분 생성");
  await page
    .getByLabel("문서 선택")
    .selectOption((await option.getAttribute("value"))!);
  await expect(page.locator(".document-warning")).toContainText(
    "부분 생성 · 전체 검토 미완료",
  );
  await expect(page.locator(".document-warning li").first()).not.toBeVisible();
  await page.locator(".document-warning summary").click();
  await expect(page.locator(".document-warning li").first()).toBeVisible();
  await expect(page.locator(".document-warning")).toContainText(
    "Missing planned sections",
  );
  await expect(page.locator("article.markdown")).toContainText(
    "부분 생성 · 전체 검토 미완료",
  );
});
