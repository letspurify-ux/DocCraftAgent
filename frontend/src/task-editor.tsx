/**
 * The form for one documentation task: the sources it reads, the Markdown file
 * it writes, and the direction the document should take.
 */
import React, { useState } from "react";
import { api, send, type Task } from "./api";
import { Field } from "./shared";

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
          onSave({
            ...v,
            include: v.include.filter(Boolean),
            exclude: v.exclude.filter(Boolean),
          });
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
          <label className="checkbox">
            <input
              type="checkbox"
              checked={v.preview_outline}
              onChange={(e) => change("preview_outline", e.target.checked)}
            />
            본문 작성 전에 목차 먼저 확인
          </label>
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
            <Field
              label="최대 검토 회차"
              help="1회는 검토만 수행합니다. 2회 이상이면 앞 회차 지적을 수정한 뒤 다음 회차에서 다시 검토합니다."
            >
              <input
                type="number"
                min="1"
                max="20"
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
            <Field
              label="최대 실행 시간 (초)"
              help="최대 604,800초(7일)까지 설정할 수 있습니다."
            >
              <input
                type="number"
                min="30"
                max="604800"
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
                onChange={(e) => change("include", e.target.value.split("\n"))}
                placeholder="**/*.rs"
              />
            </Field>
            <Field label="제외 패턴 (한 줄에 하나)">
              <textarea
                value={v.exclude.join("\n")}
                onChange={(e) => change("exclude", e.target.value.split("\n"))}
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

export { TaskEditor };
