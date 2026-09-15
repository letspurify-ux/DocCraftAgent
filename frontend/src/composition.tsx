import { useEffect, useRef, useState } from "react";
import { api, send, type Run } from "./api";
import type { components } from "./api.generated";

type Section = Required<components["schemas"]["SectionPlan"]>;
type Outline = Omit<Required<components["schemas"]["Outline"]>, "sections"> & {
  sections: Section[];
};
function normalizePlan(p: components["schemas"]["Outline"]): Outline {
  return {
    ...p,
    reader_goal: p.reader_goal ?? "",
    storyline: p.storyline ?? "",
    terminology: p.terminology ?? [],
    revision: p.revision ?? 0,
    requirements: p.requirements ?? [],
    sections: p.sections.map((s) => ({
      ...s,
      id: s.id ?? "",
      reader_question: s.reader_question ?? "",
      handoff: s.handoff ?? "",
      diagrams: s.diagrams ?? null,
      depends_on: s.depends_on ?? [],
      evidence_ids: s.evidence_ids ?? [],
      owns_requirement_ids: s.owns_requirement_ids ?? [],
      key_points: s.key_points ?? [],
      out_of_scope: s.out_of_scope ?? [],
    })),
  };
}
type Brief = {
  findings: { topic: string; observation: string }[];
  uncertainties: string[];
};
type Understanding = {
  coverage?: {
    reading_complete?: boolean;
    unresolved_nodes?: number;
    additional_reading_unresolved?: boolean;
    complete: boolean;
    read_files: number;
    read_chunks: number;
    read_batches: number;
  };
  overview?: Brief;
  questions?: {
    requirement_id: string;
    question: string;
    brief: Brief;
    validation_unresolved: boolean;
  }[];
  batches: {
    id: string;
    files: string[];
    brief: Brief;
    validation_issues?: string[];
  }[];
  next_cursor: string;
  has_more: boolean;
  total_files: number;
};

export function Composition({ run }: { run: Run }) {
  const [tab, setTab] = useState<"source" | "outline" | null>(null);
  const [source, setSource] = useState<Understanding | null>(null);
  const [plan, setPlan] = useState<Outline | null>(null);
  const [issues, setIssues] = useState<{ severity: string; message: string }[]>(
    [],
  );
  const [error, setError] = useState("");
  const [feedback, setFeedback] = useState("");
  const [saving, setSaving] = useState(false);
  const dirty = useRef(false);
  const active = ["queued", "running", "cancelling"].includes(run.status);

  useEffect(() => {
    dirty.current = false;
    setPlan(null);
    setSource(null);
    setFeedback("");
    setError("");
  }, [run.id]);
  useEffect(() => {
    if (!tab || dirty.current) return;
    const controller = new AbortController();
    const route = tab === "source" ? "understanding" : "outline";
    api(`/runs/${run.id}/${route}`, { signal: controller.signal })
      .then((value) => {
        if (tab === "source") setSource(value);
        else {
          setPlan(value.outline ? normalizePlan(value.outline) : null);
          setIssues(value.review?.issues ?? []);
        }
        setError("");
      })
      .catch((e) => {
        if (e.name !== "AbortError") setError(e.message);
      });
    return () => controller.abort();
  }, [run.id, run.status, run.updated_at, tab]);

  function change(index: number, patch: Partial<Section>) {
    dirty.current = true;
    setPlan(
      (p) =>
        p && {
          ...p,
          sections: p.sections.map((s, i) =>
            i === index ? { ...s, ...patch } : s,
          ),
        },
    );
  }
  function reorder(from: number, to: number) {
    if (!plan || to < 0 || to >= plan.sections.length) return;
    const old = plan.sections;
    const next = [...old];
    const [item] = next.splice(from, 1);
    next.splice(to, 0, item);
    dirty.current = true;
    setPlan({
      ...plan,
      sections: next.map((s) => ({
        ...s,
        depends_on: s.depends_on.map((i) =>
          next.findIndex((n) => n.id === old[i]?.id),
        ),
      })),
    });
  }
  function remove(index: number) {
    if (!plan || plan.sections.length < 2) return;
    dirty.current = true;
    setPlan({
      ...plan,
      sections: plan.sections
        .filter((_, i) => i !== index)
        .map((s) => ({
          ...s,
          depends_on: s.depends_on
            .filter((i) => i !== index)
            .map((i) => (i > index ? i - 1 : i)),
        })),
    });
  }
  function add() {
    if (!plan || plan.sections.length >= 32) return;
    dirty.current = true;
    const section: Section = {
      id: crypto.randomUUID(),
      title: "새 섹션",
      query: "",
      reader_question: "",
      handoff: "",
      diagrams: [],
      depends_on: [],
      evidence_ids: plan.sections[0]?.evidence_ids ?? [],
      owns_requirement_ids: [],
      key_points: [],
      out_of_scope: [],
    };
    setPlan({ ...plan, sections: [...plan.sections, section] });
  }
  async function action(kind: "save" | "feedback" | "continue") {
    if (!plan) return;
    setSaving(true);
    setError("");
    try {
      await api(
        `/runs/${run.id}/outline/${kind === "continue" ? "continue" : "revisions"}`,
        send(
          "POST",
          kind === "continue"
            ? { revision: plan.revision }
            : {
                base_revision: plan.revision,
                request_id: crypto.randomUUID(),
                ...(kind === "save" ? { outline: plan } : { feedback }),
              },
        ),
      );
      dirty.current = false;
      setFeedback("");
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setSaving(false);
    }
  }
  return (
    <div className="composition">
      <div className="tabs">
        <button
          className={tab === "source" ? "active" : ""}
          onClick={() => setTab(tab === "source" ? null : "source")}
        >
          소스 이해
        </button>
        <button
          className={tab === "outline" ? "active" : ""}
          onClick={() => setTab(tab === "outline" ? null : "outline")}
        >
          문서 구성
        </button>
      </div>
      {error && (
        <p role="alert" className="banner error">
          {error}
        </p>
      )}
      {tab === "source" && source && (
        <div className="composition-body">
          <p>
            전체 목록 {source.total_files.toLocaleString()}개 ·{" "}
            {source.coverage?.reading_complete && !source.coverage.complete
              ? "원문 읽기 완료 · 검증 미해결 항목이 있습니다"
              : source.coverage?.complete
                ? `원문 읽기 ${source.coverage.read_files}개 파일 / ${source.coverage.read_chunks}개 코드 묶음 완료`
                : "전체 소스를 나누어 읽는 중입니다."}
          </p>
          <small>
            읽기 범위는 이해 정확도를 의미하지 않습니다. 미확인 연결은 아래에
            표시합니다.
          </small>
          {source.coverage?.additional_reading_unresolved && (
            <p className="banner">
              추가 분석을 검증하지 못해 이전 검증 결과로 진행했습니다.
            </p>
          )}
          {source.overview && <BriefView brief={source.overview} />}
          {!!source.questions?.length && (
            <div>
              <h4>문서 목적별 질문과 근거</h4>
              {source.questions.map((question) => (
                <details key={question.requirement_id}>
                  <summary>{question.question}</summary>
                  {question.validation_unresolved && (
                    <p className="banner">
                      추가 분석을 검증하지 못해 보존한 소스 관찰로 진행했습니다.
                    </p>
                  )}
                  <BriefView brief={question.brief} />
                </details>
              ))}
            </div>
          )}
          {source.batches.map((batch) => (
            <details key={batch.id}>
              <summary>
                {batch.files.map((p) => p.split(/[\\/]/).pop()).join(", ")}
              </summary>
              {batch.validation_issues?.map((issue, index) => (
                <p className="banner" key={index}>
                  근거 검증 미해결: {issue}
                </p>
              ))}
              <BriefView brief={batch.brief} />
            </details>
          ))}
          {source.has_more && (
            <button
              onClick={async () => {
                try {
                  const next = await api<Understanding>(
                    `/runs/${run.id}/understanding?after=${encodeURIComponent(source.next_cursor)}`,
                  );
                  setSource({
                    ...next,
                    batches: [...source.batches, ...next.batches],
                  });
                } catch (e) {
                  setError((e as Error).message);
                }
              }}
            >
              다음 분석 묶음 보기
            </button>
          )}
        </div>
      )}
      {tab === "outline" && !plan && (
        <p>전체 소스 이해를 마치면 목차 후보가 표시됩니다.</p>
      )}
      {tab === "outline" && plan && (
        <div className="composition-body">
          <p>
            <strong>목차 버전 {plan.revision || 0}</strong> · {plan.reader_goal}
          </p>
          <p>{plan.storyline}</p>
          {active && <p>실행 중인 구성을 변경하려면 먼저 실행을 중단하세요.</p>}
          {issues.map((issue, i) => (
            <p key={i} className="banner">
              {issue.severity === "major" ? "수정 필요" : "참고"}:{" "}
              {issue.message}
            </p>
          ))}
          {plan.sections.map((section, index) => (
            <fieldset
              className="outline-section"
              key={section.id || index}
              disabled={active || saving}
            >
              <legend>{index + 1}장</legend>
              <label>
                제목
                <input
                  value={section.title}
                  onChange={(e) => change(index, { title: e.target.value })}
                />
              </label>
              <label>
                독자가 해결할 질문
                <input
                  value={section.reader_question}
                  onChange={(e) =>
                    change(index, { reader_question: e.target.value })
                  }
                />
              </label>
              <label>
                핵심 설명 · 한 줄에 하나
                <textarea
                  value={section.key_points.join("\n")}
                  onChange={(e) =>
                    change(index, {
                      key_points: e.target.value.split("\n").filter(Boolean),
                    })
                  }
                />
              </label>
              <details>
                <summary>담당 주제와 선행 설명</summary>
                <label>
                  근거를 확인할 파일·기능
                  <input
                    value={section.query}
                    onChange={(e) => change(index, { query: e.target.value })}
                  />
                </label>
                {plan.requirements.map((r) => (
                  <label className="checkbox" key={r.id}>
                    <input
                      type="checkbox"
                      checked={section.owns_requirement_ids.includes(r.id)}
                      onChange={(e) =>
                        change(index, {
                          owns_requirement_ids: e.target.checked
                            ? [...section.owns_requirement_ids, r.id]
                            : section.owns_requirement_ids.filter(
                                (id) => id !== r.id,
                              ),
                        })
                      }
                    />
                    {r.question}
                  </label>
                ))}
                <label>
                  다음 장에 이어지는 결과
                  <input
                    value={section.handoff}
                    onChange={(e) => change(index, { handoff: e.target.value })}
                  />
                </label>
                {plan.sections.map(
                  (s, i) =>
                    i !== index && (
                      <label className="checkbox" key={s.id || i}>
                        <input
                          type="checkbox"
                          checked={section.depends_on.includes(i)}
                          onChange={(e) =>
                            change(index, {
                              depends_on: e.target.checked
                                ? [...section.depends_on, i]
                                : section.depends_on.filter((n) => n !== i),
                            })
                          }
                        />
                        선행 설명: {i + 1}장 {s.title}
                      </label>
                    ),
                )}
              </details>
              <div className="card-actions">
                <button
                  disabled={index === 0}
                  onClick={() => reorder(index, index - 1)}
                >
                  위로
                </button>
                <button
                  disabled={index === plan.sections.length - 1}
                  onClick={() => reorder(index, index + 1)}
                >
                  아래로
                </button>
                <button
                  disabled={plan.sections.length < 2}
                  onClick={() => remove(index)}
                >
                  섹션 삭제
                </button>
              </div>
            </fieldset>
          ))}
          <div className="card-actions">
            <button
              disabled={active || saving || plan.sections.length >= 32}
              onClick={add}
            >
              섹션 추가
            </button>
            <button disabled={active || saving} onClick={() => action("save")}>
              수정 목차 검토
            </button>
            {run.status === "awaiting_outline" && (
              <button
                className="primary"
                disabled={
                  saving ||
                  dirty.current ||
                  issues.some((i) => i.severity === "major")
                }
                onClick={() => action("continue")}
              >
                목차 확정·본문 작성
              </button>
            )}
          </div>
          <label>
            구성 변경 요청
            <textarea
              placeholder="예: 중복된 두 장을 합치고 정상 요청과 취소 흐름을 별도 장으로 나눠주세요."
              value={feedback}
              disabled={active || saving}
              onChange={(e) => setFeedback(e.target.value)}
            />
          </label>
          <button
            disabled={active || saving || !feedback.trim()}
            onClick={() => action("feedback")}
          >
            요청대로 목차 재구성
          </button>
        </div>
      )}
    </div>
  );
}
function BriefView({ brief }: { brief: Brief }) {
  return (
    <>
      <ul>
        {brief.findings.map((f, i) => (
          <li key={i}>
            <strong>{f.topic}</strong>
            <p>{f.observation}</p>
          </li>
        ))}
      </ul>
      {brief.uncertainties.length > 0 && (
        <p className="banner">미확인: {brief.uncertainties.join(" · ")}</p>
      )}
    </>
  );
}
