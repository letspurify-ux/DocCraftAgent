/**
 * Rendering the Markdown a run produced, including its Mermaid diagrams.
 *
 * `mermaid` is imported lazily and only once: it is large, and most screens
 * never show a diagram. Component identities stay stable so the app's polling
 * updates do not re-render every diagram on every tick.
 */
import React, { useState, useEffect, useRef } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";

// Keep renderer component identities stable across the app's polling updates.
let mermaidLoader: Promise<(typeof import("mermaid"))["default"]> | undefined;
function loadMermaid() {
  mermaidLoader ??= import("mermaid")
    .then(({ default: m }) => {
      m.initialize({
        startOnLoad: false,
        securityLevel: "strict",
        theme: "neutral",
        maxTextSize: 50000,
      });
      return m;
    })
    .catch((error) => {
      mermaidLoader = undefined;
      throw error;
    });
  return mermaidLoader;
}
function Mermaid({ code }: { code: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const [err, setErr] = useState("");
  useEffect(() => {
    let cancelled = false;
    setErr("");
    loadMermaid()
      .then(async (m) => {
        if (cancelled) return;
        const { svg } = await m.render(
          "m" + crypto.randomUUID().replaceAll("-", ""),
          code,
        );
        if (!cancelled && ref.current) ref.current.innerHTML = svg;
      })
      .catch(() => {
        if (!cancelled) setErr("다이어그램 구문을 표시할 수 없습니다.");
      });
    return () => {
      cancelled = true;
    };
  }, [code]);
  return (
    <>
      {err && <pre>{err + "\n" + code}</pre>}
      <div className="mermaid" ref={ref} hidden={!!err} />
    </>
  );
}
const markdownComponents: Components = {
  code({ className, children, ...props }) {
    return className === "language-mermaid" ? (
      <Mermaid code={String(children)} />
    ) : (
      <code className={className} {...props}>
        {children}
      </code>
    );
  },
};
const markdownPlugins = [remarkGfm];
const Markdown = React.memo(function Markdown({ text }: { text: string }) {
  return (
    <ReactMarkdown
      remarkPlugins={markdownPlugins}
      components={markdownComponents}
    >
      {text}
    </ReactMarkdown>
  );
});

export { Markdown, Mermaid };
