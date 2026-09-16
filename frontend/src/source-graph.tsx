import { useEffect, useState } from "react";
import { api } from "./api";

type FileList = {
  files: { id: number; path: string }[];
  next_cursor: number;
  has_more: boolean;
};
type Graph = {
  path: string;
  supported: boolean;
  parse_errors: boolean;
  symbols: {
    id: string;
    qualified_name: string;
    kind: string;
    span: { start: number; end: number };
  }[];
  edges: {
    source_name: string;
    edge: { kind: string; target: string; span: { start: number } };
  }[];
  next_cursor: number;
  has_more: boolean;
};

export function SourceGraph({
  runId,
  graph,
}: {
  runId: string;
  graph: {
    symbols: number;
    edges: number;
    unsupported_files: number;
    parse_error_files: number;
  };
}) {
  const [files, setFiles] = useState<FileList | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const [detail, setDetail] = useState<Graph | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    setFiles(null);
    setDetail(null);
    setSelected(null);
    setError("");
  }, [runId]);
  async function load(
    kind: "files" | "detail",
    next = false,
    fileId = selected,
  ) {
    setBusy(true);
    setError("");
    try {
      if (kind === "files") {
        const value: FileList = await api(
          `/runs/${runId}/graph?after=${next ? (files?.next_cursor ?? 0) : 0}`,
        );
        setFiles((previous) => ({
          ...value,
          files: next
            ? [...(previous?.files ?? []), ...value.files]
            : value.files,
        }));
      } else if (kind === "detail" && fileId !== null) {
        const value: Graph = await api(
          `/runs/${runId}/graph?file_id=${fileId}&after=${next ? (detail?.next_cursor ?? 0) : 0}`,
        );
        setSelected(fileId);
        setDetail((previous) => ({
          ...value,
          symbols: next
            ? [...(previous?.symbols ?? []), ...value.symbols]
            : value.symbols,
          edges: next
            ? [...(previous?.edges ?? []), ...value.edges]
            : value.edges,
        }));
      }
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  }
  return (
    <details>
      <summary>
        코드 그래프 · 심볼 {graph.symbols.toLocaleString()}개 / 관계·분기{" "}
        {graph.edges.toLocaleString()}개
      </summary>
      <p>
        클래스와 함수의 소유 관계, 호출 후보, 분기 및 반환 위치를 원본과 함께
        보존합니다. 호출 후보는 실행이 확인된 연결을 뜻하지 않습니다.
      </p>
      {(graph.unsupported_files > 0 || graph.parse_error_files > 0) && (
        <p className="banner">
          구조 분석 미지원 {graph.unsupported_files}개 · 구문 오류{" "}
          {graph.parse_error_files}개 파일. 해당 원본은 구조 없이 보존합니다.
        </p>
      )}
      <button disabled={busy} onClick={() => void load("files")}>
        파일별 그래프 보기
      </button>{" "}
      {error && <p role="alert">{error}</p>}
      {files && (
        <ul>
          {files.files.map((file) => (
            <li key={file.id}>
              <button
                disabled={busy}
                onClick={() => void load("detail", false, file.id)}
              >
                {file.path}
              </button>
            </li>
          ))}
        </ul>
      )}
      {files?.has_more && (
        <button disabled={busy} onClick={() => void load("files", true)}>
          파일 더 보기
        </button>
      )}
      {detail && (
        <section aria-label="선택한 파일의 코드 그래프">
          <h4>{detail.path}</h4>
          {!detail.supported && (
            <p>
              이 언어는 구조 분석을 지원하지 않습니다. 원본 대조 기록을
              확인하세요.
            </p>
          )}
          <ul>
            {detail.symbols.map((s) => (
              <li key={s.id}>
                <code>{s.qualified_name}</code> · {s.kind} · {s.span.start}–
                {s.span.end}행
              </li>
            ))}
          </ul>
          <ul>
            {detail.edges.map((e, i) => (
              <li key={i}>
                <code>{e.source_name}</code> → {e.edge.kind}
                {e.edge.target && (
                  <>
                    {" "}
                    · <code>{e.edge.target}</code>
                  </>
                )}{" "}
                · {e.edge.span.start}행
              </li>
            ))}
          </ul>
          {detail.has_more && (
            <button disabled={busy} onClick={() => void load("detail", true)}>
              관계 더 보기
            </button>
          )}
        </section>
      )}
    </details>
  );
}
