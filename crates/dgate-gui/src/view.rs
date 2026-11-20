//! dgate-gui GPUI 뷰 — TUI 5계층 플로우 토폴로지의 네이티브 Metal 렌더링.
//!
//! 목적: 터미널 TUI(`src/ui/renderApp.ts` + `src/theme.ts`)의 시그니처 레이아웃을
//! GPUI 네이티브 창에 그대로 옮긴다 — 상단 크롬(모드 배지·라이브 카운터·컨트롤),
//! 범례+레포 필터 스트립, 좌측 Advisor 자문 패널 + 우측 4단 파이프라인(Lead→
//! Decision Gate→Workers→다시 Lead, Verifier 교차 검증 링크), 하단 세션 기록,
//! 푸터 상태 바. 색은 `src/theme.ts` 팔레트를 정수 리터럴로 1:1 이식했다.
//!
//! 외부 연결:
//! - `state::DashboardController` — 스냅숏 적용·탭 필터·DEMO 전환·2단계 중단 상태.
//! - `state::PollerHandle` — 새로고침 웨이크·일시정지 플래그(뷰 스레드 I/O 금지).
//! - UI 갱신은 250ms 타이머가 백그라운드 폴러의 version 카운터만 관찰해 `cx.notify()`.
//!
//! 주의:
//! - 이 파일의 데이터 조립은 전부 `state.rs` 순수 로직 위임 — 뷰는 렌더링만 담당.
//! - `hc kill`은 리스너에서 직접 실행하지 않고 백그라운드 실행기로 보내 UI가 멈추지
//!   않게 한다. 결과는 `note_kill_result`로 1줄 회신.
//! - 동적 N스템 커넥터 — 보이는 워커 수(1/2/3/4+)와 정확히 같은 수의 스템을 그리고
//!   초과 워커는 2행 랩 + '+M개 추가' 배지로 절대 가리지 않는다.
//! - 라이브 모션 — 실제 활동 중(`state::has_active_work`)일 때만 16ms(~60fps)
//!   프레임으로 벽시계 경과(anim_secs)를 갱신해 연속 부동소수 모션을 그린다.
//!   라이브 유휴(실행 0)는 anim_secs 갱신도 cx.notify도 없는 제로-첸 침묵
//!   상태를 유지한다(유휴 폴링은 여전히 조용한 250ms 버전 관찰).
//! - 무 지터 레이아웃 — 모든 애니메이션 와이어·점은 고정 픽셀 높이 컨테이너 안에서
//!   `.relative()` + `.absolute()` 오프셋으로만 움직인다(프레임마다 자식 증감·세그
//!   먼트 높이 변화 금지). 카드의 모든 텍스트 줄은 truncate + 고정 높이로 줄바
//!   꿈·카드 높이 변화를 원천 차단하고 스피너는 고정폭 박스에 담아 수평 흔들림도
//!   없다.
//! - 반응형 — viewport 폭에 따라 Advisor/Pipeline·Lead/Verifier 스택킹(960px)과
//!   워커 그리드 열 상한(820px 기준 2열)이 바뀐다(640px까지 자유 리사이즈).
//! - Zed 스타일 스플리터 3종 — Advisor 폭(세로)·Verifier 폭(와이어 영역 세로)·
//!   세션 기록 높이(가로)를 마우스 드래그로 조절하고 더블클릭으로 기본값 복귀.
//!   크기 상태는 스냅숏과 무관한 `PanelSizes`(폴링 사이 불변 → 드래그 중 점프 없음)
//!   이고 드래그 추적은 루트 컨테이너의 mouse_move/mouse_up이 담당한다.
//! - 역할별 다중 세션 — 같은 레포에도 lead/verifier/advisor 세션이 여러 개 떠 있을
//!   수 있어 각자 박스에 주소(t155/t182/t165…)로 구분해 나열하고 Workers 그리드는
//!   하위 워커(sub_workers)만 그린다(역할 세션 중복 배치 금지). 부모 매핑은
//!   `hc ls --json`의 parent 필드에서 유도해 '← 부모' pill로 표시한다.
//! - 수직 컴팩트 — 780px 창 높이에 헤더→푸터 전체가 한 화면으로 들어온다
//!   (overflow_y_scroll은 창을 매우 줄였을 때의 안전 폴백으로만 남아 있다).
//! - 대기 상태(워커 0)에는 가짜 진행·애니메이션을 두지 않는다 — 침착한 청사진 슬롯.
//!   완료(done) 워커의 활동바 채움은 주소 해시의 안정 값(재렌더링에도 변하지 않는다).

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gpui::{
    div, prelude::*, px, relative, Context, FontWeight, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Render, Rgba, SharedString, Window,
};

use crate::state::{
    continuous_phase, format_elapsed_mmss, has_active_work, kill_worker_safe,
    lead_display_count, partition_sessions_by_role, smooth_pulse_opacity,
    visible_worker_columns, DashboardController, DashboardSnapshot, DefaultKillExecutor,
    DecisionLabel, DragTarget, GuiCliOptions, LogPanelSize, PollerHandle, RecentLogEntry,
    RouteMetric, ACTIVE_ANIM_FRAME_MS, UI_REFRESH_TICK_MS,
};
use dgate_core::live_sync::LiveWorkerInfo;
use dgate_core::redact::redact_secrets;

// ---------------------------------------------------------------------------
// 팔레트 — src/theme.ts 1:1 이식 (gpui::rgb 는 const fn 이 아니라 수동 변환)
// ---------------------------------------------------------------------------

const fn hex_rgba(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

const BG: Rgba = hex_rgba(0x1b2026); // THEME.bg
const BG_CHROME: Rgba = hex_rgba(0x252a32); // THEME.bgChrome
const BG_ELEVATED: Rgba = hex_rgba(0x20262e); // THEME.bgElevated
const BG_GLOW_PURPLE: Rgba = hex_rgba(0x242132); // THEME.bgGlowPurple
const BG_GLOW_NEON: Rgba = hex_rgba(0x26261e); // THEME.bgGlowNeon
const BG_GREEN_TRACK: Rgba = hex_rgba(0x2b3d34); // THEME.bgGreenTrack
const BG_ORANGE_TRACK: Rgba = hex_rgba(0x3c3029); // THEME.bgOrangeTrack
const BG_BLUE_TRACK: Rgba = hex_rgba(0x293342); // THEME.bgBlueTrack
const BG_NEON_TRACK: Rgba = hex_rgba(0x3b3928); // THEME.bgNeonTrack

const TEXT: Rgba = hex_rgba(0xd6dce3); // THEME.fg
const TEXT_SOFT: Rgba = hex_rgba(0xaeb6c2); // THEME.fgSoft
const MUTED: Rgba = hex_rgba(0x89919b); // THEME.muted
const DIM: Rgba = hex_rgba(0x69727f); // THEME.dim

const BORDER_HEADER: Rgba = hex_rgba(0x4c535c); // THEME.borderHeader
const BORDER_WIRE: Rgba = hex_rgba(0x505761); // THEME.borderWire
const BORDER_ADVISORY_IDLE: Rgba = hex_rgba(0x5a5668); // THEME.borderAdvisoryIdle
const BORDER_LOG: Rgba = hex_rgba(0x454c56); // THEME.borderLog

const LEAD_ORANGE: Rgba = hex_rgba(0xe9a17c); // Lead · 리드
const WORKER_BLUE: Rgba = hex_rgba(0x93b9ef); // Workers · 스웜
const GATE_GREEN: Rgba = hex_rgba(0x8cd5ae); // Decision Gate · 분기
const ADVISOR_PURPLE: Rgba = hex_rgba(0xbda6ef); // Advisor · 자문
const VERIFIER_NEON: Rgba = hex_rgba(0xe8d97a); // Verifier · 교차 검증
const ROSE_DANGER: Rgba = hex_rgba(0xf16e69); // trafficRed — 강제분기/중단

const ROSE_BG_HOVER: Rgba = hex_rgba(0x3a2230); // 중단 버튼 호버 배경

/// 브라일 스피너 프레임 — 활동 중인 카드·역할 패널이 10Hz로 회전.
const SPINNER_FRAMES: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

/// 슬롯 idx만큼 위상을 옮긴 스피너 문자 — 여러 워커가 돌아도 페이즈가 흩어진다.
/// anim_secs는 실측 벽시계 경과(초)라 10Hz 회전이 프레임 카운터와 무관하게 정확하다.
fn spinner_char(anim_secs: f32, idx: usize) -> char {
    let tick = (anim_secs * 10.0) as u64;
    SPINNER_FRAMES[(tick.wrapping_add(idx as u64) % 8) as usize]
}

/// 뷰 엔티티 — GPUI 창 1개가 1개 소유.
pub struct DashboardView {
    pub controller: DashboardController,
    pub poller: PollerHandle,
    pub db_path_label: String,
    pub poll_interval_label: String,
    pub hc_bin: String,
    /// 라이브 모션 기준 경과 초 — 활동 중일 때만 흐르는 실측 벽시계(유휴 정지·제로-첸).
    pub anim_secs: f32,
    /// anim_secs의 기준 시각 — 프레임마다 elapsed로 환산(누적 오차 없는 벽시계).
    pub anim_epoch: Instant,
    /// 진행 중인 스플리터 드래그 — (대상 축, 드래그 시작 마우스 축 좌표, 시작 크기).
    /// None이면 유휴 — 루트의 mouse_move 리스너는 아무 일도 하지 않는다.
    pub active_drag: Option<(DragTarget, f32, f32)>,
}

impl DashboardView {
    pub fn new(opts: &GuiCliOptions, poller: PollerHandle, cx: &mut Context<Self>) -> Self {
        let mut controller = DashboardController::new(opts.watch_only);
        // `--demo` 기동 — 첫 화면부터 내장 토폴로지 미리보기.
        controller.demo_mode = opts.demo;
        let shared = poller.shared.clone();
        // 백그라운드 폴러 관찰 태스크 — version 변화 감지 + 활동 중 60fps 연속 모션.
        // 유휴(!demo && 실행 워커 0)에는 anim_secs 갱신·cx.notify 모두 하지 않고
        // 프레임 주기도 조용한 250ms 버전 폴링으로 내려간다(완전 제로-첸 유휴).
        cx.spawn(async move |this, cx| {
            let mut last_version = 0u64;
            let mut last_active = false;
            loop {
                // 적응형 프레임 — 활동 중 16ms(~60fps), 유휴는 250ms 버전 폴링.
                cx.background_executor()
                    .timer(Duration::from_millis(if last_active {
                        ACTIVE_ANIM_FRAME_MS
                    } else {
                        UI_REFRESH_TICK_MS
                    }))
                    .await;
                if shared.shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let (version, snap) = shared.take();
                let version_changed = version != last_version;
                if version_changed {
                    last_version = version;
                }
                let updated = this
                    .update(cx, |view, cx| {
                        if version_changed {
                            if let Some(snap) = snap {
                                // 워커 최초 관측 시각(mm:ss 타이머 기준점)도 여기서 갱신.
                                view.controller.apply_snapshot(snap);
                            }
                        }
                        // 활동 게이트 — DEMO 또는 실행 중(done 아님) 워커가 있을 때만
                        // 연속 모션으로 다시 그린다. 원 스냅숏에 활동이 없으면
                        // filtered() 복제 비용도 들이지 않는다(완전 유휴 크론 0).
                        let demo = view.controller.demo_mode;
                        let coarse = demo
                            || view
                                .controller
                                .snapshot
                                .as_ref()
                                .is_some_and(|s| has_active_work(s, false));
                        let active = coarse && has_active_work(&view.controller.filtered(), demo);
                        if active {
                            // 실측 벽시계 경과 — 프레임 카운터가 아닌 연속 시간.
                            view.anim_secs = view.anim_epoch.elapsed().as_secs_f32();
                            last_active = true;
                            cx.notify();
                        } else {
                            last_active = false;
                            // 유휴 중 새 스냅숏 — 데이터 반영 1회만 다시 그린다.
                            if version_changed {
                                cx.notify();
                            }
                        }
                    })
                    .is_ok();
                if !updated {
                    break; // 뷰 소멸 — 태스크도 종료
                }
            }
        })
        .detach();

        DashboardView {
            controller,
            poller,
            db_path_label: opts
                .db_path
                .clone()
                .unwrap_or_else(|| "~/.local/state/dgate/decision.db".to_string()),
            poll_interval_label: format!("{}ms", opts.poll_ms),
            hc_bin: "hc".to_string(),
            anim_secs: 0.0,
            anim_epoch: Instant::now(),
            active_drag: None,
        }
    }

    /// 스플리터 드래그 시작 — 현재 크기를 기준으로 찍는다(이후 이동량은 순수 델타).
    fn begin_panel_drag(&mut self, target: DragTarget, start_mouse_axis_px: f32) {
        let start_size = self.controller.panel_sizes.value_of(target);
        self.active_drag = Some((target, start_mouse_axis_px, start_size));
    }

    // -----------------------------------------------------------------------
    // 1. 상단 헤더 바 — 타이틀·모드 배지·프로필 | 라이브 카운터 | 컨트롤
    // -----------------------------------------------------------------------

    fn render_header(&self, snap: &DashboardSnapshot, cx: &Context<Self>) -> gpui::Div {
        // 모드 배지 — WATCH(청) / LIVE(녹) / DEMO(주), TUI renderHeader와 동일 우선순위
        let (mode_text, mode_color) = if self.controller.watch_only {
            ("[읽기전용 관측 · WATCH]", WORKER_BLUE)
        } else if self.controller.demo_mode {
            ("[시뮬레이션 · DEMO]", LEAD_ORANGE)
        } else {
            ("[대화형 제어 · LIVE]", GATE_GREEN)
        };

        let left = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .child(
                div()
                    .text_size(px(16.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(TEXT)
                    .child("DGATE"),
            )
            .child(pill(mode_text, mode_color, BG_CHROME))
            .child(pill(format!("profile={}", snap.profile_name), MUTED, BG_CHROME))
            .when(!snap.hc_available && !self.controller.demo_mode, |el| {
                el.child(pill("hc 없음", LEAD_ORANGE, BG_CHROME))
            });

        // 라이브 카운터 — TUI 헤더 우측 4색 카운터의 역할별 live 세션 수.
        // 역할별 세션 분할 기준 — LEAD는 아래 lead_display_count 계약(하위 워커
        // 실행 중이면 최소 1), WORKERS는 sub_workers(explore·coder·기타)만 센다.
        let parts = partition_sessions_by_role(&snap.workers);
        let counters = div()
            .flex()
            .flex_1()
            .flex_row()
            .items_center()
            .justify_center()
            .gap_3()
            .overflow_hidden()
            .child(counter_label("LEAD", lead_display_count(snap), LEAD_ORANGE))
            .child(counter_label("WORKERS", parts.sub_workers.len(), WORKER_BLUE))
            .child(counter_label("VERIFIES", parts.verifiers.len(), VERIFIER_NEON))
            .child(counter_label("ADVISOR", parts.advisors.len(), ADVISOR_PURPLE));

        // 우측 컨트롤 — LIVE/DEMO 토글 · 새로고침 · 일시정지 · 제어 허용
        let mut right = div().flex().flex_row().items_center().gap_2();
        let demo_on = self.controller.demo_mode;
        right = right.child(if demo_on {
            div()
                .id("btn-demo")
                .flex()
                .items_center()
                .px_3()
                .h_8()
                .rounded_md()
                .bg(LEAD_ORANGE)
                .text_size(px(12.))
                .font_weight(FontWeight::BOLD)
                .text_color(BG_CHROME)
                .cursor_pointer()
                .child("DEMO")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.toggle_demo_mode();
                    cx.notify();
                }))
        } else {
            div()
                .id("btn-demo")
                .flex()
                .items_center()
                .px_3()
                .h_8()
                .rounded_md()
                .border_1()
                .border_color(BORDER_HEADER)
                .bg(BG_ELEVATED)
                .hover(|s| s.border_color(GATE_GREEN))
                .text_size(px(12.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(GATE_GREEN)
                .cursor_pointer()
                .child("LIVE")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.controller.toggle_demo_mode();
                    cx.notify();
                }))
        });
        right = right.child(action_button(
            "새로고침",
            "btn-refresh",
            WORKER_BLUE,
            cx.listener(|this, _, _, cx| {
                if this.controller.demo_mode {
                    this.controller.last_action_message =
                        Some("DEMO 미리보기는 새로고침 대상이 아닙니다".to_string());
                } else {
                    this.poller.wake();
                    this.controller.last_action_message = Some("새로고침 요청".to_string());
                }
                cx.notify();
            }),
        ));
        let (pause_label, pause_color) = if self.controller.paused {
            ("재개", GATE_GREEN)
        } else {
            ("일시정지", LEAD_ORANGE)
        };
        right = right.child(action_button(
            pause_label,
            "btn-pause",
            pause_color,
            cx.listener(|this, _, _, cx| {
                this.controller.toggle_pause();
                this.poller.set_paused(this.controller.paused);
                cx.notify();
            }),
        ));
        if !self.controller.watch_only {
            let (control_label, control_color, control_active) =
                if self.controller.control_enabled {
                    ("제어 허용됨", ROSE_DANGER, true)
                } else {
                    ("제어 허용", MUTED, false)
                };
            right = right.child(
                div()
                    .id("btn-control")
                    .flex()
                    .items_center()
                    .px_3()
                    .h_8()
                    .rounded_md()
                    .border_1()
                    .border_color(if control_active { control_color } else { BORDER_HEADER })
                    .bg(if control_active { BG_ELEVATED } else { BG_CHROME })
                    .hover(|s| s.border_color(control_color))
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if control_active { control_color } else { MUTED })
                    .cursor_pointer()
                    .child(control_label)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.controller.toggle_control();
                        cx.notify();
                    })),
            );
        }

        // 이중 강조 하단 보더 — 1px 헤더 라인 + 1px 5색 역할 액센트 스트립
        let double_border = div()
            .flex()
            .flex_col()
            .child(div().h(px(1.)).w_full().bg(BORDER_HEADER))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .h(px(1.))
                    .child(div().flex_1().h_full().bg(LEAD_ORANGE).opacity(0.85))
                    .child(div().flex_1().h_full().bg(WORKER_BLUE).opacity(0.85))
                    .child(div().flex_1().h_full().bg(GATE_GREEN).opacity(0.85))
                    .child(div().flex_1().h_full().bg(ADVISOR_PURPLE).opacity(0.85))
                    .child(div().flex_1().h_full().bg(VERIFIER_NEON).opacity(0.85)),
            );

        div()
            .flex()
            .flex_col()
            .bg(BG_CHROME)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap_3()
                    .gap_y_2()
                    .min_h_10()
                    .px_4()
                    .child(left)
                    .child(counters)
                    .child(right),
            )
            .child(double_border)
    }

    // -----------------------------------------------------------------------
    // 2. 범례 + 레포 필터 스트립 (TUI renderLegendNav 대응)
    // -----------------------------------------------------------------------

    fn render_legend_strip(&self, snap: &DashboardSnapshot, cx: &Context<Self>) -> gpui::Div {
        let legend = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_3()
            .child(legend_item(
                LEAD_ORANGE,
                format!("Lead · {}", role_model_label(snap, "lead")),
            ))
            .child(legend_item(
                WORKER_BLUE,
                format!(
                    "Workers · {}/{}",
                    role_model_label(snap, "explore"),
                    role_model_label(snap, "coder")
                ),
            ))
            .child(legend_item(GATE_GREEN, "Decision Gate".to_string()))
            .child(legend_item(
                VERIFIER_NEON,
                format!("Verifier · {}", role_model_label(snap, "verifier")),
            ))
            .child(legend_item(
                ADVISOR_PURPLE,
                format!("Advisor · {}", role_model_label(snap, "advisor")),
            ));

        let mut tabs = div().flex().flex_row().items_center().gap_1();
        tabs = tabs.child(repo_tab(
            "tab-global",
            format!("전체 ({})", snap.workers.len()),
            self.controller.selected_repo.is_none(),
            cx.listener(|this, _, _, cx| {
                this.controller.select_repo(None);
                cx.notify();
            }),
        ));
        for repo in &snap.detected_repos {
            let count = snap.workers.iter().filter(|w| &w.repo_tag == repo).count();
            let selected = self.controller.selected_repo.as_deref() == Some(repo.as_str());
            let repo_label = repo.clone();
            tabs = tabs.child(repo_tab(
                SharedString::from(format!("tab-repo-{}", repo)),
                format!("{} ({})", repo, count),
                selected,
                cx.listener(move |this, _, _, cx| {
                    this.controller.select_repo(Some(repo_label.as_str()));
                    cx.notify();
                }),
            ));
        }

        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .gap_y_2()
            .min_h(px(28.))
            .py_0()
            .px_4()
            .bg(BG)
            .child(legend)
            .child(tabs)
    }

    // -----------------------------------------------------------------------
    // 3. 메인 토폴로지 캔버스 — 좌측 Advisor 패널 + 우측 4단 파이프라인
    // -----------------------------------------------------------------------

    fn render_topology_canvas(
        &self,
        snap: &DashboardSnapshot,
        cx: &Context<Self>,
        vw: Pixels,
    ) -> gpui::Stateful<gpui::Div> {
        let advisor = self.render_advisor_panel(snap, vw);
        let pipeline = self.render_pipeline(snap, cx, vw);
        // 반응형 본문 — 넓은 창(≥960px) 2단 나란히(사이에 드래그 스플리터) /
        // 좁은 창 Advisor 위 스택(스플리터 없음).
        let body = if vw < px(960.) {
            div()
                .flex()
                .flex_col()
                .gap_3()
                .w_full()
                .child(advisor)
                .child(pipeline)
        } else {
            // CSS flexbox 기본 align_items=stretch — Advisor 패널이 행 전체 높이를 채운다
            div()
                .flex()
                .flex_row()
                .gap_3()
                .w_full()
                .child(advisor)
                .child(self.render_advisor_splitter(cx))
                .child(pipeline)
        };
        div()
            .id("topology-canvas")
            .overflow_y_scroll()
            .flex_1()
            .min_h_0()
            .px_4()
            .py_2()
            .child(body)
    }

    /// 좌측 Advisor 자문 패널 — 보라 경계 + 3개 자문 게이트 + 최근 조언 카드.
    /// 좁은 창(<960px)에서는 전체 폭으로 파이프라인 위에 스택된다.
    fn render_advisor_panel(&self, snap: &DashboardSnapshot, vw: Pixels) -> gpui::Div {
        let advisor_running = snap
            .workers
            .iter()
            .any(|w| w.slot.as_str() == "advisor" && worker_running(w));
        let advisors = partition_sessions_by_role(&snap.workers).advisors;
        let active = self.controller.demo_mode || advisor_running;
        let border = if active { ADVISOR_PURPLE } else { BORDER_ADVISORY_IDLE };
        let model = role_model_label(snap, "advisor");
        let (state_label, state_color) = if active {
            ("자문 중", ADVISOR_PURPLE)
        } else {
            ("침묵 · 대기", MUTED)
        };
        let advice_summary = if self.controller.demo_mode {
            "shadow 26건 · 호출 3회 · 읽은 토큰 31.4K".to_string()
        } else {
            match &snap.shadow_summary {
                Some(s) => format!("shadow 평가 {}건 · 호출 — · 토큰 —", s.total),
                None => "shadow 평가 없음 · 호출 — · 토큰 —".to_string(),
            }
        };
        // 활동 중 헤더 스피너 — 고정폭 박스에 담아 활성/비활성 전환에도 레이아웃가
        // 흔들리지 않고, 대기 중에는 빈 박스로 정지 상태 그대로.
        let title = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .min_w_0()
            .child(
                div()
                    .w(px(14.))
                    .flex_shrink_0()
                    .text_size(px(13.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(ADVISOR_PURPLE)
                    .when(active, |el| {
                        el.child(spinner_char(self.anim_secs, 0).to_string())
                    }),
            )
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(ADVISOR_PURPLE)
                    .truncate()
                    .child("Advisor"),
            );

        let mut panel = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_lg()
            .bg(if active { BG_GLOW_PURPLE } else { BG_ELEVATED })
            .border_1()
            .border_color(border);
        // 반응형 폭 — 넓은 창 드래그 가능 고정 폭 사이드 패널 / 좁은 창 전체 폭.
        panel = if vw < px(960.) {
            panel.w_full()
        } else {
            panel
                .w(px(self.controller.panel_sizes.advisor_width))
                .flex_shrink_0()
        };
        panel = panel
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_h(px(18.))
                    .child(title)
                    .child(div().flex_1())
                    .child(pill(state_label, state_color, BG)),
            )
            .child(
                div()
                    .h(px(14.))
                    .text_size(px(10.))
                    .text_color(if active { ADVISOR_PURPLE } else { MUTED })
                    .truncate()
                    .child(model),
            );
        // 명시적 advisor 세션 — 자문 카드 위에 세션마다 고정 높이 1행(주소 구분).
        // 동일 레포에 여러 자문 세션이 떠 있어도 각자 보인다.
        for (i, w) in advisors.iter().enumerate() {
            let elapsed = self
                .controller
                .worker_elapsed_secs(&w.address, Some(i))
                .map(format_elapsed_mmss);
            panel = panel.child(role_session_row(
                w,
                i,
                ADVISOR_PURPLE,
                elapsed,
                self.anim_secs,
                false,
                true,
            ));
        }
        panel
            // 자문 게이트 3종 — 활성 게이트 배지는 사인 호흡(대기 정지)
            .child(advisor_gate(
                true,
                "계획 확정 전",
                "━━◈▶",
                self.anim_secs,
                active,
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_1()
                    .rounded_md()
                    .bg(BG)
                    .border_1()
                    .border_color(BORDER_LOG)
                    .child(
                        div()
                            .h(px(14.))
                            .text_size(px(10.))
                            .text_color(MUTED)
                            .truncate()
                            .child("최근 조언"),
                    )
                    .child(
                        div()
                            .h(px(14.))
                            .text_size(px(10.))
                            .text_color(ADVISOR_PURPLE)
                            .truncate()
                            .child(advice_summary),
                    )
                    .child(
                        div()
                            .h(px(14.))
                            .text_size(px(10.))
                            .text_color(MUTED)
                            .truncate()
                            .child("일상 턴에서는 침묵"),
                    ),
            )
            .child(advisor_gate(
                false,
                "같은 오류 2회",
                "┄┄▷",
                self.anim_secs,
                active,
            ))
            .child(advisor_gate(
                false,
                "완료 전",
                "┄┄▷",
                self.anim_secs,
                active,
            ))
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(DIM)
                    .truncate()
                    .child("코드는 직접 쓰지 않습니다 · 조언은 Lead가 적용합니다"),
            )
    }

    /// 우측 4단 파이프라인 — Lead+Verifier → Decision Gate → Workers → 다시 Lead.
    /// 커넥터 스템 수는 하위 워커(sub_workers) 수와 정확히 일치한다(동적 N스템) —
    /// 리드/검증/자문 세션은 각자의 박스에만 나타나고 그리드로 중복 배치되지 않는다.
    fn render_pipeline(&self, snap: &DashboardSnapshot, cx: &Context<Self>, vw: Pixels) -> gpui::Div {
        // 하위 워커만 그리드에 — 역할별 분할로 리드/검증/자문 세션을 제외한다.
        let sub_workers = partition_sessions_by_role(&snap.workers).sub_workers;
        // 보이는 워커 열 수 — 좁은 창(<820px)은 2열 상한(카드 가독성), 넓으면 최대 4열.
        let col_cap = if vw < px(820.) { 2 } else { 4 };
        let cols = if sub_workers.is_empty() {
            // 라이브 0 — 3슬롯 청사진(뷰포트와 무관한 침묵 3스템 토폴로지)
            visible_worker_columns(0)
        } else {
            visible_worker_columns(sub_workers.len()).min(col_cap)
        };
        // 첫 행(최대 cols) 슬롯별 활성 플래그 — 활성 스템만 밝게, 대기 스템은 뮤트.
        let stem_flags: Vec<bool> = if sub_workers.is_empty() {
            vec![false; cols]
        } else {
            sub_workers
                .iter()
                .take(cols)
                .map(|w| worker_running(w))
                .collect()
        };

        // Stage 1 — Lead 메인 ↔ Verifier 교차 검증 (넓은 창: 수평 나란히 — 와이어 전체가
        // Verifier 폭 드래그 핸들 / 좁은 창: 스택, 핸들 없음)
        let stage1 = if vw < px(960.) {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(self.render_lead_card(snap))
                .child(horizontal_wire(
                    self.verifier_motion(snap),
                    self.anim_secs,
                ))
                .child(self.render_verifier_card(snap, vw))
        } else {
            div()
                .flex()
                .flex_row()
                .gap_1()
                .child(self.render_lead_card(snap))
                .child(self.render_verifier_drag_handle(snap, cx))
                .child(self.render_verifier_card(snap, vw))
        };

        div()
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .min_w_0()
            .w_full()
            .child(stage1)
            // Lead → Decision Gate 수직 와이어 — 모션 중 주황 점이 매끄럽게 내려간다.
            .child(vertical_wire(self.lead_motion(snap), self.anim_secs))
            // Stage 2 — Decision Gate · 분기 계층
            .child(self.render_decision_gate(snap))
            // Stage 3 — Workers 로의 팬아웃(동적 N스템 + 초과분 '+M개 추가' 배지)
            .child(branching_connector(
                cols,
                &stem_flags,
                self.anim_secs,
                sub_workers.len(),
            ))
            .child(self.render_workers_section(snap, cx, &sub_workers, cols))
            .child(converging_connector(cols, &stem_flags, self.anim_secs))
            // Stage 4 — 다시 Lead · 리드
            .child(self.render_back_bar(snap))
    }

    // -----------------------------------------------------------------------
    // 3b. Zed 스타일 스플리터 — 드래그 리사이즈 핸들 3종 (더블클릭 리셋)
    // -----------------------------------------------------------------------

    /// Advisor|Pipeline 세로 스플리터(≥960px에서만 표시) — 오른쪽 드래그로 Advisor
    /// 폭 확장, 더블클릭 기본폭 복귀. 핸들 자체가 고정 8px 트랙이라 드래그 중에도
    /// 레이아웃 지오메트리는 일정하다(무지터).
    fn render_advisor_splitter(&self, cx: &Context<Self>) -> gpui::Div {
        let dragging =
            matches!(self.active_drag, Some((DragTarget::AdvisorWidth, _, _)));
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .w(px(8.))
            .flex_none()
            .rounded_sm()
            .border_1()
            .border_color(BORDER_WIRE)
            .cursor_ew_resize()
            .hover(|s| s.bg(BG_GLOW_PURPLE))
            .when(dragging, |el| el.bg(BG_GLOW_PURPLE))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    if ev.click_count >= 2 {
                        this.controller.reset_panel(DragTarget::AdvisorWidth);
                    } else {
                        this.begin_panel_drag(DragTarget::AdvisorWidth, f32::from(ev.position.x));
                    }
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(2.))
                    .flex_none()
                    .rounded_full()
                    .bg(if dragging { ADVISOR_PURPLE } else { BORDER_ADVISORY_IDLE }),
            )
    }

    /// Lead↔Verifier 와이어 전체를 Verifier 폭 드래그 핸들로(≥960px) — 왼쪽 드래그로
    /// Verifier 확장, 더블클릭 기본폭 복귀. 와이어 애니메이션은 그대로 살아있다.
    fn render_verifier_drag_handle(
        &self,
        snap: &DashboardSnapshot,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let dragging =
            matches!(self.active_drag, Some((DragTarget::VerifierWidth, _, _)));
        div()
            .flex()
            .flex_row()
            .items_center()
            .min_w(px(44.))
            .flex_none()
            .rounded_sm()
            .cursor_ew_resize()
            .hover(|s| s.bg(BG_GLOW_NEON))
            .when(dragging, |el| el.bg(BG_GLOW_NEON))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    if ev.click_count >= 2 {
                        this.controller.reset_panel(DragTarget::VerifierWidth);
                    } else {
                        this.begin_panel_drag(DragTarget::VerifierWidth, f32::from(ev.position.x));
                    }
                    cx.notify();
                }),
            )
            .child(horizontal_wire(self.verifier_motion(snap), self.anim_secs))
    }

    /// 세션 기록 상단 가로 스플리터 — 위로 드래그하면 패널이 높아지고 더블클릭 시
    /// 기본 높이로 복귀. 48px 알약 그립은 드래그 중 GATE_GREEN으로 켜진다.
    fn render_log_splitter(&self, cx: &Context<Self>) -> gpui::Div {
        let dragging = matches!(self.active_drag, Some((DragTarget::LogHeight, _, _)));
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_center()
            .mx_4()
            .h(px(8.))
            .flex_none()
            .rounded_sm()
            .cursor_ns_resize()
            .hover(|s| s.bg(BG_GREEN_TRACK))
            .when(dragging, |el| el.bg(BG_GREEN_TRACK))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                    if ev.click_count >= 2 {
                        this.controller.reset_panel(DragTarget::LogHeight);
                    } else {
                        this.begin_panel_drag(DragTarget::LogHeight, f32::from(ev.position.y));
                    }
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(48.))
                    .h(px(2.))
                    .flex_none()
                    .rounded_full()
                    .bg(if dragging { GATE_GREEN } else { BORDER_WIRE }),
            )
    }

    /// Verifier 모션 여부 — verifier 실행 중이거나 DEMO 미리보기.
    fn verifier_motion(&self, snap: &DashboardSnapshot) -> bool {
        self.controller.demo_mode || verifier_active(snap)
    }

    /// Lead 모션 여부 — lead 실행 중이거나 하위 워커 실행 중 또는 DEMO 미리보기.
    fn lead_motion(&self, snap: &DashboardSnapshot) -> bool {
        self.controller.demo_mode
            || lead_active(snap)
            || snap
                .workers
                .iter()
                .any(|w| w.slot.as_str() != "lead" && worker_running(w))
    }

    /// Stage 1 좌측 — Lead 카드. 모션: lead 실행 중 또는 하위 워커 실행 중 / DEMO.
    /// 명시적 lead 세션이 있으면 세션마다 고정 높이 1행(주소로 구분 — 동일 레포
    /// 다중 lead도 각자 보인다), 없으면 기존 폴백(조율 중/대기 중) 표기.
    fn render_lead_card(&self, snap: &DashboardSnapshot) -> gpui::Div {
        let parts = partition_sessions_by_role(&snap.workers);
        let model = role_model_label(snap, "lead");
        let subordinates_running = parts.sub_workers.iter().any(|w| worker_running(w));
        let motion = self.controller.demo_mode
            || parts.leads.iter().any(|w| worker_running(w))
            || subordinates_running;
        let state_pill = if motion {
            spinner_pill(
                spinner_char(self.anim_secs, 0),
                if subordinates_running || self.controller.demo_mode {
                    "워커 조율 중".to_string()
                } else {
                    "작업 중".to_string()
                },
                LEAD_ORANGE,
                BG,
            )
        } else {
            pill("대기 중", MUTED, BG)
        };
        let mut card = div()
            .flex()
            .flex_col()
            .gap_1()
            .flex_1()
            .min_w_0()
            .p_2()
            .rounded_lg()
            .bg(BG_ELEVATED)
            .border_2()
            .border_color(LEAD_ORANGE)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_h(px(18.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(LEAD_ORANGE)
                            .truncate()
                            .child(format!("Lead · {}", model)),
                    )
                    .child(div().flex_1())
                    .child(state_pill),
            );
        if parts.leads.is_empty() {
            // 세션 없음 폴백 — 기존 단일 카드 표기(활동 1줄 + effort/profile).
            let activity = match snap.workers.iter().find(|w| w.slot.as_str() == "lead") {
                Some(w) => redact_secrets(&w.activity),
                None => "계획 수립·워커 배차 대기".to_string(),
            };
            card = card
                .child(
                    div()
                        .h(px(16.))
                        .text_size(px(10.))
                        .text_color(TEXT_SOFT)
                        .truncate()
                        .child(activity),
                )
                .child(
                    div()
                        .h(px(14.))
                        .text_size(px(10.))
                        .text_color(MUTED)
                        .truncate()
                        .child(format!("effort ▮▮▮▮ · profile={}", snap.profile_name)),
                );
        } else {
            for (i, w) in parts.leads.iter().enumerate() {
                let elapsed = self
                    .controller
                    .worker_elapsed_secs(&w.address, Some(i))
                    .map(format_elapsed_mmss);
                card = card.child(role_session_row(
                    w,
                    i,
                    LEAD_ORANGE,
                    elapsed,
                    self.anim_secs,
                    true,
                    true,
                ));
            }
        }
        card
    }

    /// Stage 1 우측 — Verifier 교차 검증 카드. 모션: verifier 실행 중 / DEMO.
    /// 좁은 창(<960px)에서는 전체 폭으로 Lead 아래 스택된다.
    /// 명시적 verifier 세션이 있으면 세션마다 (메타행 + 네온 슬라이딩 바 + 활동 1줄)
    /// 블록으로 나열 — 동일 레포 다중 검증 세션도 주소로 구분된다.
    fn render_verifier_card(&self, snap: &DashboardSnapshot, vw: Pixels) -> gpui::Div {
        let parts = partition_sessions_by_role(&snap.workers);
        let active = self.verifier_motion(snap);
        let model = role_model_label(snap, "verifier");
        let badge_pill = if active {
            spinner_pill(
                spinner_char(self.anim_secs, 0),
                "검증 중".to_string(),
                VERIFIER_NEON,
                BG,
            )
        } else {
            pill("대기", MUTED, BG)
        };
        let mut card = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_lg()
            .bg(if active { BG_GLOW_NEON } else { BG_ELEVATED })
            .border_2()
            .border_color(if active { VERIFIER_NEON } else { BORDER_WIRE });
        card = if vw < px(960.) {
            card.w_full()
        } else {
            card.w(px(self.controller.panel_sizes.verifier_width)).flex_shrink_0()
        };
        card = card.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .min_h(px(18.))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(if active { VERIFIER_NEON } else { TEXT_SOFT })
                        .truncate()
                        .child(format!("Verifier · {}", model)),
                )
                .child(div().flex_1())
                .child(badge_pill),
        );
        if parts.verifiers.is_empty() {
            // 진행 트랙 — 활성: 네온 알약이 좌↔우 글라이드 / 대기: 빈 트랙(위장 금지).
            // 높이는 양쪽 다 5px 고정 — 카드 높이 점프 없음.
            card = card
                .child(if active {
                    sliding_pill_track(BG_NEON_TRACK, VERIFIER_NEON, self.anim_secs, 0)
                } else {
                    div().flex().w_full().h(px(5.)).flex_none().rounded_full().bg(BG_NEON_TRACK)
                })
                .child(
                    div()
                        .h(px(14.))
                        .text_size(px(10.))
                        .text_color(if active { VERIFIER_NEON } else { MUTED })
                        .truncate()
                        .child(if active {
                            "◉ 계획·결과 교차 검토"
                        } else {
                            "○ 교차 검증 대기"
                        }),
                );
        } else {
            for (i, w) in parts.verifiers.iter().enumerate() {
                let running = worker_running(w);
                let elapsed = self
                    .controller
                    .worker_elapsed_secs(&w.address, Some(i))
                    .map(format_elapsed_mmss);
                card = card
                    .child(role_session_row(
                        w,
                        i,
                        VERIFIER_NEON,
                        elapsed,
                        self.anim_secs,
                        false,
                        false,
                    ))
                    // 세션별 네온 트랙 — 실행 중 글라이드 / 완료 빈 트랙(높이 5px 고정).
                    .child(if running {
                        sliding_pill_track(BG_NEON_TRACK, VERIFIER_NEON, self.anim_secs, i)
                    } else {
                        div().flex().w_full().h(px(5.)).flex_none().rounded_full().bg(BG_NEON_TRACK)
                    })
                    .child(
                        div()
                            .h(px(14.))
                            .text_size(px(10.))
                            .text_color(if running { VERIFIER_NEON } else { MUTED })
                            .truncate()
                            .child(redact_secrets(&w.activity)),
                    );
            }
        }
        card
    }

    /// Stage 2 — Decision Gate · 분기 계층 (라우트 확률 바 + 우측 compact 지표).
    fn render_decision_gate(&self, snap: &DashboardSnapshot) -> gpui::Div {
        let forks_label = match &snap.timing_summary {
            Some(t) if !t.intervals_unavailable && t.forks_total > 0 => {
                format!("{}", t.forks_total)
            }
            _ => "unknown".to_string(),
        };
        let cost_label = match &snap.cost_summary {
            Some(c) => match c.total_usd_display {
                Some(v) => format!("${:.2}", v),
                // 불변식 C-1 — unknown이 있으면 총액을 위장하지 않는다
                None => "unknown".to_string(),
            },
            None => "unknown".to_string(),
        };
        let shadow_label = snap
            .shadow_summary
            .as_ref()
            .map(|s| format!("{}건", s.shadow_count))
            .unwrap_or_else(|| "0건".to_string());

        let mut panel = div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .rounded_lg()
            .bg(BG_ELEVATED)
            .border_1()
            .border_color(GATE_GREEN)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_h(px(16.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::BOLD)
                            .text_color(GATE_GREEN)
                            .child("Decision Gate · 분기 계층"),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(MUTED)
                            .truncate()
                            .child(format!(
                                "forks {} · 비용 {} · shadow {}",
                                forks_label, cost_label, shadow_label
                            )),
                    ),
            );
        // 표시 순서 — 강제분기 → split → sharp (TUI sortRoutesForDisplay와 동일)
        let mut routes = snap.routes.clone();
        routes.sort_by_key(route_rank);
        for r in &routes {
            panel = panel.child(route_row(r));
        }
        panel
    }

    /// Stage 3 — Workers 그리드 — 보이는 하위 워커 수 = 열 수(1/2/3/4+, 초과 2행 랩).
    /// lead/verifier/advisor 세션은 호출부에서 이미 제외되어 여기 내려오지 않는다.
    /// 라이브 0이면 3슬롯 청사진(침묵 3스템 와이어와 동일 지오메트리).
    fn render_workers_section(
        &self,
        snap: &DashboardSnapshot,
        cx: &Context<Self>,
        sub_workers: &[LiveWorkerInfo],
        cols: usize,
    ) -> gpui::Div {
        if sub_workers.is_empty() {
            // 라이브 워커 0 — 침착한 청사진 슬롯 3열
            let mut grid = div().flex().flex_row();
            for slot in ["explore", "coder", "verifier"] {
                grid = grid.child(blueprint_slot_card(slot, &role_model_label(snap, slot)));
            }
            return grid;
        }
        if cols == 1 {
            // 단일 워커 — 중앙 정렬 카드(단일 중앙 스템과 정렬, 가로 레일 없음)
            return div().flex().flex_row().justify_center().child(
                self.render_worker_card(&sub_workers[0], cx, 0, parent_of(snap, &sub_workers[0]))
                    .w_full()
                    .max_w(px(460.)),
            );
        }
        // 2~4열 — 카드 flex_1·mx_1 → 셀 중심 = 스템 중심 정확 정렬.
        // cols 초과분은 cols열씩 랩하고 마지막 행 빈 열은 스페이서로 열 폭 유지.
        let mut column = div().flex().flex_col().gap_2();
        for (row_idx, chunk) in sub_workers.chunks(cols).enumerate() {
            let mut row = div().flex().flex_row();
            for (i, w) in chunk.iter().enumerate() {
                row = row.child(
                    self.render_worker_card(w, cx, row_idx * cols + i, parent_of(snap, w))
                        .flex_1()
                        .min_w_0()
                        .mx_1(),
                );
            }
            for _ in chunk.len()..cols {
                row = row.child(div().flex_1().h_0().mx_1());
            }
            column = column.child(row);
        }
        column
    }

    /// 실행 중 워커 카드 1개 — 활동바 + 1줄 활동 + 실측 경과 타이머(mm:ss) +
    /// (제어 허용 시) 2단계 중단 버튼 + (부모 있으면) '← 부모 주소' pill. idx는 보이는
    /// 그리드 순서 — 스피너·슬라이딩 알약 위상을 워커마다 흩는다. 모든 줄이 고정
    /// 높이(truncate)라 형제 카드·연속 프레임 사이에 세로 1px 흔들림도 없다.
    /// 폭 설정은 호출부에서 결정한다.
    fn render_worker_card(
        &self,
        w: &LiveWorkerInfo,
        cx: &Context<Self>,
        idx: usize,
        parent: Option<&str>,
    ) -> gpui::Div {
        let confirming = self.controller.confirm_kill_address.as_deref() == Some(w.address.as_str());
        let can_control = self.controller.control_enabled
            && !self.controller.watch_only
            && !self.controller.demo_mode;
        let running = w.status.as_deref() != Some("done");
        let frac = stable_activity_frac(&w.address);
        // 실측 경과 — LIVE는 최초 관측 시각 기준, DEMO는 결정론적 표본값.
        let elapsed_label = self
            .controller
            .worker_elapsed_secs(&w.address, Some(idx))
            .map(format_elapsed_mmss);

        let mut actions = div().flex().flex_row().gap_2().items_center().min_w_0();
        if confirming {
            actions = actions
                .child(
                    div()
                        .text_size(px(10.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(ROSE_DANGER)
                        .child("정말 중단?"),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("kill-confirm-{}", w.address)))
                        .px_2()
                        .h(px(18.))
                        .flex()
                        .items_center()
                        .rounded_md()
                        .bg(ROSE_DANGER)
                        .text_size(px(10.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(BG)
                        .cursor_pointer()
                        .child("정말 중단")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let Some(addr) = this.controller.confirm_kill() else {
                                cx.notify();
                                return;
                            };
                            let addr_for_note = addr.clone();
                            let workers = this
                                .controller
                                .snapshot
                                .as_ref()
                                .map(|s| s.workers.clone())
                                .unwrap_or_default();
                            let hc_bin = this.hc_bin.clone();
                            let poller = this.poller.clone();
                            cx.spawn(async move |this, cx| {
                                let res = cx
                                    .background_executor()
                                    .spawn(async move {
                                        kill_worker_safe(
                                            &addr,
                                            &workers,
                                            &hc_bin,
                                            &DefaultKillExecutor,
                                        )
                                    })
                                    .await;
                                this.update(cx, |view, cx| {
                                    view.controller.note_kill_result(&addr_for_note, &res);
                                    cx.notify();
                                })
                                .ok();
                                poller.wake();
                            })
                            .detach();
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("kill-cancel-{}", w.address)))
                        .px_2()
                        .h(px(18.))
                        .flex()
                        .items_center()
                        .rounded_md()
                        .border_1()
                        .border_color(BORDER_HEADER)
                        .text_size(px(10.))
                        .text_color(MUTED)
                        .cursor_pointer()
                        .child("취소")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller.cancel_kill();
                            cx.notify();
                        })),
                );
        } else if can_control {
            let addr = w.address.clone();
            actions = actions.child(
                div()
                    .id(SharedString::from(format!("kill-ask-{}", w.address)))
                    .px_2()
                    .h(px(18.))
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(ROSE_DANGER)
                    .text_size(px(10.))
                    .text_color(ROSE_DANGER)
                    .cursor_pointer()
                    .hover(|s| s.bg(ROSE_BG_HOVER))
                    .child("중단")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.controller.request_kill(&addr);
                        cx.notify();
                    })),
            );
        }

        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .min_h(px(96.))
            .rounded_lg()
            .bg(BG_ELEVATED)
            .border_1()
            .border_color(if confirming { ROSE_DANGER } else { WORKER_BLUE })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .min_h(px(18.))
                    .child(pill(format!("[{}]", w.repo_tag), WORKER_BLUE, BG))
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(TEXT)
                            .truncate()
                            .child(w.name.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(MUTED)
                            .child(w.slot.as_str().to_string()),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(WORKER_BLUE)
                            .truncate()
                            .child(w.short_model.clone()),
                    ),
            )
            // 활동바 — 실행 중: WORKER_BLUE 알약이 좌↔우 연속 글라이드(실시간 미정
            // 진행), 완료: 주소 해시의 안정 채움(애니메이션 없음, 완료율 아님).
            // 양쪽 다 높이 5px 고정 — 카드 높이 점프 없음.
            .child(if running {
                sliding_pill_track(BG_BLUE_TRACK, WORKER_BLUE, self.anim_secs, idx)
            } else {
                div()
                    .flex()
                    .w_full()
                    .h(px(5.))
                    .flex_none()
                    .rounded_full()
                    .bg(BG_BLUE_TRACK)
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .w(relative(frac))
                            .rounded_full()
                            .bg(WORKER_BLUE),
                    )
            })
            .child(
                div()
                    .h(px(16.))
                    .text_size(px(10.))
                    .text_color(TEXT_SOFT)
                    .truncate()
                    .child(redact_secrets(&w.activity)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .min_h(px(20.))
                    .child(if running {
                        spinner_status(
                            spinner_char(self.anim_secs, idx),
                            match &elapsed_label {
                                Some(t) => format!("실행 중 · {}", t),
                                None => "실행 중".to_string(),
                            },
                            WORKER_BLUE,
                        )
                    } else {
                        div()
                            .text_size(px(10.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(GATE_GREEN)
                            .truncate()
                            .child(match &elapsed_label {
                                Some(t) => format!("✓ 완료 · {}", t),
                                None => "✓ 완료".to_string(),
                            })
                    })
                    // 부모 리드 표시 — 어느 Lead 세션이 이 워커를 스폰했는지(미상 생략).
                    .when(parent.is_some(), |el| {
                        el.child(
                            div()
                                .flex()
                                .items_center()
                                .px_1()
                                .h(px(14.))
                                .flex_none()
                                .max_w(px(84.))
                                .rounded_sm()
                                .border_1()
                                .border_color(BORDER_WIRE)
                                .text_size(px(9.))
                                .text_color(MUTED)
                                .truncate()
                                .child(format!("← {}", parent.unwrap_or_default())),
                        )
                    })
                    .child(div().flex_1())
                    .child(actions),
            )
    }

    /// Stage 4 — 다시 Lead · 리드 (compact 바 + 우측 timing/cost/shadow 요약).
    fn render_back_bar(&self, snap: &DashboardSnapshot) -> gpui::Div {
        let delay = match &snap.timing_summary {
            Some(t) if !t.intervals_unavailable => fmt_duration(t.wait_ms),
            _ => "—".to_string(),
        };
        let gain = match &snap.timing_summary {
            Some(t) if !t.intervals_unavailable => fmt_duration(t.parallel_gain_ms),
            _ => "—".to_string(),
        };
        let cost = match &snap.cost_summary {
            Some(c) => format!("${:.2}", c.known_usd),
            None => "—".to_string(),
        };
        let agreement = snap
            .shadow_summary
            .as_ref()
            .and_then(|s| s.disagreement_rate)
            .map(|r| format!("{:.1}%", (1.0 - r) * 100.0))
            .unwrap_or_else(|| "—".to_string());
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .min_h(px(26.))
            .rounded_lg()
            .bg(BG_ELEVATED)
            .border_1()
            .border_color(LEAD_ORANGE)
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(LEAD_ORANGE)
                    .child("다시 Lead · 리드"),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(MUTED)
                    .truncate()
                    .child(format!(
                        "평균 지연 {} · 절감 {} · 비용 {} · shadow 일치율 {}",
                        delay, gain, cost, agreement
                    )),
            )
    }

    // -----------------------------------------------------------------------
    // 4. 하단 세션 기록 패널 — decision_events 최신 이벤트 (역할 색 태그)
    // -----------------------------------------------------------------------

    fn render_session_log(&self, snap: &DashboardSnapshot, cx: &Context<Self>) -> gpui::Div {
        let size = self.controller.log_panel_size;
        let rows = self.controller.panel_sizes.visible_log_rows();
        let mut panel = div()
            .flex()
            .flex_col()
            .gap_1()
            .mx_4()
            .mb_2()
            .p_2()
            .flex_none()
            // 드래그 가능 고정 높이 — 상단 스플리터가 이 값을 움직인다. 높이에는
            // 헤더 크롬(~34px)이 포함되고 본문은 내부 스크롤로 창을 밀어내지 않는다.
            .h(px(self.controller.panel_sizes.log_height))
            .rounded_lg()
            .bg(BG_ELEVATED)
            .border_1()
            .border_color(BORDER_LOG)
            // 헤더 바 — 제목·건수 + 크기 토글(접기/작게/크게, 현재 모드 강조)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .min_h(px(16.))
                    .child(
                        div()
                            .id("log-panel-cycle")
                            .flex()
                            .items_center()
                            .text_size(px(11.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(TEXT_SOFT)
                            .cursor_pointer()
                            .hover(|s| s.text_color(TEXT))
                            .child("세션 기록")
                            .on_click(cx.listener(|this, _, _, cx| {
                                // 제목 클릭 — 크기 순환(접기→작게→크게)
                                this.controller.cycle_log_panel_size();
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(MUTED)
                            .child(format!("{}건", snap.recent_log.len())),
                    )
                    .child(div().flex_1())
                    .child(log_size_button(LogPanelSize::Compact, size, cx))
                    .child(log_size_button(LogPanelSize::Expanded, size, cx))
                    .child(log_size_button(LogPanelSize::Collapsed, size, cx))
                    .child(
                        div()
                            .text_size(px(9.))
                            .text_color(DIM)
                            .child("decision_events · 읽기전용 조회"),
                    ),
            );
        if rows == 0 {
            // 접힘 — 헤더 바만 남는다(높이 34px = 패딩+헤더).
        } else if snap.recent_log.is_empty() {
            panel = panel.child(
                div()
                    .h(px(16.))
                    .text_size(px(10.))
                    .text_color(MUTED)
                    .truncate()
                    .child("아직 기록된 결정 이벤트가 없습니다"),
            );
        } else {
            // 펼침 — 높이 예산(20px/줄)만큼의 줄을 내부 스크롤 창에 올린다.
            // 패널 높이가 고정이라 창 밖으로 밀어내지 않고 스크롤로 흡수한다.
            let mut body = div()
                .id("session-log-body")
                .flex()
                .flex_col()
                .gap_1()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll();
            for e in snap.recent_log.iter().take(rows) {
                body = body.child(log_line(e));
            }
            panel = panel.child(body);
        }
        panel
    }

    // -----------------------------------------------------------------------
    // 5. 푸터 상태 바 — DB·폴링·토스트 | TUI 스타일 상태 배지 4종
    // -----------------------------------------------------------------------

    fn render_footer(&self, snap: &DashboardSnapshot) -> gpui::Div {
        let db_color = if snap.db_status == "연결됨" || self.controller.demo_mode {
            GATE_GREEN
        } else {
            LEAD_ORANGE
        };
        let verifier_state = if verifier_active(snap) { "검증중" } else { "대기" };
        let forks = match &snap.timing_summary {
            Some(t) if !t.intervals_unavailable && t.forks_total > 0 => t.forks_total,
            _ => 0,
        };
        let left = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .min_w_0()
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(db_color)
                    .truncate()
                    .child(format!("DB {} · {}", snap.db_status, self.db_path_label)),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(MUTED)
                    .child(format!(
                        "{} · {}",
                        self.poll_interval_label,
                        if self.controller.paused { "일시정지됨" } else { "폴링" }
                    )),
            )
            .when(
                self.controller.last_action_message.is_some(),
                |el| {
                    el.child(
                        div()
                            .text_size(px(10.))
                            .text_color(LEAD_ORANGE)
                            .truncate()
                            .child(self.controller.last_action_message.clone().unwrap_or_default()),
                    )
                },
            );
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(footer_badge(format!("워커 {}", snap.workers.len()), WORKER_BLUE))
            .child(footer_badge(
                format!("Verifier {}", verifier_state),
                VERIFIER_NEON,
            ))
            .child(footer_badge(
                format!("자문 {}", live_count(snap, "advisor")),
                ADVISOR_PURPLE,
            ))
            .child(footer_badge(format!("{} 분기", forks), GATE_GREEN));
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .h_8()
            .px_4()
            .bg(BG_CHROME)
            .border_t_1()
            .border_color(BORDER_HEADER)
            .child(left)
            .child(right)
    }
}

impl Render for DashboardView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snap = self.controller.filtered();
        // 뷰포트 폭 — 반응형 스택킹(960px)·열 상한(820px) 판정 기준
        let vw = window.viewport_size().width;
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(BG)
            .text_color(TEXT)
            // 스플리터 드래그 추적 — 루트가 창 전체를 커버해 드래그 중 마우스가
            // 패널 밖으로 나가도 축 이동량을 놓치지 않는다(유휴 시 0비용 분기).
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                if let Some((target, start_pos, start_val)) = this.active_drag {
                    let current = match target {
                        DragTarget::LogHeight => f32::from(ev.position.y),
                        DragTarget::AdvisorWidth | DragTarget::VerifierWidth => {
                            f32::from(ev.position.x)
                        }
                    };
                    this.controller.apply_panel_drag(target, start_val, current - start_pos);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    if this.active_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| {
                    if this.active_drag.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .child(self.render_header(&snap, cx))
            .child(self.render_legend_strip(&snap, cx))
            .child(self.render_topology_canvas(&snap, cx, vw))
            .child(self.render_log_splitter(cx))
            .child(self.render_session_log(&snap, cx))
            .child(self.render_footer(&snap))
    }
}

// ---------------------------------------------------------------------------
// 공통 조립 헬퍼 — 스냅숏 파생 수치·와이어·카드 조각
// ---------------------------------------------------------------------------

/// live 워커 중 해당 슬롯 세션 수.
fn live_count(snap: &DashboardSnapshot, slot: &str) -> usize {
    snap.workers.iter().filter(|w| w.slot.as_str() == slot).count()
}

/// 워커의 부모(스폰 리드) 주소 조회 — 스냅숏 parent_map에서(미상 None).
fn parent_of<'a>(snap: &'a DashboardSnapshot, w: &LiveWorkerInfo) -> Option<&'a str> {
    snap.parent_map.get(w.address.as_str()).map(String::as_str)
}

/// 역할 박스 세션 1행 — 고정 높이(20px) 단일 줄: 스피너(고정폭) + 주소 pill +
/// [레포] + (선택) 모델 + 경과 mm:ss + (선택) 1줄 활동(truncate). 동일 레포 다중
/// 세션도 고유 터미널 주소(t155/t182/t165…)로 명확히 구분된다. 모든 슬롯이 고정
/// 폭·truncate라 어떤 폭에서도 줄바꿈·높이 점프가 없다(무지터).
fn role_session_row(
    w: &LiveWorkerInfo,
    idx: usize,
    color: Rgba,
    elapsed_label: Option<String>,
    anim_secs: f32,
    show_model: bool,
    show_activity: bool,
) -> gpui::Div {
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .min_w_0()
        .h(px(20.))
        .flex_none()
        // 스피너 — 고정폭 박스(활성/비활성 전환에도 폭 흔들림 없음)
        .child(
            div()
                .w(px(14.))
                .flex_shrink_0()
                .text_size(px(10.))
                .text_color(color)
                .child(spinner_char(anim_secs, idx).to_string()),
        )
        // 주소 pill — 세션 식별자(동일 레포 다중 세션 구분의 핵심)
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .px_1()
                .h(px(16.))
                .min_w(px(30.))
                .max_w(px(60.))
                .flex_none()
                .rounded_sm()
                .bg(BG)
                .border_1()
                .border_color(color)
                .text_size(px(9.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .truncate()
                .child(w.address.clone()),
        )
        // [레포 태그] — 고정 폭 truncate
        .child(
            div()
                .w(px(76.))
                .flex_none()
                .text_size(px(9.))
                .text_color(MUTED)
                .truncate()
                .child(format!("[{}]", w.repo_tag)),
        );
    if show_model {
        row = row.child(
            div()
                .w(px(80.))
                .flex_none()
                .text_size(px(9.))
                .text_color(TEXT_SOFT)
                .truncate()
                .child(w.short_model.clone()),
        );
    }
    row = row.child(
        div()
            .w(px(38.))
            .flex_none()
            .text_size(px(9.))
            .text_color(DIM)
            .truncate()
            .child(elapsed_label.unwrap_or_else(|| "--:--".to_string())),
    );
    if show_activity {
        row = row.child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(10.))
                .text_color(TEXT_SOFT)
                .truncate()
                .child(redact_secrets(&w.activity)),
        );
    }
    row
}

/// 워커 실행 여부 — status가 'done'이 아니면 실행 중(카드 표기·모션 공통 기준).
fn worker_running(w: &LiveWorkerInfo) -> bool {
    w.status.as_deref() != Some("done")
}

/// Lead 실행 여부 — lead 슬롯에 실행 중(done 아님) 세션 존재 (DEMO 스냅숏은 항상 true).
fn lead_active(snap: &DashboardSnapshot) -> bool {
    snap.workers
        .iter()
        .any(|w| w.slot.as_str() == "lead" && worker_running(w))
}

/// Verifier 검증 진행 여부 — verifier 슬롯에 실행 중 세션 존재.
fn verifier_active(snap: &DashboardSnapshot) -> bool {
    snap.workers
        .iter()
        .any(|w| w.slot.as_str() == "verifier" && worker_running(w))
}

/// 로스터 슬롯의 표시 모델 — live 배지 우선, 없으면 roster.json 대기 배지.
fn role_model_label(snap: &DashboardSnapshot, slot: &str) -> String {
    snap.roles
        .iter()
        .find(|r| r.slot == slot)
        .map(|r| r.live_model.clone().unwrap_or_else(|| r.standby_model.clone()))
        .unwrap_or_else(|| "—".to_string())
}

/// 헤더 카운터 1개 — "LEAD [1]" 형태의 역할색 카운터.
fn counter_label(label: &str, count: usize, color: Rgba) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .child(
            div()
                .text_size(px(10.))
                .text_color(MUTED)
                .child(label.to_string()),
        )
        .child(
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::BOLD)
                .text_color(color)
                .child(format!("[{}]", count)),
        )
}

/// 범례 항목 1개 — 역할색 사각형 + 라벨.
fn legend_item(color: Rgba, label: String) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .min_w_0()
        .child(div().size_2().rounded_sm().bg(color))
        .child(
            div()
                .text_size(px(10.))
                .text_color(TEXT_SOFT)
                .truncate()
                .child(label),
        )
}

fn pill(text: impl Into<SharedString>, fg: Rgba, bg: Rgba) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .px_2()
        .h_5()
        .rounded_md()
        .bg(bg)
        .border_1()
        .border_color(fg)
        .text_size(px(10.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(fg)
        .child(text.into())
}

/// 스피너 포함 상태 pill — 스피너 문자를 고정폭(12px) 박스에 담아 비례폭 글꼴의
/// 글자별 폭 차가 pill 전체 폭을 흔들지 않게 한다(수평 무지터).
fn spinner_pill(frame: char, text: String, fg: Rgba, bg: Rgba) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .h_5()
        .rounded_md()
        .bg(bg)
        .border_1()
        .border_color(fg)
        .text_size(px(10.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(fg)
        .child(div().w(px(12.)).flex_shrink_0().child(frame.to_string()))
        .child(div().truncate().child(text))
}

/// 스피너 포함 상태 텍스트 1행 — 고정폭(14px) 스피너 박스 + truncate 텍스트.
/// 워커 카드 하단 상태 줄('⠋ 실행 중 · 01:24')용(수평·수직 무지터).
fn spinner_status(frame: char, text: String, color: Rgba) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .min_w_0()
        .child(
            div()
                .w(px(14.))
                .flex_shrink_0()
                .text_size(px(10.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(color)
                .child(frame.to_string()),
        )
        .child(
            div()
                .text_size(px(10.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(color)
                .truncate()
                .child(text),
        )
}

/// 세션 기록 크기 토글 버튼 1개 — 현재 모드는 밝은 배경·테두리로 강조.
fn log_size_button(
    target: LogPanelSize,
    current: LogPanelSize,
    cx: &Context<DashboardView>,
) -> gpui::Stateful<gpui::Div> {
    let active = target == current;
    div()
        .id(SharedString::from(format!("log-size-{}", target.label())))
        .flex()
        .items_center()
        .px_2()
        .h(px(16.))
        .rounded_sm()
        .bg(if active { BG_CHROME } else { BG })
        .border_1()
        .border_color(if active { TEXT_SOFT } else { BORDER_LOG })
        .hover(|s| s.border_color(TEXT_SOFT))
        .text_size(px(9.))
        .font_weight(if active { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
        .text_color(if active { TEXT } else { MUTED })
        .cursor_pointer()
        .child(target.label().to_string())
        .on_click(cx.listener(move |this, _, _, cx| {
            this.controller.set_log_panel_size(target);
            cx.notify();
        }))
}

fn action_button(
    label: &str,
    id: &'static str,
    color: Rgba,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .px_3()
        .h_8()
        .rounded_md()
        .border_1()
        .border_color(BORDER_HEADER)
        .bg(BG_ELEVATED)
        .hover(|s| s.border_color(color))
        .text_size(px(12.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(color)
        .cursor_pointer()
        .child(label.to_string())
        .on_click(on_click)
}

fn repo_tab(
    id: impl Into<gpui::ElementId>,
    label: String,
    selected: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .px_2()
        .h(px(20.))
        .rounded_md()
        .bg(if selected { BG_ELEVATED } else { BG })
        .border_1()
        .border_color(if selected { GATE_GREEN } else { BORDER_HEADER })
        .hover(|s| s.border_color(GATE_GREEN))
        .text_size(px(11.))
        .font_weight(if selected { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
        .text_color(if selected { TEXT } else { MUTED })
        .cursor_pointer()
        .child(label)
        .on_click(on_click)
}

/// Advisor 자문 게이트 1행 — 활성(●/━◈▶ 보라) ↔ 대기(◇/┄┄▷ 뮤트).
/// 모션 중 활성 게이트 배지는 사인 호흡(0.45..=1.0)으로 매끄럽게 숨쉰다(대기는 정지).
fn advisor_gate(active: bool, label: &str, badge: &str, anim_secs: f32, motion: bool) -> gpui::Div {
    let (prefix, color, badge_color) = if active {
        ("●", ADVISOR_PURPLE, ADVISOR_PURPLE)
    } else {
        ("◇", MUTED, BORDER_ADVISORY_IDLE)
    };
    let badge_opacity = if active && motion {
        smooth_pulse_opacity(anim_secs, 0.9, 0.25, 0.45, 1.0)
    } else {
        1.0
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .min_h(px(16.))
        .child(
            div()
                .text_size(px(11.))
                .font_weight(if active { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
                .text_color(color)
                .truncate()
                .child(format!("{} {}", prefix, label)),
        )
        .child(div().flex_1())
        .child(
            div()
                .text_size(px(10.))
                .text_color(badge_color)
                .opacity(badge_opacity)
                .child(badge.to_string()),
        )
}

/// Lead ↔ Verifier 수평 와이어 — 고정 h(14px) 트랙 위를 네온 점이 삼각파로
/// 좌↔우 매끄럽게 왕복 글라이드한다(60fps). 점은 `.absolute().left(relative(..))`
/// 오프셋으로만 움직이고 프레임마다 자식을 늬거나 빼지 않으니 레이아웃 점프가
/// 없다. 대기: 중앙 점 정지(동일 지오메트리).
fn horizontal_wire(neon: bool, anim_secs: f32) -> gpui::Div {
    let color = if neon { VERIFIER_NEON } else { BORDER_WIRE };
    let mut wire = div()
        .flex()
        .flex_row()
        .items_center()
        .min_w(px(36.))
        .flex_none();
    if neon {
        let t = continuous_phase(anim_secs, 0.7, 0.0);
        let tri = if t < 0.5 { t * 2.0 } else { 2.0 - t * 2.0 }; // 0→1→0 왕복
        let pos = tri * 0.86;
        wire = wire.child(
            div()
                .relative()
                .flex_1()
                .h(px(14.))
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top(px(6.))
                        .left(px(0.))
                        .w_full()
                        .h(px(2.))
                        .bg(color),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(3.5))
                        .left(relative(pos))
                        .size(px(7.))
                        .rounded_full()
                        .bg(VERIFIER_NEON),
                ),
        );
    } else {
        wire = wire.child(
            div()
                .flex()
                .flex_1()
                .items_center()
                .h(px(14.))
                .child(div().flex_1().h(px(2.)).bg(color))
                .child(div().size(px(7.)).rounded_full().bg(color).flex_none())
                .child(div().flex_1().h(px(2.)).bg(color)),
        );
    }
    wire.child(
        div()
            .text_size(px(10.))
            .text_color(color)
            .flex_none()
            .child("▶"),
    )
}

/// Lead → Decision Gate 수직 와이어 — 고정 h(18px) 트랙을 주황 점이 위→아래로
/// 매끄럽게 미끄러진다(absolute 오프셋 — 레이아웃 점프 없음). 대기: 중앙 정지점.
fn vertical_wire(glowing: bool, anim_secs: f32) -> gpui::Div {
    let mut wire = div().flex().flex_col().items_center();
    let track = || {
        div()
            .relative()
            .flex_none()
            .w(px(7.))
            .h(px(18.))
            .child(
                div()
                    .absolute()
                    .left(px(2.5))
                    .top(px(0.))
                    .w(px(2.))
                    .h(px(18.))
                    .bg(BORDER_WIRE),
            )
    };
    if glowing {
        let t = continuous_phase(anim_secs, 0.85, 0.0);
        wire = wire.child(
            track().child(
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(t * 11.0))
                    .size(px(7.))
                    .rounded_full()
                    .bg(LEAD_ORANGE),
            ),
        );
    } else {
        wire = wire.child(
            track().child(
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(5.5))
                    .size(px(7.))
                    .rounded_full()
                    .bg(BORDER_WIRE),
            ),
        );
    }
    wire.child(
        div()
            .text_size(px(10.))
            .text_color(MUTED)
            .h(px(13.))
            .child("▼"),
    )
}

// ---------------------------------------------------------------------------
// 동적 N스템 커넥터 — 보이는 워커 열 수 n과 슬롯별 활성 플래그로 스템을 그린다
// (고정 3스템 매달림 방지: n=1 중앙 단일 스템, n≥2 첫/끝 스템 중심을 잇는 레일).
// 셀 지오메트리(flex_1 무마진)가 카드 그리드(flex_1·mx_1)와 정확히 같아
// 대칭 마진 때문에 카드 콘텐츠 중심 = 스템 셀 중심이 수학적으로 일치한다.
// ---------------------------------------------------------------------------

/// Decision Gate → Workers 팬아웃 커넥터 — 위임 라벨(+초과분 배지) + N스템 레일.
fn branching_connector(n: usize, active: &[bool], anim_secs: f32, total: usize) -> gpui::Div {
    let mut header = div()
        .flex()
        .flex_row()
        .items_center()
        .min_h(px(14.))
        .gap_1()
        .child(
            div()
                .text_size(px(11.))
                .text_color(TEXT_SOFT)
                .child("하위 에이전트에 위임 · "),
        )
        .child(
            div()
                .text_size(px(11.))
                .font_weight(FontWeight::BOLD)
                .text_color(WORKER_BLUE)
                .child("Workers"),
        );
    // 2행 랩에 들어간 초과 워커 고지 — 어떤 워커도 숨겨지지 않는다.
    let extra = total.saturating_sub(n);
    if extra > 0 {
        header = header.child(
            div()
                .text_size(px(10.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(WORKER_BLUE)
                .child(format!("+{}개 추가", extra)),
        );
    }
    stem_assembly(n, active, anim_secs, false, Some(header))
}

/// Workers → 다시 Lead 팬인 커넥터 — 분기와 동일한 n·플래그의 수렴 미러.
fn converging_connector(n: usize, active: &[bool], anim_secs: f32) -> gpui::Div {
    stem_assembly(n, active, anim_secs, true, None)
}

/// N스템 어셈블리 — converging=false: 레일→점→스템(팬아웃), true: 스템→점→레일→▼.
/// n==1은 레일 없는 중앙 단일 스템(브리프 계약 — 가로 레일 금지).
fn stem_assembly(
    n: usize,
    active: &[bool],
    anim_secs: f32,
    converging: bool,
    header: Option<gpui::Div>,
) -> gpui::Div {
    let mut out = div().flex().flex_col().items_center().w_full();
    if let Some(header) = header {
        out = out.child(header);
    }
    if n <= 1 {
        out = out.child(single_stem(
            active.first().copied().unwrap_or(false),
            anim_secs,
            0,
            converging,
        ));
    } else {
        let any_active = active.iter().any(|&a| a);
        // 레일 — 첫/끝 스템 중심을 잇는 선: 셀 중심 간 거리 = (n-1)/n 폭.
        // 활성 스템이 하나라도 있으면 레일도 파란빛으로 살짝 깨어난다.
        let rail = if any_active {
            div()
                .w(relative((n - 1) as f32 / n as f32))
                .h(px(1.))
                .bg(WORKER_BLUE)
                .opacity(0.55)
        } else {
            div()
                .w(relative((n - 1) as f32 / n as f32))
                .h(px(1.))
                .bg(BORDER_WIRE)
        };
        let dots = stem_dots_row(n, active, anim_secs);
        let stems = stem_row(n, active, anim_secs, converging);
        if converging {
            out = out
                .child(stems)
                .child(dots)
                .child(rail)
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(MUTED)
                        .h(px(13.))
                        .child("▼"),
                );
        } else {
            out = out.child(rail).child(dots).child(stems);
        }
    }
    out
}

/// 스템 행 — n개 flex_1 셀 중심에 고정 h(12px) 스템. 활성 스템은 WORKER_BLUE로
/// 빛나며 5px 점이 스템 전체를 위→아래(수렴은 반대 방향)로 매끄럽게 글라이드한다.
/// 점은 absolute 오프셋 전용이라 프레임 사이 높이 변화가 없다.
fn stem_row(n: usize, active: &[bool], anim_secs: f32, converging: bool) -> gpui::Div {
    const STEM_H: f32 = 12.0;
    let mut row = div().flex().flex_row().w_full();
    for i in 0..n {
        let is_active = active.get(i).copied().unwrap_or(false);
        let color = if is_active { WORKER_BLUE } else { BORDER_WIRE };
        let cell = if is_active {
            let t = continuous_phase(anim_secs, 0.9, i as f32 * 0.31);
            let travel = if converging { 1.0 - t } else { t };
            div()
                .relative()
                .flex_none()
                .w(px(7.))
                .h(px(STEM_H))
                .child(
                    div()
                        .absolute()
                        .left(px(2.5))
                        .top(px(0.))
                        .w(px(2.))
                        .h(px(STEM_H))
                        .bg(color),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(1.))
                        .top(px(travel * (STEM_H - 5.0)))
                        .size(px(5.))
                        .rounded_full()
                        .bg(WORKER_BLUE),
                )
        } else {
            div()
                .flex()
                .flex_none()
                .w(px(7.))
                .h(px(STEM_H))
                .flex_row()
                .justify_center()
                .child(div().w(px(2.)).h(px(STEM_H)).bg(color))
        };
        row = row.child(div().flex_1().flex().flex_row().justify_center().child(cell));
    }
    row
}

/// 스템 접점 행 — 활성 스템은 고정 12px 박스 안에서 사인 호흡(크기·불투명도)하는
/// 발광 점(워커 idx별 위상 차등 — 모든 실행 워커에 뚜렷한 라이브 플로우),
/// 대기 스템은 작은 뮤트 점(펄스 없음 — 침묵). 박스 높이가 고정이라 행 점프 없음.
fn stem_dots_row(n: usize, active: &[bool], anim_secs: f32) -> gpui::Div {
    const DOT_BOX: f32 = 12.0;
    let mut row = div().flex().flex_row().items_center().w_full();
    for i in 0..n {
        let dot = if active.get(i).copied().unwrap_or(false) {
            let op = smooth_pulse_opacity(anim_secs, 0.8, i as f32 * 0.37, 0.35, 1.0);
            let grow = smooth_pulse_opacity(anim_secs, 0.8, i as f32 * 0.37, 0.0, 1.0);
            let s = 4.0 + 4.0 * grow;
            let inset = (DOT_BOX - s) * 0.5;
            div()
                .relative()
                .flex_none()
                .size(px(DOT_BOX))
                .child(
                    div()
                        .absolute()
                        .top(px(inset))
                        .left(px(inset))
                        .size(px(s))
                        .rounded_full()
                        .bg(WORKER_BLUE)
                        .opacity(op),
                )
        } else {
            div()
                .flex()
                .flex_none()
                .size(px(DOT_BOX))
                .flex_row()
                .items_center()
                .justify_center()
                .child(div().size(px(4.)).rounded_full().bg(BORDER_WIRE))
        };
        row = row.child(div().flex_1().flex().flex_row().justify_center().child(dot));
    }
    row
}

/// 단일 스템(n==1) — 레일 없이 카드 중앙으로 곧장(수렴 모드는 점이 아래).
/// 활성이면 호흡 접점 점 + 스템 글라이드 점(팬아웃/팬인 방향 반영).
fn single_stem(active: bool, anim_secs: f32, idx: usize, converging: bool) -> gpui::Div {
    let color = if active { WORKER_BLUE } else { BORDER_WIRE };
    let junction = if active {
        let op = smooth_pulse_opacity(anim_secs, 0.8, idx as f32 * 0.37, 0.35, 1.0);
        let grow = smooth_pulse_opacity(anim_secs, 0.8, idx as f32 * 0.37, 0.0, 1.0);
        let s = 4.0 + 4.0 * grow;
        let inset = (12.0 - s) * 0.5;
        div()
            .relative()
            .flex_none()
            .size(px(12.))
            .child(
                div()
                    .absolute()
                    .top(px(inset))
                    .left(px(inset))
                    .size(px(s))
                    .rounded_full()
                    .bg(WORKER_BLUE)
                    .opacity(op),
            )
    } else {
        div()
            .flex()
            .flex_none()
            .size(px(12.))
            .flex_row()
            .items_center()
            .justify_center()
            .child(div().size(px(4.)).rounded_full().bg(BORDER_WIRE))
    };
    let stem = if active {
        let t = continuous_phase(anim_secs, 0.9, idx as f32 * 0.31);
        let travel = if converging { 1.0 - t } else { t };
        div()
            .relative()
            .flex_none()
            .w(px(7.))
            .h(px(12.))
            .child(
                div()
                    .absolute()
                    .left(px(2.5))
                    .top(px(0.))
                    .w(px(2.))
                    .h(px(12.))
                    .bg(color),
            )
            .child(
                div()
                    .absolute()
                    .left(px(1.))
                    .top(px(travel * 7.0))
                    .size(px(5.))
                    .rounded_full()
                    .bg(WORKER_BLUE),
            )
    } else {
        div()
            .flex()
            .flex_none()
            .w(px(7.))
            .h(px(12.))
            .flex_row()
            .justify_center()
            .child(div().w(px(2.)).h(px(12.)).bg(color))
    };
    let cell = div().flex().flex_col().items_center();
    if converging {
        cell.child(stem).child(junction)
    } else {
        cell.child(junction).child(stem)
    }
}

/// 라우트 1행 — 단일 수평 행: 라벨(110px 고정) | flex_1 확률 바(h 6px) | 우측
/// 통계(155px · text_right). 2행 스택이 아니라 한 줄로 컴팩트하게(TUI
/// renderDecisionPanel 대응 — Decision Gate 계층의 수직 예산 절약).
fn route_row(r: &RouteMetric) -> gpui::Div {
    let forced = r.forced_split;
    let split = forced || r.decision == DecisionLabel::Split;
    let (fill, track) = if forced {
        (ROSE_DANGER, BG_ORANGE_TRACK)
    } else if split {
        (LEAD_ORANGE, BG_ORANGE_TRACK)
    } else {
        (GATE_GREEN, BG_GREEN_TRACK)
    };
    let basis_label = if r.basis == "measured" {
        format!("실측 n={}", r.samples)
    } else {
        "사전".to_string()
    };
    let badge = if forced {
        "SPLIT!".to_string()
    } else {
        r.decision.as_str().to_lowercase()
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .min_w_0()
        .min_h(px(14.))
        .flex_none()
        // 라벨 — 고정 폭 110px(줄바꿈 없는 1행, truncate)
        .child(
            div()
                .w(px(110.))
                .flex_none()
                .text_size(px(10.))
                .text_color(if split { fill } else { TEXT })
                .truncate()
                .child(r.name.clone()),
        )
        // 확률 바 — 나머지 폭 전부(h 6px 고정)
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .h(px(6.))
                .rounded_full()
                .bg(track)
                .overflow_hidden()
                .child(
                    div()
                        .h_full()
                        .w(relative(r.p.clamp(0.0, 1.0) as f32))
                        .rounded_full()
                        .bg(fill),
                ),
        )
        // 우측 통계 — 고정 폭 155px 우측 정렬 1행
        .child(
            div()
                .w(px(155.))
                .flex_none()
                .text_size(px(10.))
                .font_weight(if forced { FontWeight::BOLD } else { FontWeight::MEDIUM })
                .text_color(fill)
                .text_right()
                .truncate()
                .child(format!("p={:.2} {} · {}", r.p, badge, basis_label)),
        )
}

/// 표시 정렬 랭크 — 강제분기(0) → split(1) → sharp(2).
fn route_rank(r: &RouteMetric) -> u8 {
    if r.forced_split {
        0
    } else if r.decision == DecisionLabel::Split {
        1
    } else {
        2
    }
}

/// 라이브 워커 0일 때의 청사진 슬롯 카드 — 실제 roster 모델 라벨 + 정직한 대기 상태.
/// 가짜 진행바·애니메이션 일절 없음(3열 토폴로지 형태만 유지).
fn blueprint_slot_card(slot: &str, model: &str) -> gpui::Div {
    let description = match slot {
        "explore" => "파일·구조 탐색 대기",
        "coder" => "코드 수정·빌드 대기",
        _ => "계획·결과 교차 검증 대기",
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .min_h(px(96.))
        .rounded_lg()
        .bg(BG)
        .border_1()
        .border_color(WORKER_BLUE)
        .flex_1()
        .min_w_0()
        .mx_1()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .min_w_0()
                .min_h(px(18.))
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(TEXT_SOFT)
                        .truncate()
                        .child(slot.to_string()),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(MUTED)
                        .child("○ 대기 중"),
                ),
        )
        .child(
            div()
                .h(px(14.))
                .text_size(px(10.))
                .text_color(MUTED)
                .truncate()
                .child(model.to_string()),
        )
        // 빈 트랙 — 채움 0 (대기 상태 위장 금지), 높이는 실행 카드와 동일 5px
        .child(
            div()
                .flex()
                .w_full()
                .h(px(5.))
                .flex_none()
                .rounded_full()
                .bg(BG_BLUE_TRACK),
        )
        .child(
            div()
                .h(px(16.))
                .text_size(px(10.))
                .text_color(DIM)
                .truncate()
                .child(description),
        )
}

/// 세션 기록 1줄 — HH:MM:SS (dim) + 역할색 태그 + 1줄 요약.
fn log_line(e: &RecentLogEntry) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .min_w_0()
        .h(px(16.))
        .flex_none()
        .child(
            div()
                .w(px(56.))
                .flex_none()
                .text_size(px(10.))
                .text_color(DIM)
                .truncate()
                .child(e.time_hhmmss.clone()),
        )
        .child(
            div()
                .w(px(84.))
                .flex_none()
                .text_size(px(10.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(log_role_color(&e.source_role))
                .truncate()
                .child(e.source_role.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(10.))
                .text_color(TEXT_SOFT)
                .truncate()
                .child(e.message.clone()),
        )
}

/// 세션 기록 역할 태그 색 — TUI 5역할 아이덴티티 색.
fn log_role_color(role: &str) -> Rgba {
    match role {
        "Decision Gate" => GATE_GREEN,
        "Worker" => WORKER_BLUE,
        "Verifier" => VERIFIER_NEON,
        "Advisor" => ADVISOR_PURPLE,
        "Lead" => LEAD_ORANGE,
        _ => MUTED,
    }
}

/// 푸터 상태 배지 — `[ 워커 N ]` 형태.
fn footer_badge(text: String, color: Rgba) -> gpui::Div {
    div()
        .text_size(px(10.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(format!("[ {} ]", text))
}

/// 실행 중 워커 활동바의 안정 채움 비율 — 주소 FNV-1a 해시(재렌더링·폴링에도 불변).
/// 완료율이 아니라 '활동 존재' 표시등이며 대기 카드에는 적용하지 않는다.
fn stable_activity_frac(key: &str) -> f32 {
    let mut h: u64 = 1469598103934665603;
    for b in key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    let v = (h % 61) as f32 / 61.0;
    0.15 + v * 0.75
}

/// 미정 진행 슬라이딩 트랙 — 알약(폭 28%)이 continuous_phase 삼각파로 0..72%를
/// 좌↔우 매끄럽게 왕복 글라이드한다(60fps · 위상은 워커 idx로 분산).
/// 완료율이 아니라 '진행 중' 표시등이며, anim_secs가 멈추는 유휴에는 알약도
/// 정지한다(가짜 진행 금지). 높이 5px 고정 — 카드 높이 점프 없음.
fn sliding_pill_track(track_bg: Rgba, pill: Rgba, anim_secs: f32, idx: usize) -> gpui::Div {
    let t = continuous_phase(anim_secs, 0.75, idx as f32 * 0.29);
    let tri = if t < 0.5 { t * 2.0 } else { 2.0 - t * 2.0 }; // 0→1→0 왕복
    let pos = tri * 0.72;
    div()
        .flex()
        .flex_row()
        .w_full()
        .h(px(5.))
        .flex_none()
        .rounded_full()
        .bg(track_bg)
        .overflow_hidden()
        .child(div().w(relative(pos)).h_full().flex_none())
        .child(
            div()
                .w(relative(0.28))
                .h_full()
                .flex_none()
                .rounded_full()
                .bg(pill),
        )
}

/// ms → 사람이 읽는 시간 ("1h 23m" · "4m 05s" · "42s").
fn fmt_duration(ms: f64) -> String {
    let secs = ms.max(0.0) as u64;
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}h {:02}m", h, m)
    } else if m > 0 {
        format!("{}m {:02}s", m, s)
    } else {
        format!("{}s", s)
    }
}
