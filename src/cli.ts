#!/usr/bin/env node
import * as fs from 'node:fs';
import * as path from 'node:path';
import * as tty from 'node:tty';
import { fileURLToPath } from 'node:url';
import {
  AgentRunner,
  type BackendPreference,
} from './backend/agentRunner.ts';
import {
  attachTranscriptWatcher,
  detectWatchedLeadLabel,
  findLatestTranscriptPath,
} from './backend/transcriptMapper.ts';
import {
  createLiveRosterSyncer,
  type LiveRosterSyncer,
  type LiveWorkerInfo,
  type MergedLiveRoster,
} from './backend/liveWorkerSync.ts';
import { parseKeyEvents, TerminalScreen, type KeyEvent } from './core/terminal.ts';
import {
  applyDemoTime,
  createDemoController,
  executePromptCommand,
  type LivePromptHooks,
} from './state/demoEngine.ts';
import {
  appendLog,
  applyRosterToState,
  createInitialState,
  cycleFocus,
  flashStatus,
  formatCompactPath,
  getFilteredLogs,
} from './state/store.ts';
import {
  attachFileStream,
  attachStdinStream,
  parseJsonLine,
} from './state/streamAdapter.ts';
import type { AppState, CompactTab, EffortLevel, ModalKind, QuadRole, RunMode } from './types.ts';
import { computeLayout } from './ui/layout.ts';
import { renderFrame } from './ui/renderApp.ts';
import { renderQuadRoleFrame } from './ui/renderQuadRole.ts';
import { QuadBusClient, QuadBusServer, type QuadMessage } from './ipc/quadBus.ts';
import { resolveDbPath } from './decision/db.ts';
import { loadRosterConfig, resolveRosterProfile } from './decision/roster.ts';
import { readTelemetrySnapshot } from './decision/telemetry.ts';

interface CliOptions {
  mode: RunMode;
  filePath: string | null;
  watchMode: boolean;
  watchDir: string | null;
  watchDb: string | null;
  /** `--repo <dir>`(별칭 `--project`) — 관측 스코프를 특정 레포로 한정 */
  repoDir: string | null;
  /** `--all` / `-a` — 전체 세션·레포 전역 관측 */
  all: boolean;
  /** `--session <id>` — 관측 스코프를 특정 hc 세션(주소)으로 한정 */
  sessionId: string | null;
  follow: boolean;
  replayMs: number;
  initialTime: number;
  paused: boolean;
  snapshotPlain: boolean;
  snapshotAnsi: boolean;
  colsOverride: number | null;
  rowsOverride: number | null;
  modalOverride: ModalKind;
  modelName: string | null;
  effort: EffortLevel | null;
  initialPrompt: string | null;
  backendPref: BackendPreference;
  autoApprove: boolean;
  showHelp: boolean;
  quad: boolean;
  quadRole: QuadRole | null;
  isHub: boolean;
}

function parseArgs(argv: string[]): CliOptions {
  const stdinPiped = !process.stdin.isTTY;
  const opts: CliOptions = {
    mode: stdinPiped ? 'stream' : 'demo',
    filePath: null,
    watchMode: false,
    watchDir: null,
    watchDb: null,
    repoDir: null,
    all: false,
    sessionId: null,
    follow: false,
    replayMs: 0,
    initialTime: 0,
    paused: false,
    snapshotPlain: false,
    snapshotAnsi: false,
    colsOverride: null,
    rowsOverride: null,
    modalOverride: 'none',
    modelName: null,
    effort: null,
    initialPrompt: null,
    backendPref: 'auto',
    autoApprove: false,
    showHelp: false,
    quad: false,
    quadRole: null,
    isHub: false,
  };

  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i]!;
    switch (arg) {
      case '--demo':
        opts.mode = 'demo';
        break;
      case '--live':
      case '--agy':
        opts.mode = 'live';
        break;
      case '--watch':
      case '-w':
        opts.mode = 'watch';
        opts.watchMode = true;
        opts.follow = true;
        if (argv[i + 1] && !argv[i + 1]!.startsWith('-')) {
          opts.filePath = argv[++i]!;
        }
        break;
      case '--watch-dir':
        opts.watchDir = argv[++i] ?? null;
        break;
      case '--watch-db': {
        // N8: SQLite 결정 DB 읽기전용 구독 경로. 우선순위 --watch-db > DGATE_DB_PATH > DECISION_ORCHESTRATOR_DB_PATH > GEMINI_TRICK_DB_PATH(레거시) > 기본 경로
        const p = argv[++i];
        if (p && !p.startsWith('-')) opts.watchDb = p;
        break;
      }
      case '--repo':
      case '--project': {
        // 관측 스코프를 특정 레포로 한정 — 헤더 repo=<이름> 표기, 해당 레포 워커·통계만 표시
        const p = argv[++i];
        if (p && !p.startsWith('-')) opts.repoDir = p;
        break;
      }
      case '--all':
      case '-a':
        opts.all = true;
        break;
      case '--session': {
        const s = argv[++i];
        if (s && !s.startsWith('-')) opts.sessionId = s;
        break;
      }
      case '--stream':
        opts.mode = 'stream';
        break;
      case '--file':
      case '-f':
        opts.mode = 'stream';
        opts.filePath = argv[++i] ?? null;
        break;
      case '--follow':
        opts.follow = true;
        break;
      case '--replay-ms':
        opts.replayMs = Number(argv[++i] ?? '350') || 350;
        break;
      case '--model':
      case '-m':
        opts.modelName = argv[++i] ?? null;
        break;
      case '--effort': {
        const eff = argv[++i];
        if (eff === 'low' || eff === 'medium' || eff === 'high' || eff === 'max') {
          opts.effort = eff;
        }
        break;
      }
      case '--prompt':
      case '-p':
        opts.initialPrompt = argv[++i] ?? null;
        if (opts.mode === 'demo' && !stdinPiped) {
          opts.mode = 'live';
        }
        break;
      case '--backend': {
        const b = argv[++i];
        if (b === 'auto' || b === 'agy' || b === 'api') {
          opts.backendPref = b;
        }
        break;
      }
      case '--auto-approve':
        opts.autoApprove = true;
        break;
      case '--time':
      case '--seek':
      case '-t':
        opts.initialTime = Number(argv[++i] ?? '0') || 0;
        break;
      case '--paused':
        opts.paused = true;
        break;
      case '--snapshot':
        opts.snapshotPlain = true;
        break;
      case '--ansi-snapshot':
        opts.snapshotAnsi = true;
        break;
      case '--cols':
        opts.colsOverride = Number(argv[++i] ?? '148') || 148;
        break;
      case '--rows':
        opts.rowsOverride = Number(argv[++i] ?? '43') || 43;
        break;
      case '--modal': {
        const m = argv[++i];
        if (
          m === 'help' ||
          m === 'swarm' ||
          m === 'advisor' ||
          m === 'neon' ||
          m === 'decision' ||
          m === 'logs' ||
          m === 'response'
        ) {
          opts.modalOverride = m;
        }
        break;
      }
      case '--quad':
        opts.quad = true;
        break;
      case '--role': {
        const r = argv[++i];
        if (r === 'argon' || r === 'carbon' || r === 'neon' || r === 'hydrogen' || r === 'all') {
          opts.quadRole = r;
          opts.quad = true;
        }
        break;
      }
      case '--hub':
        opts.isHub = true;
        break;
      case '--help':
      case '-h':
        opts.showHelp = true;
        break;
    }
  }
  return opts;
}

function printCliHelp(): void {
  const usage = `
DGATE TUI (Lead · Decision Gate · Workers · Verifier · Advisor)

사용법:
  node ./src/cli.ts [옵션]

실행 모드 옵션:
  --watch, -w [path]     실행 중인 agy/세션 트랜스크립트(transcript.jsonl) 자동 감지 및 실시간 관제
  --watch-dir <path>     트랜스크립트 검색 기본 디렉터리 지정 (기본: CLI ~/.gemini/antigravity-cli/brain 및
                         IDE ~/.gemini/antigravity/brain 중 최신 mtime 자동 선택)
  --watch-db <path>      SQLite 결정 DB(decision.db) 읽기전용 구독 경로 지정 (N8)
                         우선순위: --watch-db > DGATE_DB_PATH > DECISION_ORCHESTRATOR_DB_PATH > GEMINI_TRICK_DB_PATH(레거시)
                                  > ~/.local/state/dgate/decision.db
  --repo, --project <dir> 관측 스코프를 특정 프로젝트(레포)로 한정 — 헤더에 repo=<이름> 표기,
                         해당 레포의 활성 hc 워커·결정 통계만 표시 (기본: 전체 레포 관측)
  --session <id>         관측 스코프를 특정 hc 세션(주소)과 그 자식 워커로 한정
  --all, -a              관측 스코프를 전체 세션·레포(전역)로 확장 — 기본(현 세션/현 레포)의 반대
  --live, --agy          실제 Antigravity(agy) / Gemini API 백엔드 에이전트 루프 연결 실행
  --demo                 gemini_sub_neon.html의 40초 멀티 에이전트 시나리오 데모 실행 (기본값)
  --stream               표준 입력(stdin)으로부터 JSONL(또는 agy stream-json) 실시간 렌더링
  --file, -f <path>      지정한 JSONL 이벤트 로그 파일을 로드하여 렌더링
  --follow               --file 사용 시 파일에 추가되는 새 이벤트를 실시간 추적 (tail -f)
  --replay-ms <ms>       --file 사용 시 각 이벤트를 지정 간격(ms)으로 순차 재생

라이브 백엔드 옵션 (--live):
  --model, -m <name>     사용할 모델 지정 (기본: 활성 roster.json lead 바인딩)
  --effort <level>       추론 강도 지정 (low | medium | high | max)
  --prompt, -p <text>    시작과 동시에 실행할 초기 프롬프트 지정
  --backend <kind>       백엔드 선택 (auto | agy | api)
  --auto-approve         agy 실행 시 도구 권한 자동 승인(--dangerously-skip-permissions) 전달

타임라인 및 스냅샷 옵션:
  --time, -t <sec>       데모 시작 타임라인 초(0~40) 지정 (예: --time 10)
  --paused               일시정지 상태로 시작
  --snapshot             지정 해상도의 플레인 텍스트 프레임을 출력하고 종료
  --ansi-snapshot        지정 해상도의 24비트 ANSI 컬러 프레임을 출력하고 종료
  --cols <n> --rows <n>  스냅샷 렌더링 시 터미널 가로/세로 셀 크기 지정
  --help, -h             도움말 출력

단축키 요약:
  Tab / Shift+Tab        패널 포커스 이동
  Enter                  포커스된 패널 상세 모달 열기
  r / s / n / c / d / l  응답(r) · 스웜(s) · Verifier(n) · Advisor(c) · 분기(d) · 로그(l) 모달 열기
  1~5 / 0                역할별 로그 필터 (1:Lead, 2:Workers, 3:Decision, 4:Verifier, 5:Advisor, 0:전체)
  / 또는 i               하단 프롬프트 입력창 진입 (/model, /neon, /carbon, /new, /cancel 지원)
  Space / ← →            데모 일시정지·재생 및 타임라인 탐색
  ?                      인앱 도움말 모달 열기
  q / Ctrl+C             종료
`.trim();
  console.log(usage);
}

/** 라이브 역할 패널 동기화 추적 — 이 동기화가 덮어쓴 패널만 종료 시 깨끗하게 복귀시킨다. */
/** 실측 경과(초)를 MM:SS(1시간↑ HH:MM:SS)로 — 음수·NaN은 0 클램프 */
export function formatElapsedMmss(secs: number): string {
  if (!Number.isFinite(secs) || secs < 0) secs = 0;
  const total = Math.floor(secs);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const mm = String(m).padStart(2, '0');
  const ss = String(s).padStart(2, '0');
  return h > 0 ? `${String(h).padStart(2, '0')}:${mm}:${ss}` : `${mm}:${ss}`;
}

export interface LiveRolePanelTrail {
  phaseSet: boolean;
  /** false=미설정, 'run'=실행 중, 'pass'=완료 표시 중(소멸 시 idle 복귀 대기) */
  neonSet: boolean | 'run' | 'pass';
  advisorSet: boolean;
}

export function createLiveRoleTrail(): LiveRolePanelTrail {
  return { phaseSet: false, neonSet: false, advisorSet: false };
}

/**
 * Live 로스터 동기화 시 `merged.workers` 슬롯을 Lead/Verifier/Advisor 역할 패널에 반영한다.
 * - verifier 실행 → neonState='run'(부재 진행률 0 — 렌더에서 활성 바) + 1줄 활동 라벨
 * - advisor 실행 → advisorState='자문 중' + activeTrigger=0 (Advisor→Lead 와이어 펄스)
 * - lead 실행 → phase='작업 중', 그 외 워커 실행 → phase='워커 실행 중'(Lead 패널·와이어 펄스)
 * - 전부 종료 → 이 함수가 설정한 값만 '대기 중'/'idle'로 복귀(침착 대기 불변식 유지).
 * 트랜스크립트 매퍼가 쓰는 state.cards 3슬롯은 건드리지 않는다.
 */
export function applyLiveRolePanels(
  state: AppState,
  merged: MergedLiveRoster,
  trail: LiveRolePanelTrail
): void {
  const active = merged.workers.filter((w) => w.status !== 'done');

  // Verifier → 교차 검증(neon) 패널
  const verifier = active.find((w) => w.slot === 'verifier');
  if (verifier) {
    trail.neonSet = 'run';
    state.neonState = 'run';
    state.neonProgress = 0;
    state.neonLabel = `[${verifier.repoTag}] ${verifier.name}: ${verifier.activity}`;
  } else if (trail.neonSet === 'run') {
    // 실행 중이던 verifier가 완료로 남아 있으면 잠깐 'pass', 목록에서 사라지면 'idle' 복귀
    const verifierDone = merged.workers.some((w) => w.slot === 'verifier');
    if (verifierDone) {
      trail.neonSet = 'pass';
      state.neonState = 'pass';
      state.neonProgress = 100;
      state.neonVerified += 1;
    } else {
      trail.neonSet = false;
      state.neonState = 'idle';
      state.neonProgress = 0;
    }
    state.neonLabel = '';
  } else if (trail.neonSet === 'pass' && !merged.workers.some((w) => w.slot === 'verifier')) {
    // 'pass' 표시 중 verifier가 목록에서 사라지면 'idle' 복귀
    trail.neonSet = false;
    state.neonState = 'idle';
    state.neonProgress = 0;
    state.neonLabel = '';
  }

  // Advisor → 자문 패널 (트리거 0번 라인 활성 — Advisor→Lead 와이어 펄스)
  const advisor = active.find((w) => w.slot === 'advisor');
  if (advisor) {
    trail.advisorSet = true;
    state.advisorState = '자문 중';
    state.activeTrigger = 0;
    state.latestAdvice = advisor.activity;
  } else if (trail.advisorSet) {
    trail.advisorSet = false;
    state.advisorState = '대기 중';
    state.activeTrigger = -1;
  }

  // Lead → 위상 배지 (Lead 패널·Lead→Decision 와이어 펄스)
  const lead = active.find((w) => w.slot === 'lead');
  if (lead) {
    trail.phaseSet = true;
    state.phase = '작업 중';
  } else if (active.length > 0) {
    trail.phaseSet = true;
    state.phase = '워커 실행 중';
  } else if (trail.phaseSet) {
    trail.phaseSet = false;
    state.phase = '대기 중';
  }
}

/** resolveTuiScope 입력 — parseArgs 결과 중 스코프 판정에 쓰는 필드만. */
export interface TuiScopeOptions {
  all?: boolean;
  sessionId?: string | null;
  repoDir?: string | null;
}

/** resolveTuiScope 결과 — 관측 스코프 종류·레포 필터·세션 필터·헤더 라벨. */
export interface TuiScope {
  kind: 'all' | 'session' | 'repo';
  repoFilterDir: string | null;
  sessionId: string | null;
  scopeLabel: string | null;
}

/**
 * TUI 기본 관측 스코프 판정(순수 함수):
 * ① `--all`이면 전역 ② `--session <id>`면 해당 세션(+`--repo` 병행 가능)
 * ③ `--repo <dir>`면 해당 레포 ④ HC_TERMINAL_ID 환경변수가 있으면 현재 hc 세션
 * (라벨은 `레포명·세션ID`) ⑤ 마지막 폴백은 현재 cwd 레포.
 */
export function resolveTuiScope(
  opts: TuiScopeOptions,
  env: NodeJS.ProcessEnv,
  cwd: string
): TuiScope {
  if (opts.all) {
    return { kind: 'all', repoFilterDir: null, sessionId: null, scopeLabel: null };
  }
  if (opts.sessionId) {
    return {
      kind: 'session',
      repoFilterDir: opts.repoDir ? path.resolve(opts.repoDir) : null,
      sessionId: opts.sessionId,
      scopeLabel: 'session=' + opts.sessionId,
    };
  }
  if (opts.repoDir) {
    const dir = path.resolve(opts.repoDir);
    return {
      kind: 'repo',
      repoFilterDir: dir,
      sessionId: null,
      scopeLabel: path.basename(dir) || dir,
    };
  }
  const envSession = env.HC_TERMINAL_ID?.trim();
  if (envSession) {
    return {
      kind: 'session',
      repoFilterDir: null,
      sessionId: envSession,
      scopeLabel: path.basename(path.resolve(cwd)) + '·' + envSession,
    };
  }
  const dir = path.resolve(cwd);
  return {
    kind: 'repo',
    repoFilterDir: dir,
    sessionId: null,
    scopeLabel: path.basename(dir) || dir,
  };
}

function main(): void {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.showHelp) {
    printCliHelp();
    return;
  }

  const state = createInitialState(opts.mode);
  state.paused = opts.paused;
  state.modal = opts.modalOverride;

  // 관측 스코프 결정 — --all > --session > --repo > HC_TERMINAL_ID(현 세션) > cwd(현 레포).
  // 기본값이 과거의 전역 관측에서 “현재 세션(또는 현 레포)”로 바뀌었다(전역은 --all).
  const tuiScope = resolveTuiScope(opts, process.env, process.cwd());
  const repoFilterDir = tuiScope.repoFilterDir;
  if (tuiScope.scopeLabel) {
    state.repoScopeLabel = tuiScope.scopeLabel;
  }

  // 활성 roster.json 역할·모델 바인딩 반영 — 실패 시 createInitialState의
  // 내장(builtin) 라벨을 그대로 유지하는 안전 폴백.
  try {
    const loaded = loadRosterConfig({ repoDir: repoFilterDir ?? opts.watchDir ?? process.cwd() });
    const resolved = resolveRosterProfile(loaded.config, loaded.config.defaultProfile);
    applyRosterToState(state, resolved.name, loaded.source, resolved.profile);
  } catch {
    // 내장 라벨 폴백 유지
  }

  if (opts.modelName) {
    state.modelName = opts.modelName;
    state.modelFlag = `--model ${opts.modelName}`;
  }
  if (opts.effort) {
    state.effort = opts.effort;
  }

  // watch 모드 진실성: 관측 중인 transcript의 실제 호스트(Antigravity/Claude/Codex)를
  // Lead 카드 라벨로 반영한다. 미인식 경로면 roster.json 라벨을 복원한다.
  const rosterLead = state.rosterLabels.lead;
  const rosterModelName = state.modelName;
  const rosterModelFlag = state.modelFlag;
  // live 로스터 동기화기가 watchedLead를 존중해 lead 슬롯을 덮지 않게 하는 추적 변수
  let watchedLeadLabel: string | null = null;
  const applyWatchedLeadLabel = (transcriptPath: string | null): void => {
    const detected = detectWatchedLeadLabel(transcriptPath);
    watchedLeadLabel = detected?.leadBadge ?? null;
    state.rosterLabels.lead = detected?.leadBadge ?? rosterLead;
    state.modelName = detected?.modelName ?? rosterModelName;
    state.modelFlag = detected?.modelFlag ?? rosterModelFlag;
  };

  // N8: --watch-db > DGATE_DB_PATH > DECISION_ORCHESTRATOR_DB_PATH > GEMINI_TRICK_DB_PATH(레거시) > 기본 경로
  // 우선순위로 SQLite 결정 DB 읽기전용 구독.
  // watch 모드에서 스냅샷·데모가 아니면 기본 경로 DB가 존재할 때 자동 연결한다
  // (DB가 없으면 만들지 않고 건너뛴다 — 스냅샷/데모 테스트 결정성 유지).
  // W1은 최초 1회 로드만 수행한다(주기 구독 훅은 W2-B가 src/state에 추가).
  const decisionDbPath = resolveDbPath(opts.watchDb);
  const dbExplicitlyRequested =
    opts.watchDb !== null ||
    process.env.DGATE_DB_PATH ||
    process.env.DECISION_ORCHESTRATOR_DB_PATH ||
    process.env.GEMINI_TRICK_DB_PATH;
  const defaultDbAutoConnect =
    opts.mode === 'watch' && !opts.snapshotPlain && !opts.snapshotAnsi;
  if ((dbExplicitlyRequested || defaultDbAutoConnect) && fs.existsSync(decisionDbPath)) {
    try {
      // `--repo` 지정 시 해당 레포의 projectId로 스코프(경유 cwd → 내부 projectId 환산)
      const snapshot = readTelemetrySnapshot(
        decisionDbPath,
        repoFilterDir ?? opts.watchDir ?? process.cwd()
      );
      state.routes = snapshot.routes;
      const splitCount = snapshot.routes.filter((r) => r.forcedSplit === true || r.p < 0.7).length;
      state.sharpCount = snapshot.routes.length - splitCount;
      state.splitCount = splitCount;
    } catch {
      // 텔레메트리 로드 실패 시 기존 라우트 유지(§3.5 안전 폴백)
    }
  }

  const runner =
    opts.mode === 'live'
      ? new AgentRunner({
          preference: opts.backendPref,
          autoApprove: opts.autoApprove,
        })
      : null;
  if (runner) {
    state.backendKind = runner.backend;
  }

  const demoCtrl = createDemoController();

  let baseWallMs = Date.now();
  let timeOffsetSec = opts.initialTime;

  const seekTo = (sec: number) => {
    timeOffsetSec = ((sec % 40) + 40) % 40;
    baseWallMs = Date.now();
    if (state.mode === 'demo') {
      applyDemoTime(state, timeOffsetSec, demoCtrl);
    } else {
      state.simTime = timeOffsetSec;
    }
  };

  if (opts.mode === 'demo') {
    applyDemoTime(state, opts.initialTime, demoCtrl);
  } else if (opts.watchMode || opts.mode === 'watch') {
    if (!opts.filePath) {
      opts.filePath = findLatestTranscriptPath(opts.watchDir ?? undefined);
    }
    applyWatchedLeadLabel(opts.filePath);
    if (opts.filePath && (opts.snapshotPlain || opts.snapshotAnsi)) {
      const stop = attachTranscriptWatcher(opts.filePath, state, () => {});
      stop();
      state.simTime = opts.initialTime;
    }
  } else if (opts.filePath) {
    // 스냅샷 모드일 경우 즉시 동기 로드, 라이브일 경우 아래에서 attach
    if (opts.snapshotPlain || opts.snapshotAnsi) {
      const stop = attachFileStream(opts.filePath, state, false, 0, () => {});
      stop();
      state.simTime = opts.initialTime;
    }
  } else if ((opts.snapshotPlain || opts.snapshotAnsi) && !process.stdin.isTTY) {
    const piped = fs.readFileSync(0, 'utf8');
    for (const line of piped.split(/\r?\n/)) {
      parseJsonLine(line, state);
    }
    state.simTime = opts.initialTime;
  }

  // 단발성 스냅샷 출력 모드
  if (opts.snapshotPlain || opts.snapshotAnsi) {
    const cols = opts.colsOverride ?? (opts.quadRole ? 80 : 148);
    const rows = opts.rowsOverride ?? (opts.quadRole ? 24 : 43);
    const frame =
      opts.quad && opts.quadRole && opts.quadRole !== 'all'
        ? renderQuadRoleFrame(state, opts.quadRole, cols, rows)
        : renderFrame(state, cols, rows);
    console.log(opts.snapshotAnsi ? frame.toAnsiFullFrame() : frame.toPlainString());
    return;
  }

  // 인터랙티브 라이브 TUI 루프 시작
  const screen = new TerminalScreen();
  const stdinIsTty = Boolean(process.stdin.isTTY);
  screen.enter(stdinIsTty);

  let stopFileWatcher: (() => void) | null = null;
  let checkNewSessionTimer: ReturnType<typeof setInterval> | null = null;
  let liveSyncer: LiveRosterSyncer | null = null;
  let extraTtyStream: tty.ReadStream | null = null;
  let busServer: QuadBusServer | null = null;
  let busClient: QuadBusClient | null = null;
  let isSyncedClient = false;

  const quitApp = () => {
    if (runner) {
      runner.cancel(state, () => {});
    }
    if (busServer) busServer.close();
    if (busClient) busClient.close();
    if (stopFileWatcher) stopFileWatcher();
    if (checkNewSessionTimer) clearInterval(checkNewSessionTimer);
    if (liveSyncer) liveSyncer.stop();
    if (extraTtyStream) {
      try {
        extraTtyStream.setRawMode(false);
        extraTtyStream.destroy();
      } catch {
        // 무시
      }
    }
    screen.leave();
    process.exit(0);
  };

  const renderNow = () => {
    const { cols: rawCols, rows: rawRows } = screen.getSize();
    const cols = opts.colsOverride ?? rawCols;
    const rows = opts.rowsOverride ?? rawRows;
    const buf =
      opts.quad && opts.quadRole && opts.quadRole !== 'all'
        ? renderQuadRoleFrame(state, opts.quadRole, cols, rows)
        : renderFrame(state, cols, rows);
    screen.present(buf);
    if (busServer) {
      busServer.broadcast({ type: 'state', state });
    }
  };

  // ---- Live 워커 로스터 동기화 (--watch) — 활성 hc 워커 배지·[repo] 태그·1줄 활동 ----
  const SLOT_LOG_COLOR = {
    lead: 'blue',
    explore: 'blue',
    coder: 'blue',
    verifier: 'neon',
    advisor: 'purple',
  } as const;
  const MATRIX_ROLE_BY_SLOT = {
    lead: 'index',
    explore: 'search',
    coder: 'grep',
    verifier: 'verify',
    advisor: 'audit',
  } as const;

  // 로그 중복 제거용 직전 워커 상태(주소 키)
  const prevLiveWorkers = new Map<
    string,
    {
      activity: string;
      finished: boolean;
      name: string;
      repoTag: string;
      shortModel: string;
      slot: LiveWorkerInfo['slot'];
      /** 주소 첫 관측 시각(ms) — 워커가 사라질 때까지 유지 */
      startedAtMs: number;
      /** done 전환 시 경과를 동결해 완료 카드에 그대로 노출 */
      frozenElapsedSec: number | null;
    }
  >();
  // 역할 패널 동기화가 덮어쓴 영역 추적 — 종료 복귀 판정에 사용
  const liveRoleTrail = createLiveRoleTrail();
  // liveWorkers 카드와 나란히 놓인 경과 계산 기준 — 폴 사이 틱에서 초 갱신용
  const liveElapsedBasis: ({ startedAtMs: number; frozenElapsedSec: number | null } | null)[] = [];

  const liveRosterStateSignature = (): string =>
    JSON.stringify([
      state.rosterLabels,
      state.modelName,
      state.modelFlag,
      state.neonModelName,
      state.neonModelFlag,
      state.swarmActive,
      state.liveWorkers,
      state.workers,
      state.logs.length,
    ]);

  const appendLiveWorkerLog = (w: LiveWorkerInfo, suffix?: string): void => {
    const base = `[${w.repoTag}] ${w.name} (${w.shortModel})`;
    appendLog(
      state,
      w.name,
      SLOT_LOG_COLOR[w.slot],
      suffix ? `${base} — ${suffix}` : `${base} — ${w.activity}`
    );
  };

  const applyLiveRosterUpdate = (merged: MergedLiveRoster): void => {
    const prevSig = liveRosterStateSignature();
    state.rosterLabels = merged.labels;
    state.neonModelName = merged.verifierModelName;
    state.neonModelFlag = merged.verifierModelFlag;
    // lead는 라이브 리드 워커가 있을 때만(관측 중 세션 배지 우선) 반영
    const hasLiveLead = merged.workers.some((w) => w.slot === 'lead');
    if (hasLiveLead && !watchedLeadLabel) {
      state.modelName = merged.leadModelName;
      state.modelFlag = merged.leadModelFlag;
    }

    // Workers 카드 — 제목: [repoTag] name · shortModel (repo 스코프 시 태그 생략), 본문: 1줄 활동.
    // 가변 길이 라이브 목록은 state.liveWorkers로 — state.cards는 transcriptMapper가 쓰는
    // 고정 3슬롯(tool 카테고리) 계약이므로 절대 덮어쓰지 않는다(레이아웃·렌더는 liveWorkers 우선).
    const scoped = Boolean(repoFilterDir);
    const nowMs = Date.now();
    // 주소별 경과 추적 — 첫 관측 startedAtMs 고정, done 시 경과 동결, 소멸 시 제거
    const elapsedByAddr = new Map<string, { startedAtMs: number; frozenElapsedSec: number | null }>();
    state.liveWorkers = merged.workers.map((w) => {
      const prev = prevLiveWorkers.get(w.address);
      const startedAtMs = prev?.startedAtMs ?? nowMs;
      const done = w.status === 'done';
      const liveElapsed = Math.max(0, Math.floor((nowMs - startedAtMs) / 1000));
      const frozenElapsedSec = done ? (prev?.frozenElapsedSec ?? liveElapsed) : null;
      elapsedByAddr.set(w.address, { startedAtMs, frozenElapsedSec });
      return {
        name: scoped ? `${w.name} · ${w.shortModel}` : `[${w.repoTag}] ${w.name} · ${w.shortModel}`,
        model: w.shortModel,
        progress: done ? 100 : 0,
        description: w.activity,
        status: done ? ('done' as const) : ('running' as const),
        elapsedSec: frozenElapsedSec ?? liveElapsed,
      };
    });
    state.swarmActive = merged.activeWorkerCount;
    liveElapsedBasis.length = state.liveWorkers.length;
    merged.workers.forEach((w, i) => {
      liveElapsedBasis[i] = elapsedByAddr.get(w.address) ?? null;
    });

    // 역할 패널(Lead/Verifier/Advisor) 라이브 활성화 — 워커 종료 시 깨끗한 대기 복귀
    applyLiveRolePanels(state, merged, liveRoleTrail);

    // 워커 매트릭스(s 모달) 앞 슬롯에도 live 상태 반영
    for (let i = 0; i < state.workers.length; i++) {
      const w = merged.workers[i];
      const cur = state.workers[i]!;
      if (w) {
        state.workers[i] = {
          ...cur,
          role: MATRIX_ROLE_BY_SLOT[w.slot],
          target: scoped ? w.name : `[${w.repoTag}] ${w.name}`,
          progress: w.status === 'done' ? 100 : 0,
          status: w.status === 'done' ? 'done' : 'running',
        };
      } else if (cur.status !== 'idle') {
        state.workers[i] = { ...cur, progress: 0, status: 'idle' };
      }
    }

    // 중복 제거된 세션 로그 — 등장·활동 변화·완료/종료 시에만 1줄씩
    const seen = new Set<string>();
    for (const w of merged.workers) {
      seen.add(w.address);
      const prev = prevLiveWorkers.get(w.address);
      const finished = w.status === 'done';
      if (!prev) {
        appendLiveWorkerLog(w);
      } else if (finished && !prev.finished) {
        appendLiveWorkerLog(w, '완료');
      } else if (w.activity !== prev.activity) {
        appendLiveWorkerLog(w);
      }
      prevLiveWorkers.set(w.address, {
        activity: w.activity,
        finished,
        name: w.name,
        repoTag: w.repoTag,
        shortModel: w.shortModel,
        slot: w.slot,
        startedAtMs: elapsedByAddr.get(w.address)?.startedAtMs ?? Date.now(),
        frozenElapsedSec: elapsedByAddr.get(w.address)?.frozenElapsedSec ?? null,
      });
    }
    for (const [addr, prev] of [...prevLiveWorkers]) {
      if (!seen.has(addr)) {
        prevLiveWorkers.delete(addr);
        appendLog(
          state,
          prev.name,
          SLOT_LOG_COLOR[prev.slot],
          `[${prev.repoTag}] ${prev.name} (${prev.shortModel}) — 종료`
        );
      }
    }

    // 실제 변경 시에만 재렌더 (매 틱 방송 중복 억제)
    if (liveRosterStateSignature() !== prevSig) renderNow();
  };

  // 실행 중 라이브 워커의 경과 초 갱신 — 폴을 기다리지 않고 매 틱 MM:SS를 살아 있게
  const refreshLiveElapsed = (): void => {
    const cards = state.liveWorkers;
    if (!cards || cards.length === 0) return;
    let changed = false;
    for (let i = 0; i < cards.length; i++) {
      const basis = liveElapsedBasis[i];
      const card = cards[i]!;
      if (!basis || card.status !== 'running') continue;
      const next = Math.max(0, Math.floor((Date.now() - basis.startedAtMs) / 1000));
      if (card.elapsedSec !== next) {
        card.elapsedSec = next;
        changed = true;
      }
    }
    if (changed) renderNow();
  };

  // 선행 버그 #1 수정: liveHooks를 Hub IPC 등록 이전에 선언해 핸들러에서 참조 가능하게 함
  const liveHooks: LivePromptHooks | undefined = runner
    ? {
        onLivePrompt: (text: string) => {
          void runner.runPrompt(text, state, renderNow);
        },
        onCarbonAsk: (q: string) => {
          runner.askCarbonAdvisor(q, state, renderNow);
        },
        onNeonVerify: (target: string) => {
          runner.askNeonVerify(target, state, renderNow);
        },
        onCancel: () => {
          runner.cancel(state, renderNow);
        },
        onNewSession: () => {
          state.conversationId = null;
          state.latestResponse = '';
          state.phase = '대기 중';
          appendLog(state, 'Lead', 'orange', '새 대화 세션이 초기화되었습니다');
          flashStatus(state, '새 대화 시작 (컨텍스트 초기화)');
          renderNow();
        },
      }
    : undefined;

  if (opts.isHub) {
    busServer = new QuadBusServer((msg) => {
      if (msg.type === 'action' && msg.action === 'prompt' && typeof msg.payload === 'string') {
        // 선행 버그 #1 수정: executePromptCommand 위치 인자 시그니처 준수 + 즉시 렌더
        executePromptCommand(msg.payload, state, demoCtrl, seekTo, quitApp, liveHooks);
        renderNow();
      }
    });
    void busServer.start().catch(() => {
      // 소켓 바인드 실패 시 단독 모드로 동작
      busServer = null;
    });
  } else if (opts.quad && opts.quadRole && opts.quadRole !== 'argon') {
    busClient = new QuadBusClient((msg) => {
      if (msg.type === 'state' && msg.state) {
        isSyncedClient = true;
        Object.assign(state, msg.state);
        renderNow();
      }
    });
    busClient.connect();
  }

  if (opts.mode === 'live' && runner) {
    if (runner.backend === 'none') {
      appendLog(
        state,
        'Advisor',
        'purple',
        '연결 가능한 백엔드(agy 또는 GEMINI_API_KEY)를 찾지 못했습니다'
      );
      flashStatus(state, 'agy 로그인 또는 GEMINI_API_KEY 설정이 필요합니다', 5000);
    } else {
      appendLog(
        state,
        'Lead',
        'orange',
        `백엔드 연결 준비 완료 (${runner.backend} · ${state.modelName})`
      );
      if (opts.initialPrompt) {
        void runner.runPrompt(opts.initialPrompt, state, renderNow);
      }
    }
  }

  // 트랜스크립트 실시간 감시 (watch 모드) 또는 일반 스트림 어댑터 연결
  if (opts.watchMode || opts.mode === 'watch') {
    if (!opts.filePath) {
      opts.filePath = findLatestTranscriptPath(opts.watchDir ?? undefined);
    }
    if (opts.filePath) {
      applyWatchedLeadLabel(opts.filePath);
      stopFileWatcher = attachTranscriptWatcher(opts.filePath, state, renderNow);
      state.cwd = formatCompactPath(path.dirname(path.dirname(path.dirname(opts.filePath))));
      const sessShort = state.conversationId ? state.conversationId.slice(0, 8) : 'active';
      appendLog(state, 'Lead', 'orange', `세션 트랜스크립트 관제 연결됨 (${sessShort})`);
      flashStatus(state, `실시간 관제 연결: 세션 ${sessShort} (transcript.jsonl)`, 5000);
    } else {
      flashStatus(state, '활성 세션 트랜스크립트를 찾지 못했습니다. agy 실행 후 다시 시도하세요.', 6000);
    }

    if (opts.watchMode) {
      // Live 워커 로스터 동기화(v2) — roster 핫리로드 + 활성 hc 워커 배지·[repo] 태그·1줄 활동
      liveSyncer = createLiveRosterSyncer({
        repoDir: repoFilterDir ?? opts.watchDir ?? process.cwd(),
        repoFilterDir,
        sessionId: tuiScope.sessionId ?? undefined,
        getWatchedLead: () => watchedLeadLabel,
        onUpdate: (merged) => applyLiveRosterUpdate(merged),
      });
      checkNewSessionTimer = setInterval(() => {
        const latest = findLatestTranscriptPath(opts.watchDir ?? undefined);
        if (latest && latest !== opts.filePath) {
          opts.filePath = latest;
          applyWatchedLeadLabel(latest);
          if (stopFileWatcher) stopFileWatcher();
          const sessShort = path.basename(path.dirname(path.dirname(latest))).slice(0, 8);
          flashStatus(state, `새 활성 세션 감지 및 자동 전환: ${sessShort}`);
          stopFileWatcher = attachTranscriptWatcher(latest, state, renderNow);
        }
      }, 2500);
    }
  } else if (opts.mode === 'stream') {
    if (opts.filePath) {
      stopFileWatcher = attachFileStream(
        opts.filePath,
        state,
        opts.follow,
        opts.replayMs,
        renderNow
      );
    } else if (!stdinIsTty) {
      attachStdinStream(state, renderNow);
      // 파이프 입력 중에도 로컬 터미널 키보드 조작이 가능하도록 /dev/tty 보조 연결 시도
      try {
        const ttyFd = fs.openSync('/dev/tty', 'r');
        extraTtyStream = new tty.ReadStream(ttyFd);
        extraTtyStream.setRawMode(true);
        extraTtyStream.on('data', (chunk: Buffer) => {
          for (const ev of parseKeyEvents(chunk)) {
            handleKey(ev);
          }
          renderNow();
        });
      } catch {
        // 비대화형 환경에서는 안전하게 무시
      }
    } else {
      flashStatus(state, 'LIVE STREAM 대기 중 (/help 로 사용법 확인)', 5000);
    }
  }

  const handleKey = (ev: KeyEvent) => {
    if (ev.kind === 'ctrl-c') {
      quitApp();
      return;
    }

    // 1. 프롬프트 입력 활성 모드 처리
    if (state.inputActive) {
      if (ev.kind === 'escape') {
        state.inputActive = false;
        return;
      }
      if (ev.kind === 'enter') {
        const submitted = state.inputValue;
        state.inputValue = '';
        state.inputActive = false;
        executePromptCommand(submitted, state, demoCtrl, seekTo, quitApp, liveHooks);
        return;
      }
      if (ev.kind === 'backspace') {
        const chars = Array.from(state.inputValue);
        chars.pop();
        state.inputValue = chars.join('');
        return;
      }
      if (ev.kind === 'char') {
        state.inputValue += ev.value;
        return;
      }
      return;
    }

    // 2. 모달이 열려 있을 때의 키 처리
    if (state.modal !== 'none') {
      if (ev.kind === 'escape' || (ev.kind === 'char' && ev.value === 'q')) {
        state.modal = 'none';
        return;
      }
      if (state.modal === 'logs') {
        const filteredLen = getFilteredLogs(state).length;
        if (ev.kind === 'arrow' && ev.dir === 'up') {
          state.logScrollOffset = Math.min(Math.max(0, filteredLen - 3), state.logScrollOffset + 1);
        } else if (ev.kind === 'arrow' && ev.dir === 'down') {
          state.logScrollOffset = Math.max(0, state.logScrollOffset - 1);
        }
      }
      if (state.modal === 'response') {
        if (ev.kind === 'arrow' && ev.dir === 'up') {
          state.responseScrollOffset = Math.max(0, state.responseScrollOffset - 1);
        } else if (ev.kind === 'arrow' && ev.dir === 'down') {
          state.responseScrollOffset += 1;
        } else if (ev.kind === 'page' && ev.dir === 'up') {
          state.responseScrollOffset = Math.max(0, state.responseScrollOffset - 10);
        } else if (ev.kind === 'page' && ev.dir === 'down') {
          state.responseScrollOffset += 10;
        } else if (ev.kind === 'char' && (ev.value === '/' || ev.value === 'i')) {
          state.modal = 'none';
          state.inputActive = true;
          if (ev.value === '/') state.inputValue = '/';
          return;
        }
      }
      if (ev.kind === 'char') {
        applyFilterShortcut(ev.value);
      }
      return;
    }

    // 3. 기본 관제 네비게이션 모드 처리
    if (ev.kind === 'tab') {
      cycleFocus(state, ev.shift);
      return;
    }

    if (ev.kind === 'enter') {
      switch (state.focus) {
        case 'advisor':
          state.modal = 'advisor';
          break;
        case 'main':
          state.inputActive = true;
          break;
        case 'neon':
          state.modal = 'neon';
          break;
        case 'decision':
          state.modal = 'decision';
          break;
        case 'swarm':
          state.modal = 'swarm';
          break;
        case 'logs':
          state.modal = 'logs';
          break;
      }
      return;
    }

    if (ev.kind === 'arrow') {
      if (ev.dir === 'up') {
        const maxOff = Math.max(0, getFilteredLogs(state).length - 3);
        state.logScrollOffset = Math.min(maxOff, state.logScrollOffset + 1);
      } else if (ev.dir === 'down') {
        state.logScrollOffset = Math.max(0, state.logScrollOffset - 1);
      } else if (ev.dir === 'left' && state.mode === 'demo') {
        const cur = state.simTime;
        state.paused = true;
        seekTo(cur - 2);
        flashStatus(state, `타임라인 ◀ t=${state.simTime.toFixed(1)}s (일시정지)`);
      } else if (ev.dir === 'right' && state.mode === 'demo') {
        const cur = state.simTime;
        state.paused = true;
        seekTo(cur + 2);
        flashStatus(state, `타임라인 ▶ t=${state.simTime.toFixed(1)}s (일시정지)`);
      }
      return;
    }

    if (ev.kind === 'char') {
      const ch = ev.value;
      if (ch === 'q') {
        quitApp();
        return;
      }
      if (ch === '?' || ch === 'h') {
        state.modal = 'help';
        return;
      }
      if (ch === '/' || ch === 'i') {
        state.inputActive = true;
        if (ch === '/') state.inputValue = '/';
        return;
      }
      if (ch === ' ') {
        if (state.mode === 'demo') {
          if (state.paused) {
            baseWallMs = Date.now();
            timeOffsetSec = state.simTime;
            state.paused = false;
            flashStatus(state, '▶ 데모 재생 재개');
          } else {
            timeOffsetSec = state.simTime;
            state.paused = true;
            flashStatus(state, '⏸ 데모 일시정지 (←/→ 탐색 가능)');
          }
        }
        return;
      }
      if (ch === 's') {
        state.modal = 'swarm';
        return;
      }
      if (ch === 'n') {
        state.modal = 'neon';
        return;
      }
      if (ch === 'c') {
        state.modal = 'advisor';
        return;
      }
      if (ch === 'd') {
        state.modal = 'decision';
        return;
      }
      if (ch === 'l') {
        state.modal = 'logs';
        return;
      }
      if (ch === 'r') {
        state.modal = 'response';
        return;
      }

      // Compact 레이아웃일 경우 1~4는 탭 전환을 겸함
      const { cols, rows } = screen.getSize();
      const curLayout = computeLayout(opts.colsOverride ?? cols, opts.rowsOverride ?? rows);
      if (curLayout.kind === 'compact' && (ch === '1' || ch === '2' || ch === '3' || ch === '4')) {
        state.compactTab = (Number(ch) - 1) as CompactTab;
        return;
      }

      applyFilterShortcut(ch);
    }
  };

  const applyFilterShortcut = (ch: string) => {
    if (ch === '0') {
      state.logFilter = 'all';
      state.logScrollOffset = 0;
      flashStatus(state, '로그 필터 해제 (전체 보기)');
    } else if (ch === '1') {
      state.logFilter = state.logFilter === 'argon' ? 'all' : 'argon';
      state.logScrollOffset = 0;
      flashStatus(state, `로그 필터: ${state.logFilter}`);
    } else if (ch === '2') {
      state.logFilter = state.logFilter === 'hydrogen' ? 'all' : 'hydrogen';
      state.logScrollOffset = 0;
      flashStatus(state, `로그 필터: ${state.logFilter}`);
    } else if (ch === '3') {
      state.logFilter = state.logFilter === 'decision' ? 'all' : 'decision';
      state.logScrollOffset = 0;
      flashStatus(state, `로그 필터: ${state.logFilter}`);
    } else if (ch === '4') {
      state.logFilter = state.logFilter === 'neon' ? 'all' : 'neon';
      state.logScrollOffset = 0;
      flashStatus(state, `로그 필터: ${state.logFilter}`);
    } else if (ch === '5') {
      state.logFilter = state.logFilter === 'carbon' ? 'all' : 'carbon';
      state.logScrollOffset = 0;
      flashStatus(state, `로그 필터: ${state.logFilter}`);
    }
  };

  if (stdinIsTty) {
    process.stdin.on('data', (chunk: Buffer) => {
      for (const ev of parseKeyEvents(chunk)) {
        handleKey(ev);
      }
      renderNow();
    });
  }

  process.stdout.on('resize', () => {
    screen.invalidate();
    renderNow();
  });

  // 80ms (약 12.5fps) 애니메이션 루프 — 와이어 펄스 및 시뮬레이션 시간 갱신
  setInterval(() => {
    if (isSyncedClient) return;
    state.tickCount += 1;
    if (state.mode === 'demo') {
      if (!state.paused) {
        const elapsed = (Date.now() - baseWallMs) / 1000;
        applyDemoTime(state, timeOffsetSec + elapsed, demoCtrl);
      }
    } else {
      // 라이브 스트림 모드에서도 와이어 흐름과 커서 점멸이 살아 움직이도록 시간 전진
      state.simTime += 0.08;
      refreshLiveElapsed();
    }
    renderNow();
  }, 80);

  renderNow();
}

// 모듈 테스트 임포트 시 main()이 실행되지 않도록 직접 실행(entry)일 때만 구동한다.
// entryPath가 없으면(node -e 등) 절대 구동하지 않는다.
const entryPath = process.argv[1] ? path.resolve(process.argv[1]) : null;
if (entryPath && entryPath === fileURLToPath(import.meta.url)) {
  main();
}

