# dgate — 실측 통계 기반 멀티 에이전트 오케스트레이터

5개 역할 슬롯(lead·explore·coder·verifier·advisor)에 임의의 로컬 CLI 에이전트(claude·codex·devin·pi·gemini·agy 및 cli: "custom"으로 임의 로컬 CLI 전부 지원)를 바인딩해, **실측 통계 기반 Decision Gate**가 저비용 경로와 리드 승격 경로를 자동 분기하는 오케스트레이션 시스템이다. 핵심 목적:

- **비용·리드 턴 절감** — 통계적으로 안정된 조사 태스크는 Pre-Lead 라우터가 읽기전용 워커로 직행시켜 리드 LLM 턴을 0으로 만든다.
- **모델 가변성** — 어떤 모델을 어느 역할에 쓸지는 `roster.json` 설정 파일 하나로 교체한다. 코드 수정 불필요.
- **로컬 전체 프로젝트 공용** — `--repo <path>`로 이 머신의 어느 로컬 저장소에서든 같은 오케스트레이터를 실행한다.

결정 엔진은 SQLite(WAL) 위에 EWMA Beta 사후확률을 유지하고, **동일 조건 연속 오류 2회 시 강제 `split`**(리드 승격)으로 안전하게 폴백한다. 외부 npm 의존성은 0개 — Node.js 내장 모듈만으로 동작한다.

> **Rust v2 네이티브 CLI & GPUI 데스크톱 앱** — 같은 SQLite DB·roster.json을 공유하는 네이티브 구현이 Cargo 워크스페이스(`crates/dgate-core`·`crates/dgate-cli`)로 준비돼 있다. `cargo build --release` 한 번으로 `target/release/dg`·`dgate` 네이티브 바이너리(~3.7ms 기동, Node 대비 약 24배 빠름)를 얻고, 선택적으로 `cargo build -p dgate-gui`로 GPUI 데스크톱 관제앱도 띄울 수 있다. 자세한 것은 [§7](#7-rust-v2-네이티브-cli--선택적-gpui-데스크톱-관제앱).

---

## 1. 아키텍처 요약 (Pre-Lead Gate + 5계층)

```text
                    ┌────────────────────────────────────────────┐
  태스크 입력 ─────▶ │ Layer 0 · Pre-Lead Router (dg, LLM 0턴)    │
                    │ sharp = 읽기전용 fast-path / split = 리드 승격 │
                    └───────────────┬────────────────────────────┘
              sharp (p≥0.7, 저위험)  │  split
                    ┌───────────────▼────────────────────────────┐
                    │ Layer 1 · Lead — 계획 수립·워커 브리프 발행    │
                    ├────────────────────────────────────────────┤
                    │ Layer 2 · Swarm — 읽기전용 탐색 + worktree    │
                    │            코딩 워커 (동시 최대 2)            │
                    ├────────────────────────────────────────────┤
                    │ Layer 3 · 독립 검증자 — 이종 모델·읽기전용.    │
                    │            [FAIL] → dg backprop → 재작업     │
                    ├────────────────────────────────────────────┤
                    │ Layer 4 · 온디맨드 자문 3관문 (계획/오류/완료) │
                    └────────────────────────────────────────────┘
```

- **Layer 0 (`dg` Pre-Lead Router)** — 태스크 종류 7종(`explore_read`·`code_search`·`single_edit` 등)별 사전확률과 실측 사후확률을 비교해 `sharp`(읽기전용 탐색 워커 직행(기본 pi 등 roster 설정 CLI), 리드 턴 0) 또는 `split`(리드 승격)을 판정한다. LLM 호출 없이 순수 통계 판정이다.
- **Layer 1 (Lead)** — `split`된 태스크의 계획을 수립하고 Layer 2 워커 브리프를 발행한다. `HERDR_DB_WRITE=1`로 결정 통계를 기록한다.
- **Layer 2 (Swarm)** — 읽기전용 탐색 워커(동시 최대 4)와 `git worktree`에서 격리된 코딩 워커(동시 최대 2, 서로 다른 파일 영역)가 병렬 작업한다.
- **Layer 3 (독립 검증자)** — 리드와 이종 모델·읽기전용 권한으로 검증한다. 판정 SSOT는 구조화 verdict JSON(`.herdr/verify/<fork_id>-verdict.json`)이고, 리드가 `dg backprop --verdict-file`로 pass/rework/fail/human을 원본 fork에 fail-closed 반영한다(§5).
- **Layer 4 (자문 3관문)** — 계획·오류·완료 시점에만 온디맨드로 기동하는 자문 세션이다. 관문 통과 후 즉시 `hc kill; hc rm`으로 정리된다.

세션 관제·런치는 `hc`(Herdr 터미널 CLI) 위에서 동작한다. 모든 통계는 `~/.local/state/dgate/decision.db`(SQLite)에 SSOT로 축적된다.

---

## 2. 요구사항 & 빠른 시작

### 요구사항

- **Node.js 22.6+** (`node:sqlite` 내장 모듈 필요 — 외부 npm 의존성 0개, `npm install` 불필요)
- 워커로 사용할 로컬 CLI 에이전트 중 최소 1종 (claude / codex / devin / pi / gemini / agy 또는 cli: "custom"으로 임의 터미널 CLI — 없으면 `--dry-run` 및 로컬 dg 통계·체크포인트 단독 사용)
- (선택) `hc`·`herdr-app` — 세션 기동·4분할 관제 화면용

### 단독 실행 (외부 도구·API 키 없이 3줄 시작)

**Node.js >= 22.6 하나만 있으면 된다.** npm 의존성 0개, 외부 스킬·워커 CLI·API 키 전부 불필요 — clone 후 바로:

```bash
node ./src/decision/dg.ts route --task-kind explore_read          # dg route — sharp/split 라우팅 판정 (LLM 0턴)
node ./src/decision/dg.ts wrap --route tool_select --tool-class shell_exec -- echo hi  # dg wrap — 명령 감시 래핑
node ./src/decision/dg.ts stats --cost --timing && node ./src/decision/dg.ts report    # dg stats / report — 비용·타이밍 통계, 규칙 후보 리포트
```

- (선택) 실시간 TUI 모니터: `npm run dgate-tui` (기본값 현재 세션 관측, 기본 결정 DB가 있으면 자동 연결 · 전체 세션/프로젝트는 `npm run dgate-tui -- --all`)
- (선택) GPUI 데스크톱 관제앱: `npm run dgate-gui` (Rust 빌드 필요 — [§7](#7-rust-v2-네이티브-cli--선택적-gpui-데스크톱-관제앱))
- (선택) 에이전트 스킬로 연동하려면: [skills/dgate/SKILL.md](skills/dgate/SKILL.md)

### 빠른 시작

```bash
git clone https://github.com/insightflo/dgate.git && cd dgate

# 0) 환경 진단·온보딩 — 보유 CLI·사용 가능 모델 감지, 역할별 추천 + 맞춤 roster 설정
npm run setup            # 대화형 위저드 (환경 진단·로스터 맞춤 설정·선택 도구 안내)
npm run setup:check      # 읽기전용 진단 리포트만 (파일 쓰기 0 — 안전 점검용)

# 1) 오케스트레이션 계획 리허설 (모델 호출 0회, 실기동 0회 — 무조건 안전)
npm run orchestrate:dry

# 2) 전체 테스트 171건 (외부 의존성 0개)
npm test

# 3) 데모 TUI 스냅샷 (터미널에 관제 화면 정적 출력)
npm run snapshot

# 4) 4분할 데모 오케스트레이션 (Herdr App 필요)
npm run herdr:quad
```

### `npm run setup` — 0토큰 온보딩 위저드

**모델 호출 없이(0토큰)** 설치된 CLI 에이전트와 로컬 모델을 자동 감지하고, 역할별 성능 가이드와 선호 모델 피커를 보여주며 맞춤 roster를 완성한다. 사용자 환경과 비교해 선택 동반 스킬(mistology·NeverStuck·todo-flow·Effort-Router)을 추천하고, 옵트인 GitHub 별점 안내도 여기서 나온다.

### 자연어 스킬 플로우

AI 채팅에서 `dgate로 진행해줘` 한 줄이면 끝난다: 조용한 환경·안전 설정 → 사전 평가(preflight) 모델 판정 + 저장소별 모니터 승인 → 워커 실행·교차 검증 → 쿼터 초과·스톨 시 워크트리를 보존하는 페일오버.

> **TUI 관측 vs 제어**: `npm run watch`/`--watch` 관제 화면의 `[읽기전용 관측 · WATCH]` 배지는 **읽기전용 라이브 대시보드**다 — DB·분기 확률·로그를 실시간으로 보여줄 뿐, 화면 자체가 작업을 실행하거나 제어하지 않는다. 실제 태스크 제어는 AI 채팅에서 `dgate` 스킬로 하거나 별도 셸에서 `dg`(또는 `scripts/dispatch.ts`) 명령으로 수행한다. 와이어 애니메이션은 work/워커가 실제로 실행 중일 때만 움직이며, 대기 상태에서는 멈춰 있다.

---

## 3. 모델·구독 가변 설정 (`roster.json` & 환경변수)

역할↔모델 바인딩은 전부 외부 설정으로 분리돼 있다. **구독·쿼터·결제 상황이 바뀌면 설정 파일만 교체한다.**

### roster.json 탐색 우선순위

```text
--roster <path> 플래그  >  HERDR_ROSTER_PATH 환경변수  >  <repo>/.herdr/roster.json
>  ~/.config/herdr-quad/roster.json (전역 기본)  >  내장 기본값 (quota-saver)
```

### 설정 예시

[`.herdr/roster.example.json`](.herdr/roster.example.json)을 참고해 자신의 환경에 맞게 복사·편집한다:

```bash
mkdir -p ~/.config/herdr-quad
cp .herdr/roster.example.json ~/.config/herdr-quad/roster.json
# 이후 구독 변동 시 이 파일의 bin·provider·model·dailyTurnBudgets만 수정
```

```jsonc
{
  "defaultProfile": "quota-saver",
  "profiles": {
    "quota-saver": {
      "roles": {
        "lead":     { "cli": "pi", "provider": "opencodex", "model": "<lead-model>", "thinking": "max" },
        "explore":  { "cli": "pi", "provider": "opencodex", "model": "<cheap-model>", "thinking": "medium", "readonly": true },
        "verifier": { "cli": "pi", "provider": "opencodex", "model": "<verifier-model>", "thinking": "max", "readonly": true },
        "advisor":  { "cli": "pi", "provider": "opencodex", "model": "<advisor-model>", "thinking": "high", "readonly": true, "onDemand": true, "quotaBucket": "pi" }
      }
    }
  }
}
```

- 미지정 필드는 내장 기본값을 상속한다(부분 오버라이드 허용).
- `"cli": "custom"` — 임의의 로컬 터미널 CLI를 워커로 바인딩한다 (예: codex / opencode / aider 등 무엇이든). `bin`에 명령을, `extraArgs`에 고정 인자를 지정한다:

```jsonc
"coder": { "cli": "custom", "bin": "aider", "extraArgs": ["--no-auto-commits"] }
```
- `bin`은 포터블한 명령 이름(`"pi"` 등)을 권장한다. 절대경로가 필요할 때만 명시.
- 실행 파일 해석 순서: `bin` 필드 → 환경변수 → `PATH` 탐색 → 관례적 설치 위치(`~/.bun/bin/pi`, `~/.local/bin/agy`).
- `pricing.relativeCost`(선택) — 역할별 상대비용 가중치. `dg route`/`dg gate`에 `--roster`/`--profile`을 주면 상수 0.70 대신 비용 인식 동적 `θ_sharp`를 산출한다(§5). 미지정 시 슬롯 기본값.

### 환경변수

| 변수 | 용도 |
| :--- | :--- |
| `DGATE_DB_PATH` | 결정 엔진 SQLite DB 경로 (기본 `~/.local/state/dgate/decision.db`, 레거시 별칭 `DECISION_ORCHESTRATOR_DB_PATH`·`GEMINI_TRICK_DB_PATH`) |
| `HERDR_ROSTER_PATH` | roster.json 경로 오버라이드 |
| `PI_BIN` / `HERDR_PI_BIN` | `pi` 실행 파일 경로 오버라이드 |
| `AGY_BIN` / `DEVIN_BIN` / `HC_BIN` / `CLAUDE_BIN` | 각 CLI 실행 파일 경로 오버라이드. `"cli": "custom"`은 자체 `bin` 필드(미지정 시 `PATH` 탐색)로 실행 파일을 해석하므로 별도 환경변수가 필요 없다 |
| `HERDR_AGY_TURN_BUDGET` | agy 일일 턴 예산 (기본 20 — 초과 시 프로필 자동 강등) |
| `DECISION_API` | Decision API 모드 — `off`(기본, 네트워크 0) / `shadow`(병행 평가·로컬 불변) / `on`(콜드스타트 `basis:prior`만 API 판정 적용) |
| `GEMINI_DECISION_API_KEY` | Decision API 인증 키 — `x-goog-api-key` 헤더로만 전송(URL `?key=` 미사용). 부재·공백 시 `shadow`/`on`도 `off`로 강등 |
| `DECISION_API_BASE_URL` | 엔드포인트 오버라이드 (기본 `https://generativelanguage.googleapis.com`, `POST {base}/v1beta/decisions`) |
| `DECISION_API_MODEL` | 모델 오버라이드 (기본 `gemini-decision-1`) |
| `DECISION_API_TIMEOUT_MS` | 요청 타임아웃 ms (기본 3000, 50–30000 클램프) |

`dg route`/`dg gate`/`scripts/dispatch.ts`는 `--decision-api off|shadow|on`(env 덮어쓰기), `--context <text>`(리댁션 후 프롬프트 포함), `--api-eval`(measured 기반에서도 평가 강제) 플래그를 지원한다. 모든 실패는 로컬 EWMA로 fail-safe 폴백하고, 감사 결과는 `forks.metadata_json.decision_api`에 기록돼 `dg report`의 `decisionApiShadow` 요약으로 집계된다. `decision_events`에는 아무것도 기록되지 않는다(로컬 EWMA 오염 0).

> **API 키는 절대 이 저장소에 커밋하지 않는다.** 키 주입은 환경변수로만 하고, `.env*` 파일은 `.gitignore`로 차단돼 있다.

### 내장 프로필

| 프로필 | 구성 | 비고 |
| :--- | :--- | :--- |
| `quota-saver` (기본) | lead/verifier/advisor 모두 `pi` (advisor는 온디맨드 1회성) | agy 상시·온디맨드 모두 0 |
| `agy-quad` (opt-in) | lead·verifier·advisor 모두 `agy` 상시 | 일일 예산 초과 시 `quota-saver`로 자동 강등 |

---

## 4. 타 로컬 저장소 공용 실행 (`--repo`) & `dg` CLI

저장소 단위 다중 task·PR 위임 개발 플레이북(PRD 비겹침 분해 → 소유주 첫 task 시범 → 워크트리 병렬 PR 위임 · main 병합 승인 게이트 · CI 비용 통제)은 [docs/WORKFLOW.md](docs/WORKFLOW.md)를 참고한다 (단순 질문·즉석 수정에는 적용을 강제하지 않는다).

### 다른 저장소에서 실행

`dg.ts`·역할 계약서·roster는 오케스트레이터 루트 기준, 작업 cwd·worktree·project_id는 `--repo` 기준으로 해석된다. 어느 프로젝트에서든 동일하게 동작한다:

```bash
node ./scripts/herdr-quad-launch.ts --repo /path/to/your-project --mode orchestrate --dry-run
```

### `dg` 서브커맨드 (15종)

```bash
node ./src/decision/dg.ts <command> [flags]
```

| 명령 | 용도 |
| :--- | :--- |
| `route` | 태스크 종류별 `sharp`/`split` 라우팅 판정 (LLM 턴 0). `--roster`/`--profile` 지정 시 비용 인식 동적 `θ_sharp` 적용 |
| `gate` | 게이트 진입 시 사후확률·강제 조건 평가 (`--roster`/`--profile` 동적 `θ_sharp` 연동) |
| `close` | fork 종료 처리 (`--outcome ok\|fail\|rerouted`) |
| `touch` | fork TTL 갱신 (워커 하트비트) |
| `record` | 도구 사용 결과 표본 기록 (EWMA 입력) |
| `backprop` | 검증 `[FAIL]`을 원본 fork에 역전파 — 수동 또는 `--verdict-file` 구조화 판정 적용(fail-closed) |
| `attempt` | fork `generation`·`attempt` 펜스 조회·증가 (`--bump-gen`, DDL v2) |
| `retro` | fork 회고 마크다운(상태·원인·실패 도구·권고)을 다음 워커 브리프용으로 생성 |
| `settle-orphans` | TTL 만료 fork 일괄 정산 |
| `wrap` | 워커 커맨드 래핑 — 워치독 감시·산출물 계약·원자 커밋으로 SSOT 기록 (§5) |
| `stats` | 비용·실행/대기 시간 통계 (`--cost`·`--timing`·`--days`·`--db`) — 완전 읽기전용, 구간 합집합으로 병렬 중복 제거 |
| `checkpoint` | 워크트리 체크포인트 `create\|verify\|restore` — git 패치+untracked 스냅숏, sha256 manifest 검증 |
| `worktree` | `worktree remove` — 체크포인트 검증 통과 전 삭제 물리 차단 (`exit 4`, 우회 불가) |
| `report` | 읽기전용 규칙 후보 리포트 (`--days`·`--out`) — `--out`은 `<repo>/.herdr/report/` 또는 저장소 외부만 허용 |
| `compact` | 결정 DB 압축·통계 정리 |

---

## 5. 보안·격리 설계

- **fail-closed 읽기전용 가드** — `.agents/hooks.json` + `.agents/hooks/dg-hook.ts`가 `HERDR_READONLY=1` 워커의 도구 호출을 사전 차단한다. 개행·제어문자 주입, 위험 플래그(`--dangerously-skip-permissions` 등), 쓰기·위임 도구를 차단하고, 가드 자체 오류 시에도 차단(fail-closed)한다.
- **fail-open 수집** — `PostToolUse` 텔레메트리 수집은 본령이 아니므로 실패해도 워커를 막지 않는다(fail-open).
- **읽기전용 워커 도구 제한** — `pi` 워커는 `--tools read,grep,find,ls`로 기동되며 bash·편집 도구가 계약상 존재하지 않는다. 사용자 roster가 임의 `--tools`를 지정해도 `--exclude-tools edit,write`가 자동 부여된다.
- **세션명 충돌 방지** — 모든 `hc run --name`은 `^[a-z0-9_-]{1,32}$` 계약 + 런타임 6-hex 접미로 유일성을 보장하고, 기동 전 `hc ls` 충돌 점검을 수행한다.
- **dry-run 무오염** — `--dry-run`은 임시 `:memory:` DB에서 리허설해 사용자 결정 DB·파일 시스템을 건드리지 않는다.

### 실행 수명주기 & `dg wrap` 워치독

`dg wrap`이 워커 자식을 `detached` 프로세스 그룹으로 띄워 종료 코드·신호·출력 tail을 함께 수집하고, 결과를 `commitWrapOutcome` 단일 트랜잭션으로 SSOT에 기록한다(이중 표본·TOCTOU 차단). `--expected-gen`/`--expected-attempt` 펜스로 세대·시도 불일치의 stale 커밋을 거부하고, 디스패처는 `repo_scope` cooperative lease로 동일 저장소를 점유 중인 세션과의 중복 디스패치를 미룬다(점유자 생존·만료 시 탈취, 조회 실패는 fail-open). 주요 계약:

- **산출물·출력 계약** — `--require-artifact <path>`(파일 존재), `--require-nonempty`(크기 >0), `--require-stdout-nonempty`(비공백 stdout)를 clean exit(0) 시 AND 게이트로 검사한다. stdout 첫 비공백 줄이 `rerouted`면 워커의 리드 승격 신호 — 표본·실패 없이 `rerouted`로만 닫힌다.
- **워치독** — `--stall-ms`(기본 120s) 무출력 스톨 시 프로세스 그룹 전체 SIGTERM→SIGKILL kill + 리댁션된 20줄 tail 보존, `--max-ms`(기본 20분) 총 시간 상한. 출력 진전이 있을 때만 `dg touch` 하트비트(`--heartbeat-ms`, 기본 60s)로 fork TTL을 갱신한다.
- **`FailureCause` 11종** — `ok_pass`·`transient_network`·`tool_error`·`max_turns`·`no_result`·`artifact_missing`·`gate_fail`·`stall`·`timeout`·`config_error`·`interrupted`. 신호·종료코드 우선 분류라 문자열 휴리스틱 오탐을 막는다. `config_error`·`interrupted`는 EWMA·연속오류 카운터에서 격리돼 감사 로그만 남고, `ttl_orphan` 고아 정산은 w=1.0 실패 표본을 강제해 생존편향을 차단한다.

### Fail-Closed 구조화 Verdict & `dg retro`

- **`.herdr/verify/<fork_id>-verdict.json`** — 검증자가 남기는 SSOT 판정 문서(`schema_version: 1`). `decision`은 `pass|rework|fail|human` 4종, `findings[].severity`는 `P0|P1|P2|info`. `dg backprop --verdict-file <path> [--expected-head <sha>] [--max-reworks 2]`가 3중 불변식(스키마·fork_id/HEAD 결속·모순 판정)으로 검증해 fork에 반영한다 — `pass`는 fork→`ok`, `fail`은 역전파, `rework`는 재작업 카운트+w1.0 페널티, `human`/무효·누락·Stale·Replay는 전부 `exit 3` + `waiting_human` 파킹이다. pass에 P0/P1 finding이 있거나 fail/rework에 비공백 evidence가 없으면 모순으로 거부된다.
- **`dg retro --fork <id> [--out <path>]`** — fork의 상태·원인·시도·재작업 횟수·실패 도구 상위 5건·다음 시도 권고를 리댁션된 마크다운으로 생성해 다음 워커 브리프에 주입한다(opt-in — 자동 생성하지 않는다).

### DDL v2 무중단 마이그레이션·비용 인식 라우팅·시간 통계

- **`PRAGMA user_version = 2` 자동 승격** — v1 결정 DB를 열 때 `VACUUM INTO` 백업 후 단일 `BEGIN IMMEDIATE` 트랜잭션 안에서 `forks` 리빌드·백필·버전 승격을 수행한다(이후 핫패스는 비용 0). v2는 `generation`/`attempt` 펜스와 실행·대기 구간 테이블 `fork_intervals`를 추가하며, `dg attempt --fork <id> [--bump-gen]`이 세대·시도를 관리한다.
- **비용 인식 동적 `θ_sharp`** — roster의 `pricing.relativeCost`(미지정 시 슬롯 기본값)와 쿼터 희소도 승수 `κ_quota`(일일 예산 50% 초과분부터 제곱 증가, 100% 시 2.0)로 `θ_sharp = clamp((C_eff(explore)+C_switch)/(C_eff(lead)+C_switch), 0.70, 0.90)`을 산출한다 — 비싼 탐색 워커 프로필은 자동 보수 분기. `cost_status` 집계는 `unknown` 표본을 $0으로 위장하지 않는다.
- **`dg stats` 실행/대기 시간 분리** — `fork_intervals`의 exec/wait/human 구간을 합집합으로 합산해 병렬 워커 중복을 제거한다(`--cost` 비용 집계, `--timing` 구간 통계, `--db` 임의 DB 지정). 읽기전용 연결로 어떤 쓰기 경로도 호출하지 않는다.

### Git-Native 워크트리 체크포인트 & 삭제 게이트

- **`dg checkpoint create|verify|restore`** — worktree의 staged/unstaged `git diff --binary` 패치와 untracked 원본(파일당·총합 12MiB 상한)을 `.herdr/checkpoints/<run_id>/<wt_name>/`에 스냅숏하고 sha256 manifest로 무결성을 검증한다. 경로 순회·제외 대상(`.git`·`node_modules`·`.herdr`·`.verify`·`*.db*`)은 저장·검증·복원에서 동일하게 차단하고, 심볼릭 링크는 대상 경로 문자열만 기록한다.
- **`dg worktree remove` 삭제 게이트** — "체크포인트 저장 성공 + verify 유효"를 통과하기 전에는 `git worktree remove` 호출에 물리적으로 도달할 수 없다(우회 플래그 없음, 거부 시 `exit 4`).

### 읽기전용 규칙 후보 리포트

- **`dg report [--days 14] [--out <path>]`** — `decision_events`/`forks` 통계에서 사람이 검토할 규칙 후보 4종(도구 반복·핑퐁·FailureCause·고아/스톨)을 구조화 JSON+마크다운으로만 출력한다. 읽기전용 연결 + `PRAGMA query_only` 이중 보장이며, 프롬프트·역할 파일·코드·DB 자동 수정 기능은 존재하지 않는다(자동 변조 영구 금지). `--out`은 `<repo>/.herdr/report/` 내부 또는 저장소 외부 절대경로만 허용한다(allowlist, 심볼릭 링크 우회 차단).

### 시크릿 3중 방어

- **헤더 전용 키 전달** — Gemini REST 폴백은 `x-goog-api-key` 헤더로만 키를 보낸다(`?key=` URL 쿼리 폐기 — URL·로그·프록시 기록에 키가 남지 않는다).
- **자식 env 최소 격리** — `buildSanitizedChildEnv`가 자식 워커 env에서 `*_API_KEY|_TOKEN|_SECRET|_PASSWORD|_PRIVATE_KEY` 계열을 제거하고 안전 키(`PATH`·`HOME`·`HERDR_*` 등)+명시 allow 목록만 통과시킨다. `dg wrap --sanitize-env --allow-env K1,K2`로 동일 격리를 래핑 자식에 적용한다.
- **전역 리댁션** — `redactSecrets`(무예외 순수 함수)가 런타임 env 실측값·URL 인코딩 형태와 패턴 세트(`Authorization`/`x-goog-api-key` 헤더, `?key=` 쿼리, Bearer, `AIza…`·`sk-…`·`ghp_…`·`AKIA…` 등)를 `<redacted>`로 치환한다. 로그·`error_snippet`·`command_line`·verdict/retro 출력 등 기록 경로 전체에 적용된다. `tests/redact-security.test.ts`의 키 모양 문자열은 리댁션 검증용 의도된 합성 더미다.

---

## 6. DGATE TUI 관제 화면

읽기전용 라이브 관제 TUI다. `roster.json` 변경을 핫리로드로 반영하고, 글로벌 watch 모드에서는 활성 `hc` 워커의 모델과 `[repo]` 태그가 붙은 1줄 리덕션 활동을 실시간으로 보여준다. 저장소 단위 스코프는 `--repo <dir>`로 — cwd 기준 워커 필터·project_id 기준 DB 필터가 적용된다. 감시 중인 호스트 Lead 라벨은 Antigravity / Claude Code / Codex 실제 값을 정직하게 표시하고, 좌측 상단 Watch/Control 모드 배지·`?` 평이한 한국어 도움말 모달, 대기 조용한(idle-quiet) 와이어로 구성된다.

```bash
npm run demo          # 40초 데모 시나리오 (모델 호출 없음)
npm run live          # 대화형 라이브 모드
npm run watch         # 실행 중인 세션 트랜스크립트·결정 DB 실시간 관제 (dgate-tui의 별칭)
npm run dgate-tui     # 기본 관제 TUI — 현재 세션 기준, 기본 결정 DB 자동 연결 (`--all`로 전체 세션/프로젝트)
npm run dgate-gui     # GPUI 데스크톱 관제앱 (cargo run -p dgate-gui)
node ./src/cli.ts --watch --repo /path/to/your-project            # 저장소 단위 모니터 (--repo <dir>)
npm run snapshot      # 현재 상태 정적 스냅샷 출력
agy --output-format stream-json --print "…" | node ./src/cli.ts --stream   # JSONL 파이프 관제
```

주요 조작: `Tab` 패널 포커스 이동 · `Enter` 상세 모달 · `r` 응답 전문 · `s`/`n`/`c` 스웜·검증·자문 모달 · `/` 프롬프트 입력 · `?` 도움말. JSONL 스트림 프로토콜은 [examples/sample-session.jsonl](examples/sample-session.jsonl) 참고.

---

## 7. Rust v2 네이티브 CLI & 선택적 GPUI 데스크톱 관제앱

Node v1과 **동일한 SQLite DB·`roster.json`을 완전 공유**하는 Rust v2 구현. Cargo 워크스페이스로 코어·CLI·GUI가 크레이트 단위로 분리돼 있다.

### Cargo 워크스페이스 분리 — CLI는 GUI 컴파일 무게 0

```toml
default-members = ["crates/dgate-core", "crates/dgate-cli"]
```

- **CLI/헤드리스 빌드는 GUI 의존성을 전혀 컴파일하지 않는다.** `dgate-gui`는 별도 opt-in 멤버다.
- `cargo build --release` → 네이티브 `target/release/dg` · `target/release/dgate` 바이너리 생성.
- **~3.7ms 기동 / Node 대비 약 24배 빠름** — 콜드스타트가 중요한 래핑·통계·루팅 호출에 유리하다.
- **SQLite DDL v2 100% 호환** — Node v1이 만든 `decision.db`를 그대로 읽고 쓴다. 마이그레이션·변환 없이 v1↔v2 혼용 가능.
- 편의 스크립트: `npm run rust:build` / `npm run rust:test` (npm 의존성은 여전히 0개).

### 선택적 GPUI 네이티브 데스크톱 관제앱 (`crates/dgate-gui`)

[gpui 0.2.2](https://github.com/longbridge/gpui) 기반 · Metal GPU 가속 네이티브 데스크톱 앱. 완전 선택 사항이다:

```bash
npm run gui:build    # = cargo build -p dgate-gui
npm run dgate-gui    # = cargo run -p dgate-gui --
# 또는 직접:
cargo run -p dgate-gui -- [--repo <dir>] [--db <path>] [--roster <path>] [--watch-only] [--poll-ms 2000]
```

하이라이트:

- **저장소별 탭** — 여러 `[--repo]`를 한 창에서 전환하며 관제
- **5-역할 로스터 카드** — lead·explore·coder·verifier·advisor 바인딩·모델 한눈에
- **hc 워커 실시간 활동** — 활성 워커당 1줄 리덕션(민감정보 마스킹) 스트림
- **Decision Gate 확률 바 + 비용/타이밍/shadow 요약** — `dg stats`/`report` 데이터를 시각화
- **2단계 명시 확인 워커 정지** — 안전한 worker stop(실수 방지 이중 확인)
- **조용한 대기 상태** — 유휴 시 과잉 애니메이션 없는 차분한 화면

### Node v1 ↔ Rust v2 테스트·호환성

| 구현 | 테스트 | 비고 |
| :--- | :--- | :--- |
| Node v1 | 171건 | `npm test` — 외부 의존성 0개 |
| Rust v2 | 274건 (core 248 + cli/교차호환 9 + gui 17) | `cargo test` — 동일 SQLite DB·`roster.json` SSOT 공유 |

두 구현은 **정확히 같은 SQLite DB와 `roster.json`**을 쓰므로 어떤 CLI로 기록해도 다른 쪽에서 그대로 읽힌다.

---

## 라이선스·크레딧

- **라이선스** — 이 저장소는 [MIT License](LICENSE)로 배포된다.
- **TUI 원조** — 관제 TUI는 [leecg39/Gemini_trick](https://github.com/leecg39/Gemini_trick)에서 왔다. 본 저장소는 그 오케스트레이션 후속으로, Decision Gate·roster·읽기전용 격리 계약을 통합해 일반화했다. 참고로 원조 저장소에는 현재 명시된 LICENSE 파일이 없다.
- **설계 영감** — LoopForge의 실행 가드레일·체크포인트·텔레메트리 패턴에서 설계 영감을 받았다.
- **선택 동반 스킬** (각 저장소 라이선스 지배):
  - [chldbwnstm/mistology](https://github.com/chldbwnstm/mistology)
  - [chldbwnstm/NeverStuck](https://github.com/chldbwnstm/NeverStuck)
  - [JakeB-5/todo-flow](https://github.com/JakeB-5/todo-flow)
  - [acc0mplish/Effort-Router](https://github.com/acc0mplish/Effort-Router)
