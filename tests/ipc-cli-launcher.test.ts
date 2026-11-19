/**
 * W3 IPC·CLI·런처 통합 테스트 (brief-w3 §3.E.1, plan-r2.1 §7.2-3).
 *
 * 목적: ① `cli.ts` Hub IPC `prompt` 핸들러(버그 #1 위치 인자 시그니처) 회귀,
 * ② `dispatch.ts` task_kind 라우팅(sharp/split·rerouted 통계 불변),
 * ③ `herdr-quad-launch.ts --mode orchestrate --dry-run`(프로필·세션 계약·DB 미오염),
 * ④ `checkHcNameCollisions`(N12), ⑤ `.agents/hooks.json` 스펙(N10·N-NEW-1)을 검증한다.
 *
 * 실행: `node --test tests/ipc-cli-launcher.test.ts` (외부 의존성 0개)
 *
 * 주의: 런처 dry-run 테스트는 `GEMINI_TRICK_DB_PATH`를 고유 임시 경로로 바꿔
 * 기본 사용자 DB 미생성을 검증한다 — dry-run은 어떤 DB도 생성하지 않아야 한다.
 */
import assert from 'node:assert/strict';
import { spawnSync, type SpawnSyncReturns } from 'node:child_process';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { test } from 'node:test';
import { bumpAgyBudget, openDb, resolveProjectIdentity } from '../src/decision/db.ts';
import { createDemoController, executePromptCommand } from '../src/state/demoEngine.ts';
import { createInitialState } from '../src/state/store.ts';
import {
  checkHcNameCollisions,
  generateRunId,
  planDispatch,
  sessionNameOk,
} from '../scripts/dispatch.ts';
import { resolveEffectiveProfile } from '../scripts/herdr-quad-launch.ts';

const REPO_ROOT = path.resolve(import.meta.dirname, '..');
const DG_PATH = path.join(REPO_ROOT, 'src', 'decision', 'dg.ts');
const CLI_PATH = path.join(REPO_ROOT, 'src', 'cli.ts');
const LAUNCHER_PATH = path.join(REPO_ROOT, 'scripts', 'herdr-quad-launch.ts');
const HOOKS_JSON_PATH = path.join(REPO_ROOT, '.agents', 'hooks.json');

interface TestEnv {
  base: string;
  dbPath: string;
  projDir: string;
  env: NodeJS.ProcessEnv;
}

function makeEnv(): TestEnv {
  const base = fs.mkdtempSync(path.join(os.tmpdir(), `w3test-${process.pid}-`));
  const dbPath = path.join(base, 'decision.db');
  const projDir = path.join(base, 'proj');
  fs.mkdirSync(projDir, { recursive: true });
  return { base, dbPath, projDir, env: { ...process.env, GEMINI_TRICK_DB_PATH: dbPath, DGATE_DB_PATH: undefined, DECISION_ORCHESTRATOR_DB_PATH: undefined } };
}

function cleanupEnv(e: TestEnv): void {
  fs.rmSync(e.base, { recursive: true, force: true });
}

function dg(e: TestEnv, args: string[]): SpawnSyncReturns<string> {
  return spawnSync(process.execPath, [DG_PATH, ...args], { env: e.env, encoding: 'utf8' });
}

function dgOk(e: TestEnv, args: string[]): Record<string, unknown> {
  const res = dg(e, [...args, '--json']);
  assert.equal(res.status, 0, `dg ${args.join(' ')} failed (exit ${res.status}): ${res.stderr}`);
  const lines = res.stdout.trim().split('\n');
  return JSON.parse(lines[lines.length - 1]!) as Record<string, unknown>;
}

// ---------------------------------------------------------------------------

test('1. cli.ts Hub IPC prompt 핸들러 회귀 (버그 #1)', () => {
  // 정적 계약: 수정된 위치 인자 호출이 cli.ts에 존재
  const src = fs.readFileSync(CLI_PATH, 'utf8');
  assert.ok(
    src.includes('executePromptCommand(msg.payload, state, demoCtrl, seekTo, quitApp, liveHooks)'),
    'hub handler must call executePromptCommand with positional args (bug #1 fix)'
  );
  // 동적: 동일 인자 형태 호출이 예외 없이 동작 (liveHooks undefined 허용)
  const state = createInitialState('demo');
  const ctrl = createDemoController();
  const seekTo = (_sec: number) => {};
  const quitApp = () => {};
  assert.doesNotThrow(() =>
    executePromptCommand('일반 프롬프트 메시지', state, ctrl, seekTo, quitApp, undefined)
  );
  assert.doesNotThrow(() => executePromptCommand('/help', state, ctrl, seekTo, quitApp, undefined));
  assert.doesNotThrow(() => executePromptCommand('/model gemini-4-neon-high', state, ctrl, seekTo, quitApp, undefined));
});

test('2. dispatch: single_edit → split (리드 승격)', () => {
  const e = makeEnv();
  try {
    const plan = planDispatch({ taskKind: 'single_edit', repoDir: e.projDir, dbPath: e.dbPath, runId: 'ab12cd' });
    assert.equal(plan.ok, true);
    assert.equal(plan.decision, 'split');
    assert.equal(plan.requiresWrite, true);
    assert.equal(plan.sessionName, 'lead-ab12cd');
    const cmd = `hc ${plan.hcCommands[0]!.join(' ')}`;
    assert.ok(cmd.includes('--name lead-ab12cd'));
    assert.ok(cmd.includes('glm-5.3'));
    assert.ok(cmd.includes('--no-approve'));
    assert.ok(sessionNameOk(plan.sessionName!));
  } finally {
    cleanupEnv(e);
  }
});

test('3. dispatch: static_check → split (sharp 자격 구조 배제)', () => {
  const e = makeEnv();
  try {
    const plan = planDispatch({ taskKind: 'static_check', repoDir: e.projDir, dbPath: e.dbPath, runId: 'ab12cd' });
    assert.equal(plan.decision, 'split');
  } finally {
    cleanupEnv(e);
  }
});

test('4. dispatch: explore_read → sharp + dg wrap --fork + pi 읽기전용 커맨드', () => {
  const e = makeEnv();
  try {
    const plan = planDispatch({
      taskKind: 'explore_read',
      repoDir: e.projDir,
      dbPath: e.dbPath,
      runId: 'ab12cd',
      idx: 2,
      taskFile: 'TASK.md',
    });
    assert.equal(plan.ok, true);
    assert.equal(plan.decision, 'sharp');
    assert.equal(plan.sessionName, 'exp-2-ab12cd');
    assert.match(plan.forkId!, /^fk_[0-9a-f]{12}$/);
    const cmd = `hc ${plan.hcCommands[0]!.join(' ')}`;
    assert.ok(cmd.includes('dg.ts wrap'), cmd);
    assert.ok(cmd.includes(`--fork ${plan.forkId}`), cmd);
    assert.ok(cmd.includes('--tools read,grep,find,ls'), cmd);
    assert.ok(cmd.includes('--no-approve'), cmd);
    assert.ok(cmd.includes('deepseek-deepseek-v4.1-flash'), cmd);
    assert.ok(!cmd.includes('bash'), 'explore worker must not get bash');
    // fork가 실제 DB에 open으로 발행됐는지 확인
    const db = openDb(e.dbPath, { readOnly: true, busyTimeoutMs: 1000 });
    try {
      const row = db.prepare('SELECT status, decision FROM forks WHERE fork_id = ?').get(plan.forkId) as
        | { status: string; decision: string }
        | undefined;
      assert.equal(row?.status, 'open');
      assert.equal(row?.decision, 'sharp');
    } finally {
      db.close();
    }
  } finally {
    cleanupEnv(e);
  }
});

test('5. dispatch: rerouted close → 통계 불변 (표본 0건)', () => {
  const e = makeEnv();
  try {
    const routed = dgOk(e, ['route', '--task-kind', 'explore_read', '--project', e.projDir]);
    const forkId = routed.fork_id as string;
    const closed = dgOk(e, ['close', '--fork', forkId, '--outcome', 'rerouted', '--by', 'worker']);
    assert.equal(closed.status, 'rerouted');
    assert.equal(closed.recorded, false);
    const { projectId } = resolveProjectIdentity(e.projDir);
    const db = openDb(e.dbPath, { readOnly: true, busyTimeoutMs: 1000 });
    try {
      const stats = db
        .prepare('SELECT raw_samples FROM stats_summary WHERE project_id = ? AND route = ? AND tool_class = ?')
        .get(projectId, 'file_select', 'file_read') as { raw_samples: number } | undefined;
      assert.equal(stats, undefined, 'rerouted must not write stats samples');
      const events = db
        .prepare('SELECT COUNT(*) AS n FROM decision_events WHERE project_id = ?')
        .get(projectId) as { n: number };
      assert.equal(events.n, 0, 'rerouted must not write decision_events');
    } finally {
      db.close();
    }
  } finally {
    cleanupEnv(e);
  }
});

test('6. launcher: quota-saver orchestrate --dry-run 계약', () => {
  const e = makeEnv();
  try {
    const neverDb = path.join(e.base, 'never-created.db');
    const env = { ...process.env, GEMINI_TRICK_DB_PATH: neverDb, DGATE_DB_PATH: undefined, DECISION_ORCHESTRATOR_DB_PATH: undefined, HERDR_AGY_TURN_BUDGET: '20' };
    const res = spawnSync(
      process.execPath,
      [LAUNCHER_PATH, '--mode', 'orchestrate', '--profile', 'quota-saver', '--dry-run'],
      { env, encoding: 'utf8', cwd: REPO_ROOT }
    );
    assert.equal(res.status, 0, `dry-run failed: ${res.stderr}`);
    const out = res.stdout;
    assert.match(out, /profile=quota-saver/);
    assert.match(out, /standing_agy_sessions=0/);
    assert.match(out, /max_ondemand_agy=0/);
    assert.ok(out.includes('--no-approve'), 'pi commands must carry --no-approve');
    assert.ok(out.includes('--watch-db'), 'Tab 1 monitor must use --watch-db');
    assert.ok(out.includes('lead-'), 'lead session planned');
    assert.ok(out.includes('verify-'), 'verifier session planned');
    assert.ok(out.includes('worktree'), 'coder worktree example planned');
    const carbonLine = out.split('\n').find((l) => l.includes('carbon-gate1') && l.includes('hc run'));
    assert.ok(carbonLine, 'carbon on-demand template planned');
    assert.ok(carbonLine!.includes('HERDR_ROLE=carbon'), `carbon role env expected: ${carbonLine}`);
    assert.ok(carbonLine!.includes('glm-zcode/glm-5.3'), `carbon advisor must use pi glm-5.3: ${carbonLine}`);
    assert.ok(carbonLine!.includes('--no-approve'), `carbon advisor pi command must carry --no-approve: ${carbonLine}`);
    assert.ok(!out.includes('gemini-4-hydrogen-xhigh'), 'quota-saver must not reference the agy advisor model');
    // B2: verifier 읽기전용 격리 — --tools read,grep,find,ls 포함·bash 불포함 (§6.1 Layer 3)
    const verifyLine = out.split('\n').find((l) => l.includes('verify-') && l.includes('hc run'));
    assert.ok(verifyLine, 'verifier hc command line expected');
    assert.ok(verifyLine!.includes('--tools read,grep,find,ls'), `verifier tools contract: ${verifyLine}`);
    assert.ok(!verifyLine!.includes('bash'), `verifier must not include bash: ${verifyLine}`);
    assert.ok(out.includes('readonly sessions without bash (§6.1 Layer 3): OK'), 'dry-run must assert the no-bash contract');
    for (const m of out.matchAll(/--name\s+(\S+)/g)) {
      assert.match(m[1]!, /^[a-z0-9_-]{1,32}$/, `session name contract violated: ${m[1]}`);
    }
    assert.ok(!fs.existsSync(neverDb), 'dry-run must not create the orchestration DB');
    assert.ok(!fs.existsSync(`${neverDb}-wal`) && !fs.existsSync(`${neverDb}-shm`));
  } finally {
    cleanupEnv(e);
  }
});

test('7. launcher: agy-quad dry-run + agy_budget 강등', () => {
  const e = makeEnv();
  try {
    const env = { ...process.env, GEMINI_TRICK_DB_PATH: e.dbPath, DGATE_DB_PATH: undefined, DECISION_ORCHESTRATOR_DB_PATH: undefined, HERDR_AGY_TURN_BUDGET: '20' };
    const res = spawnSync(
      process.execPath,
      [LAUNCHER_PATH, '--mode', 'orchestrate', '--profile', 'agy-quad', '--dry-run'],
      { env, encoding: 'utf8', cwd: REPO_ROOT }
    );
    assert.equal(res.status, 0, res.stderr);
    assert.match(res.stdout, /profile=agy-quad/);
    assert.match(res.stdout, /standing_agy_sessions=3/);
    assert.match(res.stdout, /max_ondemand_agy=3/);
    for (const model of ['gemini-4-argon-high', 'gemini-4-carbon-high', 'gemini-4-neon-high']) {
      assert.ok(res.stdout.includes(model), `missing ${model}`);
    }

    // 예산 초과 → 자동 quota-saver 강등 (단위 + e2e)
    const unit = resolveEffectiveProfile('agy-quad', 25, 20);
    assert.equal(unit.effective, 'quota-saver');
    assert.equal(unit.downgraded, true);

    const budgetDb = path.join(e.base, 'budget.db');
    const db = openDb(budgetDb);
    try {
      bumpAgyBudget(db, { turns: 25, sessions: 3 });
    } finally {
      db.close();
    }
    const res2 = spawnSync(
      process.execPath,
      [LAUNCHER_PATH, '--mode', 'orchestrate', '--profile', 'agy-quad', '--db', budgetDb, '--dry-run'],
      { env: { ...process.env, HERDR_AGY_TURN_BUDGET: '20' }, encoding: 'utf8', cwd: REPO_ROOT }
    );
    assert.equal(res2.status, 0, res2.stderr);
    assert.match(res2.stdout, /profile_requested=agy-quad/);
    assert.match(res2.stdout, /effective_profile=quota-saver/);
    assert.match(res2.stdout, /downgraded=true/);
    assert.match(res2.stdout, /standing_agy_sessions=0/);
  } finally {
    cleanupEnv(e);
  }
});

test('8. checkHcNameCollisions 충돌 감지 (N12)', () => {
  const fakeLs = {
    hidden_finished: 2,
    terminals: [
      { address: 't1', name: 'lead-ab12cd', state: 'live' },
      { address: 't2', state: 'live' },
      { address: 't3', name: 'other-worker', state: 'live' },
    ],
  };
  const r1 = checkHcNameCollisions(['lead-ab12cd', 'verify-ab12cd', 't2'], fakeLs);
  assert.deepEqual(r1.collisions.sort(), ['lead-ab12cd', 't2']);
  const r2 = checkHcNameCollisions(['fresh-name-1'], fakeLs);
  assert.deepEqual(r2.collisions, []);
  const r3 = checkHcNameCollisions(['lead-ab12cd'], JSON.stringify(fakeLs));
  assert.deepEqual(r3.collisions, ['lead-ab12cd']);
  assert.match(generateRunId(), /^[0-9a-f]{6}$/);
});

test('9. .agents/hooks.json 스펙 (N10 + N-NEW-1 경로 교정)', () => {
  const cfg = JSON.parse(fs.readFileSync(HOOKS_JSON_PATH, 'utf8')) as Record<string, any>;
  assert.deepEqual(Object.keys(cfg).sort(), ['dg-collect', 'dg-guard']);
  const guard = cfg['dg-guard'].PreToolUse;
  assert.equal(guard.length, 1);
  assert.equal(guard[0].matcher, '*');
  assert.equal(guard[0].hooks.length, 1);
  assert.equal(guard[0].hooks[0].type, 'command');
  assert.equal(guard[0].hooks[0].timeout, 3);
  assert.equal(guard[0].hooks[0].command, 'node hooks/dg-hook.ts --guard');
  const collect = cfg['dg-collect'].PostToolUse;
  assert.equal(collect.length, 1);
  assert.equal(collect[0].matcher, '*');
  assert.equal(collect[0].hooks[0].type, 'command');
  assert.equal(collect[0].hooks[0].timeout, 3);
  assert.equal(collect[0].hooks[0].command, 'node hooks/dg-hook.ts --collect');
  // N-NEW-1: 훅 cwd는 .agents/ 이므로 command에 .agents/ 접두가 있으면 MODULE_NOT_FOUND
  for (const cmd of [guard[0].hooks[0].command, collect[0].hooks[0].command]) {
    assert.ok(!cmd.includes('.agents/'), `hook command must be relative to .agents/ cwd: ${cmd}`);
  }
});
