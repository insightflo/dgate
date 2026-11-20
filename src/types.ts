import type { AgentColorKey } from './theme.ts';

export type QuadRole = 'argon' | 'carbon' | 'neon' | 'hydrogen' | 'all';

export type EffortLevel = 'low' | 'medium' | 'high' | 'max';

export type FocusTarget = 'advisor' | 'main' | 'neon' | 'decision' | 'swarm' | 'logs';

export type ModalKind =
  | 'none'
  | 'help'
  | 'swarm'
  | 'advisor'
  | 'neon'
  | 'decision'
  | 'logs'
  | 'response';

export type CompactTab = 0 | 1 | 2 | 3; // 0: 파이프라인, 1: 스웜, 2: Advisor, 3: 세션 기록

export type LogFilter = 'all' | 'argon' | 'hydrogen' | 'decision' | 'neon' | 'carbon';

export type NeonStatus = 'idle' | 'run' | 'pass' | 'fail';

/** Decision 게이트 고정 라우트 열거형 (§4.1 — 4종만 허용) */
export type RouteId = 'file_select' | 'tool_select' | 'retry_or_abort' | 'delegate_swarm';

/** 사후확률 근거 표지: n_raw < N_min(=5) 구간은 prior, 이후 measured */
export type DecisionBasis = 'prior' | 'measured';

export type RunMode = 'demo' | 'stream' | 'live' | 'watch';

export type BackendKind = 'agy' | 'gemini-api' | 'none';

export interface NeonCheckRecord {
  time: string;
  label: string;
  status: 'run' | 'pass' | 'fail';
  summary: string;
  model: string;
}

export interface DecisionRoute {
  name: string;
  p: number; // 0.0 ~ 1.0 (p >= 0.70 => sharp, p < 0.70 => split)
  targetHint?: string;
  // SQLite SSOT 확장 선택 필드 (W1 — §5.3)
  id?: RouteId;
  basis?: DecisionBasis;
  samples?: number;
  consecutiveErrors?: number;
  forcedSplit?: boolean;
}

export interface SwarmCard {
  name: string;
  model: string;
  progress: number; // 0 ~ 100
  description: string;
  status: 'running' | 'done' | 'idle' | 'error';
  /** 라이브 워커 실측 경과 시간(초) — 렌더에서 MM:SS로 표시. 폴백 카드에는 없음. */
  elapsedSec?: number;
}

export interface SwarmWorker {
  id: number; // 1..50
  role: 'search' | 'grep' | 'audit' | 'index' | 'verify';
  target: string;
  progress: number;
  status: 'running' | 'done' | 'idle';
}

export interface AdviceRecord {
  time: string;
  triggerIndex: 0 | 1 | 2;
  triggerLabel: string;
  text: string;
  tokensRead: string;
}

export interface LogEntry {
  id: number;
  time: string;
  agent: string;
  color: AgentColorKey;
  message: string;
}

/**
 * 활성 roster.json 바인딩의 TUI 표시용 라벨 묶음.
 * 각 역할 슬롯 값은 `formatRoleBadge`가 만든 간결 태그
 * (예: 'pi:glm-5.3', 'pi:deepseek-v4.1-flash', 'devin', 'claude:sonnet-4-6').
 */
export interface RosterDisplayLabels {
  /** 활성 roster 프로필 이름 (예: 'zero-agy', 'quota-saver') */
  profileName: string;
  /** roster 로딩 출처: flag | env | project | global | builtin */
  rosterSource: string;
  lead: string;
  explore: string;
  coder: string;
  verifier: string;
  advisor: string;
}

export interface AppState {
  mode: RunMode;
  paused: boolean;
  simTime: number; // 초 단위 (데모는 0~40 순환, 라이브는 연속 증가)
  tickCount: number;

  // 세션 메타
  cwd: string;
  sessionId: number;
  /** `--repo <dir>` 스코프 지정 시 레포 표시명(예: 'Gemini_trick'). 미지정 null — watch 헤더는 '전체' 표기 */
  repoScopeLabel?: string | null;
  /** 활성 roster.json 역할·모델 바인딩 라벨 (리드·탐색·코더·검증·자문) */
  rosterLabels: RosterDisplayLabels;
  modelFlag: string;
  modelName: string;
  backendKind: BackendKind;
  conversationId: string | null;
  agentBusy: boolean;
  totalTokensUsed: number;
  consecutiveToolErrors: number;
  latestResponse: string;
  responseScrollOffset: number;

  // 상단 배지 & 메트릭
  argonBuilds: number;
  swarmActive: number;
  swarmMax: number;
  neonVerified: number;
  carbonCalls: number;
  totalForks: number;

  // Lead 메인 & 리턴 패널
  mainSummary: string;
  effort: EffortLevel;
  phase: string;
  backSummary: string;

  // Verifier 교차 검증 패널
  neonModelName: string;
  neonModelFlag: string;
  neonState: NeonStatus;
  neonProgress: number; // 0 ~ 100
  neonLabel: string;
  neonSummary: string;
  neonHistory: NeonCheckRecord[];

  // Advisor 자문 패널
  advisorState: '대기 중' | '자문 중';
  activeTrigger: -1 | 0 | 1 | 2; // -1: 비활성, 0: 계획 확정 전, 1: 같은 오류 2회, 2: 완료 전
  latestAdvice: string;
  tokensRead: string;
  adviceHistory: AdviceRecord[];

  // Decision Gate 분기 패널
  routes: DecisionRoute[];
  sharpCount: number;
  splitCount: number;

  // Workers 스웜 패널
  cards: SwarmCard[];
  /**
   * 라이브 hc 워커 카드(Live TUI Sync v2) — `--watch` 실시간 동기화 결과 표시용.
   * 길이 가변(0~N)이므로 트랜스크립트 3슬롯 계약을 쓰는 `cards`를 대체하지 않고
   * 별도 슬롯에 담는다. 렌더는 `liveWorkers?.length > 0`일 때 이 값을 우선 보여주고,
   * 없으면 기존 `cards`로 폴백한다.
   */
  liveWorkers?: SwarmCard[];
  workers: SwarmWorker[];

  // 세션 기록
  logs: LogEntry[];
  logScrollOffset: number; // 0이면 최신 줄 고정, 양수면 과거로 스크롤
  logFilter: LogFilter;

  // UI 인터랙션 상태
  focus: FocusTarget;
  inputActive: boolean;
  inputValue: string;
  statusFlash: string | null;
  statusFlashUntil: number;
  modal: ModalKind;
  compactTab: CompactTab;
}

/**
 * 실시간 JSONL 이벤트 스트림 프로토콜 (stdin 또는 --file 입력)
 */
export type StreamEvent =
  | {
      type: 'session';
      sessionId?: number;
      cwd?: string;
      model?: string;
      effort?: EffortLevel;
    }
  | {
      type: 'phase';
      phase: string;
      summary?: string;
      backSummary?: string;
      argonBuilds?: number;
    }
  | {
      type: 'decision';
      forks?: number;
      routes: Array<{
        name: string;
        p: number;
        targetHint?: string;
        id?: RouteId;
        basis?: DecisionBasis;
        samples?: number;
        consecutiveErrors?: number;
        forcedSplit?: boolean;
      }>;
    }
  | {
      type: 'swarm';
      active?: number;
      max?: number;
      cards?: Array<{
        name: string;
        model?: string;
        progress: number;
        description?: string;
        status?: 'running' | 'done' | 'idle' | 'error';
      }>;
    }
  | {
      type: 'advisor';
      trigger: -1 | 0 | 1 | 2;
      state?: '대기 중' | '자문 중';
      calls?: number;
      tokens?: string;
      advice?: string;
    }
  | {
      type: 'neon';
      state: NeonStatus;
      progress?: number;
      label?: string;
      verified?: number;
      summary?: string;
      model?: string;
    }
  | {
      type: 'log';
      time?: string;
      agent: string;
      color?: AgentColorKey;
      message: string;
    }
  | {
      type: 'reset';
    };
