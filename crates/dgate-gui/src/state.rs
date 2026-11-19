//! dgate-gui 순수 상태 계층 — GPUI 뷰·윈도우 없이 100% 단위 테스트 가능한 로직.
//!
//! 목적: 네이티브 관제 앱의 모든 데이터 조립·필터링·안전 장치를 GPU 렌더링과
//! 분리해 `cargo test -p dgate-gui`가 창 없이 돌게 한다.
//!
//! 외부 연결:
//! - `dgate_core::live_sync::LiveSyncSnapshotter::tick_now` — 실제 `hc` 워커 로스터.
//! - `dgate_core::telemetry::read_telemetry_snapshot` — 읽기전용 라우트 사후확률.
//! - `dgate_core::db::open_db(read_only)` + `stats`·`report` — 비용/시간/shadow 집계.
//! - `hc kill <addr>` — 워커 중단(2단계 확인 + 알려진 워커 검증 통과 시에만).
//!
//! 주의:
//! - DB는 절대 생성·마이그레이션하지 않는다(`read_only: true` — 파일 없으면 대기 상태).
//! - 백그라운드 폴러 스레드가 `Arc<Mutex<Option<DashboardSnapshot>>>`을 갱신하고
//!   version 카운터를 올리면 GPUI 엔티티가 짧은 타이머로 이를 감지해 `cx.notify()`
//!   한다(UI 스레드는 `hc`/SQLite I/O를 절대 직접 실행하지 않는다).
//! - 워커 중단은 단일 클릭으로 절대 실행되지 않는다: ① 제어 허용 토글 ② 중단 버튼
//!   ③ 확인 버튼의 3단계 관문 + 주소가 현재 live 워커 목록에 있는 재검증.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dgate_core::db::{open_db, resolve_db_path, resolve_project_identity, OpenDbOptions};
use dgate_core::engine::RouteId;
use dgate_core::live_sync::{
    format_role_badge, DefaultHcRunner, HcExecRunner, LiveSyncOptions, LiveSyncSnapshotter,
    LiveWorkerInfo,
};
use dgate_core::redact::redact_secrets;
use dgate_core::report::{generate_rule_candidate_report, DecisionApiShadowSummary, ReportOptions};
use dgate_core::roster::{
    load_roster_config, resolve_roster_profile, RoleSlot, ROLE_SLOTS,
};
use dgate_core::stats::{
    compute_cost_stats, compute_timing_stats, CostByStatus, CostStatsOptions, CostStatsResult,
    CostTokensSummary, SHARP_FLOOR, TimingStatsOptions, TimingStatsResult,
};
use dgate_core::telemetry::{heuristic_prior_routes, read_telemetry_snapshot, TelemetryRoute};

/// 기본 폴링 주기(ms) — brief 계약 2000ms.
pub const DEFAULT_POLL_MS: u64 = 2000;
/// UI 리프레시 감지 타이머 주기(ms) — 폴러와 뷰를 잇는 가벼운 버전 폴링.
pub const UI_REFRESH_TICK_MS: u64 = 250;

// ---------------------------------------------------------------------------
// CLI 옵션
// ---------------------------------------------------------------------------

/// `dgate-gui` CLI 옵션 — 수동 파서(clap 미사용, 의존성 최소).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GuiCliOptions {
    /// `--db <path>` — decision.db 경로 오버라이드(없으면 XDG 기본 경로).
    pub db_path: Option<String>,
    /// `--repo <dir>` — 관측 스코프 레포 디렉터리(없으면 전체 레포 관측).
    pub repo_dir: Option<String>,
    /// `--roster <path>` — roster.json 경로 오버라이드.
    pub roster_path: Option<String>,
    /// `--watch-only` — 제어 잠금(중단 버튼 비활성, 토글도 숨김).
    pub watch_only: bool,
    /// `--demo` — 기동 시부터 내장 DEMO 토폴로지 미리보기로 표시.
    pub demo: bool,
    /// `--poll-ms <n>` — 폴링 주기(기본 2000).
    pub poll_ms: u64,
    /// `--help` / `-h` — 도움말 출력 후 종료 요청.
    pub help: bool,
}

/// `--help` 출력 텍스트 — main.rs가 그대로 println하고 exit 0 한다.
pub const HELP_TEXT: &str = "\
dgate-gui — 선택적 네이티브 GPUI 관제/제어 데스크톱 앱 (Rust v2 Wave 2B)

사용법:
  dgate-gui [옵션]

옵션:
  --db <path>       decision.db 경로 오버라이드 (기본: ~/.local/state/dgate/decision.db)
  --repo <dir>      관측 스코프 레포 디렉터리 (기본: 전체 레포)
  --roster <path>   roster.json 경로 오버라이드 (기본: herdr-quad 탐색 규칙)
  --watch-only      관측 전용 모드로 기동 — 워커 중단 제어 영구 잠금
  --demo            기동 시부터 내장 DEMO 토폴로지 미리보기 표시 (LIVE/DEMO 토글로 전환)
  --poll-ms <n>     백그라운드 폴링 주기 밀리초 (기본: 2000)
  --help, -h        이 도움말을 출력하고 종료

DB는 항상 읽기전용으로만 엽니다. 워커 중단은 '제어 허용' 토글 + 2단계 확인 후
현재 live 워커 목록 재검증을 통과한 주소에만 `hc kill`을 실행합니다.
";

/// 인자 벡터 → GuiCliOptions. 값 누락·비숫자 poll-ms·알 수 없는 플래그는 Err.
pub fn parse_gui_cli_options(args: &[String]) -> Result<GuiCliOptions, String> {
    let mut opts = GuiCliOptions { poll_ms: DEFAULT_POLL_MS, ..Default::default() };
    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--help" | "-h" => {
                opts.help = true;
                return Ok(opts);
            }
            "--db" | "--repo" | "--roster" | "--poll-ms" => {
                i += 1;
                let value = args
                    .get(i)
                    .cloned()
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| {
                        format!("{} 플래그에 값이 필요합니다 (예: {} <value>)", arg, arg)
                    })?;
                match arg {
                    "--db" => opts.db_path = Some(value),
                    "--repo" => opts.repo_dir = Some(value),
                    "--roster" => opts.roster_path = Some(value),
                    _ => {
                        opts.poll_ms = value
                            .parse::<u64>()
                            .map_err(|_| format!("--poll-ms 값이 숫자가 아닙니다: {}", value))?
                            .max(100);
                    }
                }
            }
            "--watch-only" => opts.watch_only = true,
            "--demo" => opts.demo = true,
            other => {
                return Err(format!("알 수 없는 옵션입니다: {}", other));
            }
        }
        i += 1;
    }
    Ok(opts)
}

// ---------------------------------------------------------------------------
// 스냅숏 데이터 형태
// ---------------------------------------------------------------------------

/// 결정 게이트 표지 — θ_sharp(SHARP_FLOOR=0.70) 이상이면 Sharp, 미만·강제분기면 Split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionLabel {
    Sharp,
    Split,
}

impl DecisionLabel {
    pub fn as_str(&self) -> &'static str {
        match self {
            DecisionLabel::Sharp => "SHARP",
            DecisionLabel::Split => "SPLIT",
        }
    }
}

/// 라우트 1개의 게이트 표시 지표.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteMetric {
    pub id: String,
    pub name: String,
    pub p: f64,
    /// "prior" | "measured"
    pub basis: String,
    pub samples: i64,
    pub forced_split: bool,
    pub decision: DecisionLabel,
}

/// 5역할 카드 1개 — 대기(standby) 배지 vs live 오버라이드 배지.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleCardData {
    pub slot: String,
    /// 표시명 — Lead / Explore / Coder / Verifier / Advisor
    pub title: String,
    /// roster.json 기준 대기 모델 배지 (예: "pi:glm-5.3")
    pub standby_model: String,
    /// 지금 실제 실행 중이면 그 워커의 live 배지
    pub live_model: Option<String>,
    pub running: bool,
    pub readonly: bool,
    pub on_demand: bool,
}

/// 레포별 텔레메트리 번들 — 레포 탭 클릭 시 이 값으로 스냅숏을 갈아끼운다.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RepoStatsBundle {
    /// git 루트 realpath sha256 기준 project_id (루트 미상이면 None → 비필터 전역 조회)
    pub project_id: Option<String>,
    pub routes: Vec<RouteMetric>,
    pub cost: Option<CostStatsResult>,
    pub timing: Option<TimingStatsResult>,
    pub shadow: Option<DecisionApiShadowSummary>,
}

/// 폴러 1틱이 만드는 불변 대시보드 스냅숏.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DashboardSnapshot {
    pub updated_at_label: String,
    pub profile_name: String,
    pub roster_source: String,
    pub roles: Vec<RoleCardData>,
    pub workers: Vec<LiveWorkerInfo>,
    /// live 워커 중 agy CLI 사용 수 — "agy: N" 표시기.
    pub agy_live_count: usize,
    /// 탭 바에 올릴 고유·정렬된 레포명 (repo_tag + `--repo` basename).
    pub detected_repos: Vec<String>,
    /// 전체(전체) 뷰 라우트·통계(레포 집계 or prior 폴백).
    pub routes: Vec<RouteMetric>,
    pub cost_summary: Option<CostStatsResult>,
    pub timing_summary: Option<TimingStatsResult>,
    pub shadow_summary: Option<DecisionApiShadowSummary>,
    /// 레포 탭별 번들 — 키는 repo_tag.
    pub repo_stats: BTreeMap<String, RepoStatsBundle>,
    /// 세션 기록 패널에 올릴 최신 결정 이벤트(읽기전용 조회 + 리댁션 적용).
    pub recent_log: Vec<RecentLogEntry>,
    /// "연결됨" | "DB 없음 (대기)" — 파일 부재에도 절대 죽지 않는다.
    pub db_status: String,
    pub hc_available: bool,
}

// ---------------------------------------------------------------------------
// 세션 기록 — decision_events 읽기전용 조회 (TUI 5계층 하단 패널 대응)
// ---------------------------------------------------------------------------

/// 세션 기록 1줄 — `decision_events` 최신 이벤트를 역할 색 태그와 함께 표시.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentLogEntry {
    /// HH:MM:SS (로컬 타임존).
    pub time_hhmmss: String,
    /// 원본 event source ('hook'·'wrap'·'neon_backprop' …).
    pub source: String,
    /// 역할 표시명 — "Decision Gate" | "Worker" | "Verifier" | "Advisor" | "Lead".
    pub source_role: String,
    /// 리댁션 적용된 1줄 요약.
    pub message: String,
    /// 레포 탭 필터링용 project_id (전역이면 None).
    pub project_id: Option<String>,
}

/// 라우트 id → TUI 한국어 라벨(모르는 id는 원문 그대로 — 위장 번역 금지).
fn route_display_label(route: &str) -> &str {
    match route {
        "file_select" => "어느 파일",
        "tool_select" => "어느 도구",
        "retry_or_abort" => "재시도 또는 중단",
        "delegate_swarm" => "스웜 위임",
        other => other,
    }
}

/// 이벤트 source·route → 역할 태그(색은 뷰가 매핑).
/// Advisor는 decision_events에 기록하지 않으므로 라이브 조회에서는 등장하지 않고,
/// DEMO 표시용 로그에서만 등장한다.
pub fn event_source_role(source: &str, route: &str) -> &'static str {
    if source == "neon_backprop" {
        "Verifier"
    } else if route == "delegate_swarm" {
        "Worker"
    } else if source == "agy_stream" || source == "transcript" {
        "Lead"
    } else {
        "Decision Gate"
    }
}

/// `decision_events` 최신 `limit`건을 읽기전용으로 조회해 세션 기록 줄을 만든다.
/// 모든 오류(테이블 부재·컬럼 불일치 외래 DB 등)는 빈 벡터로 폴백 — 절대 패닉하지 않는다.
/// 반드시 `read_only: true`로 연 커넥션만 넣는다(호출 계약 — 이 함수는 쓰기를 하지 않는다).
pub fn fetch_recent_log(conn: &rusqlite::Connection, limit: usize) -> Vec<RecentLogEntry> {
    let mut stmt = match conn.prepare(
        "SELECT created_at_ms, project_id, source, route, tool_name, ok, error_snippet
         FROM decision_events
         ORDER BY created_at_ms DESC, event_id DESC
         LIMIT ?1",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    let rows = match stmt.query_map([limit as i64], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    }) {
        Ok(rows) => rows,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for row in rows.flatten() {
        let (created_ms, project_id, source, route, tool_name, ok, error_snippet) = row;
        let source_role = event_source_role(&source, &route).to_string();
        // 리댁션은 절단 이전에 적용(레드시스템 계약) — 잘린 경계의 패턴 누락을 막는다.
        let mut message = format!(
            "{} · {} · {}",
            route_display_label(&route),
            tool_name,
            if ok != 0 { "ok" } else { "실패" }
        );
        if let Some(snippet) = error_snippet
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let redacted = redact_secrets(snippet);
            let short: String = redacted.chars().take(64).collect();
            message.push_str(" — ");
            message.push_str(&short);
        }
        out.push(RecentLogEntry {
            time_hhmmss: format_local_time_label(created_ms),
            source,
            source_role,
            message: redact_secrets(&message),
            project_id,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// 레포 태그·필터링 (순수 함수)
// ---------------------------------------------------------------------------

/// 워커 cwd → 소속 레포 루트. `.herdr/worktrees/<wt>` 마커가 있으면 그 앞까지,
/// 아니면 cwd 자체. cwd 미상이면 None.
pub fn repo_root_from_cwd(cwd: Option<&str>) -> Option<PathBuf> {
    let cwd = cwd.map(str::trim).filter(|s| !s.is_empty())?;
    let norm = cwd.replace('\\', "/");
    if let Some(idx) = norm.find("/.herdr/worktrees/").filter(|i| *i > 0) {
        return Some(PathBuf::from(&norm[..idx]));
    }
    Some(PathBuf::from(cwd))
}

/// 워커 repo_tag들 + `--repo` basename → 중복 제거·정렬된 탭 목록.
pub fn build_detected_repos(
    workers: &[LiveWorkerInfo],
    repo_dir: Option<&Path>,
) -> Vec<String> {
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for w in workers {
        if !w.repo_tag.is_empty() {
            set.insert(w.repo_tag.clone());
        }
    }
    if let Some(dir) = repo_dir {
        if let Some(name) = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.trim().is_empty())
        {
            set.insert(name);
        }
    }
    set.into_iter().collect()
}

/// 스냅숏을 선택 레포로 필터링한다(순수 함수 — 원본 불변).
/// - `repo == None` → 전체(전역) 뷰 그대로 반환.
/// - `repo == Some(name)` → 워커는 해당 repo_tag만, 라우트·비용·시간·shadow는
///   `repo_stats[name]` 번들로 교체(번들 미 존재 시 전역 값 유지 — 위장 조회 금지).
/// - `detected_repos`는 탭 바 표시용이므로 항상 전체 목록을 보존한다.
pub fn filter_snapshot_by_repo(snapshot: &DashboardSnapshot, repo: Option<&str>) -> DashboardSnapshot {
    let Some(name) = repo.map(str::trim).filter(|s| !s.is_empty()) else {
        return snapshot.clone();
    };
    let mut out = snapshot.clone();
    out.workers = snapshot
        .workers
        .iter()
        .filter(|w| w.repo_tag == name)
        .cloned()
        .collect();
    if let Some(bundle) = snapshot.repo_stats.get(name) {
        out.routes = bundle.routes.clone();
        out.cost_summary = bundle.cost.clone();
        out.timing_summary = bundle.timing.clone();
        out.shadow_summary = bundle.shadow.clone();
        // 세션 기록도 선택 레포의 project_id로 필터링(번들 project_id 미상이면 전체 유지).
        if let Some(pid) = bundle.project_id.as_deref() {
            out.recent_log = snapshot
                .recent_log
                .iter()
                .filter(|e| e.project_id.as_deref() == Some(pid))
                .cloned()
                .collect();
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 워커 그리드 열 계산 · 라이브 모션 활동 판정 (순수 함수 — GUI 뷰가 사용)
// ---------------------------------------------------------------------------

/// 화면에 배치할 워커 카드 열 수(= 커넥터 스템 수).
/// - 라이브 0 — 3슬롯 청사진 토폴로지 유지(침묵 3스템 와이어와 정렬).
/// - 1/2/3 — 카드 수와 정확히 같은 수의 스템(매달린 여분 스템 금지).
/// - 4+ — 최대 4열, 초과 워커는 2행 랩('+M개 추가' 배지로 절대 가리지 않는다).
pub fn visible_worker_columns(worker_count: usize) -> usize {
    match worker_count {
        0 => 3,
        1 => 1,
        2 => 2,
        3 => 3,
        _ => 4,
    }
}

/// 지금 실제로 활동 중인가 — DEMO 모드이거나 done 아닌 live 워커가 하나라도
/// 있을 때만 true. 라이브 & 실행 0(빈 로스터·전부 완료)은 반드시 false여서
/// GUI 애니메이션 틱(anim_tick 증가·cx.notify)이 완전히 멈추는 침착 유휴를
/// 보장한다. 역할 카드(Lead/Verifier/Advisor)의 running은 어차피 live 워커
/// 존재에서 유도되므로 워커 상태 기준 이 판정 하나로 충분하다.
pub fn has_active_work(snap: &DashboardSnapshot, demo_mode: bool) -> bool {
    demo_mode || snap.workers.iter().any(|w| w.status.as_deref() != Some("done"))
}

// ---------------------------------------------------------------------------
// 워커 중단 안전 장치 (2단계 확인 + live 목록 재검증)
// ---------------------------------------------------------------------------

/// `hc kill` 실행 추상 — 테스트가 호출 기록 mock으로 갈아끼운다.
pub trait KillExecutor: Send + Sync {
    fn kill(&self, hc_bin: &str, address: &str) -> Result<String, String>;
}

/// 기본 실행기 — argv 배열로 직접 실행(셸 주입 경로 자체가 없다).
pub struct DefaultKillExecutor;

impl KillExecutor for DefaultKillExecutor {
    fn kill(&self, hc_bin: &str, address: &str) -> Result<String, String> {
        let out = std::process::Command::new(hc_bin)
            .args(["kill", address])
            .output()
            .map_err(|e| format!("{} 실행 실패: {}", hc_bin, e))?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(format!(
                "hc kill 실패 (exit {:?}){}",
                out.status.code(),
                if stderr.is_empty() { String::new() } else { format!(": {}", stderr) }
            ));
        }
        Ok(if stdout.is_empty() { "ok".to_string() } else { stdout })
    }
}

/// 안전한 워커 중단 — 반드시 현재 live 워커 목록에 존재하는 주소만 허용.
/// 모니터(`dg-monitor*`·`mon-*`) 이름 방어 + argv 배열 실행. 절대 단독 호출 금지
/// (호출자는 2단계 확인 상태를 통과한 뒤에만 이 함수를 부른다).
pub fn kill_worker_safe(
    address: &str,
    known_workers: &[LiveWorkerInfo],
    hc_bin: &str,
    exec: &dyn KillExecutor,
) -> Result<String, String> {
    let addr = address.trim();
    if addr.is_empty() {
        return Err("중단 대상 주소가 비어 있습니다".to_string());
    }
    let Some(worker) = known_workers.iter().find(|w| w.address == addr) else {
        return Err(format!(
            "현재 live 워커 목록에 없는 주소입니다 — 중단 거부: {}",
            addr
        ));
    };
    let name_lower = worker.name.to_lowercase();
    if name_lower.starts_with("dg-monitor") || name_lower.starts_with("mon-") {
        return Err(format!(
            "모니터 세션({})는 중단 대상이 아닙니다 — 거부",
            worker.name
        ));
    }
    exec.kill(hc_bin, addr)
}

// ---------------------------------------------------------------------------
// 대시보드 컨트롤러 (UI 상태 기계 — 순수 Rust)
// ---------------------------------------------------------------------------

/// GPUI 뷰가 들고 다니는 UI 상태 — 스냅숏 적용·탭 선택·확인 상태 전이만 담당.
#[derive(Debug, Clone, PartialEq)]
pub struct DashboardController {
    pub snapshot: Option<DashboardSnapshot>,
    pub selected_repo: Option<String>,
    pub paused: bool,
    pub watch_only: bool,
    /// 관측 전용(false) ↔ 제어 허용(true) — `--watch-only`면 영구 false.
    pub control_enabled: bool,
    /// LIVE 스냅숏 ↔ 내장 DEMO 토폴로지 미리보기 (`--demo` 시작 시 true).
    pub demo_mode: bool,
    /// 2단계 확인 진행 중인 워커 주소.
    pub confirm_kill_address: Option<String>,
    /// 사용자에게 보여줄 최근 조작 결과 1줄.
    pub last_action_message: Option<String>,
}

impl DashboardController {
    pub fn new(watch_only: bool) -> Self {
        DashboardController {
            snapshot: None,
            selected_repo: None,
            paused: false,
            watch_only,
            control_enabled: false,
            demo_mode: false,
            confirm_kill_address: None,
            last_action_message: None,
        }
    }

    /// 폴러가 만든 새 스냅숏 반영 — 사라진 레포 탭 선택·확인 대상은 자동 해제.
    pub fn apply_snapshot(&mut self, snap: DashboardSnapshot) {
        if let Some(sel) = &self.selected_repo {
            if !snap.detected_repos.iter().any(|r| r == sel) {
                self.selected_repo = None;
            }
        }
        if let Some(addr) = &self.confirm_kill_address {
            if !snap.workers.iter().any(|w| &w.address == addr) {
                self.confirm_kill_address = None;
                self.last_action_message =
                    Some("확인 중이던 워커가 목록에서 사라져 확인 상태를 해제했습니다".to_string());
            }
        }
        self.snapshot = Some(snap);
    }

    /// 현재 선택 기준 필터링된 뷰 스냅숏(스냅숏 미수신 시 대기 상태 기본값).
    /// DEMO 모드면 라이브 스냅숏 대신 내장 시뮬레이션 토폴로지를 반환한다.
    pub fn filtered(&self) -> DashboardSnapshot {
        if self.demo_mode {
            return demo_snapshot();
        }
        let Some(snap) = &self.snapshot else {
            let mut idle = DashboardSnapshot::default();
            idle.db_status = "초기화 중…".to_string();
            idle.routes = route_metrics_from(&heuristic_prior_routes());
            return idle;
        };
        filter_snapshot_by_repo(snap, self.selected_repo.as_deref())
    }

    /// 레포 탭 선택(전체 탭은 None). 확인 상태는 유지(주소 재검증은 confirm에서).
    pub fn select_repo(&mut self, repo: Option<&str>) {
        self.selected_repo = repo
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }

    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        self.last_action_message =
            Some(if self.paused { "폴링 일시정지" } else { "폴링 재개" }.to_string());
    }

    /// LIVE ↔ DEMO 전환 — DEMO 진입 시 확인 상태를 해제한다(가상 주소 차단).
    pub fn toggle_demo_mode(&mut self) {
        self.demo_mode = !self.demo_mode;
        if self.demo_mode {
            self.confirm_kill_address = None;
        }
        self.last_action_message = Some(
            if self.demo_mode {
                "DEMO 시뮬레이션 미리보기 — LIVE로 돌아가려면 토글을 다시 누르세요"
            } else {
                "LIVE 스냅숏으로 복귀"
            }
            .to_string(),
        );
    }

    /// 관측 전용 ↔ 제어 허용 — watch-only 잠금 시 무시된다.
    pub fn toggle_control(&mut self) {
        if self.watch_only {
            self.last_action_message =
                Some("관측 전용(--watch-only) 잠금 — 제어를 켤 수 없습니다".to_string());
            return;
        }
        self.control_enabled = !self.control_enabled;
        if !self.control_enabled {
            self.confirm_kill_address = None;
        }
        self.last_action_message =
            Some(if self.control_enabled { "제어 허용 — 2단계 확인 후 중단 가능" } else { "관측 전용" }.to_string());
    }

    /// 중단 1단계 — 제어 허용 + live 목록 주소일 때만 확인 상태 진입.
    pub fn request_kill(&mut self, address: &str) -> bool {
        let Some(snap) = &self.snapshot else {
            self.last_action_message = Some("아직 스냅숏이 없습니다".to_string());
            return false;
        };
        if self.watch_only || !self.control_enabled {
            self.last_action_message =
                Some("관측 전용 모드 — 상단 '제어 허용'을 먼저 켜세요".to_string());
            return false;
        }
        if self.demo_mode {
            self.last_action_message =
                Some("DEMO 모드에서는 워커 중단을 실행할 수 없습니다 — LIVE로 전환하세요".to_string());
            return false;
        }
        if !snap.workers.iter().any(|w| w.address == address) {
            self.last_action_message =
                Some(format!("live 워커 목록에 없는 주소 — 중단 요청 거부: {}", address));
            return false;
        }
        self.confirm_kill_address = Some(address.to_string());
        true
    }

    /// 중단 2단계 — 확인 상태 주소가 여전히 live면 반환하고 상태를 해제한다.
    pub fn confirm_kill(&mut self) -> Option<String> {
        let addr = self.confirm_kill_address.take()?;
        let still_live = self
            .snapshot
            .as_ref()
            .map(|s| s.workers.iter().any(|w| w.address == addr))
            .unwrap_or(false);
        if still_live {
            Some(addr)
        } else {
            self.last_action_message =
                Some("확인 대상 워커가 이미 종료되었습니다".to_string());
            None
        }
    }

    pub fn cancel_kill(&mut self) {
        self.confirm_kill_address = None;
        self.last_action_message = Some("중단 확인 취소".to_string());
    }

    /// 비동기 kill 결과 회신(뷰가 백그라운드 실행 후 호출).
    pub fn note_kill_result(&mut self, address: &str, result: &Result<String, String>) {
        self.last_action_message = Some(match result {
            Ok(out) => format!("hc kill {} 완료 — {}", address, out),
            Err(err) => format!("hc kill {} 실패 — {}", address, err),
        });
    }
}

// ---------------------------------------------------------------------------
// 스냅숏 조립
// ---------------------------------------------------------------------------

/// 폴러 구성 — PollerConfig → LiveSyncSnapshotter + DB 조회 경로.
#[derive(Debug, Clone)]
pub struct PollerConfig {
    pub db_path: Option<String>,
    pub repo_dir: Option<PathBuf>,
    pub roster_path: Option<String>,
    pub poll_ms: u64,
    pub hc_bin: String,
}

impl PollerConfig {
    pub fn from_options(opts: &GuiCliOptions) -> Self {
        PollerConfig {
            db_path: opts.db_path.clone(),
            repo_dir: opts.repo_dir.as_deref().map(PathBuf::from),
            roster_path: opts.roster_path.clone(),
            poll_ms: opts.poll_ms.max(100),
            hc_bin: "hc".to_string(),
        }
    }
}

/// 로컬 타임존 기준 HH:MM:SS 라벨(마지막 새로고침 표시).
pub fn format_local_time_label(now_ms: i64) -> String {
    let secs = now_ms.div_euclid(1000);
    let t: libc::time_t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// TelemetryRoute → RouteMetric 변환 — θ_sharp(0.70) 기준 SHARP/SPLIT 표지.
pub fn route_metric_from(tr: TelemetryRoute) -> RouteMetric {
    let decision = if tr.forced_split || tr.p < SHARP_FLOOR {
        DecisionLabel::Split
    } else {
        DecisionLabel::Sharp
    };
    RouteMetric {
        id: tr.id.as_str().to_string(),
        name: tr.name.to_string(),
        p: tr.p,
        basis: tr.basis.as_str().to_string(),
        samples: tr.samples,
        forced_split: tr.forced_split,
        decision,
    }
}

fn route_metrics_from(routes: &[TelemetryRoute]) -> Vec<RouteMetric> {
    routes.iter().cloned().map(route_metric_from).collect()
}

/// 레포 번들들의 라우트를 표본수 가중 평균으로 집계한 전역(전체) 뷰 라우트.
/// 표본 0 라우트는 휴리스틱 prior로 폴백 — 절대 0으로 나누지 않는다.
fn aggregate_global_routes(bundles: &[RepoStatsBundle]) -> Vec<RouteMetric> {
    const ORDER: [(&str, &str); 4] = [
        ("file_select", "어느 파일"),
        ("tool_select", "어느 도구"),
        ("retry_or_abort", "재시도 또는 중단"),
        ("delegate_swarm", "스웜 위임"),
    ];
    let priors = heuristic_prior_routes();
    ORDER
        .iter()
        .map(|&(id, name)| {
            let matches: Vec<&RouteMetric> = bundles
                .iter()
                .flat_map(|b| b.routes.iter().filter(|r| r.id == id))
                .collect();
            let total: i64 = matches.iter().map(|r| r.samples.max(0)).sum();
            if total <= 0 {
                let prior = priors
                    .iter()
                    .find(|r| r.id.as_str() == id)
                    .cloned()
                    .unwrap_or_else(|| TelemetryRoute {
                        id: RouteId::parse(id).unwrap_or(RouteId::FileSelect),
                        name,
                        p: 0.8,
                        basis: dgate_core::engine::DecisionBasis::Prior,
                        samples: 0,
                        consecutive_errors: 0,
                        forced_split: false,
                    });
                return route_metric_from(prior);
            }
            let p = matches.iter().map(|r| r.p * r.samples.max(0) as f64).sum::<f64>() / total as f64;
            let measured = matches.iter().any(|r| r.basis == "measured");
            let forced = matches.iter().any(|r| r.forced_split);
            RouteMetric {
                id: id.to_string(),
                name: name.to_string(),
                p,
                basis: if measured { "measured" } else { "prior" }.to_string(),
                samples: total,
                forced_split: forced,
                decision: if forced || p < SHARP_FLOOR {
                    DecisionLabel::Split
                } else {
                    DecisionLabel::Sharp
                },
            }
        })
        .collect()
}

fn role_title(slot: &RoleSlot) -> &'static str {
    match slot {
        RoleSlot::Lead => "Lead",
        RoleSlot::Explore => "Explore",
        RoleSlot::Coder => "Coder",
        RoleSlot::Verifier => "Verifier",
        RoleSlot::Advisor => "Advisor",
    }
}

/// `hc` 사용 가능 프로브 — `hc --version` 실행 성공 여부(부재 시 조용히 false).
fn probe_hc_available(hc_bin: &str) -> bool {
    DefaultHcRunner.exec(hc_bin, &["--version"], 700).is_ok()
}

/// 레포 루트 목록(중복 제거, repo_tag 기준 정렬 보존) — 워커 cwd에서 유도.
fn repo_roots_from_workers(workers: &[LiveWorkerInfo]) -> BTreeMap<String, Option<PathBuf>> {
    let mut map: BTreeMap<String, Option<PathBuf>> = BTreeMap::new();
    for w in workers {
        if w.repo_tag.is_empty() {
            continue;
        }
        map.entry(w.repo_tag.clone())
            .or_insert_with(|| repo_root_from_cwd(w.cwd.as_deref()));
    }
    map
}

/// 폴러 1틱 — live 로스터 + 읽기전용 DB 조립. 어떤 오류(SPA 경합·DB 부재)에도
/// 죽지 않고 대기 상태 스냅숏을 반환한다. 순수 데이터 조립이라 창 없이 테스트 가능.
pub fn poll_once(cfg: &PollerConfig, snapshotter: &mut LiveSyncSnapshotter) -> DashboardSnapshot {
    let repo_dir = cfg
        .repo_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));

    // 1) live hc 워커 + roster 라벨 병합(모든 hc 오류는 내부에서 조용히 폴백)
    let merged = snapshotter.tick_now();
    let workers = merged.workers.clone();

    // 2) roster 대기 스펙(프로필 해석은 live_sync와 동일 규칙: defaultProfile)
    let loaded = load_roster_config(cfg.roster_path.as_deref(), Some(&repo_dir));
    let (profile_name, profile, _) = resolve_roster_profile(&loaded.config, &loaded.config.default_profile);

    let roles: Vec<RoleCardData> = ROLE_SLOTS
        .iter()
        .map(|&slot| {
            let spec = profile.roles.get(&slot);
            let live = workers.iter().find(|w| w.slot == slot);
            RoleCardData {
                slot: slot.as_str().to_string(),
                title: role_title(&slot).to_string(),
                standby_model: spec
                    .map(|s| format_role_badge(s.cli.as_str(), s.model.as_deref()))
                    .unwrap_or_else(|| "—".to_string()),
                live_model: live.map(|w| w.short_model.clone()),
                running: live.is_some(),
                readonly: spec.and_then(|s| s.readonly).unwrap_or(false),
                on_demand: spec.and_then(|s| s.on_demand).unwrap_or(false),
            }
        })
        .collect();
    let agy_live_count = workers
        .iter()
        .filter(|w| w.short_model.starts_with("agy:"))
        .count();

    // 3) DB — 항상 읽기전용. 파일 없으면 대기 상태(생성·마이그레이션 절대 없음).
    let db_resolved = resolve_db_path(cfg.db_path.as_deref());
    let db_status = if db_resolved.exists() {
        "연결됨".to_string()
    } else {
        "DB 없음 (대기)".to_string()
    };
    let db_str = db_resolved.to_string_lossy().into_owned();
    let conn = open_db(
        Some(db_str.as_str()),
        OpenDbOptions { read_only: true, busy_timeout_ms: Some(1000) },
    )
    .ok();

    // 4) 레포별 번들 — 워커 cwd에서 유도한 루트의 project_id로 필터링.
    let roots = repo_roots_from_workers(&workers);
    let mut repo_stats: BTreeMap<String, RepoStatsBundle> = BTreeMap::new();
    for (tag, root) in &roots {
        let project_id = root
            .as_deref()
            .map(|r| resolve_project_identity(r).project_id);
        let routes = match root.as_deref() {
            Some(r) => {
                let snap = read_telemetry_snapshot(cfg.db_path.as_deref(), Some(r));
                route_metrics_from(&snap.routes)
            }
            None => route_metrics_from(&heuristic_prior_routes()),
        };
        let (cost, timing, shadow) = match &conn {
            Some(db) => {
                let pid_ref = project_id.as_deref().map(|s| s.to_string());
                let cost = compute_cost_stats(
                    db,
                    &CostStatsOptions { project_id: pid_ref.clone(), ..Default::default() },
                );
                let timing = compute_timing_stats(
                    db,
                    &TimingStatsOptions { project_id: pid_ref.clone(), ..Default::default() },
                );
                let shadow = generate_rule_candidate_report(
                    db,
                    &ReportOptions { project_id: pid_ref, ..Default::default() },
                )
                .decision_api_shadow;
                (Some(cost), Some(timing), Some(shadow))
            }
            None => (None, None, None),
        };
        repo_stats.insert(
            tag.clone(),
            RepoStatsBundle { project_id, routes, cost, timing, shadow },
        );
    }

    // 5) 전역(전체) 통계 — 프로젝트 필터 없음 + 라우트는 레포 가중 집계.
    let (global_cost, global_timing, global_shadow) = match &conn {
        Some(db) => {
            let cost = compute_cost_stats(db, &CostStatsOptions::default());
            let timing = compute_timing_stats(db, &TimingStatsOptions::default());
            let shadow = generate_rule_candidate_report(db, &ReportOptions::default())
                .decision_api_shadow;
            (Some(cost), Some(timing), Some(shadow))
        }
        None => (None, None, None),
    };
    // 세션 기록 — 동일 읽기전용 커넥션에서 최신 이벤트 조회(리댁션 내장).
    let recent_log = conn
        .as_ref()
        .map(|c| fetch_recent_log(c, RECENT_LOG_LIMIT))
        .unwrap_or_default();
    drop(conn);
    let global_routes = aggregate_global_routes(&repo_stats.values().cloned().collect::<Vec<_>>());

    DashboardSnapshot {
        updated_at_label: format_local_time_label(now_ms()),
        profile_name,
        roster_source: loaded.source.as_str().to_string(),
        roles,
        workers,
        agy_live_count,
        detected_repos: build_detected_repos(
            merged.workers.as_slice(),
            cfg.repo_dir.as_deref(),
        ),
        routes: global_routes,
        cost_summary: global_cost,
        timing_summary: global_timing,
        shadow_summary: global_shadow,
        repo_stats,
        recent_log,
        db_status,
        hc_available: probe_hc_available(&cfg.hc_bin),
    }
}

// ---------------------------------------------------------------------------
// DEMO 시뮬레이션 스냅숏 — TUI 5계층 토폴로지 미리보기 (`--demo` · LIVE/DEMO 토글)
// ---------------------------------------------------------------------------

/// 세션 기록 패널에 올리는 최대 이벤트 수 (TUI 하단 패널 6~8줄과 동일 규모).
pub const RECENT_LOG_LIMIT: usize = 8;

/// 데모용 live 워커 카드 — 주소 고정(활동바 안정 채움 계산에 사용).
fn demo_worker(slot: RoleSlot, name: &str, activity: &str, short_model: &str) -> LiveWorkerInfo {
    LiveWorkerInfo {
        slot,
        badge: format!("{} (실행 중)", short_model),
        short_model: short_model.to_string(),
        model_name: short_model.to_string(),
        address: format!("demo-{}", name),
        name: name.to_string(),
        cwd: Some("/demo/demo-app".to_string()),
        repo_tag: "demo-app".to_string(),
        activity: activity.to_string(),
        status: Some("working".to_string()),
    }
}

fn demo_role(slot: &str, title: &str, model: &str, running: bool) -> RoleCardData {
    RoleCardData {
        slot: slot.to_string(),
        title: title.to_string(),
        standby_model: model.to_string(),
        live_model: running.then(|| model.to_string()),
        running,
        readonly: false,
        on_demand: slot == "advisor" || slot == "verifier",
    }
}

fn demo_route(
    id: &str,
    name: &str,
    p: f64,
    samples: i64,
    decision: DecisionLabel,
) -> RouteMetric {
    RouteMetric {
        id: id.to_string(),
        name: name.to_string(),
        p,
        basis: "measured".to_string(),
        samples,
        forced_split: false,
        decision,
    }
}

/// 내장 DEMO 토폴로지 스냅숏 — live DB/hc 없이 TUI 5계층 레이아웃을 그대로 미리본다.
/// 순수 함수(외부 I/O 없음)이므로 DEMO 모드에서 폴러를 대체해도 부작용이 없다.
pub fn demo_snapshot() -> DashboardSnapshot {
    let routes = vec![
        demo_route("file_select", "어느 파일", 0.86, 128, DecisionLabel::Sharp),
        demo_route("tool_select", "어느 도구", 0.74, 96, DecisionLabel::Sharp),
        demo_route("delegate_swarm", "스웜 위임", 0.62, 47, DecisionLabel::Split),
        demo_route("retry_or_abort", "재시도 또는 중단", 0.31, 22, DecisionLabel::Split),
    ];
    let cost = CostStatsResult {
        schema_v1: false,
        total_events: 48,
        by_status: CostByStatus { reported: 40, estimated: 5, unknown: 3 },
        per_status: Default::default(),
        reported_usd: 3.28,
        estimated_usd: 0.61,
        known_usd: 3.89,
        unknown_events: 3,
        total_usd_display: None,
        tokens: CostTokensSummary {
            input: 412_000,
            output: 38_500,
            cache_read: 96_000,
            cache_write: 12_000,
        },
    };
    let timing = TimingStatsResult {
        intervals_unavailable: false,
        coverage_ratio: 0.87,
        execution_ms: 812_000.0,
        sum_execution_ms: 1_904_000.0,
        parallel_gain_ms: 1_092_000.0,
        wait_ms: 96_000.0,
        human_wait_ms: 12_000.0,
        wall_ms: 2_610_000.0,
        dropped: 0,
        forks_total: 128,
        forks_with_intervals: 111,
        intervals: 111,
    };
    let shadow = DecisionApiShadowSummary {
        total: 26,
        ok: 26,
        errors: 0,
        shadow_count: 26,
        on_applied_count: 0,
        disagreed: 3,
        disagreement_rate: Some(3.0 / 26.0),
        total_input_tokens: 31_400,
        total_cost_usd: 0.018,
    };
    let workers = vec![
        demo_worker(
            RoleSlot::Lead,
            "w-lead-1",
            "작업 분해 · Decision Gate 사전 확정 검토 중",
            "pi:glm-5.3",
        ),
        demo_worker(
            RoleSlot::Explore,
            "w-explore-1",
            "src/ui 레이아웃 구조 탐색 중",
            "pi:gpt-5.6-luna",
        ),
        demo_worker(
            RoleSlot::Coder,
            "w-coder-1",
            "renderApp.ts 5계층 토폴로지 리팩터 적용 중",
            "pi:glm-5.3",
        ),
        demo_worker(
            RoleSlot::Verifier,
            "w-verify-1",
            "cargo test -p demo-app 결과 교차 검토 중",
            "pi:gpt-6-astra",
        ),
        demo_worker(
            RoleSlot::Advisor,
            "w-advisor-1",
            "완료 전 최종 점검 자문 준비 중",
            "zai:glm-5.3-flash",
        ),
    ];
    let recent_log = vec![
        RecentLogEntry {
            time_hhmmss: "10:42:18".to_string(),
            source: "wrap".to_string(),
            source_role: "Decision Gate".to_string(),
            message: "스웜 위임 · p=0.62 split — w-explore-1 배차".to_string(),
            project_id: None,
        },
        RecentLogEntry {
            time_hhmmss: "10:41:52".to_string(),
            source: "wrap".to_string(),
            source_role: "Worker".to_string(),
            message: "w-coder-1 · renderApp.ts 토폴로지 리팩터 적용".to_string(),
            project_id: None,
        },
        RecentLogEntry {
            time_hhmmss: "10:41:30".to_string(),
            source: "neon_backprop".to_string(),
            source_role: "Verifier".to_string(),
            message: "검증 통과 — 계획·결과 교차 검토 일치".to_string(),
            project_id: None,
        },
        RecentLogEntry {
            time_hhmmss: "10:40:07".to_string(),
            source: "hook".to_string(),
            source_role: "Advisor".to_string(),
            message: "같은 오류 2회 감지 — 재시도 전략 자문 제안".to_string(),
            project_id: None,
        },
        RecentLogEntry {
            time_hhmmss: "10:39:44".to_string(),
            source: "agy_stream".to_string(),
            source_role: "Lead".to_string(),
            message: "작업 분해 완료 — 하위 에이전트 3종 배차".to_string(),
            project_id: None,
        },
    ];
    DashboardSnapshot {
        updated_at_label: "DEMO".to_string(),
        profile_name: "demo".to_string(),
        roster_source: "builtin-demo".to_string(),
        roles: vec![
            demo_role("lead", "Lead", "pi:glm-5.3", true),
            demo_role("explore", "Explore", "pi:gpt-5.6-luna", true),
            demo_role("coder", "Coder", "pi:glm-5.3", true),
            demo_role("verifier", "Verifier", "pi:gpt-6-astra", true),
            demo_role("advisor", "Advisor", "zai:glm-5.3-flash", true),
        ],
        workers,
        agy_live_count: 0,
        detected_repos: vec!["demo-app".to_string()],
        repo_stats: BTreeMap::from([(
            "demo-app".to_string(),
            RepoStatsBundle {
                project_id: Some("demo-project".to_string()),
                routes: routes.clone(),
                cost: Some(cost.clone()),
                timing: Some(timing.clone()),
                shadow: Some(shadow.clone()),
            },
        )]),
        routes,
        cost_summary: Some(cost),
        timing_summary: Some(timing),
        shadow_summary: Some(shadow),
        recent_log,
        db_status: "DEMO 시뮬레이션".to_string(),
        hc_available: true,
    }
}

// ---------------------------------------------------------------------------
// 백그라운드 폴러 스레드
// ---------------------------------------------------------------------------

/// 폴러↔뷰 공유 상태 — 스냅숏 + 버전 카운터 + 제어 플래그.
pub struct SharedPollerState {
    pub snapshot: Mutex<Option<DashboardSnapshot>>,
    pub version: AtomicU64,
    pub paused: AtomicBool,
    pub shutdown: AtomicBool,
}

impl SharedPollerState {
    /// 현재 스냅숏을 (버전과 함께) 원자적으로 읽는다.
    pub fn take(&self) -> (u64, Option<DashboardSnapshot>) {
        let version = self.version.load(Ordering::SeqCst);
        let snap = self
            .snapshot
            .lock()
            .map(|g| g.clone())
            .unwrap_or(None);
        (version, snap)
    }
}

/// 뷰가 폴러를 깨우고 상태를 읽는 핸들(Clone 가능 — 조인은 spawn이 돌려준 별도 핸들 담당).
#[derive(Clone)]
pub struct PollerHandle {
    pub shared: Arc<SharedPollerState>,
    wake_tx: mpsc::Sender<()>,
}

impl PollerHandle {
    /// 즉시 폴링 트리거(새로고침 버튼·재개).
    pub fn wake(&self) {
        let _ = self.wake_tx.send(());
    }

    /// 폴링 일시정지/재개 전환 + 즉시 웨이크.
    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::SeqCst);
        self.wake();
    }
}

/// 폴러 조인 핸들 — 앱 종료 시 shutdown 플래그 + 웨이크 + 조인으로 깨끗하게 닫는다.
pub struct PollerJoinHandle {
    shared: Arc<SharedPollerState>,
    wake_tx: mpsc::Sender<()>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl PollerJoinHandle {
    pub fn shutdown(mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        let _ = self.wake_tx.send(());
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// 백그라운드 폴러 기동 — 즉시 1틱 후 `poll_ms` 주기(또는 웨이크 수신 시 즉시)로
/// 스냅숏을 갱신한다. 일시정지 중에는 틱을 건너뛰고 웨이크만 대기한다.
/// UI 스레드는 이 스레드가 올린 version 카운터만 짧은 타이머로 관찰한다.
pub fn spawn_poller(cfg: PollerConfig) -> (PollerHandle, PollerJoinHandle) {
    let shared = Arc::new(SharedPollerState {
        snapshot: Mutex::new(None),
        version: AtomicU64::new(0),
        paused: AtomicBool::new(false),
        shutdown: AtomicBool::new(false),
    });
    let (wake_tx, wake_rx) = mpsc::channel::<()>();
    let poll_ms = cfg.poll_ms.max(100);

    let thread_shared = shared.clone();
    let join = std::thread::Builder::new()
        .name("dgate-gui-poller".to_string())
        .spawn(move || {
            let sync_opts = LiveSyncOptions {
                repo_filter_dir: cfg.repo_dir.clone(),
                ..LiveSyncOptions::new(
                    cfg.repo_dir
                        .clone()
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))),
                )
            };
            let mut snapshotter = LiveSyncSnapshotter::new(sync_opts);
            loop {
                if thread_shared.shutdown.load(Ordering::SeqCst) {
                    break;
                }
                if !thread_shared.paused.load(Ordering::SeqCst) {
                    let snap = poll_once(&cfg, &mut snapshotter);
                    if let Ok(mut guard) = thread_shared.snapshot.lock() {
                        *guard = Some(snap);
                    } else {
                        break; // 락 오염 — 더 진행해도 무의미
                    }
                    thread_shared.version.fetch_add(1, Ordering::SeqCst);
                }
                match wake_rx.recv_timeout(Duration::from_millis(poll_ms)) {
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    _ => {}
                }
            }
        })
        .expect("dgate-gui 폴러 스레드 기동 실패");

    let handle = PollerHandle { shared: shared.clone(), wake_tx: wake_tx.clone() };
    let join_handle = PollerJoinHandle { shared, wake_tx, join: Some(join) };
    (handle, join_handle)
}

// ---------------------------------------------------------------------------
// 단위 테스트 — 전부 창/GPU 없이 실행된다
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dgate_core::live_sync::LiveWorkerInfo;
    use dgate_core::roster::RoleSlot;
    use std::sync::Mutex as StdMutex;

    // ---------- GUI CLI 파싱 ----------

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cli_defaults_when_no_args() {
        let o = parse_gui_cli_options(&args(&[])).unwrap();
        assert_eq!(o.db_path, None);
        assert_eq!(o.repo_dir, None);
        assert_eq!(o.roster_path, None);
        assert!(!o.watch_only);
        assert_eq!(o.poll_ms, 2000);
        assert!(!o.help);
    }

    #[test]
    fn cli_parses_all_flags() {
        let o = parse_gui_cli_options(&args(&[
            "--db", "/tmp/x.db", "--repo", "/tmp/repo", "--roster", "/tmp/r.json",
            "--watch-only", "--poll-ms", "500",
        ]))
        .unwrap();
        assert_eq!(o.db_path.as_deref(), Some("/tmp/x.db"));
        assert_eq!(o.repo_dir.as_deref(), Some("/tmp/repo"));
        assert_eq!(o.roster_path.as_deref(), Some("/tmp/r.json"));
        assert!(o.watch_only);
        assert_eq!(o.poll_ms, 500);
        assert!(!o.help);
    }

    #[test]
    fn cli_help_flag_short_circuits() {
        let o = parse_gui_cli_options(&args(&["--help"])).unwrap();
        assert!(o.help);
        let o2 = parse_gui_cli_options(&args(&["-h", "--bogus-after"])).unwrap();
        assert!(o2.help);
    }

    #[test]
    fn cli_parses_demo_flag() {
        let o = parse_gui_cli_options(&args(&["--demo"])).unwrap();
        assert!(o.demo);
        assert!(!o.watch_only);
        // 다른 플래그와 조합 가능
        let o2 = parse_gui_cli_options(&args(&["--demo", "--watch-only", "--poll-ms", "250"]))
            .unwrap();
        assert!(o2.demo);
        assert!(o2.watch_only);
        assert_eq!(o2.poll_ms, 250);
        // 기본값은 DEMO 꺼짐 — 실제 관측이 기본
        let o3 = parse_gui_cli_options(&args(&[])).unwrap();
        assert!(!o3.demo);
        // 도움말 텍스트에 --demo 옵션이 문서화되어 있다
        assert!(HELP_TEXT.contains("--demo"));
    }

    #[test]
    fn cli_rejects_missing_values_bad_numbers_and_unknown_flags() {
        assert!(parse_gui_cli_options(&args(&["--db"])).is_err());
        assert!(parse_gui_cli_options(&args(&["--repo"])).is_err());
        assert!(parse_gui_cli_options(&args(&["--poll-ms"])).is_err());
        assert!(parse_gui_cli_options(&args(&["--poll-ms", "abc"])).is_err());
        assert!(parse_gui_cli_options(&args(&["--nonsense"])).is_err());
        // poll-ms 하한 클램프(100ms 미만 차단)
        let clamped = parse_gui_cli_options(&args(&["--poll-ms", "10"])).unwrap();
        assert_eq!(clamped.poll_ms, 100);
    }

    // ---------- 헬퍼: 테스트 픽스처 ----------

    fn worker(addr: &str, name: &str, repo: &str, cwd: Option<&str>) -> LiveWorkerInfo {
        LiveWorkerInfo {
            slot: RoleSlot::Coder,
            badge: "pi:glm-5.3 (실행 중)".to_string(),
            short_model: "pi:glm-5.3".to_string(),
            model_name: "glm-zcode/glm-5.3".to_string(),
            address: addr.to_string(),
            name: name.to_string(),
            cwd: cwd.map(str::to_string),
            repo_tag: repo.to_string(),
            activity: "테스트 활동 줄".to_string(),
            status: Some("working".to_string()),
        }
    }

    fn snapshot_fixture() -> DashboardSnapshot {
        let mut repo_stats = BTreeMap::new();
        repo_stats.insert(
            "alpha".to_string(),
            RepoStatsBundle {
                project_id: Some("proj_alpha".to_string()),
                routes: vec![RouteMetric {
                    id: "file_select".to_string(),
                    name: "어느 파일".to_string(),
                    p: 0.9,
                    basis: "measured".to_string(),
                    samples: 9,
                    forced_split: false,
                    decision: DecisionLabel::Sharp,
                }],
                cost: None,
                timing: None,
                shadow: None,
            },
        );
        DashboardSnapshot {
            updated_at_label: "12:00:00".to_string(),
            profile_name: "quota-saver".to_string(),
            roster_source: "builtin".to_string(),
            roles: vec![],
            workers: vec![
                worker("addr-a", "w-alpha-1", "alpha", Some("/repos/alpha")),
                worker("addr-b", "w-beta-1", "beta", Some("/repos/beta")),
            ],
            agy_live_count: 0,
            detected_repos: vec!["alpha".to_string(), "beta".to_string()],
            routes: vec![RouteMetric {
                id: "file_select".to_string(),
                name: "어느 파일".to_string(),
                p: 0.8,
                basis: "prior".to_string(),
                samples: 0,
                forced_split: false,
                decision: DecisionLabel::Sharp,
            }],
            cost_summary: None,
            timing_summary: None,
            shadow_summary: None,
            repo_stats,
            recent_log: vec![],
            db_status: "연결됨".to_string(),
            hc_available: true,
        }
    }

    // ---------- 레포 태그·탭 ----------

    #[test]
    fn repo_root_strips_worktree_marker() {
        assert_eq!(
            repo_root_from_cwd(Some("/a/b/my-repo/.herdr/worktrees/coder1")),
            Some(PathBuf::from("/a/b/my-repo"))
        );
        assert_eq!(
            repo_root_from_cwd(Some("/a/b/my-repo")),
            Some(PathBuf::from("/a/b/my-repo"))
        );
        assert_eq!(repo_root_from_cwd(None), None);
        assert_eq!(repo_root_from_cwd(Some("")), None);
    }

    #[test]
    fn detected_repos_dedup_and_sort_with_extra_dir() {
        let workers = vec![
            worker("a", "w-1", "zeta", None),
            worker("b", "w-2", "alpha", None),
            worker("c", "w-3", "zeta", None),
        ];
        let repos = build_detected_repos(&workers, Some(Path::new("/tmp/alpha")));
        assert_eq!(repos, vec!["alpha".to_string(), "zeta".to_string()]);
        let repos_none = build_detected_repos(&[], None);
        assert!(repos_none.is_empty());
    }

    #[test]
    fn filter_by_repo_global_vs_specific() {
        let snap = snapshot_fixture();
        let global = filter_snapshot_by_repo(&snap, None);
        assert_eq!(global.workers.len(), 2);
        assert_eq!(global.routes[0].basis, "prior");

        let alpha = filter_snapshot_by_repo(&snap, Some("alpha"));
        assert_eq!(alpha.workers.len(), 1);
        assert_eq!(alpha.workers[0].repo_tag, "alpha");
        assert_eq!(alpha.routes[0].basis, "measured");
        assert_eq!(alpha.routes[0].samples, 9);
        // 탭 바 목록은 필터 후에도 전체를 유지해야 한다
        assert_eq!(alpha.detected_repos.len(), 2);

        // 번들 없는 레포는 전역 값 유지(위장 조회 금지)
        let beta = filter_snapshot_by_repo(&snap, Some("beta"));
        assert_eq!(beta.workers.len(), 1);
        assert_eq!(beta.routes[0].basis, "prior");
        // 빈 문자열 = 전체
        assert_eq!(filter_snapshot_by_repo(&snap, Some("")).workers.len(), 2);
    }

    // ---------- 세션 기록 (RecentLogEntry 읽기전용 조회) ----------

    #[test]
    fn recent_log_fetch_orders_limits_redacts_and_maps_roles() {
        let tmp = temp_dir("recent-log");
        let db_path = tmp.join("decision.db");
        {
            use dgate_core::db::open_db;
            let conn = open_db(db_path.to_str(), OpenDbOptions::default()).unwrap();
            for i in 0..10 {
                conn.execute(
                    "INSERT INTO decision_events
                       (project_id, source, route, tool_name, tool_class, ok, weight, is_retry, created_at_ms, error_snippet)
                     VALUES (?1, ?2, ?3, ?4, 'shell_exec', ?5, 1.0, 0, ?6, ?7)",
                    rusqlite::params![
                        "proj-log",
                        if i % 3 == 0 { "neon_backprop" } else { "wrap" },
                        if i % 2 == 0 { "delegate_swarm" } else { "tool_select" },
                        format!("tool-{}", i),
                        if i == 9 { 0 } else { 1 },
                        1_700_000_000_000i64 + i as i64 * 1000,
                        if i == 9 {
                            Some(" Authorization: Bearer sk-test1234567890abcdef".to_string())
                        } else {
                            None
                        },
                    ],
                )
                .unwrap();
            }
        }
        // 읽기전용으로만 다시 연다 — fetch는 조회만 한다
        let ro = open_db(
            Some(db_path.to_str().unwrap()),
            OpenDbOptions { read_only: true, busy_timeout_ms: Some(500) },
        )
        .unwrap();
        let entries = fetch_recent_log(&ro, 6);
        assert_eq!(entries.len(), 6, "limit 존중");
        // 최신순 — 첫 줄이 가장 최근 이벤트(i=9, 실패 + 시크릿 스니펫)
        assert!(entries[0].message.contains("실패"));
        assert!(entries[0].message.contains("<redacted>"), "Bearer 토큰 리댁션");
        assert!(!entries[0].message.contains("sk-test1234567890abcdef"));
        assert!(entries[0].time_hhmmss.len() == 8);
        // 역할 매핑 — neon_backprop→Verifier, delegate_swarm→Worker, wrap+tool_select→Decision Gate
        assert!(entries.iter().any(|e| e.source_role == "Verifier"));
        assert!(entries.iter().any(|e| e.source_role == "Worker"));
        assert!(entries.iter().any(|e| e.source_role == "Decision Gate"));
        assert!(entries.iter().all(|e| e.project_id.as_deref() == Some("proj-log")));
        // 존재하지 않는 테이블(빈 DB)·limit 0 — 패닉 없이 안전 폴백
        let empty_db = tmp.join("empty.db");
        let w = open_db(empty_db.to_str(), OpenDbOptions::default()).unwrap();
        drop(w);
        let ro2 = open_db(
            Some(empty_db.to_str().unwrap()),
            OpenDbOptions { read_only: true, busy_timeout_ms: None },
        )
        .unwrap();
        assert_eq!(fetch_recent_log(&ro2, 6).len(), 0);
        assert_eq!(fetch_recent_log(&ro, 0).len(), 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn filter_by_repo_also_filters_recent_log() {
        let mut snap = snapshot_fixture();
        snap.recent_log = vec![
            RecentLogEntry {
                time_hhmmss: "01:00:00".to_string(),
                source: "wrap".to_string(),
                source_role: "Decision Gate".to_string(),
                message: "alpha 이벤트".to_string(),
                project_id: Some("proj_alpha".to_string()),
            },
            RecentLogEntry {
                time_hhmmss: "01:00:01".to_string(),
                source: "wrap".to_string(),
                source_role: "Worker".to_string(),
                message: "기타 이벤트".to_string(),
                project_id: Some("proj_other".to_string()),
            },
        ];
        let alpha = filter_snapshot_by_repo(&snap, Some("alpha"));
        assert_eq!(alpha.recent_log.len(), 1);
        assert_eq!(alpha.recent_log[0].message, "alpha 이벤트");
        // 전체 뷰는 로그를 그대로 유지
        assert_eq!(filter_snapshot_by_repo(&snap, None).recent_log.len(), 2);
    }

    // ---------- 동적 N스템 · 라이브 모션 활동 판정 ----------

    #[test]
    fn visible_worker_columns_matches_topology() {
        // 라이브 0 — 3슬롯 청사진(침묵 3스템 와이어)
        assert_eq!(visible_worker_columns(0), 3);
        // 1/2/3 — 카드 수 = 스템 수 (고정 3스템 매달림 금지)
        assert_eq!(visible_worker_columns(1), 1);
        assert_eq!(visible_worker_columns(2), 2);
        assert_eq!(visible_worker_columns(3), 3);
        // 4+ — 최대 4열, 초과분 2행 랩
        assert_eq!(visible_worker_columns(4), 4);
        assert_eq!(visible_worker_columns(6), 4);
    }

    #[test]
    fn has_active_work_idle_vs_running_vs_demo() {
        let mut snap = DashboardSnapshot::default();
        // 라이브 유휴(실행 0) — 반드시 false → 제로-첸 유휴 불변식
        assert!(!has_active_work(&snap, false));
        // DEMO 모드는 항상 활동
        assert!(has_active_work(&snap, true));

        // 실행 중 워커 1명 → 활동
        snap.workers.push(worker("addr-a", "w-1", "alpha", None));
        assert!(has_active_work(&snap, false));

        // 완료(done)만 남은 로스터는 활동 아님 — 침착한 유휴 유지
        snap.workers[0].status = Some("done".to_string());
        assert!(!has_active_work(&snap, false));

        // status 미상(None) 세션은 실행 중으로 본다(카드 '실행 중' 표기와 동일 기준)
        snap.workers[0].status = None;
        assert!(has_active_work(&snap, false));
    }

    // ---------- 2단계 중단 상태 기계 ----------

    /// 호출 기록 mock 실행기.
    struct RecordingKill {
        calls: StdMutex<Vec<String>>,
        result: Result<String, String>,
    }
    impl KillExecutor for RecordingKill {
        fn kill(&self, hc_bin: &str, address: &str) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{} kill {}", hc_bin, address));
            self.result.clone()
        }
    }

    #[test]
    fn controller_two_step_kill_state_machine() {
        let mut c = DashboardController::new(false);
        c.apply_snapshot(snapshot_fixture());
        assert!(c.control_enabled == false);
        // 제어 꺼짐 → 1단계 진입 불가
        assert!(!c.request_kill("addr-a"));
        assert!(c.confirm_kill_address.is_none());
        // 제어 허용 → 1단계 진입
        c.toggle_control();
        assert!(c.control_enabled);
        assert!(c.request_kill("addr-a"));
        assert_eq!(c.confirm_kill_address.as_deref(), Some("addr-a"));
        // 알 수 없는 주소 거부
        assert!(!c.request_kill("unknown-addr"));
        // 2단계 확인 → 주소 반환 후 상태 해제
        assert_eq!(c.confirm_kill().as_deref(), Some("addr-a"));
        assert!(c.confirm_kill_address.is_none());
        // 취소 경로
        assert!(c.request_kill("addr-a"));
        c.cancel_kill();
        assert!(c.confirm_kill_address.is_none());
        assert!(c.confirm_kill().is_none());
    }

    #[test]
    fn controller_watch_only_locks_everything() {
        let mut c = DashboardController::new(true);
        c.apply_snapshot(snapshot_fixture());
        c.toggle_control();
        assert!(!c.control_enabled, "--watch-only는 제어 허용 불가");
        assert!(!c.request_kill("addr-a"));
        assert!(c.confirm_kill_address.is_none());
    }

    #[test]
    fn controller_confirm_rejects_vanished_worker() {
        let mut c = DashboardController::new(false);
        c.apply_snapshot(snapshot_fixture());
        c.toggle_control();
        assert!(c.request_kill("addr-a"));
        // 스냅숏 갱신으로 워커가 사라지면 확인 상태 자동 해제
        let mut snap2 = snapshot_fixture();
        snap2.workers.clear();
        c.apply_snapshot(snap2);
        assert!(c.confirm_kill_address.is_none());
        assert!(c.confirm_kill().is_none());
    }

    #[test]
    fn controller_selected_repo_resets_when_repo_disappears() {
        let mut c = DashboardController::new(false);
        c.apply_snapshot(snapshot_fixture());
        c.select_repo(Some("alpha"));
        assert_eq!(c.selected_repo.as_deref(), Some("alpha"));
        let mut snap2 = snapshot_fixture();
        snap2.detected_repos = vec!["beta".to_string()];
        c.apply_snapshot(snap2);
        assert_eq!(c.selected_repo, None, "사라진 레포 탭 선택은 전체로 복귀");
        // 전체 탭 선택
        c.select_repo(None);
        assert_eq!(c.selected_repo, None);
        c.select_repo(Some("  "));
        assert_eq!(c.selected_repo, None);
    }

    // ---------- DEMO 모드 (LIVE/DEMO 토글 · --demo 기동) ----------

    #[test]
    fn controller_toggle_demo_mode_swaps_filtered_snapshot() {
        let mut c = DashboardController::new(false);
        c.apply_snapshot(snapshot_fixture());
        assert!(!c.demo_mode);
        assert_eq!(c.filtered().workers.len(), 2, "기본은 LIVE 스냅숏");

        c.toggle_demo_mode();
        assert!(c.demo_mode);
        let demo = c.filtered();
        assert_eq!(demo.profile_name, "demo");
        assert_eq!(demo.workers.len(), 5, "DEMO는 5역할 카드를 모두 제시");
        assert_eq!(demo.detected_repos, vec!["demo-app".to_string()]);
        assert!(demo.db_status.contains("DEMO"));
        assert!(!demo.recent_log.is_empty(), "DEMO에도 세션 기록 미리보기가 있다");

        c.toggle_demo_mode();
        assert!(!c.demo_mode);
        assert_eq!(c.filtered().workers.len(), 2, "LIVE 복귀 시 실제 스냅숏 재개");
    }

    #[test]
    fn demo_snapshot_shape_is_realistic_topology() {
        let s = demo_snapshot();
        // 4라우트 — sharp/split 혼합, 전부 실측 표기
        assert_eq!(s.routes.len(), 4);
        assert!(s.routes.iter().any(|r| r.decision == DecisionLabel::Sharp));
        assert!(s.routes.iter().any(|r| r.decision == DecisionLabel::Split));
        assert!(s.routes.iter().all(|r| r.basis == "measured" && r.samples > 0));
        // 5역할 로스터 전부 실행 중 + 비용/시간/shadow 요약 존재
        assert_eq!(s.roles.len(), 5);
        assert!(s.roles.iter().all(|r| r.running));
        assert!(s.cost_summary.is_some());
        assert!(s.timing_summary.is_some());
        assert!(s.shadow_summary.is_some());
        // 세션 기록 역할 태그는 TUI 5색 역할 안에서만
        let known = ["Decision Gate", "Worker", "Verifier", "Advisor", "Lead"];
        assert!(s.recent_log.iter().all(|e| known.contains(&e.source_role.as_str())));
    }

    #[test]
    fn controller_kill_blocked_in_demo_mode() {
        let mut c = DashboardController::new(false);
        c.apply_snapshot(snapshot_fixture());
        c.toggle_control();
        assert!(c.control_enabled);
        // DEMO 진입 → 라이브에 있는 주소라도 중단 1단계 진입 불가
        c.toggle_demo_mode();
        assert!(!c.request_kill("addr-a"), "DEMO 모드에서는 중단 요청 거부");
        assert!(c.confirm_kill_address.is_none());
        assert!(c.last_action_message.as_deref().unwrap().contains("DEMO"));
        // LIVE 복귀 → 동일 주소가 다시 허용된다
        c.toggle_demo_mode();
        assert!(c.request_kill("addr-a"));
        // DEMO 재진입 시 확인 상태도 해제된다(가상 주소 잔류 방지)
        assert!(c.request_kill("addr-a"));
        c.toggle_demo_mode();
        assert!(c.confirm_kill_address.is_none());
    }

    #[test]
    fn kill_worker_safe_rejects_unknown_monitor_and_arbitrary() {
        let known = vec![
            worker("addr-a", "w-1", "alpha", None),
            LiveWorkerInfo { name: "dg-monitor".into(), ..worker("addr-mon", "dg-monitor", "x", None) },
            LiveWorkerInfo { name: "mon-9".into(), ..worker("addr-mon2", "mon-9", "x", None) },
        ];
        let exec = RecordingKill { calls: StdMutex::new(vec![]), result: Ok("killed".into()) };

        // 알려진 워커만 통과
        assert!(kill_worker_safe("addr-a", &known, "hc", &exec).is_ok());
        assert_eq!(exec.calls.lock().unwrap().len(), 1);
        assert_eq!(exec.calls.lock().unwrap()[0], "hc kill addr-a");

        // 모니터·셸·임의 문자열·빈 주소·목록 밖 주소 전부 거부
        assert!(kill_worker_safe("addr-mon", &known, "hc", &exec).is_err());
        assert!(kill_worker_safe("addr-mon2", &known, "hc", &exec).is_err());
        assert!(kill_worker_safe("dg-monitor", &known, "hc", &exec).is_err());
        assert!(kill_worker_safe("random-string", &known, "hc", &exec).is_err());
        assert!(kill_worker_safe("", &known, "hc", &exec).is_err());
        assert!(kill_worker_safe("sudo rm -rf /", &known, "hc", &exec).is_err());
        // 거부된 호출은 실행기에 도달하지 않는다
        assert_eq!(exec.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn kill_worker_safe_propagates_executor_failure() {
        let known = vec![worker("addr-a", "w-1", "alpha", None)];
        let exec = RecordingKill { calls: StdMutex::new(vec![]), result: Err("exit 1".into()) };
        let res = kill_worker_safe("addr-a", &known, "hc", &exec);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("exit 1"));
    }

    // ---------- 시간 라벨 ----------

    #[test]
    fn local_time_label_shape() {
        let label = format_local_time_label(0);
        assert_eq!(label.len(), 8, "HH:MM:SS 형태여야 한다: {}", label);
        let parts: Vec<&str> = label.split(':').collect();
        assert_eq!(parts.len(), 3);
        for p in parts {
            assert!(p.parse::<u32>().is_ok(), "숫자 세그먼트: {}", p);
        }
    }

    // ---------- 실제 임시 v2 DB + roster.json 스냅숏 로딩 ----------

    /// 테스트용 임시 디렉터리(고유) — 생성까지 담당.
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dgate-gui-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 고정 응답 mock `hc` 실행기 — ls/get/read만 응답하고 나머지는 버전으로.
    struct MockHc {
        terminals_json: String,
        get_json: String,
        read_screen: String,
    }
    impl dgate_core::live_sync::HcExecRunner for MockHc {
        fn exec(&self, _file: &str, args: &[&str], _timeout_ms: u64) -> Result<String, String> {
            match args.first() {
                Some(&"ls") => Ok(self.terminals_json.clone()),
                Some(&"get") => Ok(self.get_json.clone()),
                Some(&"read") => Ok(self.read_screen.clone()),
                _ => Ok("hc-mock 1.0".to_string()),
            }
        }
    }

    fn mock_sync(mock: MockHc, repo_dir: &Path) -> LiveSyncSnapshotter {
        LiveSyncSnapshotter::new(LiveSyncOptions {
            exec_runner: Arc::new(mock),
            ..LiveSyncOptions::new(repo_dir.to_path_buf())
        })
    }

    #[test]
    fn poll_once_builds_snapshot_from_real_db_and_roster() {
        let tmp = temp_dir("snapshot-real");
        let repo_dir = tmp.join("my-gui-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let repo_str = repo_dir.to_str().unwrap().to_string();

        // 실제 v2 DB 생성(writer open은 스키마 생성·마이그레이션까지 수행)
        let db_path = tmp.join("decision.db");
        let pid = resolve_project_identity(&repo_dir).project_id;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        {
            use dgate_core::db::open_db;
            let conn = open_db(db_path.to_str(), OpenDbOptions::default()).unwrap();
            // 라우트 사후확률용 표본 — file_select/file_read 8건 성공 7
            conn.execute(
                "INSERT INTO stats_summary
                   (project_id, project_label, route, tool_class, ewma_success, ewma_fail,
                    raw_samples, consecutive_errors, updated_at_ms)
                 VALUES (?1, 'my-gui-repo', 'file_select', 'file_read', 7.0, 1.0, 8, 0, ?2)",
                rusqlite::params![pid, now],
            )
            .unwrap();
            // 비용 — reported $1.25 + unknown 1건 (불변식 C-1 유도). 리포트 기본
            // 14일 윈도우 안에 들어오도록 최근 시각을 쓴다.
            conn.execute(
                "INSERT INTO decision_events
                   (project_id, source, route, tool_name, tool_class, ok, weight, is_retry, created_at_ms,
                    input_tokens, output_tokens, cost_usd, cost_status)
                 VALUES (?1, 'wrap', 'tool_select', 'x', 'shell_exec', 1, 1.0, 0, ?2, 100000, 5000, 1.25, 'reported'),
                        (?1, 'wrap', 'tool_select', 'y', 'shell_exec', 1, 1.0, 0, ?2, NULL, NULL, NULL, 'unknown')",
                rusqlite::params![pid, now - 1000],
            )
            .unwrap();
            // shadow API 감사 메타데이터 — shadow 불일치 1건 (윈도우 내 최근 시각)
            conn.execute(
                "INSERT INTO forks
                   (fork_id, project_id, project_label, route, tool_class, decision, forced_split,
                    p_at_gate, basis_at_gate, status, closed_by, ttl_ms, created_at_ms, last_touch_ms,
                    closed_at_ms, metadata_json)
                 VALUES ('fk-gui-1', ?1, 'my-gui-repo', 'tool_select', 'shell_exec', 'sharp', 0,
                         0.9, 'measured', 'ok', 'dispatcher', 600000, ?2, ?3, ?4,
                         '{\"decision_api\":{\"ok\":1,\"mode\":\"shadow\",\"applied\":0,\"disagreed\":1,\"input_tokens\":200,\"cost_usd\":0.00002}}')",
                rusqlite::params![pid, now - 2000, now - 1500, now - 1000],
            )
            .unwrap();
        }

        // 임시 roster.json — gui-test 프로필(coder만 오버라이드)
        let roster_path = tmp.join("roster.json");
        std::fs::write(
            &roster_path,
            serde_json::json!({
                "defaultProfile": "gui-test",
                "profiles": {
                    "gui-test": {
                        "roles": {
                            "coder": { "cli": "pi", "model": "test/model-x" }
                        }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let mock = MockHc {
            terminals_json: format!(
                "{{\"terminals\":[{{\"address\":\"addr-w1\",\"agent\":\"pi\",\"name\":\"w-coder-1\",\"parent\":\"lead\",\"state\":\"live\",\"status\":\"working\",\"cwd\":\"{}\"}}]}}",
                repo_str
            ),
            get_json: format!(
                "{{\"address\":\"addr-w1\",\"agent\":\"pi\",\"name\":\"w-coder-1\",\"cwd\":\"{}\",\"state\":\"live\",\"status\":\"working\",\"argv\":[\"env\",\"HERDR_ROLE=coder\",\"pi\",\"--model\",\"glm-zcode/glm-5.3\"]}}",
                repo_str
            ),
            read_screen: "의미 있는 활동 줄\n[hc] idle".to_string(),
        };
        let mut snapshotter = mock_sync(mock, &repo_dir);

        let cfg = PollerConfig {
            db_path: Some(db_path.to_string_lossy().into_owned()),
            repo_dir: None,
            roster_path: Some(roster_path.to_string_lossy().into_owned()),
            poll_ms: 2000,
            hc_bin: "hc".to_string(),
        };
        let snap = poll_once(&cfg, &mut snapshotter);

        // 프로필·로스터
        assert_eq!(snap.profile_name, "gui-test");
        assert_eq!(snap.roster_source, "flag");
        assert_eq!(snap.roles.len(), 5);
        let coder = snap.roles.iter().find(|r| r.slot == "coder").unwrap();
        assert!(coder.running, "live 워커가 coder 슬롯을 점유");
        assert_eq!(coder.live_model.as_deref(), Some("pi:glm-5.3"));
        assert_eq!(coder.standby_model, "pi:model-x");
        let explore = snap.roles.iter().find(|r| r.slot == "explore").unwrap();
        assert!(!explore.running);
        assert!(explore.readonly, "내장 quota-saver explore는 readonly 상속");

        // 워커·레포 탭
        assert_eq!(snap.workers.len(), 1);
        assert_eq!(snap.workers[0].repo_tag, "my-gui-repo");
        assert_eq!(snap.workers[0].activity, "의미 있는 활동 줄");
        assert_eq!(snap.detected_repos, vec!["my-gui-repo".to_string()]);
        assert_eq!(snap.agy_live_count, 0);
        assert_eq!(snap.db_status, "연결됨");

        // 레포 번들 — 실측 라우트 + 비용 + shadow
        let bundle = snap.repo_stats.get("my-gui-repo").unwrap();
        assert_eq!(bundle.project_id.as_deref(), Some(pid.as_str()));
        let file_route = bundle.routes.iter().find(|r| r.id == "file_select").unwrap();
        assert_eq!(file_route.basis, "measured");
        assert_eq!(file_route.samples, 8);
        assert_eq!(file_route.decision, DecisionLabel::Sharp);
        let cost = bundle.cost.as_ref().unwrap();
        assert!((cost.known_usd - 1.25).abs() < 1e-9);
        assert_eq!(cost.unknown_events, 1);
        assert_eq!(cost.total_usd_display, None, "불변식 C-1: unknown>0 → 총액 None");
        assert_eq!(cost.tokens.input, 100000);
        let shadow = bundle.shadow.as_ref().unwrap();
        assert_eq!(shadow.total, 1);
        assert_eq!(shadow.shadow_count, 1);
        assert_eq!(shadow.disagreed, 1);

        // 전체(전역) 뷰 — 가중 집계 라우트 + 전역 비용/shadow
        let global_route = snap.routes.iter().find(|r| r.id == "file_select").unwrap();
        assert_eq!(global_route.basis, "measured");
        assert_eq!(global_route.samples, 8);
        let gcost = snap.cost_summary.as_ref().unwrap();
        assert!((gcost.known_usd - 1.25).abs() < 1e-9);
        assert_eq!(gcost.unknown_events, 1);
        assert_eq!(snap.shadow_summary.as_ref().unwrap().disagreed, 1);
        // 세션 기록 — 삽입한 decision_events 2건이 최신순으로 조회된다
        assert_eq!(snap.recent_log.len(), 2);
        assert!(snap.recent_log.iter().all(|e| e.source == "wrap"));
        assert!(snap.recent_log.iter().all(|e| e.source_role == "Decision Gate"));
        assert!(snap.recent_log.iter().all(|e| !e.message.is_empty()));

        // 레포 필터링 연결 확인 — filtered 뷰도 동일 번들로 교체된다
        let filtered = filter_snapshot_by_repo(&snap, Some("my-gui-repo"));
        assert_eq!(filtered.workers.len(), 1);
        assert_eq!(filtered.routes[0].id, snap.routes[0].id);
        assert_eq!(filtered.cost_summary.as_ref().unwrap().unknown_events, 1);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn poll_once_graceful_when_db_missing() {
        let tmp = temp_dir("snapshot-missing");
        let repo_dir = tmp.join("idle-repo");
        std::fs::create_dir_all(&repo_dir).unwrap();

        let mock = MockHc {
            terminals_json: "{\"terminals\":[]}".to_string(),
            get_json: "{}".to_string(),
            read_screen: String::new(),
        };
        let mut snapshotter = mock_sync(mock, &repo_dir);
        let cfg = PollerConfig {
            db_path: Some(tmp.join("nope.db").to_string_lossy().into_owned()),
            repo_dir: None,
            roster_path: Some(tmp.join("no-roster.json").to_string_lossy().into_owned()),
            poll_ms: 2000,
            hc_bin: "hc".to_string(),
        };

        // DB·roster 파일 부재에도 절대 패닉하지 않는다
        let snap = poll_once(&cfg, &mut snapshotter);
        assert_eq!(snap.db_status, "DB 없음 (대기)");
        assert!(snap.workers.is_empty());
        assert!(snap.detected_repos.is_empty());
        assert!(snap.repo_stats.is_empty());
        assert_eq!(snap.routes.len(), 4, "휴리스틱 prior 폴백 4라우트");
        for r in &snap.routes {
            assert_eq!(r.basis, "prior");
            assert_eq!(r.samples, 0);
        }
        assert!(snap.cost_summary.is_none());
        assert!(snap.timing_summary.is_none());
        assert!(snap.shadow_summary.is_none());
        // roster 파일 부재 시 전역(~/.config/herdr-quad/roster.json) 또는 내장으로
        // 폴백한다 — 기계 의존 없는 계약은 '비어 있지 않은 프로필로 폴백'이다.
        assert!(!snap.profile_name.is_empty(), "폴백 프로필명은 항상 존재");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn spawn_poller_publishes_snapshot_and_shuts_down_cleanly() {
        let tmp = temp_dir("poller");
        let cfg = PollerConfig {
            db_path: Some(tmp.join("nope.db").to_string_lossy().into_owned()),
            repo_dir: None,
            roster_path: Some(tmp.join("no-roster.json").to_string_lossy().into_owned()),
            poll_ms: 100,
            hc_bin: "hc".to_string(),
        };
        let (handle, join) = spawn_poller(cfg);

        // 첫 스냅숏이 수 초 내 발행되는지(실hc 환경과 무관하게 항상 발행)
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut published = false;
        while std::time::Instant::now() < deadline {
            let (version, snap) = handle.shared.take();
            if version >= 1 && snap.is_some() {
                published = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(published, "폴러가 10초 내 첫 스냅숏을 발행해야 한다");

        // 일시정지 플래그 전환 → 웨이크 → 종료가 교착 없이 반환
        handle.set_paused(true);
        join.shutdown();
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
