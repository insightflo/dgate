import type { Rect } from '../core/buffer.ts';

export interface FullLayout {
  readonly kind: 'full';
  readonly cols: number;
  readonly rows: number;
  readonly padX: number;
  readonly showChrome: boolean;
  readonly headerY: number;
  readonly navY: number;

  readonly advisor: Rect;
  readonly tr0Row: number;
  readonly tr1Row: number;
  readonly tr2Row: number;

  readonly centerX: number;
  readonly main: Rect;
  readonly neon: Rect;
  readonly neonLinkRow: number;
  readonly decision: Rect;
  readonly delegateLabelY: number;
  readonly fanOutY: number;
  /** 실제 렌더링되는 워커 카드 수(1..maxCardSlots) — 중심축 centerX 기준 정렬 */
  readonly cards: readonly Rect[];
  readonly fanInY: number;
  readonly back: Rect;

  readonly logs: Rect;
  readonly footerY: number;
}

export interface CompactLayout {
  readonly kind: 'compact';
  readonly cols: number;
  readonly rows: number;
  readonly headerY: number;
  readonly tabBarY: number;
  readonly body: Rect;
  readonly footerY: number;
}

export type ComputedLayout = FullLayout | CompactLayout;

/** 전체 레이아웃 파이프라인 영역 폭 — 카드 슬롯 상한 산정과 computeLayout이 공유. */
export function pipelineWidthFor(cols: number): number {
  return pipelineMetricsFor(cols).pipeW;
}

function pipelineMetricsFor(cols: number): { padX: number; pipeX: number; pipeW: number; advisorW: number } {
  const padX = cols >= 136 ? 2 : 1;
  const advisorW = cols >= 142 ? 36 : cols >= 122 ? 32 : 28;
  const bridgeMin = cols >= 142 ? 6 : cols >= 122 ? 5 : 3;
  const pipeX = padX + advisorW + bridgeMin;
  return { padX, pipeX, pipeW: Math.max(58, cols - padX - pipeX), advisorW };
}

/** 현재 폭에서 렌더링 가능한 워커 카드 최대 슬롯 수 — 파이프 폭 기반 2~4개. */
export function maxCardSlotsFor(cols: number): number {
  const pipeW = pipelineWidthFor(cols);
  return pipeW >= 92 ? 4 : pipeW >= 68 ? 3 : 2;
}

export function computeLayout(cols: number, rows: number, visibleCardCount = 3): ComputedLayout {
  if (cols < 98 || rows < 30) {
    const headerY = 0;
    const tabBarY = 2;
    const footerY = Math.max(4, rows - 1);
    const bodyY = 3;
    const bodyH = Math.max(3, footerY - bodyY);
    return {
      kind: 'compact',
      cols,
      rows,
      headerY,
      tabBarY,
      body: { x: 1, y: bodyY, w: Math.max(10, cols - 2), h: bodyH },
      footerY,
    };
  }

  const padX = cols >= 136 ? 2 : 1;
  const showChrome = rows >= 38;
  const headerY = showChrome ? 1 : 0;
  const navY = headerY + 2;
  const footerY = rows - 1;

  // 로그 패널 높이 결정 (원본은 5줄 = 높이 7)
  const logLines = rows >= 41 ? 5 : rows >= 36 ? 4 : 3;
  const logH = logLines + 2;
  const bottomGap = rows >= 40 ? 1 : 0;
  const logsY = footerY - bottomGap - logH;

  const topGap = rows >= 42 ? 1 : 0;
  const workTop = navY + 1 + topGap;
  const workBottom = logsY - (rows >= 39 ? 1 : 0);
  const workH = Math.max(21, workBottom - workTop);

  // 우측 파이프라인 각 박스 높이 배분 (기본 최소 21행 필요: 5 + 1 + 6 + 2 + 5 + 1 + 3 = 23 -> 초소형 시 컴팩트 높이)
  const roomy = workH >= 28;
  const medium = workH >= 25;

  const mainH = roomy ? 6 : 5;
  const decisionH = roomy ? 7 : 6;
  const cardH = roomy ? 7 : medium ? 6 : 5;
  const backH = roomy ? 4 : 3;

  const fixedBoxesH = mainH + decisionH + cardH + backH;
  const remainWireRows = Math.max(4, workH - fixedBoxesH);

  // 와이어 구간 3곳 분배: gap1(main->decision), gap2(decision->cards: 라벨+팬아웃 포함), gap3(cards->back: 팬인)
  const gap2 = Math.min(4, Math.max(2, Math.ceil(remainWireRows * 0.5)));
  const restAfterGap2 = Math.max(2, remainWireRows - gap2);
  const gap1 = Math.max(1, Math.floor(restAfterGap2 / 2));
  const gap3 = Math.max(1, restAfterGap2 - gap1);

  // 좌우 폭 계산
  const { pipeX, pipeW, advisorW } = pipelineMetricsFor(cols);
  const centerX = pipeX + Math.floor(pipeW / 2);

  // 각 패널 X/W 계산 (Lead 메인 중앙 축 대칭 + 우측 Verifier 교차 검증 패널)
  const rightBound = cols - padX;
  const neonLinkGap = pipeW >= 96 ? 4 : pipeW >= 78 ? 3 : 2;
  const neonW = pipeW >= 112 ? 32 : pipeW >= 96 ? 28 : pipeW >= 78 ? 26 : 23;
  const neonX = rightBound - neonW;
  const neonY = workTop;

  const idealMainW = pipeW >= 112 ? 42 : pipeW >= 96 ? 38 : pipeW >= 78 ? 36 : 34;
  const maxRightHalf = neonX - neonLinkGap - centerX;
  let mainW = idealMainW;
  let mainX = centerX - Math.floor(mainW / 2);

  if (mainX + mainW > neonX - neonLinkGap) {
    if (maxRightHalf >= 17) {
      mainW = Math.min(idealMainW, maxRightHalf * 2);
      mainX = centerX - Math.floor(mainW / 2);
    } else {
      const maxMainW = Math.max(26, neonX - neonLinkGap - (pipeX + 2));
      mainW = Math.min(idealMainW, maxMainW);
      mainX = neonX - neonLinkGap - mainW;
    }
  }
  if (centerX >= mainX + mainW - 1) {
    mainX = Math.min(neonX - neonLinkGap - mainW, centerX - mainW + 3);
  }
  const mainY = workTop;
  const neonLinkRow = mainY + Math.floor(mainH / 2);

  const decisionW = Math.min(70, pipeW - 2);
  const decisionX = centerX - Math.floor(decisionW / 2);
  const decisionY = mainY + mainH + gap1;

  const cardsY = decisionY + decisionH + gap2;
  // 가변 워커 카드 — 실제 보여줄 카드 수만큼(1..maxCardSlots) centerX 중심 정렬.
  // 3카드일 때 폭/간격 공식은 기존과 동일하게 유지(하위 호환 렌더 동일성).
  const maxCols = maxCardSlotsFor(cols);
  const visibleCount = Math.max(1, Math.min(Math.floor(visibleCardCount), maxCols));
  const cardGap = visibleCount >= 4 ? 2 : pipeW >= 96 ? 3 : 2;
  const maxCardsSpan = Math.min(pipeW, 102);
  const cardW = Math.max(18, Math.floor((maxCardsSpan - cardGap * (visibleCount - 1)) / visibleCount));
  const actualCardsSpan = cardW * visibleCount + cardGap * (visibleCount - 1);
  const cardsStartX = centerX - Math.floor(actualCardsSpan / 2);
  const cardRects: Rect[] = [];
  for (let i = 0; i < visibleCount; i++) {
    cardRects.push({ x: cardsStartX + (cardW + cardGap) * i, y: cardsY, w: cardW, h: cardH });
  }

  const fanOutY = cardsY - 1;
  const delegateLabelY = gap2 >= 2 ? cardsY - 2 : fanOutY;

  const backY = cardsY + cardH + gap3;
  const backW = Math.min(52, pipeW - 4);
  const backX = centerX - Math.floor(backW / 2);
  const fanInY = cardsY + cardH;

  const pipelineBottom = backY + backH;
  const advisorRect: Rect = {
    x: padX,
    y: mainY,
    w: advisorW,
    h: Math.max(18, pipelineBottom - mainY),
  };

  // Advisor 3대 트리거 행을 우측 타겟 패널 내부 행과 1:1 정렬 (상단 세션 정보 3줄 확보)
  const tr0Row = mainY + Math.min(mainH - 2, Math.max(3, Math.floor(mainH / 2) + 1));
  const tr1Row = cardsY + Math.floor(cardH / 2);
  const tr2Row = backY + Math.floor(backH / 2);

  const logsRect: Rect = {
    x: padX,
    y: logsY,
    w: cols - padX * 2,
    h: logH,
  };

  return {
    kind: 'full',
    cols,
    rows,
    padX,
    showChrome,
    headerY,
    navY,
    advisor: advisorRect,
    tr0Row,
    tr1Row,
    tr2Row,
    centerX,
    main: { x: mainX, y: mainY, w: mainW, h: mainH },
    neon: { x: neonX, y: neonY, w: neonW, h: mainH },
    neonLinkRow,
    decision: { x: decisionX, y: decisionY, w: decisionW, h: decisionH },
    delegateLabelY,
    fanOutY,
    cards: cardRects,
    fanInY,
    back: { x: backX, y: backY, w: backW, h: backH },
    logs: logsRect,
    footerY,
  };
}
