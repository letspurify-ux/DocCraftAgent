# DocCraft Agent

로컬 소스 코드를 목적별 Markdown 문서로 만드는 Rust + React 애플리케이션입니다. 여러 문서화 작업, 근거 인용, Mermaid 흐름도, 자동 검토·개선, 취소·복구, 문서 버전 비교를 지원합니다.

## 실행

필요한 도구는 Rust 1.93 이상, Node.js 22 이상, 로컬 MariaDB입니다. MariaDB를 새로 설치하거나 Docker로 실행하지 않습니다.

```sh
node scripts/start.mjs
```

브라우저에서 **http://127.0.0.1:8765** 를 엽니다. 제공된 로컬 DB 계정은 이 환경의 `.local/settings.enc`에 암호화하여 설정합니다. 다른 환경에서는 화면의 **설정 → MariaDB**에서 계정을 입력하고 저장하세요. 최초 부트스트랩에만 `DOCCRAFT_DB_PASSWORD` 환경변수를 사용할 수도 있습니다.

1. **설정 → LLM 연결**에서 API 주소·모델·키를 입력하고 연결 테스트를 실행합니다.
2. **컨텍스트 · Reasoning**에서 모델의 실제 한도와 파라미터 방식을 설정합니다.
3. **경로 · 실행 정책**에서 존재하는 소스·출력 디렉터리를 허용 루트로 등록합니다.
4. **새 작업**에서 소스 경로, 대상 `.md` 경로, 원하는 문서 생성 방향을 입력합니다.
5. 실행하면 파일 분석, 작성, 검토·수정, 최종 저장까지 자동으로 진행합니다.

출력 디렉터리는 먼저 만들어야 합니다. 설정은 활성 실행이 없을 때 변경할 수 있습니다. 설정의 비밀번호 칸에 표시되는 `********`는 기존 값을 유지한다는 뜻입니다.

## 개발 및 검증

```sh
cargo run
npm --prefix frontend run dev
```

개발 화면은 http://127.0.0.1:5173 이며 API 요청은 Rust 서버로 전달됩니다.

```sh
cargo test --locked
cargo clippy --all-targets -- -D warnings
npm --prefix frontend run build
npm --prefix frontend run generate:api
```

실제 로컬 MariaDB와 모의 OpenAI 서버를 사용하는 통합 테스트:

```sh
# DOCCRAFT_DB_PASSWORD를 현재 셸에서 설정한 뒤 실행
python3 scripts/smoke.py
```

`DOCCRAFT_UI_TEST=1`을 추가하면 Playwright 브라우저 테스트를 실행합니다. 먼저 `frontend`에서 `npx playwright install chromium`을 실행하세요. `DOCCRAFT_LARGE_TEST=1`은 10만·100만 줄의 구조 분석과 캐시 재실행을 추가합니다. 테스트는 `doccraft_agent_test`에만 애플리케이션 데이터를 기록합니다. 기존 테스트 결과는 보존 기간에 따라 정리됩니다.

## 구성

- `backend/src`: API, 실행 상태 머신, 소스 색인, 격리 파서, LLM 예산 검사, 파일 게시·복구.
- `backend/migrations`: 전용 MariaDB 스키마와 공유 근거 청크.
- `frontend/src`: React 화면, OpenAPI에서 생성한 타입.
- `scripts/smoke.py`: 장애를 주입하는 통합 테스트와 선택적 대형 소스 벤치마크.
- [설계와 운영](docs/architecture.md): 상태, 컨텍스트 계약, 복구, 저장소와 한계.

## 데이터와 운영

업무 DB는 `doccraft_agent`, 테스트 DB는 `doccraft_agent_test`만 허용합니다. 기존 다른 데이터베이스를 변경하지 않습니다. 로컬 `.local/`에는 암호화 설정, 암호화 키, 스냅샷과 복구 저널이 들어갑니다. **DB 백업과 `.local/` 백업을 함께 보관하세요.** 암호화 키가 없으면 저장된 실행 설정을 복호화할 수 없습니다.

앱은 loopback 주소에만 바인딩합니다. 기본 포트는 `8765`이며 `DOCCRAFT_PORT`로 변경할 수 있습니다. `DOCCRAFT_DATA_DIR`로 로컬 데이터 경로를 변경할 수 있습니다. 키·암호·원본 LLM 요청은 로그에 출력하지 않습니다. 소스 코드와 생성 문서는 로컬 DB 및 설정한 LLM 서버로 전달됩니다.

## 검증 범위

실제 LLM 품질 검증에는 사용자의 API 주소·모델·키가 필요합니다. 통합 테스트의 LLM은 오류 복구와 전체 흐름을 확인하는 모의 서버입니다. 벤치마크 시간은 실제 모델의 추론 시간을 의미하지 않습니다.

추정 토큰 모드는 서버의 토크나이저·숨겨진 템플릿 차이 때문에 초과 오류를 절대 배제하지 못합니다. 서버 토큰 계산 URL을 제공하면 계산 계약을 강화할 수 있습니다. 원격 서버 내부 추론은 HTTP 연결 종료 후에도 계속될 수 있습니다.

Windows·Linux·macOS용 CI 정의를 포함합니다. 현재 로컬에서 실행한 검증과 CI에서 아직 실행하지 않은 플랫폼 검증은 구분합니다.
