/**
 * Vocabulary shared by every screen: run status labels, the shapes the API
 * returns, and the presentational pieces used on more than one page.
 *
 * A screen imports from here rather than from another screen, so the pages stay
 * independent of one another.
 */
import React from "react";

type Page = "tasks" | "runs" | "documents" | "settings";
type RunEvent = { id: number; kind: string; data: unknown };
type RunFile = {
  id: number;
  path: string;
  language: string;
  status: string;
  detail: string;
};
type RunFilesResponse = { files: RunFile[]; has_more?: boolean };
const FILE_PAGE_SIZE = 200;
const EVENT_LIMIT = 500;
const busy = (s: string) => ["queued", "running", "cancelling"].includes(s);
const labels: Record<string, string> = {
  queued: "대기 중",
  running: "실행 중",
  cancelling: "중단 중",
  cancelled: "중단됨",
  failed: "실패",
  completed: "완료",
  completed_with_warnings: "검토 사항 있음",
  interrupted: "복구 대기",
  awaiting_source: "소스 분석 대기",
  awaiting_outline: "목차 확인 필요",
  indexed: "색인 완료",
  excluded: "분석 제외",
  skipped: "분석 실패",
};
const fmt = (n: number) => new Intl.NumberFormat("ko-KR").format(n);
type ExactProgress = {
  label: string;
  current: number;
  total: number;
  unit: string;
};
const progressValue = (value: unknown) =>
  typeof value === "number" && Number.isSafeInteger(value) && value >= 0
    ? value
    : null;
function sourceProgress(events: RunEvent[]): ExactProgress | null {
  for (let i = events.length - 1; i >= 0; i--) {
    const event = events[i];
    if (!event.data || typeof event.data !== "object") continue;
    const data = event.data as Record<string, unknown>;
    if (event.kind === "source_connections") {
      const current = progressValue(data.completed_summaries);
      const total = progressValue(data.total_summaries);
      if (current !== null && total !== null && total > 0)
        return { label: "모듈 흐름 종합", current, total, unit: "요약" };
    }
    if (event.kind === "source_batch" || event.kind === "source_progress") {
      const current = progressValue(data.read_chunks);
      const total = progressValue(data.total_chunks);
      if (current !== null && total !== null && total > 0)
        return { label: "전체 소스 읽기", current, total, unit: "청크" };
    }
  }
  return null;
}
const localTime = (value: string) => {
  const normalized = /(?:Z|[+-]\d{2}:\d{2})$/i.test(value.trim())
    ? value.trim()
    : `${value.trim().replace(" ", "T")}Z`;
  const date = new Date(normalized);
  if (Number.isNaN(date.getTime())) return value;
  return new Intl.DateTimeFormat("ko-KR", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hourCycle: "h23",
  }).format(date);
};
function Badge({ status }: { status: string }) {
  return (
    <span className={"badge " + status}>
      <i />
      {labels[status] ?? status}
    </span>
  );
}

function Stat({
  label,
  value,
  sub,
  icon,
}: {
  label: string;
  value: string | number;
  sub: string;
  icon: React.ReactNode;
}) {
  return (
    <div className="stat">
      <div>
        <span>{label}</span>
        {icon}
      </div>
      <strong>{value}</strong>
      <small>{sub}</small>
    </div>
  );
}
function Field({
  label,
  help,
  children,
}: {
  label: string;
  help?: string;
  children: React.ReactNode;
}) {
  const id = React.useId();
  return (
    <label className="field" htmlFor={id}>
      <span id={id + "-label"}>{label}</span>
      {React.isValidElement(children)
        ? React.cloneElement(children as React.ReactElement<any>, {
            id,
            "aria-labelledby": id + "-label",
            "aria-describedby": help ? id + "-help" : undefined,
          })
        : children}
      {help && <small id={id + "-help"}>{help}</small>}
    </label>
  );
}

export {
  Badge,
  Stat,
  Field,
  busy,
  labels,
  fmt,
  localTime,
  sourceProgress,
  progressValue,
  FILE_PAGE_SIZE,
  EVENT_LIMIT,
};
export type { Page, RunEvent, RunFile, RunFilesResponse, ExactProgress };
