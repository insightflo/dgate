//! dgate-gui GPUI 뷰 — Metal GPU 렌더링 다크 대시보드 (Rust v2 Wave 2B).
//!
//! 목적: 터미널 TUI의 한계를 넘는 네이티브 관제 화면 — 헤더(라이브 필·새로고침·
//! 일시정지·제어 토글) · 레포 탭 · 5역할 로스터 스트립 · 실행 중 워커 카드(2단계
//! 중단 확인) · Decision Gate 확률 바 · 비용/시간/shadow 요약 · 정직한 대기 상태.
//!
//! 외부 연결:
//! - `state::DashboardController` — 스냅숏 적용·탭 필터·확인 상태 등 모든 상태 전이.
//! - `state::PollerHandle` — 새로고침 웨이크·일시정지 플래그 전달(뷰 스레드 I/O 금지).
//! - UI 갱신은 250ms 타이머가 백그라운드 폴러의 version 카운터만 관찰해 `cx.notify()`.
//!
//! 주의:
//! - 이 파일의 데이터 조립은 전부 `state.rs` 순수 로직 위임 — 뷰는 렌더링만 담당.
//! - `hc kill`은 리스너에서 직접 실행하지 않고 백그라운드 실행기로 보내 UI가 멈추지
//!   않게 한다. 결과는 `note_kill_result`로 1줄 회신.
//! - 대기 상태(워커 0)에는 가짜 애니메이션을 두지 않는다 — 침착한 뮤트 배지만.

use std::sync::atomic::Ordering;
use std::time::Duration;

use gpui::{
    div, prelude::*, px, relative, Context, FontWeight, Render, Rgba, SharedString, Window,
};

use crate::state::{
    kill_worker_safe, DashboardController, DashboardSnapshot, DefaultKillExecutor, DecisionLabel,
    GuiCliOptions, PollerHandle, RouteMetric, UI_REFRESH_TICK_MS,
};

// ---------------------------------------------------------------------------
// 팔레트 — brief 지정 딥슬레이트 다크
// ---------------------------------------------------------------------------

/// gpui::rgb 는 const fn이 아니어서 팔레트 상수용으로 동등한 const 변환기를 둔다.
const fn hex_rgba(hex: u32) -> Rgba {
    Rgba {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

const BG: Rgba = hex_rgba(0x0f1117); // 딥 슬레이트 배경
const SURFACE: Rgba = hex_rgba(0x171b26); // 헤더·푸터 표면
const CARD: Rgba = hex_rgba(0x1e2433); // 카드
const BORDER: Rgba = hex_rgba(0x2b3347); // 경계선
const TEXT: Rgba = hex_rgba(0xe7ecf5); // 본문 밝은 텍스트
const MUTED: Rgba = hex_rgba(0x8b95ab); // 뮤트 텍스트
const EMERALD: Rgba = hex_rgba(0x34d399); // sharp / 활성
const AMBER: Rgba = hex_rgba(0xf59e0b); // split / 주의
const ROSE: Rgba = hex_rgba(0xf87171); // 강제분기 / 중단
const SKY: Rgba = hex_rgba(0x38bdf8); // 정보
const ROSE_BG_HOVER: Rgba = hex_rgba(0x3a2230); // 중단 버튼 호버 배경

/// 뷰 엔티티 — GPUI 창 1개가 1개 소유.
pub struct DashboardView {
    pub controller: DashboardController,
    pub poller: PollerHandle,
    pub db_path_label: String,
    pub hc_bin: String,
}

impl DashboardView {
    pub fn new(opts: &GuiCliOptions, poller: PollerHandle, cx: &mut Context<Self>) -> Self {
        let controller = DashboardController::new(opts.watch_only);
        let shared = poller.shared.clone();
        // 백그라운드 폴러 관찰 태스크 — version 변화만 감지해 cx.notify (UI 스레드 I/O 0)
        cx.spawn(async move |this, cx| {
            let mut last_version = 0u64;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(UI_REFRESH_TICK_MS))
                    .await;
                if shared.shutdown.load(Ordering::SeqCst) {
                    break;
                }
                let (version, snap) = shared.take();
                if version != last_version {
                    last_version = version;
                    let updated = this
                        .update(cx, |view, cx| {
                            if let Some(snap) = snap {
                                view.controller.apply_snapshot(snap);
                            }
                            cx.notify();
                        })
                        .is_ok();
                    if !updated {
                        break; // 뷰 소멸 — 태스크도 종료
                    }
                }
            }
        })
        .detach();

        let db_path_label = opts
            .db_path
            .clone()
            .unwrap_or_else(|| "~/.local/state/dgate/decision.db".to_string());
        DashboardView {
            controller,
            poller,
            db_path_label,
            hc_bin: "hc".to_string(),
        }
    }

    /// 헤더 바 — 좌: 타이틀·프로필·라이브 필 / 우: 새로고침·일시정지·제어 토글.
    fn render_header(&self, snap: &DashboardSnapshot, cx: &Context<Self>) -> gpui::Div {
        let active = snap.workers.len();
        let (pill_bg, pill_fg, pill_text) = if active > 0 {
            (EMERALD, BG, format!("실행 중 · 워커 {}", active))
        } else {
            (BORDER, MUTED, "대기 중 · 활성 워커 0".to_string())
        };

        let mut right = div().flex().flex_row().gap_2().items_center();
        right = right.child(action_button(
            "새로고침",
            "btn-refresh",
            SKY,
            cx.listener(|this, _, _, cx| {
                this.poller.wake();
                this.controller.last_action_message = Some("새로고침 요청".to_string());
                cx.notify();
            }),
        ));
        let (pause_label, pause_color) = if self.controller.paused {
            ("재개", EMERALD)
        } else {
            ("일시정지", AMBER)
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
                    ("제어 허용됨", ROSE, true)
                } else {
                    ("관측 전용", MUTED, false)
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
                    .border_color(if control_active { control_color } else { BORDER })
                    .bg(if control_active { CARD } else { SURFACE })
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

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h_12()
            .px_4()
            .bg(SURFACE)
            .border_b_1()
            .border_color(BORDER)
            .child(
                div()
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
                    .child(
                        pill(format!("profile={}", snap.profile_name), SKY, SURFACE),
                    )
                    .child(pill(pill_text, pill_fg, pill_bg))
                    .when(!snap.hc_available, |el| {
                        el.child(pill("hc 없음", AMBER, SURFACE))
                    }),
            )
            .child(right)
    }

    /// 레포 탭 스트립 — [전체 (N)] + 동적 레포 탭.
    fn render_repo_tabs(&self, snap: &DashboardSnapshot, cx: &Context<Self>) -> gpui::Div {
        let total = snap.workers.len();
        let mut tabs = div().flex().flex_row().items_center().gap_1().px_4().h_9();
        tabs = tabs.child(repo_tab(
            "tab-global",
            format!("전체 ({})", total),
            self.controller.selected_repo.is_none(),
            cx.listener(|this, _, _, cx| {
                this.controller.select_repo(None);
                cx.notify();
            }),
        ));
        for repo in &snap.detected_repos {
            let count = snap
                .workers
                .iter()
                .filter(|w| &w.repo_tag == repo)
                .count();
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
            .items_center()
            .gap_1()
            .h_10()
            .px_4()
            .bg(SURFACE)
            .border_b_1()
            .border_color(BORDER)
            .child(tabs)
    }

    /// 5역할 로스터 스트립 — live 러닝 닷 + 모델 배지 + readonly/on-demand 태그.
    fn render_roster_strip(&self, snap: &DashboardSnapshot) -> gpui::Div {
        let mut strip = div().flex().flex_row().gap_2();
        for role in &snap.roles {
            let model_badge = role
                .live_model
                .clone()
                .unwrap_or_else(|| role.standby_model.clone());
            let (model_color, state_label) = if role.running {
                (EMERALD, "실행 중")
            } else {
                (MUTED, "대기")
            };
            let mut card = div()
                .flex()
                .flex_1()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_lg()
                .bg(CARD)
                .border_1()
                .border_color(if role.running { EMERALD } else { BORDER });
            card = card
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1p5()
                        .child(
                            div()
                                .size_2()
                                .rounded_full()
                                .bg(if role.running { EMERALD } else { BORDER }),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(TEXT)
                                .child(role.title.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(if role.running { EMERALD } else { MUTED })
                                .child(state_label),
                        ),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(model_color)
                        .truncate()
                        .child(model_badge),
                );
            let mut tags = div().flex().flex_row().gap_1();
            if role.readonly {
                tags = tags.child(mini_tag("readonly", SKY));
            }
            if role.on_demand {
                tags = tags.child(mini_tag("on-demand", AMBER));
            }
            card = card.child(tags);
            strip = strip.child(card);
        }
        // agy 지표 — 5카드 오른쪽 끼움 카드
        strip = strip.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_2()
                .rounded_lg()
                .bg(SURFACE)
                .border_1()
                .border_color(BORDER)
                .justify_center()
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(TEXT)
                        .child(format!("agy: {}", snap.agy_live_count)),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(MUTED)
                        .child("live 세션"),
                ),
        );
        strip
    }

    /// 좌측 — 실행 중 워커 카드 목록(스크롤) + 정직한 빈 상태.
    fn render_workers_column(
        &self,
        snap: &DashboardSnapshot,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(TEXT)
                    .child("실행 중 워커"),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(MUTED)
                    .child(format!("{} · 제어 {}", snap.workers.len(), if self.controller.control_enabled { "허용" } else { "잠금" })),
            );

        let list: gpui::Stateful<gpui::Div> = if snap.workers.is_empty() {
            div()
                .id("workers-scroll")
                .overflow_y_scroll()
                .flex_1()
                .min_h_0()
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .p_6()
                        .rounded_lg()
                        .bg(CARD)
                        .border_1()
                        .border_color(BORDER)
                        .child(
                            div()
                                .size_6()
                                .rounded_full()
                                .border_2()
                                .border_color(BORDER),
                        )
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(MUTED)
                                .text_center()
                                .child("현재 실행 중인 워커가 없습니다."),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(MUTED)
                                .opacity(0.7)
                                .text_center()
                                .child("AI 채팅창에서 `dgate로 진행해줘`로 작업을 시작하세요."),
                        ),
                )
        } else {
            let mut scroll = div()
                .id("workers-scroll")
                .overflow_y_scroll()
                .flex_1()
                .min_h_0()
                .flex_col()
                .gap_2();
            for w in &snap.workers {
                scroll = scroll.child(self.render_worker_card(w, cx));
            }
            scroll
        };

        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_3()
            .rounded_lg()
            .bg(SURFACE)
            .border_1()
            .border_color(BORDER)
            .flex_basis(relative(0.46))
            .min_h_0()
            .child(header)
            .child(list)
    }

    /// 워커 카드 1개 — [repo] 태그 · 이름 · 모델 배지 · 1줄 활동 · 2단계 중단 버튼.
    fn render_worker_card(
        &self,
        w: &dgate_core::live_sync::LiveWorkerInfo,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let confirming = self.controller.confirm_kill_address.as_deref() == Some(w.address.as_str());
        let can_control = self.controller.control_enabled && !self.controller.watch_only;

        let mut actions = div().flex().flex_row().gap_2().items_center();
        if confirming {
            actions = actions
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(ROSE)
                        .child("정말 중단?"),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("kill-confirm-{}", w.address)))
                        .px_2()
                        .h_6()
                        .flex()
                        .items_center()
                        .rounded_md()
                        .bg(ROSE)
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(BG)
                        .cursor_pointer()
                        .child("확인")
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
                        }))
                )
                .child({
                    div()
                        .id(SharedString::from(format!("kill-cancel-{}", w.address)))
                        .px_2()
                        .h_6()
                        .flex()
                        .items_center()
                        .rounded_md()
                        .border_1()
                        .border_color(BORDER)
                        .text_size(px(11.))
                        .text_color(MUTED)
                        .cursor_pointer()
                        .child("취소")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.controller.cancel_kill();
                            cx.notify();
                        }))
                });
        } else if can_control {
            let addr = w.address.clone();
            actions = actions.child(
                div()
                    .id(SharedString::from(format!("kill-ask-{}", w.address)))
                    .px_2()
                    .h_6()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .border_1()
                    .border_color(ROSE)
                    .text_size(px(11.))
                    .text_color(ROSE)
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
            .p_3()
            .rounded_lg()
            .bg(CARD)
            .border_1()
            .border_color(if confirming { ROSE } else { BORDER })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(pill(format!("[{}]", w.repo_tag), SKY, SURFACE))
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
                    .child(
                        div()
                            .flex_1(),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(EMERALD)
                            .truncate()
                            .child(w.short_model.clone()),
                    ),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(MUTED)
                    .truncate()
                    .child(w.activity.clone()),
            )
            .child(actions)
    }

    /// 우측 — Decision Gate 4카드(확률 바) + 비용/시간/shadow 요약.
    fn render_gates_column(&self, snap: &DashboardSnapshot) -> gpui::Div {
        let mut routes = div().flex().flex_col().gap_2();
        for r in &snap.routes {
            routes = routes.child(route_card(r));
        }

        div()
            .flex()
            .flex_col()
            .gap_2()
            .flex_1()
            .min_h_0()
            .p_3()
            .rounded_lg()
            .bg(SURFACE)
            .border_1()
            .border_color(BORDER)
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(TEXT)
                    .child("Decision Gate 통계"),
            )
            .child(routes)
            .child(stats_summary_card(snap))
    }

    /// 푸터 — DB 상태·경로 · 마지막 새로고침 · UX 힌트 1줄.
    fn render_footer(&self, snap: &DashboardSnapshot) -> gpui::Div {
        let db_color = if snap.db_status == "연결됨" { EMERALD } else { AMBER };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .h_8()
            .px_4()
            .bg(SURFACE)
            .border_t_1()
            .border_color(BORDER)
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(db_color)
                    .child(format!("DB {} · {}", snap.db_status, self.db_path_label)),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(MUTED)
                    .child(format!("새로고침 {}", snap.updated_at_label)),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(MUTED)
                    .child("읽기전용 실시간 관측 · 워커 중단은 상단 '제어 허용' 켜고 2단계 확인 후 실행"),
            )
    }
}

impl Render for DashboardView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snap = self.controller.filtered();
        let header = self.render_header(&snap, cx);
        let tabs = self.render_repo_tabs(&snap, cx);
        let roster = self.render_roster_strip(&snap);
        let workers = self.render_workers_column(&snap, cx);
        let gates = self.render_gates_column(&snap);
        let footer = self.render_footer(&snap);
        // 마지막 조작 결과 1줄 — 푸터 바로 위, 뷰를 가리지 않는 얇은 상태줄
        let message_bar = self
            .controller
            .last_action_message
            .clone()
            .map(|msg| {
                div()
                    .flex()
                    .items_center()
                    .px_4()
                    .h_5()
                    .bg(SURFACE)
                    .text_size(px(10.))
                    .text_color(MUTED)
                    .child(msg)
            })
            .unwrap_or_else(|| div().h_0());

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(BG)
            .text_color(TEXT)
            .child(header)
            .child(tabs)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .flex_1()
                    .min_h_0()
                    .child(roster)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_3()
                            .flex_1()
                            .min_h_0()
                            .child(workers)
                            .child(gates),
                    ),
            )
            .child(message_bar)
            .child(footer)
    }
}

// ---------------------------------------------------------------------------
// 공통 조립 헬퍼
// ---------------------------------------------------------------------------

fn pill(text: impl Into<SharedString>, fg: Rgba, bg: Rgba) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .px_2()
        .h_5()
        .rounded_md()
        .bg(bg)
        .text_size(px(10.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(fg)
        .child(text.into())
}

fn mini_tag(text: &str, color: Rgba) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .px_1()
        .h_4()
        .rounded_sm()
        .border_1()
        .border_color(color)
        .text_size(px(9.))
        .text_color(color)
        .child(text.to_string())
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
        .border_color(BORDER)
        .bg(CARD)
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
        .px_3()
        .h_7()
        .rounded_md()
        .bg(if selected { CARD } else { SURFACE })
        .border_1()
        .border_color(if selected { SKY } else { BORDER })
        .hover(|s| s.border_color(SKY))
        .text_size(px(12.))
        .font_weight(if selected { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
        .text_color(if selected { TEXT } else { MUTED })
        .cursor_pointer()
        .child(label)
        .on_click(on_click)
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

/// 게이트 확률 바 색 — SHARP 초록 / SPLIT 주황 / 강제분기 빨강.
fn route_color(r: &RouteMetric) -> Rgba {
    if r.forced_split {
        ROSE
    } else if r.decision == DecisionLabel::Sharp {
        EMERALD
    } else {
        AMBER
    }
}

fn route_card(r: &RouteMetric) -> gpui::Div {
    let color = route_color(r);
    let frac = (r.p.clamp(0.0, 1.0)) as f32;
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .rounded_lg()
        .bg(CARD)
        .border_1()
        .border_color(BORDER)
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(TEXT)
                        .child(r.name.clone()),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(MUTED)
                        .child(format!("n={}", r.samples)),
                )
                .child(
                    div()
                        .px_1()
                        .rounded_sm()
                        .bg(color)
                        .text_size(px(10.))
                        .font_weight(FontWeight::BOLD)
                        .text_color(BG)
                        .child(r.decision.as_str().to_string()),
                ),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(MUTED)
                .child(format!(
                    "p = {:.2} · {}{}",
                    r.p,
                    r.basis,
                    if r.forced_split { " · 강제 split" } else { "" }
                )),
        )
        .child(
            div()
                .flex()
                .w_full()
                .h_1()
                .rounded_full()
                .bg(BORDER)
                .overflow_hidden()
                .child(div().h_full().w(relative(frac)).rounded_full().bg(color)),
        )
}

/// 비용(C-1 정직 합계)·시간·shadow 요약 카드.
fn stats_summary_card(snap: &DashboardSnapshot) -> gpui::Div {
    let cost_line = match &snap.cost_summary {
        Some(c) => {
            let known = format!("known ${:.2}", c.known_usd);
            let total = if c.unknown_events > 0 {
                // 불변식 C-1 — unknown이 있으면 총액을 숨기고 unavailable로 명시
                format!("총액 unavailable (unknown {}건)", c.unknown_events)
            } else {
                format!("총액 ${:.2}", c.total_usd_display.unwrap_or(c.known_usd))
            };
            format!(
                "비용 {} · {} · 토큰 in {} / out {}",
                known,
                total,
                fmt_tokens(c.tokens.input),
                fmt_tokens(c.tokens.output),
            )
        }
        None => "비용 집계 없음 (DB 대기)".to_string(),
    };
    let timing_line = match &snap.timing_summary {
        Some(t) if !t.intervals_unavailable => format!(
            "시간 exec {} · wait {} · wall {}",
            fmt_duration(t.execution_ms),
            fmt_duration(t.wait_ms),
            fmt_duration(t.wall_ms),
        ),
        Some(_) => "시간 구간 데이터 없음".to_string(),
        None => "시간 집계 없음 (DB 대기)".to_string(),
    };
    let shadow_line = match &snap.shadow_summary {
        Some(s) => format!(
            "shadow API 평가 {}건 · disagreed {}{}",
            s.total,
            s.disagreed,
            s.disagreement_rate
                .map(|r| format!(" ({:.0}%)", r * 100.0))
                .unwrap_or_default(),
        ),
        None => "shadow API 집계 없음 (DB 대기)".to_string(),
    };

    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .rounded_lg()
        .bg(CARD)
        .border_1()
        .border_color(BORDER)
        .child(
            div()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(TEXT)
                .child("비용 · 시간 · Shadow API"),
        )
        .child(div().text_size(px(11.)).text_color(MUTED).truncate().child(cost_line))
        .child(div().text_size(px(11.)).text_color(MUTED).truncate().child(timing_line))
        .child(div().text_size(px(11.)).text_color(MUTED).truncate().child(shadow_line))
}

/// 토큰 수 축약 (1.2M · 340K · 900).
fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}K", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}
