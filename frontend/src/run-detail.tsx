/**
 * One run as it happens: the stages it reports, the files it indexed, the
 * source understanding and outline it settled on, and the document it produced.
 */
import React, { useState, useEffect, useRef } from "react";
import { Square, RefreshCw } from "lucide-react";

import { api, send, type Run } from "./api";
import { Composition } from "./composition";
import { Markdown } from "./markdown";
import {
  Badge,
  busy,
  fmt,
  sourceProgress,
  FILE_PAGE_SIZE,
  EVENT_LIMIT,
  type RunEvent,
  type RunFile,
  type RunFilesResponse,
} from "./shared";

function RunDetail({
  run,
  onCancel,
  onResume,
}: {
  run?: Run;
  onCancel: (id: string) => void;
  onResume: (
    id: string,
    currentLlm: boolean,
    currentTokenLimit: boolean,
    currentReviewLimit: boolean,
  ) => void;
}) {
  const [currentLlm, setCurrentLlm] = useState(false);
  const [currentTokenLimit, setCurrentTokenLimit] = useState(false);
  const [currentReviewLimit, setCurrentReviewLimit] = useState(false);
  const [events, setEvents] = useState<RunEvent[]>([]),
    [eventError, setEventError] = useState(""),
    [files, setFiles] = useState<RunFile[]>([]),
    [filesLoading, setFilesLoading] = useState(false),
    [filesHasMore, setFilesHasMore] = useState(false),
    [filesError, setFilesError] = useState(""),
    [filesReload, setFilesReload] = useState(0),
    [tab, setTab] = useState("events");
  const fileRequest = useRef(0);
  const fileAbort = useRef<AbortController | null>(null);
  const streamActive = run ? busy(run.status) : false;
  useEffect(() => {
    setEvents([]);
    setEventError("");
  }, [run?.id]);
  useEffect(() => {
    if (!run || streamActive) return;
    const controller = new AbortController();
    api<{ events?: RunEvent[] }>(`/runs/${run.id}/history`, {
      signal: controller.signal,
    })
      .then((value) => {
        if (Array.isArray(value.events)) setEvents(value.events);
      })
      .catch((error) => {
        if (error.name !== "AbortError")
          setEventError("이벤트 기록을 불러오지 못했습니다.");
      });
    return () => controller.abort();
  }, [run?.id, run?.updated_at, streamActive]);
  useEffect(() => {
    if (!run || !streamActive) return;
    const es = new EventSource(`/api/v1/runs/${run.id}/events`);
    let active = true;
    let ended = false;
    es.onopen = () => {
      if (active) setEventError("");
    };
    es.onerror = () => {
      if (active && !ended)
        setEventError(
          "이벤트 스트림 연결이 끊겼습니다. 자동으로 다시 연결합니다.",
        );
    };
    es.addEventListener("connection", () => {
      if (active)
        setEventError(
          "이벤트 저장소에 일시적으로 연결할 수 없습니다. 자동으로 다시 시도합니다.",
        );
    });
    es.addEventListener("progress", (e) => {
      if (!active) return;
      try {
        const value = JSON.parse((e as MessageEvent).data) as RunEvent;
        if (
          !Number.isSafeInteger(value?.id) ||
          typeof value?.kind !== "string" ||
          !("data" in value)
        )
          throw new Error("Invalid progress event");
        setEvents((prev) =>
          prev.some((v) => v.id === value.id)
            ? prev
            : [...prev, value].slice(-EVENT_LIMIT),
        );
      } catch {
        setEventError(
          "형식이 잘못된 진행 이벤트를 건너뛰었습니다. 나머지 이벤트는 계속 표시합니다.",
        );
      }
    });
    es.addEventListener("stream-end", () => {
      ended = true;
      es.close();
      if (active) setEventError("");
    });
    return () => {
      active = false;
      es.close();
    };
  }, [run?.id, streamActive]);
  useEffect(() => {
    const request = ++fileRequest.current;
    fileAbort.current?.abort();
    setFiles([]);
    setFilesHasMore(false);
    setFilesError("");
    setFilesLoading(false);
    if (!run || tab !== "files") return;
    const controller = new AbortController();
    fileAbort.current = controller;
    setFilesLoading(true);
    api<RunFilesResponse>(`/runs/${run.id}/files`, {
      signal: controller.signal,
    })
      .then((response) => {
        if (fileRequest.current !== request) return;
        setFiles(response.files);
        setFilesHasMore(
          response.has_more ?? response.files.length === FILE_PAGE_SIZE,
        );
      })
      .catch((error: Error) => {
        if (fileRequest.current !== request || error.name === "AbortError")
          return;
        setFilesError(`파일 분석 내역을 불러오지 못했습니다. ${error.message}`);
      })
      .finally(() => {
        if (fileRequest.current === request) setFilesLoading(false);
      });
    return () => controller.abort();
  }, [run?.id, tab, filesReload]);

  async function loadMoreFiles() {
    const after = files.at(-1)?.id;
    if (!run || !after || filesLoading || !filesHasMore) return;
    const request = ++fileRequest.current;
    fileAbort.current?.abort();
    const controller = new AbortController();
    fileAbort.current = controller;
    setFilesLoading(true);
    setFilesError("");
    try {
      const response = await api<RunFilesResponse>(
        `/runs/${run.id}/files?after=${after}`,
        { signal: controller.signal },
      );
      if (fileRequest.current !== request) return;
      setFiles((current) => {
        const existing = new Set(current.map((file) => file.id));
        return [
          ...current,
          ...response.files.filter((file) => !existing.has(file.id)),
        ];
      });
      setFilesHasMore(
        response.has_more ?? response.files.length === FILE_PAGE_SIZE,
      );
    } catch (error) {
      if (
        fileRequest.current === request &&
        (error as Error).name !== "AbortError"
      )
        setFilesError(
          `파일 분석 내역을 더 불러오지 못했습니다. ${(error as Error).message}`,
        );
    } finally {
      if (fileRequest.current === request) setFilesLoading(false);
    }
  }
  if (!run)
    return (
      <div className="panel empty">
        실행을 선택하면 상세 진행 상황이 표시됩니다.
      </div>
    );
  const progress = run.progress;
  const exactProgress = sourceProgress(events);
  const progressPercent = exactProgress
    ? Math.min(100, (exactProgress.current / exactProgress.total) * 100)
    : null;
  return (
    <section className="panel run-detail">
      <div className="detail-heading">
        <div>
          <Badge status={run.status} />
          <h2>{progress.title ?? "실행 상세"}</h2>
          <code>{run.id.slice(0, 8)}</code>
        </div>
        {busy(run.status) ? (
          <button
            className="danger"
            disabled={run.status === "cancelling"}
            onClick={() => onCancel(run.id)}
          >
            <Square size={14} />
            즉시 중단
          </button>
        ) : (
          ([
            "failed",
            "cancelled",
            "interrupted",
            "awaiting_source",
            "awaiting_outline",
          ].includes(run.status) ||
            run.status === "completed_with_warnings") && (
            <div>
              <label className="checkbox">
                <input
                  type="checkbox"
                  checked={currentLlm}
                  onChange={(e) => setCurrentLlm(e.target.checked)}
                />
                현재 LLM 설정 적용
              </label>
              <label className="checkbox">
                <input
                  type="checkbox"
                  checked={currentTokenLimit}
                  onChange={(e) => setCurrentTokenLimit(e.target.checked)}
                />
                현재 작업의 토큰 한도 적용
              </label>
              <label className="checkbox">
                <input
                  type="checkbox"
                  checked={currentReviewLimit}
                  onChange={(e) => setCurrentReviewLimit(e.target.checked)}
                />
                현재 작업의 검토 회차 적용
              </label>
              <button
                className="secondary"
                onClick={() =>
                  onResume(
                    run.id,
                    currentLlm,
                    currentTokenLimit,
                    currentReviewLimit,
                  )
                }
              >
                <RefreshCw size={14} />
                체크포인트 재개
              </button>
              <small className="resume-note">
                기존 소스와 완료 섹션을 유지합니다. 검토 완료 경고를 이어서
                수정하려면 작업의 검토 회차를 늘린 뒤 현재 검토 회차 적용을
                선택하세요. 시간·비용 한도는 유지합니다.
              </small>
            </div>
          )
        )}
      </div>
      <div className="mini-stats">
        <div>
          <small>현재 단계</small>
          <strong>{progress.stage ?? "대기"}</strong>
        </div>
        <div>
          <small>누적 토큰</small>
          <strong>{fmt(run.tokens)}</strong>
        </div>
        <div>
          <small>비용 (가격 설정 시)</small>
          <strong>${run.cost.toFixed(4)}</strong>
        </div>
      </div>
      {exactProgress && progressPercent !== null && (
        <div className="source-progress">
          <div>
            <strong>{exactProgress.label}</strong>
            <span>{progressPercent.toFixed(1)}%</span>
          </div>
          <div
            className="progress-track"
            role="progressbar"
            aria-label={exactProgress.label}
            aria-valuemin={0}
            aria-valuemax={exactProgress.total}
            aria-valuenow={exactProgress.current}
          >
            <span style={{ width: `${progressPercent}%` }} />
          </div>
          <small>
            {fmt(exactProgress.current)} / {fmt(exactProgress.total)}{" "}
            {exactProgress.unit}
          </small>
        </div>
      )}
      {run.error && <div className="banner error">{run.error}</div>}
      <Composition run={run} />
      <div className="tabs">
        <button
          className={tab === "events" ? "active" : ""}
          onClick={() => setTab("events")}
        >
          실시간 이벤트
        </button>
        <button
          className={tab === "files" ? "active" : ""}
          onClick={() => setTab("files")}
        >
          파일 분석 내역
        </button>
      </div>
      {tab === "events" ? (
        <div className="event-list">
          {eventError && (
            <div role="alert" className="inline-error">
              {eventError}
            </div>
          )}
          {!events.length && (
            <p className="muted">진행 이벤트를 기다리는 중…</p>
          )}
          {events.length === EVENT_LIMIT && (
            <p className="list-note">
              최근 {EVENT_LIMIT}개 이벤트를 표시합니다.
            </p>
          )}
          {[...events].reverse().map((e) => (
            <div className="event" key={e.id}>
              <span className="event-dot" />
              <div>
                <div>
                  <b>{e.kind}</b>
                  <small>#{e.id}</small>
                </div>
                <pre>{JSON.stringify(e.data, null, 2)}</pre>
              </div>
            </div>
          ))}
        </div>
      ) : (
        <div className="file-list">
          {filesError && (
            <div role="alert" className="inline-error">
              <span>{filesError}</span>
              <button
                className="secondary"
                onClick={() => setFilesReload((value) => value + 1)}
              >
                다시 시도
              </button>
            </div>
          )}
          {!files.length && filesLoading && (
            <p className="muted">파일 분석 내역을 불러오는 중…</p>
          )}
          {!files.length && !filesLoading && !filesError && (
            <p className="muted">기록된 파일 분석 내역이 없습니다.</p>
          )}
          {files.map((f) => (
            <div className="file-row" key={f.id}>
              <Badge status={f.status} />
              <code>{f.path}</code>
              <small>{f.detail}</small>
            </div>
          ))}
          {files.length > 0 && (
            <div className="list-footer">
              <small>{fmt(files.length)}개 파일</small>
              {filesHasMore && (
                <button
                  className="secondary"
                  disabled={filesLoading}
                  onClick={loadMoreFiles}
                >
                  {filesLoading ? "불러오는 중…" : "더 보기"}
                </button>
              )}
            </div>
          )}
        </div>
      )}
    </section>
  );
}

export { RunDetail };
