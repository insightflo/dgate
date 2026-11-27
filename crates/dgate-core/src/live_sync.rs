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
    /// `hc ls --json`의 `turn` — 현재까지 실측된 턴 수(관측 불가 시 None).
    pub turn: Option<u64>,
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
            Some(v) if v.is_object() => v.get("state").and_then(Value::as_str).map(str::to_string),
            _ => None,
        };
        Some(HcTerminalSummary {
            address: address.to_string(),
            agent: raw.get("agent").and_then(Value::as_str).map(str::to_string),
            name: raw.get("name").and_then(Value::as_str).map(str::to_string),
            parent: raw
                .get("parent")
                .and_then(Value::as_str)
                .map(str::to_string),
            state: raw.get("state").and_then(Value::as_str).map(str::to_string),
            status,
            cwd: raw.get("cwd").and_then(Value::as_str).map(str::to_string),
            turn: raw.get("turn").and_then(Value::as_u64),
        })
    }
}

/// `hc get <addr> --json` 터미널 항목(상세) — argv 및 현재 foreground.argv 포함.
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
    /// 현재 터미널 포그라운드 프로세스 argv — 있으면 초기 셸 런치 argv보다 우선.
    pub foreground_argv: Option<Vec<String>>,
    /// 현재 터미널 포그라운드 프로세스 cwd — `cd <other_repo> && agy`처럼 셸 초기 cwd와 다를 때 우선.
    pub foreground_cwd: Option<String>,
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
            foreground_argv: None,
            foreground_cwd: None,
        }
    }
}

/// 라이브 워커 슬롯 6종 — TS `LIVE_WORKER_SLOTS`(src/types.ts) 미러.
/// roster 역할 슬롯(`RoleSlot` 5종)과 분리한다: sublead는 라이브 관측·wrap `--role`용
/// 슬롯일 뿐 roster.json 역할·비용 스펙이 아니다(Phase 1 슬롯 신원 — docs/sublead-design.md).
/// 어휘·값·의미는 Node v1과 일치해야 한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LiveWorkerSlot {
    Lead,
    Sublead,
    Explore,
    Coder,
    Verifier,
    Advisor,
}

impl LiveWorkerSlot {
    pub fn as_str(&self) -> &'static str {
        match self {
            LiveWorkerSlot::Lead => "lead",
            LiveWorkerSlot::Sublead => "sublead",
            LiveWorkerSlot::Explore => "explore",
            LiveWorkerSlot::Coder => "coder",
            LiveWorkerSlot::Verifier => "verifier",
            LiveWorkerSlot::Advisor => "advisor",
        }
    }

    /// `--role`·`HERDR_ROLE` 값 → 슬롯(TS `parseLiveWorkerSlot` 미러) — 대소문자·앞뒤
    /// 공백은 무시하고, `carbon`은 advisor 별칭, 미등값은 None(호출자가 폴백을 정한다).
    pub fn parse(value: &str) -> Option<Self> {
        let v = value.trim().to_lowercase();
        match v.as_str() {
            "lead" => Some(LiveWorkerSlot::Lead),
            "sublead" => Some(LiveWorkerSlot::Sublead),
            "explore" => Some(LiveWorkerSlot::Explore),
            "coder" => Some(LiveWorkerSlot::Coder),
            "verifier" => Some(LiveWorkerSlot::Verifier),
            "advisor" | "carbon" => Some(LiveWorkerSlot::Advisor),
            _ => None,
        }
    }

    /// roster 역할 슬롯으로 환원 — sublead는 roster 역할이 아니므로 None(라벨 오버라이드 대상 아님).
    pub fn as_role(&self) -> Option<RoleSlot> {
        match self {
            LiveWorkerSlot::Lead => Some(RoleSlot::Lead),
            LiveWorkerSlot::Sublead => None,
            LiveWorkerSlot::Explore => Some(RoleSlot::Explore),
            LiveWorkerSlot::Coder => Some(RoleSlot::Coder),
            LiveWorkerSlot::Verifier => Some(RoleSlot::Verifier),
            LiveWorkerSlot::Advisor => Some(RoleSlot::Advisor),
        }
    }

    /// 이름(세션명·라벨)의 **접두**로 슬롯 판정(TS `inferLiveWorkerSlotFromName` 미러).
    /// - ASCII 접두는 뒤에 `-`·`_`·숫자·끝이 와야 한다(`submit-3`≠sub, `expandex`≠exp).
    /// - 한글 접두는 구분자 없이 붙여도 인정한다(`검증-g1`·`검증g1`).
    /// - `w-` 접두는 hc 세션명 규약상 벗겨고 본다. coder는 라벨로 유도하지 않는다.
    /// 부분 문자열 판정을 쓰지 않는다 — 주제에 역할어가 들어간 coder 라벨 오분류 방지.
    pub fn infer_from_name(name: &str) -> Option<Self> {
        let lower = name.trim().to_lowercase();
        let n = lower.strip_prefix("w-").unwrap_or(lower.as_str());
        if n.is_empty() {
            return None;
        }
        let matches_prefix = |prefix: &str| -> bool {
            n.strip_prefix(prefix).is_some_and(|rest| {
                if prefix.chars().all(|c| c.is_ascii_lowercase()) {
                    rest.is_empty()
                        || rest.starts_with('-')
                        || rest.starts_with('_')
                        || rest.as_bytes()[0].is_ascii_digit()
                } else {
                    true // 한글 접두 — 구분자 없이 붙여도 인정
                }
            })
        };
        // TS `SLOT_NAME_PREFIXES` 삽입 순서 미러 — 접두끼리 겹치지 않아 순서는 결과에 영향 없다.
        // (`sub` 접두는 `^sub(?:[-_\d]|$)` 규칙상 `sublead-x`·`submit-3` 모두와 불일치하고,
        // `sublead-x`는 같은 슬롯의 더 긴 접두 `sublead`가 잡는다.)
        let table: [(Self, &[&str]); 5] = [
            (Self::Lead, &["lead", "argon"]),
            (Self::Sublead, &["sublead", "sub"]),
            (Self::Verifier, &["verifier", "verify", "neon", "검증"]),
            (Self::Explore, &["explore", "exp", "research", "탐색", "조사"]),
            (Self::Advisor, &["advisor", "carbon", "자문"]),
        ];
        for (slot, prefixes) in table {
            if prefixes.iter().any(|p| matches_prefix(p)) {
                return Some(slot);
            }
        }
        None
    }
}

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
    pub fn get_slot(&self, slot: &RoleSlot) -> &String {
        match slot {
            RoleSlot::Lead => &self.lead,
            RoleSlot::Explore => &self.explore,
            RoleSlot::Coder => &self.coder,
            RoleSlot::Verifier => &self.verifier,
            RoleSlot::Advisor => &self.advisor,
        }
    }

    pub fn set_slot(&mut self, slot: &RoleSlot, value: String) {
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
    /// pid:N(wrap) 워커를 소유한 hc 터미널 주소 — sublead 클러스터 그룹핑용(고아·직속 워커는 None).
    pub owner_address: Option<String>,
    pub name: String,
    pub cwd: Option<String>,
    /// 레포 basename — `.herdr/worktrees/<wt>`는 소속 레포명으로 환원
    pub repo_tag: String,
    /// `hc read`에서 뽑은 정화된 1줄 활동 (없으면 간결 폴백)
    pub activity: String,
    /// `hc` 터미널 상태('working'·'done' 등) — 카드 완료 표시·완료 로그 판정용(선택).
    pub status: Option<String>,
    /// `hc ls` 실측 턴 수 — LIVE 카드 'turn N' pill 원천(관측 불가 시 None).
    pub turn: Option<u64>,
    /// 실측 CLI 이름 (예: 'agy', 'pi', 'devin') — roster.json 미사용.
    pub cli: String,
    /// 실측 모델명 (예: 'Gemini 4 Neon', 'glm-zcode/glm-5.3') — 미확인 시 None.
    pub model: Option<String>,
    /// 실측 effort/thinking (예: 'medium', 'max', 'high') — 미확인 시 None.
    pub effort: Option<String>,
    /// 리드 세션이 내장 서브에이전트(invoke_subagent)를 실행 중일 때 경고 문구(없으면 None).
    pub subagent_warning: Option<String>,
    /// 리드 세션의 `--conversation` transcript에서 추출한 최근 사용자 입력·리드 응답.
    pub recent_io: Vec<TranscriptIoEntry>,
    /// 워커의 최근 출력 줄(`dg wrap` 실시간 로그 또는 `hc read` 꼬리, 시크릿 리덕션 완료).
    pub recent_output: Vec<String>,
}

/// transcript.jsonl에서 추출한 사용자 입력 / 리드 응답 1건.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptIoEntry {
    pub time_hhmmss: String,
    /// `"user"` | `"assistant"`
    pub role: String,
    pub text: String,
}

impl LiveWorkerInfo {
    /// 카드 하단 표시용 실측 요약 (`cli · model · effort`).
    pub fn telemetry_line(&self) -> String {
        format_live_telemetry(
            Some(self.cli.as_str()),
            self.model.as_deref(),
            self.effort.as_deref(),
        )
    }
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
    "zsh",
    "bash",
    "fish",
    "sh",
    "ksh",
    "dash",
    "pwsh",
    "powershell.exe",
    "antigravity",
];

/// parent 매칭 판정에 쓰는 워커 CLI 에이전트 목록.
const PARENT_MATCH_AGENTS: [&str; 7] = [
    "pi", "devin", "claude", "codex", "gemini", "opencode", "hermes",
];

/// 워커 세션명 접두 — herdr-quad-launch/dispatch가 만드는 이름 규약.
/// `sublead`(대시 없음·`coder`와 같은 형식)와 `sub-`는 sublead 스코프 리드 세션이
/// parent 없이 떠도 워커로 관측되게 한다 — 이 접두가 없으면 parent 없는 sublead 세션이
/// 최상위 리드 후보로 오인되어 Lead 카드로 정규화된다(단일 리드 불변식 위반).
const WORKER_NAME_PREFIXES: [&str; 11] = [
    "w-",
    "exp-",
    "explore-",
    "coder",
    "verify-",
    "verifier-",
    "lead-",
    "sublead",
    "sub-",
    "advisor-",
    "carbon-",
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
const TOP_LEVEL_AI_AGENTS: [&str; 9] = [
    "antigravity",
    "pi",
    "claude",
    "codex",
    "devin",
    "gemini",
    "agy",
    "opencode",
    "hermes",
];

/// 모니터·GUI 전용 세션명 — 최상위 세션 관측에서도 제외.
fn is_monitor_or_gui_name(name: &str) -> bool {
    re_monitor_name().is_match(name) || name.starts_with("mon-") || name.starts_with("dgate-gui")
}

/// `hc ls` 터미널이 "관측 대상"인지 판정 — 기존 워커 규약 + 최상위 활성 AI 세션.
/// is_worker_terminal 동작은 그대로 두고 조합만 확장한다.
pub fn is_observable_terminal(t: &HcTerminalSummary) -> bool {
    is_worker_terminal(t) || is_active_top_level_session(t)
}

/// sublead 세션명 규약 — 최상위 리드 후보에서 제외할 때 쓰는 정확한 접두 판정.
fn is_sublead_name(name: &str) -> bool {
    name == "sublead" || name.starts_with("sublead-") || name.starts_with("sub-")
}

/// 최상위(parent 없음) 활성 AI 에이전트 세션 판정 — 리드 관측 확장:
/// ① `state === 'live'` ② `status ∈ {working, idle, done}` (None 불가 — 종료·불명 제외)
/// ③ parent 부재·빈 값 ④ 모니터·GUI(dg-monitor*/mon-*/dgate-gui*) 제외
/// ⑤ agent(소문자)가 AI 코딩 에이전트 7종 안팎.
/// ⑥ sublead 이름 규약(`sublead`·`sublead-`·`sub-`) 제외 — sublead는 parent를 가진
///    스코프 리드다(단일 리드 불변식). 이름으로 선언된 sublead는 부모 없이 떠도
///    최상위 리드 후보가 될 수 없다(Lead 정규화 대상 아님, 워커 접두로 관측됨).
/// idle/done도 관측해 'antigravity (대기 중)'처럼 리드 자리가 사라지지 않게 한다.
pub fn is_active_top_level_session(t: &HcTerminalSummary) -> bool {
    if t.state.as_deref() != Some("live") {
        return false;
    }
    if !matches!(
        t.status.as_deref(),
        Some("working") | Some("idle") | Some("done")
    ) {
        return false;
    }
    if t.parent
        .as_deref()
        .map(str::trim)
        .is_some_and(|p| !p.is_empty())
    {
        return false;
    }
    let name = t.name.as_deref().unwrap_or("");
    if is_monitor_or_gui_name(name) {
        return false;
    }
    if is_sublead_name(name) {
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

/// Windows `canonicalize`는 `\\?\C:\…` verbatim 경로를 반환하지만, 미존재 경로의
/// resolve 폴백은 `C:\…` 일반 형태를 낸다 — 두 형태가 섞이면 접두 비교가 어긋난다.
/// `\\?\` 접두를 벗겨 비교 형태를 통일한다(UNC는 `\\server\share`로 환원).
fn strip_verbatim_prefix(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix("\\\\?\\UNC\\") {
            return PathBuf::from(format!("\\\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix("\\\\?\\") {
            return PathBuf::from(rest);
        }
    }
    p
}

/// 워커 cwd가 `--repo <dir>` 스코프 안에 있는지 판정.
/// 필터 없음(None)이면 전역 관측(항상 true), 워커 cwd 미상이면 스코프 지정 시 false.
/// 워크트리 서브디렉터리(`.herdr/worktrees/…`)도 소속 레포 스코프에 포함된다.
/// 경로가 연결된 git worktree 내부라면 `.git` 파일(`gitdir: <main>/.git/worktrees/<name>`)을
/// 순수 파일 읽기로 추적해 원본 메인 저장소 루트 경로를 반환한다(외부 `git` 프로세스 스폰 0회).
fn resolve_linked_worktree_main_repo(path: &Path) -> Option<PathBuf> {
    let mut cur = path.to_path_buf();
    for _ in 0..8 {
        let dot_git = cur.join(".git");
        if dot_git.is_dir() {
            return None; // 일반 메인 저장소 — 워크트리 아님
        }
        if dot_git.is_file() {
            let content = std::fs::read_to_string(&dot_git).ok()?;
            let gitdir_raw = content
                .lines()
                .find_map(|l| l.trim().strip_prefix("gitdir:"))?
                .trim();
            if gitdir_raw.is_empty() {
                return None;
            }
            let gitdir_path = if Path::new(gitdir_raw).is_absolute() {
                PathBuf::from(gitdir_raw)
            } else {
                cur.join(gitdir_raw)
            };
            let normalized = strip_verbatim_prefix(resolve_path_best(
                gitdir_path.to_str().unwrap_or(gitdir_raw),
            ));
            // `<main_repo>/.git/worktrees/<wt_name>` → `<main_repo>` 추출
            let mut ancestor = normalized.as_path();
            while let Some(parent) = ancestor.parent() {
                if ancestor.file_name().and_then(|n| n.to_str()) == Some(".git") {
                    return Some(parent.to_path_buf());
                }
                ancestor = parent;
            }
            return None;
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

pub fn matches_repo_scope(worker_cwd: Option<&str>, repo_filter_dir: Option<&str>) -> bool {
    let Some(filter) = repo_filter_dir.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    let Some(worker) = worker_cwd.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let worker_path = strip_verbatim_prefix(resolve_path_best(worker));
    let scope_path = strip_verbatim_prefix(resolve_path_best(filter));
    // 컴포넌트 단위 접두 — `/repo2/x`가 `/repo` 스코프에 잘못 포함되지 않고,
    // 루트 스코프('/' 또는 'C:\')는 그 하위 전부를 포함한다(구분자 더블링 사고 없음).
    if worker_path.starts_with(&scope_path) {
        return true;
    }
    // 레포 바깥 형제 디렉터리(`.worktrees/<repo>/...`, `../<repo>-worktrees/...`)에 만든
    // 연결 git worktree도 `.git` 파일의 `gitdir:`이 같은 메인 저장소를 가리키면 스코프에 포함한다.
    let scope_main = resolve_linked_worktree_main_repo(&scope_path).unwrap_or(scope_path);
    if let Some(worker_main) = resolve_linked_worktree_main_repo(&worker_path) {
        if worker_main.starts_with(&scope_main) || scope_main.starts_with(&worker_main) {
            return worker_main == scope_main;
        }
    }
    false
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
    RE.get_or_init(|| regex::Regex::new(r"^[\s─═┌┐└┘│├┤┬┴┼║╔╗╚╝╠╣╦╩╬\-_=·]+$").unwrap())
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

fn re_agy_footer() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)·\s+([^·]+?)\s+·\s+(minimal|low|medium|high|xhigh|max)(?:\s+·|\s*$)",
        )
        .unwrap()
    })
}

fn re_pi_footer() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)(?:\([^)]+\)\s+)?([^\s•]+)\s+•\s+(off|minimal|low|medium|high|xhigh|max)\b")
            .unwrap()
    })
}

fn re_status_chrome_line() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)^(?:esc to cancel\b|\?\s+for shortcuts\b|ctx\s+\d+%|↑\d+[kKmM]?\s+↓\d+[kKmM]?)",
        )
        .unwrap()
    })
}

/// 라이브 터미널 화면 하단 상태줄에서 실측 `(model, effort)`를 추출한다.
/// - Antigravity(agy): `auto · Gemini 4 Neon · medium`
/// - Pi(pi): `(opencodex) glm-zcode/glm-5.3 • max`
pub fn parse_live_screen_telemetry(raw_screen: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(raw) = raw_screen.filter(|s| !s.is_empty()) else {
        return (None, None);
    };
    let stripped = strip_ansi(&raw.replace("\r\n", "\n"));
    for line in stripped.split('\n').rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(cap) = re_agy_footer().captures(trimmed) {
            let model = cap.get(1).map(|m| m.as_str().trim().to_string()).filter(|s| !s.is_empty());
            let effort = cap.get(2).map(|m| m.as_str().to_lowercase());
            if model.is_some() {
                return (model, effort);
            }
        }
        if let Some(cap) = re_pi_footer().captures(trimmed) {
            let model = cap.get(1).map(|m| m.as_str().trim().to_string()).filter(|s| !s.is_empty());
            let effort = cap.get(2).map(|m| m.as_str().to_lowercase());
            if model.is_some() {
                return (model, effort);
            }
        }
    }
    (None, None)
}

/// argv에서 `--effort` / `--thinking` 값을 추출한다.
fn parse_worker_effort(argv: &[String]) -> Option<String> {
    let mut i = 0;
    while i < argv.len() {
        let t = argv[i].as_str();
        if (t == "--effort" || t == "--thinking") && i + 1 < argv.len() {
            let next = argv[i + 1].trim();
            if !next.is_empty() && !next.starts_with('-') {
                return Some(next.to_lowercase());
            }
        } else if let Some(v) = t
            .strip_prefix("--effort=")
            .or_else(|| t.strip_prefix("--thinking="))
        {
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_lowercase());
            }
        }
        i += 1;
    }
    None
}

/// 실측 `cli · model · effort` 한 줄 포맷(확인된 항목만 조합).
pub fn format_live_telemetry(
    cli: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    let c = cli.map(str::trim).unwrap_or("");
    if !c.is_empty() && c != "custom" {
        parts.push(c.to_string());
    }
    if let Some(m_raw) = model.map(str::trim).filter(|s| !s.is_empty()) {
        let after_slash = match m_raw.rfind('/') {
            Some(idx) => {
                let tail = m_raw[idx + 1..].trim();
                if tail.is_empty() { m_raw } else { tail }
            }
            None => m_raw,
        };
        // 중복 벤더 접두 정리 (예: `deepseek-deepseek-v4.1-flash` → `deepseek-v4.1-flash`)
        let compact = if let Some((first, rest)) = after_slash.split_once('-') {
            if !first.is_empty()
                && rest.len() > first.len() + 1
                && rest[..first.len()].eq_ignore_ascii_case(first)
                && rest.as_bytes()[first.len()] == b'-'
            {
                rest
            } else {
                after_slash
            }
        } else {
            after_slash
        };
        if !compact.eq_ignore_ascii_case(c) {
            parts.push(compact.to_string());
        }
    }
    if let Some(e) = effort.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(e.to_lowercase());
    }
    if parts.is_empty() {
        if c.is_empty() {
            "live".to_string()
        } else {
            c.to_string()
        }
    } else {
        parts.join(" · ")
    }
}

/// argv에서 `--conversation=<id>` 또는 `--conversation <id>`를 추출한다.
pub fn extract_conversation_id_from_argv(argv: &[String]) -> Option<String> {
    let is_uuid = |s: &str| {
        s.len() >= 8
            && s.chars()
                .all(|c| c.is_ascii_hexdigit() || c == '-')
    };
    let mut i = 0;
    while i < argv.len() {
        let t = argv[i].as_str();
        if let Some(id) = t.strip_prefix("--conversation=").map(str::trim) {
            if is_uuid(id) {
                return Some(id.to_string());
            }
        } else if t == "--conversation" && i + 1 < argv.len() {
            let id = argv[i + 1].trim();
            if is_uuid(id) {
                return Some(id.to_string());
            }
        }
        i += 1;
    }
    None
}

fn re_subagent_created_id() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r#"\\?"conversationId\\?"\s*:\s*\\?"([0-9a-fA-F-]{8,})\\?""#).unwrap()
    })
}

fn re_subagent_list_state() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r#"\\?"conversationId\\?"\s*:\s*\\?"([0-9a-fA-F-]{8,})\\?"[^}]*?\\?"state\\?"\s*:\s*\\?"(running|idle)\\?""#,
        )
        .unwrap()
    })
}

fn re_subagent_sender() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"sender=([0-9a-fA-F-]{8,})").unwrap())
}

/// Antigravity transcript.jsonl 꼬리(JSONL)에서 현재 실행 중인 내장 서브에이전트(`invoke_subagent`)를
/// 감지해 경고 배지 문구를 반환한다. 모두 완료되었으면 None.
pub fn detect_subagent_warning_from_transcript(raw_tail: Option<&str>) -> Option<String> {
    let raw = raw_tail.filter(|s| !s.is_empty())?;
    let mut active_ids: HashSet<String> = HashSet::new();
    let mut pending_invoke = false;

    for line in raw.lines() {
        if line.is_empty() {
            continue;
        }
        if line.contains("\"invoke_subagent\"") && line.contains("\"tool_calls\"") {
            pending_invoke = true;
        }
        if line.contains("Created the following subagent:") {
            if let Some(cap) = re_subagent_created_id().captures(line) {
                if let Some(id) = cap.get(1) {
                    active_ids.insert(id.as_str().to_string());
                    pending_invoke = false;
                }
            }
        }
        if line.contains("active subagent(s):") {
            active_ids.clear();
            pending_invoke = false;
            for cap in re_subagent_list_state().captures_iter(line) {
                if cap.get(2).map(|m| m.as_str()) == Some("running") {
                    if let Some(id) = cap.get(1) {
                        active_ids.insert(id.as_str().to_string());
                    }
                }
            }
        }
        if line.contains("sender=") {
            for cap in re_subagent_sender().captures_iter(line) {
                if let Some(id) = cap.get(1) {
                    active_ids.remove(id.as_str());
                }
            }
            pending_invoke = false;
        }
        if line.contains("\"manage_subagents\"") && line.contains("kill_all") {
            active_ids.clear();
            pending_invoke = false;
        }
    }

    let count = active_ids.len() + usize::from(pending_invoke);
    if count > 0 {
        Some(format!(
            "⚠ 내장 서브에이전트 실행 중 ({}개 · 관측 불가)",
            count
        ))
    } else {
        None
    }
}

const TRANSCRIPT_TAIL_BYTES: u64 = 128 * 1024;
const MAX_RECENT_IO_ENTRIES: usize = 12;

fn format_iso_hhmmss(iso: Option<&str>) -> String {
    let Some(s) = iso.filter(|s| s.len() >= 19) else {
        return String::new();
    };
    // ISO8601 `YYYY-MM-DDTHH:MM:SSZ` — UTC 시각이 들어오면 로컬 오프셋 보정 대신
    // chrono 없이도 안전하게 HH:MM:SS를 추출(또는 기존 localtime 포맷 사용).
    if let Some(t_idx) = s.find('T') {
        let rest = &s[t_idx + 1..];
        if rest.len() >= 8 {
            return rest[..8].to_string();
        }
    }
    String::new()
}

fn strip_xml_block(input: &str, tag: &str) -> String {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let mut s = input.to_string();
    while let Some(start) = s.find(&open) {
        if let Some(end_rel) = s[start..].find(&close) {
            s.replace_range(start..start + end_rel + close.len(), "");
        } else {
            break;
        }
    }
    s
}

/// Antigravity / Pi / Claude Code / Codex / Gemini CLI transcript 꼬리에서
/// 최근 사용자 입력·리드 응답을 최대 `limit`건 추출한다(오래된→최신 순, 시크릿 리덕션 적용).
pub fn extract_recent_io_from_transcript(
    raw_tail: Option<&str>,
    limit: usize,
) -> Vec<TranscriptIoEntry> {
    let Some(raw) = raw_tail.filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    let push_clean = |entries: &mut Vec<TranscriptIoEntry>,
                      role: &str,
                      content: &str,
                      iso_ts: Option<&str>| {
        if content.trim().is_empty() {
            return;
        }
        let mut redacted = redact_secrets(content);
        redacted = strip_xml_block(&redacted, "CONTEXT_SUMMARY");
        redacted = strip_xml_block(&redacted, "ADDITIONAL_METADATA");
        redacted = strip_xml_block(&redacted, "USER_SETTINGS_CHANGE");
        redacted = redacted
            .replace("<USER_REQUEST>", "")
            .replace("</USER_REQUEST>", "");
        let cleaned = re_whitespace_run()
            .replace_all(&redacted, " ")
            .trim()
            .to_string();
        if cleaned.is_empty() || cleaned == "." || cleaned.starts_with("# AGENTS.md instructions") {
            return;
        }
        entries.push(TranscriptIoEntry {
            time_hhmmss: format_iso_hhmmss(iso_ts),
            role: role.to_string(),
            text: truncate_chars(&cleaned, 160),
        });
    };
    let extract_blocks = |val: Option<&Value>, types: &[&str]| -> String {
        match val {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|b| {
                    let bo = b.as_object()?;
                    let bt = bo.get("type").and_then(Value::as_str).unwrap_or("");
                    if types.contains(&bt) {
                        bo.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        }
    };

    // Gemini CLI session JSON (`{ messages: [{ type: "user"|"gemini", content }] }`)
    let trimmed = raw.trim_start();
    if trimmed.starts_with('{') && trimmed.contains("\"messages\"") && !trimmed.contains("\n{") {
        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(trimmed) {
            if let Some(Value::Array(msgs)) = obj.get("messages") {
                for m in msgs {
                    let Some(mo) = m.as_object() else { continue };
                    let mt = mo.get("type").and_then(Value::as_str).unwrap_or("");
                    if mt == "user" || mt == "gemini" {
                        let txt = extract_blocks(mo.get("content"), &["text"]);
                        push_clean(
                            &mut entries,
                            if mt == "user" { "user" } else { "assistant" },
                            &txt,
                            mo.get("timestamp").and_then(Value::as_str),
                        );
                    }
                }
                return if entries.len() > limit {
                    entries.split_off(entries.len() - limit)
                } else {
                    entries
                };
            }
        }
    }

    for line in raw.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ty = obj.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "USER_INPUT" | "PLANNER_RESPONSE" => {
                let txt = obj.get("content").and_then(Value::as_str).unwrap_or("");
                push_clean(
                    &mut entries,
                    if ty == "USER_INPUT" {
                        "user"
                    } else {
                        "assistant"
                    },
                    txt,
                    obj.get("created_at").and_then(Value::as_str),
                );
            }
            "message" => {
                // Pi
                if let Some(msg) = obj.get("message").and_then(Value::as_object) {
                    let r = msg.get("role").and_then(Value::as_str).unwrap_or("");
                    if r == "user" || r == "assistant" {
                        let txt = extract_blocks(msg.get("content"), &["text"]);
                        let ts = obj
                            .get("timestamp")
                            .and_then(Value::as_str)
                            .or_else(|| msg.get("timestamp").and_then(Value::as_str));
                        push_clean(&mut entries, r, &txt, ts);
                    }
                }
            }
            "user" | "assistant" => {
                // Claude Code
                if let Some(msg) = obj.get("message").and_then(Value::as_object) {
                    let txt = extract_blocks(msg.get("content"), &["text"]);
                    push_clean(
                        &mut entries,
                        ty,
                        &txt,
                        obj.get("timestamp").and_then(Value::as_str),
                    );
                }
            }
            "event_msg" => {
                // Codex event_msg
                if let Some(p) = obj.get("payload").and_then(Value::as_object) {
                    let pt = p.get("type").and_then(Value::as_str).unwrap_or("");
                    if pt == "user_message" || pt == "agent_message" {
                        let txt = p.get("message").and_then(Value::as_str).unwrap_or("");
                        push_clean(
                            &mut entries,
                            if pt == "user_message" {
                                "user"
                            } else {
                                "assistant"
                            },
                            txt,
                            obj.get("timestamp").and_then(Value::as_str),
                        );
                    }
                }
            }
            "response_item" => {
                // Codex response_item
                if let Some(p) = obj.get("payload").and_then(Value::as_object) {
                    if p.get("type").and_then(Value::as_str) == Some("message") {
                        let r = p.get("role").and_then(Value::as_str).unwrap_or("");
                        if r == "user" || r == "assistant" {
                            let txt = extract_blocks(
                                p.get("content"),
                                &["input_text", "output_text", "text"],
                            );
                            push_clean(
                                &mut entries,
                                r,
                                &txt,
                                obj.get("timestamp").and_then(Value::as_str),
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if entries.len() > limit {
        entries.split_off(entries.len() - limit)
    } else {
        entries
    }
}

fn find_newest_file_in_dir(dir: &Path, ext: &str) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some(ext) || !p.is_file() {
            continue;
        }
        if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
            if best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// Pi 세션 디렉터리(`~/.pi/agent/sessions/--<cwd>--/`)에서 가장 최근 `.jsonl` 경로를 찾는다.
fn find_latest_pi_session_path(cwd: Option<&str>) -> Option<PathBuf> {
    let cwd = cwd.filter(|s| !s.is_empty())?;
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let real_cwd = resolve_path_best(cwd);
    let slug = format!(
        "-{}--",
        real_cwd.to_string_lossy().replace(['/', '\\'], "-")
    );
    let dir = PathBuf::from(home)
        .join(".pi")
        .join("agent")
        .join("sessions")
        .join(slug);
    find_newest_file_in_dir(&dir, "jsonl")
}

/// Claude Code 세션 디렉터리(`~/.claude/projects/<cwd-slug>/`)에서 가장 최근 `.jsonl` 경로를 찾는다.
fn find_latest_claude_session_path(cwd: Option<&str>) -> Option<PathBuf> {
    let cwd = cwd.filter(|s| !s.is_empty())?;
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let real_cwd = resolve_path_best(cwd);
    let slug = real_cwd.to_string_lossy().replace(['/', '\\', '.'], "-");
    let dir = PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(slug);
    find_newest_file_in_dir(&dir, "jsonl")
}

/// Gemini CLI 세션 디렉터리(`~/.gemini/tmp/<repoTag>/chats/`)에서 가장 최근 `.json`을 찾는다.
fn find_latest_gemini_session_path(cwd: Option<&str>) -> Option<PathBuf> {
    let cwd = cwd.filter(|s| !s.is_empty())?;
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let tag = extract_repo_tag(Some(cwd));
    if tag.is_empty() || tag == "unknown" {
        return None;
    }
    let dir = PathBuf::from(home)
        .join(".gemini")
        .join("tmp")
        .join(tag)
        .join("chats");
    find_newest_file_in_dir(&dir, "json")
}

/// Codex 세션(`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`) 중 첫 줄 session_meta.cwd 일치 최신 파일.
fn find_latest_codex_session_path(cwd: Option<&str>) -> Option<PathBuf> {
    let cwd = cwd.filter(|s| !s.is_empty())?;
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let target = resolve_path_best(cwd);
    let root = PathBuf::from(home).join(".codex").join("sessions");
    let sorted_desc = |dir: &Path| -> Vec<PathBuf> {
        let mut items: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        items.sort();
        items.reverse();
        items
    };
    let mut checked = 0usize;
    for y in sorted_desc(&root) {
        for m in sorted_desc(&y) {
            for d in sorted_desc(&m) {
                for f in sorted_desc(&d) {
                    if f.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    checked += 1;
                    if checked > 15 {
                        return None;
                    }
                    if let Ok(mut file) = std::fs::File::open(&f) {
                        let mut buf = [0u8; 4096];
                        if let Ok(n) = file.read(&mut buf) {
                            let first = String::from_utf8_lossy(&buf[..n]);
                            let line = first.lines().next().unwrap_or("");
                            if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(line) {
                                if let Some(scwd) = obj
                                    .get("payload")
                                    .and_then(|p| p.get("cwd"))
                                    .and_then(Value::as_str)
                                {
                                    if resolve_path_best(scwd) == target {
                                        return Some(f);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

type TranscriptCacheEntry = (String, Option<String>, Vec<TranscriptIoEntry>);

fn read_transcript_file_info(
    cache_key: &str,
    tx_path: &Path,
    cache: &mut HashMap<String, TranscriptCacheEntry>,
) -> (Option<String>, Vec<TranscriptIoEntry>) {
    let Ok(meta) = std::fs::metadata(tx_path) else {
        return (None, Vec::new());
    };
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let sig = format!("{}:{}:{}", tx_path.display(), mtime_ms, meta.len());
    if let Some((cached_sig, cached_warn, cached_io)) = cache.get(cache_key) {
        if cached_sig == &sig {
            return (cached_warn.clone(), cached_io.clone());
        }
    }
    use std::io::Seek;
    let Ok(mut f) = std::fs::File::open(tx_path) else {
        return (None, Vec::new());
    };
    let start = meta.len().saturating_sub(TRANSCRIPT_TAIL_BYTES);
    let _ = f.seek(std::io::SeekFrom::Start(start));
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return (None, Vec::new());
    }
    let raw = String::from_utf8_lossy(&buf);
    let warn = detect_subagent_warning_from_transcript(Some(&raw));
    let recent_io = extract_recent_io_from_transcript(Some(&raw), MAX_RECENT_IO_ENTRIES);
    cache.insert(
        cache_key.to_string(),
        (sig, warn.clone(), recent_io.clone()),
    );
    (warn, recent_io)
}

fn read_conversation_transcript_info(
    conv_id: &str,
    cache: &mut HashMap<String, TranscriptCacheEntry>,
) -> (Option<String>, Vec<TranscriptIoEntry>) {
    let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) else {
        return (None, Vec::new());
    };
    let tx_path = PathBuf::from(home)
        .join(".gemini")
        .join("antigravity-cli")
        .join("brain")
        .join(conv_id)
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl");
    read_transcript_file_info(conv_id, &tx_path, cache)
}

/// 터미널 화면 원문에서 "의미 있는 마지막 1줄"을 뽑는다.
/// 아래에서 위로 훑으며 빈 줄·`[hc] …` 상태줄·박스/구분선·스피너 전용 줄·CLI 하단 상태줄은 건너뛴다.
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
        if re_status_chrome_line().is_match(&line) {
            continue;
        }
        if re_agy_footer().is_match(&line) || re_pi_footer().is_match(&line) {
            continue;
        }
        // 시크릿 리덕션은 절단 이전에(레댁션 계약 — 잘린 경계 무효화 방지)
        let safe = re_whitespace_run()
            .replace_all(&redact_secrets(&line), " ")
            .trim()
            .to_string();
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

/// 파서가 인정하는 워커 CLI 집합 — parent 매칭 7종에 agy를 더한다.
const WORKER_CLI_SET: [&str; 8] = [
    "pi", "devin", "claude", "codex", "gemini", "agy", "opencode", "hermes",
];

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
    if tokens.first().is_some_and(|t| token_base(t) == "env") {
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

/// 셸 `-c`/`-lc` 단일 문자열 안 `dg wrap` 코맨드 세그먼트 — `…; dg wrap …` 형태.
fn re_dg_wrap_in_shell() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?:^|[;&\n][;&\s]*)dg(?:\.ts)?\s+wrap\b").unwrap()
    })
}

/// argv가 `dg wrap` 실행을 호스팅하는지 — 이중 카드 방지 판정.
/// ① dg/dg.ts 토큰 바로 뒤가 'wrap'(env 접두는 먼저 벗긴다)
/// ② `env X=1 bash -lc '…; dg wrap …'`처럼 셸 -c/-lc 단일 문자열 안 코맨드로 감싼 형태.
/// 이런 터미널은 §3.5 ps wrap 탐지가 같은 워커를 정확히 표시하므로 hc 카드를 만들지 않는다.
fn is_dg_wrap_host_argv(tokens: &[String]) -> bool {
    let stripped = strip_env_prefix(tokens);
    // ① 정확한 인접 — dg/dg.ts 바로 뒤 'wrap'
    for i in 0..stripped.len().saturating_sub(1) {
        let base = token_base(&stripped[i]);
        if (base == "dg" || base == "dg.ts") && stripped[i + 1] == "wrap" {
            return true;
        }
    }
    // ② 셸 -c/-lc 단일 문자열 안 코맨드 세그먼트
    for i in 0..stripped.len().saturating_sub(1) {
        let base = token_base(&stripped[i]);
        if (base == "bash" || base == "zsh" || base == "sh")
            && (stripped[i + 1] == "-lc" || stripped[i + 1] == "-c")
        {
            let joined = stripped[i + 2..].join(" ");
            if re_dg_wrap_in_shell().is_match(&joined) {
                return true;
            }
        }
    }
    false
}

/// argv(+agent 폴백)에서 실제 워커 CLI 이름과 `--model` 값을 찾는다.
fn parse_worker_cli_and_model(
    argv: &[String],
    fallback_agent: Option<&str>,
) -> (String, Option<String>) {
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
            .map(|spec| format_role_badge(spec.cli.as_str(), spec.model.as_deref()))
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
/// …/sublead-/sub-) 순으로 판정하고, CLI·모델은 `env`·`dg wrap` 래퍼를 벗겨낸 뒤 실제 CLI와
/// `--model`에서 읽는다(실패 시 `detail.agent` 폴백).
pub fn infer_live_worker_slot(
    detail: &HcTerminalDetail,
    raw_screen: Option<&str>,
) -> Option<LiveWorkerInfo> {
    if detail.address.is_empty() {
        return None;
    }
    let argv: Vec<String> = detail.argv.clone().unwrap_or_default();
    let fg_argv: Vec<String> = detail.foreground_argv.clone().unwrap_or_default();
    // 현재 포그라운드 프로세스 argv가 있으면 초기 셸 런치 argv(스테일 --model 등) 대신 사용한다.
    let cli_argv: &[String] = if !fg_argv.is_empty() {
        &fg_argv
    } else {
        &argv
    };

    // 1) 슬롯 — HERDR_ROLE env가 최우선 (래퍼 argv·포그라운드 argv 모두 검사).
    //    미등값은 TS와 같이 무시하고(폴백 유지) 세션명 추론으로 넘어간다(Node v1 패리티).
    let mut slot: LiveWorkerSlot = LiveWorkerSlot::Coder;
    let mut role_from_argv = false;
    for token in argv.iter().chain(fg_argv.iter()) {
        if let Some(m) = re_herdr_role().captures(token) {
            if let Some(parsed) = m.get(1).and_then(|g| LiveWorkerSlot::parse(g.as_str())) {
                slot = parsed;
                role_from_argv = true;
                break;
            }
        }
    }
    // 세션명 접두 — 공유 접두 판정(`LiveWorkerSlot::infer_from_name`).
    //
    // 단일 리드 불변식(docs/sublead-design.md): parent 없는 세션만 lead다. 강등
    // (lead-* + parent → coder)은 Lead 접두 경로에만 적용하고, sublead는 강등 대상이
    // 아니다 — sublead는 parent를 가진 스코프 리드라서 `sublead-*`/`sub-*` 이름 또는
    // `HERDR_ROLE=sublead`면 parent 유무와 무관하게 sublead 슬롯을 유지한다.
    // 아래 `Some(other)` 분기가 parent를 보지 않으므로 코드 추가 없이 성립한다(회귀 테스트가 고정).
    let name = detail.name.clone().unwrap_or_default();
    if !role_from_argv && !name.is_empty() {
        let has_parent = detail
            .parent
            .as_deref()
            .map(str::trim)
            .is_some_and(|p| !p.is_empty());
        match LiveWorkerSlot::infer_from_name(&name) {
            Some(LiveWorkerSlot::Lead) => {
                slot = if has_parent {
                    LiveWorkerSlot::Coder
                } else {
                    LiveWorkerSlot::Lead
                };
            }
            Some(other) => slot = other,
            None => slot = LiveWorkerSlot::Coder,
        }
    }

    // 2) CLI·모델·effort — 현재 포그라운드 프로세스(cli_argv) + 실시간 터미널 하단 상태줄 우선
    let (cli, argv_model) = parse_worker_cli_and_model(cli_argv, detail.agent.as_deref());
    let (screen_model, screen_effort) = parse_live_screen_telemetry(raw_screen);
    let argv_effort = parse_worker_effort(cli_argv);
    let live_model = screen_model.or(argv_model);
    let live_effort = screen_effort.or(argv_effort);
    let display_cli = if cli == "custom" {
        match detail.agent.as_deref().map(str::to_lowercase).as_deref() {
            Some("antigravity") => "agy".to_string(),
            Some(a) if !a.is_empty() => a.to_string(),
            _ => cli.clone(),
        }
    } else {
        cli.clone()
    };
    let short_model = format_role_badge(&cli, live_model.as_deref());
    let model_name = live_model.clone().unwrap_or_else(|| cli.clone());

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
        owner_address: detail
            .parent
            .clone()
            .filter(|p| !p.trim().is_empty()),
        name: if name.is_empty() {
            detail.address.clone()
        } else {
            name
        },
        cwd: detail.cwd.clone(),
        repo_tag: extract_repo_tag(detail.cwd.as_deref()),
        activity,
        status: detail.status.clone(),
        turn: None,
        cli: display_cli,
        model: live_model,
        effort: live_effort,
        subagent_warning: None,
        recent_io: Vec::new(),
        recent_output: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// dg wrap 프로세스 워커 (hc 터미널이 아닌 자식 프로세스)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// herdr agent list 어댑터 (상주 AI 좌석 관제 — "화면만 쓰기" 지원, fail-open)
// ---------------------------------------------------------------------------

/// `herdr agent list --json` 1항목 요약 — 외부 `herdr` 상주 AI 좌석 어댑터용.
#[derive(Debug, Clone, PartialEq)]
pub struct HerdrAgentSeat {
    pub id: String,
    pub name: String,
    pub role: Option<String>,
    pub cli: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub status: Option<String>,
    pub cwd: Option<String>,
    pub activity: Option<String>,
    pub parent: Option<String>,
    pub turn: Option<u64>,
}

fn json_first_str(obj: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = obj.get(*k).and_then(Value::as_str) {
            let t = s.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// `herdr agent list --json` 응답 파싱 — 배열 `[...]` 또는 `{agents|seats|items|data: [...]}`
/// 양쪽을 모두 수용한다. 파싱 실패 시 빈 Vec 반환(fail-open).
pub fn parse_herdr_agent_list(raw: &str) -> Vec<HerdrAgentSeat> {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    let arr = v
        .as_array()
        .or_else(|| v.get("agents").and_then(Value::as_array))
        .or_else(|| v.get("seats").and_then(Value::as_array))
        .or_else(|| v.get("items").and_then(Value::as_array))
        .or_else(|| v.get("data").and_then(Value::as_array));
    let Some(items) = arr else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let Some(id) = json_first_str(item, &["id", "address", "seat_id", "agent_id", "name"])
        else {
            continue;
        };
        let name = json_first_str(item, &["name", "label", "seat", "title"])
            .unwrap_or_else(|| id.clone());
        out.push(HerdrAgentSeat {
            id,
            name,
            role: json_first_str(item, &["role", "slot"]),
            cli: json_first_str(item, &["cli", "agent", "provider", "tool", "runtime"]),
            model: json_first_str(item, &["model", "model_name"]),
            effort: json_first_str(item, &["effort", "thinking"]),
            status: json_first_str(item, &["status", "state"]),
            cwd: json_first_str(item, &["cwd", "repo", "workdir", "project_path"]),
            activity: json_first_str(item, &["activity", "task", "summary", "current_task"]),
            parent: json_first_str(item, &["parent", "owner", "owner_address", "sublead"]),
            turn: item
                .get("turn")
                .or_else(|| item.get("turns"))
                .and_then(Value::as_u64),
        });
    }
    out
}

fn herdr_seat_is_inactive(status: Option<&str>) -> bool {
    matches!(
        status.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("stopped" | "dead" | "exited" | "terminated" | "offline" | "closed" | "removed")
    )
}

/// `HerdrAgentSeat` → `LiveWorkerInfo` 변환. 종료/오프라인 좌석은 제외한다.
pub fn herdr_seat_to_worker(seat: &HerdrAgentSeat) -> Option<LiveWorkerInfo> {
    if herdr_seat_is_inactive(seat.status.as_deref()) {
        return None;
    }
    let slot = seat
        .role
        .as_deref()
        .and_then(LiveWorkerSlot::parse)
        .or_else(|| LiveWorkerSlot::infer_from_name(&seat.name))
        .unwrap_or(LiveWorkerSlot::Coder);
    let cli = seat.cli.clone().unwrap_or_else(|| "herdr".to_string());
    let short_model = format_role_badge(&cli, seat.model.as_deref());
    let model_name = seat.model.clone().unwrap_or_else(|| cli.clone());
    let status = seat
        .status
        .clone()
        .unwrap_or_else(|| "working".to_string());
    let activity = seat
        .activity
        .clone()
        .unwrap_or_else(|| format!("herdr 좌석 · {status}"));
    Some(LiveWorkerInfo {
        slot,
        badge: format!("{short_model} ({status})"),
        short_model,
        model_name,
        address: format!("herdr:{}", seat.id),
        owner_address: seat.parent.clone(),
        name: seat.name.clone(),
        cwd: seat.cwd.clone(),
        repo_tag: extract_repo_tag(seat.cwd.as_deref()),
        activity,
        status: Some(status),
        turn: seat.turn,
        cli,
        model: seat.model.clone(),
        effort: seat.effort.clone(),
        subagent_warning: None,
        recent_io: Vec::new(),
        recent_output: Vec::new(),
    })
}

/// `ps` 한 줄 — pid·ppid·전체 커맨드.
#[derive(Debug, Clone, PartialEq)]
pub struct PsRow {
    pub pid: u64,
    pub ppid: u64,
    pub command: String,
}

/// `ps -axo pid=,ppid=,command=` 출력 파싱.
pub fn parse_ps_rows(out: &str) -> Vec<PsRow> {
    let mut rows = Vec::new();
    for line in out.lines() {
        let t = line.trim_start();
        let Some((a, rest)) = t.split_once(char::is_whitespace) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some((b, cmd)) = rest.split_once(char::is_whitespace) else {
            continue;
        };
        let cmd = cmd.trim();
        if let (Ok(pid), Ok(ppid)) = (a.parse::<u64>(), b.parse::<u64>()) {
            if !cmd.is_empty() {
                rows.push(PsRow {
                    pid,
                    ppid,
                    command: cmd.to_string(),
                });
            }
        }
    }
    rows
}

/// 라이브 `dg wrap` 실행 1건 — 에이전트(리드)가 hc 터미널 없이 백그라운드로 띄운 워커.
#[derive(Debug, Clone, PartialEq)]
pub struct WrapProc {
    pub wrap_pid: u64,
    /// wrap 프로세스부터 위로 올라가는 조상 pid 체인(자기 자신 제외).
    pub ancestors: Vec<u64>,
    pub cli: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// 표시명 우선순위 1순위 `--label` 원문.
    pub label: Option<String>,
    /// 명시적 역할 `--role <slot>`(최우선) — 미지정·무효값이면 None(라벨 접두로 폴백).
    pub role: Option<LiveWorkerSlot>,
    /// 원본 `--project` 경로 — 고아 wrap 스코프 판정·cwd 폴백용(표시명은 `project`).
    pub project_path: Option<String>,
    pub project: Option<String>,
    pub route: Option<String>,
}

fn flag_value(tokens: &[String], flag: &str) -> Option<String> {
    let i = tokens.iter().position(|t| t == flag)?;
    tokens.get(i + 1).cloned()
}

fn split_tokens(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<String>>()
}

/// ps 목록에서 `node …/dg.ts wrap …` 프로세스와 그 실제 워커 자식(devin/pi/claude/…)을 찾는다.
/// 워커 자식이 아직 없고 `--` 이후 커맨드도 없으면 건너뛴다(허위 카드 금지).
pub fn find_wrap_processes(rows: &[PsRow]) -> Vec<WrapProc> {
    let by_pid: HashMap<u64, &PsRow> = rows.iter().map(|r| (r.pid, r)).collect();
    let mut children: HashMap<u64, Vec<&PsRow>> = HashMap::new();
    for r in rows {
        children.entry(r.ppid).or_default().push(r);
    }
    let launchers = ["node", "bun", "deno", "tsx", "dg", "dg.ts"];
    let mut out = Vec::new();
    for row in rows {
        // `env VAR=… dg wrap …` 런처도 감지 — env 접두를 벗기고 dg/wrap을 본다.
        let tokens: Vec<String> =
            strip_env_prefix(&split_tokens(&row.command)).to_vec();
        if tokens.len() < 3 || !launchers.contains(&token_base(&tokens[0]).as_str()) {
            continue;
        }
        let Some(dg_idx) = tokens
            .iter()
            .position(|t| t == "dg" || token_base(t) == "dg.ts" || token_base(t) == "dg")
        else {
            continue;
        };
        if tokens.get(dg_idx + 1).map(String::as_str) != Some("wrap") {
            continue;
        }

        // 1) `dg wrap ... -- <cmd...>` 뒤 원본 커맨드 (자식이 process.title을 'pi'로 덮어도 --model/--thinking 보존)
        let dash_tokens: Option<Vec<String>> = tokens
            .iter()
            .skip(dg_idx)
            .position(|t| t == "--")
            .and_then(|dash| {
                let start = dg_idx + dash + 1;
                (start < tokens.len()).then(|| tokens[start..].to_vec())
            });

        // 2) BFS — 가장 가까운 워커 CLI 자식
        let mut child_tokens: Option<Vec<String>> = None;
        let mut queue: Vec<u64> = vec![row.pid];
        let mut qi = 0;
        while qi < queue.len() && qi < 64 && child_tokens.is_none() {
            let cur = queue[qi];
            qi += 1;
            if let Some(kids) = children.get(&cur) {
                for child in kids {
                    // `env VAR=… pi …` 자식도 감지 — env 접두를 벗기고 실제 CLI 헤드를 본다.
                    let ct_raw = split_tokens(&child.command);
                    let ct = strip_env_prefix(&ct_raw);
                    let head = ct.first().map(|t| token_base(t)).unwrap_or_default();
                    let second = ct.get(1).map(|t| token_base(t)).unwrap_or_default();
                    if WORKER_CLI_SET.contains(&head.as_str())
                        || (head == "node" && WORKER_CLI_SET.contains(&second.as_str()))
                    {
                        child_tokens = Some(ct.to_vec());
                        break;
                    }
                    queue.push(child.pid);
                }
            }
        }
        if dash_tokens.is_none() && child_tokens.is_none() {
            continue;
        }

        let from_dash = dash_tokens
            .as_deref()
            .map(|t| parse_worker_cli_and_model(t, None));
        let from_child = child_tokens
            .as_deref()
            .map(|t| parse_worker_cli_and_model(t, None));
        let cli = from_dash
            .as_ref()
            .filter(|(c, _)| c != "custom")
            .map(|(c, _)| c.clone())
            .or_else(|| from_child.as_ref().map(|(c, _)| c.clone()))
            .or_else(|| from_dash.as_ref().map(|(c, _)| c.clone()))
            .unwrap_or_else(|| "custom".to_string());
        let model = from_dash
            .and_then(|(_, m)| m)
            .or_else(|| from_child.and_then(|(_, m)| m));
        let effort = dash_tokens
            .as_deref()
            .and_then(parse_worker_effort)
            .or_else(|| child_tokens.as_deref().and_then(parse_worker_effort));
        let mut ancestors: Vec<u64> = Vec::new();
        let mut cur = by_pid.get(&row.ppid).copied();
        let mut hops = 0;
        while let Some(p) = cur {
            if hops >= 40 {
                break;
            }
            ancestors.push(p.pid);
            cur = by_pid.get(&p.ppid).copied();
            hops += 1;
        }
        if row.ppid > 1 && !ancestors.contains(&row.ppid) {
            ancestors.insert(0, row.ppid);
        }
        let label = flag_value(&tokens, "--label");
        let project_path = flag_value(&tokens, "--project");
        let project = label.clone().or_else(|| {
            project_path.as_deref().and_then(|p| {
                let trimmed = p.trim_end_matches(['/', '\\']).to_string();
                let base = token_base(&trimmed);
                if base.is_empty() {
                    None
                } else {
                    Some(base)
                }
            })
        });
        out.push(WrapProc {
            wrap_pid: row.pid,
            ancestors,
            cli,
            model,
            effort,
            label,
            role: flag_value(&tokens, "--role").as_deref().and_then(LiveWorkerSlot::parse),
            project_path,
            project,
            route: flag_value(&tokens, "--route"),
        });
    }
    out
}

/// 독립 실행 AI CLI 5종 — `hc` 터미널·`dg wrap` 트리에 속하지 않는 OS 프로세스만 카드로 만든다.
const STANDALONE_AI_CLIS: [&str; 5] = ["devin", "pi", "claude", "codex", "gemini"];

/// 독립 프로세스 카드로 승격하면 안 되는 도움말·데몬·보조 서브커맨드 호출 판정.
fn is_standalone_helper_invocation(cli: &str, tokens: &[String]) -> bool {
    let first_arg = tokens.get(1).map(String::as_str);
    if matches!(first_arg, Some("--help" | "-h" | "--version" | "-V")) {
        return true;
    }
    cli == "devin" && matches!(first_arg, Some("acp" | "daemon"))
}

/// 대상 pid 자체 또는 조상 체인 중 하나라도 제외 집합에 있으면 true — hc/wrap 소유 프로세스의
/// 이중 카드를 원천 차단한다.
fn ps_row_has_excluded_pid_or_ancestor(
    row: &PsRow,
    by_pid: &HashMap<u64, &PsRow>,
    excluded_pids: &HashSet<u64>,
) -> bool {
    if excluded_pids.contains(&row.pid) {
        return true;
    }
    let mut cur = row.ppid;
    for _ in 0..64 {
        if excluded_pids.contains(&cur) {
            return true;
        }
        if cur <= 1 {
            return false;
        }
        let Some(parent) = by_pid.get(&cur) else {
            return false;
        };
        if parent.ppid == cur {
            return false;
        }
        cur = parent.ppid;
    }
    false
}

/// raw `ps` 행에서 `hc`/`dg wrap`에 속하지 않은 독립 AI CLI 프로세스를 찾는다.
/// raw `ps` 행은 cwd를 싣지 않으므로 repo 스코프 판정은 호출자 책임이다.
pub fn find_standalone_ai_processes(
    rows: &[PsRow],
    excluded_pids: &HashSet<u64>,
) -> Vec<LiveWorkerInfo> {
    let by_pid: HashMap<u64, &PsRow> = rows.iter().map(|r| (r.pid, r)).collect();
    let mut out = Vec::new();
    for row in rows {
        let raw_tokens = split_tokens(&row.command);
        let tokens = strip_env_prefix(&raw_tokens);
        let Some(head) = tokens.first() else {
            continue;
        };
        let cli = token_base(head);
        if !STANDALONE_AI_CLIS.contains(&cli.as_str())
            || is_standalone_helper_invocation(&cli, tokens)
            || ps_row_has_excluded_pid_or_ancestor(row, &by_pid, excluded_pids)
        {
            continue;
        }
        let (_, model) = parse_worker_cli_and_model(tokens, None);
        let effort = parse_worker_effort(tokens);
        let short_model = format_role_badge(&cli, model.as_deref());
        out.push(LiveWorkerInfo {
            slot: LiveWorkerSlot::Coder,
            badge: format!("{} (실행 중)", short_model),
            short_model,
            model_name: model.clone().unwrap_or_else(|| cli.clone()),
            address: format!("os:{}", row.pid),
            owner_address: None,
            name: format!("{}-{}", cli, row.pid),
            cwd: None,
            repo_tag: "unknown".to_string(),
            activity: "OS 프로세스 실행 중 (ps)".to_string(),
            status: Some("working".to_string()),
            turn: None,
            cli,
            model,
            effort,
            subagent_warning: None,
            recent_io: Vec::new(),
            recent_output: Vec::new(),
        });
    }
    out
}

/// `dg wrap` 실시간 출력 로그 디렉터리 (`$XDG_STATE_HOME/dgate/wrap-logs` 또는 `~/.local/state/dgate/wrap-logs`).
pub fn default_wrap_log_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        let trimmed = xdg.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("dgate").join("wrap-logs");
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("dgate")
        .join("wrap-logs")
}

/// raw 터미널/로그 문자열에서 의미 있는 최근 출력 줄(최대 max_lines)을 추출한다.
pub fn extract_recent_output_lines(raw: &str, max_lines: usize) -> Vec<String> {
    let cleaned = strip_ansi(raw).replace('\r', "");
    let mut out: Vec<String> = Vec::new();
    for line in cleaned.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("[hc]") {
            continue;
        }
        if trimmed.chars().all(|c| {
            c.is_whitespace()
                || "─━═│┃┌┐└┘├┤┬┴┼╭╮╰╯·•⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(c)
        }) {
            continue;
        }
        let redacted = redact_secrets(trimmed);
        let clipped: String = redacted.chars().take(140).collect();
        out.push(clipped);
    }
    if out.len() > max_lines {
        out.split_off(out.len() - max_lines)
    } else {
        out
    }
}

/// `dg wrap` 백그라운드 워커의 실시간 출력 꼬리 파일을 읽어 최근 줄 목록을 반환한다.
pub fn read_wrap_output_tail(
    wrap_pid: u64,
    dir_override: Option<&Path>,
    max_lines: usize,
) -> Vec<String> {
    let dir = dir_override
        .map(PathBuf::from)
        .unwrap_or_else(default_wrap_log_dir);
    let path = dir.join(format!("{}.log", wrap_pid));
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    extract_recent_output_lines(&raw, max_lines)
}

/// wrap 프로세스를 상위 hc 세션(parent) 카드 정보로 변환 — slot=coder, 실행 중.
pub fn wrap_proc_to_worker(p: &WrapProc, parent: &HcTerminalSummary) -> LiveWorkerInfo {
    wrap_proc_to_worker_with_log_dir(p, parent, None)
}

/// wrap 워커 슬롯 — ① 명시 `--role`(최우선) ② 라벨/프로젝트명 **접두**
/// (`LiveWorkerSlot::infer_from_name` — TS와 같은 공유 접두 규칙) ③ 라벨 없는 file_select →
/// explore ④ coder. 라벨만으로 리드가 되지는 않는다(wrap 워커는 리드가 아니다) — 명시
/// `--role lead`만 인정. sublead 라벨(`sublead-auth`)은 스코프 리드 신원이므로 인정한다.
fn infer_wrap_slot(p: &WrapProc) -> LiveWorkerSlot {
    if let Some(role) = p.role {
        return role;
    }
    if let Some(by_label) = p.project.as_deref().and_then(LiveWorkerSlot::infer_from_name) {
        if by_label != LiveWorkerSlot::Lead {
            return by_label;
        }
    }
    if p.project.is_none() && p.route.as_deref() == Some("file_select") {
        return LiveWorkerSlot::Explore;
    }
    LiveWorkerSlot::Coder
}

/// 테스트·커스텀 경로용 wrap 프로세스 → LiveWorkerInfo 변환.
pub fn wrap_proc_to_worker_with_log_dir(
    p: &WrapProc,
    parent: &HcTerminalSummary,
    log_dir: Option<&Path>,
) -> LiveWorkerInfo {
    let short_model = format_role_badge(&p.cli, p.model.as_deref());
    let recent_output = read_wrap_output_tail(p.wrap_pid, log_dir, 12);
    let activity = if let Some(last) = recent_output.last() {
        last.chars().take(96).collect()
    } else {
        format!(
            "dg wrap 실행 중{} (상위 {})",
            p.route
                .as_deref()
                .map(|r| format!(" · {}", r))
                .unwrap_or_default(),
            parent.address
        )
    };
    LiveWorkerInfo {
        slot: infer_wrap_slot(p),
        badge: format!("{} (실행 중)", short_model),
        model_name: p.model.clone().unwrap_or_else(|| p.cli.clone()),
        short_model,
        address: format!("pid:{}", p.wrap_pid),
        owner_address: Some(parent.address.clone()),
        name: p
            .project
            .clone()
            .unwrap_or_else(|| format!("wrap-{}", p.wrap_pid)),
        cwd: parent.cwd.clone(),
        repo_tag: extract_repo_tag(parent.cwd.as_deref()),
        activity,
        status: Some("working".to_string()),
        turn: None,
        cli: p.cli.clone(),
        model: p.model.clone(),
        effort: p.effort.clone(),
        subagent_warning: None,
        recent_io: Vec::new(),
        recent_output,
    }
}

/// 소유 hc 세션을 못 찾은 고아 `dg wrap` 워커 변환 — `nohup … &`로 상위 셸이 죽어
/// PPID=1이 된 경우. 관측 공백 방지를 위해 미소속 카드로라도 Workers에 올린다.
pub fn wrap_proc_to_unowned_worker(p: &WrapProc) -> LiveWorkerInfo {
    wrap_proc_to_unowned_worker_with_log_dir(p, None)
}

/// 테스트·커스텀 경로용 고아 wrap 프로세스 → LiveWorkerInfo 변환.
pub fn wrap_proc_to_unowned_worker_with_log_dir(
    p: &WrapProc,
    log_dir: Option<&Path>,
) -> LiveWorkerInfo {
    let short_model = format_role_badge(&p.cli, p.model.as_deref());
    let recent_output = read_wrap_output_tail(p.wrap_pid, log_dir, 12);
    let activity = if let Some(last) = recent_output.last() {
        last.chars().take(96).collect()
    } else {
        format!(
            "dg wrap 실행 중{} · 소유 세션 없음",
            p.route
                .as_deref()
                .map(|r| format!(" · {}", r))
                .unwrap_or_default()
        )
    };
    LiveWorkerInfo {
        slot: infer_wrap_slot(p),
        badge: format!("{} (실행 중)", short_model),
        model_name: p.model.clone().unwrap_or_else(|| p.cli.clone()),
        short_model,
        address: format!("pid:{}", p.wrap_pid),
        owner_address: None,
        name: p
            .project
            .clone()
            .unwrap_or_else(|| format!("wrap-{}", p.wrap_pid)),
        cwd: p.project_path.clone(),
        repo_tag: p
            .project_path
            .as_deref()
            .map(|pp| extract_repo_tag(Some(pp)))
            .unwrap_or_else(|| "미소속".to_string()),
        activity,
        status: Some("working".to_string()),
        turn: None,
        cli: p.cli.clone(),
        model: p.model.clone(),
        effort: p.effort.clone(),
        subagent_warning: None,
        recent_io: Vec::new(),
        recent_output,
    }
}

/// 기본 프로세스 목록 공급자 — Windows·테스트에서는 비활성(None).
fn default_proc_lister() -> Option<String> {
    if cfg!(windows) || cfg!(test) {
        return None;
    }
    let out = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8(out.stdout).ok()
}

/// `hc get --json`에서 워커 소유 판정에 필요한 프로세스 정보 — 터미널 자신과
/// 포그라운드 pid, 그리고 이 터미널이 `hc run --bg -- dg wrap …`처럼 wrap 명령
/// 전용으로 시작됐는지 여부. 초기 argv가 아니라 foreground만 wrap인 터미널은
/// 사용자가 셸/CLI 안에서 동기적으로 wrap을 실행한 것이므로 전용 호스트가 아니다.
#[derive(Debug, Clone, Default, PartialEq)]
struct HcTerminalProcessInfo {
    pids: Vec<u64>,
    dedicated_wrap_host: bool,
}

/// `hc get --json` 출력에서 터미널 프로세스 정보를 모은다.
fn parse_hc_get_process_info(raw: &str) -> HcTerminalProcessInfo {
    let Ok(parsed) = serde_json::from_str::<Value>(raw) else {
        return HcTerminalProcessInfo::default();
    };
    let mut pids = Vec::new();
    if let Some(p) = parsed.get("pid").and_then(Value::as_u64) {
        pids.push(p);
    }
    if let Some(p) = parsed
        .get("foreground")
        .and_then(|f| f.get("pid"))
        .and_then(Value::as_u64)
    {
        pids.push(p);
    }
    let argv: Option<Vec<String>> = parsed.get("argv").and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect::<Vec<String>>()
    });
    let dedicated_wrap_host = argv.as_deref().is_some_and(is_dg_wrap_host_argv);
    HcTerminalProcessInfo {
        pids,
        dedicated_wrap_host,
    }
}

// ---------------------------------------------------------------------------
// 라벨 병합
// ---------------------------------------------------------------------------

/// 최상위 활성 AI 세션을 Lead 카드로 정규화:
/// - 슬롯은 Lead 고정(LEAD 카운터·Lead 카드에 반영).
/// - 표시명은 비어 있지 않은 세션명, 없으면 'lead'(원 주소 노출 방지).
/// - `--model` 미파싱 시 shortModel·modelName을 agent명(예: 'antigravity')으로 폴백.
pub fn normalize_top_level_worker(worker: &mut LiveWorkerInfo, summary: &HcTerminalSummary) {
    worker.slot = LiveWorkerSlot::Lead;
    let name = summary
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
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

/// 라이브 오버라이드 대상 슬롯(lead는 watchedLead 부재 시에만). sublead는 roster.json
/// 역할이 아니라(5종 그대로) Workers 카드로만 표시되는 라이브 슬롯이므로 라벨을 덮지 않는다.
const LIVE_OVERRIDE_SLOTS: [LiveWorkerSlot; 4] = [
    LiveWorkerSlot::Verifier,
    LiveWorkerSlot::Coder,
    LiveWorkerSlot::Explore,
    LiveWorkerSlot::Advisor,
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
        if w.slot == LiveWorkerSlot::Lead {
            if watched_lead.is_none() && lead_worker.is_none() {
                lead_worker = Some(w);
            }
            continue;
        }
        if LIVE_OVERRIDE_SLOTS.contains(&w.slot) && !claimed.contains(&w.slot) {
            claimed.insert(w.slot);
            // sublead는 `as_role()`가 None이지만 LIVE_OVERRIDE_SLOTS에 없으므로 여기 도달하지 않는다.
            if let Some(role) = w.slot.as_role() {
                labels.set_slot(&role, w.short_model.clone());
            }
            if w.slot == LiveWorkerSlot::Verifier {
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

/// `hc get --json` 출력 → 상세(argv·foreground.argv 포함). 파싱 실패·address 누락은 None.
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
    let foreground_obj = obj.get("foreground").and_then(Value::as_object);
    let foreground_argv = foreground_obj
        .and_then(|fg| fg.get("argv"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<String>>()
        });
    let foreground_cwd = foreground_obj
        .and_then(|fg| fg.get("cwd"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut detail = HcTerminalDetail::from_summary(&summary);
    detail.argv = argv;
    detail.foreground_argv = foreground_argv;
    if let Some(ref fg_cwd) = foreground_cwd {
        detail.cwd = Some(fg_cwd.clone());
    }
    detail.foreground_cwd = foreground_cwd;
    Some(detail)
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
    /// `ps -axo pid=,ppid=,command=` 출력 공급자 — `dg wrap` 백그라운드 워커 탐지용(None=비활성).
    pub proc_lister: Arc<dyn Fn() -> Option<String> + Send + Sync>,
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
            proc_lister: Arc::new(default_proc_lister),
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
    proc_lister: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    pid_cache: HashMap<String, HcTerminalProcessInfo>,
    base_labels: Option<RosterDisplayLabels>,
    roster_signature: Option<String>,
    last_workers: Vec<LiveWorkerInfo>,
    detail_cache: HashMap<String, HcTerminalDetail>,
    screen_cache: HashMap<String, String>,
    subagent_cache: HashMap<String, TranscriptCacheEntry>,
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
            proc_lister: opts.proc_lister,
            pid_cache: HashMap::new(),
            base_labels: None,
            roster_signature: None,
            last_workers: Vec::new(),
            detail_cache: HashMap::new(),
            screen_cache: HashMap::new(),
            subagent_cache: HashMap::new(),
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
        let labels = self
            .base_labels
            .clone()
            .unwrap_or_else(builtin_fallback_labels);
        let watched = self.get_watched_lead.as_ref().and_then(|f| f());
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
        let labels = self
            .base_labels
            .clone()
            .unwrap_or_else(builtin_fallback_labels);

        // 1) `hc ls --json` — 실행 오류·JSON 파싱 실패 모두 이전 워커 상태 유지(fail-safe).
        //    (파싱 실패 시 빈 목록으로 방송하면 워커 0건 + 허위 종료 로그가 터진다.)
        let all_terminals: Option<Vec<HcTerminalSummary>> = self
            .runner
            .exec(&self.hc_bin, &["ls", "--json"], HC_LS_TIMEOUT_MS)
            .ok()
            .and_then(|raw| parse_hc_ls_json(&raw));
        let repo_scope_str = self.repo_filter_dir.as_deref().and_then(|p| p.to_str());
        let summaries: Option<Vec<HcTerminalSummary>> = all_terminals.as_ref().map(|terminals| {
            terminals
                .iter()
                .filter(|t| {
                    if !is_observable_terminal(t) {
                        return false;
                    }
                    // 셸 초기 cwd가 달라도 활성 에이전트(`status.is_some()`)는 `hc get`의
                    // `foreground.cwd`로 재판정하기 위해 후보에 남겨둔다.
                    matches_repo_scope(t.cwd.as_deref(), repo_scope_str) || t.status.is_some()
                })
                .cloned()
                .collect()
        });

        // 2) 상세 캐시 갱신 — 떠난 주소는 즉시 증발
        if let Some(summaries) = summaries {
            let active: HashSet<&str> = summaries.iter().map(|t| t.address.as_str()).collect();
            self.detail_cache
                .retain(|addr, _| active.contains(addr.as_str()));
            self.screen_cache
                .retain(|addr, _| active.contains(addr.as_str()));

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
                detail.cwd = detail
                    .foreground_cwd
                    .clone()
                    .or_else(|| summary.cwd.clone())
                    .or(detail.cwd);
                self.detail_cache
                    .insert(summary.address.clone(), detail.clone());

                // `foreground.cwd` 반영 후 최종 레포 스코프 판정 — 다른 레포의 활성 에이전트는 제외.
                if !matches_repo_scope(detail.cwd.as_deref(), repo_scope_str) {
                    continue;
                }

                // 이중 카드 방지 — 터미널(포그라운드·런치 argv)이 `dg wrap`을 호스팅하면
                // §3.5 ps wrap 탐지가 같은 워커를 정확히 표시하므로 hc 카드는 만들지
                // 않는다. wrap이 아닌 일반 bash/기타 실행은 유지.
                if detail
                    .foreground_argv
                    .as_deref()
                    .is_some_and(is_dg_wrap_host_argv)
                    || detail.argv.as_deref().is_some_and(is_dg_wrap_host_argv)
                {
                    continue;
                }

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
                let raw_screen =
                    screen.or_else(|| self.screen_cache.get(&summary.address).cloned());
                if let Some(mut worker) = infer_live_worker_slot(&detail, raw_screen.as_deref()) {
                    // 턴 수는 `hc ls` 요약의 실측값을 그대로 얹는다(detail 캐시에는 없음).
                    worker.turn = summary.turn;
                    // 최상위 활성 AI 세션(워커 규약 밖 리드 등) — Lead 슬롯·표시명·
                    // shortModel 폴백을 정규화한다. 워커 규약 터미널은 그대로 둔다.
                    // sublead(sublead-/sub- 이름)는 워커 접두라 이 분기에 들어오지 않아
                    // sublead 슬롯이 보존된다(최상위 리드 후보 아님).
                    if is_active_top_level_session(summary) && !is_worker_terminal(summary) {
                        normalize_top_level_worker(&mut worker, summary);
                    }
                    if worker.slot == LiveWorkerSlot::Lead {
                        let conv_id = detail
                            .foreground_argv
                            .as_deref()
                            .and_then(extract_conversation_id_from_argv)
                            .or_else(|| {
                                detail
                                    .argv
                                    .as_deref()
                                    .and_then(extract_conversation_id_from_argv)
                            });
                        if let Some(cid) = conv_id {
                            let (warn, io) = read_conversation_transcript_info(
                                &cid,
                                &mut self.subagent_cache,
                            );
                            worker.subagent_warning = warn;
                            worker.recent_io = io;
                        } else {
                            let tx_path = match worker.cli.as_str() {
                                "pi" => find_latest_pi_session_path(worker.cwd.as_deref()),
                                "claude" => find_latest_claude_session_path(worker.cwd.as_deref()),
                                "codex" => find_latest_codex_session_path(worker.cwd.as_deref()),
                                "gemini" => find_latest_gemini_session_path(worker.cwd.as_deref()),
                                _ => None,
                            };
                            if let Some(p) = tx_path {
                                let key = format!("{}:{}", worker.cli, worker.address);
                                let (_, io) = read_transcript_file_info(
                                    &key,
                                    &p,
                                    &mut self.subagent_cache,
                                );
                                worker.recent_io = io;
                            }
                        }
                        // Claude Code 리드 세션에서 Explore.md 4줄 오버라이드가 없으면 Opus grep 낭비 경고
                        if worker.cli == "claude" && worker.subagent_warning.is_none() {
                            let home = std::env::var("HOME")
                                .or_else(|_| std::env::var("USERPROFILE"))
                                .unwrap_or_default();
                            let global_explore = PathBuf::from(&home)
                                .join(".claude")
                                .join("agents")
                                .join("Explore.md");
                            let repo_explore = worker.cwd.as_deref().map(|c| {
                                PathBuf::from(c)
                                    .join(".claude")
                                    .join("agents")
                                    .join("Explore.md")
                            });
                            let has_override = global_explore.is_file()
                                || repo_explore.as_ref().is_some_and(|p| p.is_file());
                            if !has_override {
                                worker.subagent_warning = Some(
                                    "Explore.md 없음 — grep이 Opus로 실행됨 (dgate setup으로 4줄 haiku 강등)"
                                        .to_string(),
                                );
                            }
                        }
                    }
                    if worker.slot != LiveWorkerSlot::Lead {
                        if let Some(raw) = raw_screen.as_deref() {
                            worker.recent_output = extract_recent_output_lines(raw, 12);
                        }
                    }
                    workers.push(worker);
                }
            }
            // 3.5) `dg wrap`으로 에이전트가 백그라운드로 띄운 워커 — hc 터미널이 아니라 ps
            //      프로세스 트리에서만 보인다. 조상 pid가 hc 세션(pid/foreground.pid)과 맞으면
            //      그 세션 소속으로 Workers에 올린다.
            if let Some(ps_out) = (self.proc_lister)() {
                let rows = parse_ps_rows(&ps_out);
                let procs = find_wrap_processes(&rows);
                let terminals: &[HcTerminalSummary] = all_terminals.as_deref().unwrap_or(&[]);
                self.pid_cache
                    .retain(|addr, _| terminals.iter().any(|t| &t.address == addr));
                let mut fetched = 0usize;
                let mut pid_owner: HashMap<u64, (&HcTerminalSummary, bool)> = HashMap::new();
                // parent 있는 서브-리드/워커 터미널을 먼저 캐싱해 다수의 유휴 셸 탭(20+)이
                // 있더라도 첫 틱부터 wrap 소유자 매핑이 누락되지 않게 한다.
                let mut live_terminals: Vec<&HcTerminalSummary> = terminals
                    .iter()
                    .filter(|t| t.state.as_deref() == Some("live"))
                    .collect();
                live_terminals.sort_by_key(|t| t.parent.is_none());
                for t in live_terminals {
                    if !self.pid_cache.contains_key(&t.address) && fetched < 32 {
                        fetched += 1;
                        if let Ok(raw) = self.runner.exec(
                            &self.hc_bin,
                            &["get", t.address.as_str(), "--json"],
                            HC_LS_TIMEOUT_MS,
                        ) {
                            self.pid_cache.insert(
                                t.address.clone(),
                                parse_hc_get_process_info(&raw),
                            );
                        }
                    }
                    if let Some(info) = self.pid_cache.get(&t.address) {
                        for pid in &info.pids {
                            pid_owner.insert(*pid, (t, info.dedicated_wrap_host));
                        }
                    }
                }
                let repo_scope = self.repo_filter_dir.as_deref().and_then(|p| p.to_str());
                for p in &procs {
                    // `hc run --bg -- dg wrap …`는 중간 셸 없이 `dg` 자체가 터미널 루트 pid
                    // (`t.pid == p.wrap_pid`)로 exec되므로 wrap_pid 자체를 먼저 조회하고,
                    // 없으면 조상 pid 체인을 탐색한다.
                    let owner = pid_owner
                        .get(&p.wrap_pid)
                        .copied()
                        .or_else(|| p.ancestors.iter().find_map(|a| pid_owner.get(a).copied()));
                    let Some((owner, owner_dedicated_wrap_host)) = owner else {
                        // 고아 wrap(nohup & 등으로 PPID=1)도 미소속 카드로 표시 —
                        // --project 있을 때만 레포 스코프 필터를 적용한다.
                        let scope_ok = p
                            .project_path
                            .as_deref()
                            .map(|pp| matches_repo_scope(Some(pp), repo_scope))
                            .unwrap_or(true);
                        if !scope_ok {
                            continue;
                        }
                        workers.push(wrap_proc_to_unowned_worker(p));
                        continue;
                    };
                    if !matches_repo_scope(owner.cwd.as_deref(), repo_scope) {
                        continue;
                    }
                    let mut worker = wrap_proc_to_worker(p, owner);
                    // `hc run --bg -- dg wrap` 전용 터미널은 카드가 억제되는
                    // 실행 껍데기다. 비-sublead 자식 워커는 이 껍데기 주소가 아니라
                    // hc parent(보통 해당 껍데기를 만든 sublead 터미널)에 귀속시켜
                    // sublead 클러스터가 자식 수를 실제로 보이게 한다.
                    // 반대로 일반 셸/CLI 안에서 동기 실행한 wrap은 기존처럼 실행
                    // 터미널 자신을 소유자로 유지한다.
                    if owner_dedicated_wrap_host && worker.slot != LiveWorkerSlot::Sublead {
                        if let Some(parent_addr) = owner
                            .parent
                            .as_deref()
                            .filter(|addr| !addr.trim().is_empty())
                        {
                            worker.owner_address = Some(parent_addr.to_string());
                        }
                    }
                    workers.push(worker);
                }
                // raw ps 행은 cwd를 싣지 않으므로 repo 스코프가 있을 때는 독립 OS 프로세스를
                // 합치지 않는다(다른 레포 프로세스 누출 방지). 전역 관측(None)에서만 허용한다.
                if self.repo_filter_dir.is_none() {
                    let mut excluded_pids: HashSet<u64> = pid_owner.keys().copied().collect();
                    excluded_pids.extend(procs.iter().map(|p| p.wrap_pid));
                    workers.extend(find_standalone_ai_processes(&rows, &excluded_pids));
                }
            }
            // 3.6) `herdr agent list --json` 어댑터 — 상주 AI 좌석 병합(fail-open: 미설치·오류 시 무시).
            let herdr_bin = std::env::var("HERDR_BIN")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "herdr".to_string());
            if let Ok(raw_seats) =
                self.runner
                    .exec(&herdr_bin, &["agent", "list", "--json"], HC_LS_TIMEOUT_MS)
            {
                let repo_scope = self.repo_filter_dir.as_deref().and_then(|p| p.to_str());
                let mut seen: HashSet<String> =
                    workers.iter().map(|w| w.address.clone()).collect();
                for seat in parse_herdr_agent_list(&raw_seats) {
                    if !matches_repo_scope(seat.cwd.as_deref(), repo_scope) {
                        continue;
                    }
                    if let Some(worker) = herdr_seat_to_worker(&seat) {
                        if seen.insert(worker.address.clone()) {
                            workers.push(worker);
                        }
                    }
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
            turn: None,
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
        assert!(!is_worker_terminal(&summary(
            "dg-monitor",
            "pi",
            "live",
            None
        )));
        assert!(!is_worker_terminal(&summary(
            "dg-monitor-1",
            "pi",
            "live",
            None
        )));
        assert!(!is_worker_terminal(&summary("mon-x", "pi", "live", None)));
        for shell in [
            "zsh",
            "bash",
            "fish",
            "sh",
            "ksh",
            "dash",
            "pwsh",
            "powershell.exe",
        ] {
            assert!(
                !is_worker_terminal(&summary("w-1", shell, "live", Some("lead"))),
                "{}",
                shell
            );
        }
        assert!(!is_worker_terminal(&summary(
            "w-1",
            "antigravity",
            "live",
            None
        )));
        // 대소문자 혼합 에이전트도 소문자 정규화로 제외된다
        assert!(!is_worker_terminal(&summary(
            "w-1",
            "ZSH",
            "live",
            Some("p")
        )));
    }

    #[test]
    fn worker_terminal_matches_name_prefix_or_parent_cli() {
        for name in [
            "w-1",
            "exp-2",
            "explore-3",
            "coder1",
            "coder-x",
            "verify-1",
            "verifier-2",
            "lead-1",
            "advisor-9",
            "carbon-1",
        ] {
            assert!(
                is_worker_terminal(&summary(name, "pi", "live", None)),
                "{}",
                name
            );
        }
        // parent + 워커 CLI 조합
        for agent in ["pi", "devin", "claude", "codex", "gemini"] {
            assert!(is_worker_terminal(&summary(
                "some-session",
                agent,
                "live",
                Some("lead-addr")
            )));
        }
        // parent가 있어도 워커 CLI가 아니면 제외
        assert!(!is_worker_terminal(&summary(
            "some-session",
            "python",
            "live",
            Some("x")
        )));
        // 이름 접두 없이 parent 없음 → 제외
        assert!(!is_worker_terminal(&summary(
            "random-name",
            "pi",
            "live",
            None
        )));
        // agy는 parent 매칭 대상 아님(이름 접두로만 통과)
        assert!(!is_worker_terminal(&summary(
            "random-name",
            "agy",
            "live",
            Some("x")
        )));
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
            turn: None,
        }
    }

    #[test]
    fn top_level_active_sessions_are_observable_as_lead() {
        // 긍정 — 최상위 working/idle/done AI 에이전트는 관측된다(워커 규약 밖 포함)
        for agent in [
            "antigravity",
            "pi",
            "claude",
            "codex",
            "devin",
            "gemini",
            "agy",
        ] {
            for status in [Some("working"), Some("idle"), Some("done")] {
                let t = top_summary(
                    &format!("paperwork-{}", agent),
                    agent,
                    status,
                );
                assert!(is_active_top_level_session(&t), "{}", agent);
                assert!(is_observable_terminal(&t), "{}", agent);
            }
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
        // status 엄격 — None 불가(idle/done은 관측 허용)
        assert!(!is_active_top_level_session(&top_summary(
            "s",
            "antigravity",
            None
        )));
        assert!(!is_active_top_level_session(&top_summary("s", "pi", None)));
        // state 불일치
        let mut dead = top_summary("s", "pi", Some("working"));
        dead.state = Some("done".into());
        assert!(!is_active_top_level_session(&dead));
        // parent 존재 — 최상위 아님
        let mut child = top_summary("s", "pi", Some("working"));
        child.parent = Some("lead-addr".into());
        assert!(!is_active_top_level_session(&child));
        // 셸·런타임·모니터·GUI 제외
        for agent in [
            "zsh",
            "bash",
            "sh",
            "node",
            "python3",
            "powershell.exe",
            "dgate-gui",
        ] {
            assert!(
                !is_active_top_level_session(&top_summary("s", agent, Some("working"))),
                "{}",
                agent
            );
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
        let mut w = worker(LiveWorkerSlot::Coder, "custom", None);
        w.name = "t155".to_string();
        normalize_top_level_worker(&mut w, &sum);
        assert_eq!(w.slot, LiveWorkerSlot::Lead);
        assert_eq!(w.name, "paperwork-operations");
        assert_eq!(w.short_model, "antigravity");
        assert_eq!(w.badge, "antigravity (실행 중)");
        assert_eq!(w.model_name, "antigravity");
        // 이름 없으면 'lead'로 표시
        let mut sum2 = top_summary("", "pi", Some("working"));
        sum2.name = None;
        let mut w2 = worker(LiveWorkerSlot::Coder, "pi", None);
        w2.name = "t155".to_string();
        normalize_top_level_worker(&mut w2, &sum2);
        assert_eq!(w2.slot, LiveWorkerSlot::Lead);
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
        assert_eq!(
            extract_repo_tag(Some("/Users/x/proj/.herdr/worktrees/exp-1")),
            "proj"
        );
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
        assert_eq!(
            extract_repo_tag(Some(tmp.to_str().unwrap())),
            tmp.file_name().unwrap().to_string_lossy()
        );
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
        assert!(!matches_repo_scope(
            None,
            Some("/Users/kwak/Projects/Gemini_trick")
        ));
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
        assert!(matches_repo_scope(
            Some(wt.to_str().unwrap()),
            Some(&repo_s)
        ));
        assert!(!matches_repo_scope(
            Some(outside.to_str().unwrap()),
            Some(&repo_s)
        ));
        // 스코프가 워커 cwd의 상위여도 거짓 — 접두사 방향만 허용
        assert!(!matches_repo_scope(
            Some(&repo_s),
            Some(wt.to_str().unwrap())
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn repo_scope_root_scope_handles_separator_cleanly() {
        // 루트 '/' 스코프는 '//'-접두 사고 없이 전부 매칭
        assert!(matches_repo_scope(Some("/Users/anywhere"), Some("/")));
        assert!(matches_repo_scope(Some("/"), Some("/")));
        // 스코프가 미존재 경로여도 resolve 폴백으로 접두사 판정
        assert!(matches_repo_scope(
            Some("/definitely/missing/child"),
            Some("/definitely/missing")
        ));
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
        assert_eq!(
            extract_last_activity_line(Some(screen), 68).as_deref(),
            Some("last actual line")
        );
    }

    #[test]
    fn activity_skips_hc_footer_box_and_spinner_lines() {
        let screen = "real work\n[hc] idle\n─────────────\n╔════════╗\n⠋\n";
        assert_eq!(
            extract_last_activity_line(Some(screen), 68).as_deref(),
            Some("real work")
        );
        // 박스형 상태줄 '│ [hc] idle │' — 가장자리 제거 후 스킵
        let boxed = "keep me\n│ [hc] idle │";
        assert_eq!(
            extract_last_activity_line(Some(boxed), 68).as_deref(),
            Some("keep me")
        );
        // 상태줄만 있으면 None
        assert_eq!(extract_last_activity_line(Some("[hc] running"), 68), None);
        assert_eq!(extract_last_activity_line(Some("╚══════╝"), 68), None);
        assert_eq!(extract_last_activity_line(Some("⠙"), 68), None);
    }

    #[test]
    fn activity_box_edges_stripped_from_content_lines() {
        let screen = "│ cat src/main.rs │";
        assert_eq!(
            extract_last_activity_line(Some(screen), 68).as_deref(),
            Some("cat src/main.rs")
        );
        let dbl = "║ double border ║";
        assert_eq!(
            extract_last_activity_line(Some(dbl), 68).as_deref(),
            Some("double border")
        );
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
        assert_eq!(
            extract_last_activity_line(Some(&exact), 68).as_deref(),
            Some(exact.as_str())
        );
    }

    #[test]
    fn activity_collapses_whitespace_and_handles_crlf() {
        let screen = "a\t\tb   c\r\n";
        assert_eq!(
            extract_last_activity_line(Some(screen), 68).as_deref(),
            Some("a b c")
        );
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

    fn detail(
        addr: &str,
        name: &str,
        agent: &str,
        argv: &[&str],
        cwd: Option<&str>,
    ) -> HcTerminalDetail {
        HcTerminalDetail {
            address: addr.to_string(),
            agent: Some(agent.to_string()),
            name: Some(name.to_string()),
            parent: None,
            state: Some("live".to_string()),
            status: Some("working".to_string()),
            cwd: cwd.map(str::to_string),
            argv: Some(argv.iter().map(|s| s.to_string()).collect()),
            foreground_argv: None,
            foreground_cwd: None,
        }
    }

    #[test]
    fn wrap_process_worker_attaches_to_parent_session() {
        let ps = "  500     1 /bin/zsh -l\n\
  600   500 agy --dangerously-skip-permissions --conversation=abc\n\
  700   600 node /x/src/decision/dg.ts wrap --route file_select --tool-class file_edit --fork fk_1 --project /w/.worktrees/pi-cli-manager --stall-ms 1 --require-stdout\n\
  701   700 /home/u/.local/bin/devin -p --permission-mode dangerous -- 작업 내용 공백 포함\n\
  702   701 /home/u/.local/bin/devin acp --process-model=in-process\n\
  800   500 node /x/other.js\n";
        let procs = find_wrap_processes(&parse_ps_rows(ps));
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].cli, "devin");
        assert_eq!(procs[0].project.as_deref(), Some("pi-cli-manager"));
        assert_eq!(procs[0].route.as_deref(), Some("file_select"));
        assert!(procs[0].ancestors.contains(&600));

        let parent = HcTerminalSummary {
            address: "t155".into(),
            agent: Some("antigravity".into()),
            cwd: Some("/tmp/paperwork-operations".into()),
            state: Some("live".into()),
            ..Default::default()
        };
        let w = wrap_proc_to_worker(&procs[0], &parent);
        assert_eq!(w.slot, LiveWorkerSlot::Coder);
        assert_eq!(w.address, "pid:700");
        assert_eq!(w.name, "pi-cli-manager");
        assert_eq!(w.repo_tag, "paperwork-operations");
        assert_eq!(w.cli, "devin");
        assert_eq!(w.status.as_deref(), Some("working"));

        // 자식 프로세스가 bare 'pi'여도 dg wrap -- 뒤 --model/--thinking 및 --label을 보존한다
        let ps2 = "57156 77038 /x/dg wrap --route delegate_swarm --label engine-rust-phase1-explore -- pi -p --model command-code/deepseek-deepseek-v4.1-flash --thinking medium -- Analyze\n57182 57156 pi\n";
        let p2 = find_wrap_processes(&parse_ps_rows(ps2));
        assert_eq!(p2.len(), 1);
        assert_eq!(p2[0].cli, "pi");
        assert_eq!(
            p2[0].model.as_deref(),
            Some("command-code/deepseek-deepseek-v4.1-flash")
        );
        assert_eq!(p2[0].effort.as_deref(), Some("medium"));
        assert_eq!(p2[0].project.as_deref(), Some("engine-rust-phase1-explore"));
    }

    #[test]
    fn standalone_ai_processes_detect_direct_cli_with_model_and_effort() {
        let ps = "  800     1 devin --model swe-2-1-high --effort high -p implement scanner\n\
  801     1 claude --model sonnet\n\
  802     1 env PI_API=1 pi --model glm-5.3 --thinking medium -p inspect\n";
        let rows = parse_ps_rows(ps);
        let workers = find_standalone_ai_processes(&rows, &HashSet::new());
        assert_eq!(workers.len(), 3);

        let devin = workers.iter().find(|w| w.address == "os:800").unwrap();
        assert_eq!(devin.slot, LiveWorkerSlot::Coder);
        assert_eq!(devin.name, "devin-800");
        assert_eq!(devin.cli, "devin");
        assert_eq!(devin.model.as_deref(), Some("swe-2-1-high"));
        assert_eq!(devin.effort.as_deref(), Some("high"));
        assert_eq!(devin.status.as_deref(), Some("working"));
        assert_eq!(devin.activity, "OS 프로세스 실행 중 (ps)");
        assert_eq!(devin.owner_address, None);

        let claude = workers.iter().find(|w| w.address == "os:801").unwrap();
        assert_eq!(claude.cli, "claude");
        assert_eq!(claude.model.as_deref(), Some("sonnet"));

        let pi = workers.iter().find(|w| w.address == "os:802").unwrap();
        assert_eq!(pi.cli, "pi");
        assert_eq!(pi.model.as_deref(), Some("glm-5.3"));
        assert_eq!(pi.effort.as_deref(), Some("medium"));
    }

    #[test]
    fn standalone_ai_processes_ignore_helper_daemon_and_meta_invocations() {
        let ps = "  810     1 devin acp --process-model=in-process\n\
  811     1 devin daemon\n\
  812     1 pi --help\n\
  813     1 claude --version\n";
        let rows = parse_ps_rows(ps);
        assert!(find_standalone_ai_processes(&rows, &HashSet::new()).is_empty());
    }

    #[test]
    fn standalone_ai_processes_skip_excluded_pid_and_ancestors() {
        let ps = "  900     1 /bin/zsh -l\n\
  901   900 devin --model swe-2-1-high -p owned by hc\n\
  902   901 claude --model sonnet\n\
  910     1 dg wrap --label owned-wrap -- pi --model glm-5.3\n\
  911   910 pi --model glm-5.3\n\
  920     1 codex --model gpt-5\n";
        let rows = parse_ps_rows(ps);
        let excluded = HashSet::from([900_u64, 910_u64]);
        let workers = find_standalone_ai_processes(&rows, &excluded);
        let addresses: Vec<&str> = workers.iter().map(|w| w.address.as_str()).collect();
        assert_eq!(addresses, vec!["os:920"]);

        let direct_excluded = HashSet::from([901_u64]);
        let workers = find_standalone_ai_processes(&rows, &direct_excluded);
        assert!(!workers.iter().any(|w| w.address == "os:901"));
        assert!(!workers.iter().any(|w| w.address == "os:902"));
    }

    #[test]
    fn orphan_wrap_process_shows_as_unowned_worker() {
        // nohup dg wrap … & → 상위 셸 종료로 PPID=1(launchd) — 조상 체인이 끊겨 소유 세션 불명
        let ps = "  500     1 /bin/zsh -l\n\
  67954     1 /x/target/release/dg wrap --route delegate_swarm --tool-class coder --label card-escalate-gate --stall-ms 900000 -- devin -p 작업\n\
68001 67954 devin --respect-workspace-trust false -p 작업\n";
        let procs = find_wrap_processes(&parse_ps_rows(ps));
        assert_eq!(procs.len(), 1);
        assert!(
            !procs[0].ancestors.contains(&500),
            "고아 wrap은 세션 후보(500)와 연결 없음"
        );
        assert_eq!(procs[0].label.as_deref(), Some("card-escalate-gate"));
        assert_eq!(procs[0].project_path, None);
        let w = wrap_proc_to_unowned_worker(&procs[0]);
        assert_eq!(w.address, "pid:67954");
        assert_eq!(w.name, "card-escalate-gate");
        assert_eq!(w.repo_tag, "미소속");
        assert_eq!(w.cwd, None);
        assert_eq!(w.cli, "devin");
        assert_eq!(w.status.as_deref(), Some("working"));
        assert!(w.activity.contains("소유 세션 없음"));

        // --project가 있으면 cwd·repoTag를 그 경로에서 채운다
        let ps2 = "70000 1 dg wrap --project /w/.worktrees/rust-s9-port -- devin -p x\n70001 70000 devin -p x\n";
        let p2 = find_wrap_processes(&parse_ps_rows(ps2));
        assert_eq!(p2.len(), 1);
        assert_eq!(
            p2[0].project_path.as_deref(),
            Some("/w/.worktrees/rust-s9-port")
        );
        let w2 = wrap_proc_to_unowned_worker(&p2[0]);
        assert_eq!(w2.cwd.as_deref(), Some("/w/.worktrees/rust-s9-port"));
        assert_ne!(w2.repo_tag, "미소속");
    }

    #[test]
    fn subagent_warning_detected_when_running_and_cleared_when_finished() {
        let running = concat!(
            r#"{"step_index":10,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"invoke_subagent"}]}"#,
            "\n",
            r#"{"step_index":11,"type":"GENERIC","content":"Created the following subagent:\n{\n  \"conversationId\": \"657931dc-d5b0-4118-8d76-dc2089606369\"\n}"}"#
        );
        assert_eq!(
            detect_subagent_warning_from_transcript(Some(running)).as_deref(),
            Some("⚠ 내장 서브에이전트 실행 중 (1개 · 관측 불가)")
        );

        let finished = format!(
            "{}\n{}",
            running,
            r#"{"step_index":15,"type":"SYSTEM_MESSAGE","content":"[Message] sender=657931dc-d5b0-4118-8d76-dc2089606369 content=done"}"#
        );
        assert_eq!(
            detect_subagent_warning_from_transcript(Some(&finished)),
            None
        );

        let io_tail = concat!(
            r#"{"type":"USER_INPUT","created_at":"2026-11-20T09:00:00Z","content":"<USER_REQUEST>상태 확인해줘</USER_REQUEST>"}"#,
            "\n",
            r#"{"type":"PLANNER_RESPONSE","created_at":"2026-11-20T09:00:05Z","content":"정상 동작 확인했습니다."}"#
        );
        let io = extract_recent_io_from_transcript(Some(io_tail), 10);
        assert_eq!(io.len(), 2);
        assert_eq!(io[0].role, "user");
        assert_eq!(io[0].text, "상태 확인해줘");
        assert_eq!(io[1].role, "assistant");
        assert_eq!(io[1].text, "정상 동작 확인했습니다.");
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
            ("lead", LiveWorkerSlot::Lead),
            ("explore", LiveWorkerSlot::Explore),
            ("verifier", LiveWorkerSlot::Verifier),
            ("advisor", LiveWorkerSlot::Advisor),
            ("coder", LiveWorkerSlot::Coder),
            ("carbon", LiveWorkerSlot::Advisor), // carbon→advisor 매핑
            ("LEAD", LiveWorkerSlot::Lead),      // 대소문자 무시
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
            ("verify-1", LiveWorkerSlot::Verifier),
            ("w-verifier", LiveWorkerSlot::Verifier),
            ("exp-2", LiveWorkerSlot::Explore),
            ("explore-x", LiveWorkerSlot::Explore),
            ("advisor-1", LiveWorkerSlot::Advisor),
            ("carbon-9", LiveWorkerSlot::Advisor),
            ("lead-1", LiveWorkerSlot::Lead),
            ("w-plain", LiveWorkerSlot::Coder),
            ("coderr", LiveWorkerSlot::Coder), // 'coder' 접두가 아니라 contains 아님 — name은 규약 접두
            ("w-live-lead-rust", LiveWorkerSlot::Coder),
            ("w-live-lead-node", LiveWorkerSlot::Coder),
            ("w-fix-advisor-width", LiveWorkerSlot::Coder),
            ("w-fix-verify-panel", LiveWorkerSlot::Coder),
            ("w-fix-explore-layout", LiveWorkerSlot::Coder),
        ] {
            let d = detail("a", name, "pi", &["pi"], None);
            let w = infer_live_worker_slot(&d, None).unwrap();
            assert_eq!(w.slot, expect, "name={}", name);
        }
        // parent가 있는 하위 워커는 `lead-*` 이름이어도 lead가 아닌 coder로 분류된다.
        let mut child_lead = detail("a", "lead-sub", "pi", &["pi"], None);
        child_lead.parent = Some("t165".to_string());
        assert_eq!(
            infer_live_worker_slot(&child_lead, None).unwrap().slot,
            LiveWorkerSlot::Coder
        );
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
        let d3 = detail(
            "c",
            "w-3",
            "pi",
            &["/usr/local/bin/codex", "--model", "gpt-5"],
            None,
        );
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

        let d2 = detail(
            "b",
            "w-2",
            "agy",
            &["agy", "--model", "gemini-4-neon-high"],
            None,
        );
        let w2 = infer_live_worker_slot(&d2, None).unwrap();
        assert_eq!(w2.model_name, "gemini-4-neon-high");
        assert_eq!(w2.short_model, "agy:gemini-4-neon-high");

        // foreground.argv 우선 + 터미널 하단 상태줄에서 실측 model·effort 추출
        let mut d3 = detail(
            "t165",
            "",
            "antigravity",
            &[
                "/bin/zsh",
                "-lic",
                "\"$@\"; exec \"$0\" -li",
                "/bin/zsh",
                "agy",
                "--model",
                "gemini-4-neon-high",
            ],
            None,
        );
        d3.foreground_argv = Some(vec![
            "agy".into(),
            "--dangerously-skip-permissions".into(),
            "--conversation=abc".into(),
        ]);
        let w3 = infer_live_worker_slot(
            &d3,
            Some("● Edited 2 files\nesc to cancel     auto · Gemini 4 Neon · medium\n"),
        )
        .unwrap();
        assert_eq!(w3.cli, "agy");
        assert_eq!(w3.model.as_deref(), Some("Gemini 4 Neon"));
        assert_eq!(w3.effort.as_deref(), Some("medium"));
        assert_eq!(w3.telemetry_line(), "agy · Gemini 4 Neon · medium");
        assert_eq!(w3.activity, "● Edited 2 files");

        let mut d4 = detail("t1r", "", "pi", &["/bin/zsh", "-l"], None);
        d4.foreground_argv = Some(vec!["pi".into(), "".into()]);
        let w4 = infer_live_worker_slot(
            &d4,
            Some("작업 완료\n↑675k ↓68k (opencodex) glm-zcode/glm-5.3 • max\nctx 15%\n"),
        )
        .unwrap();
        assert_eq!(w4.cli, "pi");
        assert_eq!(w4.model.as_deref(), Some("glm-zcode/glm-5.3"));
        assert_eq!(w4.effort.as_deref(), Some("max"));
        assert_eq!(w4.telemetry_line(), "pi · glm-5.3 · max");
        assert_eq!(w4.activity, "작업 완료");
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
        let d = detail(
            "addr-9",
            "",
            "pi",
            &["pi"],
            Some("/r/my-repo/.herdr/worktrees/c1"),
        );
        let w = infer_live_worker_slot(&d, None).unwrap();
        assert_eq!(w.repo_tag, "my-repo");
        assert_eq!(w.name, "addr-9"); // 이름 없으면 address
        assert_eq!(w.cwd.as_deref(), Some("/r/my-repo/.herdr/worktrees/c1"));
    }

    // ---------- format_role_badge ----------

    #[test]
    fn role_badge_formats() {
        assert_eq!(format_role_badge("pi", None), "pi");
        assert_eq!(
            format_role_badge("pi", Some("glm-zcode/glm-5.3")),
            "pi:glm-5.3"
        );
        assert_eq!(format_role_badge("pi", Some("  ")), "pi");
        // 중복 접두 정규화 glm-glm-5.3 → glm-5.3
        assert_eq!(format_role_badge("pi", Some("x/glm-glm-5.3")), "pi:glm-5.3");
        assert_eq!(format_role_badge("devin", Some("devin-1")), "devin:1"); // cli- 접두 제거
                                                                            // 경계: 중복이 아닌 접두는 유지
        assert_eq!(format_role_badge("pi", Some("glm-5.3")), "pi:glm-5.3");
        assert_eq!(
            format_role_badge("claude", Some("claude-sonnet-4-6")),
            "claude:sonnet-4-6"
        );
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

    fn worker(slot: LiveWorkerSlot, short: &str, model: Option<&str>) -> LiveWorkerInfo {
        LiveWorkerInfo {
            slot,
            badge: format!("{} (실행 중)", short),
            short_model: short.to_string(),
            model_name: model
                .map(str::to_string)
                .unwrap_or_else(|| short.to_string()),
            address: format!("addr-{}", short),
            owner_address: None,
            name: short.to_string(),
            cwd: None,
            repo_tag: "r".into(),
            activity: "a".into(),
            status: None,
            turn: None,
            cli: short.split(':').next().unwrap_or(short).to_string(),
            model: model.map(str::to_string),
            effort: None,
            subagent_warning: None,
            recent_io: Vec::new(),
            recent_output: Vec::new(),
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
            worker(LiveWorkerSlot::Coder, "pi:glm-5.3", Some("glm-zcode/glm-5.3")),
            worker(LiveWorkerSlot::Coder, "devin", None), // 동일 슬롯 2번째 — 무시
            worker(LiveWorkerSlot::Verifier, "devin", None),
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
            worker(LiveWorkerSlot::Lead, "pi:glm-5.3", Some("glm-zcode/glm-5.3")),
            worker(
                LiveWorkerSlot::Verifier,
                "pi:glm-5.3",
                Some("glm-zcode/glm-5.3"),
            ),
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
        fn new(
            ls_json: Vec<String>,
            details: HashMap<String, String>,
            screens: HashMap<String, String>,
        ) -> Self {
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
            self.calls.lock().unwrap().push((
                args[0].to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
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

    #[test]
    fn snapshotter_propagates_hc_ls_turn_into_worker() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-turn");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let mut entry = worker_ls_entry("a1", "w-1");
        entry["turn"] = serde_json::json!(14);
        let ls = serde_json::json!({ "terminals": [entry] }).to_string();
        let details = HashMap::from([(
            "a1".to_string(),
            serde_json::json!({ "address": "a1", "agent": "pi", "name": "w-1", "argv": ["pi"] })
                .to_string(),
        )]);
        let mock = Arc::new(MockHc::new(vec![ls], details, HashMap::new()));
        let mut snap = snapshot_with_mock(mock, &tmp);
        let merged = snap.tick_now();
        assert_eq!(merged.workers[0].turn, Some(14));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn snapshot_with_mock(mock: Arc<MockHc>, repo_dir: &Path) -> LiveSyncSnapshotter {
        LiveSyncSnapshotter::new(LiveSyncOptions {
            repo_dir: repo_dir.to_path_buf(),
            repo_filter_dir: None,
            hc_bin: "hc".to_string(),
            exec_runner: mock,
            get_watched_lead: None,
            proc_lister: Arc::new(|| None),
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
            proc_lister: Arc::new(|| None),
            on_update: Some(Arc::new(Mutex::new(move |m: &MergedLiveRoster| {
                sink.lock().unwrap().push(m.clone());
            }))),
        });
        let merged = snap.tick_now();
        assert_eq!(merged.active_worker_count, 1);
        assert_eq!(merged.workers[0].address, "a1");
        assert_eq!(merged.workers[0].slot, LiveWorkerSlot::Coder);
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
        // 워커 규약 밖 최상위 AI 세션 — working·idle 모두 Lead 카드로 관측된다
        // (idle 리드가 사라지지 않고 'antigravity (대기 중)'로 유지되는 근거)
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
        // working(t155) + idle(t156) 최상위 AI 세션 2개 관측, zsh(t157)는 제외
        assert_eq!(merged.active_worker_count, 2);
        let w = &merged.workers[0];
        assert_eq!(w.slot, LiveWorkerSlot::Lead);
        assert_eq!(w.name, "paperwork-operations");
        assert_eq!(w.short_model, "antigravity");
        let w2 = &merged.workers[1];
        assert_eq!(w2.slot, LiveWorkerSlot::Lead);
        assert_eq!(w2.name, "idle-session");
        assert_eq!(w2.short_model, "antigravity");
        assert!(merged.workers.iter().all(|w| w.name != "shell"));
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
            serde_json::json!({ "address": "a1", "agent": "pi", "name": "w-1", "argv": ["pi"] })
                .to_string(),
        )]);
        let mock = Arc::new(MockHc::new(
            vec![good, "not-json".to_string(), "{}".to_string()],
            details,
            HashMap::new(),
        ));
        let mut snap = snapshot_with_mock(mock, &tmp);
        let m1 = snap.tick_now();
        assert_eq!(m1.active_worker_count, 1);
        // 실행 오류·malformed·terminals 누락 모두 이전 워커 유지(허위 0건 방송 금지)
        let m2 = snap.tick_now();
        assert_eq!(
            m2.active_worker_count, 1,
            "exec error must preserve workers"
        );
        let m3 = snap.tick_now();
        assert_eq!(
            m3.active_worker_count, 1,
            "{{}} payload must preserve workers"
        );
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
        assert!(
            !snap.detail_cache.contains_key("a2"),
            "departed address must be evicted"
        );
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
        let details = HashMap::from([(
            "a1".to_string(),
            serde_json::json!({ "address": "a1", "agent": "pi", "name": "w-1", "argv": ["pi"] })
                .to_string(),
        )]);
        let mock = Arc::new(MockHc::new(vec![ls], details, HashMap::new()));
        let _g: MutexGuard<'_, ()> = env_lock();
        isolate_env(&tmp);
        let mut snap = LiveSyncSnapshotter::new(LiveSyncOptions {
            repo_dir: repo.clone(),
            repo_filter_dir: Some(repo.clone()),
            hc_bin: "hc".to_string(),
            exec_runner: mock,
            get_watched_lead: None,
            proc_lister: Arc::new(|| None),
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
            proc_lister: Arc::new(|| None),
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
        let mock = Arc::new(MockHc::new(
            vec![empty_ls.clone(), empty_ls],
            HashMap::new(),
            HashMap::new(),
        ));
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
            serde_json::json!({ "address": "a1", "agent": "pi", "name": "w-1", "argv": ["pi"] })
                .to_string(),
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
        assert_eq!(
            m2.workers[0].status.as_deref(),
            Some("done"),
            "fresh ls status must override cached detail"
        );
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
        let expect =
            roster_labels_from_profile("quota-saver", "builtin", &cfg.profiles["quota-saver"]);
        assert_eq!(labels, expect);
    }

    // ---------- 이중 카드 방지(dg wrap 호스팅 터미널) + env 접두어 파싱 ----------

    #[test]
    fn find_wrap_processes_env_prefix_launcher_dash_and_child() {
        // (1) 런처 env — `env VAR=1 dg wrap …` 라인 감지 자체가 env 때문에 놓치던 경우
        let ps1 = "  500     1 /bin/zsh -l\n\
  900   500 env DGATE_PROBE=1 dg wrap --label env-launch -- pi --model probe-m\n\
  901   900 pi --model probe-m\n";
        let procs1 = find_wrap_processes(&parse_ps_rows(ps1));
        assert_eq!(procs1.len(), 1, "env 접두 dg wrap 라인 감지");
        assert_eq!(procs1[0].cli, "pi");
        assert_eq!(procs1[0].model.as_deref(), Some("probe-m"));
        assert_eq!(procs1[0].label.as_deref(), Some("env-launch"));

        // (2) dash 토큰 env — `-- env VAR=1 pi …` → env 무시 후 pi·--model 파싱
        let ps2 = "  500     1 /bin/zsh -l\n\
  910   500 node /x/src/decision/dg.ts wrap --label env-dash -- env PI_API=secret pi --model glm-5.3\n";
        let procs2 = find_wrap_processes(&parse_ps_rows(ps2));
        assert_eq!(procs2.len(), 1);
        assert_eq!(procs2[0].cli, "pi");
        assert_eq!(procs2[0].model.as_deref(), Some("glm-5.3"));

        // (3) BFS 자식 env — 자식이 `env VAR=1 pi …`여도 헤드 매칭·파싱
        let ps3 = "  500     1 /bin/zsh -l\n\
  920   500 dg wrap --label env-child\n\
  921   920 env PI_TOOL=1 pi --model child-m\n";
        let procs3 = find_wrap_processes(&parse_ps_rows(ps3));
        assert_eq!(procs3.len(), 1);
        assert_eq!(procs3[0].cli, "pi");
        assert_eq!(procs3[0].model.as_deref(), Some("child-m"));
    }

    #[test]
    fn snapshotter_skips_dg_wrap_hosting_terminal_cards() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-wrapskip");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        // ① 직접 실행(foreground가 dg wrap) ② env+bash -lc 안 `; dg wrap`(실측 t1md 형태)
        // ③ 일반 bash 실행 워커 터미널 — 유지 대상
        let ls = serde_json::json!({
            "terminals": [
                { "address": "tDg", "agent": "dg", "name": "coder-direct", "parent": "t0", "state": "live", "status": "working", "cwd": "/tmp/repo-a" },
                { "address": "tEnv", "agent": "env", "name": "coder-envwrap", "parent": "t0", "state": "live", "status": "working", "cwd": "/tmp/repo-a" },
                { "address": "tSh", "agent": "pi", "name": "coder-shelljob", "parent": null, "state": "live", "status": "working", "cwd": "/tmp/repo-a" }
            ]
        })
        .to_string();
        let details = HashMap::from([
            (
                "tDg".to_string(),
                serde_json::json!({
                    "address": "tDg", "agent": "dg", "name": "coder-direct", "state": "live",
                    "status": "working", "cwd": "/tmp/repo-a",
                    "argv": ["dg", "wrap", "--label", "direct", "--", "pi", "--model", "m-dg"],
                    "foreground": { "pid": 900, "argv": ["/x/target/release/dg", "wrap", "--label", "direct", "--", "pi", "--model", "m-dg"] }
                })
                .to_string(),
            ),
            (
                "tEnv".to_string(),
                serde_json::json!({
                    "address": "tEnv", "agent": "env", "name": "coder-envwrap", "state": "live",
                    "status": "working", "cwd": "/tmp/repo-a",
                    "argv": ["env", "HERDR_MERGE_GATE=1", "bash", "-lc",
                             "cd /w/repo; dg wrap --label envwrap -- devin -p 작업"],
                    "foreground": { "pid": 901, "argv": ["bash", "-lc",
                             "cd /w/repo; dg wrap --label envwrap -- devin -p 작업"] }
                })
                .to_string(),
            ),
            (
                "tSh".to_string(),
                serde_json::json!({
                    "address": "tSh", "agent": "pi", "name": "coder-shelljob", "state": "live",
                    "status": "working", "cwd": "/tmp/repo-a",
                    "argv": ["/b/pi", "--model", "m-sh"],
                    "foreground": { "pid": 902, "argv": ["/bin/bash", "build.sh"] }
                })
                .to_string(),
            ),
        ]);
        let mock = Arc::new(MockHc::new(vec![ls], details, HashMap::new()));
        let mut snap = snapshot_with_mock(mock, &tmp);
        let merged = snap.tick_now();
        let mut addrs: Vec<&str> = merged.workers.iter().map(|w| w.address.as_str()).collect();
        addrs.sort();
        // ①② dg wrap 호스팅 터미널 카드 소멸(§3.5 ps wrap 카드가 대신 표시), ③ bash 실행 카드 유지
        assert_eq!(addrs, vec!["tSh"]);
        assert_eq!(merged.workers[0].name, "coder-shelljob");
        assert_eq!(merged.workers[0].owner_address, None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn wrap_proc_sets_owner_address_for_owned_and_none_for_unowned() {
        let proc = WrapProc {
            wrap_pid: 4242,
            ancestors: vec![100, 1],
            cli: "pi".to_string(),
            model: Some("glm-zcode/glm-5.3".to_string()),
            effort: Some("high".to_string()),
            role: None,
            label: Some("coder-auth".to_string()),
            project_path: Some("/tmp/repo-a".to_string()),
            project: Some("coder-auth".to_string()),
            route: Some("file_select".to_string()),
        };
        let parent = HcTerminalSummary {
            address: "tSub1".to_string(),
            agent: Some("pi".to_string()),
            name: Some("sublead-auth".to_string()),
            parent: Some("tLead0".to_string()),
            state: Some("live".to_string()),
            status: Some("working".to_string()),
            cwd: Some("/tmp/repo-a".to_string()),
            turn: None,
        };
        let owned = wrap_proc_to_worker(&proc, &parent);
        assert_eq!(owned.address, "pid:4242");
        assert_eq!(owned.owner_address.as_deref(), Some("tSub1"));

        let unowned = wrap_proc_to_unowned_worker(&proc);
        assert_eq!(unowned.address, "pid:4242");
        assert_eq!(unowned.owner_address, None);
    }

    #[test]
    fn terminal_worker_owner_address_uses_hc_parent_when_present() {
        let mut d = detail(
            "tChild",
            "coder-child",
            "pi",
            &["pi", "--model", "m-child"],
            Some("/tmp/repo-a"),
        );
        d.parent = Some("tSub".to_string());
        let owned = infer_live_worker_slot(&d, None).unwrap();
        assert_eq!(owned.owner_address.as_deref(), Some("tSub"));

        d.parent = Some(String::new());
        let empty = infer_live_worker_slot(&d, None).unwrap();
        assert_eq!(empty.owner_address, None);

        d.parent = None;
        let absent = infer_live_worker_slot(&d, None).unwrap();
        assert_eq!(absent.owner_address, None);
    }

    #[test]
    fn nested_hc_run_bg_wrap_child_uses_parent_sublead_as_owner() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-nested-wrap-owner");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let ls = serde_json::json!({
            "terminals": [
                { "address": "tSub", "agent": "dg", "name": "sublead-auth", "parent": null, "state": "live", "status": "working", "cwd": "/tmp/repo-a" },
                { "address": "tChild", "agent": "dg", "name": "coder-child", "parent": "tSub", "state": "live", "status": "working", "cwd": "/tmp/repo-a" }
            ]
        })
        .to_string();
        let details = HashMap::from([
            (
                "tSub".to_string(),
                serde_json::json!({
                    "address": "tSub", "agent": "dg", "name": "sublead-auth", "state": "live",
                    "status": "working", "cwd": "/tmp/repo-a", "pid": 1000,
                    "argv": ["dg", "wrap", "--role", "sublead", "--label", "sublead-auth", "--", "pi", "--model", "m-sub"],
                    "foreground": { "pid": 111, "argv": ["dg", "wrap", "--role", "sublead", "--label", "sublead-auth", "--", "pi", "--model", "m-sub"] }
                })
                .to_string(),
            ),
            (
                "tChild".to_string(),
                serde_json::json!({
                    "address": "tChild", "agent": "dg", "name": "coder-child", "parent": "tSub", "state": "live",
                    "status": "working", "cwd": "/tmp/repo-a", "pid": 2000,
                    "argv": ["dg", "wrap", "--role", "coder", "--label", "coder-child", "--", "pi", "--model", "m-child"],
                    "foreground": { "pid": 222, "argv": ["dg", "wrap", "--role", "coder", "--label", "coder-child", "--", "pi", "--model", "m-child"] }
                })
                .to_string(),
            ),
        ]);
        let mock = Arc::new(MockHc::new(vec![ls], details, HashMap::new()));
        let ps = concat!(
            "1000     1 /bin/zsh -l\n",
            "2000     1 /bin/zsh -l\n",
            " 111  1000 dg wrap --role sublead --label sublead-auth -- pi --model m-sub\n",
            " 112   111 pi --model m-sub\n",
            " 222  2000 dg wrap --role coder --label coder-child -- pi --model m-child\n",
            " 223   222 pi --model m-child\n"
        )
        .to_string();
        let mut snap = LiveSyncSnapshotter::new(LiveSyncOptions {
            repo_dir: tmp.clone(),
            repo_filter_dir: None,
            hc_bin: "hc".to_string(),
            exec_runner: mock,
            get_watched_lead: None,
            proc_lister: Arc::new(move || Some(ps.clone())),
            on_update: None,
        });
        let merged = snap.tick_now();
        let sub = merged
            .workers
            .iter()
            .find(|w| w.address == "pid:111")
            .unwrap_or_else(|| panic!("sublead wrap card missing: {:?}", merged.workers));
        assert_eq!(sub.slot, LiveWorkerSlot::Sublead);
        assert_eq!(sub.owner_address.as_deref(), Some("tSub"));
        let coder = merged
            .workers
            .iter()
            .find(|w| w.address == "pid:222")
            .unwrap_or_else(|| panic!("coder wrap card missing: {:?}", merged.workers));
        assert_eq!(coder.slot, LiveWorkerSlot::Coder);
        assert_eq!(
            coder.owner_address.as_deref(),
            Some("tSub"),
            "hc run --bg child coder must attach to the parent sublead terminal, not its own wrap-host terminal"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn direct_exec_hc_run_bg_wrap_matches_root_pid_without_shell() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-direct-exec-wrap");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let ls = serde_json::json!({
            "terminals": [
                { "address": "tSub", "agent": "env", "name": "sublead-rust-p2r", "parent": "t155", "state": "live", "status": "working", "cwd": "/tmp/repo-a" },
                { "address": "tChild", "agent": "dg", "name": "coder-p2r-w2", "parent": "tSub", "state": "live", "status": "working", "cwd": "/tmp/repo-a" }
            ]
        })
        .to_string();
        let details = HashMap::from([
            (
                "tSub".to_string(),
                serde_json::json!({
                    "address": "tSub", "agent": "env", "name": "sublead-rust-p2r", "parent": "t155", "state": "live",
                    "status": "working", "cwd": "/tmp/repo-a", "pid": 6676,
                    "argv": ["env", "HERDR_ROLE=sublead", "/usr/local/bin/dg", "wrap", "--role", "sublead", "--label", "sublead-rust-p2r", "--", "pi", "--model", "m-sub"],
                    "foreground": { "pid": 6676, "argv": ["/usr/local/bin/dg", "wrap", "--role", "sublead", "--label", "sublead-rust-p2r", "--", "pi", "--model", "m-sub"] }
                })
                .to_string(),
            ),
            (
                "tChild".to_string(),
                serde_json::json!({
                    "address": "tChild", "agent": "dg", "name": "coder-p2r-w2", "parent": "tSub", "state": "live",
                    "status": "working", "cwd": "/tmp/repo-a", "pid": 29876,
                    "argv": ["dg", "wrap", "--role", "coder", "--label", "coder-p2r-w2", "--", "/bin/sh", "-c", "devin --model swe-2-1-high -p task"],
                    "foreground": { "pid": 29876, "argv": ["/usr/local/bin/dg", "wrap", "--role", "coder", "--label", "coder-p2r-w2", "--", "/bin/sh", "-c", "devin --model swe-2-1-high -p task"] }
                })
                .to_string(),
            ),
        ]);
        let mock = Arc::new(MockHc::new(vec![ls], details, HashMap::new()));
        // 실제 hc run --bg 처럼 중간 셸 없이 dg wrap 자체가 터미널 루트 pid로 실행된 ps 트리
        let ps = concat!(
            " 6676     1 /usr/local/bin/dg wrap --role sublead --label sublead-rust-p2r -- pi --model m-sub\n",
            " 6686  6676 pi --model m-sub\n",
            "29876     1 /usr/local/bin/dg wrap --role coder --label coder-p2r-w2 -- /bin/sh -c devin --model swe-2-1-high -p task\n",
            "29887 29876 /bin/sh -c devin --model swe-2-1-high -p task\n"
        )
        .to_string();
        let mut snap = LiveSyncSnapshotter::new(LiveSyncOptions {
            repo_dir: tmp.clone(),
            repo_filter_dir: None,
            hc_bin: "hc".to_string(),
            exec_runner: mock,
            get_watched_lead: None,
            proc_lister: Arc::new(move || Some(ps.clone())),
            on_update: None,
        });
        let merged = snap.tick_now();
        let sub = merged
            .workers
            .iter()
            .find(|w| w.address == "pid:6676")
            .unwrap_or_else(|| panic!("direct-exec sublead missing: {:?}", merged.workers));
        assert_eq!(sub.slot, LiveWorkerSlot::Sublead);
        assert_eq!(sub.owner_address.as_deref(), Some("tSub"));
        let coder = merged
            .workers
            .iter()
            .find(|w| w.address == "pid:29876")
            .unwrap_or_else(|| panic!("direct-exec coder missing: {:?}", merged.workers));
        assert_eq!(coder.slot, LiveWorkerSlot::Coder);
        assert_eq!(
            coder.owner_address.as_deref(),
            Some("tSub"),
            "direct-exec hc run --bg child coder (wrap_pid == terminal.pid) must attach to parent sublead"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parse_herdr_agent_list_handles_wrapped_and_filters_stopped() {
        let wrapped = serde_json::json!({
            "agents": [
                {
                    "id": "seat-01",
                    "name": "devin-01",
                    "role": "coder",
                    "cli": "devin",
                    "model": "swe-2-1-high",
                    "status": "working",
                    "cwd": "/tmp/repo-a",
                    "task": "PR #101 구현",
                    "turn": 4
                },
                {
                    "id": "seat-02",
                    "name": "stopped-seat",
                    "role": "coder",
                    "status": "stopped",
                    "cwd": "/tmp/repo-a"
                }
            ]
        })
        .to_string();
        let seats = parse_herdr_agent_list(&wrapped);
        assert_eq!(seats.len(), 2);
        let active: Vec<LiveWorkerInfo> =
            seats.iter().filter_map(herdr_seat_to_worker).collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].address, "herdr:seat-01");
        assert_eq!(active[0].slot, LiveWorkerSlot::Coder);
        assert_eq!(active[0].cli, "devin");
        assert_eq!(active[0].model.as_deref(), Some("swe-2-1-high"));
        assert_eq!(active[0].turn, Some(4));
        assert!(active[0].activity.contains("PR #101"));
    }

    #[test]
    fn snapshotter_merges_herdr_agent_seats_when_hc_has_zero_workers() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-herdr-seats");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);

        struct HerdrOnlyMock {
            seats_json: String,
        }
        impl HcExecRunner for HerdrOnlyMock {
            fn exec(&self, bin: &str, args: &[&str], _timeout_ms: u64) -> Result<String, String> {
                if bin == "herdr" && args == ["agent", "list", "--json"] {
                    return Ok(self.seats_json.clone());
                }
                if args.first().copied() == Some("ls") {
                    return Ok(r#"{"terminals":[]}"#.to_string());
                }
                Err("not mocked".to_string())
            }
        }

        let mut agents = Vec::new();
        for i in 1..=28 {
            agents.push(serde_json::json!({
                "id": format!("s{i:02}"),
                "name": format!("devin-seat-{i:02}"),
                "cli": "devin",
                "model": "swe-2-1-high",
                "status": if i % 2 == 0 { "working" } else { "idle" },
                "cwd": "/tmp/repo-a"
            }));
        }
        let mock = Arc::new(HerdrOnlyMock {
            seats_json: serde_json::json!({ "agents": agents }).to_string(),
        });
        let mut snap = LiveSyncSnapshotter::new(LiveSyncOptions {
            repo_dir: tmp.clone(),
            repo_filter_dir: None,
            hc_bin: "hc".to_string(),
            exec_runner: mock,
            get_watched_lead: None,
            proc_lister: Arc::new(|| None),
            on_update: None,
        });
        let merged = snap.tick_now();
        assert_eq!(merged.workers.len(), 28, "28개 herdr 좌석이 hc 0개 환경에서도 전부 노출");
        assert_eq!(merged.workers[0].address, "herdr:s01");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn synchronous_wrap_in_shell_keeps_host_terminal_as_owner() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-sync-wrap-owner");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let ls = serde_json::json!({
            "terminals": [
                { "address": "tSub", "agent": "pi", "name": "sublead-auth", "parent": "tLead", "state": "live", "status": "working", "cwd": "/tmp/repo-a" }
            ]
        })
        .to_string();
        let details = HashMap::from([(
            "tSub".to_string(),
            serde_json::json!({
                "address": "tSub", "agent": "pi", "name": "sublead-auth", "parent": "tLead", "state": "live",
                "status": "working", "cwd": "/tmp/repo-a", "pid": 1000,
                "argv": ["pi", "--model", "m-sub"],
                "foreground": { "pid": 333, "argv": ["dg", "wrap", "--role", "coder", "--label", "coder-sync", "--", "pi", "--model", "m-child"] }
            })
            .to_string(),
        )]);
        let mock = Arc::new(MockHc::new(vec![ls], details, HashMap::new()));
        let ps = concat!(
            "1000     1 pi --model m-sub\n",
            " 333  1000 dg wrap --role coder --label coder-sync -- pi --model m-child\n",
            " 334   333 pi --model m-child\n"
        )
        .to_string();
        let mut snap = LiveSyncSnapshotter::new(LiveSyncOptions {
            repo_dir: tmp.clone(),
            repo_filter_dir: None,
            hc_bin: "hc".to_string(),
            exec_runner: mock,
            get_watched_lead: None,
            proc_lister: Arc::new(move || Some(ps.clone())),
            on_update: None,
        });
        let merged = snap.tick_now();
        let coder = merged
            .workers
            .iter()
            .find(|w| w.address == "pid:333")
            .unwrap_or_else(|| panic!("sync wrap card missing: {:?}", merged.workers));
        assert_eq!(coder.owner_address.as_deref(), Some("tSub"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn snapshotter_adds_only_unowned_standalone_os_processes() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-standalone-os");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let ls = serde_json::json!({
            "terminals": [
                { "address": "tShell", "agent": "zsh", "name": "shell", "state": "live", "status": "working", "cwd": "/tmp/repo-a" }
            ]
        })
        .to_string();
        let details = HashMap::from([(
            "tShell".to_string(),
            serde_json::json!({
                "address": "tShell", "agent": "zsh", "state": "live", "status": "working",
                "cwd": "/tmp/repo-a", "pid": 900,
                "foreground": { "pid": 901, "argv": ["devin", "--model", "swe-2-1-high", "-p", "owned"] }
            })
            .to_string(),
        )]);
        let mock = Arc::new(MockHc::new(vec![ls], details, HashMap::new()));
        let ps = concat!(
            " 900     1 /bin/zsh -l\n",
            " 901   900 devin --model swe-2-1-high -p owned\n",
            " 902   901 claude --model sonnet\n",
            " 910     1 dg wrap --label owned-wrap -- pi --model glm-5.3\n",
            " 911   910 pi --model glm-5.3\n",
            " 920     1 codex --model gpt-5\n"
        )
        .to_string();
        let mut snap = LiveSyncSnapshotter::new(LiveSyncOptions {
            repo_dir: tmp.clone(),
            repo_filter_dir: None,
            hc_bin: "hc".to_string(),
            exec_runner: mock,
            get_watched_lead: None,
            proc_lister: Arc::new(move || Some(ps.clone())),
            on_update: None,
        });
        let merged = snap.tick_now();
        let os_workers: Vec<&LiveWorkerInfo> = merged
            .workers
            .iter()
            .filter(|w| w.address.starts_with("os:"))
            .collect();
        assert_eq!(os_workers.len(), 1, "duplicate os cards: {:?}", merged.workers);
        assert_eq!(os_workers[0].address, "os:920");
        assert_eq!(os_workers[0].cli, "codex");
        assert!(
            merged.workers.iter().any(|w| w.address == "pid:910"),
            "wrap card itself must remain visible"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn snapshotter_skips_standalone_os_processes_when_repo_scoped() {
        let _g: MutexGuard<'_, ()> = env_lock();
        let tmp = temp_dir("live-standalone-scope");
        std::fs::create_dir_all(&tmp).unwrap();
        isolate_env(&tmp);
        let ls = serde_json::json!({ "terminals": [] }).to_string();
        let mock = Arc::new(MockHc::new(vec![ls], HashMap::new(), HashMap::new()));
        let ps = "  930     1 devin --model swe-2-1-high -p unscoped only\n".to_string();
        let mut snap = LiveSyncSnapshotter::new(LiveSyncOptions {
            repo_dir: tmp.clone(),
            repo_filter_dir: Some(tmp.clone()),
            hc_bin: "hc".to_string(),
            exec_runner: mock,
            get_watched_lead: None,
            proc_lister: Arc::new(move || Some(ps.clone())),
            on_update: None,
        });
        let merged = snap.tick_now();
        assert!(
            !merged.workers.iter().any(|w| w.address.starts_with("os:")),
            "raw ps rows have no cwd and must not leak into --repo scopes: {:?}",
            merged.workers
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn matches_repo_scope_includes_external_linked_git_worktrees() {
        let base = temp_dir("linked-wt-scope");
        let main_repo = base.join("peers_bridge");
        let other_repo = base.join("other_repo");
        let ext_wt = base.join(".worktrees").join("peers_bridge").join("track-a");
        let ext_wt_sub = ext_wt.join("src").join("server");
        let other_wt = base.join(".worktrees").join("other_repo").join("w1");

        std::fs::create_dir_all(main_repo.join(".git").join("worktrees").join("track-a")).unwrap();
        std::fs::create_dir_all(other_repo.join(".git").join("worktrees").join("w1")).unwrap();
        std::fs::create_dir_all(&ext_wt_sub).unwrap();
        std::fs::create_dir_all(&other_wt).unwrap();

        std::fs::write(
            ext_wt.join(".git"),
            format!(
                "gitdir: {}\n",
                main_repo.join(".git").join("worktrees").join("track-a").display()
            ),
        )
        .unwrap();
        std::fs::write(
            other_wt.join(".git"),
            format!(
                "gitdir: {}\n",
                other_repo.join(".git").join("worktrees").join("w1").display()
            ),
        )
        .unwrap();

        let scope = main_repo.to_str().unwrap();
        assert!(matches_repo_scope(ext_wt.to_str(), Some(scope)));
        assert!(matches_repo_scope(ext_wt_sub.to_str(), Some(scope)));
        assert!(!matches_repo_scope(other_wt.to_str(), Some(scope)));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parse_hc_get_json_prefers_foreground_cwd_over_shell_cwd() {
        let raw = r#"{
            "address": "t4kw",
            "agent": "antigravity",
            "cwd": "/Users/kwak/Projects/skills/dgate",
            "state": "live",
            "status": {"app": "antigravity", "state": "working"},
            "argv": ["/bin/zsh", "-l"],
            "foreground": {
                "argv": ["agy", "--dangerously-skip-permissions"],
                "cwd": "/Users/kwak/Projects/skills/peers_bridge",
                "pid": 38121
            }
        }"#;
        let detail = parse_hc_get_json(raw).expect("valid hc get json");
        assert_eq!(
            detail.foreground_cwd.as_deref(),
            Some("/Users/kwak/Projects/skills/peers_bridge")
        );
        assert_eq!(
            detail.cwd.as_deref(),
            Some("/Users/kwak/Projects/skills/peers_bridge")
        );
    }

}
