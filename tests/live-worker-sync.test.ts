/**
 * 목적: Live TUI Sync v2(`src/backend/liveWorkerSync.ts` + cli/렌더 배선) 단위 검증.
 * - `isWorkerTerminal`/`inferLiveWorkerSlot`: pi·dg wrap·devin·HERDR_ROLE 슬롯 판정,
 *   dg-monitor·셸 제외
 * - `extractRepoTag`/`matchesRepoScope`: 일반 경로·`.herdr/worktrees` 환원, 스코프 매칭
 * - `extractLastActivityLine`: ANSI 제거·`[hc]`/박스/스피너 스킵·시크릿 리덕션·절단
 * - `mergeLiveRosterLabels`: 라이브 오버라이드·watchedLead 보존·대기 라벨 복귀
 * - `createLiveRosterSyncer`: roster.json 핫리로드 + `hc` 오류 fail-safe (mock 실행기,
 *   라이브 `hc` 스폰 0회 — 전부 결정적)
 * - 렌더 헤더: `repo=<이름>` / `전체` 표기, 라이브 워커 카드 노출, 80/100/120/148열
 *   카운터 충돌 없음(우측 폭 선점산)
 * - 회귀 방지: 라이브 동기화(0·1 워커) 후 applyTranscriptStep 무동작(cards 3슬롯
 *   불변), working→done 전환(hc get 캐시 + 매 틱 ls 상태 병합), malformed hc ls JSON
 *   시 이전 워커 보존, 박스형 [hc] 푸터·겹선 테두리 스킵, 루트('/') 스코프, ksh/dash/pwsh 제외
 * - 동적 워커 와이어(1..4카드+초과 배지)·카드별 라이브 모션(펄스·부재 활성 바·스피너),
 *   라이브 역할 패널(verifier/advisor/lead) 활성화·침착 대기 복귀 검증
 * - 유출 스캔: 변경 파일에 홈 절대경로·키 패턴 0건
 */
import assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { test } from 'node:test';
import {
  createLiveRosterSyncer,
  extractLastActivityLine,
  extractRepoTag,
  inferLiveWorkerSlot,
  isActiveTopLevelSession,
  isObservableTerminal,
  isWorkerTerminal,
  matchesRepoScope,
  matchesSessionScope,
  mergeLiveRosterLabels,
  normalizeTopLevelWorker,
  type HcTerminalDetail,
  type LiveWorkerInfo,
  type MergedLiveRoster,
} from '../src/backend/liveWorkerSync.ts';
import {
  applyTranscriptStep,
  type TranscriptStep,
  type TranscriptToolCall,
} from '../src/backend/transcriptMapper.ts';
import { createInitialState } from '../src/state/store.ts';
import { renderFrame } from '../src/ui/renderApp.ts';
import { computeLayout, type FullLayout } from '../src/ui/layout.ts';
import { applyLiveRolePanels, createLiveRoleTrail, formatElapsedMmss, resolveTuiScope } from '../src/cli.ts';
import type { RosterDisplayLabels } from '../src/types.ts';

const REPO_ROOT = path.resolve(import.meta.dirname, '..');

function makeBaseLabels(): RosterDisplayLabels {
  return {
    profileName: 'quota-saver',
    rosterSource: 'builtin',
    lead: 'pi:glm-5.3',
    explore: 'pi:deepseek-v4.1-flash',
    coder: 'devin',
    verifier: 'pi:deepseek-v4.1-flash',
    advisor: 'pi:glm-5.3',
  };
}

function makeWorker(partial: Partial<LiveWorkerInfo>): LiveWorkerInfo {
  return {
    slot: 'coder',
    badge: 'pi:glm-5.3 (실행 중)',
    shortModel: 'pi:glm-5.3',
    modelName: 'glm-zcode/glm-5.3',
    address: 'tA',
    name: 'w-pi-x',
    cwd: '/tmp/my-repo',
    repoTag: 'my-repo',
    activity: '작업 실행 중',
    status: null,
    ...partial,
  };
}

async function waitFor(cond: () => boolean, timeoutMs = 4000, stepMs = 10): Promise<void> {
  const start = Date.now();
  while (!cond()) {
    if (Date.now() - start > timeoutMs) {
      throw new Error(`waitFor timeout after ${timeoutMs}ms`);
    }
    await new Promise((resolve) => setTimeout(resolve, stepMs));
  }
}

/** roster 탐색 환경 격리 — 실제 전역 roster·env 영향 차단 (복구 필수). */
function isolateRosterEnv(base: string): { restore: () => void } {
  const savedHrp = process.env.HERDR_ROSTER_PATH;
  const savedXdg = process.env.XDG_CONFIG_HOME;
  process.env.XDG_CONFIG_HOME = path.join(base, 'xdg');
  return {
    restore: () => {
      if (savedHrp === undefined) delete process.env.HERDR_ROSTER_PATH;
      else process.env.HERDR_ROSTER_PATH = savedHrp;
      if (savedXdg === undefined) delete process.env.XDG_CONFIG_HOME;
      else process.env.XDG_CONFIG_HOME = savedXdg;
    },
  };
}

function writeTempRoster(rosterPath: string, profile: string, leadCli: string, leadModel: string): void {
  fs.writeFileSync(
    rosterPath,
    JSON.stringify({
      defaultProfile: profile,
      profiles: {
        [profile]: { roles: { lead: { cli: leadCli, model: leadModel } } },
      },
    })
  );
}

// ---------------------------------------------------------------------------

test('1. inferLiveWorkerSlot — pi --model / dg wrap / devin / HERDR_ROLE / 모니터·셸 제외', () => {
  // (a) pi --model — 이름 w- 접두 → coder 슬롯, 화면 원문에서 활동 추출
  const pi = inferLiveWorkerSlot(
    {
      address: 't1',
      agent: 'pi',
      name: 'w-pi-glm-tui-live',
      state: 'live',
      cwd: '/tmp/my-repo',
      argv: [
        '/tools/pi',
        '--provider',
        'opencodex',
        '--model',
        'glm-zcode/glm-5.3',
        '--thinking',
        'max',
        '프롬프트',
      ],
    },
    '\x1b[32m테스트 12/12 통과\x1b[0m'
  );
  assert.equal(pi?.slot, 'coder');
  assert.equal(pi?.shortModel, 'pi:glm-5.3');
  assert.equal(pi?.badge, 'pi:glm-5.3 (실행 중)');
  assert.equal(pi?.modelName, 'glm-zcode/glm-5.3');
  assert.equal(pi?.repoTag, 'my-repo');
  assert.equal(pi?.activity, '테스트 12/12 통과');

  // (b) dg wrap 래퍼 + env — 래퍼를 벗겨 실제 CLI·모델·HERDR_ROLE 슬롯 인식
  const wrapped = inferLiveWorkerSlot({
    address: 't2',
    agent: 'node',
    name: 'coder1-abc123',
    state: 'live',
    cwd: '/tmp/my-repo/.herdr/worktrees/coder1-abc123',
    argv: [
      'node',
      '/dg/dg.ts',
      'wrap',
      '--route',
      'tool_select',
      '--',
      'env',
      'HERDR_ROLE=coder',
      'HERDR_READONLY=1',
      '/tools/pi',
      '--model',
      'glm-zcode/glm-5.3',
      '--no-approve',
      '작업',
    ],
  });
  assert.equal(wrapped?.slot, 'coder');
  assert.equal(wrapped?.shortModel, 'pi:glm-5.3');
  assert.equal(wrapped?.repoTag, 'my-repo');
  assert.equal(wrapped?.activity, '작업 실행 중'); // 화면 원문 없음 + status 없음 → 폴백

  // (c) devin — 모델 미지정 → CLI명 그대로 배지, status 폴백 활동
  const devin = inferLiveWorkerSlot({
    address: 't3',
    agent: 'devin',
    name: 'coder2-abc123',
    state: 'live',
    status: 'working',
    cwd: '/tmp/my-repo',
    argv: ['/tools/devin', '--permission-mode', 'auto', '--', '수정 반영'],
  });
  assert.equal(devin?.slot, 'coder');
  assert.equal(devin?.shortModel, 'devin');
  assert.equal(devin?.modelName, 'devin');
  assert.equal(devin?.badge, 'devin (실행 중)');
  assert.equal(devin?.activity, 'working 작업 중');

  // (d) HERDR_ROLE=verifier — argv 슬롯이 이름 규약보다 우선
  const verifier = inferLiveWorkerSlot({
    address: 't4',
    agent: 'pi',
    name: 'verify-abc123',
    state: 'live',
    cwd: '/tmp/my-repo',
    argv: [
      'env',
      'HERDR_ROLE=verifier',
      'HERDR_READONLY=1',
      '/tools/pi',
      '--model',
      'command-code/deepseek-deepseek-v4.1-flash',
      '프롬프트',
    ],
  });
  assert.equal(verifier?.slot, 'verifier');
  assert.equal(verifier?.shortModel, 'pi:deepseek-v4.1-flash');

  // (e) HERDR_ROLE=carbon → advisor 슬롯 환원
  const advisor = inferLiveWorkerSlot({
    address: 't5',
    agent: 'pi',
    name: 'carbon-gate1-abc123',
    state: 'live',
    cwd: '/tmp/my-repo',
    argv: ['env', 'HERDR_ROLE=carbon', '/tools/pi', '--model', 'glm-zcode/glm-5.3'],
  });
  assert.equal(advisor?.slot, 'advisor');

  // (f) lead — 이름 규약
  const lead = inferLiveWorkerSlot({
    address: 't6',
    agent: 'pi',
    name: 'lead-abc123',
    state: 'live',
    cwd: '/tmp/my-repo',
    argv: ['env', 'HERDR_ROLE=lead', '/tools/pi', '--model', 'glm-zcode/glm-5.3'],
  });
  assert.equal(lead?.slot, 'lead');

  // (g) 모니터·셸·비라이브 제외 — 워커 집계 대상 아님
  assert.equal(isWorkerTerminal({ address: 'a', agent: 'node', name: 'dg-monitor', state: 'live' }), false);
  assert.equal(
    isWorkerTerminal({ address: 'a', agent: 'node', name: 'dg-monitor-myrepo', state: 'live', parent: 't9' }),
    false
  );
  assert.equal(isWorkerTerminal({ address: 'a', agent: 'node', name: 'mon-xyz', state: 'live' }), false);
  assert.equal(isWorkerTerminal({ address: 'a', agent: 'zsh', state: 'live' }), false);
  assert.equal(isWorkerTerminal({ address: 'a', agent: 'zsh', name: 'w-shell', state: 'live' }), false);
  assert.equal(isWorkerTerminal({ address: 'a', agent: 'antigravity', name: 'w-agy', state: 'live' }), false);
  assert.equal(isWorkerTerminal({ address: 'a', agent: 'pi', name: 'w-x', state: 'dead' }), false);
  assert.equal(isWorkerTerminal({ address: 'a', agent: 'pi', state: 'live' }), false); // 근거 없음

  // (h) 긍정 — 이름 접두 / parent+워커 CLI 에이전트
  assert.equal(isWorkerTerminal({ address: 'a', agent: 'pi', name: 'w-x', state: 'live' }), true);
  assert.equal(isWorkerTerminal({ address: 'a', agent: 'devin', parent: 't1', state: 'live' }), true);
});

test('2. extractRepoTag / matchesRepoScope — 일반 경로·워크트리 환원·스코프 판정', () => {
  assert.equal(extractRepoTag(null), 'unknown');
  assert.equal(extractRepoTag(''), 'unknown');
  assert.equal(extractRepoTag('   '), 'unknown');
  assert.equal(extractRepoTag('/a/b/my-repo'), 'my-repo');
  assert.equal(extractRepoTag('/a/b/my-repo/'), 'my-repo');
  assert.equal(extractRepoTag('/a/b/my-repo/.herdr/worktrees/coder1'), 'my-repo');
  assert.equal(extractRepoTag('/a/b/my-repo/.herdr/worktrees/coder1/'), 'my-repo');
  assert.equal(extractRepoTag('C:\\x\\my-repo\\.herdr\\worktrees\\wt1'), 'my-repo');

  const tmp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'lws-scope-')));
  const other = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'lws-other-')));
  try {
    const repoDir = path.join(tmp, 'my-repo');
    const sub = path.join(repoDir, 'src');
    const worktree = path.join(repoDir, '.herdr', 'worktrees', 'coder1');
    fs.mkdirSync(sub, { recursive: true });
    fs.mkdirSync(worktree, { recursive: true });

    assert.equal(matchesRepoScope(repoDir, repoDir), true);
    assert.equal(matchesRepoScope(sub, repoDir), true);
    assert.equal(matchesRepoScope(worktree, repoDir), true); // 워크트리도 소속 레포 스코프
    assert.equal(matchesRepoScope(other, repoDir), false); // 무관 레포
    assert.equal(matchesRepoScope(path.join(other, 'sub'), repoDir), false);
    assert.equal(matchesRepoScope(sub, null), true); // 전역 관측
    assert.equal(matchesRepoScope(sub, undefined), true);
    assert.equal(matchesRepoScope(null, repoDir), false); // cwd 미상 + 스코프 지정
    assert.equal(matchesRepoScope(undefined, undefined), true);
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
    fs.rmSync(other, { recursive: true, force: true });
  }
});

test('3. extractLastActivityLine — ANSI 제거·[hc]/박스/스피너 스킵·리덕션·절단', () => {
  // ANSI 색상 코드 제거
  assert.equal(extractLastActivityLine('\x1b[32m테스트 12/12 통과\x1b[0m'), '테스트 12/12 통과');
  assert.equal(extractLastActivityLine('\x1b]0;title\x07\x1b[1m실제 작업\x1b[0m'), '실제 작업');

  // 아래→위 훑기: 빈 줄·[hc] 상태줄·스피너·구분선 건너뛰고 의미 줄 선택
  const screen = [
    '⠋',
    '┌──────────────┐',
    '[hc] t18s · live · pi',
    '   ',
    '\x1b[1m\x1b[32m검증 통과: 12/12\x1b[0m',
    '[hc] done',
    '⠹',
    '─ ─ ─ ─ ─',
  ].join('\n');
  assert.equal(extractLastActivityLine(screen), '검증 통과: 12/12');

  // 박스 내부 텍스트 가장자리 장식 제거
  assert.equal(extractLastActivityLine('│ 패치 적용 중 │'), '패치 적용 중');

  // 시크릿 리덕션 — sk-… / AIza… / Bearer
  const sk = extractLastActivityLine('curl -H "Authorization: Bearer sk-abcdefghijklmnop1234" https://x.io');
  assert.ok(sk !== null && !sk.includes('sk-abcdefghijklmnop1234'), `sk leaked: ${sk}`);
  assert.ok(sk !== null && sk.includes('<redacted>'));
  const googleKey = extractLastActivityLine('export KEY=AIzaSyD-12345678901234567890x 완료');
  assert.ok(googleKey !== null && !googleKey.includes('AIzaSyD'), `AIza leaked: ${googleKey}`);

  // 절단 — maxChars(68) 초과 시 … 마감
  const long = extractLastActivityLine('x'.repeat(100));
  assert.equal(long?.length, 68);
  assert.ok(long?.endsWith('…'));
  assert.equal(extractLastActivityLine('y'.repeat(50), 10)?.length, 10);

  // 의미 줄 전무 → null
  assert.equal(extractLastActivityLine('⠋\n⠙\n\n─ ─ ─\n[hc] idle'), null);
  assert.equal(extractLastActivityLine(''), null);
  assert.equal(extractLastActivityLine(null), null);
  assert.equal(extractLastActivityLine('┌────────┐\n└────────┘'), null);
});

test('4. mergeLiveRosterLabels — 라이브 오버라이드·watchedLead 보존·대기 복귀', () => {
  const base = makeBaseLabels();
  const baseSnapshot = JSON.stringify(base);

  // (a) 라이브 verifier가 대기 배지를 덮고, 모델 플래그는 실측 원문으로
  const merged = mergeLiveRosterLabels(base, null, [
    makeWorker({
      slot: 'verifier',
      shortModel: 'pi:glm-5.3',
      badge: 'pi:glm-5.3 (실행 중)',
      modelName: 'glm-zcode/glm-5.3',
      name: 'verify-abc',
      address: 'tV',
    }),
  ]);
  assert.equal(merged.labels.verifier, 'pi:glm-5.3');
  assert.equal(merged.verifierModelName, 'pi:glm-5.3');
  assert.equal(merged.verifierModelFlag, '--model glm-zcode/glm-5.3');
  assert.equal(merged.labels.coder, 'devin'); // 무관 슬롯은 대기 유지
  assert.equal(merged.labels.lead, 'pi:glm-5.3');
  assert.equal(merged.activeWorkerCount, 1);

  // base 원본 미변형 (오염 방지)
  assert.equal(JSON.stringify(base), baseSnapshot);

  // (b) watchedLead 우선 — 라이브 lead 워커가 있어도 관측 중 배지 보존
  const mergedWatched = mergeLiveRosterLabels(base, 'Antigravity (현재 세션)', [
    makeWorker({ slot: 'lead', name: 'lead-abc', address: 'tL' }),
  ]);
  assert.equal(mergedWatched.labels.lead, 'Antigravity (현재 세션)');

  // (c) 워커 종료(빈 배열) → 대기 라벨로 자동 복귀
  const reverted = mergeLiveRosterLabels(base, null, []);
  assert.deepEqual(reverted.labels, base);
  assert.equal(reverted.verifierModelFlag, 'pi:deepseek-v4.1-flash'); // 배지 폴백
  assert.equal(reverted.leadModelFlag, 'pi:glm-5.3');
  assert.equal(reverted.activeWorkerCount, 0);

  // (d) 라이브 lead(모델 실측) — watchedLead 부재 시에만 모델명·플래그 반영
  const mergedLead = mergeLiveRosterLabels(base, null, [
    makeWorker({ slot: 'lead', name: 'lead-abc', address: 'tL2' }),
  ]);
  assert.equal(mergedLead.labels.lead, 'pi:glm-5.3');
  assert.equal(mergedLead.leadModelName, 'pi:glm-5.3');
  assert.equal(mergedLead.leadModelFlag, '--model glm-zcode/glm-5.3');

  // (e) 동일 슬롯 다중 워커 — 첫 워커 우선(표기 안정)
  const multi = mergeLiveRosterLabels(base, null, [
    makeWorker({ slot: 'coder', shortModel: 'devin', modelName: 'devin', address: 'c1' }),
    makeWorker({ slot: 'coder', address: 'c2' }),
  ]);
  assert.equal(multi.labels.coder, 'devin');
  assert.equal(multi.activeWorkerCount, 2);
});

test('5. createLiveRosterSyncer — roster.json 핫리로드 + hc 오류 fail-safe', async () => {
  const tmp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'lws-hot-')));
  const env = isolateRosterEnv(tmp);
  const rosterPath = path.join(tmp, 'roster.json');
  process.env.HERDR_ROSTER_PATH = rosterPath;
  writeTempRoster(rosterPath, 't-profile-a', 'claude', 'claude-sonnet-4-6');
  const updates: MergedLiveRoster[] = [];
  const syncer = createLiveRosterSyncer({
    repoDir: tmp,
    repoFilterDir: null,
    getWatchedLead: () => null,
    onUpdate: (m) => updates.push(m),
    intervalMs: 25,
    execRunner: async (_file, args) => {
      // hc 미설치/오류 상황 — ls부터 실패
      throw Object.assign(new Error(`spawn ${args[0]} ENOENT`), { code: 'ENOENT' });
    },
  });
  try {
    await waitFor(() => updates.length >= 1);
    // fail-safe: 예외 전파 없이 대기 라벨 방송, 워커 0
    assert.equal(updates[0]!.labels.profileName, 't-profile-a');
    assert.equal(updates[0]!.labels.lead, 'claude:sonnet-4-6');
    assert.equal(updates[0]!.workers.length, 0);

    // roster.json 중간 교체 — mtime/size 서명 변화 감지 → 핫리로드
    const before = updates.length;
    writeTempRoster(rosterPath, 't-profile-b', 'pi', 'glm-zcode/glm-5.3');
    await waitFor(
      () => updates.length > before && updates[updates.length - 1]!.labels.profileName === 't-profile-b'
    );
    assert.equal(updates[updates.length - 1]!.labels.lead, 'pi:glm-5.3');
  } finally {
    syncer.stop();
    env.restore();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('6. createLiveRosterSyncer — live 워커 수집·스코프 필터·활동 캡처·캐시·stop', async () => {
  const tmp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'lws-live-')));
  const outside = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'lws-out-')));
  const env = isolateRosterEnv(tmp);
  const rosterPath = path.join(tmp, 'roster.json');
  process.env.HERDR_ROSTER_PATH = rosterPath;
  writeTempRoster(rosterPath, 't-live-prof', 'claude', 'claude-sonnet-4-6');

  const repoDir = path.join(tmp, 'my-repo');
  fs.mkdirSync(repoDir, { recursive: true });

  const lsJson = JSON.stringify({
    terminals: [
      { address: 'tA', agent: 'pi', name: 'w-pi-x', parent: 't0', state: 'live', status: 'working', cwd: repoDir },
      { address: 'tB', agent: 'devin', name: 'coder1-x', parent: 't0', state: 'live', cwd: outside }, // 스코프 밖
      { address: 'tC', agent: 'zsh', state: 'live', cwd: repoDir }, // 셸 제외
      { address: 'tD', agent: 'node', name: 'dg-monitor', parent: 't0', state: 'live', cwd: repoDir }, // 모니터 제외
    ],
  });
  const getA = JSON.stringify({
    address: 'tA',
    agent: 'pi',
    name: 'w-pi-x',
    state: 'live',
    cwd: repoDir,
    status: { app: 'pi', state: 'working' }, // hc get은 객체형 status
    argv: ['/b/pi', '--model', 'glm-zcode/glm-5.3', '프롬프트'],
  });
  const readA = '[hc] tA · live\n⠹\n테스트 3/3 통과';

  let getCalls = 0;
  const updates: MergedLiveRoster[] = [];
  const syncer = createLiveRosterSyncer({
    repoDir: tmp,
    repoFilterDir: tmp, // 스코프 = tmp (my-repo 포함, outside 제외)
    getWatchedLead: () => null,
    onUpdate: (m) => updates.push(m),
    intervalMs: 25,
    execRunner: async (_file, args) => {
      if (args[0] === 'ls') return lsJson;
      if (args[0] === 'get') {
        getCalls++;
        return getA;
      }
      if (args[0] === 'read' && args[1] === 'tA') return readA;
      return '{}';
    },
  });
  try {
    await waitFor(() => updates.some((m) => m.workers.length === 1));
    const merged = updates.find((m) => m.workers.length === 1)!;
    const w = merged.workers[0]!;
    assert.equal(w.address, 'tA'); // 스코프·셸·모니터 필터 후 1개만
    assert.equal(w.shortModel, 'pi:glm-5.3');
    assert.equal(w.repoTag, 'my-repo');
    assert.equal(w.status, 'working'); // 객체형 status 정규화
    assert.equal(w.activity, '테스트 3/3 통과'); // [hc]·스피너 스킵 후 활동
    // 라이브 coder 오버라이드 — 대기 'devin' → 'pi:glm-5.3', lead는 대기 유지
    assert.equal(merged.labels.coder, 'pi:glm-5.3');
    assert.equal(merged.labels.lead, 'claude:sonnet-4-6');
    assert.equal(merged.activeWorkerCount, 1);

    // `hc get` 상세 캐시 — 주소별 1회만 조회
    const ticks = updates.filter((m) => m.workers.length === 1).length;
    await waitFor(() => updates.filter((m) => m.workers.length === 1).length > ticks + 1);
    assert.equal(getCalls, 1);

    // stop() 후 방송 중단
    const countAtStop = updates.length;
    syncer.stop();
    await new Promise((resolve) => setTimeout(resolve, 120));
    assert.equal(updates.length, countAtStop);
  } finally {
    syncer.stop();
    env.restore();
    fs.rmSync(tmp, { recursive: true, force: true });
    fs.rmSync(outside, { recursive: true, force: true });
  }
});

test('7. renderApp 헤더·워커 카드 — repo=<이름> / 전체 표기 + 라이브 카드 노출', () => {
  // --repo 지정 watch — repo=<이름>
  const scoped = createInitialState('watch');
  scoped.repoScopeLabel = 'my-repo';
  const scopedText = renderFrame(scoped, 148, 43).toPlainString();
  assert.ok(scopedText.includes('repo=my-repo'), 'repo scope tag in header');

  // 전역 watch — '전체' 표기로 스코프 모호성 제거
  const globalWatch = createInitialState('watch');
  const globalText = renderFrame(globalWatch, 148, 43).toPlainString();
  assert.ok(globalText.includes(' · 전체'), 'global watch scope marker');
  assert.ok(!globalText.includes('repo='), 'no repo tag without --repo');

  // 비watch(demo) — 스코프 미지정 시 표기 없음
  const demo = createInitialState('demo');
  const demoText = renderFrame(demo, 148, 43).toPlainString();
  assert.ok(!demoText.includes('repo='), 'no repo tag in demo');
  assert.ok(!demoText.includes(' · 전체'), 'no global marker in demo');

  // 라이브 워커 카드 — [repoTag] + 모델 + 1줄 활동 표시
  const withCards = createInitialState('watch');
  withCards.cards = [
    {
      name: '[my-repo] w-pi-x · pi:glm-5.3',
      model: 'pi:glm-5.3',
      progress: 0,
      description: '테스트 3/3 통과',
      status: 'running',
    },
  ];
  const cardsText = renderFrame(withCards, 148, 43).toPlainString();
  assert.ok(cardsText.includes('[my-repo] w-pi-x'), 'worker card repo tag + name');
  assert.ok(cardsText.includes('테스트 3/3 통과'), 'worker card 1-line activity');
});

test('8. 유출 스캔 — 변경 파일에 홈 절대경로·키 패턴 0건', () => {
  // src 변경 파일 — 홈 절대경로·키 패턴 전부 검사
  const srcChanged = [
    'src/backend/liveWorkerSync.ts',
    'src/types.ts',
    'src/cli.ts',
    'src/ui/renderApp.ts',
    'src/ui/renderModal.ts',
  ];
  // 이 테스트 파일 자체 — 리덕션 검증용 가짜 키 fixture가 있으므로 경로 패턴만 검사
  const testChanged = ['tests/live-worker-sync.test.ts'];
  const leakPatterns: RegExp[] = [
    /\/Users\//,
    /sk-[A-Za-z0-9_-]{16,}/,
    /AIza[0-9A-Za-z_-]{20,}/,
    /gh[pousr]_[A-Za-z0-9]{20,}/,
  ];
  const pathOnly = [/\/Users\//];
  for (const rel of [...srcChanged, ...testChanged]) {
    const content = fs.readFileSync(path.join(REPO_ROOT, rel), 'utf8');
    const patterns = testChanged.includes(rel) ? pathOnly : leakPatterns;
    for (const re of patterns) {
      assert.ok(!re.test(content), `${rel} leaks ${re.source}`);
    }
  }
});

// ---------------------------------------------------------------------------
// 회귀 방지 (verify-tui-live.md 블로킹 3건 + 경계 케이스)

/** 트랜스크립트 매퍼의 SQLite SSOT 쓰기 격리 — 임시 DB로 돌린다(복구 필수). */
function makeDbEnv(): { restore: () => void } {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), 'lws-db-'));
  const prevDb = process.env.GEMINI_TRICK_DB_PATH;
  const prevDgateDb = process.env.DGATE_DB_PATH;
  const prevDoDb = process.env.DECISION_ORCHESTRATOR_DB_PATH;
  process.env.GEMINI_TRICK_DB_PATH = path.join(base, 'decision.db');
  delete process.env.DGATE_DB_PATH;
  delete process.env.DECISION_ORCHESTRATOR_DB_PATH;
  return {
    restore: () => {
      if (prevDb === undefined) delete process.env.GEMINI_TRICK_DB_PATH;
      else process.env.GEMINI_TRICK_DB_PATH = prevDb;
      if (prevDgateDb === undefined) delete process.env.DGATE_DB_PATH;
      else process.env.DGATE_DB_PATH = prevDgateDb;
      if (prevDoDb === undefined) delete process.env.DECISION_ORCHESTRATOR_DB_PATH;
      else process.env.DECISION_ORCHESTRATOR_DB_PATH = prevDoDb;
      fs.rmSync(base, { recursive: true, force: true });
    },
  };
}

function plannerGrepStep(idx: number): TranscriptStep {
  const calls: TranscriptToolCall[] = [{ name: 'grep_search', args: { query: 'TODO' } }];
  return {
    step_index: idx,
    source: 'MODEL',
    type: 'PLANNER_RESPONSE',
    status: 'DONE',
    created_at: '2026-11-19T09:00:01Z',
    model: 'gemini-4-argon-high',
    tool_calls: calls,
  };
}

test('9. 라이브 동기화(0·1 워커) 후 applyTranscriptStep — 무동작 + cards 3슬롯 불변', async () => {
  const dbEnv = makeDbEnv();
  const tmp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'lws-tx-')));
  const env = isolateRosterEnv(tmp);
  const rosterPath = path.join(tmp, 'roster.json');
  process.env.HERDR_ROSTER_PATH = rosterPath;
  writeTempRoster(rosterPath, 't-tx-prof', 'claude', 'claude-sonnet-4-6');
  const repoDir = path.join(tmp, 'my-repo');
  fs.mkdirSync(repoDir, { recursive: true });

  let lsPayload = JSON.stringify({ terminals: [] }); // 페이즈 1: 워커 0
  const updates: MergedLiveRoster[] = [];
  const syncer = createLiveRosterSyncer({
    repoDir: tmp,
    repoFilterDir: null,
    getWatchedLead: () => null,
    onUpdate: (m) => updates.push(m),
    intervalMs: 25,
    execRunner: async (_file, args) => {
      if (args[0] === 'ls') return lsPayload;
      if (args[0] === 'get') {
        return JSON.stringify({
          address: 'tA',
          agent: 'pi',
          name: 'w-pi-x',
          state: 'live',
          cwd: repoDir,
          status: 'working',
          argv: ['/b/pi', '--model', 'glm-zcode/glm-5.3'],
        });
      }
      return '[hc] tA\n그렙 실행 중';
    },
  });
  try {
    const state = createInitialState('watch');
    const cardsSnapshot = state.cards.map((c) => ({ ...c }));

    // 페이즈 1 — 워커 0건 동기화 후 트랜스크립트 도구 호출
    await waitFor(() => updates.some((m) => m.workers.length === 0));
    const merged0 = updates.at(-1)!;
    state.liveWorkers = merged0.workers.map((w) => ({
      name: `[${w.repoTag}] ${w.name} · ${w.shortModel}`,
      model: w.shortModel,
      progress: 0,
      description: w.activity,
      status: 'running' as const,
    }));
    assert.equal(state.liveWorkers.length, 0);
    assert.doesNotThrow(() => applyTranscriptStep(state, plannerGrepStep(1)));
    assert.equal(state.cards.length, 3, 'cards 3슬롯 불변 — 라이브 목록으로 덮지 않는다');
    assert.equal(state.cards[1]!.status, 'running'); // grep_search → grep 카드 갱신

    // 페이즈 2 — 워커 1건 동기화 후에도 동일 (구 버그: cards가 1장으로 줄며 throw)
    lsPayload = JSON.stringify({
      terminals: [
        { address: 'tA', agent: 'pi', name: 'w-pi-x', parent: 't0', state: 'live', status: 'working', cwd: repoDir },
      ],
    });
    await waitFor(() => updates.some((m) => m.workers.length === 1));
    const merged1 = updates.at(-1)!;
    state.liveWorkers = merged1.workers.map((w) => ({
      name: `[${w.repoTag}] ${w.name} · ${w.shortModel}`,
      model: w.shortModel,
      progress: 0,
      description: w.activity,
      status: 'running' as const,
    }));
    assert.equal(state.liveWorkers.length, 1);
    assert.equal(state.cards.length, 3);
    assert.doesNotThrow(() => applyTranscriptStep(state, plannerGrepStep(2)));
    assert.equal(state.cards.length, 3);
    assert.ok(state.cards[1]!.description.includes('grep_search'));

    // cards 원본 슬롯 이름은 라이브 동기화로 오염되지 않는다
    assert.deepEqual(
      state.cards.map((c) => c.name),
      cardsSnapshot.map((c) => c.name)
    );

    // 렌더 스모크 — 라이브 카드 1장 상태로 프레임 그리기 무동작 + 카드 노출
    const text = renderFrame(state, 120, 43).toPlainString();
    assert.ok(text.includes('w-pi-x'), '라이브 워커 카드 렌더');
  } finally {
    syncer.stop();
    env.restore();
    dbEnv.restore();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('10. working→done 전환 — hc get 캐시 정적 유지 + 매 틱 ls 상태 병합', async () => {
  const tmp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'lws-done-')));
  const env = isolateRosterEnv(tmp);
  const rosterPath = path.join(tmp, 'roster.json');
  process.env.HERDR_ROSTER_PATH = rosterPath;
  writeTempRoster(rosterPath, 't-done-prof', 'claude', 'claude-sonnet-4-6');
  const repoDir = path.join(tmp, 'my-repo');
  fs.mkdirSync(repoDir, { recursive: true });

  let lsStatus = 'working'; // hc ls 틱마다 바뀌는 실측 상태
  let getCalls = 0;
  const updates: MergedLiveRoster[] = [];
  const syncer = createLiveRosterSyncer({
    repoDir: tmp,
    repoFilterDir: null,
    getWatchedLead: () => null,
    onUpdate: (m) => updates.push(m),
    intervalMs: 25,
    execRunner: async (_file, args) => {
      if (args[0] === 'ls') {
        return JSON.stringify({
          terminals: [
            { address: 'tA', agent: 'pi', name: 'w-pi-x', parent: 't0', state: 'live', status: lsStatus, cwd: repoDir },
          ],
        });
      }
      if (args[0] === 'get') {
        getCalls++;
        return JSON.stringify({
          address: 'tA',
          agent: 'pi',
          name: 'w-pi-x',
          state: 'live',
          cwd: repoDir,
          status: 'working', // hc get은 계속 working — 상태 갱신은 ls 병합으로만
          argv: ['/b/pi', '--model', 'glm-zcode/glm-5.3'],
        });
      }
      return '[hc] tA\n작업 중';
    },
  });
  try {
    await waitFor(() => updates.some((m) => m.workers[0]?.status === 'working'));
    assert.equal(getCalls, 1, '상세는 주소별 1회만 조회(캐시)');

    // ls 상태가 working→done으로 바뀌면 다음 틱에 즉시 반영되어야 한다
    lsStatus = 'done';
    await waitFor(() => updates.some((m) => m.workers[0]?.status === 'done'));
    const doneMerged = [...updates].reverse().find((m) => m.workers[0]?.status === 'done')!;
    assert.equal(doneMerged.workers[0]!.repoTag, 'my-repo');
    assert.equal(getCalls, 1, '전환 후에도 hc get 재조회 없음 — 상태는 ls에서 병합');
    assert.equal(doneMerged.workers[0]!.shortModel, 'pi:glm-5.3', '캐시된 argv 정적 정보 유지');
  } finally {
    syncer.stop();
    env.restore();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('11. malformed hc ls JSON — 이전 워커 보존(빈 방송·허위 종료 금지)', async () => {
  const tmp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'lws-badjson-')));
  const env = isolateRosterEnv(tmp);
  const rosterPath = path.join(tmp, 'roster.json');
  process.env.HERDR_ROSTER_PATH = rosterPath;
  writeTempRoster(rosterPath, 't-bad-prof', 'claude', 'claude-sonnet-4-6');
  const repoDir = path.join(tmp, 'my-repo');
  fs.mkdirSync(repoDir, { recursive: true });

  const goodLs = JSON.stringify({
    terminals: [
      { address: 'tA', agent: 'pi', name: 'w-pi-x', parent: 't0', state: 'live', status: 'working', cwd: repoDir },
    ],
  });
  const badLs = '{"terminals": [ {"address": "tA", "agent"'; // 잘린 JSON
  let lsPayload = goodLs;
  const updates: MergedLiveRoster[] = [];
  const syncer = createLiveRosterSyncer({
    repoDir: tmp,
    repoFilterDir: null,
    getWatchedLead: () => null,
    onUpdate: (m) => updates.push(m),
    intervalMs: 25,
    execRunner: async (_file, args) => {
      if (args[0] === 'ls') return lsPayload;
      if (args[0] === 'get') {
        return JSON.stringify({
          address: 'tA',
          agent: 'pi',
          name: 'w-pi-x',
          state: 'live',
          cwd: repoDir,
          argv: ['/b/pi', '--model', 'glm-zcode/glm-5.3'],
        });
      }
      return '[hc] tA';
    },
  });
  try {
    await waitFor(() => updates.some((m) => m.workers.length === 1));
    const countWithWorker = updates.filter((m) => m.workers.length === 1).length;

    // 이후 틱 2회 이상을 malformed JSON으로 — 이전 워커가 그대로 보존되어야 한다
    lsPayload = badLs;
    await waitFor(() => updates.length > countWithWorker + 1);
    const after = updates.slice(countWithWorker);
    assert.ok(after.length >= 1, 'malformed 이후에도 방송은 계속된다');
    for (const m of after) {
      assert.equal(m.workers.length, 1, 'malformed ls로 워커 0건 방송 금지');
      assert.equal(m.workers[0]!.address, 'tA');
    }
  } finally {
    syncer.stop();
    env.restore();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

// 회귀: '{}', '{"terminals":null}'은 스키마 무효 — 이전 워커 보존.
// 명시적 '{"terminals":[]}'만 rosters 클리어.
test("11.5 ls JSON 스키마 — '{}'·'{}' 아닌 terminals 무효 vs 명시적 빈 목록", async () => {
  const tmp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'lws-schema-')));
  const env = isolateRosterEnv(tmp);
  const rosterPath = path.join(tmp, 'roster.json');
  process.env.HERDR_ROSTER_PATH = rosterPath;
  writeTempRoster(rosterPath, 't-schema', 'claude', 'claude-sonnet-4-6');
  const repoDir = path.join(tmp, 'my-repo');
  fs.mkdirSync(repoDir, { recursive: true });

  const goodLs = JSON.stringify({
    terminals: [
      { address: 'tA', agent: 'pi', name: 'w-pi-x', parent: 't0', state: 'live', status: 'working', cwd: repoDir },
    ],
  });
  let lsPayload = goodLs;
  const updates: MergedLiveRoster[] = [];
  const syncer = createLiveRosterSyncer({
    repoDir: tmp,
    repoFilterDir: null,
    getWatchedLead: () => null,
    onUpdate: (m) => updates.push(m),
    intervalMs: 25,
    execRunner: async (_file, args) => {
      if (args[0] === 'ls') return lsPayload;
      if (args[0] === 'get') {
        return JSON.stringify({
          address: 'tA',
          agent: 'pi',
          name: 'w-pi-x',
          state: 'live',
          cwd: repoDir,
          argv: ['/b/pi', '--model', 'glm-zcode/glm-5.3'],
        });
      }
      return '[hc] tA';
    },
  });
  try {
    await waitFor(() => updates.some((m) => m.workers.length === 1));

    // 1) '{}' — terminals 누락 → 무효 스키마, 이전 워커 보존
    lsPayload = '{}';
    await waitFor(() => updates.length > 1, 1000).catch(() => {});
    await new Promise((r) => setTimeout(r, 100));
    assert.ok(updates.length >= 1);
    assert.ok(
      updates.every((m) => m.workers.length === 1),
      "'{}' 응답으로 워커 0건 방송 금지"
    );

    // 2) '{"terminals":null}' — terminals 비배열 → 무효 스키마, 이전 워커 보존
    lsPayload = '{"terminals":null}';
    await new Promise((r) => setTimeout(r, 120));
    assert.ok(
      updates.every((m) => m.workers.length === 1),
      "'{\"terminals\":null}' 응답으로 워커 0건 방송 금지"
    );

    // 3) 명시적 '{"terminals":[]}' — 유효한 빈 목록, rosters 클리어
    lsPayload = '{"terminals":[]}';
    await waitFor(() => updates.some((m) => m.workers.length === 0));
    assert.ok(
      updates.slice(-3).some((m) => m.workers.length === 0),
      "명시적 '{\"terminals\":[]}' 는 워커 클리어"
    );
  } finally {
    syncer.stop();
    env.restore();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test('12. 헤더 80/100/120/148열 — repo 라벨·우측 카운터 충돌 없음', () => {
  for (const cols of [80, 100, 120, 148]) {
    const state = createInitialState('watch');
    state.repoScopeLabel = 'repo-smoke';
    const header = renderFrame(state, cols, 43)
      .toPlainString()
      .split('\n')
      .find((l) => l.includes('DGATE TUI'))!;
    assert.ok(header.includes('repo=repo-smoke'), `${cols}열: repo 라벨 온전`);
    assert.ok(
      !/repo=(LEAD|WORK|VERIF|ADV)/.test(header),
      `${cols}열: repo 라벨이 카운터를 덮지 않는다`
    );
    // 카운터가 그려지는 폭에서는 배지도 온전해야 한다
    if (header.includes('LEAD BUILDS')) {
      assert.ok(/LEAD BUILDS \[\d+\]/.test(header), `${cols}열: LEAD BUILDS 카운터 온전`);
      const repoIdx = header.indexOf('repo=repo-smoke');
      const leadIdx = header.indexOf('LEAD BUILDS');
      assert.ok(repoIdx >= 0 && leadIdx > repoIdx, `${cols}열: 라벨→카운터 순서(간격 유지)`);
    }
    // 120열 — 구 버그 재현 지점: 'repo=LEAD BUILDS [0]' 로 덮였던 자리
    if (cols === 120) {
      assert.ok(header.includes('LEAD BUILDS'), '120열: 카운터 노출');
      assert.ok(!header.includes('repo=LEAD BUILDS'), '120열 충돌 재현 없음');
    }
  }
});

test('13. 경계 — 박스형 [hc] 푸터·겹선 테두리·루트 스코프·ksh/dash/pwsh 제외', () => {
  // 박스로 감싼 [hc] 상태줄도 활동 줄이 아니라 스킵 — 가장자리 장식 제거 후 판정
  assert.equal(extractLastActivityLine('│ [hc] idle │'), null);
  assert.equal(extractLastActivityLine('║ [hc] t18s · live ║'), null);
  assert.equal(
    extractLastActivityLine('[hc] t18s\n╔════════╗\n║ 패치 적용 중 ║\n╚════════╝'),
    '패치 적용 중'
  );
  // 겹선(double-line) 테두리 전용 줄 — 본문 없음으로 스킵
  assert.equal(extractLastActivityLine('╚════════╝'), null);
  assert.equal(extractLastActivityLine('╠════╣'), null);
  assert.equal(extractLastActivityLine('╔════════╗\n╚════════╝'), null);

  // 루트('/') 스코프 — 모든 절대경로 하위를 포함해야 한다('//' 사고 방지)
  assert.equal(matchesRepoScope('/definitely/any/path', '/'), true);
  assert.equal(matchesRepoScope('/', '/'), true);
  assert.equal(matchesRepoScope(null, '/'), false); // cwd 미상 + 스코프 지정

  // 셸 확장 제외 — w- 접두가 있어도 ksh/dash/pwsh는 워커가 아니다
  for (const sh of ['ksh', 'dash', 'pwsh']) {
    assert.equal(
      isWorkerTerminal({ address: 'a', agent: sh, name: `w-${sh}`, state: 'live', parent: 't0' }),
      false,
      `${sh} 셸 제외`
    );
  }
});

// ---------------------------------------------------------------------------
// 동적 워커 와이어(1..N) + 라이브 카드 모션 + 역할 패널 동기화 (TUI Dynamic Wires)

const WIRE_CHARS = new Set(['─', '┌', '┐', '┬', '┴', '┼', '└', '┘', '│']);

/** 라이브 워커 N개가 붙은 watch 상태 — geometry 검증용은 idle(펄스 0)로 만든다. */
function makeLiveWatchState(n: number, status: 'running' | 'idle' = 'idle') {
  const state = createInitialState('watch');
  state.phase = '대기 중';
  state.swarmActive = status === 'running' ? n : 0;
  state.liveWorkers = Array.from({ length: n }, (_, i) => ({
    name: `wx${i + 1}`,
    model: 'pi:glm-5.3',
    progress: 0,
    description: `활동 ${i + 1}`,
    status,
  }));
  return state;
}

/** 148×43은 항상 전체 레이아웃 — 카드 슬롯 검증용 좁혀진 타입. */
function fullLayout148(visibleCardCount: number): FullLayout {
  return computeLayout(148, 43, visibleCardCount) as FullLayout;
}

test('14. 동적 워커 카드·와이어 — 1/2/3/4 워커 카드 수·스템·빈 슬롯 댕글링 없음', () => {
  // (a) 1..4 워커 — 카드 수·이름·각 카드로 향하는 스템(접합 문자) 존재
  for (const n of [1, 2, 3, 4]) {
    const state = makeLiveWatchState(n);
    const buf = renderFrame(state, 148, 43);
    const L = fullLayout148(n);
    assert.equal(L.cards.length, n, `${n}워커 → 카드 ${n}장`);
    const text = buf.toPlainString();
    for (let i = 0; i < n; i++) {
      assert.ok(text.includes(`wx${i + 1}`), `${n}워커: wx${i + 1} 카드 노출`);
    }
    for (let i = 0; i < n; i++) {
      const xs = L.cards[i]!.x + Math.floor(L.cards[i]!.w / 2);
      const outCh = buf.getCell(xs, L.fanOutY)?.ch;
      const inCh = buf.getCell(xs, L.fanInY)?.ch;
      assert.ok(outCh !== undefined && WIRE_CHARS.has(outCh), `${n}워커 카드${i} 팬아웃 스템: ${outCh}`);
      assert.ok(inCh !== undefined && WIRE_CHARS.has(inCh), `${n}워커 카드${i} 팬인 스템: ${inCh}`);
    }
  }

  // (b) 카드 1장 — 가로 레일 없이 곧장 세로 스템(빈 슬롯로 가로선 금지)
  const one = makeLiveWatchState(1);
  const oneBuf = renderFrame(one, 148, 43);
  const L1 = fullLayout148(1);
  const cx1 = L1.centerX;
  assert.equal(oneBuf.getCell(cx1, L1.fanOutY)?.ch, '│', '1워커: 중심 세로 스템');
  assert.equal(oneBuf.getCell(cx1 + 1, L1.fanOutY)?.ch, ' ', '1워커: 우측 가로 레일 없음');
  assert.equal(oneBuf.getCell(cx1 - 1, L1.fanOutY)?.ch, ' ', '1워커: 좌측 가로 레일 없음');

  // (c) 2워커 — 비어 있는 3번 슬롯 위치에 와이어가 남지 않는다(댕글링 금지)
  const two = makeLiveWatchState(2);
  const twoBuf = renderFrame(two, 148, 43);
  const L2 = fullLayout148(2);
  const L3 = fullLayout148(3);
  const x3 = L3.cards[2]!.x + Math.floor(L3.cards[2]!.w / 2);
  assert.equal(twoBuf.getCell(x3, L2.fanOutY)?.ch, ' ', '2워커: 빈 3번 슬롯 팬아웃 없음');
  assert.equal(twoBuf.getCell(x3, L2.fanInY)?.ch, ' ', '2워커: 빈 3번 슬롯 팬인 없음');
  const twoXs = L2.cards.map((r) => r.x + Math.floor(r.w / 2));
  for (const probe of [x3 - 1, x3 - 3, Math.floor((twoXs[1]! + x3) / 2)]) {
    assert.equal(twoBuf.getCell(probe, L2.fanOutY)?.ch, ' ', `2워커: 카드2 우측 여백(${probe})에 레일 없음`);
  }

  // (d) 5워커 — 최대 4카드 + '+1개 더' 오버플로 배지, 5번째 워커는 캔버스 미노출
  const five = makeLiveWatchState(5);
  const fiveText = renderFrame(five, 148, 43).toPlainString();
  assert.equal(fullLayout148(5).cards.length, 4, '5워커 → 카드 4장 상한');
  assert.ok(fiveText.includes('+1개 더'), '오버플로 배지 노출');
  assert.ok(!fiveText.includes('wx5'), '5번째 워커는 메인 캔버스에 그리지 않음');
});

test('15. 실행 중 워커 카드 라이브 모션 — 펄스·부재 활성 바·스피너가 simTime으로 움직인다', () => {
  // (a) 2번째 워커만 실행 — 프레임이 simTime에 따라 변한다(정지된 0% 바가 아님)
  const state = makeLiveWatchState(2);
  const baseCards = state.liveWorkers!;
  state.liveWorkers = [
    { ...baseCards[0]!, status: 'idle' as const },
    { ...baseCards[1]!, status: 'running' as const },
  ];
  const f1 = renderFrame({ ...state, simTime: 1.0 }, 148, 43).toPlainString();
  const f2 = renderFrame({ ...state, simTime: 2.7 }, 148, 43).toPlainString();
  assert.notEqual(f1, f2, '실행 중 2번째 워커 → simTime 변화가 프레임에 보인다');
  assert.ok(f2.includes('RUN'), '부재 진행률 → 활성 바 RUN 라벨');
  assert.ok(/[⠋⠙⠹⠸⠼⠴⠦⠧] 실행 중/.test(f2), '상태 줄 브라일 스피너');
  assert.ok(f1.includes('▪ 대기 중'), '대기 카드는 정지 상태');

  // (b) 실행 카드로 향하는 팬아웃 레일에만 ● 펄스 — 대기 카드 방향 레일은 잔잔
  const L2 = fullLayout148(2);
  const xs = L2.cards.map((r) => r.x + Math.floor(r.w / 2));
  const railRow1 = f1.split('\n')[L2.fanOutY]!;
  const railRow2 = f2.split('\n')[L2.fanOutY]!;
  assert.ok(railRow1.includes('●') || railRow2.includes('●'), '팬아웃 레일에 ● 펄스 헤드');
  for (const [row, label] of [[railRow1, 't=1.0'], [railRow2, 't=2.7']] as const) {
    const leftSeg = row.slice(xs[0]!, L2.centerX); // 대기 카드1 방향 — 펄스 없음
    assert.ok(!leftSeg.includes('●'), `${label}: 대기 카드 방향 레일에 펄스 0개`);
  }
  const fanInRow1 = f1.split('\n')[L2.fanInY]!;
  const fanInRow2 = f2.split('\n')[L2.fanInY]!;
  assert.ok(fanInRow1.includes('•') || fanInRow2.includes('•'), '팬인 레일에 • 되돌림 펄스');

  // (c) 가운데(2번째) 워커 펄스 — 3카드 배치에서 xs[1]===cx여도 접합점에 ●가 살아있다
  const three = makeLiveWatchState(3);
  const baseThree = three.liveWorkers!;
  three.liveWorkers = [
    { ...baseThree[0]!, status: 'idle' as const },
    { ...baseThree[1]!, status: 'running' as const },
    { ...baseThree[2]!, status: 'idle' as const },
  ];
  const L3 = fullLayout148(3);
  const midX = L3.cards[1]!.x + Math.floor(L3.cards[1]!.w / 2);
  assert.equal(midX, L3.centerX, '전제: 148열 3카드 중앙 카드 중심 === 파이프 중심축');
  for (const t of [0.3, 1.1, 2.5, 4.2]) {
    const buf = renderFrame({ ...three, simTime: t }, 148, 43);
    assert.equal(buf.getCell(midX, L3.fanOutY)?.ch, '●', `t=${t}: 가운데 워커 접합점 펄스 상시 노출`);
  }
});

test('16. 라이브 역할 패널 동기화 — verifier/advisor/lead 활성화·완료·침착 복귀', () => {
  const state = createInitialState('watch');
  const trail = createLiveRoleTrail();
  const mergedOf = (workers: LiveWorkerInfo[]): MergedLiveRoster => ({
    labels: makeBaseLabels(),
    leadModelName: 'pi:glm-5.3',
    leadModelFlag: '--model glm-zcode/glm-5.3',
    verifierModelName: 'pi:deepseek-v4.1-flash',
    verifierModelFlag: 'pi:deepseek-v4.1-flash',
    workers,
    activeWorkerCount: workers.filter((w) => w.status !== 'done').length,
  });

  // (a) verifier + advisor + 일반 워커 실행 → 각 역할 패널 활성
  applyLiveRolePanels(
    state,
    mergedOf([
      makeWorker({ slot: 'verifier', address: 'tV', status: 'working', activity: '12/12 검증 통과' }),
      makeWorker({ slot: 'advisor', address: 'tC', status: 'working', activity: '계획 조언 중' }),
      makeWorker({ slot: 'coder', address: 'tW', status: 'working' }),
    ]),
    trail
  );
  assert.equal(state.neonState, 'run', 'verifier 실행 → neonState run');
  assert.equal(state.neonProgress, 0, '라이브 검증 부재 진행률 → 0(활성 바 렌더)');
  assert.ok(state.neonLabel.includes('w-pi-x'), 'neon 라벨에 verifier 활동');
  assert.equal(state.advisorState, '자문 중', 'advisor 실행 → 자문 중');
  assert.equal(state.activeTrigger, 0, 'Advisor→Lead 와이어 트리거 활성');
  assert.equal(state.phase, '워커 실행 중', '워커 실행 중 Lead 표시');
  const runFrame = renderFrame({ ...state, simTime: 1.0, liveWorkers: [{ name: 'w', model: 'm', progress: 0, description: 'd', status: 'running' }] }, 148, 43).toPlainString();
  assert.ok(runFrame.includes('자문 중'), 'Advisor 패널 자문 중 렌더');

  // (b) lead 워커 실행 + verifier 완료(done) → phase 작업 중 + neon 잠깐 pass
  applyLiveRolePanels(
    state,
    mergedOf([
      makeWorker({ slot: 'lead', address: 'tL', status: 'working' }),
      makeWorker({ slot: 'verifier', address: 'tV', status: 'done' }),
    ]),
    trail
  );
  assert.equal(state.phase, '작업 중', 'lead 실행 → 작업 중');
  assert.equal(state.neonState, 'pass', 'verifier 완료 → pass');
  assert.equal(state.neonProgress, 100);
  applyLiveRolePanels(state, mergedOf([]), trail);
  assert.equal(state.neonState, 'idle', 'verifier 소멸 → idle 복귀');

  // (c) 전부 종료 상태 검증 — 침착 대기 복귀
  assert.equal(state.advisorState, '대기 중');
  assert.equal(state.activeTrigger, -1);
  assert.equal(state.phase, '대기 중');
  state.liveWorkers = [];
  state.swarmActive = 0;
  const calm1 = renderFrame({ ...state, simTime: 1.0 }, 148, 43).toPlainString();
  const calm2 = renderFrame({ ...state, simTime: 2.7 }, 148, 43).toPlainString();
  assert.equal(calm1, calm2, '0 실행 워커 → simTime만 바꾼 프레임은 동일(펄스·스피너 0)');
  assert.ok(!/[⠋⠙⠹⠸⠼⠴⠦⠧]/.test(calm1), '침착 상태에 스피너 없음');
});

test('18. per-worker 실측 경과 MM:SS — formatElapsedMmss 경계 + 라이브 카드 렌더', () => {
  // (a) 순수 포매터 — 0/84/3661초 및 음수·NaN 클램프
  assert.equal(formatElapsedMmss(0), '00:00');
  assert.equal(formatElapsedMmss(84), '01:24');
  assert.equal(formatElapsedMmss(3661), '01:01:01');
  assert.equal(formatElapsedMmss(-5), '00:00');
  assert.equal(formatElapsedMmss(Number.NaN), '00:00');

  // (b) 실행 중 라이브 카드 — elapsedSec 84초면 프레임에 01:24, 미지정이면 RUN 폴백
  const state = createInitialState('watch');
  state.phase = '워커 실행 중';
  state.swarmActive = 2;
  state.liveWorkers = [
    { name: 'wA', model: 'pi:glm-5.3', progress: 0, description: '활동 A', status: 'running', elapsedSec: 84 },
    { name: 'wB', model: 'pi:glm-5.3', progress: 0, description: '활동 B', status: 'running' },
  ];
  const text = renderFrame({ ...state, simTime: 1.2 }, 148, 43).toPlainString();
  assert.ok(text.includes('01:24'), 'elapsedSec=84 → 01:24 노출');
  assert.ok(/⠙? 01:24/.test(text), '스피너+MM:SS 조합 노출');

  const noElapsed = renderFrame({ ...state, liveWorkers: state.liveWorkers.map((c) => ({ ...c, elapsedSec: undefined })) }, 148, 43).toPlainString();
  assert.ok(noElapsed.includes(' RUN'), 'elapsedSec 미지정 → 기존 RUN 폴백 유지');
  assert.ok(!noElapsed.includes('01:24'), '미지정 카드에는 MM:SS 미노출');

  // (c) 완료 카드 — 동결 경과가 ✓ 완료 · MM:SS 로 표시
  const doneState = createInitialState('watch');
  doneState.swarmActive = 0;
  doneState.liveWorkers = [
    { name: 'wD', model: 'pi:glm-5.3', progress: 100, description: '완료됨', status: 'done', elapsedSec: 84 },
  ];
  const doneText = renderFrame(doneState, 148, 43).toPlainString();
  assert.ok(doneText.includes('✓ 완료 · 01:24'), '완료 카드 동결 경과 표기');

  // (d) 침착 대기 — 실행 워커 0이면 elapsed 도입 후에도 프레임 동일
  const calm = createInitialState('watch');
  calm.phase = '대기 중';
  calm.liveWorkers = [];
  const c1 = renderFrame({ ...calm, simTime: 1.0 }, 148, 43).toPlainString();
  const c2 = renderFrame({ ...calm, simTime: 2.7 }, 148, 43).toPlainString();
  assert.equal(c1, c2, '0 실행 워커 → 침착 프레임 불변');
});

test('17. 라이브 동기 카드 모션 게이트 — watch 폴백 cards(자리표)는 정지 유지', () => {
  // 라이브 목록 없음(liveWorkers=[]) → 폴백 cards가 running 상태여도 모션 없음
  const state = createInitialState('watch');
  state.phase = '완료';
  state.neonState = 'pass';
  state.liveWorkers = [];
  state.swarmActive = 0;
  state.cards = state.cards.map((c, i) =>
    i === 1 ? { ...c, status: 'running' as const, progress: 0 } : c
  );
  const f1 = renderFrame({ ...state, simTime: 1.0 }, 148, 43).toPlainString();
  const f2 = renderFrame({ ...state, simTime: 2.7 }, 148, 43).toPlainString();
  assert.equal(f1, f2, '라이브 워커 0명 → 폴백 running 카드여도 프레임 고정');
  assert.ok(f1.includes('◉ 실행 중'), '폴백 running 카드는 정적 ◉ 실행 중 (스피너 아님)');
  assert.ok(!f1.includes(' RUN'), '라이브 부재 활성 바(RUN) 미노출');
});

// ---------------------------------------------------------------------------
// 최상위 활성 AI 에이전트 세션 관측 확장 (w-top-level-sessions)

test('18. isActiveTopLevelSession / isObservableTerminal — 최상위 working AI 세션 관측', () => {
  // 긍정 — 워커 규약 밖 최상위 working 세션도 관측된다
  for (const agent of ['antigravity', 'pi', 'claude', 'codex', 'devin', 'gemini', 'agy']) {
    const t = { address: 't155', agent, name: `paperwork-${agent}`, state: 'live', status: 'working' };
    assert.equal(isActiveTopLevelSession(t), true, agent);
    assert.equal(isObservableTerminal(t), true, agent);
  }
  // kakao-manual-bot pi — 워커가 아니어도 관측
  const pi = { address: 't1', agent: 'pi', name: 'kakao-manual-bot', state: 'live', status: 'working' };
  assert.equal(isWorkerTerminal(pi), false);
  assert.equal(isObservableTerminal(pi), true);
  // 기존 워커 판정 불변 — 관측 확장이 isWorkerTerminal을 바꾸지 않는다
  assert.equal(isObservableTerminal({ address: 'a', agent: 'pi', name: 'w-x', state: 'live' }), true);

  // status 엄격 — idle/done/undefined 불가
  for (const status of ['idle', 'done', undefined]) {
    assert.equal(isActiveTopLevelSession({ address: 't', agent: 'antigravity', name: 's', state: 'live', status }), false);
    assert.equal(isActiveTopLevelSession({ address: 't', agent: 'pi', name: 's', state: 'live', status }), false);
  }
  // state 불일치 · parent 존재
  assert.equal(isActiveTopLevelSession({ address: 't', agent: 'pi', name: 's', state: 'done', status: 'working' }), false);
  assert.equal(isActiveTopLevelSession({ address: 't', agent: 'pi', name: 's', state: 'live', status: 'working', parent: 'lead' }), false);
  // 셸·런타임·모니터·GUI 제외
  for (const agent of ['zsh', 'bash', 'sh', 'node', 'python3', 'powershell.exe', 'dgate-gui']) {
    assert.equal(isActiveTopLevelSession({ address: 't', agent, name: 's', state: 'live', status: 'working' }), false, agent);
  }
  for (const name of ['dg-monitor', 'dg-monitor-x', 'mon-x', 'dgate-gui-main']) {
    assert.equal(isActiveTopLevelSession({ address: 't', agent: 'antigravity', name, state: 'live', status: 'working' }), false, name);
    assert.equal(isObservableTerminal({ address: 't', agent: 'antigravity', name, state: 'live', status: 'working' }), false, name);
  }
});

test('18.5 최상위 세션 Lead 정규화 — 이름·shortModel 폴백', () => {
  // 이름·slot·shortModel — '[paperwork-operations] lead · antigravity' 카드용
  const w = normalizeTopLevelWorker(
    makeWorker({ slot: 'coder', shortModel: 'custom', badge: 'custom (실행 중)', modelName: 'custom', name: 't155' }),
    { address: 't155', agent: 'antigravity', name: 'paperwork-operations', state: 'live', status: 'working' }
  );
  assert.equal(w.slot, 'lead');
  assert.equal(w.name, 'paperwork-operations');
  assert.equal(w.shortModel, 'antigravity');
  assert.equal(w.badge, 'antigravity (실행 중)');
  assert.equal(w.modelName, 'antigravity');
  // 이름 없으면 'lead'
  const w2 = normalizeTopLevelWorker(
    makeWorker({ name: 't156', shortModel: 'pi', modelName: 'pi' }),
    { address: 't156', agent: 'pi', state: 'live', status: 'working' }
  );
  assert.equal(w2.slot, 'lead');
  assert.equal(w2.name, 'lead');
  assert.equal(w2.shortModel, 'pi');
});

test('18.9 syncer — 최상위 working antigravity/pi만 Lead로 관측, idle·셸·모니터·GUI 제외', async () => {
  const tmp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'lws-toplevel-')));
  const env = isolateRosterEnv(tmp);
  const rosterPath = path.join(tmp, 'roster.json');
  process.env.HERDR_ROSTER_PATH = rosterPath;
  writeTempRoster(rosterPath, 't-top', 'claude', 'claude-sonnet-4-6');
  const repoDir = path.join(tmp, 'my-repo');
  fs.mkdirSync(repoDir, { recursive: true });

  const ls = JSON.stringify({
    terminals: [
      { address: 't155', agent: 'antigravity', name: 'paperwork-operations', state: 'live', status: 'working', cwd: repoDir },
      { address: 't156', agent: 'pi', name: 'kakao-manual-bot', state: 'live', status: 'working', cwd: repoDir },
      { address: 't157', agent: 'antigravity', name: 'idle-session', state: 'live', status: 'idle', cwd: repoDir },
      { address: 't158', agent: 'antigravity', name: 'done-session', state: 'live', status: 'done', cwd: repoDir },
      { address: 't159', agent: 'antigravity', name: 'no-status-session', state: 'live', cwd: repoDir },
      { address: 't160', agent: 'zsh', name: 'shell', state: 'live', status: 'working', cwd: repoDir },
      { address: 't161', agent: 'python3', name: 'py', state: 'live', status: 'working', cwd: repoDir },
      { address: 't162', agent: 'node', name: 'dg-monitor', state: 'live', status: 'working', cwd: repoDir },
      { address: 't163', agent: 'node', name: 'dgate-gui-main', state: 'live', status: 'working', cwd: repoDir },
    ],
  });
  const updates: MergedLiveRoster[] = [];
  const syncer = createLiveRosterSyncer({
    repoDir: tmp,
    repoFilterDir: null,
    getWatchedLead: () => null,
    onUpdate: (m) => updates.push(m),
    intervalMs: 25,
    execRunner: async (_file, args) => {
      if (args[0] === 'ls') return ls;
      if (args[0] === 'get') {
        return JSON.stringify({ address: args[1]!, agent: 'antigravity', argv: ['antigravity'] });
      }
      return '[hc] ok';
    },
  });
  try {
    await waitFor(() => updates.length > 0);
    const m = updates.at(-1)!;
    assert.equal(m.workers.length, 2, 'working 최상위 antigravity+pi만 관측');
    const byAddr = new Map(m.workers.map((w) => [w.address, w]));
    const ant = byAddr.get('t155')!;
    assert.equal(ant.slot, 'lead');
    assert.equal(ant.name, 'paperwork-operations');
    assert.equal(ant.shortModel, 'antigravity');
    const pi = byAddr.get('t156')!;
    assert.equal(pi.slot, 'lead');
    assert.equal(pi.name, 'kakao-manual-bot');
    assert.equal(pi.shortModel, 'pi');
    for (const addr of ['t157', 't158', 't159', 't160', 't161', 't162', 't163']) {
      assert.equal(byAddr.has(addr), false, `${addr} 미관측`);
    }
  } finally {
    syncer.stop();
    env.restore();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

// ---------- TUI 세션 스코프 기본값 (w-tui-session-scope) ----------

test('19. resolveTuiScope — 5가지 스코프 판정 (기본 세션/레포·--all·--repo·--session)', () => {
  const cwd = path.join('/tmp', 'Gemini_trick');
  const envOther: NodeJS.ProcessEnv = {};
  const envSession: NodeJS.ProcessEnv = { HC_TERMINAL_ID: '  t1bg  ' };

  // ① 기본 + HC_TERMINAL_ID — 현재 세션 스코프, 라벨 '레포명·세션ID'
  const s1 = resolveTuiScope({}, envSession, cwd);
  assert.equal(s1.kind, 'session');
  assert.equal(s1.sessionId, 't1bg'); // trim 적용
  assert.equal(s1.repoFilterDir, null);
  assert.equal(s1.scopeLabel, 'Gemini_trick·t1bg');

  // ② 기본 + HC_TERMINAL_ID 부재 — 현재 cwd 레포 스코프
  const s2 = resolveTuiScope({}, envOther, cwd);
  assert.equal(s2.kind, 'repo');
  assert.equal(s2.sessionId, null);
  assert.equal(s2.repoFilterDir, path.resolve(cwd));
  assert.equal(s2.scopeLabel, 'Gemini_trick');

  // ③ --all — 전역 (라벨·필터 없음)
  const s3 = resolveTuiScope({ all: true }, envSession, cwd); // env 무시 우선순위
  assert.deepEqual(s3, { kind: 'all', repoFilterDir: null, sessionId: null, scopeLabel: null });

  // ④ --repo <dir> — 해당 레포 (HC_TERMINAL_ID 있어도 --repo 우선은 아님 — --all 다음, env 전)
  const s4 = resolveTuiScope({ repoDir: path.join('rel', 'my-repo') }, envOther, cwd);
  assert.equal(s4.kind, 'repo');
  assert.equal(s4.repoFilterDir, path.resolve(path.join('rel', 'my-repo')));
  assert.equal(s4.scopeLabel, 'my-repo');

  // ⑤ --session <id> — 세션 스코프 (+ --repo 병행 시 레포 필터도 유지)
  const s5 = resolveTuiScope({ sessionId: 't155' }, envOther, cwd);
  assert.equal(s5.kind, 'session');
  assert.equal(s5.sessionId, 't155');
  assert.equal(s5.repoFilterDir, null);
  assert.equal(s5.scopeLabel, 'session=t155');
  const s5b = resolveTuiScope({ sessionId: 't155', repoDir: '/x/my-repo' }, envOther, cwd);
  assert.equal(s5b.repoFilterDir, path.resolve('/x/my-repo'));
});

test('20. matchesSessionScope — 같은 레포 안 두 세션(t155/t182) 자식 워커 격리', () => {
  const t155 = { address: 't155', agent: 'pi', state: 'live', status: 'working' };
  const w1 = { address: 'w-1', agent: 'pi', name: 'w-coder1', parent: 't155', state: 'live' };
  const t182 = { address: 't182', agent: 'pi', state: 'live', status: 'working' };
  const w2 = { address: 'w-2', agent: 'claude', name: 'w-coder2', parent: 't182', state: 'live' };

  // 스코프 미지정 — 전역 (기존 동작 100% 불변)
  assert.equal(matchesSessionScope(t155, undefined), true);
  assert.equal(matchesSessionScope(w2, undefined), true);

  // t155 스코프 — t155 본인 + 자식 w-1만, t182/w-2는 제외
  assert.equal(matchesSessionScope(t155, 't155'), true);
  assert.equal(matchesSessionScope(w1, 't155'), true);
  assert.equal(matchesSessionScope(t182, 't155'), false);
  assert.equal(matchesSessionScope(w2, 't155'), false);

  // t182 스코프 — 반대 격리
  assert.equal(matchesSessionScope(t182, 't182'), true);
  assert.equal(matchesSessionScope(w2, 't182'), true);
  assert.equal(matchesSessionScope(t155, 't182'), false);
  assert.equal(matchesSessionScope(w1, 't182'), false);
});

test('21. createLiveRosterSyncer sessionId — 같은 레포 내 타 세션 워커 필터, 미지정 시 전부', async () => {
  const tmp = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'lws-sess-')));
  const env = isolateRosterEnv(tmp);
  const rosterPath = path.join(tmp, 'roster.json');
  process.env.HERDR_ROSTER_PATH = rosterPath;
  writeTempRoster(rosterPath, 't-sess-prof', 'claude', 'claude-sonnet-4-6');

  // 같은 레포(cwd 동일) 안에 t155+w-1, t182+w-2 두 세션
  const repoDir = path.join(tmp, 'my-repo');
  fs.mkdirSync(repoDir, { recursive: true });
  const lsJson = JSON.stringify({
    terminals: [
      { address: 't155', agent: 'pi', state: 'live', status: 'working', cwd: repoDir },
      { address: 'w-1', agent: 'pi', name: 'w-coder1', parent: 't155', state: 'live', status: 'working', cwd: repoDir },
      { address: 't182', agent: 'pi', state: 'live', status: 'working', cwd: repoDir },
      { address: 'w-2', agent: 'claude', name: 'w-coder2', parent: 't182', state: 'live', status: 'working', cwd: repoDir },
    ],
  });

  const makeRunner = (): Parameters<typeof createLiveRosterSyncer>[0]['execRunner'] =>
    async (_file, args) => {
      if (args[0] === 'ls') return lsJson;
      if (args[0] === 'get') return JSON.stringify({});
      return '';
    };

  const updates: MergedLiveRoster[] = [];
  const syncer = createLiveRosterSyncer({
    repoDir: tmp,
    repoFilterDir: null,
    sessionId: 't155',
    getWatchedLead: () => null,
    onUpdate: (m) => updates.push(m),
    intervalMs: 25,
    execRunner: makeRunner(),
  });
  try {
    await waitFor(() => updates.length >= 1 && updates[updates.length - 1]!.workers.length >= 2);
    const addrs = updates[updates.length - 1]!.workers.map((w) => w.address).sort();
    assert.deepEqual(addrs, ['t155', 'w-1'], '같은 레포라도 t155 세션만 관측');
  } finally {
    syncer.stop();
  }

  // sessionId 미지정(전역/--all) — 같은 조건에서 4개 전부 관측
  const updatesAll: MergedLiveRoster[] = [];
  const syncerAll = createLiveRosterSyncer({
    repoDir: tmp,
    repoFilterDir: null,
    getWatchedLead: () => null,
    onUpdate: (m) => updatesAll.push(m),
    intervalMs: 25,
    execRunner: makeRunner(),
  });
  try {
    await waitFor(() => updatesAll.length >= 1 && updatesAll[updatesAll.length - 1]!.workers.length >= 4);
    const addrs = updatesAll[updatesAll.length - 1]!.workers.map((w) => w.address).sort();
    assert.deepEqual(addrs, ['t155', 't182', 'w-1', 'w-2'], '세션 필터 없으면 전부 관측');
  } finally {
    syncerAll.stop();
    env.restore();
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});
