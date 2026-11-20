//! 활성 `hc` 워커 터미널 로스터 실시간 동기화 — `src/backend/liveWorkerSync.ts`
//! (Live TUI Sync v2, CLI·GPUI 공유)의 Rust 포팅.
//!
//! 목적: `--watch` 관측 TUI가 roster.json 대기(standby) 라벨 대신 **지금 실제로 돌고
//! 있는 `hc` 워커 세션**(pi/devin/claude/codex/gemini)의 모델·레포·1줄 활동을 보여주게
//! 한다. roster.json이 바뀌면 mtime 감지로 핫리로드하고, `hc` 부재·오류는 조용히
//! 폴백해 관제 화면이 죽지 않는다(§3.5 안전 폴백 계약 준수).
//!
//! 외부 연결:
//! - `hc ls --json`(터미널 목록) · `hc get <addr> --json`(argv 상세) ·
//!   `hc read <addr> --lines 4`(화면 마지막 줄) 세 명령만 쓴다(전부 읽기 전용).
//! - 라벨 조립은 Wave 1A `roster`의 `load_roster_config`·`resolve_roster_profile`,
//!   활동 줄 정화는 `redact`의 `redact_secrets`를 재사용한다(단일 구현 원칙).
//!
//! 주의:
//! - 모든 `hc` 호출은 타임아웃을 붙인 실행이며 실패 시 해당 틱을 조용히 건너뛴다.
//! - 터미널 화면 원문에는 시크릿이 섞일 수 있으므로 활동 줄 추출 시 반드시
//!   `redact_secrets`를 경유한다(절단 이전 적용 — redact 계약).
//! - Rust 포팅은 비동기 런타임 없이 동기 `tick_now()` 주기 폴링 계약으로 제공한다 —
//!   호출자(CLI/GPUI)가 자체 타이머로 [`LiveSyncSnapshotter::tick_now`]를 주기 호출한다.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::redact::redact_secrets;
use crate::roster::{
    global_roster_path, load_roster_config, resolve_roster_profile, RoleSlot, RosterProfile,
};

/// `hc ls --json` 타임아웃 — 목록 조회는 짧게 끊는다.
pub const HC_LS_TIMEOUT_MS: u64 = 1500;
/// `hc read` 타임아웃 — 화면 캡처는 더 짧게 끊는다.
pub const HC_READ_TIMEOUT_MS: u64 = 800;
/// 기본 동기화 주기.
pub const DEFAULT_SYNC_INTERVAL_MS: u64 = 2000;
/// 틱당 `hc read` 활동 캡처 상한 — 터미널 많아도 TUI가 느려지지 않게 한다.
pub const MAX_ACTIVITY_READS_PER_TICK: usize = 6;
/// `hc` 실행 stdout 상한(4MB — TS maxBuffer와 동일).
const HC_MAX_BUFFER_BYTES: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------------
// 형태
// ---------------------------------------------------------------------------

/// `hc ls --json` 터미널 항목(요약).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HcTerminalSummary {
    pub address: String,
    pub agent: Option<String>,
    pub name: Option<String>,
    pub parent: Option<String>,
    pub state: Option<String>,
    pub status: Option<String>,
    pub cwd: Option<String>,
}

impl HcTerminalSummary {
    /// 객체형 status(`{app,state}`)도 문자열로 정규화하는 방어적 파서.
    fn from_json(raw: &serde_json::Map<String, Value>) -> Option<Self> {
        let address = raw.get("address").and_then(Value::as_str)?;
        if address.is_empty() {
            return None;
        }
        let status = match raw.get("status") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(v) if v.is_object() => v
                .get("state")
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
        };
        Some(HcTerminalSummary {
            address: address.to_string(),
            agent: raw.get("agent").and_then(Value::as_str).map(str::to_string),
            name: raw.get("name").and_then(Value::as_str).map(str::to_string),
            parent: raw.get("parent").and_then(Value::as_str).map(str::to_string),
            state: raw.get("state").and_then(Value::as_str).map(str::to_string),
            status,
            cwd: raw.get("cwd").and_then(Value::as_str).map(str::to_string),
        })
    }
}

/// `hc get <addr> --json` 터미널 항목(상세) — argv 포함.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HcTerminalDetail {
    pub address: String,
    pub agent: Option<String>,
    pub name: Option<String>,
    pub parent: Option<String>,
    pub state: Option<String>,
    pub status: Option<String>,
    pub cwd: Option<String>,
    pub argv: Option<Vec<String>>,
}

impl HcTerminalDetail {
    fn from_summary(s: &HcTerminalSummary) -> Self {
        HcTerminalDetail {
            address: s.address.clone(),
            agent: s.agent.clone(),
            name: s.name.clone(),
            parent: s.parent.clone(),
            state: s.state.clone(),
            status: s.status.clone(),
            cwd: s.cwd.clone(),
            argv: None,
        }
    }
}

/// 역할 슬롯 — roster RoleSlot과 동일한 5종.
pub type LiveWorkerSlot = RoleSlot;

/// TUI 표시용 roster 라벨 묶음 — TS `RosterDisplayLabels`.
#[derive(Debug, Clone, PartialEq)]
pub struct RosterDisplayLabels {
    pub profile_name: String,
    pub roster_source: String,
    pub lead: String,
    pub explore: String,
    pub coder: String,
    pub verifier: String,
    pub advisor: String,
}

impl RosterDisplayLabels {
    pub fn get_slot(&self, slot: &LiveWorkerSlot) -> &String {
        match slot {
            RoleSlot::Lead => &self.lead,
            RoleSlot::Explore => &self.explore,
            RoleSlot::Coder => &self.coder,
            RoleSlot::Verifier => &self.verifier,
            RoleSlot::Advisor => &self.advisor,
        }
    }

    pub fn set_slot(&mut self, slot: &LiveWorkerSlot, value: String) {
        match slot {
            RoleSlot::Lead => self.lead = value,
            RoleSlot::Explore => self.explore = value,
            RoleSlot::Coder => self.coder = value,
            RoleSlot::Verifier => self.verifier = value,
            RoleSlot::Advisor => self.advisor = value,
        }
    }
}

/// 현재 실행 중인 워커 1개의 TUI 표시 정보.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveWorkerInfo {
    pub slot: LiveWorkerSlot,
    /// 예: 'pi:glm-5.3 (실행 중)' · 'devin (실행 중)'
    pub badge: String,
    /// 예: 'pi:glm-5.3' · 'devin'
    pub short_model: String,
    /// 예: 'glm-zcode/glm-5.3' · 'devin' (원문 모델명, 없으면 CLI명)
    pub model_name: String,
    pub address: String,
    pub name: String,
    pub cwd: Option<String>,
    /// 레포 basename — `.herdr/worktrees/<wt>`는 소속 레포명으로 환원
    pub repo_tag: String,
    /// `hc read`에서 뽑은 정화된 1줄 활동 (없으면 간결 폴백)
    pub activity: String,
    /// `hc` 터미널 상태('working'·'done' 등) — 카드 완료 표시·완료 로그 판정용(선택).
    pub status: Option<String>,
}

/// `merge_live_roster_labels` 결과 — AppState에 반영할 값의 묶음.
#[derive(Debug, Clone, PartialEq)]
pub struct MergedLiveRoster {
    pub labels: RosterDisplayLabels,
    pub lead_model_name: String,
    pub lead_model_flag: String,
    pub verifier_model_name: String,
    pub verifier_model_flag: String,
    pub workers: Vec<LiveWorkerInfo>,
    pub active_worker_count: usize,
}

// ---------------------------------------------------------------------------
// 워커 터미널 판정
// ---------------------------------------------------------------------------

/// 제외할 셸·비워커 에이전트 (antigravity는 리드 관측 대상이지 워커가 아니다).
const SHELL_AGENTS: [&str; 9] = [
    "zsh", "bash", "fish", "sh", "ksh", "dash", "pwsh", "powershell.exe", "antigravity",
];

/// parent 매칭 판정에 쓰는 워커 CLI 에이전트 목록.
const PARENT_MATCH_AGENTS: [&str; 5] = ["pi", "devin", "claude", "codex", "gemini"];

/// 워커 세션명 접두 — herdr-quad-launch/dispatch가 만드는 이름 규약.
const WORKER_NAME_PREFIXES: [&str; 9] = [
    "w-", "exp-", "explore-", "coder", "verify-", "verifier-", "lead-", "advisor-", "carbon-",
];

fn re_monitor_name() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^dg-monitor(-|$)").unwrap())
}

/// `hc ls` 터미널이 우리가 표시할 "워커"인지 판정:
/// ① `state === 'live'` 필수 ② 모니터(`dg-monitor*`·`mon-*`) 제외 ③ 셸·antigravity 제외
/// ④ 이름 접두(`w-`·`exp-`·`coder`·`verify-`…) 매칭 또는 parent 존재 + 워커 CLI 에이전트.
pub fn is_worker_terminal(t: &HcTerminalSummary) -> bool {
    if t.state.as_deref() != Some("live") {
        return false;
    }
    let name = t.name.as_deref().unwrap_or("");
    if re_monitor_name().is_match(name) || name.starts_with("mon-") {
        return false;
    }
    let agent = t.agent.as_deref().unwrap_or("").to_lowercase();
    if SHELL_AGENTS.contains(&agent.as_str()) {
        return false;
    }
    if !name.is_empty() && WORKER_NAME_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return true;
    }
    if t.parent.is_some() && !agent.is_empty() && PARENT_MATCH_AGENTS.contains(&agent.as_str()) {
        return true;
    }
    false
}

/// 최상위 활성 AI 에이전트 세션으로 인정하는 에이전트 집합 — 관측 대상 확장용.
/// 셸(zsh/bash/…)·python3·dgate-gui는 절대 포함하지 않는다.
const TOP_LEVEL_AI_AGENTS: [&str; 7] = ["antigravity", "pi", "claude", "codex", "devin", "gemini", "agy"];

/// 모니터·GUI 전용 세션명 — 최상위 세션 관측에서도 제외.
fn is_monitor_or_gui_name(name: &str) -> bool {
    re_monitor_name().is_match(name) || name.starts_with("mon-") || name.starts_with("dgate-gui")
}

/// `hc ls` 터미널이 "관측 대상"인지 판정 — 기존 워커 규약 + 최상위 활성 AI 세션.
/// is_worker_terminal 동작은 그대로 두고 조합만 확장한다.
pub fn is_observable_terminal(t: &HcTerminalSummary) -> bool {
    is_worker_terminal(t) || is_active_top_level_session(t)
}

/// 최상위(parent 없음) 활성 AI 에이전트 세션 판정 — 리드 관측 확장:
/// ① `state === 'live'` ② `status === 'working'` (엄격 — idle/done/None 불가)
/// ③ parent 부재·빈 값 ④ 모니터·GUI(dg-monitor*/mon-*/dgate-gui*) 제외
/// ⑤ agent(소문자)가 AI 코딩 에이전트 7종 안팎.
pub fn is_active_top_level_session(t: &HcTerminalSummary) -> bool {
    if t.state.as_deref() != Some("live") {
        return false;
    }
    if t.status.as_deref() != Some("working") {
        return false;
    }
    if t.parent.as_deref().map(str::trim).is_some_and(|p| !p.is_empty()) {
        return false;
    }
    let name = t.name.as_deref().unwrap_or("");
    if is_monitor_or_gui_name(name) {
        return false;
    }
    let agent = t.agent.as_deref().unwrap_or("").to_lowercase();
    TOP_LEVEL_AI_AGENTS.contains(&agent.as_str())
}

// ---------------------------------------------------------------------------
// 레포 태그·스코프
// ---------------------------------------------------------------------------

/// 격리 워크트리 마커 (경로 구분자 정규화 후 탐색).
const WORKTREE_MARKER: &str = "/.herdr/worktrees/";

/// cwd → 레포 표시명. 빈 값은 'unknown'.
/// `/a/b/my-repo/.herdr/worktrees/coder1` 처럼 격리 워크트리면 소속 레포명
/// (`my-repo`)으로 환원하고, 그 외에는 resolve 후 basename.
pub fn extract_repo_tag(cwd: Option<&str>) -> String {
    let Some(cwd) = cwd.map(str::trim).filter(|s| !s.is_empty()) else {
        return "unknown".to_string();
    };
    let norm = cwd.replace('\\', "/");
    let marker_idx = norm.find(WORKTREE_MARKER);
    if let Some(idx) = marker_idx.filter(|i| *i > 0) {
        let repo_part = &norm[..idx];
        if let Some(base) = repo_part.split('/').filter(|s| !s.is_empty()).next_back() {
            return base.to_string();
        }
    }
    let resolved = resolve_path_best(cwd);
    let base = resolved
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty());
    base.unwrap_or_else(|| "unknown".to_string())
}

/// 경로 해석 — realpath 우선, 실패(미존재 등) 시 resolve 폴백.
fn resolve_path_best(p: &str) -> PathBuf {
    let path = Path::new(p);
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("/"))
                .join(path)
        }
    })
}

/// 워커 cwd가 `--repo <dir>` 스코프 안에 있는지 판정.
/// 필터 없음(None)이면 전역 관측(항상 true), 워커 cwd 미상이면 스코프 지정 시 false.
/// 워크트리 서브디렉터리(`.herdr/worktrees/…`)도 소속 레포 스코프에 포함된다.
pub fn matches_repo_scope(worker_cwd: Option<&str>, repo_filter_dir: Option<&str>) -> bool {
    let Some(filter) = repo_filter_dir.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    let Some(worker) = worker_cwd.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let worker_path = resolve_path_best(worker);
    let scope_path = resolve_path_best(filter);
    let scope_str = scope_path.to_string_lossy().into_owned();
    // 스코프가 루트('/')면 `scopePath + sep`가 '//'가 되어 전부 거짓이 되는 사고 방지 —
    // 이미 구분자로 끝나는 스코프는 그대로 접두사로 쓴다.
    let sep = std::path::MAIN_SEPARATOR.to_string();
    let prefix = if scope_str.ends_with(&sep) {
        scope_str.clone()
    } else {
        format!("{}{}", scope_str, sep)
    };
    let worker_str = worker_path.to_string_lossy();
    worker_str == scope_str || worker_str.starts_with(&prefix)
}

// ---------------------------------------------------------------------------
// 활동 줄 추출
// ---------------------------------------------------------------------------

fn re_ansi_osc() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\x1B\][^\x07\x1B]*(?:\x07|\x1B\\)").unwrap())
}

fn re_ansi_csi() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\x1B\[[0-9;?]*[ -/]*[@-~]").unwrap())
}

fn re_ansi_single() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\x1B[@-Z\\-_]").unwrap())
}

fn re_hc_status_line() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?i)^\[hc\]").unwrap())
}

fn re_box_only_line() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"^[\s─═┌┐└┘│├┤┬┴┼║╔╗╚╝╠╣╦╩╬\-_=·]+$").unwrap()
    })
}

fn re_spinner_only_line() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏\s]+$").unwrap())
}

fn re_box_edge_leading() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[\s│┌┐├┤║╔╗╠╣╦╩╬]+").unwrap())
}

fn re_box_edge_trailing() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"[\s│└┘├┤║╚╝╠╣╦╩╬]+$").unwrap())
}

fn re_whitespace_run() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\s+").unwrap())
}

/// ANSI CSI/OSC 이스케이프 제거.
pub fn strip_ansi(input: &str) -> String {
    let out = re_ansi_osc().replace_all(input, "");
    let out = re_ansi_csi().replace_all(&out, "");
    re_ansi_single().replace_all(&out, "").into_owned()
}

/// 코드포인트 기준 절단 — 초과분은 `…` 한 글자로 마감.
fn truncate_chars(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1).max(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// 터미널 화면 원문에서 "의미 있는 마지막 1줄"을 뽑는다.
/// 아래에서 위로 훑으며 빈 줄·`[hc] …` 상태줄·박스/구분선·스피너 전용 줄은 건너뛴다.
/// 후보 줄은 가장자리 박스 장식(`│` 등)을 떼고, `redact_secrets`로 시크릿을 마스킹한
/// 뒤 공백을 붙이고 maxChars(기본 68)로 절단한다. 없으면 None.
pub fn extract_last_activity_line(raw_screen: Option<&str>, max_chars: usize) -> Option<String> {
    let raw = raw_screen.filter(|s| !s.is_empty())?;
    let stripped = strip_ansi(&raw.replace("\r\n", "\n"));
    for line in stripped.split('\n').rev() {
        // 먼저 가장자리 박스 장식(│║ 등)을 떼고 판정 — '│ [hc] idle │' 같은 박스형 상태줄도
        // 스킵되고, '╚════╝' 같은 겹선 테두리는 본문 없음으로 건너뛴다.
        let line = re_box_edge_trailing()
            .replace(&re_box_edge_leading().replace(line, ""), "")
            .trim()
            .to_string();
        if line.is_empty() {
            continue;
        }
        if re_hc_status_line().is_match(&line) {
            continue;
        }
        if re_box_only_line().is_match(&line) {
            continue;
        }
        if re_spinner_only_line().is_match(&line) {
            continue;
        }
        // 시크릿 리덕션은 절단 이전에(레댁션 계약 — 잘린 경계 무효화 방지)
        let safe = re_whitespace_run().replace_all(&redact_secrets(&line), " ").trim().to_string();
        if safe.is_empty() {
            continue;
        }
        return Some(truncate_chars(&safe, max_chars));
    }
    None
}

// ---------------------------------------------------------------------------
// 슬롯·모델 추론
// ---------------------------------------------------------------------------

fn re_herdr_role() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)^HERDR_ROLE=(lead|explore|verifier|advisor|carbon|coder)$").unwrap()
    })
}

fn re_env_assign_arg() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*=").unwrap())
}

/// 파서가 인정하는 워커 CLI 집합 — parent 매칭 5종에 agy를 더한다.
const WORKER_CLI_SET: [&str; 6] = ["pi", "devin", "claude", "codex", "gemini", "agy"];

/// 토큰(절대경로 포함)의 basename — CLI 이름 추출용.
fn token_base(token: &str) -> String {
    let norm = token.replace('\\', "/");
    match norm.rfind('/') {
        Some(i) => norm[i + 1..].to_string(),
        None => norm,
    }
}

/// argv 앞단의 `env` + `KEY=VAL` 토큰 제거.
fn strip_env_prefix(tokens: &[String]) -> &[String] {
    let mut i = 0;
    if tokens.first().map(String::as_str) == Some("env") {
        i += 1;
    }
    while i < tokens.len() && re_env_assign_arg().is_match(&tokens[i]) {
        i += 1;
    }
    &tokens[i..]
}

/// `node …/dg.ts wrap … --` 래퍼 접두 제거 — `--` 이후의 실제 워커 커맨드만 남긴다.
fn strip_dg_wrap_prefix(tokens: &[String]) -> &[String] {
    let dg_idx = tokens.iter().position(|t| {
        let base = token_base(t);
        base == "dg.ts" || base == "dg"
    });
    let Some(dg_idx) = dg_idx else {
        return tokens;
    };
    match tokens[dg_idx..].iter().position(|t| t == "--") {
        Some(rel) => &tokens[dg_idx + rel + 1..],
        None => tokens,
    }
}

/// argv(+agent 폴백)에서 실제 워커 CLI 이름과 `--model` 값을 찾는다.
fn parse_worker_cli_and_model(argv: &[String], fallback_agent: Option<&str>) -> (String, Option<String>) {
    let cleaned: Vec<String> = argv.iter().filter(|t| !t.is_empty()).cloned().collect();
    let once = strip_env_prefix(&cleaned).to_vec();
    let twice_outer = strip_dg_wrap_prefix(&once).to_vec();
    let tokens = strip_env_prefix(&twice_outer).to_vec();

    let cli_idx = tokens
        .iter()
        .position(|t| WORKER_CLI_SET.contains(&token_base(t).as_str()));
    let cli = match cli_idx {
        Some(i) => token_base(&tokens[i]),
        None => {
            let agent = fallback_agent.unwrap_or("").to_lowercase();
            if WORKER_CLI_SET.contains(&agent.as_str()) {
                agent
            } else {
                "custom".to_string()
            }
        }
    };

    let mut model: Option<String> = None;
    let scan_start = cli_idx.unwrap_or(0);
    let mut i = scan_start;
    while i < tokens.len() {
        let t = &tokens[i];
        if t == "--model" {
            if let Some(v) = tokens.get(i + 1) {
                if !v.starts_with('-') {
                    model = Some(v.clone());
                    break;
                }
            }
        } else if let Some(v) = t.strip_prefix("--model=") {
            model = Some(v.to_string());
            break;
        }
        i += 1;
    }
    (cli, model)
}

/// TS `formatRoleBadge(spec)` 패리티 — `{cli}:{short-model}` 배지.
/// - model 없으면 cli만.
/// - `/` 마지막 세그먼트 사용, `X-X-` 중복 접두(`glm-glm-5.3` → `glm-5.3`) 정규화,
///   `${cli}-` 접두 제거 후 조립.
pub fn format_role_badge(cli: &str, model: Option<&str>) -> String {
    let raw = model.map(str::trim).filter(|s| !s.is_empty());
    let Some(raw) = raw else {
        return cli.to_string();
    };
    let after_slash = raw.rsplit('/').next().unwrap_or(raw);
    let mut short = dedupe_repeated_prefix(after_slash).to_string();
    let cli_prefix = format!("{}-", cli);
    if short.starts_with(&cli_prefix) {
        short = short[cli_prefix.len()..].to_string();
    }
    format!("{}:{}", cli, short)
}

/// `^([A-Za-z0-9]+)-\1-` 정규화 — regex 크레이트가 역참조를 지원하지 않아 수동 구현.
/// 선행 ASCII 영숫자 실행이 `-실행-` 형태로 한 번 더 반복되면 앞 반복을 제거한다.
fn dedupe_repeated_prefix(s: &str) -> &str {
    let run_len = s.chars().take_while(|c| c.is_ascii_alphanumeric()).count();
    if run_len == 0 {
        return s;
    }
    let bytes = s.as_bytes();
    let rest = &bytes[run_len..];
    if rest.first() == Some(&b'-')
        && rest.len() >= run_len + 2
        && &rest[1..1 + run_len] == &bytes[..run_len]
        && rest[1 + run_len] == b'-'
    {
        &s[run_len + 1..]
    } else {
        s
    }
}

/// roster 프로필 → TUI 라벨(TS `rosterLabelsFromProfile`).
pub fn roster_labels_from_profile(
    profile_name: &str,
    roster_source: &str,
    profile: &RosterProfile,
) -> RosterDisplayLabels {
    let badge = |slot: &RoleSlot| -> String {
        profile
            .roles
            .get(slot)
            .map(|spec| {
                format_role_badge(
                    spec.cli.as_str(),
                    spec.model.as_deref(),
                )
            })
            .unwrap_or_else(|| "—".to_string())
    };
    RosterDisplayLabels {
        profile_name: profile_name.to_string(),
        roster_source: roster_source.to_string(),
        lead: badge(&RoleSlot::Lead),
        explore: badge(&RoleSlot::Explore),
        coder: badge(&RoleSlot::Coder),
        verifier: badge(&RoleSlot::Verifier),
        advisor: badge(&RoleSlot::Advisor),
    }
}

/// `hc get` 상세 + 화면 원문 → LiveWorkerInfo.
/// 슬롯은 ① argv의 `HERDR_ROLE=`(carbon→advisor) ② 세션명 규약(verify-/exp-/carbon-/lead-
/// …) 순으로 판정하고, CLI·모델은 `env`·`dg wrap` 래퍼를 벗겨낸 뒤 실제 CLI와
/// `--model`에서 읽는다(실패 시 `detail.agent` 폴백).
pub fn infer_live_worker_slot(detail: &HcTerminalDetail, raw_screen: Option<&str>) -> Option<LiveWorkerInfo> {
    if detail.address.is_empty() {
        return None;
    }
    let argv: Vec<String> = detail.argv.clone().unwrap_or_default();

    // 1) 슬롯 — HERDR_ROLE env가 최우선
    let mut slot: LiveWorkerSlot = RoleSlot::Coder;
    let mut role_from_argv = false;
    for token in &argv {
        if let Some(m) = re_herdr_role().captures(token) {
            let role = m.get(1).map(|g| g.as_str().to_lowercase()).unwrap_or_default();
            slot = if role == "carbon" {
                RoleSlot::Advisor
            } else {
                RoleSlot::parse(&role).unwrap_or(RoleSlot::Coder)
            };
            role_from_argv = true;
            break;
        }
    }
    let name = detail.name.clone().unwrap_or_default();
    if !role_from_argv && !name.is_empty() {
        let lower = name.to_lowercase();
        // TS `/verify|verifier/i` — 'verifier'에는 'y'가 없어 'verify'만으론 안 걸린다.
        if lower.contains("verify") || lower.contains("verifier") {
            slot = RoleSlot::Verifier;
        } else if lower.contains("exp-") || lower.contains("explore") {
            slot = RoleSlot::Explore;
        } else if lower.contains("advisor") || lower.contains("carbon") {
            slot = RoleSlot::Advisor;
        } else if lower.contains("lead-") {
            slot = RoleSlot::Lead;
        } else {
            slot = RoleSlot::Coder;
        }
    }

    // 2) CLI·모델
    let (cli, model) = parse_worker_cli_and_model(&argv, detail.agent.as_deref());
    let short_model = format_role_badge(&cli, model.as_deref());
    let model_name = model.unwrap_or_else(|| cli.clone());

    // 3) 조립 — 활동은 화면 원문 우선, 없으면 상태 기반 간결 폴백
    let activity = extract_last_activity_line(raw_screen, 68).unwrap_or_else(|| {
        detail
            .status
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| format!("{} 작업 중", s))
            .unwrap_or_else(|| "작업 실행 중".to_string())
    });
    Some(LiveWorkerInfo {
        slot,
        badge: format!("{} (실행 중)", short_model),
        short_model,
        model_name,
        address: detail.address.clone(),
        name: if name.is_empty() { detail.address.clone() } else { name },
        cwd: detail.cwd.clone(),
        repo_tag: extract_repo_tag(detail.cwd.as_deref()),
        activity,
        status: detail.status.clone(),
    })
}

// ---------------------------------------------------------------------------
// 라벨 병합
// ---------------------------------------------------------------------------

/// 최상위 활성 AI 세션을 Lead 카드로 정규화:
/// - 슬롯은 Lead 고정(LEAD 카운터·Lead 카드에 반영).
/// - 표시명은 비어 있지 않은 세션명, 없으면 'lead'(원 주소 노출 방지).
/// - `--model` 미파싱 시 shortModel·modelName을 agent명(예: 'antigravity')으로 폴백.
pub fn normalize_top_level_worker(worker: &mut LiveWorkerInfo, summary: &HcTerminalSummary) {
    worker.slot = RoleSlot::Lead;
    let name = summary.name.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(n) = name {
        worker.name = n.to_string();
    } else {
        worker.name = "lead".to_string();
    }
    let agent = summary.agent.as_deref().unwrap_or("").trim().to_lowercase();
    if !agent.is_empty() && worker.short_model == "custom" {
        worker.short_model = agent.clone();
        worker.badge = format!("{} (실행 중)", agent);
        if worker.model_name.is_empty() || worker.model_name == "custom" {
            worker.model_name = agent;
        }
    }
}

/// 라이브 오버라이드 대상 슬롯(lead는 watchedLead 부재 시에만).
const LIVE_OVERRIDE_SLOTS: [LiveWorkerSlot; 4] = [
    RoleSlot::Verifier,
    RoleSlot::Coder,
    RoleSlot::Explore,
    RoleSlot::Advisor,
];

/// roster 대기(standby) 라벨 위에 라이브 워커 배지를 얹는다.
/// - `watched_lead`(관측 중 리드 세션 배지)가 있으면 lead 슬롯을 침범하지 않는다.
/// - 슬롯별 첫 워커가 우선(동일 슬롯 다중 워커 시 표기 안정).
/// - 워커가 없으면 대기 라벨로 그대로 되돌아간다(종료 시 자동 복귀).
/// - 모델 플래그는 실측 모델이 있을 때만 `--model <원문>` (applyRosterToState 계약 동일).
pub fn merge_live_roster_labels(
    base: &RosterDisplayLabels,
    watched_lead: Option<&str>,
    workers: &[LiveWorkerInfo],
) -> MergedLiveRoster {
    let watched_lead = watched_lead.map(str::trim).filter(|s| !s.is_empty());
    let mut labels = base.clone();
    if let Some(wl) = watched_lead {
        labels.lead = wl.to_string();
    }

    let mut lead_worker: Option<&LiveWorkerInfo> = None;
    let mut verifier_worker: Option<&LiveWorkerInfo> = None;
    let mut claimed: HashSet<LiveWorkerSlot> = HashSet::new();
    for w in workers {
        if w.slot == RoleSlot::Lead {
            if watched_lead.is_none() && lead_worker.is_none() {
                lead_worker = Some(w);
            }
            continue;
        }
        if LIVE_OVERRIDE_SLOTS.contains(&w.slot) && !claimed.contains(&w.slot) {
            claimed.insert(w.slot);
            labels.set_slot(&w.slot, w.short_model.clone());
            if w.slot == RoleSlot::Verifier {
                verifier_worker = Some(w);
            }
        }
    }

    let lead_model_name = labels.lead.clone();
    let lead_model_flag = match lead_worker {
        Some(w) if !w.model_name.is_empty() && w.model_name != w.short_model => {
            format!("--model {}", w.model_name)
        }
        _ => labels.lead.clone(),
    };
    let verifier_model_name = labels.verifier.clone();
    let verifier_model_flag = match verifier_worker {
        Some(w) if !w.model_name.is_empty() && w.model_name != w.short_model => {
            format!("--model {}", w.model_name)
        }
        _ => labels.verifier.clone(),
    };

    MergedLiveRoster {
        labels,
        lead_model_name,
        lead_model_flag,
        verifier_model_name,
        verifier_model_flag,
        workers: workers.to_vec(),
        active_worker_count: workers.len(),
    }
}

// ---------------------------------------------------------------------------
// hc 실행 추상
// ---------------------------------------------------------------------------

/// `hc` 실행 추상 — 테스트가 mock으로 갈아끼운다. stdout 문자열 반환.
/// 오류(실행 실패·타임아웃·비정상 종료·버퍼 초과)는 모두 `Err`로 조용히 폴백된다.
pub trait HcExecRunner: Send + Sync {
    fn exec(&self, file: &str, args: &[&str], timeout_ms: u64) -> Result<String, String>;
}

/// 기본 실행기 — 자식 stdout을 4MB 상한으로 수집하며 데드라인 경과 시 kill한다.
pub struct DefaultHcRunner;

impl HcExecRunner for DefaultHcRunner {
    fn exec(&self, file: &str, args: &[&str], timeout_ms: u64) -> Result<String, String> {
        let mut child = std::process::Command::new(file)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| "stdout pipe unavailable".to_string())?;
        // 읽기 스레드 — 자식이 파이프를 가득 채워도 교착하지 않게 한다.
        let reader = std::thread::spawn(move || {
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if buf.len() < HC_MAX_BUFFER_BYTES + 1 {
                            let room = HC_MAX_BUFFER_BYTES + 1 - buf.len();
                            buf.extend_from_slice(&chunk[..n.min(room)]);
                            if buf.len() > HC_MAX_BUFFER_BYTES {
                                return buf;
                            }
                        } else {
                            return buf;
                        }
                    }
                    Err(_) => break,
                }
            }
            buf
        });
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        let status;
        loop {
            match child.try_wait().map_err(|e| e.to_string())? {
                Some(s) => {
                    status = s;
                    break;
                }
                None => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!("timeout after {}ms", timeout_ms));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        let buf = reader.join().unwrap_or_default();
        if buf.len() > HC_MAX_BUFFER_BYTES {
            return Err("maxbuffer exceeded".to_string());
        }
        if !status.success() {
            return Err(format!("exit status {:?}", status.code()));
        }
        String::from_utf8(buf).map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// `hc ls` 파싱 (엄격한 terminals 배열 검사)
// ---------------------------------------------------------------------------

/// `hc ls --json` 출력 파싱 — 엄격 계약:
/// - 루트가 JSON 객체가 아니거나 `terminals`가 없거나 배열이 아니면(`{}` ·
///   `{"terminals":null}` · malformed) `None`을 반환해 **이전 워커 상태를 보존**한다.
/// - 명시적 빈 배열 `{"terminals":[]}`은 `Some(vec![])` — "실행 중 워커 0명" 방송.
pub fn parse_hc_ls_json(raw: &str) -> Option<Vec<HcTerminalSummary>> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let obj = parsed.as_object()?;
    let terminals = obj.get("terminals")?;
    let arr = terminals.as_array()?;
    Some(
        arr.iter()
            .filter_map(|t| t.as_object().and_then(HcTerminalSummary::from_json))
            .collect(),
    )
}

/// `hc get --json` 출력 → 상세(argv 포함). 파싱 실패·address 누락은 None.
fn parse_hc_get_json(raw: &str) -> Option<HcTerminalDetail> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let obj = parsed.as_object()?;
    let summary = HcTerminalSummary::from_json(obj)?;
    let argv = obj.get("argv").and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<String>>()
    });
    Some(HcTerminalDetail {
        argv,
        ..HcTerminalDetail::from_summary(&summary)
    })
}

// ---------------------------------------------------------------------------
// 주기 동기화 스냅숏터
// ---------------------------------------------------------------------------

/// 스냅숏터 구성 옵션 — 기본값은 `hc` bin·기본 실행기.
pub struct LiveSyncOptions {
    /// roster.json 프로필 후보 기준 디렉터리(프로젝트 오버라이드 탐색용).
    pub repo_dir: PathBuf,
    /// `--repo <dir>` 스코프 — None이면 전체 레포 관측.
    pub repo_filter_dir: Option<PathBuf>,
    /// `hc` 실행 파일(기본 'hc').
    pub hc_bin: String,
    /// `hc` 실행기 주입(테스트용 — 기본 DefaultHcRunner).
    pub exec_runner: Arc<dyn HcExecRunner>,
    /// 관측 중 리드 배지 제공자(있으면 lead 슬롯 라이브 오버라이드 금지).
    pub get_watched_lead: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    /// 병합 결과 수신자 — 매 틱 호출(변경 여부 판정은 수신자 책임).
    pub on_update: Option<Arc<Mutex<dyn FnMut(&MergedLiveRoster) + Send + Sync>>>,
}

impl LiveSyncOptions {
    pub fn new(repo_dir: impl Into<PathBuf>) -> Self {
        LiveSyncOptions {
            repo_dir: repo_dir.into(),
            repo_filter_dir: None,
            hc_bin: "hc".to_string(),
            exec_runner: Arc::new(DefaultHcRunner),
            get_watched_lead: None,
            on_update: None,
        }
    }
}

/// 주기 로스터 동기화 스냅숏터(동기 tick 계약):
/// - roster.json 후보 경로의 `mtimeMs:size` 서명이 바뀌면 설정을 핫리로드한다.
/// - `hc ls --json` → 워커 필터(`is_worker_terminal` + `matches_repo_scope`) →
///   주소별 `hc get --json` 캐시(떠난 주소 즉시 증발) → 활동 캡처 상한 6개의
///   `hc read <addr> --lines 4`.
/// - 모든 `hc` 오류는 조용히 폴백 — 관제 화면이 죽지 않는다(§3.5).
/// - `tick_now()`가 [`MergedLiveRoster`]를 반환하며 `on_update` 수신자에도 방송한다.
pub struct LiveSyncSnapshotter {
    repo_dir: PathBuf,
    repo_filter_dir: Option<PathBuf>,
    hc_bin: String,
    runner: Arc<dyn HcExecRunner>,
    get_watched_lead: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    on_update: Option<Arc<Mutex<dyn FnMut(&MergedLiveRoster) + Send + Sync>>>,
    base_labels: Option<RosterDisplayLabels>,
    roster_signature: Option<String>,
    last_workers: Vec<LiveWorkerInfo>,
    detail_cache: HashMap<String, HcTerminalDetail>,
    screen_cache: HashMap<String, String>,
    in_flight: bool,
    stopped: bool,
}

impl LiveSyncSnapshotter {
    pub fn new(opts: LiveSyncOptions) -> Self {
        LiveSyncSnapshotter {
            repo_dir: opts.repo_dir,
            repo_filter_dir: opts.repo_filter_dir,
            hc_bin: opts.hc_bin,
            runner: opts.exec_runner,
            get_watched_lead: opts.get_watched_lead,
            on_update: opts.on_update,
            base_labels: None,
            roster_signature: None,
            last_workers: Vec::new(),
            detail_cache: HashMap::new(),
            screen_cache: HashMap::new(),
            in_flight: false,
            stopped: false,
        }
    }

    /// 타이머 정지·캐시 정리(멱등).
    pub fn stop(&mut self) {
        self.stopped = true;
        self.detail_cache.clear();
        self.screen_cache.clear();
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    /// 현재 캐시 상태를 그대로 병합한 스냅숏(네트워크·프로세스 호출 없음).
    pub fn peek(&self) -> MergedLiveRoster {
        let labels = self.base_labels.clone().unwrap_or_else(builtin_fallback_labels);
        let watched = self
            .get_watched_lead
            .as_ref()
            .and_then(|f| f());
        merge_live_roster_labels(&labels, watched.as_deref(), &self.last_workers)
    }

    /// 1회 즉시 동기화(테스트·수동 트리거·주기 타이머용).
    pub fn tick_now(&mut self) -> MergedLiveRoster {
        if self.in_flight || self.stopped {
            return self.peek();
        }
        self.in_flight = true;
        let result = self.run_tick();
        self.in_flight = false;
        result
    }

    fn run_tick(&mut self) -> MergedLiveRoster {
        self.reload_roster_labels();
        let labels = self.base_labels.clone().unwrap_or_else(builtin_fallback_labels);

        // 1) `hc ls --json` — 실행 오류·JSON 파싱 실패 모두 이전 워커 상태 유지(fail-safe).
        //    (파싱 실패 시 빈 목록으로 방송하면 워커 0건 + 허위 종료 로그가 터진다.)
        let summaries: Option<Vec<HcTerminalSummary>> = self
            .runner
            .exec(&self.hc_bin, &["ls", "--json"], HC_LS_TIMEOUT_MS)
            .ok()
            .and_then(|raw| parse_hc_ls_json(&raw))
            .map(|terminals| {
                terminals
                    .into_iter()
                    .filter(|t| {
                        is_observable_terminal(t)
                            && matches_repo_scope(
                                t.cwd.as_deref(),
                                self.repo_filter_dir.as_deref().and_then(|p| p.to_str()),
                            )
                    })
                    .collect()
            });

        // 2) 상세 캐시 갱신 — 떠난 주소는 즉시 증발
        if let Some(summaries) = summaries {
            let active: HashSet<&str> = summaries.iter().map(|t| t.address.as_str()).collect();
            self.detail_cache.retain(|addr, _| active.contains(addr.as_str()));
            self.screen_cache.retain(|addr, _| active.contains(addr.as_str()));

            // 3) 워커별 상세·활동 수집 (활동 read는 틱당 상한)
            let mut workers: Vec<LiveWorkerInfo> = Vec::new();
            let mut read_count: usize = 0;
            for summary in &summaries {
                let mut detail = match self.detail_cache.get(&summary.address) {
                    Some(cached) => cached.clone(),
                    None => {
                        let fetched = self
                            .runner
                            .exec(
                                &self.hc_bin,
                                &["get", summary.address.as_str(), "--json"],
                                HC_LS_TIMEOUT_MS,
                            )
                            .ok()
                            .and_then(|raw| parse_hc_get_json(&raw));
                        fetched.unwrap_or_else(|| HcTerminalDetail::from_summary(summary))
                    }
                };
                // `hc get` 캐시는 argv·이름 같은 정적 정보만 담당 — status/state/cwd는
                // 매 틱의 `hc ls` 요약을 항상 위에 덮어쓴다. 그래야 working→done 전환이
                // 실시간으로 카드·로그에 반영된다.
                detail.agent = summary.agent.clone().or(detail.agent);
                detail.name = summary.name.clone().or(detail.name);
                detail.parent = summary.parent.clone().or(detail.parent);
                detail.state = summary.state.clone().or(detail.state);
                detail.status = summary.status.clone().or(detail.status);
                detail.cwd = summary.cwd.clone().or(detail.cwd);
                self.detail_cache.insert(summary.address.clone(), detail.clone());

                let mut screen: Option<String> = None;
                if read_count < MAX_ACTIVITY_READS_PER_TICK {
                    read_count += 1;
                    match self.runner.exec(
                        &self.hc_bin,
                        &["read", summary.address.as_str(), "--lines", "4"],
                        HC_READ_TIMEOUT_MS,
                    ) {
                        Ok(s) => {
                            self.screen_cache.insert(summary.address.clone(), s.clone());
                            screen = Some(s);
                        }
                        Err(_) => {
                            screen = None;
                        }
                    }
                }
                let raw_screen = screen.or_else(|| self.screen_cache.get(&summary.address).cloned());
                if let Some(mut worker) = infer_live_worker_slot(&detail, raw_screen.as_deref()) {
                    // 최상위 활성 AI 세션(워커 규약 밖 리드 등) — Lead 슬롯·표시명·
                    // shortModel 폴백을 정규화한다. 워커 규약 터미널은 그대로 둔다.
                    if is_active_top_level_session(summary) && !is_worker_terminal(summary) {
                        normalize_top_level_worker(&mut worker, summary);
                    }
                    workers.push(worker);
                }
            }
            self.last_workers = workers;
        }

        // 4) 병합 방송 — 변경 판정·렌더 제어는 수신자(cli/GPUI) 책임
        let watched = self.get_watched_lead.as_ref().and_then(|f| f());
        let merged = merge_live_roster_labels(&labels, watched.as_deref(), &self.last_workers);
        if !self.stopped {
            if let Some(sink) = &self.on_update {
                if let Ok(mut guard) = sink.lock() {
                    guard(&merged);
                }
            }
        }
        merged
    }

    fn roster_candidate_paths(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        if let Ok(env_path) = std::env::var("HERDR_ROSTER_PATH") {
            let trimmed = env_path.trim().to_string();
            if !trimmed.is_empty() {
                out.push(PathBuf::from(&trimmed));
            }
        }
        out.push(self.repo_dir.join(".herdr").join("roster.json"));
        out.push(global_roster_path());
        out
    }

    fn compute_roster_signature(&self) -> String {
        let parts: Vec<String> = self
            .roster_candidate_paths()
            .into_iter()
            .map(|p| match std::fs::metadata(&p) {
                Ok(meta) => {
                    let mtime_ms = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    format!("{}:{}:{}", p.display(), mtime_ms, meta.len())
                }
                Err(_) => format!("{}:-", p.display()),
            })
            .collect();
        parts.join("|")
    }

    fn reload_roster_labels(&mut self) {
        let sig = self.compute_roster_signature();
        if self.roster_signature.as_deref() == Some(sig.as_str()) && self.base_labels.is_some() {
            return;
        }
        self.roster_signature = Some(sig);
        let loaded = load_roster_config(None, Some(&self.repo_dir));
        let (name, profile, _fallback) =
            resolve_roster_profile(&loaded.config, &loaded.config.default_profile);
        self.base_labels = Some(roster_labels_from_profile(
            &name,
            loaded.source.as_str(),
            &profile,
        ));
    }
}

fn builtin_fallback_labels() -> RosterDisplayLabels {
    let builtin = crate::roster::builtin_roster_config();
    let (name, profile, _) = resolve_roster_profile(&builtin, "quota-saver");
    roster_labels_from_profile(&name, "builtin", &profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::builtin_roster_config;
    use crate::testutil::{env_lock, temp_dir};
    use std::sync::MutexGuard;

    fn summary(name: &str, agent: &str, state: &str, parent: Option<&str>) -> HcTerminalSummary {
        HcTerminalSummary {
            address: format!("addr-{}", name),
            agent: Some(agent.to_string()),
            name: Some(name.to_string()),
            parent: parent.map(str::to_string),
            state: Some(state.to_string()),
            status: None,
            cwd: None,
        }
    }

    // ---------- is_worker_terminal ----------

    #[test]
    fn worker_terminal_requires_live_state() {
        let mut t = summary("w-1", "pi", "done", None);
        assert!(!is_worker_terminal(&t));
        t.state = Some("live".to_string());
        assert!(is_worker_terminal(&t));
        t.state = None;
        assert!(!is_worker_terminal(&t));
    }

    #[test]
    fn worker_terminal_excludes_monitors_and_shells() {
        assert!(!is_worker_terminal(&summary("dg-monitor", "pi", "live", None)));
        assert!(!is_worker_terminal(&summary("dg-monitor-1", "pi", "live", None)));
        assert!(!is_worker_terminal(&summary("mon-x", "pi", "live", None)));
        for shell in ["zsh", "bash", "fish", "sh", "ksh", "dash", "pwsh", "powershell.exe"] {
            assert!(!is_worker_terminal(&summary("w-1", shell, "live", Some("lead"))), "{}", shell);
        }
        assert!(!is_worker_terminal(&summary("w-1", "antigravity", "live", None)));
        // 대소문자 혼합 에이전트도 소문자 정규화로 제외된다
        assert!(!is_worker_terminal(&summary("w-1", "ZSH", "live", Some("p"))));
    }

    #[test]
    fn worker_terminal_matches_name_prefix_or_parent_cli() {
        for name in ["w-1", "exp-2", "explore-3", "coder1", "coder-x", "verify-1", "verifier-2", "lead-1", "advisor-9", "carbon-1"] {
            assert!(is_worker_terminal(&summary(name, "pi", "live", None)), "{}", name);
        }
        // parent + 워커 CLI 조합
        for agent in ["pi", "devin", "claude", "codex", "gemini"] {
            assert!(is_worker_terminal(&summary("some-session", agent, "live", Some("lead-addr"))));
        }
        // parent가 있어도 워커 CLI가 아니면 제외
        assert!(!is_worker_terminal(&summary("some-session", "python", "live", Some("x"))));
        // 이름 접두 없이 parent 없음 → 제외
        assert!(!is_worker_terminal(&summary("random-name", "pi", "live", None)));
        // agy는 parent 매칭 대상 아님(이름 접두로만 통과)
        assert!(!is_worker_terminal(&summary("random-name", "agy", "live", Some("x"))));
        assert!(is_worker_terminal(&summary("w-agy", "agy", "live", None)));
    }

    // ---------- is_active_top_level_session / is_observable_terminal ----------

    fn top_summary(name: &str, agent: &str, status: Option<&str>) -> HcTerminalSummary {
        HcTerminalSummary {
            address: format!("addr-{}", name),
            agent: Some(agent.to_string()),
            name: Some(name.to_string()),
            parent: None,
            state: Some("live".to_string()),
            status: status.map(str::to_string),
            cwd: None,
        }
    }

    #[test]
    fn top_level_active_sessions_are_observable_as_lead() {
        // 긍정 — 최상위 working AI 에이전트는 관측된다(워커 규약 밖 포함)
        for agent in ["antigravity", "pi", "claude", "codex", "devin", "gemini", "agy"] {
            let t = top_summary(&format!("paperwork-{}", agent), agent, Some("working"));
            assert!(is_active_top_level_session(&t), "{}", agent);
            assert!(is_observable_terminal(&t), "{}", agent);
        }
        // kakao-manual-bot pi 예시 — 워커가 아니여도 관측된다
        let pi = top_summary("kakao-manual-bot", "pi", Some("working"));
        assert!(!is_worker_terminal(&pi));
        assert!(is_observable_terminal(&pi));
        // is_worker_terminal 동작 불변 — 관측 확장이 워커 판정을 바꾸지 않는다
        let w = summary("w-1", "pi", "live", None);
        assert!(is_observable_terminal(&w) && is_worker_terminal(&w));
    }

    #[test]
    fn top_level_session_strict_exclusions() {
        // status 엄격 — idle/done/None 불가
        for status in [Some("idle"), Some("done"), None] {
            assert!(!is_active_top_level_session(&top_summary("s", "antigravity", status)));
            assert!(!is_active_top_level_session(&top_summary("s", "pi", status)));
        }
        // state 불일치
        let mut dead = top_summary("s", "pi", Some("working"));
        dead.state = Some("done".into());
        assert!(!is_active_top_level_session(&dead));
        // parent 존재 — 최상위 아님
        let mut child = top_summary("s", "pi", Some("working"));
        child.parent = Some("lead-addr".into());
        assert!(!is_active_top_level_session(&child));
        // 셸·런타임·모니터·GUI 제외
        for agent in ["zsh", "bash", "sh", "node", "python3", "powershell.exe", "dgate-gui"] {
            assert!(!is_active_top_level_session(&top_summary("s", agent, Some("working"))), "{}", agent);
        }
        for name in ["dg-monitor", "dg-monitor-x", "mon-x", "dgate-gui-main"] {
            let t = top_summary(name, "antigravity", Some("working"));
            assert!(!is_active_top_level_session(&t), "{}", name);
            assert!(!is_observable_terminal(&t), "{}", name);
        }
    }

    #[test]
    fn top_level_worker_normalized_to_lead_card() {
        // 이름·slot·shortModel 폴백 — 렌더 카드 '[paperwork-operations] lead · antigravity'
        let sum = top_summary("paperwork-operations", "antigravity", Some("working"));
        let mut w = worker(RoleSlot::Coder, "custom", None);
        w.name = "t155".to_string();
        normalize_top_level_worker(&mut w, &sum);
        assert_eq!(w.slot, RoleSlot::Lead);
        assert_eq!(w.name, "paperwork-operations");
        assert_eq!(w.short_model, "antigravity");
        assert_eq!(w.badge, "antigravity (실행 중)");
        assert_eq!(w.model_name, "antigravity");
        // 이름 없으면 'lead'로 표시
        let mut sum2 = top_summary("", "pi", Some("working"));
        sum2.name = None;
        let mut w2 = worker(RoleSlot::Coder, "pi", None);
        w2.name = "t155".to_string();
        normalize_top_level_worker(&mut w2, &sum2);
        assert_eq!(w2.slot, RoleSlot::Lead);
        assert_eq!(w2.name, "lead");
        assert_eq!(w2.short_model, "pi");
    }

    // ---------- extract_repo_tag ----------

    #[test]
    fn repo_tag_resolves_worktree_marker_to_parent_repo() {
        assert_eq!(
            extract_repo_tag(Some("/a/b/my-repo/.herdr/worktrees/coder1")),
            "my-repo"
        );
        assert_eq!(extract_repo_tag(Some("/Users/x/proj/.herdr/worktrees/exp-1")), "proj");
        // 역슬래시 정규화
        assert_eq!(
            extract_repo_tag(Some("C:\\work\\repo\\.herdr\\worktrees\\w1")),
            "repo"
        );
    }

    #[test]
    fn repo_tag_uses_basename_for_plain_cwd() {
        let tmp = temp_dir("repo-tag");
        std::fs::create_dir_all(&tmp).unwrap();
        assert_eq!(extract_repo_tag(Some(tmp.to_str().unwrap())), tmp.file_name().unwrap().to_string_lossy());
        // 미존재 상대경로도 resolve 폴백으로 basename
        assert_eq!(extract_repo_tag(Some("relative/dir")), "dir");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn repo_tag_empty_is_unknown() {
        assert_eq!(extract_repo_tag(None), "unknown");
        assert_eq!(extract_repo_tag(Some("")), "unknown");
        assert_eq!(extract_repo_tag(Some("   ")), "unknown");
    }

    // ---------- matches_repo_scope ----------

    #[test]
    fn repo_scope_no_filter_matches_everything() {
        assert!(matches_repo_scope(None, None));
        assert!(matches_repo_scope(Some("/any/path"), None));
        assert!(matches_repo_scope(None, Some("")));
    }

    #[test]
    fn repo_scope_unknown_worker_cwd_is_excluded_when_scoped() {
        assert!(!matches_repo_scope(None, Some("/Users/kwak/Projects/Gemini_trick")));
        assert!(!matches_repo_scope(Some(""), Some("/repo")));
    }

    #[test]
    fn repo_scope_prefix_and_worktree_inclusion() {
        let tmp = temp_dir("scope");
        let repo = tmp.join("my-repo");
        let wt = repo.join(".herdr").join("worktrees").join("coder1");
        let outside = tmp.join("other-repo");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let repo_s = repo.to_str().unwrap().to_string();
        assert!(matches_repo_scope(Some(repo_s.as_str()), Some(&repo_s)));
        assert!(matches_repo_scope(Some(wt.to_str().unwrap()), Some(&repo_s)));
        assert!(!matches_repo_scope(Some(outside.to_str().unwrap()), Some(&repo_s)));
        // 스코프가 워커 cwd의 상위여도 거짓 — 접두사 방향만 허용
        assert!(!matches_repo_scope(Some(&repo_s), Some(wt.to_str().unwrap())));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn repo_scope_root_scope_handles_separator_cleanly() {
        // 루트 '/' 스코프는 '//'-접두 사고 없이 전부 매칭
        assert!(matches_repo_scope(Some("/Users/anywhere"), Some("/")));
        assert!(matches_repo_scope(Some("/"), Some("/")));
        // 스코프가 미존재 경로여도 resolve 폴백으로 접두사 판정
        assert!(matches_repo_scope(Some("/definitely/missing/child"), Some("/definitely/missing")));
    }

    // ---------- strip_ansi·extract_last_activity_line ----------

    #[test]
    fn strip_ansi_removes_csi_osc_and_single_escapes() {
        assert_eq!(strip_ansi("\x1B[31mred\x1B[0m"), "red");
        assert_eq!(strip_ansi("\x1B]0;title\x07plain"), "plain");
        assert_eq!(strip_ansi("\x1B]0;title\x1B\\plain"), "plain");
        assert_eq!(strip_ansi("\x1BMnext"), "next");
        assert_eq!(strip_ansi("clean"), "clean");
    }

    #[test]
    fn activity_picks_last_meaningful_line() {
        let screen = "\x1B[32mok line\x1B[0m\n\n   \nlast actual line";
        assert_eq!(extract_last_activity_line(Some(screen), 68).as_deref(), Some("last actual line"));
    }

    #[test]
    fn activity_skips_hc_footer_box_and_spinner_lines() {
        let screen = "real work\n[hc] idle\n─────────────\n╔════════╗\n⠋\n";
        assert_eq!(extract_last_activity_line(Some(screen), 68).as_deref(), Some("real work"));
        // 박스형 상태줄 '│ [hc] idle │' — 가장자리 제거 후 스킵
        let boxed = "keep me\n│ [hc] idle │";
        assert_eq!(extract_last_activity_line(Some(boxed), 68).as_deref(), Some("keep me"));
        // 상태줄만 있으면 None
        assert_eq!(extract_last_activity_line(Some("[hc] running"), 68), None);
        assert_eq!(extract_last_activity_line(Some("╚══════╝"), 68), None);
        assert_eq!(extract_last_activity_line(Some("⠙"), 68), None);
    }

    #[test]
    fn activity_box_edges_stripped_from_content_lines() {
        let screen = "│ cat src/main.rs │";
        assert_eq!(extract_last_activity_line(Some(screen), 68).as_deref(), Some("cat src/main.rs"));
        let dbl = "║ double border ║";
        assert_eq!(extract_last_activity_line(Some(dbl), 68).as_deref(), Some("double border"));
    }

    #[test]
    fn activity_redacts_secrets_before_truncation() {
        let screen = "curl -H \"Authorization: Bearer ya29.a0AfH6SMBx1234567890\" https://api";
        let out = extract_last_activity_line(Some(screen), 68).unwrap();
        assert!(!out.contains("ya29"), "{}", out);
        assert!(out.contains("<redacted>"), "{}", out);
        let screen2 = "run MY_API_KEY=abcdef1234567890 now";
        let out2 = extract_last_activity_line(Some(screen2), 68).unwrap();
        assert!(out2.contains("MY_API_KEY=<redacted>"), "{}", out2);
    }

    #[test]
    fn activity_truncates_to_max_chars_with_ellipsis() {
        let long = "x".repeat(100);
        let out = extract_last_activity_line(Some(&long), 68).unwrap();
        assert_eq!(out.chars().count(), 68);
        assert!(out.ends_with('…'));
        // 멀티바이트 안전 — 한글 100자
        let korean: String = "한".repeat(100);
        let out2 = extract_last_activity_line(Some(&korean), 10).unwrap();
        assert_eq!(out2.chars().count(), 10);
        assert!(out2.ends_with('…'));
        // 경계값: 정확히 maxChars면 그대로
        let exact = "y".repeat(68);
        assert_eq!(extract_last_activity_line(Some(&exact), 68).as_deref(), Some(exact.as_str()));
    }

    #[test]
    fn activity_collapses_whitespace_and_handles_crlf() {
        let screen = "a\t\tb   c\r\n";
        assert_eq!(extract_last_activity_line(Some(screen), 68).as_deref(), Some("a b c"));
        assert_eq!(extract_last_activity_line(None, 68), None);
        assert_eq!(extract_last_activity_line(Some(""), 68), None);
    }

    // ---------- parse_hc_ls_json (엄격 계약) ----------

    #[test]
    fn parse_ls_preserves_prior_workers_on_invalid_payloads() {
        assert_eq!(parse_hc_ls_json("{}"), None);
        assert_eq!(parse_hc_ls_json(r#"{"terminals":null}"#), None);
        assert_eq!(parse_hc_ls_json("not json"), None);
        assert_eq!(parse_hc_ls_json("[1,2]"), None);
        assert_eq!(parse_hc_ls_json("\"str\""), None);
        assert_eq!(parse_hc_ls_json(r#"{"something": 1}"#), None);
    }

    #[test]
    fn parse_ls_explicit_empty_array_is_some_empty() {
        assert_eq!(parse_hc_ls_json(r#"{"terminals":[]}"#), Some(vec![]));
    }

    #[test]
    fn parse_ls_normalizes_entries_defensively() {
        let raw = r#"{"terminals":[
            {"address":"a1","agent":"pi","name":"w-1","state":"live","cwd":"/r","status":"working"},
            {"address":"a2","status":{"app":"pi","state":"done"}},
            {"agent":"no-address"},
            {"address":"","agent":"pi"},
            "garbage",
            null
        ]}"#;
        let out = parse_hc_ls_json(raw).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].address, "a1");
        assert_eq!(out[0].status.as_deref(), Some("working"));
        assert_eq!(out[1].address, "a2");
        assert_eq!(out[1].status.as_deref(), Some("done")); // 객체형 status 정규화
        assert_eq!(out[1].agent, None);
    }

    // ---------- 슬롯·모델 추론 ----------

    fn detail(addr: &str, name: &str, agent: &str, argv: &[&str], cwd: Option<&str>) -> HcTerminalDetail {
        HcTerminalDetail {
            address: addr.to_string(),
            agent: Some(agent.to_string()),
            name: Some(name.to_string()),
            parent: None,
            state: Some("live".to_string()),
            status: Some("working".to_string()),
            cwd: cwd.map(str::to_string),
            argv: Some(argv.iter().map(|s| s.to_string()).collect()),
        }
    }

    #[test]
    fn infer_requires_address() {
        let mut d = detail("a", "w-1", "pi", &[], None);
        assert!(infer_live_worker_slot(&d, None).is_some());
        d.address = String::new();
        assert!(infer_live_worker_slot(&d, None).is_none());
    }

    #[test]
    fn infer_slot_from_herdr_role_env_priority() {
        for (role, expect) in [
            ("lead", RoleSlot::Lead),
            ("explore", RoleSlot::Explore),
            ("verifier", RoleSlot::Verifier),
            ("advisor", RoleSlot::Advisor),
            ("coder", RoleSlot::Coder),
            ("carbon", RoleSlot::Advisor), // carbon→advisor 매핑
            ("LEAD", RoleSlot::Lead),      // 대소문자 무시
        ] {
            let d = detail(
                "a",
                "session-name",
                "pi",
                &[&format!("HERDR_ROLE={}", role), "pi"],
                None,
            );
            let w = infer_live_worker_slot(&d, None).unwrap();
            assert_eq!(w.slot, expect, "role={}", role);
        }
    }

    #[test]
    fn infer_slot_from_session_name_fallback() {
        for (name, expect) in [
            ("verify-1", RoleSlot::Verifier),
            ("w-verifier", RoleSlot::Verifier),
            ("exp-2", RoleSlot::Explore),
            ("explore-x", RoleSlot::Explore),
            ("advisor-1", RoleSlot::Advisor),
            ("carbon-9", RoleSlot::Advisor),
            ("lead-1", RoleSlot::Lead),
            ("w-plain", RoleSlot::Coder),
            ("coderr", RoleSlot::Coder), // 'coder' 접두가 아니라 contains 아님 — name은 규약 접두
        ] {
            let d = detail("a", name, "pi", &["pi"], None);
            let w = infer_live_worker_slot(&d, None).unwrap();
            assert_eq!(w.slot, expect, "name={}", name);
        }
    }

    #[test]
    fn infer_cli_model_strips_env_and_dg_wrap_prefixes() {
        // env + KEY=VAL + dg wrap -- 형태
        let d = detail(
            "a",
            "w-1",
            "pi",
            &[
                "env",
                "HERDR_ROLE=coder",
                "FOO=bar",
                "node",
                "/Users/x/dg.ts",
                "wrap",
                "--route",
                "hook",
                "--",
                "pi",
                "--provider",
                "opencodex",
                "--model",
                "glm-zcode/glm-5.3",
            ],
            None,
        );
        let w = infer_live_worker_slot(&d, None).unwrap();
        assert_eq!(w.model_name, "glm-zcode/glm-5.3");
        assert_eq!(w.short_model, "pi:glm-5.3");
        assert_eq!(w.badge, "pi:glm-5.3 (실행 중)");

        // --model= 형태 + devin
        let d2 = detail("b", "w-2", "devin", &["devin", "--model=devin-1"], None);
        let w2 = infer_live_worker_slot(&d2, None).unwrap();
        assert_eq!(w2.model_name, "devin-1");
        assert_eq!(w2.short_model, "devin:1"); // cli- 접두 제거로 TS와 동일

        // 절대경로 CLI 토큰 basename 인식
        let d3 = detail("c", "w-3", "pi", &["/usr/local/bin/codex", "--model", "gpt-5"], None);
        let w3 = infer_live_worker_slot(&d3, None).unwrap();
        assert_eq!(w3.model_name, "gpt-5");
        assert_eq!(w3.short_model, "codex:gpt-5");
    }

    #[test]
    fn infer_cli_falls_back_to_agent_then_custom() {
        let d = detail("a", "w-1", "Claude", &["wrapper-only"], None); // agent 대소문자 무시
        let w = infer_live_worker_slot(&d, None).unwrap();
        assert_eq!(w.model_name, "claude");
        assert_eq!(w.short_model, "claude");
        assert_eq!(w.badge, "claude (실행 중)");

        let d2 = detail("b", "w-2", "python", &["some-wrapper"], None);
        let w2 = infer_live_worker_slot(&d2, None).unwrap();
        assert_eq!(w2.model_name, "custom");
    }

    #[test]
    fn infer_model_flag_values_and_dash_model_rejected() {
        // `--model` 뒤가 플래그면 폴백(모델 없음)
        let d = detail("a", "w-1", "pi", &["pi", "--model", "--verbose"], None);
        let w = infer_live_worker_slot(&d, None).unwrap();
        assert_eq!(w.model_name, "pi");

        let d2 = detail("b", "w-2", "agy", &["agy", "--model", "gemini-4-neon-high"], None);
        let w2 = infer_live_worker_slot(&d2, None).unwrap();
        assert_eq!(w2.model_name, "gemini-4-neon-high");
        assert_eq!(w2.short_model, "agy:gemini-4-neon-high");
    }

    #[test]
    fn infer_activity_prefers_screen_then_status_fallback() {
        let d = detail("a", "w-1", "pi", &["pi"], None);
        let w = infer_live_worker_slot(&d, Some("editing file\n[hc] ok")).unwrap();
        assert_eq!(w.activity, "editing file");

        let mut d2 = detail("b", "w-2", "pi", &["pi"], None);
        d2.status = Some("done".to_string());
        let w2 = infer_live_worker_slot(&d2, None).unwrap();
        assert_eq!(w2.activity, "done 작업 중");

        let mut d3 = detail("c", "w-3", "pi", &["pi"], None);
        d3.status = None;
        let w3 = infer_live_worker_slot(&d3, None).unwrap();
        assert_eq!(w3.activity, "작업 실행 중");
    }

    #[test]
    fn infer_assembles_repo_tag_and_name_fallback() {
        let d = detail("addr-9", "", "pi", &["pi"], Some("/r/my-repo/.herdr/worktrees/c1"));
        let w = infer_live_worker_slot(&d, None).unwrap();
        assert_eq!(w.repo_tag, "my-repo");
        assert_eq!(w.name, "addr-9"); // 이름 없으면 address
        assert_eq!(w.cwd.as_deref(), Some("/r/my-repo/.herdr/worktrees/c1"));
    }

    // ---------- format_role_badge ----------

    #[test]
    fn role_badge_formats() {
        assert_eq!(format_role_badge("pi", None), "pi");
        assert_eq!(format_role_badge("pi", Some("glm-zcode/glm-5.3")), "pi:glm-5.3");
        assert_eq!(format_role_badge("pi", Some("  ")), "pi");
        // 중복 접두 정규화 glm-glm-5.3 → glm-5.3
        assert_eq!(format_role_badge("pi", Some("x/glm-glm-5.3")), "pi:glm-5.3");
        assert_eq!(format_role_badge("devin", Some("devin-1")), "devin:1"); // cli- 접두 제거
        // 경계: 중복이 아닌 접두는 유지
        assert_eq!(format_role_badge("pi", Some("glm-5.3")), "pi:glm-5.3");
        assert_eq!(format_role_badge("claude", Some("claude-sonnet-4-6")), "claude:sonnet-4-6");
        assert_eq!(format_role_badge("devin", Some("default")), "devin:default");
    }

    // ---------- merge_live_roster_labels ----------

    fn base_labels() -> RosterDisplayLabels {
        RosterDisplayLabels {
            profile_name: "quota-saver".into(),
            roster_source: "builtin".into(),
            lead: "standby-lead".into(),
            explore: "standby-explore".into(),
            coder: "standby-coder".into(),
            verifier: "standby-verifier".into(),
            advisor: "standby-advisor".into(),
        }
    }

    fn worker(slot: RoleSlot, short: &str, model: Option<&str>) -> LiveWorkerInfo {
        LiveWorkerInfo {
            slot,
            badge: format!("{} (실행 중)", short),
            short_model: short.to_string(),
            model_name: model.map(str::to_string).unwrap_or_else(|| short.to_string()),
            address: format!("addr-{}", short),
            name: short.to_string(),
            cwd: None,
            repo_tag: "r".into(),
            activity: "a".into(),
            status: None,
        }
    }

    #[test]
    fn merge_without_workers_returns_standby_labels() {
        let m = merge_live_roster_labels(&base_labels(), None, &[]);
        assert_eq!(m.labels.coder, "standby-coder");
        assert_eq!(m.active_worker_count, 0);
        assert_eq!(m.lead_model_flag, "standby-lead");
        assert_eq!(m.verifier_model_flag, "standby-verifier");
    }

    #[test]
    fn merge_overrides_slots_with_first_worker_per_slot() {
        let workers = vec![
            worker(RoleSlot::Coder, "pi:glm-5.3", Some("glm-zcode/glm-5.3")),
            worker(RoleSlot::Coder, "devin", None), // 동일 슬롯 2번째 — 무시
            worker(RoleSlot::Verifier, "devin", None),
        ];
        let m = merge_live_roster_labels(&base_labels(), None, &workers);
        assert_eq!(m.labels.coder, "pi:glm-5.3");
        assert_eq!(m.labels.verifier, "devin");
        assert_eq!(m.labels.explore, "standby-explore");
        assert_eq!(m.active_worker_count, 3);
        // 실측 모델명이 있고 short와 다르면 --model 플래그
        assert_eq!(m.lead_model_flag, "standby-lead"); // lead 워커 없음
        assert_eq!(m.verifier_model_flag, "devin"); // modelName == shortModel → 라벨 유지
    }

    #[test]
    fn merge_lead_worker_and_watched_lead_precedence() {
        let workers = vec![
            worker(RoleSlot::Lead, "pi:glm-5.3", Some("glm-zcode/glm-5.3")),
            worker(RoleSlot::Verifier, "pi:glm-5.3", Some("glm-zcode/glm-5.3")),
        ];
        // watchedLead 없음 — lead 워커는 플래그만 주도(라벨 슬롯 침범 없음 —
        // TS LIVE_OVERRIDE_SLOTS가 lead를 제외하는 원문 계약).
        let m = merge_live_roster_labels(&base_labels(), None, &workers);
        assert_eq!(m.labels.lead, "standby-lead");
        assert_eq!(m.lead_model_name, "standby-lead");
        assert_eq!(m.lead_model_flag, "--model glm-zcode/glm-5.3");
        // watchedLead 있음 — lead 슬롯 침범 금지, 워커 lead는 무시
        let m2 = merge_live_roster_labels(&base_labels(), Some("watching-lead"), &workers);
        assert_eq!(m2.labels.lead, "watching-lead");
        assert_eq!(m2.lead_model_flag, "watching-lead");
        // verifier 플래그는 여전히 실측 워커 기준
        assert_eq!(m2.verifier_model_flag, "--model glm-zcode/glm-5.3");
    }

    // ---------- LiveSyncSnapshotter ----------

    /// 테스트 env 격리 — 실제 ~/.config/herdr-quad/roster.json 유입을 차단한다.
    fn isolate_env(tmp: &Path) {
        std::env::remove_var("HERDR_ROSTER_PATH");
        let xdg = tmp.join("xdg-isolated");
        std::fs::create_dir_all(&xdg).unwrap();
        std::env::set_var("XDG_CONFIG_HOME", xdg);
    }

    /// 테스트용 mock 실행기 — 호출 기록 + 주소별 응답.
    struct MockHc {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        ls_json: Mutex<Vec<String>>, // tick마다 소진
        details: HashMap<String, String>,
        screens: HashMap<String, String>,
    }

    impl MockHc {
        fn new(ls_json: Vec<String>, details: HashMap<String, String>, screens: HashMap<String, String>) -> Self {
            MockHc {
                calls: Mutex::new(Vec::new()),
                ls_json: Mutex::new(ls_json),
                details,
                screens,
            }
        }

        fn call_count(&self, prefix: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(cmd, _)| cmd == prefix)
                .count()
        }
    }

    impl HcExecRunner for MockHc {
        fn exec(&self, _file: &str, args: &[&str], _timeout_ms: u64) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push((args[0].to_string(), args.iter().map(|s| s.to_string()).collect()));
            match args[0] {
                "ls" => {
                    // tick마다 응답 1건씩 소진(선입선출)
                    let mut queue = self.ls_json.lock().unwrap();
                    if queue.is_empty() {
                        Err("no more ls responses".to_string())
                    } else {
                        Ok(queue.remove(0))
                    }
                }
                "get" => {
                    let addr = args[1];
                    self.details
                        .get(addr)
                        .cloned()
                        .ok_or_else(|| format!("no detail for {}", addr))
                }
                "read" => {
                    let addr = args[1];
                    self.screens
                        .get(addr)
                        .cloned()
                        .ok_or_else(|| format!("no screen for {}", addr))
                }
                _ => Err("unknown subcommand".to_string()),
            }
        }
    }

    fn worker_ls_entry(addr: &str, name: &str) -> serde_json::Value {
        serde_json::json!({
            "address": addr,
            "agent": "pi",
            "name": name,
            "state": "live",
            "status": "working",
            "cwd": "/tmp/repo-a"
        })
    }

    fn snapshot_with_mock(mock: Arc<MockHc>, repo_dir: &Path) -> LiveSyncSnapshotter {
        LiveSyncSnapshotter::new(LiveSyncOptions {
            repo_dir: repo_dir.to_path_buf(),
            repo_filter_dir: None,
            hc_bin: "hc".to_string(),
            exec_runner: mock,
            get_watched_lead: None,
            on_update: None,
        })
    }

    #[test]
    fn snapshotter_tick_produces_workers_and_broadcasts() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-tick");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let details = HashMap::from([(
            "a1".to_string(),
            serde_json::json!({
                "address": "a1", "agent": "pi", "name": "w-1", "state": "live",
                "status": "working", "cwd": "/tmp/repo-a",
                "argv": ["pi", "--model", "glm-zcode/glm-5.3"]
            })
            .to_string(),
        )]);
        let screens = HashMap::from([("a1".to_string(), "editing main.rs\n[hc] ok".to_string())]);
        let mock = Arc::new(MockHc::new(
            vec![serde_json::json!({ "terminals": [worker_ls_entry("a1", "w-1")] }).to_string()],
            details,
            screens,
        ));

        let received = Arc::new(Mutex::new(Vec::<MergedLiveRoster>::new()));
        let sink = received.clone();
        let mut snap = LiveSyncSnapshotter::new(LiveSyncOptions {
            repo_dir: tmp.clone(),
            repo_filter_dir: None,
            hc_bin: "hc".to_string(),
            exec_runner: mock.clone(),
            get_watched_lead: None,
            on_update: Some(Arc::new(Mutex::new(move |m: &MergedLiveRoster| {
                sink.lock().unwrap().push(m.clone());
            }))),
        });
        let merged = snap.tick_now();
        assert_eq!(merged.active_worker_count, 1);
        assert_eq!(merged.workers[0].address, "a1");
        assert_eq!(merged.workers[0].slot, RoleSlot::Coder);
        assert_eq!(merged.workers[0].short_model, "pi:glm-5.3");
        assert_eq!(merged.workers[0].activity, "editing main.rs");
        assert_eq!(merged.workers[0].repo_tag, "repo-a");
        assert_eq!(merged.workers[0].status.as_deref(), Some("working"));
        // 라벨 오버라이드 — coder 슬롯이 라이브 배지로 교체
        assert_eq!(merged.labels.coder, "pi:glm-5.3");
        assert_eq!(received.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn snapshotter_observes_top_level_active_session_as_lead() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-toplevel");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        // 워커 규약 밖 최상위 working antigravity — Lead 카드로 관측된다
        let ls = serde_json::json!({
            "terminals": [
                {"address": "t155", "agent": "antigravity", "name": "paperwork-operations",
                 "state": "live", "status": "working", "cwd": "/tmp/ops", "parent": null},
                {"address": "t156", "agent": "antigravity", "name": "idle-session",
                 "state": "live", "status": "idle", "cwd": "/tmp/ops"},
                {"address": "t157", "agent": "zsh", "name": "shell",
                 "state": "live", "status": "working", "cwd": "/tmp/ops"}
            ]
        })
        .to_string();
        let details = HashMap::from([(
            "t155".to_string(),
            serde_json::json!({"address": "t155", "agent": "antigravity", "name": "paperwork-operations", "argv": ["antigravity"]}).to_string(),
        )]);
        let mock = Arc::new(MockHc::new(vec![ls], details, HashMap::new()));
        let mut snap = snapshot_with_mock(mock, &tmp);
        let merged = snap.tick_now();
        assert_eq!(merged.active_worker_count, 1);
        let w = &merged.workers[0];
        assert_eq!(w.slot, RoleSlot::Lead);
        assert_eq!(w.name, "paperwork-operations");
        assert_eq!(w.short_model, "antigravity");
    }

    #[test]
    fn snapshotter_reuses_detail_cache_across_ticks() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-cache");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let ls = serde_json::json!({ "terminals": [worker_ls_entry("a1", "w-1")] }).to_string();
        let details = HashMap::from([(
            "a1".to_string(),
            serde_json::json!({
                "address": "a1", "agent": "pi", "name": "w-1", "state": "live",
                "argv": ["pi", "--model", "glm-zcode/glm-5.3"]
            })
            .to_string(),
        )]);
        let mock = Arc::new(MockHc::new(vec![ls.clone(), ls], details, HashMap::new()));
        let mut snap = snapshot_with_mock(mock.clone(), &tmp);
        let m1 = snap.tick_now();
        assert_eq!(m1.active_worker_count, 1);
        assert_eq!(mock.call_count("get"), 1);
        // 두 번째 틱 — 같은 주소는 캐시 사용(get 미호출), read는 매 틱 상한 내 재수행
        let m2 = snap.tick_now();
        assert_eq!(m2.active_worker_count, 1);
        assert_eq!(mock.call_count("get"), 1, "detail cache must be reused");
        assert_eq!(mock.call_count("read"), 2);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn snapshotter_malformed_ls_preserves_prior_workers() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-preserve");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let good = serde_json::json!({ "terminals": [worker_ls_entry("a1", "w-1")] }).to_string();
        let details = HashMap::from([(
            "a1".to_string(),
            serde_json::json!({ "address": "a1", "agent": "pi", "name": "w-1", "argv": ["pi"] }).to_string(),
        )]);
        let mock = Arc::new(MockHc::new(vec![good, "not-json".to_string(), "{}".to_string()], details, HashMap::new()));
        let mut snap = snapshot_with_mock(mock, &tmp);
        let m1 = snap.tick_now();
        assert_eq!(m1.active_worker_count, 1);
        // 실행 오류·malformed·terminals 누락 모두 이전 워커 유지(허위 0건 방송 금지)
        let m2 = snap.tick_now();
        assert_eq!(m2.active_worker_count, 1, "exec error must preserve workers");
        let m3 = snap.tick_now();
        assert_eq!(m3.active_worker_count, 1, "{{}} payload must preserve workers");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn snapshotter_evicts_departed_addresses() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-evict");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let ls1 = serde_json::json!({ "terminals": [worker_ls_entry("a1", "w-1"), worker_ls_entry("a2", "w-2")] }).to_string();
        let ls2 = serde_json::json!({ "terminals": [worker_ls_entry("a1", "w-1")] }).to_string();
        let details = HashMap::from([
            ("a1".to_string(), serde_json::json!({ "address": "a1", "agent": "pi", "name": "w-1", "argv": ["pi"] }).to_string()),
            ("a2".to_string(), serde_json::json!({ "address": "a2", "agent": "pi", "name": "w-2", "argv": ["pi"] }).to_string()),
        ]);
        let mock = Arc::new(MockHc::new(vec![ls1, ls2], details, HashMap::new()));
        let mut snap = snapshot_with_mock(mock, &tmp);
        assert_eq!(snap.tick_now().active_worker_count, 2);
        assert_eq!(snap.tick_now().active_worker_count, 1);
        assert!(!snap.detail_cache.contains_key("a2"), "departed address must be evicted");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn snapshotter_caps_activity_reads_at_six_per_tick() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-cap");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let terminals: Vec<serde_json::Value> = (0..8)
            .map(|i| worker_ls_entry(&format!("a{}", i), &format!("w-{}", i)))
            .collect();
        let ls = serde_json::json!({ "terminals": terminals }).to_string();
        let details: HashMap<String, String> = (0..8)
            .map(|i| {
                (
                    format!("a{}", i),
                    serde_json::json!({ "address": format!("a{}", i), "agent": "pi", "name": format!("w-{}", i), "argv": ["pi"] }).to_string(),
                )
            })
            .collect();
        let mock = Arc::new(MockHc::new(vec![ls], details, HashMap::new()));
        let mut snap = snapshot_with_mock(mock.clone(), &tmp);
        let merged = snap.tick_now();
        assert_eq!(merged.active_worker_count, 8);
        assert_eq!(mock.call_count("read"), MAX_ACTIVITY_READS_PER_TICK);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn snapshotter_scope_filter_excludes_other_repos() {
        let tmp = temp_dir("live-scope");
        std::fs::create_dir_all(&tmp).unwrap();
        let repo = tmp.join("repo-a");
        std::fs::create_dir_all(&repo).unwrap();
        let mut inside = worker_ls_entry("a1", "w-1");
        inside["cwd"] = serde_json::json!(repo.to_str().unwrap());
        let mut outside = worker_ls_entry("a2", "w-2");
        outside["cwd"] = serde_json::json!("/tmp/other-repo");
        let ls = serde_json::json!({ "terminals": [inside, outside] }).to_string();
        let details = HashMap::from([
            ("a1".to_string(), serde_json::json!({ "address": "a1", "agent": "pi", "name": "w-1", "argv": ["pi"] }).to_string()),
        ]);
        let mock = Arc::new(MockHc::new(vec![ls], details, HashMap::new()));
        let _g: MutexGuard<'_, ()> = env_lock();
        isolate_env(&tmp);
        let mut snap = LiveSyncSnapshotter::new(LiveSyncOptions {
            repo_dir: repo.clone(),
            repo_filter_dir: Some(repo.clone()),
            hc_bin: "hc".to_string(),
            exec_runner: mock,
            get_watched_lead: None,
            on_update: None,
        });
        let merged = snap.tick_now();
        assert_eq!(merged.active_worker_count, 1);
        assert_eq!(merged.workers[0].address, "a1");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn snapshotter_hc_enoent_falls_back_to_builtin_labels() {
        let tmp = temp_dir("live-enoent");
        std::fs::create_dir_all(&tmp).unwrap();
        // 실제 DefaultHcRunner — 존재하지 않는 바이너리는 Err(ENOENT) → 조용히 폴백
        let _g: MutexGuard<'_, ()> = env_lock();
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let mut snap = LiveSyncSnapshotter::new(LiveSyncOptions {
            repo_dir: tmp.clone(),
            repo_filter_dir: None,
            hc_bin: "definitely-not-a-real-hc-binary-xyz".to_string(),
            exec_runner: Arc::new(DefaultHcRunner),
            get_watched_lead: None,
            on_update: None,
        });
        let merged = snap.tick_now();
        assert_eq!(merged.active_worker_count, 0);
        // 내장 quota-saver 라벨 폴백
        assert!(!merged.labels.lead.is_empty());
        assert_eq!(merged.labels.profile_name, "quota-saver");
        assert_eq!(merged.labels.roster_source, "builtin");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn snapshotter_hot_reloads_roster_on_change() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-hotreload");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let repo = tmp.join("repo");
        std::fs::create_dir_all(repo.join(".herdr")).unwrap();
        let empty_ls = serde_json::json!({ "terminals": [] }).to_string();
        let mock = Arc::new(MockHc::new(vec![empty_ls.clone(), empty_ls], HashMap::new(), HashMap::new()));
        let mut snap = snapshot_with_mock(mock, &repo);

        let m1 = snap.tick_now();
        assert_eq!(m1.labels.profile_name, "quota-saver"); // roster 없음 → 내장

        // 프로젝트 roster.json 작성 — profile 이름이 바뀌면 라벨에 즉시 반영
        std::fs::write(
            repo.join(".herdr").join("roster.json"),
            serde_json::json!({
                "defaultProfile": "p1",
                "profiles": { "p1": { "roles": { "lead": { "cli": "pi", "model": "x/alpha-1" } } } }
            })
            .to_string(),
        )
        .unwrap();
        let m2 = snap.tick_now();
        assert_eq!(m2.labels.profile_name, "p1");
        assert_eq!(m2.labels.roster_source, "project");
        assert_eq!(m2.labels.lead, "pi:alpha-1");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn snapshotter_stop_is_idempotent_and_freezes() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-stop");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let ls = serde_json::json!({ "terminals": [worker_ls_entry("a1", "w-1")] }).to_string();
        let details = HashMap::from([(
            "a1".to_string(),
            serde_json::json!({ "address": "a1", "agent": "pi", "name": "w-1", "argv": ["pi"] }).to_string(),
        )]);
        let mock = Arc::new(MockHc::new(vec![ls], details, HashMap::new()));
        let mut snap = snapshot_with_mock(mock.clone(), &tmp);
        assert_eq!(snap.tick_now().active_worker_count, 1);
        snap.stop();
        snap.stop();
        assert!(snap.is_stopped());
        // 정지 후 tick은 이전 상태 스냅숏만 반환(추가 hc 호출 없음)
        let before = mock.call_count("ls");
        let merged = snap.tick_now();
        assert_eq!(merged.active_worker_count, 1);
        assert_eq!(mock.call_count("ls"), before);
        assert!(snap.detail_cache.is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn snapshotter_status_transition_reflected_via_ls_merge() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-status");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        // 캐시된 detail의 구 status를 매 틱 ls 요약이 덮어쓰는지 확인
        let mut e1 = worker_ls_entry("a1", "w-1");
        e1["status"] = serde_json::json!("working");
        let mut e2 = worker_ls_entry("a1", "w-1");
        e2["status"] = serde_json::json!("done");
        let ls1 = serde_json::json!({ "terminals": [e1] }).to_string();
        let ls2 = serde_json::json!({ "terminals": [e2] }).to_string();
        let details = HashMap::from([(
            "a1".to_string(),
            serde_json::json!({ "address": "a1", "agent": "pi", "name": "w-1", "argv": ["pi"], "status": "stale" }).to_string(),
        )]);
        let mock = Arc::new(MockHc::new(vec![ls1, ls2], details, HashMap::new()));
        let mut snap = snapshot_with_mock(mock, &tmp);
        let m1 = snap.tick_now();
        assert_eq!(m1.workers[0].status.as_deref(), Some("working"));
        let m2 = snap.tick_now();
        assert_eq!(m2.workers[0].status.as_deref(), Some("done"), "fresh ls status must override cached detail");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn builtin_labels_match_quota_saver() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-builtin-labels");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let labels = builtin_fallback_labels();
        let cfg = builtin_roster_config();
        let expect = roster_labels_from_profile("quota-saver", "builtin", &cfg.profiles["quota-saver"]);
        assert_eq!(labels, expect);
    }
}
