/**
 * Connection and policy settings: the database, the model endpoint with its
 * context limits, and the roots a run may read from and write to.
 */
import React, { useState, useEffect } from "react";
import { Activity, ChevronRight, Database } from "lucide-react";
import { api, send } from "./api";
import { Field } from "./shared";

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

export { SettingsPage };
