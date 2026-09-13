import { test, expect } from "@playwright/test";

test("poll failure marks stale state and recovers automatically", async ({
  page,
}) => {
  let unavailable = false;
  await page.route("**/api/v1/**", async (route) => {
    const endpoint = new URL(route.request().url()).pathname.replace(
      "/api/v1",
      "",
    );
    if (unavailable && endpoint === "/runs") {
      await route.fulfill({
        status: 503,
        json: { error: "Database unavailable" },
      });
      return;
    }
    const json =
      endpoint === "/settings"
        ? { llm: { model: "test" }, source_roots: ["/test"] }
        : endpoint === "/tasks" || endpoint === "/runs"
          ? []
          : endpoint === "/artifacts"
            ? { artifacts: [] }
            : endpoint === "/diagnostics"
              ? { database: true }
              : {};
    await route.fulfill({ json });
  });
  await page.goto("/");
  await expect(page.getByText("MariaDB 연결됨", { exact: true })).toBeVisible();
  unavailable = true;
  await expect(page.getByRole("alert")).toContainText("마지막 확인값", {
    timeout: 10000,
  });
  await expect(
    page.getByText("서버 연결 확인 필요", { exact: true }),
  ).toBeVisible();
  unavailable = false;
  await expect(page.getByRole("alert")).toHaveCount(0, { timeout: 10000 });
  await expect(page.getByText("MariaDB 연결됨", { exact: true })).toBeVisible();
});
