import React, { useState, useEffect, useCallback, useRef } from "react";
import { createRoot } from "react-dom/client";
import {
  BookOpen,
  Layers,
  Activity,
  Settings2,
  Plus,
  Play,
  Square,
  ChevronRight,
  CheckCircle2,
  AlertCircle,
  FileCode2,
  Files,
  RefreshCw,
  Copy,
  Trash2,
  Download,
  ArrowUpRight,
  Search,
  Terminal,
  Database,
  PanelLeftClose,
} from "lucide-react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { diffLines } from "diff";
import { api, send, newTask, type Task, type Run, type Artifact } from "./api";
import "./styles.css";

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
};
const fmt = (n: number) => new Intl.NumberFormat("ko-KR").format(n);
function Badge({ status }: { status: string }) {
  return (
    <span className={"badge " + status}>
      <i />
      {labels[status] ?? status}
    </span>
  );
}
function App() {
  const [page, setPage] = useState<Page>("tasks"),
    [tasks, setTasks] = useState<Task[]>([]),
    [runs, setRuns] = useState<Run[]>([]),
    [artifacts, setArtifacts] = useState<Artifact[]>([]);
  const [settings, setSettings] = useState<any>(null),
    [diagnostics, setDiagnostics] = useState<any>(null),
    [error, setError] = useState(""),
    [pollError, setPollError] = useState(""),
    [notice, setNotice] = useState(""),
    [edit, setEdit] = useState<Task | null>(null),
    [selected, setSelected] = useState<string | null>(null),
    [ready, setReady] = useState(false),
    [search, setSearch] = useState("");
  const refresh = useCallback(async (signal?: AbortSignal) => {
    const options = signal ? { signal } : undefined;
    const results = await Promise.allSettled([
      api<Task[]>("/tasks", options),
      api<Run[]>("/runs", options),
      api<{ artifacts: Artifact[] }>("/artifacts", options),
      api("/diagnostics", options),
    ]);
    if (signal?.aborted) return;
    if (results[0].status === "fulfilled") setTasks(results[0].value);
    if (results[1].status === "fulfilled") setRuns(results[1].value);
    if (results[2].status === "fulfilled")
      setArtifacts(results[2].value.artifacts);
    if (results[3].status === "fulfilled") setDiagnostics(results[3].value);
    setPollError(
      results.some((r) => r.status === "rejected")
        ? "서버 상태를 갱신하지 못했습니다. 표시된 실행 상태는 마지막 확인값입니다. 연결이 복구되면 자동으로 갱신합니다."
        : "",
    );
  }, []);
  useEffect(() => {
    const controller = new AbortController();
    api("/session", { signal: controller.signal })
      .then(() => api("/settings", { signal: controller.signal }))
      .then(async (s) => {
        setSettings(s);
        await refresh(controller.signal);
        setReady(true);
      })
      .catch((e) => {
        if (e.name !== "AbortError") setError(e.message);
      });
    return () => controller.abort();
  }, [refresh]);
  useEffect(() => {
    if (!ready) return;
    let stopped = false;
    let timer: number | undefined;
    let controller: AbortController | undefined;
    const poll = async () => {
      controller = new AbortController();
      await refresh(controller.signal);
      if (!stopped) timer = window.setTimeout(poll, 2500);
    };
    timer = window.setTimeout(poll, 2500);
    return () => {
      stopped = true;
      if (timer !== undefined) window.clearTimeout(timer);
      controller?.abort();
    };
  }, [ready, refresh]);
  const action = async (fn: () => Promise<void>) => {
    setError("");
    try {
      await fn();
      await refresh();
    } catch (e) {
      setError((e as Error).message);
    }
  };
  const runTask = (id: string) =>
    action(async () => {
      const run = await api<{ id: string }>(`/tasks/${id}/run`, send("POST"));
      setSelected(run.id);
      setPage("runs");
    });
  const totalTokens = runs.reduce((sum, r) => sum + r.tokens, 0),
    active = runs.filter((r) => busy(r.status)).length;
  return (
    <div className="app">
      <aside className="sidebar">
        <a
          className="brand"
          href="#"
          onClick={(e) => {
            e.preventDefault();
            setPage("tasks");
          }}
        >
          <span className="brand-icon">
            <BookOpen size={23} />
          </span>
          <span>
            DocCraft<small>AGENT WORKSPACE</small>
          </span>
        </a>
        <div className="workspace-label">PERSONAL WORKSPACE</div>
        <nav>
          {(
            [
              ["tasks", Layers, "문서화 작업"],
              ["runs", Activity, "실행 모니터"],
              ["documents", Files, "문서 라이브러리"],
              ["settings", Settings2, "설정"],
            ] as const
          ).map(([key, Icon, title]) => (
            <button
              key={key}
              className={page === key ? "nav-item active" : "nav-item"}
              onClick={() => setPage(key)}
            >
              <Icon size={19} />
              {title}
              {key === "runs" && active > 0 && <b>{active}</b>}
            </button>
          ))}
        </nav>
        <div className="sidebar-note">
          <div className="eyebrow">SOURCE → CLARITY</div>
          <p>
            코드의 맥락을 읽고,
            <br />
            신뢰할 수 있는 문서로.
          </p>
          <div className="local-dot">
            <i />
            로컬 워크스페이스
          </div>
        </div>
        <div className="sidebar-bottom">
          <span className="avatar">D</span>
          <div>
            Personal<small>Local · v0.1.0</small>
          </div>
          <PanelLeftClose size={16} />
        </div>
      </aside>
      <main>
        <header className="topbar">
          <span className="breadcrumb">
            <span className="breadcrumb-root">워크스페이스</span>
            <ChevronRight className="breadcrumb-separator" size={14} />
            <strong className="breadcrumb-page">
              {
                {
                  tasks: "문서화 작업",
                  runs: "실행 모니터",
                  documents: "문서 라이브러리",
                  settings: "설정",
                }[page]
              }
            </strong>
          </span>
          <div
            className={
              "connection " +
              (!pollError && diagnostics?.database ? "online" : "")
            }
          >
            <i />
            {pollError
              ? "서버 연결 확인 필요"
              : diagnostics?.database
                ? "MariaDB 연결됨"
                : "DB 연결 확인 필요"}
          </div>
        </header>
        <div className="content">
          {pollError && (
            <div role="alert" className="banner error">
              <AlertCircle size={18} />
              <span>{pollError}</span>
            </div>
          )}
          {error && (
            <div role="alert" className="banner error">
              <AlertCircle size={18} />
              <span>{error}</span>
              <button onClick={() => setError("")}>닫기</button>
            </div>
          )}
          {notice && (
            <div role="status" className="banner success">
              <CheckCircle2 size={18} />
              <span>{notice}</span>
              <button onClick={() => setNotice("")}>닫기</button>
            </div>
          )}
          {!ready ? (
            <div className="empty">
              <RefreshCw />
              서버에 연결하는 중…
            </div>
          ) : (
            <>
              {page === "tasks" && (
                <>
                  <div className="page-heading">
                    <div>
                      <div className="eyebrow">DOCUMENTATION PIPELINE</div>
                      <h1>코드에서 문서까지</h1>
                      <p>
                        소스와 목적을 연결하면, 에이전트가 분석부터 검토까지
                        이어갑니다.
                      </p>
                    </div>
                    <button
                      className="primary"
                      onClick={() => setEdit(newTask())}
                    >
                      <Plus size={17} />새 작업
                    </button>
                  </div>
                  <div className="stats">
                    <Stat
                      label="문서화 작업"
                      value={tasks.length}
                      sub="등록된 생성 목적"
                      icon={<Layers />}
                    />
                    <Stat
                      label="진행 중"
                      value={active}
                      sub="자동 분석 및 검토"
                      icon={<Activity />}
                    />
                    <Stat
                      label="생성된 문서"
                      value={artifacts.length}
                      sub="버전 이력 포함"
                      icon={<BookOpen />}
                    />
                    <Stat
                      label="누적 토큰"
                      value={fmt(totalTokens)}
                      sub="최근 200개 실행 기준"
                      icon={<Terminal />}
                    />
                  </div>
                  {(!diagnostics?.database ||
                    !settings.llm.model ||
                    !settings.source_roots.length) && (
                    <div className="setup-card">
                      <span className="setup-icon">
                        <Settings2 />
                      </span>
                      <div>
                        <h3>첫 실행을 준비해주세요</h3>
                        <p>
                          설정에서 LLM 연결과 허용 소스·출력 경로를 지정하세요.
                        </p>
                      </div>
                      <button
                        className="secondary"
                        onClick={() => setPage("settings")}
                      >
                        설정 열기 <ArrowUpRight size={15} />
                      </button>
                    </div>
                  )}
                  <div className="section-toolbar">
                    <h2>
                      내 작업 <span>{tasks.length}</span>
                    </h2>
                    <div className="toolbar-right">
                      <div className="search">
                        <Search size={16} />
                        <input
                          aria-label="작업 검색"
                          placeholder="작업 검색"
                          value={search}
                          onChange={(e) => setSearch(e.target.value)}
                        />
                      </div>
                      <button
                        className="secondary"
                        disabled={!tasks.length}
                        onClick={() =>
                          action(async () => {
                            const result = await api(
                              "/batch",
                              send("POST", { ids: tasks.map((t) => t.id) }),
                            );
                            const failures = result.results.filter(
                              (r: any) => r.error,
                            );
                            if (failures.length)
                              setError(
                                failures.map((r: any) => r.error).join(" · "),
                              );
                            setPage("runs");
                          })
                        }
                      >
                        <Play size={14} />
                        전체 실행
                      </button>
                    </div>
                  </div>
                  {!tasks.length ? (
                    <div className="empty-state">
                      <div className="empty-illustration">
                        <FileCode2 size={30} />
                        <span>→</span>
                        <BookOpen size={30} />
                      </div>
                      <h3>첫 번째 문서화 작업을 만들어보세요</h3>
                      <p>
                        하나의 소스에서도 목적에 따라 다른 문서를 만들 수
                        있습니다.
                        <br />
                        VOC 참고 자료부터 개발자를 위한 흐름도까지.
                      </p>
                      <button
                        className="primary"
                        onClick={() => setEdit(newTask())}
                      >
                        <Plus size={16} />
                        작업 만들기
                      </button>
                      <div className="example-tags">
                        <span>주제별 기능 정리</span>
                        <span>개발 흐름 · Mermaid</span>
                        <span>근거 기반 문서</span>
                      </div>
                    </div>
                  ) : (
                    <div className="task-grid">
                      {tasks
                        .filter((t) =>
                          t.name.toLowerCase().includes(search.toLowerCase()),
                        )
                        .map((task) => {
                          const last = runs.find((r) => r.task_id === task.id);
                          return (
                            <article className="task-card" key={task.id}>
                              <div className="card-top">
                                <span className="file-icon">
                                  <FileCode2 size={21} />
                                </span>
                                {last ? (
                                  <Badge status={last.status} />
                                ) : (
                                  <span className="badge">실행 전</span>
                                )}
                              </div>
                              <h3>{task.name}</h3>
                              <p className="direction">{task.direction}</p>
                              <div className="task-meta">
                                <span>
                                  <Layers size={13} />
                                  {task.sources.length}개 소스
                                </span>
                                <span>최대 {task.max_iterations}회 검토</span>
                              </div>
                              <div className="target-path" title={task.target}>
                                <BookOpen size={14} />
                                {task.target.split(/[\\/]/).pop()}
                              </div>
                              <div className="card-actions">
                                <button
                                  className="text-button"
                                  onClick={() => setEdit(task)}
                                >
                                  편집
                                </button>
                                <button
                                  aria-label={`${task.name} 복제`}
                                  onClick={() =>
                                    setEdit({
                                      ...task,
                                      id: "",
                                      name: task.name + " 복사",
                                      target: task.target.replace(
                                        /\.md$/,
                                        "-copy.md",
                                      ),
                                    })
                                  }
                                >
                                  <Copy size={15} />
                                </button>
                                <button
                                  aria-label={`${task.name} 삭제`}
                                  onClick={() =>
                                    action(async () => {
                                      await api(
                                        `/tasks/${task.id}`,
                                        send("DELETE"),
                                      );
                                    })
                                  }
                                >
                                  <Trash2 size={15} />
                                </button>
                                <button
                                  className="run-button"
                                  disabled={!!last && busy(last.status)}
                                  onClick={() => runTask(task.id)}
                                >
                                  <Play size={14} />
                                  실행
                                </button>
                              </div>
                            </article>
                          );
                        })}
                    </div>
                  )}
                </>
              )}
              {page === "runs" && (
                <>
                  <div className="page-heading">
                    <div>
                      <div className="eyebrow">LIVE OBSERVABILITY</div>
                      <h1>실행 모니터</h1>
                      <p>분석 근거, 반복 검토와 토큰 사용량을 확인합니다.</p>
                    </div>
                    <button className="secondary" onClick={() => refresh()}>
                      <RefreshCw size={16} />
                      새로고침
                    </button>
                  </div>
                  <div className="monitor-layout">
                    <div className="run-list">
                      {runs.length === 0 ? (
                        <div className="empty">아직 실행 이력이 없습니다.</div>
                      ) : (
                        runs.map((r) => (
                          <button
                            key={r.id}
                            className={
                              "run-row " +
                              ((selected ?? runs[0]?.id) === r.id
                                ? "selected"
                                : "")
                            }
                            onClick={() => setSelected(r.id)}
                          >
                            <div>
                              <strong>
                                {tasks.find((t) => t.id === r.task_id)?.name ??
                                  "보관된 작업"}
                              </strong>
                              <Badge status={r.status} />
                            </div>
                            <small>{r.created_at}</small>
                            <span>
                              {fmt(r.tokens)} tokens <ChevronRight size={14} />
                            </span>
                          </button>
                        ))
                      )}
                    </div>
                    <RunDetail
                      run={runs.find((r) => r.id === (selected ?? runs[0]?.id))}
                      onCancel={(id) =>
                        action(async () => {
                          const response = await api(
                            `/runs/${id}/cancel`,
                            send("POST"),
                          );
                          setNotice(
                            response.accepted
                              ? "중단 요청을 접수했습니다."
                              : "실행이 이미 종료되었거나 결과 저장이 완료되었습니다.",
                          );
                        })
                      }
                      onResume={(id, currentLlm, currentTokenLimit) =>
                        action(async () => {
                          await api(
                            `/runs/${id}/resume?current_llm=${currentLlm}&current_token_limit=${currentTokenLimit}`,
                            send("POST"),
                          );
                        })
                      }
                    />
                  </div>
                </>
              )}
              {page === "documents" && (
                <Documents artifacts={artifacts} onError={setError} />
              )}
              {page === "settings" && (
                <SettingsPage
                  value={settings}
                  diagnostics={diagnostics}
                  onSave={(v) =>
                    action(async () => {
                      const saved = await api("/settings", send("PUT", v));
                      setSettings(saved);
                      setNotice("설정을 저장했습니다.");
                    })
                  }
                  onError={setError}
                  onNotice={setNotice}
                />
              )}
            </>
          )}
        </div>
        <footer>
          DocCraft Agent <span>소스 근거 · 반복 검토 · 로컬 실행</span>
        </footer>
      </main>
      {edit && (
        <TaskEditor
          task={edit}
          onClose={() => setEdit(null)}
          onSave={(task) =>
            action(async () => {
              await api("/tasks", send("POST", task));
              setEdit(null);
              setNotice("문서화 작업을 저장했습니다.");
            })
          }
        />
      )}
    </div>
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
function TaskEditor({
  task,
  onClose,
  onSave,
}: {
  task: Task;
  onClose: () => void;
  onSave: (t: Task) => void;
}) {
  const [v, set] = useState(task);
  const change = (k: keyof Task, x: any) => set({ ...v, [k]: x });
  return (
    <div className="modal-backdrop">
      <form
        className="modal"
        onSubmit={(e) => {
          e.preventDefault();
          onSave(v);
        }}
      >
        <div className="modal-head">
          <div>
            <div className="eyebrow">DOCUMENTATION JOB</div>
            <h2>{task.id ? "작업 편집" : "새 문서화 작업"}</h2>
          </div>
          <button type="button" onClick={onClose}>
            ✕
          </button>
        </div>
        <div className="modal-body">
          <Field label="작업 이름">
            <input
              required
              value={v.name}
              onChange={(e) => change("name", e.target.value)}
              placeholder="예: 고객 문의 대응 가이드"
            />
          </Field>
          <Field
            label="소스 경로"
            help="허용 소스 루트 아래의 절대 경로를 한 줄에 하나씩 입력합니다."
          >
            <textarea
              required
              value={v.sources.join("\n")}
              onChange={(e) => change("sources", e.target.value.split("\n"))}
              placeholder="/absolute/path/to/source"
            />
          </Field>
          <Field
            label="대상 Markdown 경로"
            help="출력 디렉터리는 미리 생성해주세요."
          >
            <input
              required
              value={v.target}
              onChange={(e) => change("target", e.target.value)}
              placeholder="/absolute/path/to/docs/guide.md"
            />
          </Field>
          <Field label="원하는 문서 생성 방향">
            <textarea
              className="tall"
              required
              value={v.direction}
              onChange={(e) => change("direction", e.target.value)}
              placeholder="어떤 독자를 위해, 무엇을 중심으로 정리할까요?"
            />
          </Field>
          <div className="preset-buttons">
            <button
              type="button"
              onClick={() =>
                change(
                  "direction",
                  "VOC 응대에 참고할 수 있도록 사용자 기능, 사용 조건, 오류 상황과 제약을 다양한 주제별로 정리해주세요. 소스 근거를 제시하고 확인할 수 없는 내용은 명시해주세요.",
                )
              }
            >
              VOC 주제별 정리
            </button>
            <button
              type="button"
              onClick={() =>
                change(
                  "direction",
                  "개발자가 참고할 수 있도록 주요 모듈과 요청 처리 흐름, 오류 처리 및 데이터 흐름을 Mermaid 다이어그램으로 정리해주세요. 파일과 심볼 근거를 포함해주세요.",
                )
              }
            >
              개발자 흐름도
            </button>
          </div>
          <div className="form-grid">
            <Field label="문서 언어">
              <input
                value={v.language}
                onChange={(e) => change("language", e.target.value)}
              />
            </Field>
            <Field label="최대 반복 횟수">
              <input
                type="number"
                min="1"
                max="10"
                value={v.max_iterations}
                onChange={(e) => change("max_iterations", +e.target.value)}
              />
            </Field>
            <Field label="전체 다이어그램 상한 (빈칸=자동, 0=없음)">
              <input
                type="number"
                min="0"
                max="32"
                value={v.max_diagrams ?? ""}
                onChange={(e) =>
                  change(
                    "max_diagrams",
                    e.target.value === "" ? null : +e.target.value,
                  )
                }
              />
            </Field>
            <Field label="최대 실행 시간 (초)">
              <input
                type="number"
                min="30"
                value={v.max_seconds}
                onChange={(e) => change("max_seconds", +e.target.value)}
              />
            </Field>
            <Field label="총 토큰 예산">
              <input
                type="number"
                min="1024"
                value={v.max_tokens}
                onChange={(e) => change("max_tokens", +e.target.value)}
              />
            </Field>
            <Field label="최대 비용 (USD, 0=미설정)">
              <input
                type="number"
                min="0"
                step="0.01"
                value={v.max_cost}
                onChange={(e) => change("max_cost", +e.target.value)}
              />
            </Field>
          </div>
          <details>
            <summary>파일 필터</summary>
            <Field label="포함 패턴 (한 줄에 하나)">
              <textarea
                value={v.include.join("\n")}
                onChange={(e) =>
                  change("include", e.target.value.split("\n").filter(Boolean))
                }
                placeholder="**/*.rs"
              />
            </Field>
            <Field label="제외 패턴 (한 줄에 하나)">
              <textarea
                value={v.exclude.join("\n")}
                onChange={(e) =>
                  change("exclude", e.target.value.split("\n").filter(Boolean))
                }
                placeholder="**/generated/**"
              />
            </Field>
          </details>
        </div>
        <div className="modal-footer">
          <button type="button" className="secondary" onClick={onClose}>
            취소
          </button>
          <button className="primary" type="submit">
            작업 저장
          </button>
        </div>
      </form>
    </div>
  );
}
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
  ) => void;
}) {
  const [currentLlm, setCurrentLlm] = useState(false);
  const [currentTokenLimit, setCurrentTokenLimit] = useState(false);
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
    if (!run) return;
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
          (["failed", "cancelled", "interrupted"].includes(run.status) ||
            (run.status === "completed_with_warnings" &&
              JSON.stringify(run.progress).includes(
                "this document is incomplete",
              ))) && (
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
              <button
                className="secondary"
                onClick={() => onResume(run.id, currentLlm, currentTokenLimit)}
              >
                <RefreshCw size={14} />
                체크포인트 재개
              </button>
              <small>
                기존 소스와 완료 섹션을 유지합니다. 토큰 한도 적용을 선택하면
                작업에 저장된 최신 한도로 재개합니다. 시간·비용 한도는
                유지합니다.
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
      {run.error && <div className="banner error">{run.error}</div>}
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
function SettingsPage({
  value,
  diagnostics,
  onSave,
  onError,
  onNotice,
}: {
  value: any;
  diagnostics: any;
  onSave: (v: any) => void;
  onError: (s: string) => void;
  onNotice: (s: string) => void;
}) {
  const [v, set] = useState(value),
    [tab, setTab] = useState("llm"),
    [testing, setTesting] = useState("");
  useEffect(() => set(value), [value]);
  const change = (group: string, key: string, x: any) =>
    set((old: any) =>
      group
        ? { ...old, [group]: { ...old[group], [key]: x } }
        : { ...old, [key]: x },
    );
  const input = (
    group: string,
    key: string,
    label: string,
    type = "text",
    help?: string,
  ) => {
    const x = group ? v[group][key] : v[key];
    return (
      <Field key={key} label={label} help={help}>
        <input
          type={type}
          step={type === "number" ? "any" : undefined}
          value={x}
          onChange={(e) =>
            change(
              group,
              key,
              type === "number" ? +e.target.value : e.target.value,
            )
          }
        />
      </Field>
    );
  };
  const select = (
    group: string,
    key: string,
    label: string,
    options: [string, string][],
  ) => (
    <Field label={label}>
      <select
        value={v[group][key]}
        onChange={(e) => change(group, key, e.target.value)}
      >
        {options.map(([x, label]) => (
          <option key={x} value={x}>
            {label}
          </option>
        ))}
      </select>
    </Field>
  );
  const probe = async (kind: string) => {
    setTesting(kind);
    try {
      const result = await api("/settings/test-" + kind, send("POST", v));
      onNotice(JSON.stringify(result));
    } catch (e) {
      onError((e as Error).message);
    } finally {
      setTesting("");
    }
  };
  return (
    <>
      <div className="page-heading">
        <div>
          <div className="eyebrow">WORKSPACE CONFIGURATION</div>
          <h1>공통 설정</h1>
          <p>모든 작업에서 사용할 연결과 실행 정책을 관리합니다.</p>
        </div>
        <button className="primary" onClick={() => onSave(v)}>
          설정 저장
        </button>
      </div>
      <div className="settings-layout">
        <div className="settings-nav">
          {[
            ["llm", "LLM 연결"],
            ["context", "컨텍스트 · Reasoning"],
            ["proxy", "Proxy · TLS"],
            ["db", "MariaDB"],
            ["runtime", "경로 · 실행 정책"],
          ].map(([key, label]) => (
            <button
              className={tab === key ? "active" : ""}
              key={key}
              onClick={() => setTab(key)}
            >
              {label}
              <ChevronRight size={14} />
            </button>
          ))}
        </div>
        <section className="panel settings-panel">
          {tab === "llm" && (
            <>
              <h2>LLM 연결</h2>
              <p className="muted">
                OpenAI 호환 Chat Completions API를 사용합니다.
              </p>
              {input("llm", "base_url", "API 기본 주소")}
              {input("llm", "api_key", "API 키", "password")}
              {input("llm", "model", "모델 이름")}
              <div className="form-grid">
                {input("llm", "timeout_seconds", "요청 timeout (초)", "number")}
                {input("llm", "retries", "최대 재시도", "number")}
                {input("llm", "concurrency", "동시 요청 수", "number")}
                {input("llm", "rpm", "분당 요청 수 (RPM)", "number")}
                {input("llm", "tpm", "분당 예약 토큰 (TPM)", "number")}
                {input("llm", "input_price", "입력 100만 토큰당 USD", "number")}
                {input(
                  "llm",
                  "output_price",
                  "출력 100만 토큰당 USD",
                  "number",
                )}
              </div>
              <button
                className="secondary"
                disabled={!!testing}
                onClick={() => probe("llm")}
              >
                <Activity size={16} />
                {testing === "llm" ? "확인 중…" : "LLM 연결 · 파라미터 테스트"}
              </button>
              <small className="footnote">
                연결 테스트는 짧은 실제 요청을 보내며 토큰을 사용합니다.
              </small>
            </>
          )}
          {tab === "context" && (
            <>
              <h2>컨텍스트와 Reasoning</h2>
              <div className="info-box">
                입력 + 최대 생성량 + 여유분이 설정 한도를 넘지 않도록 매 요청을
                검사합니다. 추정 모드는 서버 계산 차이로 초과 오류가 발생할 수
                있으며 자동 분할로 복구합니다.
              </div>
              <div className="form-grid">
                {input(
                  "llm",
                  "context_limit",
                  "앱 컨텍스트 한도 (최대 200,000)",
                  "number",
                )}
                {input(
                  "llm",
                  "model_context_limit",
                  "모델 실제 컨텍스트 한도",
                  "number",
                )}
                {input(
                  "llm",
                  "max_output_tokens",
                  "요청별 최대 생성 토큰",
                  "number",
                )}
                {input(
                  "llm",
                  "model_max_output",
                  "모델 최대 출력 토큰",
                  "number",
                )}
                {input("llm", "safety_percent", "안전 여유분 (%)", "number")}
                {select("llm", "token_mode", "토큰 계산", [
                  ["estimate", "보수적 추정"],
                  ["server", "서버 토큰 계산"],
                ])}
              </div>
              {v.llm.token_mode === "server" &&
                input(
                  "llm",
                  "token_count_url",
                  "서버 토큰 계산 URL",
                  "text",
                  "요청 전체를 POST하고 {input_tokens:number} 응답을 받습니다.",
                )}
              {select("llm", "output_parameter", "출력 제한 파라미터", [
                ["max_completion_tokens", "max_completion_tokens"],
                ["max_tokens", "max_tokens"],
              ])}
              <div className="form-grid">
                {select("llm", "reasoning", "Reasoning", [
                  ["default", "서버 기본값"],
                  ["off", "끄기"],
                  ["on", "켜기"],
                ])}
                {select("llm", "reasoning_parameter", "서버 파라미터 방식", [
                  ["reasoning_effort", "reasoning_effort (off=none)"],
                  ["enable_thinking", "chat_template_kwargs.enable_thinking"],
                ])}
                {select(
                  "llm",
                  "effort",
                  "Reasoning effort",
                  ["minimal", "low", "medium", "high", "xhigh", "max"].map(
                    (x) => [x, x],
                  ),
                )}
              </div>
            </>
          )}
          {tab === "proxy" && (
            <>
              <h2>Proxy와 TLS</h2>
              {select("llm", "proxy_mode", "Proxy 사용", [
                ["none", "사용하지 않음 (환경변수도 무시)"],
                ["system", "시스템 환경변수"],
                ["custom", "직접 지정"],
              ])}
              {v.llm.proxy_mode === "custom" && (
                <>
                  {input("llm", "proxy_url", "Proxy URL")}
                  {input("llm", "proxy_user", "Proxy 사용자")}
                  {input("llm", "proxy_password", "Proxy 비밀번호", "password")}
                </>
              )}
              {input(
                "llm",
                "ca_path",
                "사용자 CA 인증서 경로 (선택)",
                "text",
                "PEM 파일의 절대 경로. 기본 TLS 인증서 검증은 유지합니다.",
              )}
            </>
          )}
          {tab === "db" && (
            <>
              <h2>로컬 MariaDB</h2>
              <div className="info-box">
                현재 로컬에 설치된 MariaDB를 사용합니다. 애플리케이션 전용 DB만
                생성·변경하며 기존 테스트 DB는 유지합니다.
              </div>
              <div className="form-grid">
                {input("db", "host", "호스트")}
                {input("db", "port", "포트", "number")}
                {input("db", "database", "데이터베이스")}
                {input("db", "user", "사용자")}
                {input("db", "password", "비밀번호", "password")}
                {input("db", "max_connections", "최대 연결 수", "number")}
              </div>
              <label className="checkbox">
                <input
                  type="checkbox"
                  checked={v.db.tls}
                  onChange={(e) => change("db", "tls", e.target.checked)}
                />
                TLS 연결 필수
              </label>
              <button
                className="secondary"
                disabled={!!testing}
                onClick={() => probe("db")}
              >
                <Database size={16} />
                {testing === "db" ? "확인 중…" : "DB 연결 테스트"}
              </button>
              <p className="muted">
                현재 상태: {diagnostics?.database ? "연결됨" : "연결 확인 필요"}
              </p>
            </>
          )}
          {tab === "runtime" && (
            <>
              <h2>경로와 실행 정책</h2>
              {(["source_roots", "output_roots"] as const).map((key) => (
                <Field
                  key={key}
                  label={
                    key === "source_roots" ? "허용 소스 루트" : "허용 출력 루트"
                  }
                  help="이미 존재하는 절대 디렉터리 경로를 한 줄에 하나씩 입력합니다."
                >
                  <textarea
                    value={v[key].join("\n")}
                    onChange={(e) =>
                      change("", key, e.target.value.split("\n"))
                    }
                  />
                </Field>
              ))}
              <div className="form-grid">
                {input("", "max_jobs", "동시 작업 수", "number")}
                {input("", "max_file_bytes", "파일당 최대 바이트", "number")}
                {input("", "max_files", "최대 파일 수", "number")}
                {input("", "retention_days", "이력 보존 기간 (일)", "number")}
                {input("", "cache_max_mb", "캐시 최대 크기 (MiB)", "number")}
              </div>
              <p className="muted">
                실행 중에는 공통 설정 변경이 차단됩니다. 경로는 백엔드가
                실행되는 컴퓨터를 기준으로 합니다.
              </p>
            </>
          )}
        </section>
      </div>
    </>
  );
}
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
              {a.path.split(/[\\/]/).pop()} · {a.created_at} ·{" "}
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
                {a.created_at}
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

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
