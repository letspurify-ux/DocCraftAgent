import { defineConfig } from "@playwright/test";
export default defineConfig({
  testDir: "tests",
  workers: 1,
  retries: 0,
  timeout: 45000,
  use: {
    baseURL: process.env.DOCCRAFT_UI_URL ?? "http://127.0.0.1:8765",
    headless: true,
    viewport: { width: 1440, height: 1000 },
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  reporter: [["list"], ["html", { open: "never" }]],
});
