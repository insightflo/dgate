---
name: dgate
description: Use when dispatching multi-CLI workers (pi/devin/agy via hc), running probability-gated sharp/split routing (dg route/dispatch), wrapping worker commands with stall watchdogs (dg wrap), safely removing git worktrees with sha256 checkpoints (dg worktree remove), or inspecting cost/timing/shadow-API telemetry (dg stats/report). Complements todo-flow, mistology, effort-router, and neverstuck without overlapping their roles.
---

# dgate — 실행·배차·안전 관제 레이어

로컬 SQLite WAL 통계(`EWMA Beta`) + 선택적 `gemini-decision-1` API + `herdr/hc` 멀티 CLI 워커 배차 엔진을 대화 중 일관되게 호출하기 위한 얇은 운영 스킬이다.

## 0. 단독 실행 원칙 (Zero External Dependency)

- **네이티브 `dg` 실행 원칙**: 워커 실행·배차·통계 명령은 Rust v2 네이티브 바이너리(`dg`, PATH 설치 또는 `target/release/dg`)로 실행한다 — 동일 SQLite DB·roster.json SSOT 공유, 기동 ~3ms. Node v1 폴백은 없다 — 바이너리가 없으면 `npm run build`(`cargo build --release --workspace`)로 빌드한다. **git pull 후에도 `npm run build`로 바이너리를 갱신한다**(스테일 바이너리 방지).
- **데스크톱 모니터(GPUI 앱)**: 사용자가 *"데스크톱 모니터 열어줘"*, *"GPUI 앱 열어줘"*라고 하면 `cargo run -p dgate-gui -- [--repo <dir>]`로 기동한다(빌드돼 있으면 `target/debug/dgate-gui` 또는 `target/release/dgate-gui`를 직접 실행). 편의 스크립트: `npm run dgate-gui -- --repo <dir>`.

- **3개 독립 모듈 선택 도입 (무침습 기본)**: `dgate`는 전면 도입을 강제하지 않는다. 이미 자체 오케스트레이터나 상주 좌석(`herdr agent`)을 쓰는 팀은 필요한 모듈만 골라 쓴다:
  1. **모니터 단독**: `dgate-tui` / `dgate-gui` (`hc ls` + `herdr agent list` 읽기전용 관제 — 대상 레포 파일 수정 0건)
  2. **라우터 단독**: `dg route` / `dg stats` (EWMA 성공률 기반 저비용 자동 배정 판정기만 CLI/JSON 호출)
  3. **풀 오케스트레이터**: `dg dispatch` / `dg pipeline` (격리 worktree 병렬 코딩 + 이종 검증 + 0토큰 상태머신)
- **필수 요구사항은 Rust toolchain + 이 저장소뿐이다.** `cargo build --release --workspace` 한 번이면 `dg` 전 기능이 동작한다. (`scripts/install-global.ts` 전역 래퍼 설치에만 Node.js >= 22.6 필요 — npm 의존성 0개.)
- 이 문서에 등장하는 모든 외부 도구·서비스는 **전부 선택(OPTIONAL)**이다: `todo-flow`, `mistology`, `effort-router`, `neverstuck`, `GEMINI_DECISION_API_KEY`(Decision API), `hc`/멀티 CLI 워커. **설치돼 있지 않으면 해당 단계를 조용히 스킵한다** — 없다고 실패하거나 중단하지 않는다.
- 워커 CLI는 `roster.json`에서 자유롭게 교체한다. `claude`/`codex`/`gemini`/`pi`/`devin` 등 로컬에 있는 아무 CLI나 바인딩할 수 있고, 하나도 없으면 `--dry-run` 리허설과 `dg stats`/`report` 같은 로컬 전용 기능만 사용한다.
- **경로 변수**: 아래 예시의 `$DGATE_ROOT`는 이 저장소를 clone한 루트를 가리킨다. 예: `export DGATE_ROOT=/path/to/dgate`
- **최초 사용 시 또는 설정을 모를 때**: `dg setup --check --json`으로
  사용자 환경을 먼저 진단해 보유 CLI·사용 가능 모델·미설치 선택 스킬을 맞춤형으로 안내한다.

## 1. 사용자 대면 원칙 (명령어·전문용어 노출 금지)

사용자는 `hook`, `fork`, `SSOT`, `dg wrap`, `dg dispatch`, `shadow`, `roster.json` 같은 내부 용어를 알 필요가 없고, CLI 명령을 외우거나 직접 복붙할 필요도 없다. 아래 자연어 요청이 오면 에이전트가 전 과정을 조용히 대행한다.

**다중 task 저장소 개발 위임 시** — PR을 개설하며 저장소 단위로 여러 task를 위임하는 개발에는 `docs/WORKFLOW.md`의 플레이북을 따른다 (PRD 비겹침 task 분해 → 소유주가 첫 task를 직접 시범 → 그다음 task부터 워크트리 기반 병렬 PR 위임). 단, main 병합·배포는 항상 별도 명시 승인 지점으로 유지하고, 단순 질문·즉석 수정에는 이 플레이북을 강요하지 않는다.

**진입 트리거** — 사용자가 *"오케스트레이터 적용해줘"*, *"`dgate`로 진행해줘"*, *"멀티워커로 해줘"*라고 하면:

1. **환경·로스터 무음 점검** — `dg setup --check --json`을 조용히 실행한다. 전역 `roster.json`이 없으면 감지된 CLI·모델을 바탕으로 쉬운 한국어 질문 1~2개만 물어보고 저장까지 대행한다 — 원시 설정 방법·파일 경로·JSON 예시를 그대로 나열하지 않는다.
   - **배차 모드 선선택(무조건)**: 새 워커(`dg wrap`/`hc run`)를 띄우기 직전에 소프트/하드가 아직 선택되지 않았으면 **어떤 경로로든 반드시 먼저 묻고 답을 받을 때까지 워커를 띄우지 않는다** — "현재 작업 dgate로 진행해" 같은 이어가기 요청, 이미 진행 중인 파이프라인의 검증·수정 워커 포함. 통계·모니터 조회 같은 읽기 요청에는 묻지 않는다.
   - **버전 점검**: `git -C $DGATE_ROOT fetch -q` 후 `git -C $DGATE_ROOT rev-list --count HEAD..origin/main`이 0보다 크면 "dgate 업데이트 있음 — `git pull` + `npm run build` 권장"을 한 줄 보고한다(pull은 사용자 승인 후).
   - **자동 수집(판단 루프, CLI 무관)**: pi 세션은 전역 확장(`~/.pi/agent/extensions/dg-collect.ts`, 원본 `scripts/pi-extensions/`)이 `dg hook --collect`로 자동 기록한다(source=hook·멱등). agy/gemini·claude/codex 리드용 트랜스크립트 수집기(v1 `--watch-dir`)는 아직 v2 이관 대상이다 — 현재는 pi 확장 경로만 자동 수집된다.
2. **안전장치 선택 연결 (무침습 기본 · 명시 동의 opt-in)** — 대상 저장소의 파일(`.agents/hooks.json`, `AGENTS.md`)을 **절대 묻지 않고 자동 수정하지 않는다**.
   - 모니터 단독(`dgate-tui`/`dgate-gui`)이나 라우터 단독(`dg route`/`dg stats`) 사용 시에는 저장소 파일을 0건 수정한다.
   - 풀 오케스트레이션에서 읽기전용 가드 훅과 계약 블록을 연결하려면, 먼저 사용자에게 변경될 파일 2개(`.agents/hooks.json`, `AGENTS.md`)를 안내하고 **명시적 동의를 받은 경우에만** `dg setup --repo <대상 저장소>`를 실행한다. 사용자가 거절하면 저장소 수정 없이 그대로 진행한다.
3. **상황에 맞춘 분기** —
   - 사용자가 구체 작업을 함께 줬다면(예: *"결제 버그 고쳐줘"*) → 아래 **5번(실행 전 배차안·모니터 여부 승인)**을 먼저 거친 뒤 라우팅 → 워커 실행 → 독립 교차 검증까지 진행하고 결과를 쉬운 한국어로 보고한다.
   - 작업 없이 *"오케스트레이터 적용해줘"*만 했다면 → 위 점검을 마친 뒤 `hc`가 있으면 백그라운드 `dg-monitor` 세션을 띄워 두고, **아래 3줄 안내만** 보여준다 (CLI 명령 나열 금지):
     > 준비 완료됐습니다. 이제 어떤 작업을 맡기실지 평소처럼 말씀해 주세요 (예: 코드 파악, 버그 수정, 기능 추가).
     > 진행 화면을 실시간으로 보고 싶으시면 '모니터 열어줘'라고 말씀하세요.
     > 진행 통계나 비용이 궁금하시면 '지금까지 통계 보여줘'라고 말씀하세요.
4. **자연어 요청 → 내부 명령 자동 번역** —
   - *"모니터 열어줘 / 화면 보여줘"* → `hc`로 `dg-monitor` 세션이 살아 있는지 확인·기동하고, 사용자에게는 접속 한 줄(`hc attach dg-monitor`, 창만 닫기 `Ctrl+\`)만 알려준다.
   - *"이 레포 모니터 열어줘"* (특정 프로젝트만 따로 보고 싶을 때) → `hc run --bg --name dg-monitor-<레포명> --cwd <경로> -- dgate-tui --watch-db ~/.local/state/dgate/decision.db --repo <경로>`로 레포별 전용 모니터를 띄워준다 (세션명이 32자를 넘으면 `<레포명>`을 20자 이내로 줄인다).
   - *"통계 / 리포트 보여줘"* → `dg stats --cost --timing` / `dg report`를 대신 실행하고 결과를 쉬운 한국어로 요약한다.
5. **실행 전 모델 적합성 평가 · 모니터 여부 확인 및 사용자 승인 (필수 게이트)** — 멀티워커 오케스트레이션을 시작하기 전에 현재 로스터 모델이 이번 작업 난이도·위험도에 적합한지 평가(코더와 검증자의 모델 계열 분리 여부, `agy` 0회 준수 포함)하고, `hc`가 있으면 **이 레포 전용 TUI 모니터(`dg-monitor-<레포명>`) 실행 여부도 함께** 물어본 뒤 사용자 승인(또는 모델 교체 지시)을 받고 워커를 띄운다 (사용자가 *"묻지 말고 바로 돌려"*라고 명시한 경우만 생략):
   > [이번 작업 배차안 — 승인 후 시작]
   > • 작업 성격: 코드 수정 + 독립 교차검증 필요
   > • 담당 모델 (`agy` 0회): 리드 `pi: glm-5.3` · 코더 `pi: gpt-6.2-sol` · 검증 `pi: glm-5.3`
   > • 배차 모드: `소프트`(리드가 `dg dispatch` 게이트 판정 경유·지침 기반) / `하드`(가드 환경으로 내장 위임 차단·dispatch 발급 fork만 허용) — 어느 쪽으로 할까요? (미선택 시 소프트)
   > • 실시간 모니터: 이 레포 전용 TUI(`dg-monitor-<레포명>`)도 같이 띄워둘까요? (`승인 + 모니터` 또는 `승인`)
   > 이대로 시작할까요? (바꾸고 싶은 담당 모델이 있으면 말씀해 주세요)
   - 배차 모드 적용: 소프트면 리드 지시문에 `dg dispatch --task-kind` 경유를 명시한다(게이트 자동 판정 수용·우회는 관측으로만 제재). 하드면 `dg dispatch-mode --set strict`로 집행 장치를 켠다 — 이후 delegate_swarm `dg wrap`은 `dg dispatch`/`dg route`가 발급한 `--fork` 없으면 exit 2로 거부된다(지침 무관·물리 적용, 해제 `dg dispatch-mode --set off`). 내장 위임 차단은 가드 환경(`HERDR_DELEGATE_MODE=hc` + 2번에서 설치된 `.agents/hooks.json`)이 담당한다.
   - **진행 중 적용**: 이미 돌아가는 오케스트레이션에 나중에 dgate 적용을 요청받으면(예: *"현재 작업 dgate로 진행해"*)에도 배차 모드를 한 번 물어, 이후 모든 배차(검증·수정 워커 포함)에 즉시 적용한다 — 새 시작에만 게이트를 걸지 않는다.
   - 사용자가 모니터를 함께 요청하면 `dg-monitor-<레포명>`(`--repo <경로>`)이 이미 떠 있는지 확인해 없으면 백그라운드로 띄우고, 접속 1줄(`hc attach dg-monitor-<레포명>`, 창만 닫기 `Ctrl+\`)만 함께 안내한다.
6. **실행 중 한도 초과·장애 시 작업 보존 및 대체 모델 이어달리기** — 워커가 모델 한도 초과(`rate_limit`/쿼터 소진)나 멈춤(`stall`)으로 중단되면:
   - 기존 작업 공간(`git worktree`)과 수정 중이던 파일을 절대 지우지 않고 그대로 보존한다.
   - 같은 모델로 맹목 재시도하거나 사용자 몰래 다른 유료 모델로 임의 전환하지 않는다.
   - 이 PC에서 사용 가능한 대체 모델 후보(`agy` 제외)를 2줄로 안내하고, 사용자 확인 후 **처음부터가 아니라 기존 변경분 위에서 그대로 이어서** 마무리한다.
7. **동반 스킬 환경 비교 추천 및 설치 대행 (`mistology`, `neverstuck`, `todo-flow`, `effort-router`)**:
   - `dg setup --check --json`의 `companionAdvice` 결과를 보고, 무조건 전부 설치하라고 하지 않는다.
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
- **외부 CLI 워커(`hc run` · `pi`/`devin`/`agy`) 배차, `sharp` 읽기전용 패스트패스, 워크트리 체크포인트, 런타임 실패율 통계**는 본 스킬(`dg dispatch`/`dg`)이 담당한다.
- `effort-router`의 `jev`와 본 스킬의 `gemini-decision-1`(`DECISION_API`)을 **같은 단계에서 이중 호출하지 않는다**: 단일 세션 단계 판단은 `effort-router`, `hc` 멀티워커 배차·통계 게이트는 `dgate`를 쓴다.

---

## 3. 표준 실행 흐름 (5단계) — 에이전트 내부 실행 레퍼런스

**자동 병렬 기본값 (Default-On Parallel)**: 작업 착수 시 파일 수정 범위가 겹치지 않는 독립 작업이 **2개 이상**이면 사용자가 "병렬로 해줘"라고 명시하지 않아도 **기본값으로 격리 `git worktree` + 병렬 워커(`dg dispatch` + `dg wrap`/`dg pipeline run`)를 동시 기동**한다. 직렬 실행은 동일 파일 충돌이 확실하거나 선후 의존(`--after`)이 있는 경우에만 예외로 적용한다. 착수 직전 대상 경로/영역 목록이 2개 이상이면 `dg suggest-sublead --scopes <path1,path2,...>`를 호출해 비겹침 그룹 수와 추천 병렬 배치(워커/sublead 수·상한)를 확인한다(DB 없이 즉시 판정).

**경량 사전 탐색(Pre-Coder Explore) 분업 계약**: 중량 코더(`glm-5.4 high`·`devin`·`codex` 등)가 코드베이스 탐색에 15~20분을 소진하지 않도록, 브리프에 `파일:줄번호` 앵커나 `## Context Pack`이 없으면 `dg pipeline init`이 자동으로 `ExploreRunning` 위상을 선행 활성화한다(`--with-explore` 강제 / `--skip-explore` 생략). 경량 읽기전용 `explore` 워커(`--tools read,grep,find,ls --exclude-tools edit,write`, 5분 상한, fail-open)가 `.herdr/context/<fork_id>-map.md`에 수정 대상 파일·줄번호·테스트 경로를 압축 기록하고, 코더 프롬프트에 자동 주입해 코더가 첫 턴부터 바로 구현에 착수하게 한다. `dg dispatch`도 쓰기 브리프에 코드 앵커가 없으면 `suggestExploreFirst: true`를 반환한다.

1. **착수 전 브리핑 (`mistology`)** *(선택 — 설치된 경우에만 실행, 없으면 스킵)* — 코드 수정/디버깅 전, 해당 저장소에 `mistology`가 있으면 1회 조회:
   ```bash
   mistology search -b "<대상 파일·심볼·작업 의도>"
   ```
2. **작업 라우팅·배차 (`dg dispatch` / `dg route`)** — 기본은 로컬 통계(`DECISION_API=off`, 0원). 비교 검증 시에만 `--decision-api shadow` 사용. 전송 범위·멱등·전환 평가 양식은 `docs/decision-gate-references.md` 계약을 따른다:
   ```bash
   # 리허설 (DB 미오염, 모델·API 호출 0회)
   dg dispatch \
     --task-kind <explore_read|code_search|single_edit|multi_edit|test_or_deploy> \
     --repo <TARGET_REPO> --dry-run

   # 실제 디스패치 계획 + fork 발급 (shadow 옵션)
   dg dispatch \
     --task-kind <explore_read|code_search|single_edit|multi_edit|test_or_deploy> \
     --repo <TARGET_REPO> [--decision-api shadow] [--context "<작업 요약>"] --json
   ```
   - `sharp`: 읽기전용 탐색 워커(`pi deepseek`, `--tools read,grep,find,ls --no-approve`)로 직행.
   - `split`: 격리 `git worktree` 코더(`devin`) + 다른 모델 계열 독립 검증자(`pi deepseek`/`glm-5.3`).
   - **하드 가드 불변**: `requires_write=true`, `forced_split=true`, `--risk medium|high`는 API 응답으로 절대 우회할 수 없다.
3. **워커 감시 실행 (`dg wrap`)** — 워커 명령을 `dg wrap`으로 감싸 멈춤(`--stall-ms`)·하트비트·산출물·통계를 자동 기록:
   ```bash
   dg wrap \
     --route <file_select|tool_select|retry_or_abort|delegate_swarm> \
     --tool-class <file_read|code_search|file_edit|shell_exec|swarm_dispatch> \
     --fork <FORK_ID> --project <TARGET_REPO> --require-stdout-nonempty -- <CMD...>
   ```
   - **고아 실행 금지**: 워커를 `nohup`/`disown`으로 분리하지 않는다 — `hc run --bg` 또는 부모 세션에 매인 `dg wrap`으로 띄워 부모 PID 체인(TUI 소유 매핑)을 유지한다.
   - **워커 역할·label 필수**: `dg wrap`에는 역할을 `--role <lead|sublead|explore|coder|verifier|advisor>`로 **명시**한다(오타는 즉시 실패). 표시명 `--label <역할>-<주제>`(예: `verifier-stage1`·`research-hcdesign`·`advisor-g3`·`sublead-auth`)도 함께 붙인다 — TUI 역할 카드 배치·이름·텔레메트리 매핑의 근거. `--role`이 없으면 라벨 **앞부분의 접두**(`sublead`/`sub`, `verifier`/`verify`/`neon`/`검증`, `explore`/`exp`/`research`/`탐색`/`조사`, `advisor`/`carbon`/`자문`)로 판정하므로 역할어를 주제 가운데에 쓰지 않는다(`fix-rust-verifier-observations`는 coder). 둘 다 없으면 coder/`wrap-<pid>` 폴백.
   - **tool_class canonical**: `--tool-class`는 5종만 쓴다(`file_read|code_search|file_edit|shell_exec|swarm_dispatch`) — 역할명(coder·verifier 등) 금지, 역할은 `--role`(보조로 `--label` 접두)이 담당한다(통계 파편화 방지).
   - **대기·보고**: 워커 대기는 `hc wait`(sleep·폴링 금지), 워커 지시문에는 완료 보고 `hc send $HC_TERMINAL_ID`를 명시한다.
   - **장기 자율 실행**: 무인 루프는 done-means 술어·탈출구·결정 로그를 갖춘다 — `docs/autonomous-run-contract.md` 계약 따름.
   - **검증 역귀속**: 독립 검증 판정은 구조화 verdict 파일(`.herdr/verify/<fork_id>-verdict.json`)로 남기고 `dg backprop --verdict-file <path>`로 원본 fork에 반영한다(pass/rework/fail/human) — 검증 결과가 다음 배차 통계에 귀속된다. **`pass`도 반드시 적용한다**: PASS를 파일로만 남기면 fork가 `closed_by='verifier'`로 닫히지 않고 `neon_verdict`도 NULL이라 교차검증이 실제로 일어났는지 DB로 셀 수 없다.
   - **Sublead 분리 제안**: 리드가 다중 트랙 조정 병목(재작업 반복·verdict 처리 지연·비겹침 영역 2개 이상)을 체감할 때 `dg suggest-sublead --project <TARGET_REPO>`로 3축 DB 신호(부하·영역 분리·작업 범위, LLM 호출 0) 기반 sublead 분리 여부·추천 수·상한을 판단한다. 착수 전 사전 판정은 `--scopes <p1,p2,...>`로 DB 없이 즉시 확인한다. sublead 세션(`HERDR_ROLE=sublead`)은 머지·main 푸시가 기본 차단되며 완료 보고는 `READY_PR` 핸드오프로 최상위 리드에 전달한다.
   - **하이브리드 세션 수명 정책 (`dg suggest-session`)**: 코더를 작업마다 새로 띄울지(에페머럴) 프롬프트 캐시 적중을 위해 Warm 재사용할지는 감이 아니라 `dg suggest-session`으로 0토큰 결정론적 판정한다:
     - **역할 불변식**: `verifier` · `advisor`는 확증 편향 차단을 위해 규모와 무관하게 **항상 에페머럴(새 세션)**.
     - **투입 전 3초 필터**: `door=one-way`, 예상 수정 파일 ≥ 6개, 직전과 다른 도메인, 동일 도메인 대기 작업 < 3건 중 하나라도 해당하면 **에페머럴**; 모두 통과(소형 · 동일 도메인 · 3건+ 대기)할 때만 **Warm 재사용**(`warm_reuse`).
     - **실행 중 자동 리셋선**: 연속 실패/rework ≥ 2회, 컴팩션 1회 발생(캐시 prefix 깨짐), 누적 5작업/35턴 도달, 또는 `decision.db` 실측 1차 통과율 < 70%(표본 ≥ 3) 시 즉시 리셋(`should_reset=true`).
   - **검증 pi 워커 격리**: `pi --no-session` 검증 워커는 `PI_CODING_AGENT_DIR=~/.pi/agent-verify` 환경으로 실행한다(최소 확장 — dg-collect만 포함, SoL-Pi 등 세션 의존 확장 충돌 회피).
   - **자문 3관문 (Layer 4)**: ① 계획 확정 전(코더 투입 직전) ② 같은 오류 2회째(3회 재시도 전 — 연속 실패 split 강제 신호와 동일 트리거) ③ 최종 머지 전(완료 전) — 3곳에서만 자문 세션을 온디맨드로 띄운다. 호출: `hc run --bg --name advisor-g<N>-<주제>` + 로스터 advisor 모델, 자료만 제시하고 판정 마커 단일 응답(`[advisor:pass]` / `[advisor:challenge:<이유>]`), 판정 후 즉시 `hc kill; hc rm` 정리. 계획 관문 pass 전 코더 투입 금지, 완료 관문 pass 없이는 최종 머지 금지(pass는 `.herdr/verify/`에 기록해 검증 역귀속과 같은 흐름으로 남긴다). **완료 관문 물리 집행**: 리드 세션 생성 명령에 운영자가 직접 `HERDR_MERGE_GATE=1`을 박는다 — `hc run --bg --name lead-<미션> --cwd <레포> -- env HERDR_MERGE_GATE=1 <리드CLI> …`(리드 자발 부착 금지 — 소프트 한계). 그러면 가드가 `gh pr merge`·main/master 직접 push를 pass 없으면 차단한다(pass 파일명 `advisor-pass*`, 유효창 30분).
    - **워커 위임 원칙 (서브에이전트 금지)**: dgate로 진행하는 하위 작업(탐색·코딩·검증·자문)은 에이전트 내장 서브에이전트(`invoke_subagent` 등)로 위임하지 말고, 반드시 `dg wrap`으로 감싼 뒤 `hc run --bg`로 띄운 워커로 위임한다 — `dg wrap` 없이 CLI를 직접 `hc run`하는 것은 금지(stall 감시·텔레메트리 누락).
      - 이유: 내장 서브에이전트는 리드와 같은 모델·쿼터를 쓰고(roster 역할별 모델·`agy` 0회 규칙 무력화), 격리 worktree가 없으며, `hc`/프로세스에 흔적이 없어 dgate TUI·GUI에 보이지 않는다(관측·통계·`dg backprop` 불가).
      - 워커는 역할별 모델(roster.json)·격리 worktree·`--stall-ms` 감시·`fork` 통계가 모두 적용되고, `dgate-tui`/`dgate-gui` Workers 카드에 CLI·모델·effort·경과 시간이 실측으로 표시된다.
      - 예외: 파일 1~2개를 읽고 결론만 필요한 극히 작은 질의는 리드가 직접 처리한다(서브에이전트도 쓰지 않는다).
      - **Claude Code 탐색 안전망 (`Explore.md` 4줄 오버라이드)**: `claude` CLI 환경에서는 기본 내장 `Explore` 서브에이전트가 Opus로 `Grep/Read/Glob`을 실행해 토큰을 낭비한다. `~/.claude/agents/Explore.md`(또는 레포 `.claude/agents/Explore.md`)에 `name: Explore / model: haiku / effort: low / tools: Read, Grep, Glob`을 직접 두면 탐색이 자동으로 경량 모델로 강등된다.
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
  - 전역 설치(`npm run install:global`, 일회성) 후에는 어떤 저장소에서든 `dgate-tui`(전체 보기 `dgate-tui --all`)·`dgate-gui`·`dg`·`dgate`로 실행한다.
  - 백그라운드 모니터 세션 접속 *(선택 — `hc` 설치 시)*: `hc attach dg-monitor` (나오기: `Ctrl+\`)
  - 직접 실행: `dgate-tui --watch-db ~/.local/state/dgate/decision.db` (미빌드 시 `cargo run -p dgate-tui -- --watch-db ~/.local/state/dgate/decision.db`)
- **비용·타이밍 통계 (읽기전용)**:
  ```bash
  dg stats --cost --timing [--project <TARGET_REPO>]
  ```
- **규칙 후보 + Decision API Shadow 비교 리포트 (읽기전용)**:
  ```bash
  dg report [--project <TARGET_REPO>]
  ```
- **실행 중 워커 목록** *(선택 — `hc` 설치 시)*: `hc ls`

---

## 5. 안전·보안 가드레일

- **키 관리** *(선택 — Decision API 사용 시에만)*: `GEMINI_DECISION_API_KEY`는 `~/.config/herdr-quad/secrets.env`(`chmod 600`, `~/.zshrc` 자동 로드)에서만 읽는다. 저장소 내 커밋·출력 금지.
- **기본 무과금**: `DECISION_API` 기본값은 `off`. `on` 모드 전환이나 실 유료 워커 대량 투입, `git commit`/`push`는 사용자 승인 후 실행한다.
- **`agy` 쿼터 절약** *(선택 — `agy` 워커 사용 시)*: 전역 `~/.config/herdr-quad/roster.json` 기본 프로필(`quota-saver`, 상시 `agy` 0턴)을 유지한다.
