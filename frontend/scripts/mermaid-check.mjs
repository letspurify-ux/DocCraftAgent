import { JSDOM } from "jsdom";
const dom = new JSDOM("<!doctype html><html><body></body></html>");
globalThis.window = dom.window;
globalThis.document = dom.window.document;
const { default: mermaid } = await import("mermaid");
mermaid.initialize({
  startOnLoad: false,
  securityLevel: "strict",
  maxTextSize: 50000,
});
let text = "";
for await (const chunk of process.stdin) {
  text += chunk;
  if (text.length > 4 * 1024 * 1024) process.exit(2);
}
try {
  for (const match of text.matchAll(/```mermaid\s*\n([\s\S]*?)```/g)) {
    await mermaid.parse(match[1]);
  }
} catch {
  process.exit(1);
}
