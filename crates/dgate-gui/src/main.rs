//! dgate-gui 진입점 — CLI 파싱 후 GPUI Metal 창을 연다 (Rust v2 Wave 2B).
//!
//! 목적: `--help`면 도움말 출력 후 exit 0(창 없음). 그 외에는 1240×780 중앙
//! 네이티브 창 "dgate — Native Control & Monitor (GPUI)" 를 열고 백그라운드
//! 폴러 + 대시보드 뷰를 띄운다. 최소 크기 640×480 — 자유 리사이즈 가능.
//! 1240×780은 13/14/16형 MacBook 디스플레이에서 헤더→푸터 전체 대시보드가
//! 한 화면(세로 스크롤 없음)에 들어오는 컴팩트 기본 크기다.
//!
//! 외부 연결:
//! - `state::parse_gui_cli_options` / `spawn_poller` — 옵션·폴링 스레드.
//! - `view::DashboardView` — GPUI 렌더링 엔티티.
//!
//! 주의:
//! - 앱 종료 시 폴러 조인 핸들로 스레드를 확실히 정리한다(방치 스레드 금지).
//! - 폴러는 창 열기 전에 기동해 첫 스냅숏이 최대한 빨리 도착하게 한다.

mod state;
mod view;

use gpui::{
    size, px, App, AppContext as _, Application, Bounds, TitlebarOptions, WindowBounds,
    WindowOptions,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match state::parse_gui_cli_options(&args) {
        Ok(opts) => opts,
        Err(err) => {
            eprintln!("dgate-gui: {}", err);
            eprintln!();
            eprint!("{}", state::HELP_TEXT);
            std::process::exit(2);
        }
    };
    // --help / -h — 깨끗하게 도움말 출력 후 exit 0 (창을 열지 않는다)
    if opts.help {
        print!("{}", state::HELP_TEXT);
        return;
    }

    let cfg = state::PollerConfig::from_options(&opts);
    let (poller, join) = state::spawn_poller(cfg);

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1240.0), px(780.0)), cx);
        let options = opts.clone();
        let view_poller = poller.clone();
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("dgate — TUI 5-Layer Flow Topology (GPUI)".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(640.0), px(480.0))),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| view::DashboardView::new(&options, view_poller.clone(), cx)),
        )
        .expect("dgate-gui 창 열기 실패");
        cx.activate(true);
    });

    // 앱 루프 종료 — 폴러 스레드를 깨끗하게 정리한다
    join.shutdown();
}
