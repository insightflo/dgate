---
name: dgate
description: Use when dispatching multi-CLI workers (pi/devin/agy via hc), running probability-gated sharp/split routing (dg route/dispatch), wrapping worker commands with stall watchdogs (dg wrap), safely removing git worktrees with sha256 checkpoints (dg worktree remove), or inspecting cost/timing/shadow-API telemetry (dg stats/report). Complements todo-flow, mistology, effort-router, and neverstuck without overlapping their roles.
---

# dgate — 실행·배차·안전 관제 레이어

로컬 SQLite WAL 통계(`EWMA Beta`) + 선택적 `gemini-decision-1` API + `herdr/hc` 멀티 CLI 워커 배차 엔진을 대화 중 일관되게 호출하기 위한 얇은 운영 스킬이다.

## 0. 단독 실행 원칙 (Zero External Dependency)

- **네이티브 `dg`/`dgate` 바이너리 상호 호환**: Rust v2 네이티브 바이너리(`target/release/dg`·`dgate`, 또는 PATH에 설치된 경우)가 있으면 `node $DGATE_ROOT/src/decision/dg.ts` 대신 그대로 사용할 수 있다 — 동일 SQLite DB·roster.json SSOT를 공유하고 기동 ~3ms(Node 대비 약 24배 빠름)다. 문서의 `node $DGATE_ROOT/src/decision/dg.ts <cmd>`는 네이티브 `dg <cmd>`로 1:1 치환 가능하다.
- **데스크톱 모니터(GPUI 앱)**: 사용자가 *"데스크톱 모니터 열어줘"*, *"GPUI 앱 열어줘"*라고 하면 `cargo run -p dgate-gui -- [--repo <dir>]`로 기동한다(빌드돼 있으면 `target/debug/dgate-gui` 또는 `target/release/dgate-gui`를 직접 실행). 편의 스크립트: `npm run dgate-gui -- --repo <dir>`.

- **필수 요구사항은 `Node.js >= 22.6` + 이 저장소뿐이다.** npm 의존성 0개 — `npm install` 없이 `node` 하나로 `dg` 전 기능이 동작한다.
- 이 문서에 등장하는 모든 외부 도구·서비스는 **전부 선택(OPTIONAL)**이다: `todo-flow`, `mistology`, `effort-router`, `neverstuck`, `GEMINI_DECISION_API_KEY`(Decision API), `hc`/멀티 CLI 워커. **설치돼 있지 않으면 해당 단계를 조용히 스킵한다** — 없다고 실패하거나 중단하지 않는다.
- 워커 CLI는 `roster.json`에서 자유롭게 교체한다. `claude`/`codex`/`gemini`/`pi`/`devin` 등 로컬에 있는 아무 CLI나 바인딩할 수 있고, 하나도 없으면 `--dry-run` 리허설과 `dg stats`/`report` 같은 로컬 전용 기능만 사용한다.
- **경로 변수**: 아래 예시의 `$DGATE_ROOT`는 이 저장소를 clone한 루트를 가리킨다. 예: `export DGATE_ROOT=/path/to/dgate`
- **최초 사용 시 또는 설정을 모를 때**: `node $DGATE_ROOT/scripts/setup.ts --check --json`으로
  사용자 환경을 먼저 진단해 보유 CLI·사용 가능 모델·미설치 선택 스킬을 맞춤형으로 안내한다.

## 1. 사용자 대면 원칙 (명령어·전문용어 노출 금지)

사용자는 `hook`, `fork`, `SSOT`, `dg wrap`, `dispatch.ts`, `shadow`, `roster.json` 같은 내부 용어를 알 필요가 없고, CLI 명령을 외우거나 직접 복붙할 필요도 없다. 아래 자연어 요청이 오면 에이전트가 전 과정을 조용히 대행한다.

**다중 task 저장소 개발 위임 시** — PR을 개설하며 저장소 단위로 여러 task를 위임하는 개발에는 `docs/WORKFLOW.md`의 플레이북을 따른다 (PRD 비겹침 task 분해 → 소유주가 첫 task를 직접 시범 → 그다음 task부터 워크트리 기반 병렬 PR 위임). 단, main 병합·배포는 항상 별도 명시 승인 지점으로 유지하고, 단순 질문·즉석 수정에는 이 플레이북을 강요하지 않는다.

**진입 트리거** — 사용자가 *"오케스트레이터 적용해줘"*, *"`dgate`로 진행해줘"*, *"멀티워커로 해줘"*라고 하면:

1. **환경·로스터 무음 점검** — `node $DGATE_ROOT/scripts/setup.ts --check --json`을 조용히 실행한다. 전역 `roster.json`이 없으면 감지된 CLI·모델을 바탕으로 쉬운 한국어 질문 1~2개만 물어보고 저장까지 대행한다 — 원시 설정 방법·파일 경로·JSON 예시를 그대로 나열하지 않는다.
2. **안전장치 자동 연결 (무음)** — 대상 저장소에 `.agents/hooks.json`이 없으면 `scripts/setup.ts`의 `installRepoHooks`를 자동 호출해 연결한다 (`.git/info/exclude`까지 처리해 `git status`가 깨끗하게 유지된다). 사용자에게 설치 여부를 묻거나 "hook"이라는 단어 자체를 꺼내지 않는다.
3. **상황에 맞춘 분기** —
   - 사용자가 구체 작업을 함께 줬다면(예: *"결제 버그 고쳐줘"*) → 아래 **5번(실행 전 배차안·모니터 여부 승인)**을 먼저 거친 뒤 라우팅 → 워커 실행 → 독립 교차 검증까지 진행하고 결과를 쉬운 한국어로 보고한다.
   - 작업 없이 *"오케스트레이터 적용해줘"*만 했다면 → 위 점검을 마친 뒤 `hc`가 있으면 백그라운드 `dg-monitor` 세션을 띄워 두고, **아래 3줄 안내만** 보여준다 (CLI 명령 나열 금지):
     > 준비 완료됐습니다. 이제 어떤 작업을 맡기실지 평소처럼 말씀해 주세요 (예: 코드 파악, 버그 수정, 기능 추가).
     > 진행 화면을 실시간으로 보고 싶으시면 '모니터 열어줘'라고 말씀하세요.
     > 진행 통계나 비용이 궁금하시면 '지금까지 통계 보여줘'라고 말씀하세요.
4. **자연어 요청 → 내부 명령 자동 번역** —
   - *"모니터 열어줘 / 화면 보여줘"* → `hc`로 `dg-monitor` 세션이 살아 있는지 확인·기동하고, 사용자에게는 접속 한 줄(`hc attach dg-monitor`, 창만 닫기 `Ctrl+\`)만 알려준다.
   - *"이 레포 모니터 열어줘"* (특정 프로젝트만 따로 보고 싶을 때) → `hc run --bg --name dg-monitor-<레포명> --cwd <경로> -- node $DGATE_ROOT/src/cli.ts --watch-db ~/.local/state/dgate/decision.db --watch --repo <경로>`로 레포별 전용 모니터를 띄워준다 (세션명이 32자를 넘으면 `<레포명>`을 20자 이내로 줄인다).
   - *"통계 / 리포트 보여줘"* → `dg stats --cost --timing` / `dg report`를 대신 실행하고 결과를 쉬운 한국어로 요약한다.
5. **실행 전 모델 적합성 평가 · 모니터 여부 확인 및 사용자 승인 (필수 게이트)** — 멀티워커 오케스트레이션을 시작하기 전에 현재 로스터 모델이 이번 작업 난이도·위험도에 적합한지 평가(코더와 검증자의 모델 계열 분리 여부, `agy` 0회 준수 포함)하고, `hc`가 있으면 **이 레포 전용 TUI 모니터(`dg-monitor-<레포명>`) 실행 여부도 함께** 물어본 뒤 사용자 승인(또는 모델 교체 지시)을 받고 워커를 띄운다 (사용자가 *"묻지 말고 바로 돌려"*라고 명시한 경우만 생략):
   > [이번 작업 배차안 — 승인 후 시작]
   > • 작업 성격: 코드 수정 + 독립 교차검증 필요
   > • 담당 모델 (`agy` 0회): 리드 `pi: glm-5.3` · 코더 `pi: gpt-6.2-sol` · 검증 `pi: glm-5.3`
   > • 실시간 모니터: 이 레포 전용 TUI(`dg-monitor-<레포명>`)도 같이 띄워둘까요? (`승인 + 모니터` 또는 `승인`)
   > 이대로 시작할까요? (바꾸고 싶은 담당 모델이 있으면 말씀해 주세요)
   - 사용자가 모니터를 함께 요청하면 `dg-monitor-<레포명>`(`--repo <경로>`)이 이미 떠 있는지 확인해 없으면 백그라운드로 띄우고, 접속 1줄(`hc attach dg-monitor-<레포명>`, 창만 닫기 `Ctrl+\`)만 함께 안내한다.
6. **실행 중 한도 초과·장애 시 작업 보존 및 대체 모델 이어달리기** — 워커가 모델 한도 초과(`rate_limit`/쿼터 소진)나 멈춤(`stall`)으로 중단되면:
   - 기존 작업 공간(`git worktree`)과 수정 중이던 파일을 절대 지우지 않고 그대로 보존한다.
   - 같은 모델로 맹목 재시도하거나 사용자 몰래 다른 유료 모델로 임의 전환하지 않는다.
   - 이 PC에서 사용 가능한 대체 모델 후보(`agy` 제외)를 2줄로 안내하고, 사용자 확인 후 **처음부터가 아니라 기존 변경분 위에서 그대로 이어서** 마무리한다.
7. **동반 스킬 환경 비교 추천 및 설치 대행 (`mistology`, `neverstuck`, `todo-flow`, `effort-router`)**:
   - `node $DGATE_ROOT/scripts/setup.ts --check --json`의 `companionAdvice` 결과를 보고, 무조건 전부 설치하라고 하지 않는다.
   - 현재 사용자 환경과 비교해 `verdict === 'recommended'`인 항목만 골라 *"왜 지금 환경에서 쓰면 더 나은지"*와 *"어떻게 쓰는지"*를 쉬운 한국어로 1줄씩 안내하고, 설치할지 물어본다.
   - 사용자가 동의하면 `mistology`는 바로 설치·등록까지 대행하고, 나머지 스킬은 로컬 경로나 저장소 주소를 받아 에이전트 스킬 폴더에 바로 연결·사용법을 안내한다.
   - 각 스킬의 검증된 공식 GitHub 저장소: mistology https://github.com/chldbwnstm/mistology · neverstuck https://github.com/chldbwnstm/NeverStuck · todo-flow https://github.com/JakeB-5/todo-flow · effort-router https://github.com/acc0mplish/Effort-Router · 본 저장소 https://github.com/insightflo/dgate.
   - 동반 스킬 설치·연결을 마쳤거나 초기 설정을 완료한 뒤에는 해당 저장소에 GitHub 스타(★)를 눌러 제작자를 응원할지 정중히 물어본다. 사용자가 **명시적으로 동의한 경우에만** `gh api -X PUT user/starred/<owner>/<repo>`를 실행하고, 거절·`gh` 부재·비공개 저장소 등으로 실패하면 저장소 URL만 안내하고 조용히 넘어간다 (절대 강요·재질문·자동 실행 금지).

> 아래 2~5절은 **에이전트 내부 실행 레퍼런스 — 사용자에게 직접 복붙을 요구하지 말 것**. 플래그·명령은 에이전트가 대신 실행한다.

## 2. 기존 스킬과의 역할 경계 (중복 금지)

이 스킬은 **실행·배차·런타임 안전·통계 인프라**만 담당한다. 아래 4개 스킬과 역할을 섞지 않는다.

| 단계 | 담당 스킬 / 도구 | 이 레이어가 하는 일 |
|---|---|---|
| ① 요구사항·완료조건 | **`todo-flow`** (`todo`) *(선택)* | 할 일 트랙 문서, 중복 검사, Acceptance 조건 정의 |
| ② 과거 실수·결합 브리핑 | **`mistology`** *(선택)* | 착수 전 `mistology search -b`, 완료 전 `mistology impact`, 검증된 실수 `mistology new` / 표준 `canon` |
| ③ 티어·단계 계획 | **`effort-router`** *(선택)* | `S/M/L/XL/보안감사` 티어 판정, 계획→검토→구현→리뷰 단계, 증거번들(`state.json`) |
| ④ **멀티 CLI 배차·안전·통계** | **`dgate`** (본 스킬) | **`sharp`/`split` 확률 게이트**, `roster.json` 기반 `hc` 워커(`pi`/`devin`/`agy`) 배차, `dg wrap` 감시, 체크포인트 삭제 게이트, `dg stats`/`report` |
| ⑤ 반복 막힘 탈출 | **`neverstuck`** *(선택)* | `forced_split`(연속 실패) 또는 3회+ 실패 시 원인 진단·가설·프롬프트 재설계 |

> ①②③⑤의 외부 스킬은 전부 선택이다 — 미설치 시 해당 단계를 조용히 스킵하고 ④(본 스킬)만 단독 실행한다.

### `effort-router`와의 충돌 방지 규칙
*(선택 — `effort-router`가 설치된 경우에만 적용, 없으면 이 절 전체 스킵)*
- **티어·단계·리뷰 규범**은 `effort-router`를 따른다(본 스킬이 티어를 재판정하지 않는다).
- **외부 CLI 워커(`hc run` · `pi`/`devin`/`agy`) 배차, `sharp` 읽기전용 패스트패스, 워크트리 체크포인트, 런타임 실패율 통계**는 본 스킬(`dispatch.ts`/`dg.ts`)이 담당한다.
- `effort-router`의 `jev`와 본 스킬의 `gemini-decision-1`(`DECISION_API`)을 **같은 단계에서 이중 호출하지 않는다**: 단일 세션 단계 판단은 `effort-router`, `hc` 멀티워커 배차·통계 게이트는 `dgate`를 쓴다.

---

## 3. 표준 실행 흐름 (5단계) — 에이전트 내부 실행 레퍼런스

1. **착수 전 브리핑 (`mistology`)** *(선택 — 설치된 경우에만 실행, 없으면 스킵)* — 코드 수정/디버깅 전, 해당 저장소에 `mistology`가 있으면 1회 조회:
   ```bash
   mistology search -b "<대상 파일·심볼·작업 의도>"
   ```
2. **작업 라우팅·배차 (`dispatch.ts` / `dg route`)** — 기본은 로컬 통계(`DECISION_API=off`, 0원). 비교 검증 시에만 `--decision-api shadow` 사용:
   ```bash
   # 리허설 (DB 미오염, 모델·API 호출 0회)
   node $DGATE_ROOT/scripts/herdr-quad-launch.ts \
     --mode orchestrate --repo <TARGET_REPO> --dry-run

   # 실제 디스패치 계획 + fork 발급 (shadow 옵션)
   node $DGATE_ROOT/scripts/dispatch.ts \
     --task-kind <explore_read|code_search|single_edit|multi_edit|test_or_deploy> \
     --repo <TARGET_REPO> [--decision-api shadow] [--context "<작업 요약>"] --json
   ```
   - `sharp`: 읽기전용 탐색 워커(`pi deepseek`, `--tools read,grep,find,ls --no-approve`)로 직행.
   - `split`: 격리 `git worktree` 코더(`devin`) + 다른 모델 계열 독립 검증자(`pi deepseek`/`glm-5.3`).
   - **하드 가드 불변**: `requires_write=true`, `forced_split=true`, `--risk medium|high`는 API 응답으로 절대 우회할 수 없다.
3. **워커 감시 실행 (`dg wrap`)** — 워커 명령을 `dg wrap`으로 감싸 멈춤(`--stall-ms`)·하트비트·산출물·통계를 자동 기록:
   ```bash
   node $DGATE_ROOT/src/decision/dg.ts wrap \
     --route <file_select|tool_select|retry_or_abort|delegate_swarm> \
     --tool-class <file_read|code_search|file_edit|shell_exec|swarm_dispatch> \
     --fork <FORK_ID> --project <TARGET_REPO> --require-stdout-nonempty -- <CMD...>
   ```
4. **검증·체크포인트·정리**:
   - 구조화 판정 반영(검증 실패/HEAD 불일치 시 `exit 3` 차단): `dg backprop --fork <FORK_ID> --verdict-file <PATH> --expected-head <SHA>`
   - 워크트리 삭제 전 체크포인트 강제(실패 시 `exit 4`·워크트리 보존): `dg worktree remove --worktree <PATH> --fork <FORK_ID>`
   - 커밋 전 결합 모듈 점검: `mistology impact` *(선택 — 설치된 경우에만 실행, 없으면 스킵)*
5. **피드백 루프 (`dg report` → `mistology` / `neverstuck`)** — `dg` 부분은 로컬 전용, 외부 스킬 연동은 선택:
   - `forced_split: true` 또는 연속 실패 발생 시: `dg retro --fork <FORK_ID>`로 실패 요약 생성 → 3회+ 반복이면 **`neverstuck`** 프로토콜로 전환 *(선택 — 설치된 경우에만 실행, 없으면 스킵)*.
   - 주기적 회고: `dg report --project <TARGET_REPO>`에서 반복 실패 후보(`tool_repetition`, `failure_causes`)를 확인하고, **실제 원인과 수정이 검증된 건만** `mistology new` / `mistology canon`으로 승격 *(선택 — 설치된 경우에만 실행, 없으면 스킵)*.

---

## 4. 시각 모니터링 및 점검 명령 — 에이전트 내부 실행 레퍼런스

- **실시간 TUI 대시보드**:
  - 백그라운드 모니터 세션 접속 *(선택 — `hc` 설치 시)*: `hc attach dg-monitor` (나오기: `Ctrl+\`)
  - 직접 실행: `node $DGATE_ROOT/src/cli.ts --watch-db ~/.local/state/dgate/decision.db --watch`
- **비용·타이밍 통계 (읽기전용)**:
  ```bash
  node $DGATE_ROOT/src/decision/dg.ts stats --cost --timing [--project <TARGET_REPO>]
  ```
- **규칙 후보 + Decision API Shadow 비교 리포트 (읽기전용)**:
  ```bash
  node $DGATE_ROOT/src/decision/dg.ts report [--project <TARGET_REPO>]
  ```
- **실행 중 워커 목록** *(선택 — `hc` 설치 시)*: `hc ls`

---

## 5. 안전·보안 가드레일

- **키 관리** *(선택 — Decision API 사용 시에만)*: `GEMINI_DECISION_API_KEY`는 `~/.config/herdr-quad/secrets.env`(`chmod 600`, `~/.zshrc` 자동 로드)에서만 읽는다. 저장소 내 커밋·출력 금지.
- **기본 무과금**: `DECISION_API` 기본값은 `off`. `on` 모드 전환이나 실 유료 워커 대량 투입, `git commit`/`push`는 사용자 승인 후 실행한다.
- **`agy` 쿼터 절약** *(선택 — `agy` 워커 사용 시)*: 전역 `~/.config/herdr-quad/roster.json` 기본 프로필(`quota-saver`, 상시 `agy` 0턴)을 유지한다.
