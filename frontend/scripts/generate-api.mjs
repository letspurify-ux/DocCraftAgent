import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
const root = new URL("../../", import.meta.url);
const spec = execFileSync("cargo", ["run", "--", "--openapi"], {
  cwd: root,
  encoding: "utf8",
});
writeFileSync(new URL("../openapi.json", import.meta.url), spec);
execFileSync(
  process.execPath,
  [
    "node_modules/openapi-typescript/bin/cli.js",
    "openapi.json",
    "-o",
    "src/api.generated.ts",
  ],
  { stdio: "inherit" },
);
