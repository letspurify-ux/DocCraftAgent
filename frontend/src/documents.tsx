/**
 * Published documents: reading one, and comparing it with an earlier version.
 */
import React, { useState, useEffect } from "react";
import { BookOpen, Download } from "lucide-react";
import { diffLines } from "diff";
import { api, type Artifact } from "./api";
import { Markdown } from "./markdown";
import { localTime } from "./shared";

function Documents({
  artifacts,
  onError,
}: {
  artifacts: Artifact[];
  onError: (e: string) => void;
}) {
  const [selected, setSelected] = useState(""),
    [doc, setDoc] = useState<any>(null),
    [compare, setCompare] = useState(""),
    [old, setOld] = useState("");
  const selectedArtifact = artifacts.find((a) => a.id === selected);
  const warnings = selectedArtifact?.warnings ?? [];
  const partial = warnings.some(
    (w) =>
      w.includes("this document is incomplete") ||
      w.includes("Missing planned sections:"),
  );
  useEffect(() => {
    const controller = new AbortController();
    setDoc(null);
    if (selected)
      api(`/artifacts/${selected}`, { signal: controller.signal })
        .then((value) => {
          setDoc(value);
        })
        .catch((e) => {
          if (e.name !== "AbortError") onError(e.message);
        });
    return () => controller.abort();
  }, [selected]);
  useEffect(() => {
    const controller = new AbortController();
    if (compare)
      api(`/artifacts/${compare}`, { signal: controller.signal })
        .then((d) => setOld(d.markdown))
        .catch((e) => {
          if (e.name !== "AbortError") onError(e.message);
        });
    else setOld("");
    return () => controller.abort();
  }, [compare]);
  return (
    <>
      <div className="page-heading">
        <div>
          <div className="eyebrow">KNOWLEDGE LIBRARY</div>
          <h1>문서 라이브러리</h1>
          <p>생성된 문서의 검토 상태와 실행별 변경 내용을 확인합니다.</p>
        </div>
        {doc && (
          <button
            className="secondary"
            onClick={() => {
              const url = URL.createObjectURL(
                new Blob([doc.markdown], {
                  type: "text/markdown;charset=utf-8",
                }),
              );
              const a = document.createElement("a");
              a.href = url;
              a.download = doc.path.split(/[\\/]/).pop();
              a.click();
              URL.revokeObjectURL(url);
            }}
          >
            <Download size={16} />
            Markdown 다운로드
          </button>
        )}
      </div>
      <div className="document-toolbar">
        <select
          aria-label="문서 선택"
          value={selected}
          onChange={(e) => {
            setSelected(e.target.value);
            setCompare("");
          }}
        >
          <option value="">문서를 선택하세요</option>
          {artifacts.map((a) => (
            <option key={a.id} value={a.id}>
              {a.path.split(/[\\/]/).pop()} · {localTime(a.created_at)} ·{" "}
              {a.warnings.some((w) => w.includes("this document is incomplete"))
                ? "부분 생성"
                : a.warnings.length
                  ? "검토 사항 있음"
                  : "검토 완료"}
            </option>
          ))}
        </select>
        <select
          aria-label="비교 버전"
          value={compare}
          onChange={(e) => setCompare(e.target.value)}
        >
          <option value="">버전 비교 안 함</option>
          {artifacts
            .filter(
              (a) =>
                a.id !== selected &&
                a.task_id === artifacts.find((x) => x.id === selected)?.task_id,
            )
            .map((a) => (
              <option key={a.id} value={a.id}>
                {localTime(a.created_at)}
              </option>
            ))}
        </select>
      </div>
      {selectedArtifact && warnings.length > 0 && (
        <aside className="document-warning" role="status">
          <strong>
            {partial ? "부분 생성 · 전체 검토 미완료" : "검토 사항 있음"}
          </strong>
          <p>
            {partial
              ? "최종 검토된 문서가 아닙니다. 누락된 주제와 검토 상태를 확인하세요."
              : "해결되지 않은 사항 또는 분석 범위 제한이 있습니다."}
          </p>
          <details>
            <summary>검토 상세 ({warnings.length}건)</summary>
            <ul>
              {warnings.map((warning, i) => (
                <li key={i}>{warning}</li>
              ))}
            </ul>
          </details>
        </aside>
      )}
      {doc ? (
        <article className="panel markdown">
          {compare ? (
            <pre className="diff">
              {diffLines(old, doc.markdown).map((part, i) => (
                <span
                  key={i}
                  className={
                    part.added ? "added" : part.removed ? "removed" : ""
                  }
                >
                  {part.value}
                </span>
              ))}
            </pre>
          ) : (
            <Markdown text={doc.markdown} />
          )}
        </article>
      ) : (
        <div className="empty-state">
          <BookOpen size={36} />
          <h3>문서가 쌓이는 공간</h3>
          <p>작업을 실행하면 생성된 문서가 버전별로 보관됩니다.</p>
        </div>
      )}
    </>
  );
}

export { Documents };
