/**
 * `src/backend/liveWorkerSync.ts` — 활성 `hc` 워커 터미널 로스터 실시간 동기화 (Live TUI Sync v2).
 *
 * 목적: `--watch` 관측 TUI가 roster.json 대기(standby) 라벨 대신 **지금 실제로 돌고 있는
 * `hc` 워커 세션**(pi/devin/claude/codex/gemini)의 모델·레포·1줄 활동을 보여주게 한다.
 * roster.json 이 바뀌면 mtime 감지로 핫리로드하고, `hc` 부재·오류는 조용히 폴백해
 * 관제 화면이 죽지 않는다(§3.5 안전 폴백 계약 준수).
 *
 * 외부 연결:
 * - `src/cli.ts` `--watch` 모드가 `createLiveRosterSyncer`로 주기 동기화를 시작한다.
 * - `hc ls --json`(터미널 목록) · `hc get <addr> --json`(argv 상세) ·
 *   `hc read <addr> --lines 4`(화면 마지막 줄) 세 명령만 쓴다(전부 읽기 전용).
 * - 라벨 조립은 `../state/store.ts`의 `formatRoleBadge`·`rosterLabelsFromProfile`,
 *   roster 로딩은 `../decision/roster.ts`의 `loadRosterConfig`·`resolveRosterProfile`,
 *   활동 줄 정화는 `../decision/redact.ts`의 `redactSecrets`를 재사용한다(단일 구현 원칙).
 *
 * 주의:
 * - 모든 `hc` 호출은 timeout을 붙인 비동기 실행이며 실패 시 해당 틱을 조용히 건너뛴다.
 * - 터미널 화면 원문에는 시크릿이 섞일 수 있으므로 활동 줄 추출 시 반드시
 *   `redactSecrets`를 경유한다(절단 이전 적용 — redact.ts 계약).
 * - `tsconfig` `erasableSyntaxOnly` — enum·클래스 파라미터 프로퍼티 금지.
 */
import { execFile } from 'node:child_process';
import * as fs from 'node:fs';
import * as path from 'node:path';
import { promisify } from 'node:util';
import { redactSecrets } from '../decision/redact.ts';
import {
  builtinRosterConfig,
  globalRosterPath,
  loadRosterConfig,
  resolveRosterProfile,
} from '../decision/roster.ts';
import { formatRoleBadge, rosterLabelsFromProfile } from '../state/store.ts';
import type { RosterDisplayLabels } from '../types.ts';

const execFileAsync = promisify(execFile);

/** `hc ls --json` 타임아웃 — 목록 조회는 짧게 끊는다. */
export const HC_LS_TIMEOUT_MS = 1500;
/** `hc read` 타임아웃 — 화면 캡처는 더 짧게 끊는다. */
export const HC_READ_TIMEOUT_MS = 800;
/** 기본 동기화 주기. */
export const DEFAULT_SYNC_INTERVAL_MS = 2000;
/** 틱당 `hc read` 활동 캡처 상한 — 터미널 많아도 TUI가 느려지지 않게 한다. */
export const MAX_ACTIVITY_READS_PER_TICK = 6;

// ---------- 형태 ----------

/** `hc ls --json` 터미널 항목(요약). */
export interface HcTerminalSummary {
  address: string;
  agent?: string;
  name?: string;
  parent?: string;
  state?: string;
  status?: string;
  cwd?: string;
}

/** `hc get <addr> --json` 터미널 항목(상세) — argv 포함. */
export interface HcTerminalDetail extends HcTerminalSummary {
  argv?: string[];
}

/** 역할 슬롯 — roster RoleSlot과 동일한 5종. */
export type LiveWorkerSlot = 'lead' | 'explore' | 'coder' | 'verifier' | 'advisor';

/** 현재 실행 중인 워커 1개의 TUI 표시 정보. */
export interface LiveWorkerInfo {
  slot: LiveWorkerSlot;
  /** 예: 'pi:glm-5.3 (실행 중)' · 'devin (실행 중)' */
  badge: string;
  /** 예: 'pi:glm-5.3' · 'devin' */
  shortModel: string;
  /** 예: 'glm-zcode/glm-5.3' · 'devin' (원문 모델명, 없으면 CLI명) */
  modelName: string;
  address: string;
  name: string;
  cwd: string | null;
  /** 레포 basename — `.herdr/worktrees/<wt>` 는 소속 레포명으로 환원 */
  repoTag: string;
  /** `hc read`에서 뽑은 정화된 1줄 활동 (없으면 간결 폴백) */
  activity: string;
  /** `hc` 터미널 상태('working'·'done' 등) — 카드 완료 표시·완료 로그 판정용(선택). */
  status?: string | null;
}

/** `mergeLiveRosterLabels` 결과 — cli.ts가 AppState에 반영할 값의 묶음. */
export interface MergedLiveRoster {
  labels: RosterDisplayLabels;
  leadModelName: string;
  leadModelFlag: string;
  verifierModelName: string;
  verifierModelFlag: string;
  workers: LiveWorkerInfo[];
  activeWorkerCount: number;
}

// ---------- 워커 터미널 판정 ----------

/** 제외할 셸·비워커 에이전트 (antigravity는 리드 관측 대상이지 워커가 아니다). */
const SHELL_AGENTS = new Set([
  'zsh',
  'bash',
  'fish',
  'sh',
  'ksh',
  'dash',
  'pwsh',
  'powershell.exe',
  'antigravity',
]);

/** parent 매칭 판정에 쓰는 워커 CLI 에이전트 목록. */
const PARENT_MATCH_AGENTS = new Set(['pi', 'devin', 'claude', 'codex', 'gemini']);

/** 워커 세션명 접두 — herdr-quad-launch/dispatch가 만드는 이름 규약. */
const WORKER_NAME_PREFIXES = [
  'w-',
  'exp-',
  'explore-',
  'coder',
  'verify-',
  'verifier-',
  'lead-',
  'advisor-',
  'carbon-',
] as const;

/** 모니터·관측 전용 세션명 — 워커 집계에서 제외. */
const MONITOR_NAME_RE = /^dg-monitor(-|$)/;

/** 최상위 활성 AI 에이전트 세션으로 인정하는 에이전트 집합 — 관측 대상 확장용. */
const TOP_LEVEL_AI_AGENTS = new Set([
  'antigravity',
  'pi',
  'claude',
  'codex',
  'devin',
  'gemini',
  'agy',
]);

/** 모니터·GUI 전용 세션명 — 최상위 세션 관측에서도 제외. */
function isMonitorOrGuiName(name: string): boolean {
  return MONITOR_NAME_RE.test(name) || name.startsWith('mon-') || name.startsWith('dgate-gui');
}

/**
 * 최상위(parent 없음) 활성 AI 에이전트 세션 판정 — 리드 관측 확장:
 * ① `state === 'live'` ② `status === 'working'` (엄격 — idle/done/undefined 불가)
 * ③ parent 부재·빈 값 ④ 모니터·GUI(dg-monitor-, mon-, dgate-gui- 접두) 제외
 * ⑤ agent(소문자)가 AI 코딩 에이전트 7종 안팎.
 */
export function isActiveTopLevelSession(t: HcTerminalSummary): boolean {
  if (!t || t.state !== 'live') return false;
  if (t.status !== 'working') return false;
  if (t.parent && t.parent.trim().length > 0) return false;
  if (isMonitorOrGuiName(t.name ?? '')) return false;
  const agent = (t.agent ?? '').toLowerCase();
  return TOP_LEVEL_AI_AGENTS.has(agent);
}

/** `hc ls` 터미널이 "관측 대상"인지 판정 — 기존 워커 규약 + 최상위 활성 AI 세션. */
export function isObservableTerminal(t: HcTerminalSummary): boolean {
  return isWorkerTerminal(t) || isActiveTopLevelSession(t);
}

/**
 * `hc ls` 터미널이 우리가 표시할 "워커"인지 판정:
 * ① `state === 'live'` 필수 ② 모니터(`dg-monitor*`·`mon-*`) 제외 ③ 셸·antigravity 제외
 * ④ 이름 접두(`w-`·`exp-`·`coder`·`verify-`…) 매칭 또는 parent 존재 + 워커 CLI 에이전트.
 */
export function isWorkerTerminal(t: HcTerminalSummary): boolean {
  if (!t || t.state !== 'live') return false;
  const name = t.name ?? '';
  if (MONITOR_NAME_RE.test(name) || name.startsWith('mon-')) return false;
  const agent = (t.agent ?? '').toLowerCase();
  if (SHELL_AGENTS.has(agent)) return false;
  if (name && WORKER_NAME_PREFIXES.some((p) => name.startsWith(p))) return true;
  if (t.parent && agent && PARENT_MATCH_AGENTS.has(agent)) return true;
  return false;
}

// ---------- 레포 태그·스코프 ----------

/** 격리 워크트리 마커 (경로 구분자 정규화 후 탐색). */
const WORKTREE_MARKER = '/.herdr/worktrees/';

/**
 * cwd → 레포 표시명. 빈 값은 'unknown'.
 * `/a/b/my-repo/.herdr/worktrees/coder1` 처럼 격리 워크트리면 소속 레포명
 * (`my-repo`)으로 환원하고, 그 외에는 resolve 후 basename.
 */
export function extractRepoTag(cwd: string | null | undefined): string {
  if (!cwd || cwd.trim().length === 0) return 'unknown';
  const norm = cwd.replaceAll('\\', '/');
  const markerIdx = norm.indexOf(WORKTREE_MARKER);
  if (markerIdx > 0) {
    const repoPart = norm.slice(0, markerIdx);
    const base = repoPart.split('/').filter(Boolean).pop();
    if (base) return base;
  }
  const base = path.basename(path.resolve(cwd));
  return base.length > 0 ? base : 'unknown';
}

/** 경로 해석 — realpath 우선, 실패(미존재 등) 시 resolve 폴백. */
function resolvePathBest(p: string): string {
  try {
    return fs.realpathSync(p);
  } catch {
    return path.resolve(p);
  }
}

/**
 * 워커 cwd가 `--repo <dir>` 스코프 안에 있는지 판정.
 * 필터 없음(null)이면 전역 관측(항상 true), 워커 cwd 미상이면 스코프 지정 시 false.
 * 워크트리 서브디렉터리(`.herdr/worktrees/…`)도 소속 레포 스코프에 포함된다.
 */
export function matchesRepoScope(
  workerCwd: string | null | undefined,
  repoFilterDir: string | null | undefined
): boolean {
  if (!repoFilterDir) return true;
  if (!workerCwd) return false;
  const workerPath = resolvePathBest(workerCwd);
  const scopePath = resolvePathBest(repoFilterDir);
  // 스코프가 루트('/')면 `scopePath + sep`가 '//'가 되어 전부 거짓이 되는 사고 방지 —
  // 이미 구분자로 끝나는 스코프는 그대로 접두사로 쓴다.
  const prefix = scopePath.endsWith(path.sep) ? scopePath : scopePath + path.sep;
  return workerPath === scopePath || workerPath.startsWith(prefix);
}

/**
 * 터미널이 세션 스코프 안에 있는지 판정.
 * sessionId 미지정(undefined)이면 전역(항상 true) — 기존 동작 100% 보존.
 * 지정 시 터미널 주소 자체 또는 parent(해당 세션이 띄운 자식 워커)가 일치할 때만 true.
 */
export function matchesSessionScope(
  t: HcTerminalSummary,
  sessionId?: string
): boolean {
  if (!sessionId) return true;
  return t.address === sessionId || t.parent === sessionId;
}

// ---------- 활동 줄 추출 ----------

const ANSI_OSC_RE = /\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g;
const ANSI_CSI_RE = /\x1b\[[0-9;?]*[ -/]*[@-~]/g;
const ANSI_SINGLE_RE = /\x1b[@-Z\\-_]/g;
const HC_STATUS_LINE_RE = /^\[hc\]/i;
const BOX_ONLY_LINE_RE = /^[─═┌┐└┘│├┤┬┴┼║╔╗╚╝╠╣╦╩╬\-_=·\s]+$/;
const SPINNER_ONLY_LINE_RE = /^[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏\s]+$/;
/** 줄 가장자리의 박스 장식(홑선·겹선 세로/모서리/교차) — 제거 후 본문을 재판정한다. */
const BOX_EDGE_RE = {
  leading: /^[\s│┌┐├┤║╔╗╠╣╦╩╬]+/,
  trailing: /[\s│└┘├┤║╚╝╠╣╦╩╬]+$/,
};

/** ANSI CSI/OSC 이스케이프 제거. */
export function stripAnsi(input: string): string {
  return input
    .replace(ANSI_OSC_RE, '')
    .replace(ANSI_CSI_RE, '')
    .replace(ANSI_SINGLE_RE, '');
}

/** 코드포인트 기준 절단 — 초과분은 `…` 한 글자로 마감. */
function truncateChars(s: string, maxChars: number): string {
  const chars = Array.from(s);
  if (chars.length <= maxChars) return s;
  return chars.slice(0, Math.max(1, maxChars - 1)).join('') + '…';
}

/**
 * 터미널 화면 원문에서 "의미 있는 마지막 1줄"을 뽑는다.
 * 아래에서 위로 훑으며 빈 줄·`[hc] …` 상태줄·박스/구분선·스피너 전용 줄은 건너뛴다.
 * 후보 줄은 가장자리 박스 장식(`│` 등)을 떼고, `redactSecrets`로 시크릿을.masking한 뒤
 * 공백을 붙이고 maxChars(기본 68)로 절단한다. 없으면 null.
 */
export function extractLastActivityLine(
  rawScreen: string | null | undefined,
  maxChars = 68
): string | null {
  if (typeof rawScreen !== 'string' || rawScreen.length === 0) return null;
  const lines = stripAnsi(rawScreen).split(/\r?\n/);
  for (let i = lines.length - 1; i >= 0; i--) {
    // 먼저 가장자리 박스 장식(│║ 등)을 떼고 판정 — '│ [hc] idle │' 같은 박스형 상태줄도
    // 스킵되고, '╚════╝' 같은 겹선 테두리는 본문 없음으로 건너뛴다.
    const line = (lines[i] ?? '')
      .replace(BOX_EDGE_RE.leading, '')
      .replace(BOX_EDGE_RE.trailing, '')
      .trim();
    if (line.length === 0) continue;
    if (HC_STATUS_LINE_RE.test(line)) continue;
    if (BOX_ONLY_LINE_RE.test(line)) continue;
    if (SPINNER_ONLY_LINE_RE.test(line)) continue;
    // 시크릿 리덕션은 절단 이전에(레댁션 계약 — 잘린 경계 무효화 방지)
    const safe = redactSecrets(line).replace(/\s+/g, ' ').trim();
    if (safe.length === 0) continue;
    return truncateChars(safe, maxChars);
  }
  return null;
}

// ---------- 슬롯·모델 추론 ----------

const HERDR_ROLE_RE = /^HERDR_ROLE=(lead|explore|verifier|advisor|carbon|coder)$/i;
const WORKER_CLI_SET = new Set(['pi', 'devin', 'claude', 'codex', 'gemini', 'agy']);
const ENV_ASSIGN_ARG_RE = /^[A-Za-z_][A-Za-z0-9_]*=/;

/** 토큰(절대경로 포함)의 basename — CLI 이름 추출용. */
function tokenBase(token: string): string {
  const norm = token.replaceAll('\\', '/');
  return norm.slice(norm.lastIndexOf('/') + 1);
}

/** argv 앞단의 `env` + `KEY=VAL` 토큰 제거. */
function stripEnvPrefix(tokens: readonly string[]): string[] {
  let i = 0;
  if (tokens[i] === 'env') i++;
  while (i < tokens.length && ENV_ASSIGN_ARG_RE.test(tokens[i] ?? '')) i++;
  return tokens.slice(i);
}

/** `node …/dg.ts wrap … --` 래퍼 접두 제거 — `--` 이후의 실제 워커 커맨드만 남긴다. */
function stripDgWrapPrefix(tokens: readonly string[]): string[] {
  let dgIdx = -1;
  for (let i = 0; i < tokens.length; i++) {
    const base = tokenBase(tokens[i] ?? '');
    if (base === 'dg.ts' || base === 'dg') {
      dgIdx = i;
      break;
    }
  }
  if (dgIdx < 0) return tokens as string[];
  const dashIdx = tokens.indexOf('--', dgIdx);
  return dashIdx >= 0 ? tokens.slice(dashIdx + 1) : (tokens as string[]);
}

/** argv(+agent 폴백)에서 실제 워커 CLI 이름과 `--model` 값을 찾는다. */
function parseWorkerCliAndModel(
  argv: readonly string[],
  fallbackAgent?: string
): { cli: string; model: string | null } {
  const cleaned0 = argv.filter((t) => typeof t === 'string' && t.length > 0);
  let tokens = stripEnvPrefix(cleaned0);
  tokens = stripEnvPrefix(stripDgWrapPrefix(tokens));

  let cliIdx = -1;
  for (let i = 0; i < tokens.length; i++) {
    if (WORKER_CLI_SET.has(tokenBase(tokens[i] ?? ''))) {
      cliIdx = i;
      break;
    }
  }
  let cli: string;
  if (cliIdx >= 0) {
    cli = tokenBase(tokens[cliIdx]!);
  } else {
    const agent = (fallbackAgent ?? '').toLowerCase();
    cli = WORKER_CLI_SET.has(agent) ? agent : 'custom';
  }

  let model: string | null = null;
  const scanStart = cliIdx >= 0 ? cliIdx : 0;
  for (let i = scanStart; i < tokens.length; i++) {
    const t = tokens[i]!;
    if (t === '--model') {
      const v = tokens[i + 1];
      if (v && !v.startsWith('-')) {
        model = v;
        break;
      }
    } else if (t.startsWith('--model=')) {
      model = t.slice('--model='.length);
      break;
    }
  }
  return { cli, model };
}

/**
 * `hc get` 상세 + 화면 원문 → LiveWorkerInfo.
 * 슬롯은 ① argv의 `HERDR_ROLE=`(carbon→advisor) ② 세션명 규약(verify-/exp-/carbon-/lead-
 * …) 순으로 판정하고, CLI·모델은 `env`·`dg wrap` 래퍼를 벗겨낸 뒤 실제 CLI와
 * `--model`에서 읽는다(실패 시 `detail.agent` 폴백).
 */
export function inferLiveWorkerSlot(
  detail: HcTerminalDetail,
  rawScreen?: string
): LiveWorkerInfo | null {
  if (!detail || typeof detail.address !== 'string' || detail.address.length === 0) return null;
  const argv = Array.isArray(detail.argv) ? detail.argv : [];

  // 1) 슬롯 — HERDR_ROLE env가 최우선
  let slot: LiveWorkerSlot = 'coder';
  let roleFromArgv = false;
  for (const token of argv) {
    if (typeof token !== 'string') continue;
    const m = token.match(HERDR_ROLE_RE);
    if (m) {
      const role = m[1]!.toLowerCase();
      slot = role === 'carbon' ? 'advisor' : (role as LiveWorkerSlot);
      roleFromArgv = true;
      break;
    }
  }
  const name = detail.name ?? '';
  if (!roleFromArgv && name.length > 0) {
    if (/verify|verifier/i.test(name)) slot = 'verifier';
    else if (/exp-|explore/i.test(name)) slot = 'explore';
    else if (/advisor|carbon/i.test(name)) slot = 'advisor';
    else if (/lead-/i.test(name)) slot = 'lead';
    else slot = 'coder';
  }

  // 2) CLI·모델
  const { cli, model } = parseWorkerCliAndModel(argv, detail.agent);
  const shortModel = formatRoleBadge(model ? { cli, model } : { cli });
  const modelName = model ?? cli;

  // 3) 조립 — 활동은 화면 원문 우선, 없으면 상태 기반 간결 폴백
  const activity =
    extractLastActivityLine(rawScreen ?? '') ??
    (detail.status ? `${detail.status} 작업 중` : '작업 실행 중');
  return {
    slot,
    badge: `${shortModel} (실행 중)`,
    shortModel,
    modelName,
    address: detail.address,
    name: name.length > 0 ? name : detail.address,
    cwd: detail.cwd ?? null,
    repoTag: extractRepoTag(detail.cwd),
    activity,
    status: detail.status ?? null,
  };
}

// ---------- 라벨 병합 ----------

/**
 * 최상위 활성 AI 세션을 Lead 카드로 정규화:
 * - 슬롯은 lead 고정(LEAD 카운터·Lead 카드에 반영).
 * - 표시명은 비어 있지 않은 세션명, 없으면 'lead'(원 주소 노출 방지).
 * - `--model` 미파싱 시 shortModel을 agent명(예: 'antigravity')으로 폴백.
 */
export function normalizeTopLevelWorker(
  worker: LiveWorkerInfo,
  summary: HcTerminalSummary
): LiveWorkerInfo {
  const name = (summary.name ?? '').trim();
  const agent = (summary.agent ?? '').trim().toLowerCase();
  let { shortModel, badge, modelName } = worker;
  if (agent && shortModel === 'custom') {
    shortModel = agent;
    badge = `${agent} (실행 중)`;
    if (!modelName || modelName === 'custom') modelName = agent;
  }
  return {
    ...worker,
    slot: 'lead',
    name: name.length > 0 ? name : 'lead',
    shortModel,
    badge,
    modelName,
  };
}

/** 라이브 오버라이드 대상 슬롯(lead는 watchedLead 부재 시에만). */
const LIVE_OVERRIDE_SLOTS: readonly LiveWorkerSlot[] = ['verifier', 'coder', 'explore', 'advisor'];

/**
 * roster 대기(standby) 라벨 위에 라이브 워커 배지를 얹는다.
 * - `watchedLead`(관측 중 리드 세션 배지)가 있으면 lead 슬롯을 침범하지 않는다.
 * - 슬롯별 첫 워커가 우선(동일 슬롯 다중 워커 시 표기 안정).
 * - 워커가 없으면 대기 라벨로 그대로 되돌아간다(종료 시 자동 복귀).
 * - 모델 플래그는 실측 모델이 있을 때만 `--model <원문>` (applyRosterToState 계약 동일).
 */
export function mergeLiveRosterLabels(
  base: RosterDisplayLabels,
  watchedLead: string | null | undefined,
  workers: readonly LiveWorkerInfo[]
): MergedLiveRoster {
  const labels: RosterDisplayLabels = { ...base };
  if (watchedLead) labels.lead = watchedLead;

  let leadWorker: LiveWorkerInfo | null = null;
  let verifierWorker: LiveWorkerInfo | null = null;
  const claimed = new Set<LiveWorkerSlot>();
  for (const w of workers) {
    if (w.slot === 'lead') {
      if (!watchedLead && !leadWorker) leadWorker = w;
      continue;
    }
    if (LIVE_OVERRIDE_SLOTS.includes(w.slot) && !claimed.has(w.slot)) {
      claimed.add(w.slot);
      labels[w.slot] = w.shortModel;
      if (w.slot === 'verifier') verifierWorker = w;
    }
  }

  const leadModelName = labels.lead;
  const leadModelFlag =
    leadWorker && leadWorker.modelName && leadWorker.modelName !== leadWorker.shortModel
      ? `--model ${leadWorker.modelName}`
      : labels.lead;
  const verifierModelName = labels.verifier;
  const verifierModelFlag =
    verifierWorker && verifierWorker.modelName && verifierWorker.modelName !== verifierWorker.shortModel
      ? `--model ${verifierWorker.modelName}`
      : labels.verifier;

  return {
    labels,
    leadModelName,
    leadModelFlag,
    verifierModelName,
    verifierModelFlag,
    workers: [...workers],
    activeWorkerCount: workers.length,
  };
}

// ---------- 주기 동기화 ----------

/** `hc` 실행 추상 — 테스트가 mock으로 갈아끼운다. stdout 문자열 반환. */
export type HcExecRunner = (
  file: string,
  args: readonly string[],
  opts: { timeout: number }
) => Promise<string>;

async function defaultHcExec(
  file: string,
  args: readonly string[],
  opts: { timeout: number }
): Promise<string> {
  const { stdout } = await execFileAsync(file, args, {
    timeout: opts.timeout,
    encoding: 'utf8',
    maxBuffer: 4 * 1024 * 1024,
  });
  return stdout;
}

export interface LiveRosterSyncerOptions {
  /** roster.json 프로필 후보 기준 디렉터리(프로젝트 오버라이드 탐색용). */
  repoDir: string;
  /** `--repo <dir>` 스코프 — null이면 전체 레포 관측. */
  repoFilterDir?: string | null;
  /** `--session <id>` 스코프 — undefined면 세션 필터 없음(기존 동작 불변). */
  sessionId?: string;
  /** 관측 중 리드 배지 제공자(있으면 lead 슬롯 라이브 오버라이드 금지). */
  getWatchedLead?: () => string | null;
  /** 병합 결과 수신자 — 매 틱 호출(변경 여부 판정은 수신자 책임). */
  onUpdate: (merged: MergedLiveRoster) => void;
  /** 동기화 주기(기본 2000ms). */
  intervalMs?: number;
  /** `hc` 실행기 주입(테스트용 — 기본 execFile). */
  execRunner?: HcExecRunner;
  /** `hc` 실행 파일(기본 'hc'). */
  hcBin?: string;
}

export interface LiveRosterSyncer {
  /** 타이머 정지·캐시 정리(멱등). */
  stop(): void;
  /** 1회 즉시 동기화(테스트·수동 트리거용). */
  tickNow(): Promise<void>;
}

/** `hc ls`/`hc get` 항목을 방어적으로 정규화 — status는 객체형(`{app,state}`)도 문자열로. */
function normalizeTerminal(raw: Record<string, unknown>): HcTerminalSummary | null {
  const address = typeof raw['address'] === 'string' ? raw['address'] : null;
  if (!address) return null;
  const statusRaw = raw['status'];
  let status: string | undefined;
  if (typeof statusRaw === 'string') {
    status = statusRaw;
  } else if (statusRaw && typeof statusRaw === 'object') {
    const st = (statusRaw as Record<string, unknown>)['state'];
    if (typeof st === 'string') status = st;
  }
  return {
    address,
    agent: typeof raw['agent'] === 'string' ? raw['agent'] : undefined,
    name: typeof raw['name'] === 'string' ? raw['name'] : undefined,
    parent: typeof raw['parent'] === 'string' ? raw['parent'] : undefined,
    state: typeof raw['state'] === 'string' ? raw['state'] : undefined,
    status,
    cwd: typeof raw['cwd'] === 'string' ? raw['cwd'] : undefined,
  };
}

function parseJsonOrNull(raw: string): Record<string, unknown> | null {
  try {
    const parsed: unknown = JSON.parse(raw);
    return typeof parsed === 'object' && parsed !== null && !Array.isArray(parsed)
      ? (parsed as Record<string, unknown>)
      : null;
  } catch {
    return null;
  }
}

/**
 * 주기 논블로킹 로스터 동기화기:
 * - roster.json 후보 경로의 `mtimeMs:size` 서명이 바뀌면 설정을 핫리로드한다.
 * - `hc ls --json` → 워커 필터(`isWorkerTerminal` + `matchesRepoScope`) →
 *   주소별 `hc get --json` 캐시(떠난 주소 즉시 증발) → 활동 캡처 상한 6개의
 *   `hc read <addr> --lines 4`.
 * - 틱 중첩 방지(inFlight), `stop()`으로 안전 종료. 모든 `hc` 오류는 조용히 폴백.
 */
export function createLiveRosterSyncer(opts: LiveRosterSyncerOptions): LiveRosterSyncer {
  const execRunner = opts.execRunner ?? defaultHcExec;
  const hcBin = opts.hcBin ?? 'hc';
  const intervalMs = opts.intervalMs ?? DEFAULT_SYNC_INTERVAL_MS;
  const repoFilterDir = opts.repoFilterDir ?? null;
  const sessionId = opts.sessionId;

  let baseLabels: RosterDisplayLabels | null = null;
  let rosterSignature: string | null = null;
  let lastWorkers: LiveWorkerInfo[] = [];
  const detailCache = new Map<string, HcTerminalDetail>();
  const screenCache = new Map<string, string>();
  let inFlight = false;
  let stopped = false;
  let timer: ReturnType<typeof setInterval> | null = null;

  function rosterCandidatePaths(): string[] {
    const out: string[] = [];
    const envPath = process.env.HERDR_ROSTER_PATH?.trim();
    if (envPath) out.push(path.resolve(envPath));
    out.push(path.join(opts.repoDir, '.herdr', 'roster.json'));
    out.push(globalRosterPath());
    return out;
  }

  function computeRosterSignature(): string {
    const parts: string[] = [];
    for (const p of rosterCandidatePaths()) {
      try {
        const st = fs.statSync(p);
        parts.push(`${p}:${st.mtimeMs}:${st.size}`);
      } catch {
        parts.push(`${p}:-`);
      }
    }
    return parts.join('|');
  }

  function reloadRosterLabels(): void {
    const sig = computeRosterSignature();
    if (sig === rosterSignature && baseLabels) return;
    rosterSignature = sig;
    try {
      const loaded = loadRosterConfig({ repoDir: opts.repoDir });
      const resolved = resolveRosterProfile(loaded.config, loaded.config.defaultProfile);
      baseLabels = rosterLabelsFromProfile(resolved.name, loaded.source, resolved.profile);
    } catch {
      if (!baseLabels) {
        const resolved = resolveRosterProfile(builtinRosterConfig(), 'quota-saver');
        baseLabels = rosterLabelsFromProfile(resolved.name, 'builtin', resolved.profile);
      }
    }
  }

  async function runTick(): Promise<void> {
    if (inFlight || stopped) return;
    inFlight = true;
    try {
      reloadRosterLabels();
      const labels = baseLabels!;

      // 1) `hc ls --json` — 실행 오류·JSON 파싱 실패 모두 이전 워커 상태 유지(fail-safe).
      //    (파싱 실패 시 빈 목록으로 방송하면 워커 0건 + 허위 종료 로그가 터진다.)
      let summaries: HcTerminalSummary[] | null = null;
      try {
        const raw = await execRunner(hcBin, ['ls', '--json'], { timeout: HC_LS_TIMEOUT_MS });
        const parsed = parseJsonOrNull(raw);
        if (!parsed || typeof parsed !== 'object') {
          summaries = null; // malformed JSON / invalid listing schema — 이전 워커 보존
        } else {
          const terminals = (parsed as Record<string, unknown>)['terminals'];
          if (!Array.isArray(terminals)) {
            summaries = null; // terminals 누락·비정상 — 이전 워커 보존 ({} / {"terminals":null})
          } else {
          summaries = terminals
            .map((t) =>
              t && typeof t === 'object' ? normalizeTerminal(t as Record<string, unknown>) : null
            )
            .filter((t): t is HcTerminalSummary => t !== null)
            .filter(
              (t) =>
                isObservableTerminal(t) &&
                matchesRepoScope(t.cwd, repoFilterDir) &&
                matchesSessionScope(t, sessionId)
            );
          }
        }
      } catch {
        summaries = null;
      }

      // 2) 상세 캐시 갱신 — 떠난 주소는 증발
      if (summaries) {
        const active = new Set(summaries.map((t) => t.address));
        for (const addr of [...detailCache.keys()]) {
          if (!active.has(addr)) {
            detailCache.delete(addr);
            screenCache.delete(addr);
          }
        }

        // 3) 워커별 상세·활동 수집 (활동 read는 틱당 상한)
        const workers: LiveWorkerInfo[] = [];
        let readCount = 0;
        for (const summary of summaries) {
          let detail = detailCache.get(summary.address);
          if (!detail) {
            try {
              const raw = await execRunner(hcBin, ['get', summary.address, '--json'], {
                timeout: HC_LS_TIMEOUT_MS,
              });
              const parsed = parseJsonOrNull(raw);
              const normalized = parsed ? normalizeTerminal(parsed) : null;
              detail = normalized
                ? {
                    ...normalized,
                    argv:
                      parsed && Array.isArray(parsed['argv'])
                        ? (parsed['argv'] as unknown[]).filter(
                            (a): a is string => typeof a === 'string'
                          )
                        : undefined,
                  }
                : { ...summary };
            } catch {
              detail = { ...summary };
            }
          }
          // `hc get` 캐시는 argv·이름 같은 정적 정보만 담당 — status/state/cwd는 매 틱의
          // `hc ls` 요약을 항상 위에 덮어쓴다. 그래야 working→done 전환이 실시간으로
          // 카드·로그에 반영된다(카드 완료 표시·완료 로그는 cli.ts가 이 전환으로 판정).
          detail = {
            ...detail,
            agent: summary.agent ?? detail.agent,
            name: summary.name ?? detail.name,
            parent: summary.parent ?? detail.parent,
            state: summary.state ?? detail.state,
            status: summary.status ?? detail.status,
            cwd: summary.cwd ?? detail.cwd,
          };
          detailCache.set(summary.address, detail);

          let screen: string | undefined;
          if (readCount < MAX_ACTIVITY_READS_PER_TICK) {
            readCount++;
            try {
              screen = await execRunner(
                hcBin,
                ['read', summary.address, '--lines', '4'],
                { timeout: HC_READ_TIMEOUT_MS }
              );
              screenCache.set(summary.address, screen);
            } catch {
              screen = undefined;
            }
          }
          const rawScreen = screen ?? screenCache.get(summary.address);
          let worker = inferLiveWorkerSlot(detail, rawScreen);
          // 최상위 활성 AI 세션(워커 규약 밖 리드 등) — Lead 슬롯·표시명·shortModel
          // 폴백을 정규화한다. 워커 규약 터미널은 그대로 둔다.
          if (
            worker &&
            isActiveTopLevelSession(summary) &&
            !isWorkerTerminal(summary)
          ) {
            worker = normalizeTopLevelWorker(worker, summary);
          }
          if (worker) workers.push(worker);
        }
        lastWorkers = workers;
      }

      // 4) 병합 방송 — 변경 판정·렌더 제어는 수신자(cli.ts) 책임
      const merged = mergeLiveRosterLabels(labels, opts.getWatchedLead?.() ?? null, lastWorkers);
      if (!stopped) opts.onUpdate(merged);
    } finally {
      inFlight = false;
    }
  }

  // 최초 1회 즉시 실행 후 주기 타이머(언러프 — TUI 종료를 방해하지 않는다)
  void runTick();
  timer = setInterval(() => {
    void runTick();
  }, intervalMs);
  timer.unref?.();

  return {
    stop(): void {
      stopped = true;
      if (timer) {
        clearInterval(timer);
        timer = null;
      }
      detailCache.clear();
      screenCache.clear();
    },
    tickNow(): Promise<void> {
      return runTick();
    },
  };
}
