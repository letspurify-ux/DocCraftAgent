/**
 * The application shell: which page is showing, the data every page needs, and
 * the polling that keeps runs up to date.
 *
 * Each page lives in its own module; this one decides which of them is on
 * screen and mounts the app.
 */
import React, { useState, useEffect, useCallback } from "react";
import {
  BookOpen,
  Layers,
  Activity,
  Settings2,
  Plus,
  Play,
  ChevronRight,
  CheckCircle2,
  AlertCircle,
  FileCode2,
  Files,
  RefreshCw,
  Copy,
  Trash2,
  ArrowUpRight,
  Search,
  Terminal,
  PanelLeftClose,
} from "lucide-react";
import { createRoot, type Root } from "react-dom/client";
import { api, send, newTask, type Task, type Run, type Artifact } from "./api";
import "./styles.css";
import { Badge, Stat, busy, fmt, localTime, type Page } from "./shared";
import { TaskEditor } from "./task-editor";
import { RunDetail } from "./run-detail";
import { SettingsPage } from "./settings";
import { Documents } from "./documents";

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
                            <small>{localTime(r.created_at)}</small>
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
                      onResume={(
                        id,
                        currentLlm,
                        currentTokenLimit,
                        currentReviewLimit,
                      ) =>
                        action(async () => {
                          await api(
                            `/runs/${id}/resume?current_llm=${currentLlm}&current_token_limit=${currentTokenLimit}&current_review_limit=${currentReviewLimit}`,
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

declare global {
  interface Window {
    __doccraftRoot?: Root;
  }
}

const rootContainer = document.getElementById("root")!;
// Vite can re-evaluate this entry module during HMR; reuse the mounted root.
const root = window.__doccraftRoot ?? createRoot(rootContainer);
window.__doccraftRoot = root;
root.render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
