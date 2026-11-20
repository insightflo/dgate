import { CellBuffer, type Rect } from '../core/buffer.ts';
import {
  padLeftWidth,
  padRightWidth,
  stringWidth,
  truncateWidth,
  wrapByWidth,
} from '../core/width.ts';
import { LOG_FILTER_LABELS, TRIGGER_LABELS, effortToGauge, getFilteredLogs, sortRoutesForDisplay } from '../state/store.ts';
import { THEME, lerpColor, resolveColorKey, type RGB } from '../theme.ts';
import { applyLiveRolePanels, createLiveRoleTrail, formatElapsedMmss } from '../cli.ts';
import type { AppState, SwarmCard } from '../types.ts';
import { computeLayout, maxCardSlotsFor, type CompactLayout, type FullLayout } from './layout.ts';
import { renderModalOverlay } from './renderModal.ts';

export function renderFrame(state: AppState, cols: number, rows: number): CellBuffer {
  const buf = new CellBuffer(cols, rows, THEME.bg, THEME.fg);
  // 실제 보여줄 워커 카드 수(라이브 가변 목록 우선)만큼 레이아웃 카드 슬롯을 구성한다.
  const workerCards = workerCardsOf(state);
  const visibleCount = Math.max(1, Math.min(workerCards.length, maxCardSlotsFor(cols)));
  const layout = computeLayout(cols, rows, visibleCount);

  if (layout.kind === 'full') {
    renderFullLayout(buf, state, layout);
  } else {
    renderCompactLayout(buf, state, layout);
  }

  if (state.modal !== 'none') {
    renderModalOverlay(buf, state);
  }

  return buf;
}

function renderFullLayout(buf: CellBuffer, state: AppState, L: FullLayout): void {
  // 1. 상단 크롬 바 (충분한 높이일 때 표시)
  if (L.showChrome) {
    buf.fillRect({ x: 0, y: 0, w: L.cols, h: 1 }, THEME.bgChrome);
    buf.setChar(2, 0, '●', THEME.trafficRed, THEME.bgChrome, true);
    buf.setChar(4, 0, '●', THEME.trafficYellow, THEME.bgChrome, true);
    buf.setChar(6, 0, '●', THEME.trafficGreen, THEME.bgChrome, true);

    const modeTag =
      state.mode === 'demo'
        ? state.paused
          ? 'DEMO · PAUSED'
          : 'DEMO'
        : state.mode === 'watch'
          ? `WATCH · ${state.conversationId ? state.conversationId.slice(0, 8) : 'ACTIVE'}`
          : state.mode === 'live'
            ? `LIVE (${state.backendKind})${state.agentBusy ? ' · RUNNING' : ''}`
            : 'LIVE STREAM';
    const leadCli = state.rosterLabels.lead.split(':')[0] || 'lead';
    const cliName = state.mode === 'live' && state.backendKind === 'agy' ? 'agy' : leadCli;
    const centerTitle = `${state.cwd} · ${cliName} · ${L.cols}×${L.rows} · ${modeTag}`;
    buf.writeCentered(0, 0, L.cols, centerTitle, THEME.chromeFg, THEME.bgChrome, false);
    const rightChrome =
      state.mode === 'watch' || state.mode === 'stream'
        ? '?:도움말  n:검증  s:스웜(50)  q:종료'
        : state.mode === 'live'
          ? '?:도움말  r:응답  n:검증  s:스웜  /:입력'
          : '?:도움말  n:검증  s:스웜(50)  /:입력';
    buf.writeRight(0, 0, L.cols - 2, rightChrome, THEME.chromeFg, THEME.bgChrome);
  }

  // 2. 헤더 바 + 이중선(═) 구분선
  renderHeader(buf, state, L.padX, L.headerY, L.cols - L.padX * 2);

  // 3. 범례 네비게이션 바
  renderLegendNav(buf, state, L.padX, L.navY, L.cols - L.padX * 2);

  // 4. 파이프라인 & Advisor 자문 연결 와이어 (패널보다 먼저 그려서 보더와 자연스럽게 접합)
  renderPipelineWires(buf, state, L);
  renderAdvisoryWires(buf, state, L);

  // 5. 좌측 Advisor 자문 패널
  renderAdvisorPanel(buf, state, L);

  // 6. 우측 4단 파이프라인 패널들 + Verifier 교차 검증 패널
  renderMainPanel(buf, state, L);
  renderNeonPanel(buf, state, L);
  renderDecisionPanel(buf, state, L);
  renderSwarmSection(buf, state, L);
  renderBackPanel(buf, state, L);

  // 7. 하단 세션 기록 패널
  renderLogsPanel(buf, state, L.logs);

  // 8. 최하단 프롬프트 & 상태 푸터
  renderFooter(buf, state, L.padX, L.footerY, L.cols - L.padX * 2);
}

/** Workers 패널 카드 소스 — 라이브 hc 워커 카드가 있으면 우선, 없으면 기존 3슬롯 cards로 폴백. */
function workerCardsOf(state: AppState): SwarmCard[] {
  return state.liveWorkers && state.liveWorkers.length > 0 ? state.liveWorkers : state.cards;
}

/**
 * 카드/와이어 라이브 모션 게이트 — 데모이거나 실제 라이브 워커 목록이 있을 때만 움직인다.
 * watch 폴백 cards(자리표)는 실제 실행이 아니므로 정적으로 유지(침착 대기 불변식).
 */
function liveMotionOf(state: AppState): boolean {
  return state.mode === 'demo' || (state.liveWorkers !== undefined && state.liveWorkers.length > 0);
}

/** 브라일 스피너 프레임 — simTime 구동, seed(카드 인덱스)로 카드마다 위상 엇갈림. */
const SPINNER_FRAMES = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'] as const;
function spinnerFrame(simTime: number, seed: number): string {
  const idx =
    ((Math.floor(simTime * 8 + seed) % SPINNER_FRAMES.length) + SPINNER_FRAMES.length) %
    SPINNER_FRAMES.length;
  return SPINNER_FRAMES[idx]!;
}

/** 부재 진행률(라이브 hc 워커는 % 숫자를 못 줌) — 매끄럽게 쓸어지는 활성 블록 바. */
function drawIndeterminateBar(
  buf: CellBuffer,
  x: number,
  y: number,
  w: number,
  fillFg: RGB,
  trackBg: RGB,
  simTime: number,
  seed: number
): void {
  if (w <= 0) return;
  const blockW = Math.max(3, Math.floor(w / 4));
  const trackFg = lerpColor(trackBg, THEME.muted, 0.18);
  for (let i = 0; i < w; i++) buf.setChar(x + i, y, '░', trackFg, trackBg, false);
  const travel = w + blockW;
  const t = (((simTime * 0.7 + seed * 0.31) % 1) + 1) % 1;
  const start = Math.round(t * travel) - blockW;
  for (let i = 0; i < blockW; i++) {
    const px = x + start + i;
    if (px >= x && px < x + w) buf.setChar(px, y, '█', fillFg, trackBg, true);
  }
}

/** Lead 활성 판정 — 데모이거나 phase가 대기/완료류가 아닐 때. 와이어·패널 스피너 공용. */
const IDLE_PHASES = new Set(['', '대기 중', '완료', '중단됨', 'idle', 'done']);
function isLeadActive(state: AppState): boolean {
  return state.mode === 'demo' || !IDLE_PHASES.has(state.phase.trim());
}

function renderHeader(
  buf: CellBuffer,
  state: AppState,
  x: number,
  y: number,
  width: number
): void {
  const title = 'DGATE TUI';
  buf.writeText(x, y, title, THEME.fg, THEME.bg, true, width);
  const titleW = stringWidth(title);

  // 모드 배지 — 읽기전용 관측(watch/stream)과 대화형 제어(live)/시뮬레이션(demo)을 즉시 구분
  const modeBadge =
    state.mode === 'watch'
      ? '[읽기전용 관측 · WATCH]'
      : state.mode === 'demo'
        ? '[시뮬레이션 · DEMO]'
        : state.mode === 'live'
          ? '[대화형 제어 · LIVE]'
          : '[스트림 관측 · STREAM]';
  const modeBadgeFg =
    state.mode === 'live'
      ? THEME.green
      : state.mode === 'demo'
        ? THEME.orange
        : THEME.blue;
  const badgeW = stringWidth(modeBadge);
  let leftW = titleW;
  if (width - leftW > badgeW + 10) {
    buf.writeText(x + leftW + 1, y, modeBadge, modeBadgeFg, THEME.bg, true, width - leftW - 1);
    leftW += 1 + badgeW;
  }

  // 우측 카운터 폭을 먼저 계산한다 — 프로필/스코프 문자열이 이 영역을 침범하면
  // 120열에서 `repo=LEAD BUILDS…` 처럼 라벨이 덮여 보인다(헤더 충돌 방지).
  const compactBadges = width < 96;
  const b1 = compactBadges ? `LEAD[${state.argonBuilds}]` : `LEAD BUILDS [${state.argonBuilds}]`;
  const b2 = compactBadges
    ? `WORK[${state.swarmActive}]`
    : `WORKERS [${state.swarmActive}]`;
  const b3 = compactBadges
    ? `VERIF[${state.neonVerified}]`
    : `VERIFIES [${state.neonVerified}]`;
  const b4 = compactBadges ? `ADV[${state.carbonCalls}]` : `ADVISOR [${state.carbonCalls}]`;
  const gap = width >= 124 ? '   ' : width >= 96 ? '  ' : ' ';
  const totalRightW =
    stringWidth(b1) +
    stringWidth(b2) +
    stringWidth(b3) +
    stringWidth(b4) +
    gap.length * 3;
  const startRight = x + width - totalRightW;
  const countersDrawn = startRight > x + 24 + leftW;

  if (width - leftW > 8) {
    // 관측 스코프 표기 — --repo 지정 시 repo=<이름>, watch 전역 관측 시 '전체'
    const repoScope = state.repoScopeLabel
      ? `repo=${state.repoScopeLabel}`
      : state.mode === 'watch'
        ? '전체'
        : null;
    // 카운터가 그려지는 폭에서는 최대폭을 width-leftW-countersW-2로 제한해 절대 겹치지 않게 한다.
    const scopeMaxW = countersDrawn
      ? Math.max(0, width - leftW - totalRightW - 2)
      : Math.max(0, width - leftW);
    let scopeText = ` · profile=${state.rosterLabels.profileName}${repoScope ? ` · ${repoScope}` : ''}`;
    if (stringWidth(scopeText) > scopeMaxW && repoScope) {
      // 공간이 부족하면 프로필명을 버리고라도 repo 스코프는 온전히 노출한다.
      scopeText = ` · ${repoScope}`;
    }
    buf.writeText(x + leftW, y, scopeText, THEME.muted, THEME.bg, false, scopeMaxW);
  }

  if (countersDrawn) {
    buf.writeSegments(startRight, y, [
      { text: b1, fg: THEME.orange, bg: THEME.bg, bold: true },
      { text: gap, fg: THEME.muted, bg: THEME.bg },
      { text: b2, fg: THEME.blue, bg: THEME.bg, bold: true },
      { text: gap, fg: THEME.muted, bg: THEME.bg },
      { text: b3, fg: THEME.neon, bg: THEME.bg, bold: true },
      { text: gap, fg: THEME.muted, bg: THEME.bg },
      { text: b4, fg: THEME.purple, bg: THEME.bg, bold: true },
    ]);
  }

  buf.drawHLine(x, y + 1, width, '═', THEME.borderHeader, THEME.bg);
}

function renderLegendNav(
  buf: CellBuffer,
  state: AppState,
  x: number,
  y: number,
  width: number
): void {
  const f = state.logFilter;
  const lb = state.rosterLabels;
  const items: ReadonlyArray<{ text: string; fg: RGB; on: boolean }> = [
    { text: `Lead · ${lb.lead}`, fg: THEME.orange, on: f === 'argon' },
    { text: `Workers · ${lb.explore} / ${lb.coder}`, fg: THEME.blue, on: f === 'hydrogen' },
    { text: 'Decision Gate', fg: THEME.green, on: f === 'decision' },
    { text: `Verifier · ${lb.verifier}`, fg: THEME.neon, on: f === 'neon' },
    { text: `Advisor · ${lb.advisor}`, fg: THEME.purple, on: f === 'carbon' },
  ];

  // 항목(아이콘 2칸 + 라벨)이 폭 안에 들어가는 가장 넓은 간격을 고른다.
  const fixedW = items.reduce((acc, i) => acc + 2 + stringWidth(i.text) + (i.on ? 7 : 0), 0);
  const gap =
    fixedW + 6 * (items.length - 1) <= width
      ? '      '
      : fixedW + 4 * (items.length - 1) <= width
        ? '    '
        : fixedW + 3 * (items.length - 1) <= width
          ? '   '
          : fixedW + 2 * (items.length - 1) <= width
            ? '  '
            : ' ';

  const segments: Array<{ text: string; fg: RGB; bg: RGB; bold?: boolean }> = [];
  items.forEach((item, idx) => {
    if (idx > 0) segments.push({ text: gap, fg: THEME.muted, bg: THEME.bg });
    segments.push({ text: '■ ', fg: item.fg, bg: THEME.bg, bold: true });
    segments.push({
      text: item.on ? `${item.text} [필터]` : item.text,
      fg: item.on ? item.fg : THEME.muted,
      bg: THEME.bg,
      bold: item.on,
    });
  });
  buf.writeSegmentsCentered(x, y, width, segments);
}

function renderPipelineWires(buf: CellBuffer, state: AppState, L: FullLayout): void {
  const cx = L.centerX;
  const wireFg = THEME.borderWire;

  // 1) Lead Main 하단 -> Decision 상단 수직선
  const mBottom = L.main.y + L.main.h;
  const pathMainToDec: Array<readonly [number, number]> = [];
  for (let y = mBottom; y < L.decision.y; y++) {
    buf.setChar(cx, y, '│', wireFg, THEME.bg);
    pathMainToDec.push([cx, y]);
  }

  // 2) Decision 하단 -> 위임 라벨 -> 팬아웃 트리 -> 실제 카드 상단
  const dBottom = L.decision.y + L.decision.h;
  for (let y = dBottom; y < L.fanOutY; y++) {
    if (y !== L.delegateLabelY) {
      buf.setChar(cx, y, '│', wireFg, THEME.bg);
    }
  }

  // 실제 렌더링된 카드의 중심 x목록 — 빈 슬롯으로 향하는 와이어는 그리지 않는다
  const xs = L.cards.map((r) => r.x + Math.floor(r.w / 2));
  const stemSet = new Set(xs);

  // 팬아웃/팬인 수평 레일 — min(cx, xs[0]) .. max(cx, xs[last]) 구간만, 정확한 접합 문자 사용
  const drawRail = (y: number, kind: 'fanOut' | 'fanIn'): void => {
    const railL = Math.min(cx, xs[0]!);
    const railR = Math.max(cx, xs[xs.length - 1]!);
    if (railL === railR) {
      // 카드 1장 — 빈 슬롯으로 가로선 없이 곧장 세로 스템
      buf.setChar(cx, y, '│', wireFg, THEME.bg);
      return;
    }
    for (let x = railL; x <= railR; x++) {
      const isStem = stemSet.has(x);
      const isFeed = x === cx;
      let ch = '─';
      if (isStem && isFeed) ch = '┼';
      else if (kind === 'fanOut') {
        if (isFeed) ch = '┴';
        else if (isStem) ch = '┬';
        else if (x === railL) ch = '┌';
        else if (x === railR) ch = '┐';
      } else {
        if (isFeed) ch = '┬';
        else if (isStem) ch = '┴';
        else if (x === railL) ch = '└';
        else if (x === railR) ch = '┘';
      }
      buf.setChar(x, y, ch, wireFg, THEME.bg);
    }
  };
  drawRail(L.fanOutY, 'fanOut');
  drawRail(L.fanInY, 'fanIn');

  // 3) 카드 하단 -> 팬인 트리 -> 다시 Lead 상단
  for (let y = L.fanInY + 1; y < L.back.y; y++) {
    const isLast = y === L.back.y - 1;
    buf.setChar(cx, y, isLast ? '▼' : '│', wireFg, THEME.bg);
  }

  // 4) Lead Main 우측 <-> Verifier 좌측 교차 검증 링크
  const neonStartX = L.main.x + L.main.w;
  const neonEndX = L.neon.x - 1;
  const neonActive = state.neonState === 'run' || state.neonState === 'pass';
  const neonLinkColor =
    state.neonState === 'fail'
      ? THEME.trafficRed
      : neonActive
        ? THEME.neon
        : wireFg;
  const neonLinkChar = neonActive ? '━' : '─';
  const pathNeonGo: Array<readonly [number, number]> = [];
  const pathNeonBack: Array<readonly [number, number]> = [];
  for (let x = neonStartX; x <= neonEndX; x++) {
    buf.setChar(x, L.neonLinkRow, neonLinkChar, neonLinkColor, THEME.bg, neonActive);
    pathNeonGo.push([x, L.neonLinkRow]);
  }
  for (let x = neonEndX; x >= neonStartX; x--) {
    pathNeonBack.push([x, L.neonLinkRow]);
  }

  // 데이터 흐름 펄스 애니메이션 (gemini_sub_neon.html의 comet 대응)
  const t = state.simTime;
  const isDemo = state.mode === 'demo';
  const leadActive = isLeadActive(state);
  const hasRunningWorkers = isDemo || (state.liveWorkers !== undefined
    ? state.liveWorkers.some((w) => w.status === 'running')
    : state.swarmActive > 0);

  if (leadActive || hasRunningWorkers) {
    buf.drawPulseAlongPath(pathMainToDec, t * 0.9, THEME.orange, '●');
  }

  // 카드별 개별 펄스 — 실행 중인 카드로만 흐른다(대기/빈 카드는 펄스 0개).
  // 위상을 카드 인덱스로 엇갈리게 해 2번째(가운데)·4번째 워커도 확실히 보이게 한다.
  if (liveMotionOf(state)) {
    const workerCards = workerCardsOf(state);
    L.cards.forEach((_rect, i) => {
      const card = workerCards[i];
      if (!card || (!isDemo && card.status !== 'running')) return;

      const tx = xs[i]!;
      // 팬아웃: Decision 하단 세로 → fanOutY 레일 → 카드 i 상단 (위임 라벨 행은 제외)
      const pathOut: Array<readonly [number, number]> = [];
      for (let y = dBottom; y < L.fanOutY; y++) {
        if (y !== L.delegateLabelY) pathOut.push([cx, y]);
      }
      if (tx < cx) for (let x = cx; x >= tx; x--) pathOut.push([x, L.fanOutY]);
      else for (let x = cx; x <= tx; x++) pathOut.push([x, L.fanOutY]);
      buf.drawPulseAlongPath(pathOut, t * 0.48 + i * 0.27, THEME.blue, '●');

      // 팬인: 카드 i 하단 → fanInY 레일 → 다시 Lead(back) 상단
      const pathIn: Array<readonly [number, number]> = [];
      if (tx >= cx) for (let x = tx; x >= cx; x--) pathIn.push([x, L.fanInY]);
      else for (let x = tx; x <= cx; x++) pathIn.push([x, L.fanInY]);
      for (let y = L.fanInY + 1; y < L.back.y; y++) pathIn.push([cx, y]);
      buf.drawPulseAlongPath(pathIn, t * 0.3 + i * 0.19, THEME.fg, '•');
    });
  }

  if (pathNeonGo.length > 0) {
    if (state.neonState === 'run') {
      const nt = ((t / 1.6) % 1 + 1) % 1;
      if (nt < 0.5) {
        buf.drawPulseAlongPath(pathNeonGo, nt * 2, THEME.orange, '●');
      } else {
        buf.drawPulseAlongPath(pathNeonBack, (nt - 0.5) * 2, THEME.neon, '●');
      }
    } else if (isDemo && state.neonState === 'pass') {
      buf.drawPulseAlongPath(pathNeonBack, t * 0.85, THEME.neon, '●');
    }
  }
}

function renderAdvisoryWires(buf: CellBuffer, state: AppState, L: FullLayout): void {
  const isDemo = state.mode === 'demo';
  const advisorActive = isDemo || state.advisorState === '자문 중';
  const startX = L.advisor.x + L.advisor.w;
  const targets: ReadonlyArray<{ row: number; endX: number; idx: 0 | 1 | 2 }> = [
    { row: L.tr0Row, endX: L.main.x - 1, idx: 0 },
    { row: L.tr1Row, endX: L.cards[0].x - 1, idx: 1 },
    { row: L.tr2Row, endX: L.back.x - 1, idx: 2 },
  ];

  for (const item of targets) {
    if (item.endX <= startX) continue;
    const isOn = state.activeTrigger === item.idx;
    const lineColor = isOn ? THEME.purple : THEME.borderAdvisoryIdle;
    const lineChar = isOn ? '━' : '┄';
    const arrowChar = isOn ? '▶' : '▷';

    const path: Array<readonly [number, number]> = [];
    for (let x = startX; x < item.endX; x++) {
      buf.setChar(x, item.row, lineChar, lineColor, THEME.bg, isOn);
      path.push([x, item.row]);
    }
    buf.setChar(item.endX, item.row, arrowChar, lineColor, THEME.bg, isOn);

    if (isOn && advisorActive && path.length > 0) {
      buf.drawPulseAlongPath(path, state.simTime * 0.75, THEME.borderFocusBoost, '◈');
    }
  }
}

function renderAdvisorPanel(buf: CellBuffer, state: AppState, L: FullLayout): void {
  const R = L.advisor;
  const focused = state.focus === 'advisor';
  const borderColor = focused ? lerpColor(THEME.purple, THEME.borderFocusBoost, 0.35) : THEME.purple;

  buf.drawBox(R, {
    style: focused ? 'heavy' : 'single',
    borderColor,
    fillBg: THEME.bg,
    rightLabel: focused ? 'Enter:상세' : undefined,
    rightLabelColor: THEME.purple,
    boldBorder: focused,
  });

  // 우측 보더에서 자문 화살표 분기점(├ / ┣) 마킹
  const rightX = R.x + R.w - 1;
  for (const [idx, row] of [
    [0, L.tr0Row],
    [1, L.tr1Row],
    [2, L.tr2Row],
  ] as const) {
    if (row > R.y && row < R.y + R.h - 1) {
      const on = state.activeTrigger === idx;
      buf.setChar(rightX, row, on ? '┣' : '├', on ? THEME.purple : borderColor, THEME.bg, on);
    }
  }

  const ix = R.x + 2;
  const iw = R.w - 4;

  // 상단 헤더 영역 (R.y + 1 ~ tr0Row - 1)
  let hy = R.y + 1;
  if (hy < L.tr0Row) {
    buf.writeCentered(ix, hy++, iw, 'Advisor', THEME.purple, THEME.bg, true);
  }
  if (hy < L.tr0Row) {
    const advisorActive = state.advisorState === '자문 중';
    const advisorTitle = `${state.rosterLabels.advisor} · ${state.advisorState}${
      advisorActive ? ` ${spinnerFrame(state.simTime, 3)}` : ''
    }`;
    buf.writeCentered(ix, hy++, iw, advisorTitle, THEME.purple, THEME.bg, advisorActive);
  }
  if (hy < L.tr0Row) {
    buf.writeCentered(
      ix,
      hy++,
      iw,
      `세션 ${state.sessionId} · session: ${state.sessionId}`,
      THEME.muted,
      THEME.bg
    );
  }

  // 트리거 3개 행 렌더링 (활성 시 보라 하이라이트 배경 바)
  const triggerRows = [L.tr0Row, L.tr1Row, L.tr2Row] as const;
  triggerRows.forEach((tRow, idx) => {
    if (tRow <= R.y || tRow >= R.y + R.h - 1) return;
    const active = state.activeTrigger === idx;
    const rowBg = active ? THEME.bgTriggerActive : THEME.bg;
    const rowFg = active ? THEME.purple : THEME.muted;
    buf.fillRect({ x: R.x + 1, y: tRow, w: R.w - 2, h: 1 }, rowBg);
    const prefix = active ? '● ' : '◇ ';
    buf.writeText(ix, tRow, `${prefix}${TRIGGER_LABELS[idx]}`, rowFg, rowBg, active, iw);
  });

  // 중단 자문 본문 블록 (tr0Row + 1 ~ tr1Row - 1): 좌측 점선(┆) 가이드
  const bodyStart = L.tr0Row + 1;
  const bodyEnd = L.tr1Row - 1;
  const availBody = bodyEnd - bodyStart + 1;
  if (availBody >= 3) {
    for (let y = bodyStart; y <= bodyEnd; y++) {
      buf.setChar(ix, y, '┆', THEME.borderAsideDash, THEME.bg);
    }

    const bx = ix + 2;
    const bw = iw - 2;
    let by = bodyStart;

    if (availBody >= 9) {
      buf.writeText(bx, by++, '세션 전체·도구 호출·결과를', THEME.muted, THEME.bg, false, bw);
      buf.writeText(bx, by++, '읽습니다', THEME.muted, THEME.bg, false, bw);
      by++;
    } else {
      buf.writeText(bx, by++, '세션 전체·도구 결과를 읽음', THEME.muted, THEME.bg, false, bw);
    }

    if (by <= bodyEnd) {
      buf.writeText(bx, by++, '최근 조언', THEME.muted, THEME.bg, false, bw);
    }

    // 남은 공간 계산해 최근 조언 줄바꿈 출력 (최대 2~3줄)
    const reservedStats = availBody >= 8 ? 3 : 2;
    const maxAdviceLines = Math.max(1, Math.min(3, bodyEnd - by + 1 - reservedStats));
    const adviceLines = wrapByWidth(state.latestAdvice, bw, maxAdviceLines);
    for (const line of adviceLines) {
      if (by > bodyEnd - reservedStats) break;
      buf.writeText(bx, by++, line, THEME.purple, THEME.bg, true, bw);
    }

    if (availBody >= 10 && by < bodyEnd - 2) {
      by++;
    }

    // 호출 수 & 읽은 토큰
    if (by <= bodyEnd) {
      buf.writeText(bx, by, '호출 수', THEME.muted, THEME.bg);
      buf.writeRight(bx, by, bw, String(state.carbonCalls), THEME.purple, THEME.bg, true);
      by++;
    }
    if (by <= bodyEnd) {
      buf.writeText(bx, by, '읽은 토큰', THEME.muted, THEME.bg);
      buf.writeRight(bx, by, bw, state.tokensRead, THEME.purple, THEME.bg, true);
      by++;
    }
    if (by <= bodyEnd) {
      if (availBody >= 11 && by < bodyEnd) by++;
      buf.writeText(bx, by, '일상 턴에서는 침묵', THEME.muted, THEME.bg, false, bw);
    }
  }

  // 하단 보조 설명 블록 (tr1Row + 1 ~ tr2Row - 1)
  const expStart = L.tr1Row + 1;
  const expEnd = L.tr2Row - 1;
  if (expEnd >= expStart) {
    let ey = expStart + (expEnd - expStart >= 2 ? 1 : 0);
    if (ey <= expEnd) {
      buf.writeText(ix + 1, ey++, '코드는 직접 쓰지 않습니다.', THEME.muted, THEME.bg, false, iw - 1);
    }
    if (ey <= expEnd) {
      buf.writeText(ix + 1, ey++, '조언은 Lead가 적용합니다.', THEME.muted, THEME.bg, false, iw - 1);
    }
  }
}

function renderMainPanel(buf: CellBuffer, state: AppState, L: FullLayout): void {
  const R = L.main;
  const glowing = state.activeTrigger === 0;
  const focused = state.focus === 'main';
  const panelBg = glowing ? THEME.bgGlowPurple : THEME.bg;
  const baseBorder = glowing ? THEME.purple : THEME.orange;
  const borderColor = focused ? lerpColor(baseBorder, THEME.borderFocusBoost, 0.35) : baseBorder;

  buf.drawBox(R, {
    style: glowing || focused ? 'heavy' : 'single',
    borderColor,
    fillBg: panelBg,
    rightLabel: focused ? 'Enter:편집' : glowing ? 'Advisor 자문' : undefined,
    rightLabelColor: glowing ? THEME.purple : THEME.orange,
    boldBorder: glowing || focused,
  });

  // 우측 보더에서 Verifier 연결 분기점(├ / ┣) 마킹
  if (L.neonLinkRow > R.y && L.neonLinkRow < R.y + R.h - 1) {
    const neonOn = state.neonState === 'run' || state.neonState === 'pass';
    buf.setChar(
      R.x + R.w - 1,
      L.neonLinkRow,
      glowing || focused || neonOn ? '┣' : '├',
      neonOn ? THEME.neon : borderColor,
      panelBg,
      glowing || focused || neonOn
    );
  }

  const ix = R.x + 2;
  const iw = R.w - 4;
  let cy = R.y + 1;

  buf.writeCentered(
    ix,
    cy++,
    iw,
    `Lead · ${state.rosterLabels.lead}`,
    THEME.orange,
    panelBg,
    true
  );
  if (cy < R.y + R.h - 1) {
    buf.writeCentered(ix, cy++, iw, state.mainSummary, THEME.fg, panelBg, false);
  }
  if (cy < R.y + R.h - 1) {
    const phaseSegs: Array<{ text: string; fg: RGB; bg: RGB; bold?: boolean }> = [
      { text: 'effort ', fg: THEME.muted, bg: panelBg },
      { text: effortToGauge(state.effort), fg: THEME.orange, bg: panelBg, bold: true },
      { text: ' · ', fg: THEME.muted, bg: panelBg },
      { text: state.phase, fg: THEME.fg, bg: panelBg },
    ];
    // Lead 활성 시 위상 배지 옆 회전 스피너 — 패널이 살아 있음을 즉시 보여준다(대기 시 정지)
    if (isLeadActive(state)) {
      phaseSegs.push({
        text: ` ${spinnerFrame(state.simTime, 2)}`,
        fg: THEME.orange,
        bg: panelBg,
        bold: true,
      });
    }
    buf.writeSegmentsCentered(ix, cy++, iw, phaseSegs);
  }
  if (cy < R.y + R.h - 1) {
    buf.writeCentered(ix, cy++, iw, state.modelFlag, THEME.muted, panelBg, false);
  }
}

function renderNeonPanel(buf: CellBuffer, state: AppState, L: FullLayout): void {
  const R = L.neon;
  const focused = state.focus === 'neon';
  const isRun = state.neonState === 'run';
  const isPass = state.neonState === 'pass';
  const isFail = state.neonState === 'fail';
  const active = isRun || isPass || isFail;

  const panelBg = active ? THEME.bgGlowNeon : THEME.bg;
  const baseBorder = isFail
    ? THEME.trafficRed
    : active
      ? THEME.neon
      : lerpColor(THEME.neon, THEME.borderWire, 0.32);
  const borderColor = focused
    ? lerpColor(THEME.neon, THEME.borderFocusBoost, 0.35)
    : baseBorder;

  const rightLabel = focused
    ? 'Enter:상세'
    : isRun
      ? '검증 중'
      : isPass
        ? '통과'
        : isFail
          ? '주의'
          : undefined;
  const rightLabelColor = isPass
    ? THEME.green
    : isFail
      ? THEME.trafficRed
      : THEME.neon;

  buf.drawBox(R, {
    style: active || focused ? 'heavy' : 'single',
    borderColor,
    fillBg: panelBg,
    rightLabel,
    rightLabelColor,
    boldBorder: active || focused,
  });

  // 좌측 보더에서 Lead 메인 연결 접합점(┤ / ┫) 마킹
  if (L.neonLinkRow > R.y && L.neonLinkRow < R.y + R.h - 1) {
    buf.setChar(
      R.x,
      L.neonLinkRow,
      active || focused ? '┫' : '┤',
      active ? (isFail ? THEME.trafficRed : THEME.neon) : borderColor,
      panelBg,
      active || focused
    );
  }

  const ix = R.x + 2;
  const iw = R.w - 4;
  let cy = R.y + 1;

  // 1) 제목 — 폭이 허용하면 활성 verifier 바인딩 배지까지 표기
  const badgeTitle = `Verifier · ${state.rosterLabels.verifier}`;
  const titleText =
    stringWidth(badgeTitle) <= iw ? badgeTitle : 'Verifier · 교차 검증';
  buf.writeCentered(
    ix,
    cy++,
    iw,
    titleText,
    active || focused ? THEME.neon : lerpColor(THEME.neon, THEME.muted, 0.2),
    panelBg,
    true
  );

  // 2) 프로그레스 바 + 퍼센트(또는 idle 시 —) — 라이브 검증은 %를 못 주므로 부재 활성 바
  if (cy < R.y + R.h - 1) {
    const barW = Math.max(6, iw - 5);
    const startX = ix + Math.max(0, Math.floor((iw - (barW + 1 + 4)) / 2));
    const fillCol = isFail ? THEME.trafficRed : THEME.neon;
    if (isRun && state.neonProgress === 0) {
      drawIndeterminateBar(buf, startX, cy, barW, fillCol, THEME.bgNeonTrack, state.simTime, 1);
      buf.writeText(
        startX + barW + 1,
        cy,
        `${spinnerFrame(state.simTime, 1)} RUN`,
        THEME.neon,
        panelBg,
        true
      );
    } else {
      const pctStr =
        state.neonState === 'idle' ? '  — ' : padLeftWidth(`${state.neonProgress}%`, 4);
      buf.drawProgressBar(
        startX,
        cy,
        barW,
        state.neonState === 'idle' ? 0 : state.neonProgress / 100,
        fillCol,
        THEME.bgNeonTrack
      );
      buf.writeText(startX + barW + 1, cy, pctStr, THEME.muted, panelBg);
    }
    cy++;
  }

  // 3) 실행/완료/대기 상태
  if (cy < R.y + R.h - 1) {
    let statusLabel: string;
    let statusFg: RGB;
    if (isRun) {
      statusLabel = `◉ ${state.neonLabel || '교차 검증 중'}`;
      statusFg = THEME.neon;
    } else if (isPass) {
      statusLabel = '✓ 검증 통과';
      statusFg = THEME.green;
    } else if (isFail) {
      statusLabel = '✕ 검증 주의';
      statusFg = THEME.trafficRed;
    } else {
      statusLabel = iw >= 26 ? '○ 대기 · 호출 시 독립 검증' : '○ 대기 · 호출 시 검증';
      statusFg = THEME.muted;
    }
    buf.writeCentered(ix, cy++, iw, statusLabel, statusFg, panelBg, isRun || isPass || isFail);
  }

  // 4) 모델 플래그
  if (cy < R.y + R.h - 1) {
    buf.writeCentered(ix, cy++, iw, state.neonModelFlag, THEME.muted, panelBg, false);
  }
}

function renderDecisionPanel(buf: CellBuffer, state: AppState, L: FullLayout): void {
  const R = L.decision;
  const focused = state.focus === 'decision';
  const borderColor = focused ? lerpColor(THEME.green, THEME.borderFocusBoost, 0.35) : THEME.green;

  buf.drawBox(R, {
    style: focused ? 'heavy' : 'single',
    borderColor,
    fillBg: THEME.bg,
    rightLabel: focused ? 'Enter:상세' : undefined,
    rightLabelColor: THEME.green,
    boldBorder: focused,
  });

  const ix = R.x + 2;
  const iw = R.w - 4;
  let cy = R.y + 1;

  // 헤더: Decision Gate · 분기 계층  |  forks 1,286
  buf.writeText(ix, cy, 'Decision Gate · 분기 계층', THEME.green, THEME.bg, true, iw);
  const forksStr = state.totalForks.toLocaleString('en-US');
  const forksPrefix = 'forks ';
  const rightW = stringWidth(forksPrefix) + stringWidth(forksStr);
  if (iw > rightW + 24) {
    buf.writeSegments(ix + iw - rightW, cy, [
      { text: forksPrefix, fg: THEME.muted, bg: THEME.bg },
      { text: forksStr, fg: THEME.green, bg: THEME.bg, bold: true },
    ]);
  }
  cy++;

  // 라우트 행(N3): forcedSplit → split 순으로 우선 정렬 후, 패널 높이가 허용하는 슬롯 전부 활용(최소 3행)
  const labelW = Math.min(18, Math.max(14, Math.floor(iw * 0.28)));
  const statusW = 19; // "p=0.89 sharp m:24" — 13→19 확장, barW 자동 축소로 좁은 폭 오버플로우 방지
  const barW = Math.max(8, iw - labelW - statusW - 4);
  const maxRouteRows = Math.max(3, R.y + R.h - 1 - cy);

  for (const route of sortRoutesForDisplay(state.routes).slice(0, maxRouteRows)) {
    if (cy >= R.y + R.h - 1) break;
    const isSplit = route.forcedSplit === true || route.p < 0.7;
    const rowColor: RGB =
      route.forcedSplit === true ? THEME.trafficRed : isSplit ? THEME.orange : THEME.green;
    const trackBg: RGB = isSplit ? THEME.bgOrangeTrack : THEME.bgGreenTrack;
    let tag: string;
    if (route.forcedSplit === true) {
      tag = `p=${route.p.toFixed(2)} SPLIT!`;
    } else {
      tag = `p=${route.p.toFixed(2)} ${isSplit ? 'split' : 'sharp'}`;
      // basis가 주어지면 반드시 m:<n>(실측) / p:<n>(사전) 표기. undefined(데모·구버전)면 생략
      if (route.basis !== undefined) {
        tag += ` ${route.basis === 'measured' ? 'm' : 'p'}:${route.samples ?? 0}`;
      }
    }

    buf.writeText(ix, cy, padRightWidth(route.name, labelW), isSplit ? rowColor : THEME.fg, THEME.bg);
    buf.drawProgressBar(ix + labelW + 2, cy, barW, route.p, rowColor, trackBg);
    buf.writeText(
      ix + labelW + 2 + barW + 2,
      cy,
      tag,
      rowColor,
      THEME.bg,
      isSplit,
      Math.max(0, iw - labelW - barW - 4)
    );
    cy++;
  }

  // 하단 범례 (높이 여유가 있을 때 표시)
  if (cy < R.y + R.h - 1) {
    buf.writeSegments(
      ix,
      cy,
      [
        { text: 'sharp → 코드에서 실행', fg: THEME.green, bg: THEME.bg },
        { text: '   split → Lead', fg: THEME.muted, bg: THEME.bg },
      ],
      iw
    );
  }
}

function renderSwarmSection(buf: CellBuffer, state: AppState, L: FullLayout): void {
  // 위임 라벨 (하위 에이전트에 위임 · Workers) — 슬롯 초과 워커는 +N 배지로 안내
  const workerCards = workerCardsOf(state);
  const overflow = workerCards.length - L.cards.length;
  if (L.delegateLabelY < L.fanOutY) {
    const first = L.cards[0]!;
    const last = L.cards[L.cards.length - 1]!;
    const cardSpanW = last.x + last.w - first.x;
    // 카드 1~2장일 땐 라벨이 카드 폭보다 길어질 수 있어 decision 폭까지 중앙 확장
    const labelW = Math.max(cardSpanW, Math.min(36, L.decision.w));
    const labelX = L.centerX - Math.floor(labelW / 2);
    const labelSegs: Array<{ text: string; fg: RGB; bg: RGB; bold?: boolean }> = [
      { text: '하위 에이전트에 위임 · ', fg: THEME.fg, bg: THEME.bg },
      { text: 'Workers', fg: THEME.blue, bg: THEME.bg, bold: true },
    ];
    if (overflow > 0) {
      labelSegs.push({
        text: ` · +${overflow}개 더 (s:전체)`,
        fg: THEME.orange,
        bg: THEME.bg,
        bold: true,
      });
    }
    buf.writeSegmentsCentered(labelX, L.delegateLabelY, labelW, labelSegs);
  }

  const focused = state.focus === 'swarm';
  const errAdvisory = state.activeTrigger === 1;
  // 라이브 동기화 중이면 가변 길이 라이브 워커 카드를, 아니면 트랜스크립트 3슬롯 cards를
  const motion = liveMotionOf(state);

  L.cards.forEach((cardRect, idx) => {
    const card = workerCards[idx];
    if (!card) return;

    const cardAdvisoryGlow = errAdvisory && idx === 0;
    const cardBg = cardAdvisoryGlow ? THEME.bgGlowPurple : THEME.bg;
    const baseBorder = cardAdvisoryGlow ? THEME.purple : THEME.blue;
    const borderColor = focused
      ? lerpColor(baseBorder, THEME.borderFocusBoost, 0.35)
      : baseBorder;

    buf.drawBox(cardRect, {
      style: focused || cardAdvisoryGlow ? 'heavy' : 'single',
      borderColor,
      fillBg: cardBg,
      rightLabel: focused && idx === L.cards.length - 1 ? 'Enter:50슬롯' : undefined,
      rightLabelColor: THEME.blue,
      boldBorder: focused || cardAdvisoryGlow,
    });

    const ix = cardRect.x + 2;
    const iw = cardRect.w - 4;
    let cy = cardRect.y + 1;

    // 역할 이름 (search / grep / audit)
    buf.writeCentered(ix, cy++, iw, card.name, THEME.fg, cardBg, true);

    // 모델 태그 (활성 roster explore 바인딩)
    if (cy < cardRect.y + cardRect.h - 1 && cardRect.h >= 6) {
      buf.writeCentered(ix, cy++, iw, card.model, THEME.blue, cardBg, false);
    }

    // 프로그레스 바 + 퍼센트 — 라이브 실행 중 0%는 고정 바가 아니라
    // 부재 활성 바(흘러가는 블록) + 스피너 + RUN 으로 살아 있음을 표시한다.
    if (cy < cardRect.y + cardRect.h - 1) {
      const barW = Math.max(6, iw - 6);
      const startX = ix + Math.max(0, Math.floor((iw - (barW + 1 + 4)) / 2));
      if (motion && card.status === 'running' && card.progress === 0) {
        drawIndeterminateBar(
          buf,
          startX,
          cy,
          barW,
          THEME.blue,
          THEME.bgBlueTrack,
          state.simTime,
          idx
        );
        buf.writeText(
          startX + barW + 1,
          cy,
          typeof card.elapsedSec === 'number'
            ? `${spinnerFrame(state.simTime, idx)} ${formatElapsedMmss(card.elapsedSec)}`
            : `${spinnerFrame(state.simTime, idx)} RUN`,
          THEME.blue,
          cardBg,
          true
        );
      } else {
        const pctStr = padLeftWidth(`${card.progress}%`, 4);
        buf.drawProgressBar(startX, cy, barW, card.progress / 100, THEME.blue, THEME.bgBlueTrack);
        buf.writeText(startX + barW + 1, cy, pctStr, THEME.muted, cardBg);
      }
      cy++;
    }

    // 카드 설명
    if (cy < cardRect.y + cardRect.h - 1 && cardRect.h >= 7) {
      buf.writeCentered(cardRect.x + 1, cy++, cardRect.w - 2, card.description, THEME.fgSoft, cardBg, false);
    }

    // 상태 표시 (✓ 완료 / ⠙ 실행 중 / ▪ 대기 중 / ✕ 오류) — 실행 중 라이브 모션일 때만 회전
    if (cy < cardRect.y + cardRect.h - 1) {
      const isDone = card.status === 'done' || card.progress >= 100;
      const isIdle = card.status === 'idle' && card.progress === 0;
      const isErr = card.status === 'error';
      const label = isErr
        ? '✕ 오류'
        : isDone
          ? typeof card.elapsedSec === 'number'
            ? `✓ 완료 · ${formatElapsedMmss(card.elapsedSec)}`
            : '✓ 완료'
          : isIdle
            ? '▪ 대기 중'
            : motion
              ? `${spinnerFrame(state.simTime, idx)} 실행 중`
              : '◉ 실행 중';
      const fg = isErr
        ? THEME.trafficRed
        : isDone
          ? THEME.green
          : isIdle
            ? THEME.muted
            : THEME.blue;
      buf.writeCentered(ix, cy, iw, label, fg, cardBg, isDone);
    }
  });
}

function renderBackPanel(buf: CellBuffer, state: AppState, L: FullLayout): void {
  const R = L.back;
  const glowing = state.activeTrigger === 2;
  const panelBg = glowing ? THEME.bgGlowPurple : THEME.bg;
  const borderColor = glowing ? THEME.purple : THEME.orange;

  buf.drawBox(R, {
    style: glowing ? 'heavy' : 'single',
    borderColor,
    fillBg: panelBg,
    rightLabel: glowing ? 'Advisor 최종 점검' : undefined,
    rightLabelColor: THEME.purple,
    boldBorder: glowing,
  });

  const ix = R.x + 2;
  const iw = R.w - 4;
  const y1 = R.y + 1;
  buf.writeCentered(ix, y1, iw, '다시 Lead · 리드', THEME.orange, panelBg, true);
  if (y1 + 1 < R.y + R.h - 1) {
    if (state.mode === 'live') {
      const liveText = state.agentBusy && state.latestResponse
        ? (state.latestResponse.trim().split('\n').pop()?.trim() || state.backSummary)
        : state.backSummary;
      buf.writeCentered(ix, y1 + 1, iw, liveText, THEME.fg, panelBg, false);
    } else {
      buf.writeSegmentsCentered(ix, y1 + 1, iw, [
        { text: '검토 + 확인 ', fg: THEME.fg, bg: panelBg },
        { text: '· review + verify', fg: THEME.muted, bg: panelBg },
      ]);
    }
  }
}

function renderLogsPanel(buf: CellBuffer, state: AppState, R: Rect): void {
  const focused = state.focus === 'logs';
  const borderColor = focused ? THEME.fgSoft : THEME.borderLog;

  const filtered = getFilteredLogs(state);
  const innerRows = Math.max(1, R.h - 2);
  const maxScroll = Math.max(0, filtered.length - innerRows);
  const scroll = Math.min(state.logScrollOffset, maxScroll);

  const rightHints: string[] = [];
  if (state.logFilter !== 'all') rightHints.push(`필터:${LOG_FILTER_LABELS[state.logFilter]}`);
  if (scroll > 0) rightHints.push(`-${scroll}줄`);
  if (focused) rightHints.push('Enter:전체보기');

  buf.drawBox(R, {
    style: focused ? 'heavy' : 'single',
    borderColor,
    fillBg: THEME.bg,
    title: '세션 기록',
    titleColor: THEME.muted,
    rightLabel: rightHints.length > 0 ? rightHints.join(' · ') : undefined,
    rightLabelColor: THEME.green,
    boldBorder: focused,
  });

  const end = filtered.length - scroll;
  const start = Math.max(0, end - innerRows);
  const visible = filtered.slice(start, end);

  const ix = R.x + 2;
  const iw = R.w - 4;
  const timeColW = 10;
  const agentColW = Math.min(20, Math.max(15, Math.floor(iw * 0.16)));
  const msgColX = ix + timeColW + agentColW;
  const msgColW = Math.max(10, iw - timeColW - agentColW);

  for (let i = 0; i < visible.length; i++) {
    const entry = visible[i]!;
    const y = R.y + 1 + i;
    const isLatest = scroll === 0 && i === visible.length - 1;

    buf.writeText(ix, y, entry.time, THEME.dim, THEME.bg, false, timeColW);
    buf.writeText(
      ix + timeColW,
      y,
      padRightWidth(entry.agent, agentColW - 1),
      resolveColorKey(entry.color),
      THEME.bg,
      true,
      agentColW - 1
    );
    buf.writeText(
      msgColX,
      y,
      truncateWidth(entry.message, msgColW),
      isLatest ? THEME.fg : THEME.fgSoft,
      THEME.bg,
      isLatest,
      msgColW
    );
  }
}

function renderFooter(
  buf: CellBuffer,
  state: AppState,
  x: number,
  y: number,
  width: number
): void {
  const neonStatusText =
    state.neonState === 'run'
      ? '검증 중'
      : state.neonState === 'pass'
        ? '통과'
        : state.neonState === 'fail'
          ? '주의'
          : '대기';
  const compactStatus = width < 114;
  const s1 = compactStatus
    ? `[ 스웜 ${state.swarmActive}/${state.swarmMax} ]`
    : `[ 하위 에이전트 ${state.swarmActive}/${state.swarmMax} ]`;
  const s2 = `[ Verifier ${neonStatusText} ]`;
  const s3 = `[ ${state.advisorState} ]`;
  const s4 = `[ ${state.totalForks.toLocaleString('en-US')} 분기 ]`;
  const gap = compactStatus ? ' ' : '  ';
  const statusTotalW =
    stringWidth(s1) +
    stringWidth(s2) +
    stringWidth(s3) +
    stringWidth(s4) +
    gap.length * 3;
  const statusStartX = x + Math.max(0, width - statusTotalW);

  // 관측 전용(watch/stream) 모드: 가짜 대화형 프롬프트('$ pi █') 대신 읽기전용 상태·제어
  // 힌트를 푸터 전폭으로 표기한다. 작업 지시는 사용자의 AI 채팅창에서 자연어로 받는다.
  if ((state.mode === 'watch' || state.mode === 'stream') && !state.inputActive) {
    const flashActive = state.statusFlash && Date.now() < state.statusFlashUntil;
    if (flashActive) {
      buf.writeText(x, y, `· ${state.statusFlash}`, THEME.orange, THEME.bg, false, width);
      return;
    }
    const ctlHint =
      '관측 전용 (실시간 보기)  |  [?]사용법  [Tab]칸이동  [Enter]자세히  [Space]멈춤  [q]끄기';
    const shellHint = '작업 지시: AI 채팅창에서 요청 (창만 닫기: Ctrl+\\)';
    const shellW = stringWidth(shellHint);
    buf.writeText(x, y, ctlHint, THEME.muted, THEME.bg, false, Math.max(0, width - shellW - 3));
    if (width > shellW + 24) {
      buf.writeRight(x, y, width, shellHint, THEME.orange, THEME.bg);
    }
    return;
  }

  // 우측 상태 배지 4종
  if (statusStartX > x + 20) {
    buf.writeSegments(statusStartX, y, [
      { text: s1, fg: THEME.blue, bg: THEME.bg, bold: true },
      { text: gap, fg: THEME.muted, bg: THEME.bg },
      {
        text: s2,
        fg: state.neonState === 'fail' ? THEME.trafficRed : THEME.neon,
        bg: THEME.bg,
        bold: state.neonState !== 'idle',
      },
      { text: gap, fg: THEME.muted, bg: THEME.bg },
      { text: s3, fg: THEME.purple, bg: THEME.bg, bold: state.advisorState === '자문 중' },
      { text: gap, fg: THEME.muted, bg: THEME.bg },
      { text: s4, fg: THEME.green, bg: THEME.bg, bold: true },
    ]);
  }

  // 좌측 프롬프트 입력창 (`~/app $ pi █` — 활성 lead CLI 표기)
  const promptMaxW = Math.max(18, statusStartX - x - 2);
  const promptBg = state.inputActive ? THEME.bgInputFocus : THEME.bg;
  if (state.inputActive) {
    buf.fillRect({ x, y, w: promptMaxW, h: 1 }, promptBg);
  }

  const prefix1 = `${state.cwd} $ `;
  const prefix2 =
    state.mode === 'live' && state.backendKind === 'agy'
      ? 'agy '
      : `${state.rosterLabels.lead.split(':')[0] || 'lead'} `;
  let cx = x;
  cx += buf.writeText(cx, y, prefix1, THEME.green, promptBg, true);
  cx += buf.writeText(cx, y, prefix2, THEME.fg, promptBg, false);

  const cursorVisible = state.inputActive || state.simTime % 1 < 0.6;
  const cursorFg = cursorVisible ? THEME.green : THEME.dim;

  if (state.inputActive) {
    const availInput = Math.max(1, x + promptMaxW - cx - 2);
    const displayVal =
      stringWidth(state.inputValue) > availInput
        ? state.inputValue.slice(-availInput)
        : state.inputValue;
    cx += buf.writeText(cx, y, displayVal, THEME.fg, promptBg, true, availInput);
    buf.setChar(cx, y, '█', THEME.green, promptBg, true);
  } else {
    buf.setChar(cx, y, '█', cursorFg, promptBg, false);
    cx += 2;
    const remainHint = x + promptMaxW - cx;
    if (remainHint > 12) {
      const flashActive = state.statusFlash && Date.now() < state.statusFlashUntil;
      const idleHint =
        state.mode === 'live'
          ? state.agentBusy
            ? '· 에이전트 실행 중… (/cancel 중단 · r:실시간응답)'
            : '· /:작업지시  r:응답전문  Tab:패널이동  ?:도움말'
          : '· /명령어 입력  Tab:패널이동  Space:일시정지';
      const hintText = flashActive ? `· ${state.statusFlash}` : idleHint;
      buf.writeText(
        cx,
        y,
        hintText,
        flashActive || state.agentBusy ? THEME.orange : THEME.dim,
        THEME.bg,
        false,
        remainHint
      );
    }
  }
}

/**
 * 작은 분할 터미널(<98 cols 또는 <30 rows)을 위한 반응형 탭 모드 렌더링
 */
function renderCompactLayout(buf: CellBuffer, state: AppState, L: CompactLayout): void {
  renderHeader(buf, state, 1, L.headerY, L.cols - 2);

  const tabs = [
    `[1:파이프라인]`,
    `[2:스웜(${state.swarmActive})]`,
    `[3:Advisor(${state.advisorState})]`,
    `[4:기록]`,
  ] as const;

  let tx = 1;
  tabs.forEach((label, idx) => {
    const active = state.compactTab === idx;
    const fg = active ? THEME.bg : THEME.muted;
    const bg = active
      ? idx === 0
        ? THEME.orange
        : idx === 1
          ? THEME.blue
          : idx === 2
            ? THEME.purple
            : THEME.green
      : THEME.bg;
    tx += buf.writeText(tx, L.tabBarY, ` ${label} `, fg, bg, active);
    tx += 1;
  });

  const B = L.body;
  if (state.compactTab === 0) {
    buf.drawBox(B, {
      borderColor: THEME.orange,
      title: `Lead(${state.phase}) → Decision Gate(${state.totalForks} forks)`,
      titleColor: THEME.orange,
    });
    const ix = B.x + 2;
    const iw = B.w - 4;
    let cy = B.y + 1;
    buf.writeText(
      ix,
      cy++,
      `Lead 메인: ${state.mainSummary} (${effortToGauge(state.effort)})`,
      THEME.orange,
      THEME.bg,
      true,
      iw
    );
    cy++;
    for (const r of sortRoutesForDisplay(state.routes)) {
      if (cy >= B.y + B.h - 2) break;
      const split = r.forcedSplit === true || r.p < 0.7;
      const col = r.forcedSplit === true ? THEME.trafficRed : split ? THEME.orange : THEME.green;
      buf.writeText(ix, cy, padRightWidth(r.name, 16), col, THEME.bg, false, 16);
      buf.drawProgressBar(
        ix + 17,
        cy,
        Math.max(8, iw - 33),
        r.p,
        col,
        split ? THEME.bgOrangeTrack : THEME.bgGreenTrack
      );
      let tag: string;
      if (r.forcedSplit === true) {
        tag = `p=${r.p.toFixed(2)} SPLIT!`;
      } else {
        tag = `p=${r.p.toFixed(2)} ${split ? 'split' : 'sharp'}`;
        if (r.basis !== undefined) {
          tag += ` ${r.basis === 'measured' ? 'm' : 'p'}:${r.samples ?? 0}`;
        }
      }
      buf.writeRight(ix, cy, iw, tag, col, THEME.bg, r.forcedSplit === true);
      cy++;
    }
    cy++;
    if (cy < B.y + B.h - 1) {
      const neonText =
        state.neonState === 'run'
          ? `◉ ${state.neonLabel} (${state.neonProgress}%)`
          : state.neonState === 'pass'
            ? `✓ 검증 통과 (${state.neonVerified}건)`
            : state.neonState === 'fail'
              ? `✕ 검증 주의`
              : `○ 대기 · 호출 시 독립 검증`;
      buf.writeText(
        ix,
        cy++,
        `Verifier: ${neonText}`,
        state.neonState === 'pass' ? THEME.green : THEME.neon,
        THEME.bg,
        state.neonState !== 'idle',
        iw
      );
    }
    if (cy < B.y + B.h - 1) {
      buf.writeText(
        ix,
        cy++,
        `Advisor(${state.advisorState}): ${state.latestAdvice}`,
        THEME.purple,
        THEME.bg,
        false,
        iw
      );
    }
    if (cy < B.y + B.h - 1) {
      buf.writeText(ix, cy, `리드 검증: ${state.backSummary}`, THEME.orange, THEME.bg, false, iw);
    }
  } else if (state.compactTab === 1) {
    buf.drawBox(B, {
      borderColor: THEME.blue,
      title: `Workers 하위 에이전트 (${state.swarmActive}/${state.swarmMax})`,
      titleColor: THEME.blue,
    });
    const ix = B.x + 2;
    const iw = B.w - 4;
    let cy = B.y + 1;
    const motion = liveMotionOf(state);
    for (const [idx, c] of workerCardsOf(state).entries()) {
      if (cy >= B.y + B.h - 1) break;
      buf.writeText(ix, cy, padRightWidth(c.name, 10), THEME.blue, THEME.bg, true);
      const barW = Math.max(10, iw - 28);
      if (motion && c.status === 'running' && c.progress === 0) {
        drawIndeterminateBar(buf, ix + 11, cy, barW, THEME.blue, THEME.bgBlueTrack, state.simTime, idx);
        buf.writeRight(ix, cy, iw, typeof c.elapsedSec === 'number' ? `${spinnerFrame(state.simTime, idx)} ${formatElapsedMmss(c.elapsedSec)}` : `${spinnerFrame(state.simTime, idx)} RUN`, THEME.blue, THEME.bg, true);
      } else {
        buf.drawProgressBar(ix + 11, cy, barW, c.progress / 100, THEME.blue, THEME.bgBlueTrack);
        const runningSpin = motion && c.status === 'running' && c.progress < 100;
        buf.writeRight(
          ix,
          cy,
          iw,
          `${c.progress}% ${
            c.progress >= 100
              ? typeof c.elapsedSec === 'number'
                ? `✓완료 · ${formatElapsedMmss(c.elapsedSec)}`
                : '✓완료'
              : runningSpin
                ? `${spinnerFrame(state.simTime, idx)} 실행`
                : '◉실행'
          }`,
          c.progress >= 100 ? THEME.green : THEME.blue,
          THEME.bg
        );
      }
      cy++;
      if (cy < B.y + B.h - 1) {
        buf.writeText(ix + 2, cy++, c.description, THEME.muted, THEME.bg, false, iw - 2);
      }
    }
  } else if (state.compactTab === 2) {
    buf.drawBox(B, {
      borderColor: THEME.purple,
      title: `Advisor · 자문 (${state.advisorState})`,
      titleColor: THEME.purple,
    });
    const ix = B.x + 2;
    const iw = B.w - 4;
    let cy = B.y + 1;
    TRIGGER_LABELS.forEach((lbl, idx) => {
      if (cy >= B.y + B.h - 1) return;
      const act = state.activeTrigger === idx;
      buf.writeText(
        ix,
        cy++,
        `${act ? '●' : '◇'} ${lbl}`,
        act ? THEME.purple : THEME.muted,
        act ? THEME.bgTriggerActive : THEME.bg,
        act,
        iw
      );
    });
    cy++;
    for (const line of wrapByWidth(state.latestAdvice, iw, 3)) {
      if (cy >= B.y + B.h - 1) break;
      buf.writeText(ix, cy++, line, THEME.purple, THEME.bg, true, iw);
    }
    if (cy < B.y + B.h - 1) {
      buf.writeText(
        ix,
        cy,
        `호출 수: ${state.carbonCalls}  ·  읽은 토큰: ${state.tokensRead}`,
        THEME.muted,
        THEME.bg,
        false,
        iw
      );
    }
  } else {
    renderLogsPanel(buf, state, B);
  }

  renderFooter(buf, state, 1, L.footerY, L.cols - 2);
}
