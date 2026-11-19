//! Decision 통계 엔진 코어 — `src/decision/engine.ts` (plan-r2.1 §2.0·§4) 포팅.
//!
//! 목적: 감쇠 Beta(EWMA) 사후평균, 계층적 Shrinkage, 급성 회귀 강제 split(N7 카운터
//! 분리), 재시도 1회 실행=1행 불변식(B7), Fork 생명주기(발행→touch→닫힘→고아 TTL
//! 정산→Neon backprop), 자기 호출 제외(B6)까지 공유하는 단일 구현을 제공한다.
//!
//! 주의:
//! - 상수(γ=0.92, M0=6, N_min=5, λ_max=0.25)와 p0 테이블은 §4.2~§4.3 확정값 그대로다.
//! - 모든 갱신은 `BEGIN IMMEDIATE ... COMMIT` 단일 트랜잭션으로 실행한다(§3.2).
//! - 트랜잭션 없는 단일 UPDATE는 TS 원본과 동일하게 그대로 둔다(touch 등).

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::db::{has_column, has_table, GLOBAL_PROJECT_ID};
use crate::redact::redact_secrets;
use crate::roster::CostStatus;
use crate::verdict::{VerdictDecision, VerdictValidationResult};

// ---------------------------------------------------------------------------
// 상수 (§4.2~§4.6)
// ---------------------------------------------------------------------------

/// EWMA 감쇠율 γ=0.92 (유효 윈도우 12.5건, 반감기 ≈ 8.3건)
pub const GAMMA: f64 = 0.92;
/// 가상 사전표본 크기 M0=6 (§4.3)
pub const M0: f64 = 6.0;
/// 신뢰도 표지 전환 임계 (n_raw >= 5 → measured)
pub const N_MIN: i64 = 5;
/// 전역 풀 가중치 상한 λ_max=0.25 (타 프로젝트 오염 차단 — §4.2)
pub const LAMBDA_MAX: f64 = 0.25;
/// sharp 패스트패스 확신 기준 (§2.0 4조건 중 p 조건)
pub const SHARP_P_THRESHOLD: f64 = 0.7;
/// 연속 오류 강제 split 임계 (§4.4)
pub const FORCED_SPLIT_ERRORS: i64 = 2;
/// 역할별 TTL(ms): 탐색 600,000 / 코딩 3,600,000 (§4.6)
pub const TTL_EXPLORE_MS: i64 = 600_000;
pub const TTL_CODING_MS: i64 = 3_600_000;
/// 재시도 판정: 직전 실패 이후 스텝 거리 (§4.5)
pub const RETRY_STEP_WINDOW: i64 = 3;
/// `error_snippet` 상한 — 리댁션 적용 후 이 길이로 절단한다 (§3.1-B)
pub const ERROR_SNIPPET_MAX_CHARS: usize = 300;
/// `waiting_human` 메타 상태의 TTL 면제 상한 — 7일 경과 시 고아 정산 재개 (§2.2)
pub const WAITING_HUMAN_MAX_MS: i64 = 7 * 86_400_000;

// ---------------------------------------------------------------------------
// 열거형
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RouteId {
    #[default]
    FileSelect,
    ToolSelect,
    RetryOrAbort,
    DelegateSwarm,
}

impl RouteId {
    pub fn as_str(&self) -> &'static str {
        match self {
            RouteId::FileSelect => "file_select",
            RouteId::ToolSelect => "tool_select",
            RouteId::RetryOrAbort => "retry_or_abort",
            RouteId::DelegateSwarm => "delegate_swarm",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "file_select" => Some(RouteId::FileSelect),
            "tool_select" => Some(RouteId::ToolSelect),
            "retry_or_abort" => Some(RouteId::RetryOrAbort),
            "delegate_swarm" => Some(RouteId::DelegateSwarm),
            _ => None,
        }
    }
}

pub const ROUTE_IDS: [RouteId; 4] = [
    RouteId::FileSelect,
    RouteId::ToolSelect,
    RouteId::RetryOrAbort,
    RouteId::DelegateSwarm,
];

pub fn is_route_id(v: &str) -> bool {
    RouteId::parse(v).is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionBasis {
    Prior,
    Measured,
}

impl DecisionBasis {
    pub fn as_str(&self) -> &'static str {
        match self {
            DecisionBasis::Prior => "prior",
            DecisionBasis::Measured => "measured",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    ExploreRead,
    CodeSearch,
    StaticCheck,
    SingleEdit,
    MultiEdit,
    TestOrDeploy,
    SwarmFanout,
}

pub const TASK_KINDS: [TaskKind; 7] = [
    TaskKind::ExploreRead,
    TaskKind::CodeSearch,
    TaskKind::StaticCheck,
    TaskKind::SingleEdit,
    TaskKind::MultiEdit,
    TaskKind::TestOrDeploy,
    TaskKind::SwarmFanout,
];

impl TaskKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskKind::ExploreRead => "explore_read",
            TaskKind::CodeSearch => "code_search",
            TaskKind::StaticCheck => "static_check",
            TaskKind::SingleEdit => "single_edit",
            TaskKind::MultiEdit => "multi_edit",
            TaskKind::TestOrDeploy => "test_or_deploy",
            TaskKind::SwarmFanout => "swarm_fanout",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "explore_read" => Some(TaskKind::ExploreRead),
            "code_search" => Some(TaskKind::CodeSearch),
            "static_check" => Some(TaskKind::StaticCheck),
            "single_edit" => Some(TaskKind::SingleEdit),
            "multi_edit" => Some(TaskKind::MultiEdit),
            "test_or_deploy" => Some(TaskKind::TestOrDeploy),
            "swarm_fanout" => Some(TaskKind::SwarmFanout),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl RiskLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            RiskLevel::Low => "low",
            RiskLevel::Medium => "medium",
            RiskLevel::High => "high",
        }
    }
    pub fn rank(&self) -> u8 {
        match self {
            RiskLevel::Low => 0,
            RiskLevel::Medium => 1,
            RiskLevel::High => 2,
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "low" => Some(RiskLevel::Low),
            "medium" => Some(RiskLevel::Medium),
            "high" => Some(RiskLevel::High),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    Hook,
    AgyStream,
    Transcript,
    RestApi,
    Cli,
    Wrap,
    TtlOrphan,
    NeonBackprop,
}

pub const EVENT_SOURCES: [EventSource; 8] = [
    EventSource::Hook,
    EventSource::AgyStream,
    EventSource::Transcript,
    EventSource::RestApi,
    EventSource::Cli,
    EventSource::Wrap,
    EventSource::TtlOrphan,
    EventSource::NeonBackprop,
];

impl EventSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventSource::Hook => "hook",
            EventSource::AgyStream => "agy_stream",
            EventSource::Transcript => "transcript",
            EventSource::RestApi => "rest_api",
            EventSource::Cli => "cli",
            EventSource::Wrap => "wrap",
            EventSource::TtlOrphan => "ttl_orphan",
            EventSource::NeonBackprop => "neon_backprop",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hook" => Some(EventSource::Hook),
            "agy_stream" => Some(EventSource::AgyStream),
            "transcript" => Some(EventSource::Transcript),
            "rest_api" => Some(EventSource::RestApi),
            "cli" => Some(EventSource::Cli),
            "wrap" => Some(EventSource::Wrap),
            "ttl_orphan" => Some(EventSource::TtlOrphan),
            "neon_backprop" => Some(EventSource::NeonBackprop),
            _ => None,
        }
    }
}

pub fn is_event_source(v: &str) -> bool {
    EventSource::parse(v).is_some()
}

/// 실패 원인 11종 — `dg wrap`·verdict·고아 정산이 공유하는 분류 enum.
/// `config_error`/`interrupted`는 표본 정직성 보호를 위해 `stats_summary`·
/// `__GLOBAL__`·`consecutive_errors`에서 격리된다(감사 로그만 남긴다).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FailureCause {
    #[default]
    OkPass,
    TransientNetwork,
    ToolError,
    MaxTurns,
    NoResult,
    ArtifactMissing,
    GateFail,
    Stall,
    Timeout,
    ConfigError,
    Interrupted,
}

pub const FAILURE_CAUSES: [FailureCause; 11] = [
    FailureCause::OkPass,
    FailureCause::TransientNetwork,
    FailureCause::ToolError,
    FailureCause::MaxTurns,
    FailureCause::NoResult,
    FailureCause::ArtifactMissing,
    FailureCause::GateFail,
    FailureCause::Stall,
    FailureCause::Timeout,
    FailureCause::ConfigError,
    FailureCause::Interrupted,
];

impl FailureCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            FailureCause::OkPass => "ok_pass",
            FailureCause::TransientNetwork => "transient_network",
            FailureCause::ToolError => "tool_error",
            FailureCause::MaxTurns => "max_turns",
            FailureCause::NoResult => "no_result",
            FailureCause::ArtifactMissing => "artifact_missing",
            FailureCause::GateFail => "gate_fail",
            FailureCause::Stall => "stall",
            FailureCause::Timeout => "timeout",
            FailureCause::ConfigError => "config_error",
            FailureCause::Interrupted => "interrupted",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ok_pass" => Some(FailureCause::OkPass),
            "transient_network" => Some(FailureCause::TransientNetwork),
            "tool_error" => Some(FailureCause::ToolError),
            "max_turns" => Some(FailureCause::MaxTurns),
            "no_result" => Some(FailureCause::NoResult),
            "artifact_missing" => Some(FailureCause::ArtifactMissing),
            "gate_fail" => Some(FailureCause::GateFail),
            "stall" => Some(FailureCause::Stall),
            "timeout" => Some(FailureCause::Timeout),
            "config_error" => Some(FailureCause::ConfigError),
            "interrupted" => Some(FailureCause::Interrupted),
            _ => None,
        }
    }
}

/// EWMA·카운터 오염이 금지된 원인 — decision_events 감사 로그만 남긴다
pub fn is_audit_only_cause(cause: FailureCause) -> bool {
    matches!(cause, FailureCause::ConfigError | FailureCause::Interrupted)
}

/// 재시도 허용 원인 — `transient_network`·`tool_error` 뿐 (§3.1-B 매트릭스)
pub fn is_retryable_cause(cause: FailureCause) -> bool {
    matches!(cause, FailureCause::TransientNetwork | FailureCause::ToolError)
}

/// 실패 원인 분류 입력 (§3.1-B) — 종료 신호 우선·문자열 휴리스틱 보조.
#[derive(Debug, Clone, Default)]
pub struct ClassifyFailureInput<'a> {
    pub ok: Option<bool>,
    pub exit_code: Option<i64>,
    pub signal: Option<&'a str>,
    pub stalled: bool,
    pub timed_out: bool,
    pub artifact_missing: bool,
    pub no_result: bool,
    pub gate_fail: bool,
    pub error_text: Option<&'a str>,
}

fn re_max_turns() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new("(?i)max[ -]?turns|reached max turns").unwrap())
}

fn re_config_error() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)ENOENT|command not found|not found as package|\b401\b|\b403\b|unauthorized|forbidden|authentication|permission denied|EACCES|unknown option|invalid option",
        )
        .unwrap()
    })
}

fn re_transient_network() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)ECONNRESET|ETIMEDOUT|ECONNREFUSED|EAI_AGAIN|ENETUNREACH|socket hangup|rate.?limit|\b429\b|\b502\b|\b503\b|\b504\b|network error",
        )
        .unwrap()
    })
}

/// 종료 신호를 우선·문자열 휴리스틱을 보조로 사용하는 실패 원인 분류기(§3.1-B).
pub fn classify_failure(input: &ClassifyFailureInput) -> FailureCause {
    if input.ok == Some(true) {
        return FailureCause::OkPass;
    }
    if input.stalled {
        return FailureCause::Stall;
    }
    if input.timed_out {
        return FailureCause::Timeout;
    }
    if input.artifact_missing {
        return FailureCause::ArtifactMissing;
    }
    if input.no_result {
        return FailureCause::NoResult;
    }
    if input.gate_fail {
        return FailureCause::GateFail;
    }
    if matches!(input.signal, Some("SIGINT") | Some("SIGTERM") | Some("SIGHUP")) {
        return FailureCause::Interrupted;
    }
    if input.exit_code == Some(127) || input.exit_code == Some(2) {
        return FailureCause::ConfigError;
    }
    let text = input.error_text.unwrap_or("");
    if re_max_turns().is_match(text) {
        return FailureCause::MaxTurns;
    }
    if re_config_error().is_match(text) {
        return FailureCause::ConfigError;
    }
    if re_transient_network().is_match(text) {
        return FailureCause::TransientNetwork;
    }
    if let Some(code) = input.exit_code {
        if code != 0 {
            return FailureCause::ToolError;
        }
    }
    if input.ok == Some(false) {
        return FailureCause::ToolError;
    }
    if input.exit_code == Some(0) {
        return FailureCause::OkPass;
    }
    FailureCause::ToolError
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// `error_snippet` 규약 생성: `cause != 'ok_pass'`일 때 `cause=<enum>; <snippet>`
/// 형태를 만들고 리댁션 후 300자로 절단한다(§3.1-B). ok_pass면 None.
pub fn format_error_snippet(cause: FailureCause, snippet: Option<&str>) -> Option<String> {
    if cause == FailureCause::OkPass {
        return None;
    }
    let body = match snippet {
        Some(s) => format!(" {}", redact_secrets(s)),
        None => String::new(),
    };
    Some(truncate_chars(
        &format!("cause={};{}", cause.as_str(), body),
        ERROR_SNIPPET_MAX_CHARS,
    ))
}

// ---------------------------------------------------------------------------
// 스키마 감지·메타데이터 규약
// ---------------------------------------------------------------------------

/// 커넥션별 스키마 감지 — 마이그레이션은 open_db 시점에 완료되므로 호출마다
/// PRAGMA 프로브로 확인한다(v1 raw 핸들 안전 분기).
#[derive(Debug, Clone, Copy)]
struct SchemaInfo {
    forks_v2: bool,
    intervals: bool,
    events_v2: bool,
}

fn schema_of(db: &Connection) -> SchemaInfo {
    SchemaInfo {
        forks_v2: has_column(db, "forks", "generation"),
        intervals: has_table(db, "fork_intervals"),
        events_v2: has_column(db, "decision_events", "cost_status"),
    }
}

fn parse_fork_metadata(raw: Option<&str>) -> Map<String, Value> {
    match raw {
        None | Some("") => Map::new(),
        Some(s) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        },
    }
}

fn meta_i64(meta: &Map<String, Value>, key: &str) -> Option<i64> {
    meta.get(key).and_then(|v| v.as_i64())
}

fn meta_str(meta: &Map<String, Value>, key: &str) -> Option<String> {
    meta.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

/// 사실상 "살아있는" fork 상태 — v2는 마이그레이션 백필로 `open`+`waiting_human`
/// 메타를 `waiting_human` status로 승격할 수 있어 동치로 취급한다.
pub fn is_fork_live_status(status: &str) -> bool {
    status == "open" || status == "waiting_human"
}

// ---------------------------------------------------------------------------
// ForkRow — SELECT * 열 이름 맵 기반 로딩(v1/v2 공통)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ForkRow {
    pub fork_id: String,
    pub project_id: String,
    pub project_label: String,
    pub conversation_id: Option<String>,
    pub route: String,
    pub tool_class: String,
    pub decision: String,
    pub forced_split: i64,
    pub p_at_gate: f64,
    pub basis_at_gate: String,
    /// v2 1급 컬럼 — v1 DB에서는 None(메타 폴백 경로가 정본)
    pub generation: Option<i64>,
    pub attempt: Option<i64>,
    pub lease_owner: Option<String>,
    pub lease_expires_ms: Option<i64>,
    pub status: String,
    pub closed_by: Option<String>,
    pub neon_verdict: Option<String>,
    pub ttl_ms: i64,
    pub created_at_ms: i64,
    pub last_touch_ms: i64,
    pub closed_at_ms: Option<i64>,
    pub metadata_json: Option<String>,
}

fn text_col(names: &[String], row: &Row<'_>, name: &str) -> Option<String> {
    let i = names.iter().position(|n| n == name)?;
    match row.get_ref(i).ok()? {
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        _ => None,
    }
}

fn i64_col(names: &[String], row: &Row<'_>, name: &str) -> Option<i64> {
    let i = names.iter().position(|n| n == name)?;
    match row.get_ref(i).ok()? {
        ValueRef::Integer(n) => Some(n),
        _ => None,
    }
}

fn f64_col(names: &[String], row: &Row<'_>, name: &str) -> Option<f64> {
    let i = names.iter().position(|n| n == name)?;
    match row.get_ref(i).ok()? {
        ValueRef::Real(v) => Some(v),
        ValueRef::Integer(v) => Some(v as f64),
        _ => None,
    }
}

fn fork_row_from(names: &[String], row: &Row<'_>) -> ForkRow {
    ForkRow {
        fork_id: text_col(names, row, "fork_id").unwrap_or_default(),
        project_id: text_col(names, row, "project_id").unwrap_or_default(),
        project_label: text_col(names, row, "project_label").unwrap_or_default(),
        conversation_id: text_col(names, row, "conversation_id"),
        route: text_col(names, row, "route").unwrap_or_default(),
        tool_class: text_col(names, row, "tool_class").unwrap_or_default(),
        decision: text_col(names, row, "decision").unwrap_or_default(),
        forced_split: i64_col(names, row, "forced_split").unwrap_or(0),
        p_at_gate: f64_col(names, row, "p_at_gate").unwrap_or(0.0),
        basis_at_gate: text_col(names, row, "basis_at_gate").unwrap_or_default(),
        generation: i64_col(names, row, "generation"),
        attempt: i64_col(names, row, "attempt"),
        lease_owner: text_col(names, row, "lease_owner"),
        lease_expires_ms: i64_col(names, row, "lease_expires_ms"),
        status: text_col(names, row, "status").unwrap_or_default(),
        closed_by: text_col(names, row, "closed_by"),
        neon_verdict: text_col(names, row, "neon_verdict"),
        ttl_ms: i64_col(names, row, "ttl_ms").unwrap_or(0),
        created_at_ms: i64_col(names, row, "created_at_ms").unwrap_or(0),
        last_touch_ms: i64_col(names, row, "last_touch_ms").unwrap_or(0),
        closed_at_ms: i64_col(names, row, "closed_at_ms"),
        metadata_json: text_col(names, row, "metadata_json"),
    }
}

/// generation/attempt 1급 컬럼 우선, v1은 metadata_json 폴백 — 컬럼·메타는 쓰기 시점에 동기화된다
fn gen_attempt_of(row: &ForkRow) -> (i64, i64) {
    let meta = parse_fork_metadata(row.metadata_json.as_deref());
    (
        row.generation.or_else(|| meta_i64(&meta, "gen")).unwrap_or(1),
        row.attempt.or_else(|| meta_i64(&meta, "attempt")).unwrap_or(1),
    )
}

fn query_fork(db: &Connection, fork_id: &str) -> Result<Option<ForkRow>, String> {
    let mut stmt = db
        .prepare("SELECT * FROM forks WHERE fork_id = ?1")
        .map_err(|e| e.to_string())?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query([fork_id]).map_err(|e| e.to_string())?;
    match rows.next().map_err(|e| e.to_string())? {
        Some(row) => Ok(Some(fork_row_from(&names, &row))),
        None => Ok(None),
    }
}

pub fn get_fork(db: &Connection, fork_id: &str) -> Result<Option<ForkRow>, String> {
    query_fork(db, fork_id)
}

/// fork의 현재 gen/attempt (컬럼 우선, v1 메타 폴백) — 행이 없으면 None
pub fn get_fork_gen_attempt(db: &Connection, fork_id: &str) -> Result<Option<(i64, i64)>, String> {
    Ok(query_fork(db, fork_id)?.map(|r| gen_attempt_of(&r)))
}

// ---------------------------------------------------------------------------
// fork_intervals (PB-1 §3.1~§3.2): exec·wait·human 단계 구간
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkIntervalPhase {
    Exec,
    Wait,
    Human,
}

impl ForkIntervalPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            ForkIntervalPhase::Exec => "exec",
            ForkIntervalPhase::Wait => "wait",
            ForkIntervalPhase::Human => "human",
        }
    }
}

pub struct ForkIntervalArgs<'a> {
    pub fork_id: &'a str,
    pub attempt: i64,
    pub phase: ForkIntervalPhase,
    pub now_ms: i64,
}

fn intervals_writable(db: &Connection, fork_id: &str) -> bool {
    // fk_fallback_*는 DB 연결 실패 합성 ID — 실제 forks 행이 없어 구간을 기록하지 않는다(§3.2)
    schema_of(db).intervals && !fork_id.starts_with("fk_fallback_")
}

/// 트랜잭션 내부 전용: 동일 (forkId, attempt)의 열린 구간을 먼저 닫고 새 구간을 연다.
/// 열린 구간 ≤1은 부분 유니크 인덱스가 강제하며, 여기서는 정합 순서를 보장한다.
fn open_fork_interval_in_tx(db: &Connection, args: &ForkIntervalArgs) -> Result<(), String> {
    if !intervals_writable(db, args.fork_id) {
        return Ok(());
    }
    db.execute(
        "UPDATE fork_intervals SET ended_at_ms = MAX(started_at_ms, ?1)
          WHERE fork_id = ?2 AND attempt = ?3 AND ended_at_ms IS NULL",
        rusqlite::params![args.now_ms, args.fork_id, args.attempt],
    )
    .map_err(|e| e.to_string())?;
    db.execute(
        "INSERT INTO fork_intervals (fork_id, attempt, phase, started_at_ms) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![args.fork_id, args.attempt, args.phase.as_str(), args.now_ms],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 트랜잭션 내부 전용: 해당 fork의 모든 열린 구간(attempt 무관)을 닫는다 — 종결·범프 공통
fn close_open_fork_intervals_in_tx(db: &Connection, fork_id: &str, now_ms: i64) -> Result<(), String> {
    if !intervals_writable(db, fork_id) {
        return Ok(());
    }
    db.execute(
        "UPDATE fork_intervals SET ended_at_ms = MAX(started_at_ms, ?1)
          WHERE fork_id = ?2 AND ended_at_ms IS NULL",
        rusqlite::params![now_ms, fork_id],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 자식 스폰 등 외부 시점의 구간 시작 래퍼 — 자체 BEGIN IMMEDIATE로 단독 커밋한다
pub fn record_fork_interval_start(db: &Connection, args: &ForkIntervalArgs) -> Result<(), String> {
    if !intervals_writable(db, args.fork_id) {
        return Ok(());
    }
    db.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    match open_fork_interval_in_tx(db, args) {
        Ok(()) => db.execute_batch("COMMIT").map_err(|e| e.to_string()),
        Err(err) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

/// 외부 호출용 구간 종료 래퍼 — fork의 모든 열린 구간을 닫는다
pub fn record_fork_interval_end(db: &Connection, fork_id: &str, now_ms: Option<i64>) -> Result<(), String> {
    if !intervals_writable(db, fork_id) {
        return Ok(());
    }
    let now = now_ms.unwrap_or_else(now_ms_default);
    db.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    match close_open_fork_intervals_in_tx(db, fork_id, now) {
        Ok(()) => db.execute_batch("COMMIT").map_err(|e| e.to_string()),
        Err(err) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

/// 열린 구간이 없으면 현재 attempt의 `wait` 구간을 멱등 보장한다
pub fn ensure_fork_wait_interval(db: &Connection, fork_id: &str, now_ms: Option<i64>) -> Result<(), String> {
    if !intervals_writable(db, fork_id) {
        return Ok(());
    }
    let ga = match get_fork_gen_attempt(db, fork_id)? {
        Some(ga) => ga,
        None => return Ok(()),
    };
    let open: Option<i64> = db
        .query_row(
            "SELECT 1 AS one FROM fork_intervals WHERE fork_id = ?1 AND attempt = ?2 AND ended_at_ms IS NULL LIMIT 1",
            rusqlite::params![fork_id, ga.1],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if open.is_some() {
        return Ok(());
    }
    record_fork_interval_start(
        db,
        &ForkIntervalArgs {
            fork_id,
            attempt: ga.1,
            phase: ForkIntervalPhase::Wait,
            now_ms: now_ms.unwrap_or_else(now_ms_default),
        },
    )
}

fn now_ms_default() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// p0 테이블·분류·사후확률 (§4.1~§4.3)
// ---------------------------------------------------------------------------

/// 사전확률 p0 테이블 (§4.3). 저사전 확신 키(shell_exec·delegate_swarm)는 콜드스타트
/// 1회 실패만으로 split 아래로 하강한다 — 의도된 보수 동작(N2).
pub fn prior_p0(route: RouteId, tool_class: &str) -> f64 {
    match (route.as_str(), tool_class) {
        ("file_select", "file_read") => 0.94,
        ("file_select", "file_edit") => 0.85,
        ("tool_select", "code_search") => 0.92,
        ("tool_select", "shell_exec") => 0.74,
        ("tool_select", _) => 0.88,
        ("retry_or_abort", _) => 0.85,
        ("delegate_swarm", _) => 0.80,
        _ => 0.88, // file_select 기타 — 미지 조합의 보수 기본값
    }
}

/// 도구명 → (route, tool_class) 결정론적 분류 (§4.1 — 실측 `agy` 도구명 기준).
/// `is_retry_context == true`면 `route=retry_or_abort`로 정규화한다(단일 행 귀속 — §4.5).
pub fn classify_tool(tool_name: &str, is_retry_context: bool) -> (RouteId, String) {
    let name = tool_name.trim();
    const FILE_READ: [&str; 7] = ["read_file", "view_file", "read", "list_dir", "find_by_name", "find", "ls"];
    const FILE_EDIT: [&str; 6] = [
        "edit_file",
        "write_file",
        "edit",
        "write",
        // 타 CLI 호환 별칭(실측 미존재 — 주석 표기, §4.1)
        "replace_file_content",
        "multi_replace_file_content",
    ];
    const CODE_SEARCH: [&str; 3] = ["grep_search", "grep", "search_web"];
    const SHELL_EXEC: [&str; 4] = ["run_command", "bash", "powershell", "exec"];
    const SWARM: [&str; 6] = ["invoke_subagent", "hc_run", "pi_explore", "devin_worker", "manage_subagents", "swarm"];

    let (route, tool_class) = if FILE_READ.contains(&name) {
        (RouteId::FileSelect, "file_read")
    } else if FILE_EDIT.contains(&name) {
        (RouteId::FileSelect, "file_edit")
    } else if CODE_SEARCH.contains(&name) {
        (RouteId::ToolSelect, "code_search")
    } else if SHELL_EXEC.contains(&name) {
        (RouteId::ToolSelect, "shell_exec")
    } else if SWARM.contains(&name) {
        (RouteId::DelegateSwarm, "swarm_dispatch")
    } else {
        // schedule·manage_task·ask_question·enter/exit_plan_mode 및 기타 전부 (§4.1)
        (RouteId::ToolSelect, "other")
    };
    if is_retry_context {
        (RouteId::RetryOrAbort, tool_class.to_string())
    } else {
        (route, tool_class.to_string())
    }
}

/// §2.0 task_kind 7종 결정표.
/// 주의: `static_check`은 읽기전용 pi(--tools read,grep,find,ls, bash 없음)가 수행할 수
/// 없으므로 sharp_eligible=false다. sharp 자격은 `explore_read`, `code_search` 2종뿐이다.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskKindSpec {
    pub task_kind: TaskKind,
    pub route: RouteId,
    pub tool_class: &'static str,
    pub requires_write: bool,
    pub default_risk: RiskLevel,
    pub sharp_eligible: bool,
}

const TASK_KIND_TABLE: [TaskKindSpec; 7] = [
    TaskKindSpec { task_kind: TaskKind::ExploreRead, route: RouteId::FileSelect, tool_class: "file_read", requires_write: false, default_risk: RiskLevel::Low, sharp_eligible: true },
    TaskKindSpec { task_kind: TaskKind::CodeSearch, route: RouteId::ToolSelect, tool_class: "code_search", requires_write: false, default_risk: RiskLevel::Low, sharp_eligible: true },
    TaskKindSpec { task_kind: TaskKind::StaticCheck, route: RouteId::ToolSelect, tool_class: "shell_exec", requires_write: false, default_risk: RiskLevel::Low, sharp_eligible: false },
    TaskKindSpec { task_kind: TaskKind::SingleEdit, route: RouteId::FileSelect, tool_class: "file_edit", requires_write: true, default_risk: RiskLevel::Medium, sharp_eligible: false },
    TaskKindSpec { task_kind: TaskKind::MultiEdit, route: RouteId::FileSelect, tool_class: "file_edit", requires_write: true, default_risk: RiskLevel::High, sharp_eligible: false },
    TaskKindSpec { task_kind: TaskKind::TestOrDeploy, route: RouteId::ToolSelect, tool_class: "shell_exec", requires_write: true, default_risk: RiskLevel::High, sharp_eligible: false },
    TaskKindSpec { task_kind: TaskKind::SwarmFanout, route: RouteId::DelegateSwarm, tool_class: "swarm_dispatch", requires_write: false, default_risk: RiskLevel::Medium, sharp_eligible: false },
];

pub fn task_kind_spec(kind: &str) -> Option<&'static TaskKindSpec> {
    TASK_KIND_TABLE.iter().find(|s| s.task_kind.as_str() == kind)
}

/// risk_level 상향만 허용 (low < medium < high — §2.0)
pub fn escalate_risk(base: RiskLevel, override_: Option<RiskLevel>) -> RiskLevel {
    match override_ {
        Some(o) if o.rank() > base.rank() => o,
        _ => base,
    }
}

/// 자기 호출 제외(B6): 게이트 활동(dg·dg-hook 호출)이 shell_exec 통계를 재먹이는
/// 피드백 루프를 차단한다. 명령문에서 토큰을 찾는다.
pub fn is_self_invocation(text: &str) -> bool {
    text.contains("src/decision/dg.ts") || text.contains("dg-hook.ts")
}

#[derive(Debug, Clone, PartialEq)]
struct StatsRow {
    ewma_success: f64,
    ewma_fail: f64,
    raw_samples: i64,
    consecutive_errors: i64,
}

fn read_stats_row(db: &Connection, project_id: &str, route: RouteId, tool_class: &str) -> Result<Option<StatsRow>, String> {
    let row = db
        .query_row(
            "SELECT ewma_success, ewma_fail, raw_samples, consecutive_errors
             FROM stats_summary WHERE project_id = ?1 AND route = ?2 AND tool_class = ?3",
            rusqlite::params![project_id, route.as_str(), tool_class],
            |r| {
                Ok(StatsRow {
                    ewma_success: r.get(0)?,
                    ewma_fail: r.get(1)?,
                    raw_samples: r.get(2)?,
                    consecutive_errors: r.get(3)?,
                })
            },
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(row)
}

#[derive(Debug, Clone, PartialEq)]
pub struct PosteriorResult {
    pub p: f64,
    pub basis: DecisionBasis,
    pub samples: i64,
    pub consecutive_errors: i64,
    pub lambda: f64,
    pub p_hier: f64,
    pub ewma_success: f64,
    pub ewma_fail: f64,
}

/// 계층적 Shrinkage + 감쇠 Beta 사후평균 (§4.2~§4.3).
/// p_glob는 전역 풀(__GLOBAL__) EWMA 합성에 M0·p0 사전을 결합하고,
/// p_hier = (1-λ)p0 + λ·p_glob (λ ≤ 0.25) 후 α_prior = M0·p_hier를 사전으로 같은
/// Beta 사후평균 식을 n<5 구간에도 연속 적용한다(점프 0 — basis 표지로만 구분).
pub fn get_posterior(
    db: &Connection,
    project_id: &str,
    route: RouteId,
    tool_class: &str,
) -> Result<PosteriorResult, String> {
    let p0 = prior_p0(route, tool_class);
    let local = read_stats_row(db, project_id, route, tool_class)?;
    let glob = read_stats_row(db, GLOBAL_PROJECT_ID, route, tool_class)?;
    let s = local.as_ref().map(|r| r.ewma_success).unwrap_or(0.0);
    let f = local.as_ref().map(|r| r.ewma_fail).unwrap_or(0.0);
    let samples = local.as_ref().map(|r| r.raw_samples).unwrap_or(0);
    let sg = glob.as_ref().map(|r| r.ewma_success).unwrap_or(0.0);
    let fg = glob.as_ref().map(|r| r.ewma_fail).unwrap_or(0.0);

    let lambda = ((sg + fg) / (sg + fg + 20.0)).min(LAMBDA_MAX);
    let alpha0 = M0 * p0;
    let beta0 = M0 - alpha0;
    let p_glob = (alpha0 + sg) / (alpha0 + beta0 + sg + fg);
    let p_hier = (1.0 - lambda) * p0 + lambda * p_glob;
    let alpha_prior = M0 * p_hier;
    let beta_prior = M0 - alpha_prior;
    let p = (alpha_prior + s) / (alpha_prior + beta_prior + s + f);
    Ok(PosteriorResult {
        p,
        basis: if samples >= N_MIN { DecisionBasis::Measured } else { DecisionBasis::Prior },
        samples,
        consecutive_errors: local.as_ref().map(|r| r.consecutive_errors).unwrap_or(0),
        lambda,
        p_hier,
        ewma_success: s,
        ewma_fail: f,
    })
}

pub fn get_session_consec_err(db: &Connection, conversation_id: Option<&str>) -> Result<i64, String> {
    let Some(cid) = conversation_id else { return Ok(0) };
    let n: Option<i64> = db
        .query_row(
            "SELECT session_consec_err FROM session_cursor WHERE conversation_id = ?1",
            [cid],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(n.unwrap_or(0))
}

pub fn has_hook_coverage(db: &Connection, conversation_id: &str) -> Result<bool, String> {
    let n: Option<i64> = db
        .query_row(
            "SELECT has_hook_coverage FROM session_cursor WHERE conversation_id = ?1",
            [conversation_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(n == Some(1))
}

// ---------------------------------------------------------------------------
// 표본 1건 기록 (B7 불변식)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct RecordInput<'a> {
    pub project_id: &'a str,
    pub project_label: &'a str,
    pub route: RouteId,
    pub tool_class: &'a str,
    pub tool_name: &'a str,
    pub ok: bool,
    pub weight: Option<f64>,
    pub is_retry: Option<bool>,
    pub conversation_id: Option<&'a str>,
    pub step: Option<i64>,
    pub target_hash: Option<&'a str>,
    pub error_snippet: Option<&'a str>,
    pub source: Option<EventSource>,
    pub dedupe_key: Option<&'a str>,
    pub fork_id: Option<&'a str>,
    pub command_line: Option<&'a str>,
    /// 실패 원인 분류 (§3.1-B) — config_error/interrupted는 EWMA·카운터 격리
    pub cause: Option<FailureCause>,
    /// v2 토큰·비용 컬럼(컬럼 존재 시에만 저장 — PB-1 §3.3)
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub cost_status: Option<CostStatus>,
    pub now_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordResult {
    pub recorded: bool,
    pub reason: Option<&'static str>,
    pub is_retry: bool,
    pub route: RouteId,
    pub p_after: f64,
    pub consecutive_errors: i64,
    pub session_consec_err: i64,
}

fn update_stats_in_tx(
    db: &Connection,
    project_id: &str,
    project_label: &str,
    route: RouteId,
    tool_class: &str,
    ok: bool,
    weight: f64,
    target_hash: Option<&str>,
    now_ms_value: i64,
) -> Result<(), String> {
    for (pid, label) in [(project_id, project_label), (GLOBAL_PROJECT_ID, "global-pool")] {
        db.execute(
            "INSERT INTO stats_summary
               (project_id, project_label, route, tool_class, ewma_success, ewma_fail,
                raw_samples, consecutive_errors, last_target_hash, last_outcome, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, ?9, ?10)
             ON CONFLICT(project_id, route, tool_class) DO UPDATE SET
               ewma_success = ewma_success * ?11 + ?12,
               ewma_fail = ewma_fail * ?13 + ?14,
               raw_samples = raw_samples + 1,
               consecutive_errors = CASE WHEN ?15 = 1 THEN 0 ELSE consecutive_errors + 1 END,
               last_target_hash = ?16,
               last_outcome = ?17,
               updated_at_ms = ?18",
            rusqlite::params![
                pid,
                label,
                route.as_str(),
                tool_class,
                if ok { weight } else { 0.0 },
                if ok { 0.0 } else { weight },
                if ok { 0 } else { 1 },
                target_hash,
                if ok { 1 } else { 0 },
                now_ms_value,
                GAMMA,
                if ok { weight } else { 0.0 },
                GAMMA,
                if ok { 0.0 } else { weight },
                if ok { 1 } else { 0 },
                target_hash,
                if ok { 1 } else { 0 },
                now_ms_value,
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// decision_events 공통 삽입 — v2 컬럼(cause·토큰·비용)이 존재하면 함께 저장하고,
/// v1 DB에는 v1 14컬럼 INSERT만 사용한다. 반환은 `changes`(dedupe 멱등성 검사용).
#[allow(clippy::too_many_arguments)]
fn insert_event_row(db: &Connection, e: &EventRowIn) -> Result<usize, String> {
    let v2 = schema_of(db).events_v2;
    let ok_i = if e.ok { 1 } else { 0 };
    let retry_i = if e.is_retry { 1 } else { 0 };
    if v2 {
        let changes = db
            .execute(
                "INSERT OR IGNORE INTO decision_events (dedupe_key, fork_id, project_id, conversation_id, source, route, tool_name, tool_class, ok, weight, is_retry, target_hash, error_snippet, created_at_ms, cause, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd, cost_status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
                rusqlite::params![
                    e.dedupe_key,
                    e.fork_id,
                    e.project_id,
                    e.conversation_id,
                    e.source.as_str(),
                    e.route,
                    e.tool_name,
                    e.tool_class,
                    ok_i,
                    e.weight,
                    retry_i,
                    e.target_hash,
                    e.error_snippet,
                    e.now_ms,
                    e.cause.map(|c| c.as_str()),
                    e.input_tokens,
                    e.output_tokens,
                    e.cache_read_tokens,
                    e.cache_write_tokens,
                    e.cost_usd,
                    e.cost_status.unwrap_or(CostStatus::Unknown).as_str(),
                ],
            )
            .map_err(|er| er.to_string())?;
        Ok(changes)
    } else {
        let changes = db
            .execute(
                "INSERT OR IGNORE INTO decision_events (dedupe_key, fork_id, project_id, conversation_id, source, route, tool_name, tool_class, ok, weight, is_retry, target_hash, error_snippet, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                rusqlite::params![
                    e.dedupe_key,
                    e.fork_id,
                    e.project_id,
                    e.conversation_id,
                    e.source.as_str(),
                    e.route,
                    e.tool_name,
                    e.tool_class,
                    ok_i,
                    e.weight,
                    retry_i,
                    e.target_hash,
                    e.error_snippet,
                    e.now_ms,
                ],
            )
            .map_err(|er| er.to_string())?;
        Ok(changes)
    }
}

struct EventRowIn<'a> {
    dedupe_key: Option<&'a str>,
    fork_id: Option<&'a str>,
    project_id: &'a str,
    conversation_id: Option<&'a str>,
    source: EventSource,
    route: &'a str,
    tool_name: &'a str,
    tool_class: &'a str,
    ok: bool,
    weight: f64,
    is_retry: bool,
    target_hash: Option<&'a str>,
    error_snippet: Option<&'a str>,
    cause: Option<FailureCause>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_read_tokens: Option<i64>,
    cache_write_tokens: Option<i64>,
    cost_usd: Option<f64>,
    cost_status: Option<CostStatus>,
    now_ms: i64,
}

fn update_cursor_in_tx(db: &Connection, input: &RecordInput, _is_retry: bool, now_ms_value: i64) -> Result<(), String> {
    let Some(cid) = input.conversation_id else { return Ok(()) };
    if input.ok {
        // N7: 대화 스코프 카운터는 동일 대화의 ok=1로만 리셋, 직전 실패 문맥도 해제
        db.execute(
            "INSERT INTO session_cursor
               (conversation_id, project_id, has_hook_coverage, last_failed_tool, last_failed_target,
                last_failed_step, session_consec_err, updated_at_ms)
             VALUES (?1, ?2, 0, NULL, NULL, NULL, 0, ?3)
             ON CONFLICT(conversation_id) DO UPDATE SET
               last_failed_tool = NULL,
               last_failed_target = NULL,
               last_failed_step = NULL,
               session_consec_err = 0,
               updated_at_ms = ?4",
            rusqlite::params![cid, input.project_id, now_ms_value, now_ms_value],
        )
        .map_err(|e| e.to_string())?;
    } else {
        db.execute(
            "INSERT INTO session_cursor
               (conversation_id, project_id, has_hook_coverage, last_failed_tool, last_failed_target,
                last_failed_step, session_consec_err, updated_at_ms)
             VALUES (?1, ?2, 0, ?3, ?4, ?5, 1, ?6)
             ON CONFLICT(conversation_id) DO UPDATE SET
               last_failed_tool = ?3,
               last_failed_target = ?4,
               last_failed_step = COALESCE(?5, last_failed_step),
               session_consec_err = session_consec_err + 1,
               updated_at_ms = ?7",
            rusqlite::params![
                cid,
                input.project_id,
                input.tool_class,
                input.target_hash,
                input.step,
                now_ms_value,
                now_ms_value,
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 표본 1건 기록 (B7 불변식 적용).
/// - 자기 호출(dg.ts/dg-hook.ts 포함 명령)은 표본 0건 skip(B6).
/// - 재시도 판정(§4.5): 명시 `is_retry` 또는 직전 실패 이후 RETRY_STEP_WINDOW 스텝
///   이내 동일 tool_class/target_hash 재호출 → `route=retry_or_abort` 단일 행
///   (is_retry=1), 원 라우트에는 기록하지 않는다.
pub fn record_tool_outcome(db: &Connection, input: &RecordInput) -> Result<RecordResult, String> {
    let now_ms_value = input.now_ms.unwrap_or_else(now_ms_default);
    // PR-A1 최종 방어선: DB 입력부 리댁션은 절단 이전에 강제 적용한다 (§4.1 Layer C).
    let cause = input.cause.unwrap_or(FailureCause::OkPass);
    let error_snippet: Option<String> = if cause != FailureCause::OkPass {
        format_error_snippet(cause, input.error_snippet).or_else(|| Some(format!("cause={};", cause.as_str())))
    } else {
        input.error_snippet.map(|s| truncate_chars(&redact_secrets(s), ERROR_SNIPPET_MAX_CHARS))
    };
    // §3.1-B EWMA 격리: config_error/interrupted 실패는 감사 로그만 남긴다
    let audit_only = !input.ok && is_audit_only_cause(cause);
    let self_text = format!("{} {}", input.tool_name, input.command_line.unwrap_or(""));
    if is_self_invocation(&self_text) {
        let post = get_posterior(db, input.project_id, input.route, input.tool_class)?;
        return Ok(RecordResult {
            recorded: false,
            reason: Some("self-invocation"),
            is_retry: false,
            route: input.route,
            p_after: post.p,
            consecutive_errors: post.consecutive_errors,
            session_consec_err: get_session_consec_err(db, input.conversation_id)?,
        });
    }

    let mut is_retry = input.is_retry == Some(true);
    if !is_retry {
        if let (Some(cid), Some(step)) = (input.conversation_id, input.step) {
            let cur: Option<(Option<String>, Option<String>, Option<i64>)> = db
                .query_row(
                    "SELECT last_failed_tool, last_failed_target, last_failed_step FROM session_cursor WHERE conversation_id = ?1",
                    [cid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            if let Some((last_tool, last_target, last_step)) = cur {
                if let Some(last_failed_step) = last_step {
                    if step > last_failed_step {
                        let within = step - last_failed_step <= RETRY_STEP_WINDOW;
                        let same_class = last_tool.as_deref() == Some(input.tool_class);
                        let same_target = match (input.target_hash, last_target.as_deref()) {
                            (Some(t), Some(l)) => t == l,
                            _ => false,
                        };
                        if within && (same_class || same_target) {
                            is_retry = true;
                        }
                    }
                }
            }
        }
    }

    let route = if is_retry { RouteId::RetryOrAbort } else { input.route };
    let tool_class = input.tool_class; // 재시도행도 정규화 클래스 유지(§4.5 단일 행 귀속)
    let weight = input.weight.unwrap_or(1.0);

    db.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    let work = || -> Result<Option<&'static str>, String> {
        if let Some(dk) = input.dedupe_key {
            let existing: Option<i64> = db
                .query_row("SELECT 1 AS one FROM decision_events WHERE dedupe_key = ?1", [dk], |r| r.get(0))
                .optional()
                .map_err(|e| e.to_string())?;
            if existing.is_some() {
                return Ok(Some("duplicate"));
            }
        }
        if !audit_only {
            update_stats_in_tx(db, input.project_id, input.project_label, route, tool_class, input.ok, weight, input.target_hash, now_ms_value)?;
        }
        insert_event_row(
            db,
            &EventRowIn {
                dedupe_key: input.dedupe_key,
                fork_id: input.fork_id,
                project_id: input.project_id,
                conversation_id: input.conversation_id,
                source: input.source.unwrap_or(EventSource::Cli),
                route: route.as_str(),
                tool_name: input.tool_name,
                tool_class,
                ok: input.ok,
                weight,
                is_retry,
                target_hash: input.target_hash,
                error_snippet: error_snippet.as_deref(),
                cause: input.cause,
                input_tokens: input.input_tokens,
                output_tokens: input.output_tokens,
                cache_read_tokens: input.cache_read_tokens,
                cache_write_tokens: input.cache_write_tokens,
                cost_usd: input.cost_usd,
                cost_status: input.cost_status,
                now_ms: now_ms_value,
            },
        )?;
        if !audit_only {
            update_cursor_in_tx(db, input, is_retry, now_ms_value)?;
        }
        Ok(None)
    };
    let outcome = work();
    match &outcome {
        Ok(Some("duplicate")) => {
            let _ = db.execute_batch("ROLLBACK");
            let post = get_posterior(db, input.project_id, route, tool_class)?;
            return Ok(RecordResult {
                recorded: false,
                reason: Some("duplicate"),
                is_retry,
                route,
                p_after: post.p,
                consecutive_errors: post.consecutive_errors,
                session_consec_err: get_session_consec_err(db, input.conversation_id)?,
            });
        }
        Ok(_) => db.execute_batch("COMMIT").map_err(|e| e.to_string())?,
        Err(_) => {
            let _ = db.execute_batch("ROLLBACK");
        }
    }
    outcome?;

    let post = get_posterior(db, input.project_id, route, tool_class)?;
    Ok(RecordResult {
        recorded: true,
        reason: None,
        is_retry,
        route,
        p_after: post.p,
        consecutive_errors: post.consecutive_errors,
        session_consec_err: get_session_consec_err(db, input.conversation_id)?,
    })
}

// ---------------------------------------------------------------------------
// 강제 split·판정 (§4.4, PB-2A 동적 θ_sharp)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ForcedState {
    pub forced_split: bool,
    pub forced_reason: Option<&'static str>,
    pub escalate_carbon: bool,
    pub key_consecutive_errors: i64,
    pub session_consec_err: i64,
}

/// §4.4: 키 카운터 OR 대화 카운터 >= 2 → p와 무관하게 강제 split(N7: 판정은 OR, 출력은 분리)
pub fn evaluate_forced(
    db: &Connection,
    project_id: &str,
    route: RouteId,
    tool_class: &str,
    conversation_id: Option<&str>,
) -> Result<ForcedState, String> {
    let key_err = read_stats_row(db, project_id, route, tool_class)?
        .map(|r| r.consecutive_errors)
        .unwrap_or(0);
    let sess_err = match conversation_id {
        Some(cid) => get_session_consec_err(db, Some(cid))?,
        None => 0,
    };
    let forced_split = key_err >= FORCED_SPLIT_ERRORS || sess_err >= FORCED_SPLIT_ERRORS;
    let forced_reason = if key_err >= FORCED_SPLIT_ERRORS {
        Some("consecutive_errors>=2")
    } else if sess_err >= FORCED_SPLIT_ERRORS {
        Some("session_consec_err>=2")
    } else {
        None
    };
    Ok(ForcedState {
        forced_split,
        forced_reason,
        escalate_carbon: forced_split,
        key_consecutive_errors: key_err,
        session_consec_err: sess_err,
    })
}

#[derive(Debug, Clone)]
pub struct GateEvalInput<'a> {
    pub project_id: &'a str,
    pub route: RouteId,
    pub tool_class: &'a str,
    pub conversation_id: Option<&'a str>,
    /// 비용 인식 동적 임계치. 미지정 시 SHARP_P_THRESHOLD(0.70)
    pub sharp_threshold: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GateEvalResult {
    pub decision: &'static str,
    pub p: f64,
    pub basis: DecisionBasis,
    pub samples: i64,
    pub consecutive_errors: i64,
    pub forced_split: bool,
    pub forced_reason: Option<&'static str>,
    pub escalate_carbon: bool,
    pub sharp_threshold: f64,
    pub post: PosteriorResult,
    pub forced: ForcedState,
}

/// `dg gate` 판정 — forcedSplit 우선, 아니면 `p >= θ_sharp`.
/// `sharp_threshold` 미지정 시 기존 상수 경로와 동일하다.
pub fn evaluate_gate(db: &Connection, input: &GateEvalInput) -> Result<GateEvalResult, String> {
    let theta = input.sharp_threshold.unwrap_or(SHARP_P_THRESHOLD);
    let post = get_posterior(db, input.project_id, input.route, input.tool_class)?;
    let forced = evaluate_forced(db, input.project_id, input.route, input.tool_class, input.conversation_id)?;
    let decision = if forced.forced_split {
        "split"
    } else if post.p >= theta {
        "sharp"
    } else {
        "split"
    };
    Ok(GateEvalResult {
        decision,
        p: post.p,
        basis: post.basis,
        samples: post.samples,
        consecutive_errors: forced.key_consecutive_errors,
        forced_split: forced.forced_split,
        forced_reason: forced.forced_reason,
        escalate_carbon: forced.escalate_carbon,
        sharp_threshold: theta,
        post,
        forced,
    })
}

#[derive(Debug, Clone)]
pub struct RouteEvalInput<'a> {
    pub project_id: &'a str,
    pub task_kind: &'a str,
    pub risk: Option<RiskLevel>,
    pub conversation_id: Option<&'a str>,
    /// 비용 인식 동적 임계치. 미지정 시 SHARP_P_THRESHOLD(0.70)
    pub sharp_threshold: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RouteEvalResult {
    pub spec: &'static TaskKindSpec,
    pub risk: RiskLevel,
    pub decision: &'static str,
    pub p: f64,
    pub basis: DecisionBasis,
    pub samples: i64,
    pub consecutive_errors: i64,
    pub forced_split: bool,
    pub forced_reason: Option<&'static str>,
    pub escalate_carbon: bool,
    pub sharp_threshold: f64,
    pub post: PosteriorResult,
    pub forced: ForcedState,
}

/// `dg route` 판정 — §2.0 sharp 패스트패스 4조건(sharp_eligible·!requires_write·
/// risk == low·consecutive_errors < 2) + `p >= θ_sharp`. 부정한 taskKind면 None.
pub fn evaluate_route(db: &Connection, input: &RouteEvalInput) -> Result<Option<RouteEvalResult>, String> {
    let Some(spec) = task_kind_spec(input.task_kind) else { return Ok(None) };
    let theta = input.sharp_threshold.unwrap_or(SHARP_P_THRESHOLD);
    let risk = escalate_risk(spec.default_risk, input.risk);
    let post = get_posterior(db, input.project_id, spec.route, spec.tool_class)?;
    let forced = evaluate_forced(db, input.project_id, spec.route, spec.tool_class, input.conversation_id)?;
    let decision = if forced.forced_split {
        "split"
    } else if spec.sharp_eligible
        && !spec.requires_write
        && risk == RiskLevel::Low
        && post.p >= theta
        && post.consecutive_errors < FORCED_SPLIT_ERRORS
    {
        "sharp"
    } else {
        "split"
    };
    Ok(Some(RouteEvalResult {
        spec,
        risk,
        decision,
        p: post.p,
        basis: post.basis,
        samples: post.samples,
        consecutive_errors: forced.key_consecutive_errors,
        forced_split: forced.forced_split,
        forced_reason: forced.forced_reason,
        escalate_carbon: forced.escalate_carbon,
        sharp_threshold: theta,
        post,
        forced,
    }))
}

// ---------------------------------------------------------------------------
// Fork 생명주기 (§4.7, PB-1)
// ---------------------------------------------------------------------------

pub struct IssueForkInput<'a> {
    pub project_id: &'a str,
    pub project_label: &'a str,
    pub route: RouteId,
    pub tool_class: &'a str,
    pub decision: &'static str,
    pub p_at_gate: f64,
    pub basis_at_gate: DecisionBasis,
    pub forced_split: bool,
    pub conversation_id: Option<&'a str>,
    pub ttl_ms: Option<i64>,
    pub title: Option<&'a str>,
    /// metadata_json에 병합할 추가 규약 필드 (repo_scope·lease_* 등 — §2.2·§2.4)
    pub metadata: Option<&'a Map<String, Value>>,
    pub now_ms: Option<i64>,
}

fn random_fork_suffix() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 6];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Fork 발행(§4.7 + PB-1): `fk_<12hex>` ID로 `status='open'`, `gen=1, attempt=1`
/// 메타 초기화. v2는 generation/attempt 1급 컬럼과 리스 쌍(owner+expires 동시)을
/// 함께 쓰고, fork 생성과 같은 트랜잭션에서 `wait` 구간을 연다.
pub fn issue_fork(db: &Connection, input: &IssueForkInput) -> Result<String, String> {
    let fork_id = format!("fk_{}", random_fork_suffix());
    let now_ms_value = input.now_ms.unwrap_or_else(now_ms_default);
    let mut metadata_obj = Map::new();
    metadata_obj.insert("gen".into(), Value::from(1));
    metadata_obj.insert("attempt".into(), Value::from(1));
    if let Some(extra) = input.metadata {
        for (k, v) in extra {
            metadata_obj.insert(k.clone(), v.clone());
        }
    }
    if let Some(title) = input.title {
        metadata_obj.insert("title".into(), Value::String(redact_secrets(title)));
    }
    let lease_owner = metadata_obj
        .get("lease_owner")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let lease_until = metadata_obj
        .get("lease_until_ms")
        .and_then(|v| v.as_f64())
        .filter(|f| f.is_finite())
        .map(|f| f.floor() as i64);
    // 리스는 쌍 단위 — 절반만 있으면 CHECK 위반 방지를 위해 둘 다 NULL
    let lease_ok = lease_owner.is_some() && lease_until.is_some();
    let metadata = Value::Object(metadata_obj).to_string();
    let v2 = schema_of(db).forks_v2;

    db.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    let work = || -> Result<(), String> {
        if v2 {
            db.execute(
                "INSERT INTO forks
                   (fork_id, project_id, project_label, conversation_id, route, tool_class, decision,
                    forced_split, p_at_gate, basis_at_gate, generation, attempt, lease_owner, lease_expires_ms,
                    status, ttl_ms, created_at_ms, last_touch_ms, metadata_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, 1, ?11, ?12, 'open', ?13, ?14, ?15, ?16)",
                rusqlite::params![
                    fork_id,
                    input.project_id,
                    input.project_label,
                    input.conversation_id,
                    input.route.as_str(),
                    input.tool_class,
                    input.decision,
                    if input.forced_split { 1 } else { 0 },
                    input.p_at_gate,
                    input.basis_at_gate.as_str(),
                    if lease_ok { lease_owner.as_deref() } else { None },
                    if lease_ok { lease_until } else { None },
                    input.ttl_ms.unwrap_or(TTL_EXPLORE_MS),
                    now_ms_value,
                    now_ms_value,
                    metadata,
                ],
            )
            .map_err(|e| e.to_string())?;
        } else {
            db.execute(
                "INSERT INTO forks
                   (fork_id, project_id, project_label, conversation_id, route, tool_class, decision,
                    forced_split, p_at_gate, basis_at_gate, status, ttl_ms, created_at_ms, last_touch_ms, metadata_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'open', ?11, ?12, ?13, ?14)",
                rusqlite::params![
                    fork_id,
                    input.project_id,
                    input.project_label,
                    input.conversation_id,
                    input.route.as_str(),
                    input.tool_class,
                    input.decision,
                    if input.forced_split { 1 } else { 0 },
                    input.p_at_gate,
                    input.basis_at_gate.as_str(),
                    input.ttl_ms.unwrap_or(TTL_EXPLORE_MS),
                    now_ms_value,
                    now_ms_value,
                    metadata,
                ],
            )
            .map_err(|e| e.to_string())?;
        }
        open_fork_interval_in_tx(
            db,
            &ForkIntervalArgs {
                fork_id: &fork_id,
                attempt: 1,
                phase: ForkIntervalPhase::Wait,
                now_ms: now_ms_value,
            },
        )
    };
    match work() {
        Ok(()) => db.execute_batch("COMMIT").map_err(|e| e.to_string())?,
        Err(err) => {
            let _ = db.execute_batch("ROLLBACK");
            return Err(err);
        }
    }
    Ok(fork_id)
}

/// 진행 갱신(B1): `last_touch_ms` 갱신 + 선택적 TTL 조정
pub fn touch_fork(
    db: &Connection,
    fork_id: &str,
    ttl_ms: Option<i64>,
    now_ms_value: i64,
) -> Result<(String, i64, i64), String> {
    let fork = query_fork(db, fork_id)?
        .ok_or_else(|| format!("fork not found: {}", fork_id))?;
    if !is_fork_live_status(&fork.status) {
        return Err(format!("fork {} not open (status={})", fork_id, fork.status));
    }
    let new_ttl = ttl_ms.unwrap_or(fork.ttl_ms);
    db.execute(
        "UPDATE forks SET last_touch_ms = ?1, ttl_ms = ?2 WHERE fork_id = ?3",
        rusqlite::params![now_ms_value, new_ttl, fork_id],
    )
    .map_err(|e| e.to_string())?;
    Ok((fork_id.to_string(), now_ms_value, new_ttl))
}

pub type CloseOutcome = &'static str; // 'ok' | 'fail' | 'rerouted'

#[derive(Debug, Clone, PartialEq)]
pub struct CloseForkResult {
    pub fork_id: String,
    pub status: String,
    pub closed_by: String,
    pub recorded: bool,
    pub p_after: f64,
    /// false면 펜스·상태 불일치로 롤백됨 — 표본 미기록 (§2.3 Zero-TOCTOU)
    pub committed: bool,
    pub reason: Option<&'static str>,
}

#[derive(Debug, Clone, Default)]
pub struct CloseForkOptions {
    pub now_ms: Option<i64>,
    pub record_stats: Option<bool>,
    /// Generation 펜스 — 지정 시 metadata.gen 불일치면 전면 거부 (§2.3-2)
    pub expected_gen: Option<i64>,
    /// Attempt 펜스 — fail/rerouted는 attempt 불일치 시 거부, ok는 gen만 검증
    pub expected_attempt: Option<i64>,
    /// metadata_json.cause 규약 필드
    pub cause: Option<FailureCause>,
}

fn get_project_label(db: &Connection, project_id: &str) -> Result<String, String> {
    let label: Option<String> = db
        .query_row(
            "SELECT project_label FROM stats_summary WHERE project_id = ?1 LIMIT 1",
            [project_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(label.unwrap_or_else(|| project_id.to_string()))
}

/// Fork 닫힘(§3.3 전이표). `ok`/`fail`은 EWMA 1건(source='cli') 반영,
/// `rerouted`는 통계 미반영·감사 기록 없음(§2.0 에스컬레이션).
/// BEGIN IMMEDIATE 내부 재조회 + gen/attempt 비대칭 펜스 + `changes == 1`
/// 가드로 Zero-TOCTOU를 보장한다. 거부 시 `{ committed: false, reason }` 반환.
pub fn close_fork(
    db: &Connection,
    fork_id: &str,
    outcome: CloseOutcome,
    closed_by: &str,
    reason: Option<&str>,
    opts: &CloseForkOptions,
) -> Result<CloseForkResult, String> {
    let pre = query_fork(db, fork_id)?
        .ok_or_else(|| format!("fork not found: {}", fork_id))?;
    if !is_fork_live_status(&pre.status) {
        return Err(format!("fork {} already closed (status={})", fork_id, pre.status));
    }
    let now_ms_value = opts.now_ms.unwrap_or_else(now_ms_default);
    let record_stats = opts.record_stats != Some(false);

    db.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    let work = || -> Result<CloseForkResult, String> {
        let cur = match query_fork(db, fork_id)? {
            Some(c) => c,
            None => {
                return Ok(CloseForkResult {
                    fork_id: fork_id.to_string(),
                    status: "unknown".into(),
                    closed_by: closed_by.to_string(),
                    recorded: false,
                    p_after: get_posterior(db, &pre.project_id, RouteId::parse(&pre.route).unwrap_or(RouteId::ToolSelect), &pre.tool_class)?.p,
                    committed: false,
                    reason: Some("stale_or_closed"),
                })
            }
        };
        let route = RouteId::parse(&cur.route).unwrap_or(RouteId::ToolSelect);
        let p_after = || get_posterior(db, &pre.project_id, route, &pre.tool_class).map(|p| p.p).unwrap_or(0.0);
        if !is_fork_live_status(&cur.status) {
            return Ok(CloseForkResult {
                fork_id: fork_id.to_string(),
                status: cur.status.clone(),
                closed_by: closed_by.to_string(),
                recorded: false,
                p_after: p_after(),
                committed: false,
                reason: Some("stale_or_closed"),
            });
        }
        let (cur_gen, cur_attempt) = gen_attempt_of(&cur);
        if let Some(expected) = opts.expected_gen {
            if cur_gen != expected {
                return Ok(CloseForkResult {
                    fork_id: fork_id.to_string(),
                    status: cur.status.clone(),
                    closed_by: closed_by.to_string(),
                    recorded: false,
                    p_after: p_after(),
                    committed: false,
                    reason: Some("stale_generation"),
                });
            }
        }
        if outcome != "ok" {
            if let Some(expected) = opts.expected_attempt {
                if cur_attempt != expected {
                    return Ok(CloseForkResult {
                        fork_id: fork_id.to_string(),
                        status: cur.status.clone(),
                        closed_by: closed_by.to_string(),
                        recorded: false,
                        p_after: p_after(),
                        committed: false,
                        reason: Some("stale_attempt"),
                    });
                }
            }
        }
        let mut merged = parse_fork_metadata(cur.metadata_json.as_deref());
        if let Some(r) = reason {
            merged.insert("close_reason".into(), Value::String(redact_secrets(r)));
        }
        if let Some(c) = opts.cause {
            merged.insert("cause".into(), Value::String(c.as_str().into()));
        }
        let changes = db
            .execute(
                "UPDATE forks SET status = ?1, closed_by = ?2, closed_at_ms = ?3, metadata_json = ?4
                 WHERE fork_id = ?5 AND status IN ('open','waiting_human')",
                rusqlite::params![outcome, closed_by, now_ms_value, Value::Object(merged).to_string(), fork_id],
            )
            .map_err(|e| e.to_string())?;
        if changes != 1 {
            return Ok(CloseForkResult {
                fork_id: fork_id.to_string(),
                status: "open".into(),
                closed_by: closed_by.to_string(),
                recorded: false,
                p_after: p_after(),
                committed: false,
                reason: Some("stale_or_closed"),
            });
        }
        close_open_fork_intervals_in_tx(db, fork_id, now_ms_value)?;
        if record_stats && outcome != "rerouted" {
            let label = get_project_label(db, &cur.project_id)?;
            update_stats_in_tx(db, &cur.project_id, &label, route, &cur.tool_class, outcome == "ok", 1.0, None, now_ms_value)?;
            insert_event_row(
                db,
                &EventRowIn {
                    dedupe_key: None,
                    fork_id: Some(fork_id),
                    project_id: &cur.project_id,
                    conversation_id: None,
                    source: EventSource::Cli,
                    route: &cur.route,
                    tool_name: "dg_close",
                    tool_class: &cur.tool_class,
                    ok: outcome == "ok",
                    weight: 1.0,
                    is_retry: false,
                    target_hash: None,
                    error_snippet: None,
                    cause: opts.cause,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    cost_usd: None,
                    cost_status: None,
                    now_ms: now_ms_value,
                },
            )?;
        }
        Ok(CloseForkResult {
            fork_id: fork_id.to_string(),
            status: outcome.to_string(),
            closed_by: closed_by.to_string(),
            recorded: record_stats && outcome != "rerouted",
            p_after: p_after(),
            committed: true,
            reason: None,
        })
    };
    let result = work();
    match &result {
        Ok(r) if !r.committed => {
            let _ = db.execute_batch("ROLLBACK");
        }
        Ok(_) => db.execute_batch("COMMIT").map_err(|e| e.to_string())?,
        Err(_) => {
            let _ = db.execute_batch("ROLLBACK");
        }
    }
    result
}

/// Attempt/Generation 범프(PB-1 §3.2): `open`/`waiting_human` fork의 열린 구간을
/// 닫고 `attempt+1`(`bump_gen` 시 `generation`도 +1)을 1급 컬럼·메타에 동기화한 뒤
/// 새 attempt의 `wait` 구간을 연다 — 모두 단일 BEGIN IMMEDIATE 안에서.
pub fn bump_fork_attempt(
    db: &Connection,
    fork_id: &str,
    bump_gen: bool,
    now_ms_value: Option<i64>,
) -> Result<(String, i64, i64), String> {
    let now_ms_value = now_ms_value.unwrap_or_else(now_ms_default);
    db.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    let work = || -> Result<(String, i64, i64), String> {
        let cur = query_fork(db, fork_id)?.ok_or_else(|| format!("fork not found: {}", fork_id))?;
        if !is_fork_live_status(&cur.status) {
            return Err(format!("fork {} cannot bump attempt (status={})", fork_id, cur.status));
        }
        let (gen, attempt) = gen_attempt_of(&cur);
        let new_gen = gen + if bump_gen { 1 } else { 0 };
        let new_att = attempt + 1;
        close_open_fork_intervals_in_tx(db, fork_id, now_ms_value)?;
        let mut merged = parse_fork_metadata(cur.metadata_json.as_deref());
        merged.insert("gen".into(), Value::from(new_gen));
        merged.insert("attempt".into(), Value::from(new_att));
        let changes = if schema_of(db).forks_v2 {
            db.execute(
                "UPDATE forks SET generation = ?1, attempt = ?2, metadata_json = ?3, last_touch_ms = ?4
                 WHERE fork_id = ?5 AND status IN ('open','waiting_human')",
                rusqlite::params![new_gen, new_att, Value::Object(merged).to_string(), now_ms_value, fork_id],
            )
            .map_err(|e| e.to_string())?
        } else {
            db.execute(
                "UPDATE forks SET metadata_json = ?1, last_touch_ms = ?2
                 WHERE fork_id = ?3 AND status IN ('open','waiting_human')",
                rusqlite::params![Value::Object(merged).to_string(), now_ms_value, fork_id],
            )
            .map_err(|e| e.to_string())?
        };
        if changes != 1 {
            return Err(format!("fork {} stale_or_closed", fork_id));
        }
        open_fork_interval_in_tx(
            db,
            &ForkIntervalArgs {
                fork_id,
                attempt: new_att,
                phase: ForkIntervalPhase::Wait,
                now_ms: now_ms_value,
            },
        )?;
        Ok((fork_id.to_string(), new_gen, new_att))
    };
    let result = work();
    match &result {
        Ok(_) => db.execute_batch("COMMIT").map_err(|e| e.to_string())?,
        Err(_) => {
            let _ = db.execute_batch("ROLLBACK");
        }
    }
    result
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettleOrphansResult {
    pub settled: usize,
    pub fork_ids: Vec<String>,
}

/// 고아 Fork TTL 정산(§4.6): `now - COALESCE(last_touch_ms, created_at_ms) > ttl_ms`인
/// `open`/`waiting_human` 행만 `abandoned`(closed_by='ttl')로 전환하고
/// EWMA 실패(source='ttl_orphan', w=1.0)를 반영한다. 생존 편향 차단이 목적이다.
pub fn settle_orphans(db: &Connection, now_ms_value: i64) -> Result<SettleOrphansResult, String> {
    let mut settled_ids: Vec<String> = Vec::new();
    // Zero-TOCTOU (§2.3): stale 조회·상태 검증·UPDATE를 전부 BEGIN IMMEDIATE 안에서
    // 수행하고, `changes == 1`일 때만 표본을 기록한다 — 동시 정산 경합 시 나머지
    // writer는 `status='open'` 조건 불일치로 자동 탈락한다.
    db.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    let mut work = || -> Result<(), String> {
        let mut stmt = db
            .prepare(
                "SELECT fork_id, project_id, project_label, route, tool_class, metadata_json, created_at_ms FROM forks
                 WHERE status IN ('open','waiting_human') AND (?1 - COALESCE(last_touch_ms, created_at_ms)) > ttl_ms",
            )
            .map_err(|e| e.to_string())?;
        let stale: Vec<(String, String, String, String, String, Option<String>, i64)> = stmt
            .query_map(rusqlite::params![now_ms_value], |r| {
                Ok((
                    r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        drop(stmt);
        for (fork_id, project_id, project_label, route_s, tool_class, metadata_json, created_at_ms) in stale {
            // waiting_human 예외(§2.2): 사람 승인 대기 fork는 최대 7일까지만 TTL 면제
            let meta = parse_fork_metadata(metadata_json.as_deref());
            let waiting_human = match meta.get("waiting_human") {
                Some(Value::Bool(b)) => *b,
                Some(v) => v.as_i64() == Some(1),
                None => false,
            };
            if waiting_human {
                let since = meta_i64(&meta, "waiting_since_ms").unwrap_or(created_at_ms);
                if now_ms_value - since <= WAITING_HUMAN_MAX_MS {
                    continue;
                }
            }
            let changes = db
                .execute(
                    "UPDATE forks SET status = 'abandoned', closed_by = 'ttl', closed_at_ms = ?1 WHERE fork_id = ?2 AND status IN ('open','waiting_human')",
                    rusqlite::params![now_ms_value, fork_id],
                )
                .map_err(|e| e.to_string())?;
            if changes != 1 {
                continue; // 이미 타 프로세스가 닫은 fork — 유령 표본 차단
            }
            close_open_fork_intervals_in_tx(db, &fork_id, now_ms_value)?;
            let route = RouteId::parse(&route_s).unwrap_or(RouteId::ToolSelect);
            update_stats_in_tx(db, &project_id, &project_label, route, &tool_class, false, 1.0, None, now_ms_value)?;
            insert_event_row(
                db,
                &EventRowIn {
                    dedupe_key: None,
                    fork_id: Some(&fork_id),
                    project_id: &project_id,
                    conversation_id: None,
                    source: EventSource::TtlOrphan,
                    route: &route_s,
                    tool_name: "ttl_settle",
                    tool_class: &tool_class,
                    ok: false,
                    weight: 1.0,
                    is_retry: false,
                    target_hash: None,
                    error_snippet: None,
                    cause: None,
                    input_tokens: None,
                    output_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    cost_usd: None,
                    cost_status: None,
                    now_ms: now_ms_value,
                },
            )?;
            settled_ids.push(fork_id);
        }
        Ok(())
    };
    let result = work();
    match &result {
        Ok(_) => db.execute_batch("COMMIT").map_err(|e| e.to_string())?,
        Err(_) => {
            let _ = db.execute_batch("ROLLBACK");
        }
    }
    result?;
    Ok(SettleOrphansResult {
        settled: settled_ids.len(),
        fork_ids: settled_ids,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackpropResult {
    pub fork_id: String,
    pub status: String,
    pub neon_verdict: Option<&'static str>,
    pub weight_applied: f64,
    pub p_after: f64,
    /// false면 상태 불일치로 롤백됨 — 이중 역귀속 차단 (§2.3)
    pub committed: bool,
    pub reason: Option<&'static str>,
}

/// Neon FAIL 지연 역귀속(§4.6): `open`/`ok` fork를 `backprop_fail`로 재개방 후
/// 가중 실패(weight=penalty, source='neon_backprop')를 stats에 반영한다.
/// `WHERE status IN ('open','ok')` + `changes == 1` 가드로 이중 역귀속을
/// 원자적으로 차단한다. 거부 시 `{ committed: false, reason }` 반환.
pub fn backprop_fork(
    db: &Connection,
    fork_id: &str,
    penalty: f64,
    reason: Option<&str>,
    opts: BackpropOpts,
) -> Result<BackpropResult, String> {
    let pre = query_fork(db, fork_id)?
        .ok_or_else(|| format!("fork not found: {}", fork_id))?;
    if !is_fork_live_status(&pre.status) && pre.status != "ok" {
        return Err(format!("fork {} cannot backprop (status={})", fork_id, pre.status));
    }
    let now_ms_value = opts.now_ms.unwrap_or_else(now_ms_default);
    let route = RouteId::parse(&pre.route).unwrap_or(RouteId::ToolSelect);
    db.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    let work = || -> Result<BackpropResult, String> {
        let cur: Option<(String, Option<String>, String, String, String)> = db
            .query_row(
                "SELECT status, metadata_json, project_id, route, tool_class FROM forks WHERE fork_id = ?1",
                [fork_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let p_after = get_posterior(db, &pre.project_id, route, &pre.tool_class).map(|p| p.p).unwrap_or(0.0);
        let Some((status, metadata_json, project_id, route_s, tool_class)) = cur else {
            return Ok(BackpropResult {
                fork_id: fork_id.to_string(),
                status: "unknown".into(),
                neon_verdict: None,
                weight_applied: 0.0,
                p_after,
                committed: false,
                reason: Some("stale_or_closed"),
            });
        };
        if !is_fork_live_status(&status) && status != "ok" {
            return Ok(BackpropResult {
                fork_id: fork_id.to_string(),
                status,
                neon_verdict: None,
                weight_applied: 0.0,
                p_after,
                committed: false,
                reason: Some("stale_or_closed"),
            });
        }
        let mut merged = parse_fork_metadata(metadata_json.as_deref());
        if let Some(r) = reason {
            merged.insert("backprop_reason".into(), Value::String(redact_secrets(r)));
        }
        merged.insert("penalty".into(), Value::from(penalty));
        if let Some(c) = opts.cause {
            merged.insert("cause".into(), Value::String(c.as_str().into()));
        }
        let changes = db
            .execute(
                "UPDATE forks SET status = 'backprop_fail', closed_by = 'verifier', neon_verdict = 'fail',
                   closed_at_ms = COALESCE(closed_at_ms, ?1), metadata_json = ?2
                 WHERE fork_id = ?3 AND status IN ('open','ok','waiting_human')",
                rusqlite::params![now_ms_value, Value::Object(merged).to_string(), fork_id],
            )
            .map_err(|e| e.to_string())?;
        if changes != 1 {
            return Ok(BackpropResult {
                fork_id: fork_id.to_string(),
                status,
                neon_verdict: None,
                weight_applied: 0.0,
                p_after,
                committed: false,
                reason: Some("stale_or_closed"),
            });
        }
        close_open_fork_intervals_in_tx(db, fork_id, now_ms_value)?;
        let cur_route = RouteId::parse(&route_s).unwrap_or(RouteId::ToolSelect);
        let label = get_project_label(db, &project_id)?;
        update_stats_in_tx(db, &project_id, &label, cur_route, &tool_class, false, penalty, None, now_ms_value)?;
        insert_event_row(
            db,
            &EventRowIn {
                dedupe_key: None,
                fork_id: Some(fork_id),
                project_id: &project_id,
                conversation_id: None,
                source: EventSource::NeonBackprop,
                route: &route_s,
                tool_name: "neon_verifier",
                tool_class: &tool_class,
                ok: false,
                weight: penalty,
                is_retry: false,
                target_hash: None,
                error_snippet: None,
                cause: opts.cause,
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                cost_usd: None,
                cost_status: None,
                now_ms: now_ms_value,
            },
        )?;
        let p_after_final = get_posterior(db, &pre.project_id, route, &pre.tool_class).map(|p| p.p).unwrap_or(0.0);
        Ok(BackpropResult {
            fork_id: fork_id.to_string(),
            status: "backprop_fail".into(),
            neon_verdict: Some("fail"),
            weight_applied: penalty,
            p_after: p_after_final,
            committed: true,
            reason: None,
        })
    };
    let result = work();
    match &result {
        Ok(r) if !r.committed => {
            let _ = db.execute_batch("ROLLBACK");
        }
        Ok(_) => db.execute_batch("COMMIT").map_err(|e| e.to_string())?,
        Err(_) => {
            let _ = db.execute_batch("ROLLBACK");
        }
    }
    result
}

#[derive(Debug, Clone, Default)]
pub struct BackpropOpts {
    pub now_ms: Option<i64>,
    pub cause: Option<FailureCause>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompactResult {
    pub deleted_events: usize,
    pub deleted_forks: usize,
    pub checkpoint: &'static str,
    pub remaining_events: i64,
}

/// 로그 컴팩션(§3.5): `stats_summary`는 상수 크기 유지, `decision_events`는
/// 30일 경과 또는 MAX_EVENTS 초과분(이 중 hook/wrap이 아니거나 14일 경과)만 삭제하고
/// 종료 후 30일 경과 forks를 정리한 뒤 WAL 체크포인트(TRUNCATE)를 실행한다.
pub fn compact_db(db: &Connection, max_events: i64, days: i64, now_ms_value: i64) -> Result<CompactResult, String> {
    let cutoff30 = now_ms_value - days * 86_400_000;
    let cutoff14 = now_ms_value - 14 * 86_400_000;
    db.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    let work = || -> Result<CompactResult, String> {
        let r1 = db
            .execute("DELETE FROM decision_events WHERE created_at_ms < ?1", [cutoff30])
            .map_err(|e| e.to_string())?;
        let mut deleted = r1;
        let count: i64 = db
            .query_row("SELECT COUNT(*) AS n FROM decision_events", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if count > max_events {
            let r2 = db
                .execute(
                    "DELETE FROM decision_events WHERE event_id IN (
                       SELECT event_id FROM decision_events
                       WHERE source NOT IN ('hook','wrap') OR created_at_ms < ?1
                       ORDER BY event_id DESC LIMIT -1 OFFSET ?2
                     )",
                    rusqlite::params![cutoff14, max_events],
                )
                .map_err(|e| e.to_string())?;
            deleted += r2;
        }
        if schema_of(db).intervals {
            // forks 행 삭제에 앞서 대응 구간 정리 — fork_intervals에는 FK가 없어 고아 방지는 호출자 책임 (§1.6)
            db.execute(
                "DELETE FROM fork_intervals WHERE fork_id IN (
                   SELECT fork_id FROM forks
                   WHERE status NOT IN ('open','waiting_human') AND closed_at_ms IS NOT NULL AND closed_at_ms < ?1)",
                [cutoff30],
            )
            .map_err(|e| e.to_string())?;
            db.execute(
                "DELETE FROM fork_intervals WHERE fork_id NOT IN (SELECT fork_id FROM forks)",
                [],
            )
            .map_err(|e| e.to_string())?;
        }
        let r3 = db
            .execute(
                "DELETE FROM forks WHERE status NOT IN ('open','waiting_human') AND closed_at_ms IS NOT NULL AND closed_at_ms < ?1",
                [cutoff30],
            )
            .map_err(|e| e.to_string())?;
        let remaining: i64 = db
            .query_row("SELECT COUNT(*) AS n FROM decision_events", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        Ok(CompactResult {
            deleted_events: deleted,
            deleted_forks: r3,
            checkpoint: "TRUNCATE",
            remaining_events: remaining,
        })
    };
    let result = work();
    match &result {
        Ok(_) => {
            db.execute_batch("COMMIT").map_err(|e| e.to_string())?;
            let _: (i64, i64, i64) = db
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(|e| e.to_string())?;
        }
        Err(_) => {
            let _ = db.execute_batch("ROLLBACK");
        }
    }
    result
}

/// `sample_sources` 분포(§5.2): 최근 표본의 source별 건수. 표본 편중 관측용.
pub fn sample_sources(
    db: &Connection,
    project_id: &str,
    route: Option<RouteId>,
    tool_class: Option<&str>,
    limit: i64,
) -> Result<Vec<(String, i64)>, String> {
    let mut stmt = db
        .prepare(
            "SELECT source, COUNT(*) AS n FROM (
               SELECT source FROM decision_events
               WHERE project_id = ?1
                 AND (?2 IS NULL OR route = ?2)
                 AND (?3 IS NULL OR tool_class = ?3)
               ORDER BY event_id DESC LIMIT ?4
             ) GROUP BY source ORDER BY n DESC",
        )
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map(
            rusqlite::params![
                project_id,
                route.map(|r| r.as_str()),
                tool_class,
                limit,
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// PR-A2: 단일 트랜잭션 wrap 커밋 · verdict 적용 (§2.3~§2.5)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CommitWrapInput<'a> {
    pub fork_id: &'a str,
    pub tool_name: Option<&'a str>,
    pub ok: bool,
    pub outcome: CloseOutcome,
    pub cause: FailureCause,
    pub exit_code: Option<i64>,
    pub signal: Option<&'a str>,
    pub error_snippet: Option<&'a str>,
    pub command_line: Option<&'a str>,
    pub source: Option<EventSource>,
    pub expected_gen: Option<i64>,
    pub expected_attempt: Option<i64>,
    pub record_stats: Option<bool>,
    pub conversation_id: Option<&'a str>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub cost_status: Option<CostStatus>,
    pub now_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommitWrapResult {
    pub committed: bool,
    pub closed: bool,
    pub recorded: bool,
    pub outcome: String,
    pub reason: Option<&'static str>,
    pub p_after: f64,
}

/// `dg wrap` 종료 원자 커밋(§2.3-3): fork 상태·gen/attempt 펜스·UPDATE
/// `changes === 1`·`dedupe_key = wrap:<forkId>:a<attempt>`를 모두 같은
/// 트랜잭션에서 검증한다. 이미 닫힌 fork에 지연 도착하면
/// `{ committed: false, reason: "stale_or_closed" }`.
pub fn commit_wrap_outcome(db: &Connection, input: &CommitWrapInput) -> Result<CommitWrapResult, String> {
    let now_ms_value = input.now_ms.unwrap_or_else(now_ms_default);
    let record_stats = input.record_stats != Some(false);
    let self_invoke = is_self_invocation(&format!("{} {}", input.tool_name.unwrap_or(""), input.command_line.unwrap_or("")));
    db.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    let work = || -> Result<CommitWrapResult, String> {
        let cur = match query_fork(db, input.fork_id)? {
            Some(c) => c,
            None => {
                return Ok(CommitWrapResult {
                    committed: false,
                    closed: false,
                    recorded: false,
                    outcome: "unknown".into(),
                    reason: Some("not_found"),
                    p_after: 0.0,
                })
            }
        };
        let route = RouteId::parse(&cur.route).unwrap_or(RouteId::ToolSelect);
        let p_after = get_posterior(db, &cur.project_id, route, &cur.tool_class).map(|p| p.p).unwrap_or(0.0);
        if !is_fork_live_status(&cur.status) {
            return Ok(CommitWrapResult {
                committed: false,
                closed: false,
                recorded: false,
                outcome: cur.status.clone(),
                reason: Some("stale_or_closed"),
                p_after,
            });
        }
        let (cur_gen, cur_attempt) = gen_attempt_of(&cur);
        if let Some(expected) = input.expected_gen {
            if cur_gen != expected {
                return Ok(CommitWrapResult {
                    committed: false,
                    closed: false,
                    recorded: false,
                    outcome: "open".into(),
                    reason: Some("stale_generation"),
                    p_after,
                });
            }
        }
        if input.outcome != "ok" {
            if let Some(expected) = input.expected_attempt {
                if cur_attempt != expected {
                    return Ok(CommitWrapResult {
                        committed: false,
                        closed: false,
                        recorded: false,
                        outcome: "open".into(),
                        reason: Some("stale_attempt"),
                        p_after,
                    });
                }
            }
        }

        let mut merged = parse_fork_metadata(cur.metadata_json.as_deref());
        merged.insert("cause".into(), Value::String(input.cause.as_str().into()));
        merged.insert("exit_code".into(), input.exit_code.map(Value::from).unwrap_or(Value::Null));
        if let Some(snippet) = input.error_snippet {
            merged.insert(
                "close_reason".into(),
                Value::String(truncate_chars(&redact_secrets(snippet), ERROR_SNIPPET_MAX_CHARS)),
            );
        }
        let changes = db
            .execute(
                "UPDATE forks SET status = ?1, closed_by = 'worker', closed_at_ms = ?2, metadata_json = ?3
                 WHERE fork_id = ?4 AND status IN ('open','waiting_human')",
                rusqlite::params![
                    input.outcome,
                    now_ms_value,
                    Value::Object(merged).to_string(),
                    input.fork_id
                ],
            )
            .map_err(|e| e.to_string())?;
        if changes != 1 {
            return Ok(CommitWrapResult {
                committed: false,
                closed: false,
                recorded: false,
                outcome: "open".into(),
                reason: Some("stale_or_closed"),
                p_after,
            });
        }
        close_open_fork_intervals_in_tx(db, input.fork_id, now_ms_value)?;

        let mut recorded = false;
        if !self_invoke {
            let dedupe_key = format!("wrap:{}:a{}", input.fork_id, cur_attempt);
            let ev_changes = insert_event_row(
                db,
                &EventRowIn {
                    dedupe_key: Some(&dedupe_key),
                    fork_id: Some(input.fork_id),
                    project_id: &cur.project_id,
                    conversation_id: input.conversation_id,
                    source: input.source.unwrap_or(EventSource::Wrap),
                    route: &cur.route,
                    tool_name: input.tool_name.unwrap_or("dg_wrap"),
                    tool_class: &cur.tool_class,
                    ok: input.outcome == "ok",
                    weight: 1.0,
                    is_retry: false,
                    target_hash: None,
                    error_snippet: format_error_snippet(input.cause, input.error_snippet).as_deref(),
                    cause: Some(input.cause),
                    input_tokens: input.input_tokens,
                    output_tokens: input.output_tokens,
                    cache_read_tokens: input.cache_read_tokens,
                    cache_write_tokens: input.cache_write_tokens,
                    cost_usd: input.cost_usd,
                    cost_status: input.cost_status,
                    now_ms: now_ms_value,
                },
            )?;
            // 표본 기록은 이벤트가 실제로 1건 삽입됐을 때만 — dedupe 충돌·비표본 결과 차단
            let skip_stats = input.outcome == "rerouted" || is_audit_only_cause(input.cause) || !record_stats;
            if ev_changes == 1 && !skip_stats {
                update_stats_in_tx(
                    db,
                    &cur.project_id,
                    &cur.project_label,
                    route,
                    &cur.tool_class,
                    input.outcome == "ok",
                    1.0,
                    None,
                    now_ms_value,
                )?;
                recorded = true;
            }
        }
        Ok(CommitWrapResult {
            committed: true,
            closed: true,
            recorded,
            outcome: input.outcome.to_string(),
            reason: None,
            p_after,
        })
    };
    let result = work();
    match &result {
        Ok(r) if !r.committed => {
            let _ = db.execute_batch("ROLLBACK");
        }
        Ok(_) => db.execute_batch("COMMIT").map_err(|e| e.to_string())?,
        Err(_) => {
            let _ = db.execute_batch("ROLLBACK");
        }
    }
    result
}

/// `waiting_human` 메타 상태 마킹(§2.2 Phase A): fork 상태는 유지한 채
/// `metadata_json.waiting_human=true`를 세팅한다 — settleOrphans가 최대
/// `WAITING_HUMAN_MAX_MS`(7일)까지 TTL 정산을 면제한다. `open`/`waiting_human`/
/// `ok`만 허용. 런타임은 status를 `open`으로 유지하고, 열린 구간이면 `human`
/// 단계 구간으로 전환해 대기 시간을 fork_intervals에 남긴다.
pub fn mark_fork_waiting_human(
    db: &Connection,
    fork_id: &str,
    reason: &str,
    rework_count: Option<i64>,
    now_ms_value: Option<i64>,
) -> Result<(bool, Option<&'static str>), String> {
    let now_ms_value = now_ms_value.unwrap_or_else(now_ms_default);
    db.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    let work = || -> Result<(bool, Option<&'static str>), String> {
        let cur = match query_fork(db, fork_id)? {
            Some(c) => c,
            None => return Ok((false, Some("not_found"))),
        };
        if !is_fork_live_status(&cur.status) && cur.status != "ok" {
            return Ok((false, Some("stale_or_closed")));
        }
        let mut merged = parse_fork_metadata(cur.metadata_json.as_deref());
        merged.insert("waiting_human".into(), Value::from(true));
        merged.insert(
            "waiting_reason".into(),
            Value::String(truncate_chars(&redact_secrets(reason), ERROR_SNIPPET_MAX_CHARS)),
        );
        merged.insert("waiting_since_ms".into(), Value::from(now_ms_value));
        if let Some(rc) = rework_count {
            merged.insert("rework_count".into(), Value::from(rc));
        }
        let changes = db
            .execute(
                "UPDATE forks SET metadata_json = ?1 WHERE fork_id = ?2 AND status IN ('open','ok','waiting_human')",
                rusqlite::params![Value::Object(merged).to_string(), fork_id],
            )
            .map_err(|e| e.to_string())?;
        if changes != 1 {
            return Ok((false, Some("stale_or_closed")));
        }
        // 열려 있는 fork(open/waiting_human)만 human 구간으로 전환 — ok는 종결이라 구간 없음
        if is_fork_live_status(&cur.status) {
            let (_, attempt) = gen_attempt_of(&cur);
            open_fork_interval_in_tx(
                db,
                &ForkIntervalArgs {
                    fork_id,
                    attempt,
                    phase: ForkIntervalPhase::Human,
                    now_ms: now_ms_value,
                },
            )?;
        }
        Ok((true, None))
    };
    let result = work();
    match &result {
        Ok(_) => db.execute_batch("COMMIT").map_err(|e| e.to_string())?,
        Err(_) => {
            let _ = db.execute_batch("ROLLBACK");
        }
    }
    result
}

#[derive(Debug, Clone, PartialEq)]
pub struct VerdictApplyResult {
    /// pass(0) / backprop_fail(0) / rework(0) / escalate_human(3)
    pub action: &'static str,
    pub exit_code: i32,
    pub committed: bool,
    pub reason: Option<String>,
    pub waiting_human: Option<bool>,
    pub p_after: Option<f64>,
}

/// 검증된 verdict의 fork 상태 반영(§2.6 fail-closed).
/// - `pass`: `open`→`ok`(closed_by='verifier'). 이미 `ok`면 멱등 통과.
/// - `fail`: `backprop_fork(2.0, cause='gate_fail')`. 이미 `backprop_fail`이면 멱등.
/// - `rework`: `rework_count < max_reworks`면 count+1 + w=1.0 페널티 표본(fork 유지,
///   단 `ok`면 backprop_fail 전이); 예산 소진 시 `waiting_human` + escalate.
/// - `human`/무효 verdict: `waiting_human` 마킹 + escalate. 어떤 경로도
///   누락·불량·재생 verdict를 pass로 해석하지 않는다.
pub fn apply_verdict_to_fork(
    db: &Connection,
    fork_id: &str,
    verdict: &VerdictValidationResult,
    max_reworks: Option<i64>,
    now_ms_value: Option<i64>,
) -> Result<VerdictApplyResult, String> {
    let now_ms_value = now_ms_value.unwrap_or_else(now_ms_default);
    let fork = match query_fork(db, fork_id)? {
        Some(f) => f,
        None => {
            return Ok(VerdictApplyResult {
                action: "escalate_human",
                exit_code: 3,
                committed: false,
                reason: Some("fork_not_found".into()),
                waiting_human: None,
                p_after: None,
            })
        }
    };
    let v_reason = verdict
        .document
        .as_ref()
        .map(|d| {
            d.action_required
                .clone()
                .unwrap_or_else(|| d.reason.clone())
        })
        .unwrap_or_else(|| verdict.errors.join("; "));

    if !verdict.valid || verdict.effective_decision == VerdictDecision::Human {
        let reason_text = if v_reason.is_empty() {
            "invalid/human verdict".to_string()
        } else {
            v_reason.clone()
        };
        let (marked, _) = mark_fork_waiting_human(db, fork_id, &reason_text, None, Some(now_ms_value))?;
        return Ok(VerdictApplyResult {
            action: "escalate_human",
            exit_code: 3,
            committed: marked,
            reason: Some(reason_text),
            waiting_human: Some(marked),
            p_after: None,
        });
    }

    if verdict.effective_decision == VerdictDecision::Pass {
        if fork.status == "ok" {
            return Ok(VerdictApplyResult {
                action: "pass",
                exit_code: 0,
                committed: true,
                reason: Some("idempotent".into()),
                waiting_human: None,
                p_after: None,
            });
        }
        if !is_fork_live_status(&fork.status) {
            return Ok(VerdictApplyResult {
                action: "escalate_human",
                exit_code: 3,
                committed: false,
                reason: Some(format!("fork status {}", fork.status)),
                waiting_human: None,
                p_after: None,
            });
        }
        let res = close_fork(
            db,
            fork_id,
            "ok",
            "verifier",
            Some(if v_reason.is_empty() { "verdict pass" } else { &v_reason }),
            &CloseForkOptions {
                cause: Some(FailureCause::OkPass),
                now_ms: Some(now_ms_value),
                ..Default::default()
            },
        )?;
        if !res.committed {
            return Ok(VerdictApplyResult {
                action: "escalate_human",
                exit_code: 3,
                committed: false,
                reason: res.reason.map(str::to_string),
                waiting_human: None,
                p_after: None,
            });
        }
        return Ok(VerdictApplyResult {
            action: "pass",
            exit_code: 0,
            committed: true,
            reason: None,
            waiting_human: None,
            p_after: Some(res.p_after),
        });
    }

    if verdict.effective_decision == VerdictDecision::Fail {
        if fork.status == "backprop_fail" {
            return Ok(VerdictApplyResult {
                action: "backprop_fail",
                exit_code: 0,
                committed: true,
                reason: Some("idempotent".into()),
                waiting_human: None,
                p_after: None,
            });
        }
        if !is_fork_live_status(&fork.status) && fork.status != "ok" {
            return Ok(VerdictApplyResult {
                action: "escalate_human",
                exit_code: 3,
                committed: false,
                reason: Some(format!("fork status {}", fork.status)),
                waiting_human: None,
                p_after: None,
            });
        }
        let res = backprop_fork(
            db,
            fork_id,
            2.0,
            Some(if v_reason.is_empty() { "verdict fail" } else { &v_reason }),
            BackpropOpts {
                cause: Some(FailureCause::GateFail),
                now_ms: Some(now_ms_value),
            },
        )?;
        if !res.committed {
            return Ok(VerdictApplyResult {
                action: "escalate_human",
                exit_code: 3,
                committed: false,
                reason: res.reason.map(str::to_string),
                waiting_human: None,
                p_after: None,
            });
        }
        return Ok(VerdictApplyResult {
            action: "backprop_fail",
            exit_code: 0,
            committed: true,
            reason: None,
            waiting_human: None,
            p_after: Some(res.p_after),
        });
    }

    // rework — 예산 검사·count 증분·페널티 표본·attempt 범프를 단일 트랜잭션으로
    let max_reworks = max_reworks.unwrap_or(2);
    /// 트랜잭션 내부 결과 — 예산 소진은 롤백 후 waiting_human 경로로 분기한다
    enum ReworkTxOutcome {
        BudgetExhausted { rework_count: i64, v_reason: String },
        Escalate(String),
        Committed,
    }
    db.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    let work = || -> Result<ReworkTxOutcome, String> {
        let cur = match query_fork(db, fork_id)? {
            Some(c) => c,
            None => return Ok(ReworkTxOutcome::Escalate("fork_not_found".into())),
        };
        let meta = parse_fork_metadata(cur.metadata_json.as_deref());
        let rework_count = meta_i64(&meta, "rework_count").unwrap_or(0);
        if rework_count >= max_reworks {
            // 예산 소진 → 트랜잭션 롤백 후 waiting_human(가능 시) + 사람 에스컬레이션
            return Ok(ReworkTxOutcome::BudgetExhausted { rework_count, v_reason: v_reason.clone() });
        }
        if !is_fork_live_status(&cur.status) && cur.status != "ok" {
            return Ok(ReworkTxOutcome::Escalate(format!("fork status {}", cur.status)));
        }
        let (_, cur_attempt) = gen_attempt_of(&cur);
        let openish = is_fork_live_status(&cur.status);
        let mut merged = meta;
        merged.insert("rework_count".into(), Value::from(rework_count + 1));
        merged.insert("cause".into(), Value::String("gate_fail".into()));
        if openish {
            merged.insert("attempt".into(), Value::from(cur_attempt + 1));
        }
        if !v_reason.is_empty() {
            merged.insert(
                "rework_reason".into(),
                Value::String(truncate_chars(&redact_secrets(&v_reason), ERROR_SNIPPET_MAX_CHARS)),
            );
        }
        // open 계열이면 재작업을 위해 fork 유지 + attempt 범프, ok면 backprop_fail로 전이 — 둘 다 w=1.0 페널티
        let changes = if openish {
            if schema_of(db).forks_v2 {
                db.execute(
                    "UPDATE forks SET metadata_json = ?1, attempt = attempt + 1
                     WHERE fork_id = ?2 AND status IN ('open','waiting_human')",
                    rusqlite::params![Value::Object(merged).to_string(), fork_id],
                )
                .map_err(|e| e.to_string())?
            } else {
                db.execute(
                    "UPDATE forks SET metadata_json = ?1 WHERE fork_id = ?2 AND status IN ('open','waiting_human')",
                    rusqlite::params![Value::Object(merged).to_string(), fork_id],
                )
                .map_err(|e| e.to_string())?
            }
        } else {
            db.execute(
                "UPDATE forks SET status = 'backprop_fail', closed_by = 'verifier', neon_verdict = 'fail',
                   closed_at_ms = COALESCE(closed_at_ms, ?1), metadata_json = ?2
                 WHERE fork_id = ?3 AND status = 'ok'",
                rusqlite::params![now_ms_value, Value::Object(merged).to_string(), fork_id],
            )
            .map_err(|e| e.to_string())?
        };
        if changes != 1 {
            return Ok(ReworkTxOutcome::Escalate("stale_or_closed".into()));
        }
        if openish {
            // 재시도 경계: 구형 구간(현 attempt)을 닫고 새 attempt의 wait 구간을 연다 (§3.2)
            close_open_fork_intervals_in_tx(db, fork_id, now_ms_value)?;
            open_fork_interval_in_tx(
                db,
                &ForkIntervalArgs {
                    fork_id,
                    attempt: cur_attempt + 1,
                    phase: ForkIntervalPhase::Wait,
                    now_ms: now_ms_value,
                },
            )?;
        } else {
            close_open_fork_intervals_in_tx(db, fork_id, now_ms_value)?;
        }
        insert_event_row(
            db,
            &EventRowIn {
                dedupe_key: None,
                fork_id: Some(fork_id),
                project_id: &cur.project_id,
                conversation_id: None,
                source: EventSource::NeonBackprop,
                route: &cur.route,
                tool_name: "neon_verifier",
                tool_class: &cur.tool_class,
                ok: false,
                weight: 1.0,
                is_retry: false,
                target_hash: None,
                error_snippet: None,
                cause: Some(FailureCause::GateFail),
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                cost_usd: None,
                cost_status: None,
                now_ms: now_ms_value,
            },
        )?;
        let cur_route = RouteId::parse(&cur.route).unwrap_or(RouteId::ToolSelect);
        update_stats_in_tx(db, &cur.project_id, &cur.project_label, cur_route, &cur.tool_class, false, 1.0, None, now_ms_value)?;
        Ok(ReworkTxOutcome::Committed)
    };
    let result = work();
    match result {
        Ok(ReworkTxOutcome::BudgetExhausted { rework_count, v_reason }) => {
            let _ = db.execute_batch("ROLLBACK");
            let reason_text = format!(
                "rework budget exhausted ({}>={}): {}",
                rework_count, max_reworks, v_reason
            );
            let (marked, _) =
                mark_fork_waiting_human(db, fork_id, &reason_text, Some(rework_count), Some(now_ms_value))?;
            Ok(VerdictApplyResult {
                action: "escalate_human",
                exit_code: 3,
                committed: marked,
                reason: Some("rework budget exhausted".into()),
                waiting_human: Some(marked),
                p_after: None,
            })
        }
        Ok(ReworkTxOutcome::Escalate(reason)) => {
            let _ = db.execute_batch("ROLLBACK");
            Ok(VerdictApplyResult {
                action: "escalate_human",
                exit_code: 3,
                committed: false,
                reason: Some(reason),
                waiting_human: None,
                p_after: None,
            })
        }
        Ok(ReworkTxOutcome::Committed) => {
            db.execute_batch("COMMIT").map_err(|e| e.to_string())?;
            let cur = query_fork(db, fork_id)?;
            let p_after = cur
                .as_ref()
                .map(|c| {
                    get_posterior(
                        db,
                        &c.project_id,
                        RouteId::parse(&c.route).unwrap_or(RouteId::ToolSelect),
                        &c.tool_class,
                    )
                    .map(|p| p.p)
                    .unwrap_or(0.0)
                })
                .unwrap_or(0.0);
            Ok(VerdictApplyResult {
                action: "rework",
                exit_code: 0,
                committed: true,
                reason: None,
                waiting_human: None,
                p_after: Some(p_after),
            })
        }
        Err(err) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ForkRetroToolRow {
    pub tool_name: String,
    pub count: i64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ForkRetroData {
    pub fork: ForkRow,
    pub metadata: Map<String, Value>,
    pub event_count: i64,
    /// 해당 프로젝트 실패 도구 상위 N (retro 마크다운 소재, §2.7)
    pub top_failed_tools: Vec<ForkRetroToolRow>,
}

/// `dg retro`용 fork 회고 데이터 수집 — 읽기 전용, 표본 기록 없음
pub fn collect_fork_retro_data(db: &Connection, fork_id: &str, limit: i64) -> Result<Option<ForkRetroData>, String> {
    let Some(fork) = query_fork(db, fork_id)? else { return Ok(None) };
    let mut stmt = db
        .prepare(
            "SELECT e.tool_name AS tool_name, COUNT(*) AS n,
               (SELECT e2.error_snippet FROM decision_events e2
                 WHERE e2.project_id = e.project_id AND e2.tool_name = e.tool_name
                   AND e2.ok = 0 AND e2.error_snippet IS NOT NULL
                 ORDER BY e2.created_at_ms DESC LIMIT 1) AS last_error
             FROM decision_events e
             WHERE e.project_id = ?1 AND e.ok = 0
             GROUP BY e.tool_name ORDER BY n DESC, e.tool_name ASC LIMIT ?2",
        )
        .map_err(|e| e.to_string())?;
    let tools: Vec<ForkRetroToolRow> = stmt
        .query_map(rusqlite::params![fork.project_id, limit], |r| {
            Ok(ForkRetroToolRow {
                tool_name: r.get(0)?,
                count: r.get(1)?,
                last_error: r.get(2)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let ev: i64 = db
        .query_row("SELECT COUNT(*) AS n FROM decision_events WHERE fork_id = ?1", [fork_id], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    Ok(Some(ForkRetroData {
        metadata: parse_fork_metadata(fork.metadata_json.as_deref()),
        fork,
        event_count: ev,
        top_failed_tools: tools,
    }))
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScopeLeaseResult {
    pub acquired: bool,
    pub holder: Option<String>,
    pub reason: Option<String>,
}

/// 초(millis) → ISO 8601 UTC 문자열(JS `new Date(ms).toISOString()` 동일 형식).
fn epoch_ms_to_iso_utc(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let millis_of_day = ms.rem_euclid(86_400_000);
    let secs_of_day = millis_of_day / 1000;
    let msec = millis_of_day % 1000;
    // Howard Hinnant civil_from_days 알고리즘
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year,
        m,
        d,
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
        msec
    )
}

/// Cooperative Lease(§2.4): 동일 `metadata_json.repo_scope`를 가진 `open` fork 중
/// lease 미만료이고 `lease_owner`가 `live_sessions`에 있는 점유자가 있으면
/// `{ acquired: false, holder }`. 소유자 생존 증명(미제공·부재) 또는 만료 시
/// `acquired: true` — 미검증 점유로 배치를 막지 않는다(fail-open).
/// 순수 조회이며 새 점유는 호출자가 `issue_fork(metadata)`로 기록한다.
pub fn acquire_scope_lease(
    db: &Connection,
    repo_scope: &str,
    lease_owner: &str,
    live_sessions: Option<&[String]>,
    now_ms_value: i64,
) -> Result<ScopeLeaseResult, String> {
    let v2 = schema_of(db).forks_v2;
    let rows: Vec<(Option<String>, Option<String>, Option<i64>)> = if v2 {
        let mut stmt = db
            .prepare("SELECT metadata_json, lease_owner, lease_expires_ms FROM forks WHERE status IN ('open','waiting_human')")
            .map_err(|e| e.to_string())?;
        let out = stmt
            .query_map([], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        out
    } else {
        let mut stmt = db
            .prepare("SELECT metadata_json FROM forks WHERE status = 'open'")
            .map_err(|e| e.to_string())?;
        let out = stmt
            .query_map([], |r| Ok((r.get::<_, Option<String>>(0)?, None, None)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        out
    };
    for (metadata_json, col_owner, col_expires) in rows {
        let meta = parse_fork_metadata(metadata_json.as_deref());
        if meta_str(&meta, "repo_scope").as_deref() != Some(repo_scope) {
            continue;
        }
        let owner = if v2 {
            col_owner
                .or_else(|| meta_str(&meta, "lease_owner"))
                .filter(|s| !s.is_empty())
        } else {
            meta_str(&meta, "lease_owner")
        };
        let Some(owner) = owner else { continue }; // 스코프 기록은 있으나 점유자 미지정
        if owner == lease_owner {
            continue; // 자기 자신은 충돌 아님
        }
        let raw_until: Option<i64> = if v2 {
            col_expires.or_else(|| meta_i64(&meta, "lease_until_ms"))
        } else {
            meta_i64(&meta, "lease_until_ms")
        };
        let until = raw_until.unwrap_or(0);
        if until <= now_ms_value {
            continue; // lease 만료 → 탈취 가능
        }
        match live_sessions {
            Some(live) if !live.iter().any(|s| s == &owner) => continue, // 소유 세션 사망 확인 → 탈취
            None => continue, // 생존 검증 수단 없음 → 점유로 보지 않음 (fail-open)
            _ => {}
        }
        return Ok(ScopeLeaseResult {
            acquired: false,
            holder: Some(owner.clone()),
            reason: Some(format!("scope lease held by {} until {}", owner, epoch_ms_to_iso_utc(until))),
        });
    }
    Ok(ScopeLeaseResult {
        acquired: true,
        holder: None,
        reason: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrate;
    use crate::verdict::{validate_verdict_data, ValidateVerdictOptions};
    use rusqlite::Connection;
    use serde_json::json;

    const PID: &str = "proj_test";

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.busy_timeout(std::time::Duration::from_millis(5000)).unwrap();
        migrate(&c, None).unwrap();
        c
    }

    fn ri<'a>(route: RouteId, tc: &'a str, tn: &'a str, ok: bool) -> RecordInput<'a> {
        RecordInput {
            project_id: PID,
            project_label: "Test",
            route,
            tool_class: tc,
            tool_name: tn,
            ok,
            ..Default::default()
        }
    }

    fn rec(conn: &Connection, route: RouteId, tc: &str, tn: &str, ok: bool) -> RecordResult {
        record_tool_outcome(conn, &ri(route, tc, tn, ok)).unwrap()
    }

    fn issue(conn: &Connection, route: RouteId, tc: &str, now: i64) -> String {
        issue_fork(
            conn,
            &IssueForkInput {
                project_id: PID,
                project_label: "Test",
                route,
                tool_class: tc,
                decision: "split",
                p_at_gate: 0.5,
                basis_at_gate: DecisionBasis::Prior,
                forced_split: false,
                conversation_id: None,
                ttl_ms: None,
                title: None,
                metadata: None,
                now_ms: Some(now),
            },
        )
        .unwrap()
    }

    fn stats(conn: &Connection, pid: &str, route: RouteId, tc: &str) -> (f64, f64, i64, i64) {
        conn.query_row(
            "SELECT ewma_success, ewma_fail, raw_samples, consecutive_errors FROM stats_summary
             WHERE project_id = ?1 AND route = ?2 AND tool_class = ?3",
            rusqlite::params![pid, route.as_str(), tc],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
    }

    fn event_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM decision_events", [], |r| r.get(0)).unwrap()
    }

    fn open_intervals(conn: &Connection, fork_id: &str) -> Vec<(i64, String)> {
        let mut stmt = conn
            .prepare("SELECT attempt, phase FROM fork_intervals WHERE fork_id = ?1 AND ended_at_ms IS NULL")
            .unwrap();
        stmt.query_map([fork_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .flatten()
            .collect()
    }

    fn verdict_for(doc: &Value, fork_id: &str) -> VerdictValidationResult {
        validate_verdict_data(
            doc,
            &ValidateVerdictOptions {
                expected_fork_id: fork_id.to_string(),
                expected_head: None,
            },
        )
    }

    // ---------- 상수·분류 ----------

    #[test]
    fn constants_match_spec() {
        assert_eq!(GAMMA, 0.92);
        assert_eq!(M0, 6.0);
        assert_eq!(N_MIN, 5);
        assert_eq!(LAMBDA_MAX, 0.25);
        assert_eq!(SHARP_P_THRESHOLD, 0.7);
        assert_eq!(FORCED_SPLIT_ERRORS, 2);
        assert_eq!(TTL_EXPLORE_MS, 600_000);
        assert_eq!(TTL_CODING_MS, 3_600_000); // TS 원본 §4.6 값
        assert_eq!(RETRY_STEP_WINDOW, 3);
        assert_eq!(ERROR_SNIPPET_MAX_CHARS, 300);
        assert_eq!(WAITING_HUMAN_MAX_MS, 7 * 86_400_000);
    }

    #[test]
    fn prior_p0_table_exact_values() {
        assert_eq!(prior_p0(RouteId::FileSelect, "file_read"), 0.94);
        assert_eq!(prior_p0(RouteId::FileSelect, "file_edit"), 0.85);
        assert_eq!(prior_p0(RouteId::ToolSelect, "code_search"), 0.92);
        assert_eq!(prior_p0(RouteId::ToolSelect, "shell_exec"), 0.74);
        assert_eq!(prior_p0(RouteId::ToolSelect, "other"), 0.88);
        assert_eq!(prior_p0(RouteId::ToolSelect, "whatever"), 0.88);
        assert_eq!(prior_p0(RouteId::RetryOrAbort, "shell_exec"), 0.85);
        assert_eq!(prior_p0(RouteId::DelegateSwarm, "swarm_dispatch"), 0.8);
        assert_eq!(prior_p0(RouteId::FileSelect, "unknown"), 0.88);
    }

    #[test]
    fn classify_tool_covers_all_classes() {
        assert_eq!(classify_tool("read_file", false), (RouteId::FileSelect, "file_read".into()));
        assert_eq!(classify_tool(" read ", false), (RouteId::FileSelect, "file_read".into()));
        assert_eq!(classify_tool("ls", false), (RouteId::FileSelect, "file_read".into()));
        assert_eq!(classify_tool("find_by_name", false), (RouteId::FileSelect, "file_read".into()));
        assert_eq!(classify_tool("write_file", false), (RouteId::FileSelect, "file_edit".into()));
        assert_eq!(classify_tool("multi_replace_file_content", false), (RouteId::FileSelect, "file_edit".into()));
        assert_eq!(classify_tool("grep_search", false), (RouteId::ToolSelect, "code_search".into()));
        assert_eq!(classify_tool("bash", false), (RouteId::ToolSelect, "shell_exec".into()));
        assert_eq!(classify_tool("run_command", false), (RouteId::ToolSelect, "shell_exec".into()));
        assert_eq!(classify_tool("hc_run", false), (RouteId::DelegateSwarm, "swarm_dispatch".into()));
        assert_eq!(classify_tool("invoke_subagent", false), (RouteId::DelegateSwarm, "swarm_dispatch".into()));
        assert_eq!(classify_tool("schedule", false), (RouteId::ToolSelect, "other".into()));
        assert_eq!(classify_tool("ask_question", false), (RouteId::ToolSelect, "other".into()));
        assert_eq!(classify_tool("totally_new_tool", false), (RouteId::ToolSelect, "other".into()));
        // 재시도 문맥 정규화 — tool_class 유지
        assert_eq!(classify_tool("bash", true), (RouteId::RetryOrAbort, "shell_exec".into()));
        assert_eq!(classify_tool("read", true), (RouteId::RetryOrAbort, "file_read".into()));
    }

    #[test]
    fn classify_failure_priority_matrix() {
        let f = |i: ClassifyFailureInput| classify_failure(&i);
        // 종료 신호 우선
        assert_eq!(f(ClassifyFailureInput { ok: Some(true), ..Default::default() }), FailureCause::OkPass);
        assert_eq!(f(ClassifyFailureInput { stalled: true, error_text: Some("401"), ..Default::default() }), FailureCause::Stall);
        assert_eq!(f(ClassifyFailureInput { timed_out: true, ..Default::default() }), FailureCause::Timeout);
        assert_eq!(f(ClassifyFailureInput { artifact_missing: true, ..Default::default() }), FailureCause::ArtifactMissing);
        assert_eq!(f(ClassifyFailureInput { no_result: true, ..Default::default() }), FailureCause::NoResult);
        assert_eq!(f(ClassifyFailureInput { gate_fail: true, ..Default::default() }), FailureCause::GateFail);
        for sig in ["SIGINT", "SIGTERM", "SIGHUP"] {
            assert_eq!(
                f(ClassifyFailureInput { signal: Some(sig), exit_code: Some(1), ..Default::default() }),
                FailureCause::Interrupted
            );
        }
        assert_eq!(f(ClassifyFailureInput { exit_code: Some(127), ..Default::default() }), FailureCause::ConfigError);
        assert_eq!(f(ClassifyFailureInput { exit_code: Some(2), ..Default::default() }), FailureCause::ConfigError);
        // 문자열 휴리스틱은 신호·코드 다음
        assert_eq!(
            f(ClassifyFailureInput { error_text: Some("reached max turns"), ..Default::default() }),
            FailureCause::MaxTurns
        );
        assert_eq!(
            f(ClassifyFailureInput { error_text: Some("max-turns exceeded"), ..Default::default() }),
            FailureCause::MaxTurns
        );
        for txt in ["ENOENT", "command not found", "not found as package", "401", "403", "unauthorized", "forbidden", "authentication failed", "permission denied", "EACCES", "unknown option", "invalid option"] {
            assert_eq!(
                f(ClassifyFailureInput { error_text: Some(txt), ..Default::default() }),
                FailureCause::ConfigError,
                "txt={}",
                txt
            );
        }
        for txt in ["ECONNRESET", "ETIMEDOUT", "ECONNREFUSED", "EAI_AGAIN", "ENETUNREACH", "socket hangup", "rate limit", "rate-limit", "429", "502", "503", "504", "network error"] {
            assert_eq!(
                f(ClassifyFailureInput { error_text: Some(txt), ..Default::default() }),
                FailureCause::TransientNetwork,
                "txt={}",
                txt
            );
        }
        // config 휴리스틱이 network보다 우선(원본 순서)
        assert_eq!(
            f(ClassifyFailureInput { error_text: Some("401 then ECONNRESET"), ..Default::default() }),
            FailureCause::ConfigError
        );
        assert_eq!(f(ClassifyFailureInput { exit_code: Some(1), ..Default::default() }), FailureCause::ToolError);
        assert_eq!(f(ClassifyFailureInput { ok: Some(false), ..Default::default() }), FailureCause::ToolError);
        assert_eq!(f(ClassifyFailureInput { exit_code: Some(0), ..Default::default() }), FailureCause::OkPass);
        assert_eq!(f(ClassifyFailureInput::default()), FailureCause::ToolError);
    }

    #[test]
    fn audit_only_and_retryable_causes() {
        assert!(is_audit_only_cause(FailureCause::ConfigError));
        assert!(is_audit_only_cause(FailureCause::Interrupted));
        for c in [FailureCause::OkPass, FailureCause::ToolError, FailureCause::TransientNetwork, FailureCause::GateFail] {
            assert!(!is_audit_only_cause(c));
        }
        assert!(is_retryable_cause(FailureCause::TransientNetwork));
        assert!(is_retryable_cause(FailureCause::ToolError));
        assert!(!is_retryable_cause(FailureCause::Timeout));
        assert!(!is_retryable_cause(FailureCause::ConfigError));
    }

    #[test]
    fn format_error_snippet_convention() {
        assert_eq!(format_error_snippet(FailureCause::OkPass, Some("x")), None);
        let s = format_error_snippet(FailureCause::ToolError, Some("boom")).unwrap();
        assert_eq!(s, "cause=tool_error; boom");
        let s2 = format_error_snippet(FailureCause::ToolError, None).unwrap();
        assert_eq!(s2, "cause=tool_error;");
        // 리댁션 + 300자 절단
        let long = "E".repeat(500);
        let s3 = format_error_snippet(FailureCause::GateFail, Some(&long)).unwrap();
        assert!(s3.starts_with("cause=gate_fail; "));
        assert_eq!(s3.chars().count(), 300);
    }

    #[test]
    fn self_invocation_detection() {
        assert!(is_self_invocation("bash run src/decision/dg.ts gate"));
        assert!(is_self_invocation("node .agents/hooks/dg-hook.ts --collect"));
        assert!(!is_self_invocation("cargo test"));
        assert!(!is_self_invocation(""));
    }

    #[test]
    fn task_kind_table_sharp_eligibility() {
        let explore = task_kind_spec("explore_read").unwrap();
        assert!(explore.sharp_eligible && !explore.requires_write && explore.default_risk == RiskLevel::Low);
        let search = task_kind_spec("code_search").unwrap();
        assert!(search.sharp_eligible);
        for kind in ["static_check", "single_edit", "multi_edit", "test_or_deploy", "swarm_fanout"] {
            assert!(!task_kind_spec(kind).unwrap().sharp_eligible, "{} must not be sharp eligible", kind);
        }
        assert_eq!(task_kind_spec("multi_edit").unwrap().default_risk, RiskLevel::High);
        assert_eq!(task_kind_spec("test_or_deploy").unwrap().requires_write, true);
        assert!(task_kind_spec("bogus_kind").is_none());
    }

    #[test]
    fn escalate_risk_only_upward() {
        assert_eq!(escalate_risk(RiskLevel::Low, None), RiskLevel::Low);
        assert_eq!(escalate_risk(RiskLevel::Low, Some(RiskLevel::High)), RiskLevel::High);
        assert_eq!(escalate_risk(RiskLevel::High, Some(RiskLevel::Low)), RiskLevel::High);
        assert_eq!(escalate_risk(RiskLevel::Medium, Some(RiskLevel::Medium)), RiskLevel::Medium);
    }

    // ---------- 사후확률·EWMA ----------

    #[test]
    fn posterior_on_empty_db_is_p0_prior() {
        let conn = mem();
        let post = get_posterior(&conn, PID, RouteId::FileSelect, "file_read").unwrap();
        assert!((post.p - 0.94).abs() < 1e-12);
        assert_eq!(post.basis, DecisionBasis::Prior);
        assert_eq!(post.samples, 0);
        assert_eq!(post.consecutive_errors, 0);
        assert_eq!(post.lambda, 0.0);
        assert_eq!(post.p_hier, 0.94);
    }

    #[test]
    fn ewma_update_math_and_basis_transition() {
        let conn = mem();
        rec(&conn, RouteId::FileSelect, "file_read", "read", true);
        let (s, fl, raw, ce) = stats(&conn, PID, RouteId::FileSelect, "file_read");
        assert!((s - 1.0).abs() < 1e-12 && fl == 0.0 && raw == 1 && ce == 0);
        // 전역 풀에도 동일 반영
        let (sg, _, rawg, _) = stats(&conn, GLOBAL_PROJECT_ID, RouteId::FileSelect, "file_read");
        assert!((sg - 1.0).abs() < 1e-12 && rawg == 1);

        rec(&conn, RouteId::FileSelect, "file_read", "read", true);
        let (s2, _, _, _) = stats(&conn, PID, RouteId::FileSelect, "file_read");
        assert!((s2 - (0.92 * 1.0 + 1.0)).abs() < 1e-12, "ewma_success={}", s2);

        // 실패 → ewma_fail 적분 + consecutive 증가
        rec(&conn, RouteId::FileSelect, "file_read", "read", false);
        let (s3, f3, raw3, ce3) = stats(&conn, PID, RouteId::FileSelect, "file_read");
        assert!((s3 - 0.92 * 1.92).abs() < 1e-12, "decay={}", s3);
        assert!((f3 - 1.0).abs() < 1e-12);
        assert_eq!(raw3, 3);
        assert_eq!(ce3, 1);
        // ok 시 연속 오류 리셋
        rec(&conn, RouteId::FileSelect, "file_read", "read", true);
        let (_, _, _, ce4) = stats(&conn, PID, RouteId::FileSelect, "file_read");
        assert_eq!(ce4, 0);
        // basis 전환: 5 표본부터 measured (현재 4건 기록됨)
        for _ in 0..1 {
            rec(&conn, RouteId::FileSelect, "file_read", "read", true);
        }
        let post = get_posterior(&conn, PID, RouteId::FileSelect, "file_read").unwrap();
        assert_eq!(post.samples, 5);
        assert_eq!(post.basis, DecisionBasis::Measured);
    }

    #[test]
    fn posterior_formula_with_global_pool_shrinkage() {
        let conn = mem();
        for _ in 0..3 {
            rec(&conn, RouteId::ToolSelect, "shell_exec", "bash", true);
        }
        let post = get_posterior(&conn, PID, RouteId::ToolSelect, "shell_exec").unwrap();
        // 공식 독립 재계산으로 검증 (§4.2~§4.3)
        let p0 = 0.74;
        let (s, f, _, _) = stats(&conn, PID, RouteId::ToolSelect, "shell_exec");
        let (sg, fg, _, _) = stats(&conn, GLOBAL_PROJECT_ID, RouteId::ToolSelect, "shell_exec");
        let lambda = ((sg + fg) / (sg + fg + 20.0)).min(LAMBDA_MAX);
        let alpha0 = 6.0 * p0;
        let p_glob = (alpha0 + sg) / (6.0 + sg + fg);
        let p_hier = (1.0 - lambda) * p0 + lambda * p_glob;
        let alpha_prior = 6.0 * p_hier;
        let expected = (alpha_prior + s) / (6.0 + s + f);
        assert!((post.p - expected).abs() < 1e-12);
        assert!((post.lambda - lambda).abs() < 1e-12);
        assert!(lambda > 0.0 && lambda <= 0.25);
        // 콜드스타트 실패 1회에도 prior 0.74를 크게 하회(저사전 확신 키 — N2)
        let conn2 = mem();
        rec(&conn2, RouteId::ToolSelect, "shell_exec", "bash", false);
        let post2 = get_posterior(&conn2, PID, RouteId::ToolSelect, "shell_exec").unwrap();
        assert!(post2.p < 0.74, "1 fail must drop below prior: {}", post2.p);
    }

    // ---------- record_tool_outcome 부가 경로 ----------

    #[test]
    fn record_dedupe_key_is_idempotent() {
        let conn = mem();
        let mut input = ri(RouteId::ToolSelect, "shell_exec", "bash", true);
        input.dedupe_key = Some("dk-1");
        input.now_ms = Some(1000);
        let r1 = record_tool_outcome(&conn, &input).unwrap();
        assert!(r1.recorded);
        let r2 = record_tool_outcome(&conn, &input).unwrap();
        assert!(!r2.recorded);
        assert_eq!(r2.reason, Some("duplicate"));
        assert_eq!(event_count(&conn), 1);
        let (_, _, raw, _) = stats(&conn, PID, RouteId::ToolSelect, "shell_exec");
        assert_eq!(raw, 1, "dedupe는 표본도 1회만 반영");
    }

    #[test]
    fn record_self_invocation_skips_everything() {
        let conn = mem();
        let mut input = ri(RouteId::ToolSelect, "shell_exec", "bash", true);
        input.command_line = Some("node src/decision/dg.ts gate --route explore_read");
        let r = record_tool_outcome(&conn, &input).unwrap();
        assert!(!r.recorded);
        assert_eq!(r.reason, Some("self-invocation"));
        assert_eq!(event_count(&conn), 0);
        assert!(stats_opt(&conn, PID).is_none());
    }

    fn stats_opt(conn: &Connection, pid: &str) -> Option<(f64, f64, i64, i64)> {
        conn.query_row(
            "SELECT ewma_success, ewma_fail, raw_samples, consecutive_errors FROM stats_summary WHERE project_id = ?1 AND route = 'tool_select' AND tool_class = 'shell_exec'",
            [pid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .unwrap()
    }

    #[test]
    fn record_retry_detection_via_session_cursor() {
        let conn = mem();
        // step 5 실패(대화 c1)
        let mut fail = ri(RouteId::ToolSelect, "shell_exec", "bash", false);
        fail.conversation_id = Some("c1");
        fail.step = Some(5);
        fail.target_hash = Some("hash-a");
        fail.now_ms = Some(1000);
        let _ = record_tool_outcome(&conn, &fail).unwrap();
        // step 7 같은 클래스(거리 2 ≤ 3) → 재시도
        let mut again = ri(RouteId::ToolSelect, "shell_exec", "bash", true);
        again.conversation_id = Some("c1");
        again.step = Some(7);
        again.now_ms = Some(2000);
        let r2 = record_tool_outcome(&conn, &again).unwrap();
        assert!(r2.is_retry);
        assert_eq!(r2.route, RouteId::RetryOrAbort);
        // retry_or_abort 라우트에 성공 1행, 원 라우트엔 실패 1개만(단일 행 귀속)
        let (_, _, raw_retry, _) = stats(&conn, PID, RouteId::RetryOrAbort, "shell_exec");
        let (_, _, raw_orig, _) = stats(&conn, PID, RouteId::ToolSelect, "shell_exec");
        assert_eq!(raw_orig, 1);
        assert_eq!(raw_retry, 1, "재시도 성공행은 retry_or_abort에 귀속");
        // is_retry=1 플래그 확인
        let flagged: i64 = conn
            .query_row("SELECT is_retry FROM decision_events WHERE route = 'retry_or_abort'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(flagged, 1);
    }

    #[test]
    fn record_retry_window_expires_and_target_match() {
        let conn = mem();
        let mut fail = ri(RouteId::ToolSelect, "shell_exec", "bash", false);
        fail.conversation_id = Some("c2");
        fail.step = Some(5);
        fail.target_hash = Some("hash-x");
        record_tool_outcome(&conn, &fail).unwrap();
        // 거리 4 > 3 → 재시도 아님
        let mut far = ri(RouteId::ToolSelect, "shell_exec", "bash", true);
        far.conversation_id = Some("c2");
        far.step = Some(9);
        let rf = record_tool_outcome(&conn, &far).unwrap();
        assert!(!rf.is_retry);

        // 다른 클래스라도 같은 target이면 재시도
        let conn2 = mem();
        let mut f2 = ri(RouteId::ToolSelect, "shell_exec", "bash", false);
        f2.conversation_id = Some("c3");
        f2.step = Some(2);
        f2.target_hash = Some("tgt-1");
        record_tool_outcome(&conn2, &f2).unwrap();
        let mut same = ri(RouteId::FileSelect, "file_read", "read", true);
        same.conversation_id = Some("c3");
        same.step = Some(3);
        same.target_hash = Some("tgt-1");
        let rs = record_tool_outcome(&conn2, &same).unwrap();
        assert!(rs.is_retry);
    }

    #[test]
    fn record_session_consec_err_and_reset() {
        let conn = mem();
        let mut f = ri(RouteId::ToolSelect, "code_search", "grep", false);
        f.conversation_id = Some("cx");
        record_tool_outcome(&conn, &f).unwrap();
        let mut f2 = ri(RouteId::FileSelect, "file_read", "read", false); // 다른 키여도 대화 카운터는 누적
        f2.conversation_id = Some("cx");
        record_tool_outcome(&conn, &f2).unwrap();
        assert_eq!(get_session_consec_err(&conn, Some("cx")).unwrap(), 2);
        // ok면 같은 대화 카운터 리셋(N7)
        let mut ok = ri(RouteId::FileSelect, "file_read", "read", true);
        ok.conversation_id = Some("cx");
        record_tool_outcome(&conn, &ok).unwrap();
        assert_eq!(get_session_consec_err(&conn, Some("cx")).unwrap(), 0);
        assert_eq!(get_session_consec_err(&conn, None).unwrap(), 0);
    }

    #[test]
    fn record_config_error_isolated_from_ewma_and_counters() {
        let conn = mem();
        let mut input = ri(RouteId::ToolSelect, "shell_exec", "bash", false);
        input.cause = Some(FailureCause::ConfigError);
        input.error_snippet = Some("command not found: agy");
        input.conversation_id = Some("cc");
        input.now_ms = Some(1234);
        let r = record_tool_outcome(&conn, &input).unwrap();
        assert!(r.recorded);
        assert_eq!(r.session_consec_err, 0, "config_error는 대화 카운터도 오염 금지");
        // 감사 로그(이벤트)는 남는다
        assert_eq!(event_count(&conn), 1);
        let (cause, snippet): (String, String) = conn
            .query_row("SELECT cause, error_snippet FROM decision_events", [], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap();
        assert_eq!(cause, "config_error");
        assert!(snippet.starts_with("cause=config_error;"));
        assert!(snippet.contains("command not found"));
        // EWMA·카운터는 격리
        assert!(stats_opt(&conn, PID).is_none());
        // interrupted도 동일 격리
        let mut i2 = ri(RouteId::ToolSelect, "shell_exec", "bash", false);
        i2.cause = Some(FailureCause::Interrupted);
        record_tool_outcome(&conn, &i2).unwrap();
        assert_eq!(event_count(&conn), 2);
        assert!(stats_opt(&conn, PID).is_none());
    }

    #[test]
    fn record_error_snippet_redacted_before_store() {
        let conn = mem();
        let secret = "MY_TOKEN_VALUE_123456";
        let mut input = ri(RouteId::ToolSelect, "shell_exec", "bash", false);
        let snippet = format!("failed: Authorization: Bearer {}", secret);
        input.cause = Some(FailureCause::ToolError);
        input.error_snippet = Some(&snippet);
        record_tool_outcome(&conn, &input).unwrap();
        let snippet: String = conn
            .query_row("SELECT error_snippet FROM decision_events", [], |r| r.get(0))
            .unwrap();
        assert!(snippet.starts_with("cause=tool_error;"));
        assert!(!snippet.contains(secret));
        assert!(snippet.contains("<redacted>"));
    }

    #[test]
    fn record_v2_cost_columns_stored() {
        let conn = mem();
        let mut input = ri(RouteId::ToolSelect, "shell_exec", "bash", true);
        input.input_tokens = Some(100);
        input.output_tokens = Some(200);
        input.cache_read_tokens = Some(50);
        input.cache_write_tokens = Some(10);
        input.cost_usd = Some(0.0123);
        input.cost_status = Some(CostStatus::Reported);
        input.source = Some(EventSource::Wrap);
        record_tool_outcome(&conn, &input).unwrap();
        let row: (i64, i64, i64, i64, f64, String, String) = conn
            .query_row(
                "SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd, cost_status, source FROM decision_events",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .unwrap();
        assert_eq!(row.0, 100);
        assert_eq!(row.1, 200);
        assert_eq!(row.2, 50);
        assert_eq!(row.3, 10);
        assert!((row.4 - 0.0123).abs() < 1e-9);
        assert_eq!(row.5, "reported");
        assert_eq!(row.6, "wrap");
        // 기본 cost_status는 'unknown'
        rec(&conn, RouteId::FileSelect, "file_read", "read", true);
        let cs: String = conn
            .query_row("SELECT cost_status FROM decision_events WHERE route = 'file_select'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cs, "unknown");
    }

    // ---------- 판정 ----------

    #[test]
    fn evaluate_forced_key_and_session_counters() {
        let conn = mem();
        let f0 = evaluate_forced(&conn, PID, RouteId::ToolSelect, "shell_exec", None).unwrap();
        assert!(!f0.forced_split && f0.forced_reason.is_none());
        rec(&conn, RouteId::ToolSelect, "shell_exec", "bash", false);
        let f1 = evaluate_forced(&conn, PID, RouteId::ToolSelect, "shell_exec", None).unwrap();
        assert!(!f1.forced_split);
        rec(&conn, RouteId::ToolSelect, "shell_exec", "bash", false);
        let f2 = evaluate_forced(&conn, PID, RouteId::ToolSelect, "shell_exec", None).unwrap();
        assert!(f2.forced_split);
        assert_eq!(f2.forced_reason, Some("consecutive_errors>=2"));
        assert!(f2.escalate_carbon);
        assert_eq!(f2.key_consecutive_errors, 2);
        // 대화 카운터 경로 — 다른 키 실패 2회(대화 동일)
        let conn2 = mem();
        for tc in ["code_search", "shell_exec"] {
            let mut i = ri(RouteId::ToolSelect, tc, "bash", false);
            i.conversation_id = Some("cs1");
            record_tool_outcome(&conn2, &i).unwrap();
        }
        let f3 = evaluate_forced(&conn2, PID, RouteId::ToolSelect, "other", Some("cs1")).unwrap();
        assert!(f3.forced_split);
        assert_eq!(f3.forced_reason, Some("session_consec_err>=2"));
        assert_eq!(f3.session_consec_err, 2);
    }

    #[test]
    fn evaluate_gate_forced_split_beats_probability() {
        let conn = mem();
        let g = evaluate_gate(
            &conn,
            &GateEvalInput {
                project_id: PID,
                route: RouteId::FileSelect,
                tool_class: "file_read",
                conversation_id: None,
                sharp_threshold: None,
            },
        )
        .unwrap();
        assert_eq!(g.decision, "sharp"); // prior 0.94 ≥ 0.70
        assert_eq!(g.sharp_threshold, 0.70);
        rec(&conn, RouteId::FileSelect, "file_read", "read", false);
        rec(&conn, RouteId::FileSelect, "file_read", "read", false);
        let g2 = evaluate_gate(
            &conn,
            &GateEvalInput {
                project_id: PID,
                route: RouteId::FileSelect,
                tool_class: "file_read",
                conversation_id: None,
                sharp_threshold: None,
            },
        )
        .unwrap();
        assert_eq!(g2.decision, "split");
        assert!(g2.forced_split);
        // 동적 임계치 우선 — 0.95면 prior 0.94로도 split
        let conn2 = mem();
        let g3 = evaluate_gate(
            &conn2,
            &GateEvalInput {
                project_id: PID,
                route: RouteId::FileSelect,
                tool_class: "file_read",
                conversation_id: None,
                sharp_threshold: Some(0.95),
            },
        )
        .unwrap();
        assert_eq!(g3.decision, "split");
        assert_eq!(g3.sharp_threshold, 0.95);
    }

    #[test]
    fn evaluate_route_four_conditions() {
        let conn = mem();
        // sharp 4조건 충족(탐색·읽기전용·low risk·p≥θ·연속오류<2)
        let r = evaluate_route(
            &conn,
            &RouteEvalInput {
                project_id: PID,
                task_kind: "explore_read",
                risk: None,
                conversation_id: None,
                sharp_threshold: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.decision, "sharp");
        assert_eq!(r.risk, RiskLevel::Low);
        // risk 상향 → split
        let r2 = evaluate_route(
            &conn,
            &RouteEvalInput {
                project_id: PID,
                task_kind: "explore_read",
                risk: Some(RiskLevel::High),
                conversation_id: None,
                sharp_threshold: None,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(r2.decision, "split");
        assert_eq!(r2.risk, RiskLevel::High);
        // sharp 비자격 → 항상 split
        for kind in ["multi_edit", "static_check", "swarm_fanout", "test_or_deploy"] {
            let rr = evaluate_route(
                &conn,
                &RouteEvalInput {
                    project_id: PID,
                    task_kind: kind,
                    risk: None,
                    conversation_id: None,
                    sharp_threshold: None,
                },
            )
            .unwrap()
            .unwrap();
            assert_eq!(rr.decision, "split", "{}", kind);
        }
        // 부정한 taskKind → None
        assert!(evaluate_route(
            &conn,
            &RouteEvalInput {
                project_id: PID,
                task_kind: "nope",
                risk: None,
                conversation_id: None,
                sharp_threshold: None,
            },
        )
        .unwrap()
        .is_none());
        // 동적 임계치
        let r3 = evaluate_route(
            &conn,
            &RouteEvalInput {
                project_id: PID,
                task_kind: "code_search",
                risk: None,
                conversation_id: None,
                sharp_threshold: Some(0.93), // prior 0.92 < 0.93
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(r3.decision, "split");
    }

    // ---------- fork 생명주기 ----------

    #[test]
    fn issue_fork_id_format_and_wait_interval() {
        let conn = mem();
        let fk = issue(&conn, RouteId::FileSelect, "file_read", 1000);
        assert!(regex::Regex::new(r"^fk_[0-9a-f]{12}$").unwrap().is_match(&fk), "{}", fk);
        let fork = get_fork(&conn, &fk).unwrap().unwrap();
        assert_eq!(fork.status, "open");
        assert_eq!(fork.generation, Some(1));
        assert_eq!(fork.attempt, Some(1));
        assert_eq!(fork.ttl_ms, TTL_EXPLORE_MS);
        assert_eq!(fork.created_at_ms, 1000);
        let meta = parse_fork_metadata(fork.metadata_json.as_deref());
        assert_eq!(meta_i64(&meta, "gen"), Some(1));
        assert_eq!(meta_i64(&meta, "attempt"), Some(1));
        // wait 구간이 같은 트랜잭션에서 열림
        assert_eq!(open_intervals(&conn, &fk), vec![(1, "wait".to_string())]);
    }

    #[test]
    fn issue_fork_custom_ttl_and_lease_metadata() {
        let conn = mem();
        let mut meta = Map::new();
        meta.insert("repo_scope".into(), json!("repo-A"));
        meta.insert("lease_owner".into(), json!("owner-1"));
        meta.insert("lease_until_ms".into(), json!(999_999));
        let fk = issue_fork(
            &conn,
            &IssueForkInput {
                project_id: PID,
                project_label: "T",
                route: RouteId::ToolSelect,
                tool_class: "shell_exec",
                decision: "split",
                p_at_gate: 0.5,
                basis_at_gate: DecisionBasis::Prior,
                forced_split: true,
                conversation_id: Some("c9"),
                ttl_ms: Some(1234),
                title: Some("title with secret sk-abcdefgh12345678"),
                metadata: Some(&meta),
                now_ms: Some(42),
            },
        )
        .unwrap();
        let fork = get_fork(&conn, &fk).unwrap().unwrap();
        assert_eq!(fork.ttl_ms, 1234);
        assert_eq!(fork.forced_split, 1);
        assert_eq!(fork.conversation_id.as_deref(), Some("c9"));
        assert_eq!(fork.lease_owner.as_deref(), Some("owner-1"));
        assert_eq!(fork.lease_expires_ms, Some(999_999));
        let m = parse_fork_metadata(fork.metadata_json.as_deref());
        assert_eq!(m.get("repo_scope").unwrap(), "repo-A");
        assert!(m.get("title").unwrap().as_str().unwrap().contains("<redacted>"));
    }

    #[test]
    fn issue_fork_half_lease_normalized_to_null_pair() {
        let conn = mem();
        let mut meta = Map::new();
        meta.insert("lease_owner".into(), json!("only-owner"));
        // lease_until_ms 없음 → CHECK 위반 방지로 둘 다 NULL
        let fk = issue_fork(
            &conn,
            &IssueForkInput {
                project_id: PID,
                project_label: "T",
                route: RouteId::FileSelect,
                tool_class: "file_read",
                decision: "split",
                p_at_gate: 0.5,
                basis_at_gate: DecisionBasis::Prior,
                forced_split: false,
                conversation_id: None,
                ttl_ms: None,
                title: None,
                metadata: Some(&meta),
                now_ms: Some(1),
            },
        )
        .unwrap();
        let fork = get_fork(&conn, &fk).unwrap().unwrap();
        assert!(fork.lease_owner.is_none());
        assert!(fork.lease_expires_ms.is_none());
    }

    #[test]
    fn touch_fork_updates_and_errors() {
        let conn = mem();
        let fk = issue(&conn, RouteId::FileSelect, "file_read", 1000);
        let (_, last, ttl) = touch_fork(&conn, &fk, Some(55_000), 2000).unwrap();
        assert_eq!(last, 2000);
        assert_eq!(ttl, 55_000);
        let fork = get_fork(&conn, &fk).unwrap().unwrap();
        assert_eq!(fork.last_touch_ms, 2000);
        // 미지정 TTL은 현행 유지
        let (_, _, ttl2) = touch_fork(&conn, &fk, None, 3000).unwrap();
        assert_eq!(ttl2, 55_000);
        assert!(touch_fork(&conn, "fk_missing000000", None, 1).is_err());
        close_fork(&conn, &fk, "ok", "worker", None, &CloseForkOptions::default()).unwrap();
        assert!(touch_fork(&conn, &fk, None, 1).is_err(), "닫힌 fork는 touch 불가");
    }

    #[test]
    fn close_fork_ok_fail_stats_and_events() {
        let conn = mem();
        let fk = issue(&conn, RouteId::FileSelect, "file_read", 1000);
        let res = close_fork(&conn, &fk, "ok", "worker", Some("all good"), &CloseForkOptions::default()).unwrap();
        assert!(res.committed);
        assert!(res.recorded);
        assert!(res.p_after > 0.0);
        let fork = get_fork(&conn, &fk).unwrap().unwrap();
        assert_eq!(fork.status, "ok");
        assert_eq!(fork.closed_by.as_deref(), Some("worker"));
        assert!(fork.closed_at_ms.is_some());
        let m = parse_fork_metadata(fork.metadata_json.as_deref());
        assert_eq!(m.get("close_reason").unwrap(), "all good");
        // EWMA + cli 이벤트 반영
        let (_, _, raw, _) = stats(&conn, PID, RouteId::FileSelect, "file_read");
        assert_eq!(raw, 1);
        let (src, tool, ok): (String, String, i64) = conn
            .query_row("SELECT source, tool_name, ok FROM decision_events", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!(src, "cli");
        assert_eq!(tool, "dg_close");
        assert_eq!(ok, 1);
        // 구간 종결
        assert!(open_intervals(&conn, &fk).is_empty());
        // 닫힌 fork 재닫기 → 오류
        assert!(close_fork(&conn, &fk, "ok", "worker", None, &CloseForkOptions::default()).is_err());
        // rerouted는 통계 미반영
        let fk2 = issue(&conn, RouteId::FileSelect, "file_read", 2000);
        let events_before = event_count(&conn);
        let res2 = close_fork(&conn, &fk2, "rerouted", "lead", None, &CloseForkOptions::default()).unwrap();
        assert!(res2.committed);
        assert!(!res2.recorded);
        assert_eq!(event_count(&conn), events_before, "rerouted는 감사 이벤트도 없음");
        let (_, _, raw2, _) = stats(&conn, PID, RouteId::FileSelect, "file_read");
        assert_eq!(raw2, 1, "rerouted는 EWMA 미반영");
        assert!(close_fork(&conn, "fk_nope00000000", "ok", "worker", None, &CloseForkOptions::default()).is_err());
    }

    #[test]
    fn close_fork_fencing_zero_toctou() {
        let conn = mem();
        let fk = issue(&conn, RouteId::ToolSelect, "shell_exec", 1000);
        // gen 펜스 불일치 → 전면 거부 (ok 포함)
        let res = close_fork(
            &conn,
            &fk,
            "ok",
            "worker",
            None,
            &CloseForkOptions { expected_gen: Some(2), ..Default::default() },
        )
        .unwrap();
        assert!(!res.committed);
        assert_eq!(res.reason, Some("stale_generation"));
        assert_eq!(get_fork(&conn, &fk).unwrap().unwrap().status, "open");
        // ok는 attempt 펜스 불일치 무시(비대칭)
        let res2 = close_fork(
            &conn,
            &fk,
            "ok",
            "worker",
            None,
            &CloseForkOptions { expected_attempt: Some(9), ..Default::default() },
        )
        .unwrap();
        assert!(res2.committed);
        // fail은 attempt 펜스 검사
        let fk2 = issue(&conn, RouteId::ToolSelect, "shell_exec", 2000);
        let res3 = close_fork(
            &conn,
            &fk2,
            "fail",
            "worker",
            None,
            &CloseForkOptions { expected_attempt: Some(2), ..Default::default() },
        )
        .unwrap();
        assert!(!res3.committed);
        assert_eq!(res3.reason, Some("stale_attempt"));
        assert_eq!(get_fork(&conn, &fk2).unwrap().unwrap().status, "open");
        let res4 = close_fork(
            &conn,
            &fk2,
            "fail",
            "worker",
            None,
            &CloseForkOptions { expected_gen: Some(1), expected_attempt: Some(1), ..Default::default() },
        )
        .unwrap();
        assert!(res4.committed);
    }

    #[test]
    fn bump_fork_attempt_syncs_columns_meta_intervals() {
        let conn = mem();
        let fk = issue(&conn, RouteId::FileSelect, "file_read", 1000);
        let (_, gen, att) = bump_fork_attempt(&conn, &fk, false, Some(2000)).unwrap();
        assert_eq!(gen, 1);
        assert_eq!(att, 2);
        let fork = get_fork(&conn, &fk).unwrap().unwrap();
        assert_eq!(fork.attempt, Some(2));
        assert_eq!(fork.generation, Some(1));
        let meta = parse_fork_metadata(fork.metadata_json.as_deref());
        assert_eq!(meta_i64(&meta, "attempt"), Some(2));
        assert_eq!(meta_i64(&meta, "gen"), Some(1));
        // 구간: attempt 1 wait 닫힘 → attempt 2 wait 열림
        let ivs = open_intervals(&conn, &fk);
        assert_eq!(ivs, vec![(2, "wait".to_string())]);
        // bumpGen
        let (_, gen2, att2) = bump_fork_attempt(&conn, &fk, true, Some(3000)).unwrap();
        assert_eq!(gen2, 2);
        assert_eq!(att2, 3);
        assert!(bump_fork_attempt(&conn, "fk_zzz000000000", false, None).is_err());
        close_fork(&conn, &fk, "ok", "worker", None, &CloseForkOptions::default()).unwrap();
        assert!(bump_fork_attempt(&conn, &fk, false, None).is_err(), "닫힌 fork는 범프 불가");
    }

    #[test]
    fn fork_interval_helpers() {
        let conn = mem();
        let fk = issue(&conn, RouteId::FileSelect, "file_read", 1000);
        // exec 구간 시작 — wait 닫히고 exec 열림(같은 attempt)
        record_fork_interval_start(
            &conn,
            &ForkIntervalArgs {
                fork_id: &fk,
                attempt: 1,
                phase: ForkIntervalPhase::Exec,
                now_ms: 1500,
            },
        )
        .unwrap();
        assert_eq!(open_intervals(&conn, &fk), vec![(1, "exec".to_string())]);
        // ensure wait — 이미 열린 구간이 있으므로 no-op
        ensure_fork_wait_interval(&conn, &fk, Some(1600)).unwrap();
        assert_eq!(open_intervals(&conn, &fk), vec![(1, "exec".to_string())]);
        // 전부 닫기
        record_fork_interval_end(&conn, &fk, Some(1700)).unwrap();
        assert!(open_intervals(&conn, &fk).is_empty());
        // 이제 wait 멱급 보장
        ensure_fork_wait_interval(&conn, &fk, Some(1800)).unwrap();
        assert_eq!(open_intervals(&conn, &fk), vec![(1, "wait".to_string())]);
        // fk_fallback_* 합성 ID는 구간 기록 없음
        record_fork_interval_start(
            &conn,
            &ForkIntervalArgs {
                fork_id: "fk_fallback_abc",
                attempt: 1,
                phase: ForkIntervalPhase::Exec,
                now_ms: 1,
            },
        )
        .unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM fork_intervals WHERE fork_id = 'fk_fallback_abc'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn settle_orphans_ttl_with_waiting_human_exemption() {
        let conn = mem();
        // ① 만료 fork
        let fk1 = issue_fork(&conn, &IssueForkInput {
            project_id: PID, project_label: "T", route: RouteId::ToolSelect, tool_class: "shell_exec",
            decision: "split", p_at_gate: 0.5, basis_at_gate: DecisionBasis::Prior, forced_split: false,
            conversation_id: None, ttl_ms: Some(1000), title: None, metadata: None, now_ms: Some(0),
        }).unwrap();
        // ② 최근 touch로 살아있는 fork
        let fk2 = issue_fork(&conn, &IssueForkInput {
            project_id: PID, project_label: "T", route: RouteId::ToolSelect, tool_class: "shell_exec",
            decision: "split", p_at_gate: 0.5, basis_at_gate: DecisionBasis::Prior, forced_split: false,
            conversation_id: None, ttl_ms: Some(60_000), title: None, metadata: None, now_ms: Some(0),
        }).unwrap();
        // 정산 시점(now) 30초 전에 touch — TTL 60초 이내라 생존
        let now = 8 * 86_400_000;
        touch_fork(&conn, &fk2, None, now - 30_000).unwrap();
        // ③ waiting_human(승인 대기 1일) — TTL 면제
        let fk3 = issue_fork(&conn, &IssueForkInput {
            project_id: PID, project_label: "T", route: RouteId::FileSelect, tool_class: "file_edit",
            decision: "split", p_at_gate: 0.5, basis_at_gate: DecisionBasis::Prior, forced_split: false,
            conversation_id: None, ttl_ms: Some(1000), title: None, metadata: None, now_ms: Some(0),
        }).unwrap();
        mark_fork_waiting_human(&conn, &fk3, "await approval", None, Some(1 * 86_400_000)).unwrap();
        // ④ waiting_human 8일 경과 — 면제 만료
        let fk4 = issue_fork(&conn, &IssueForkInput {
            project_id: PID, project_label: "T", route: RouteId::FileSelect, tool_class: "file_read",
            decision: "split", p_at_gate: 0.5, basis_at_gate: DecisionBasis::Prior, forced_split: false,
            conversation_id: None, ttl_ms: Some(1000), title: None, metadata: None, now_ms: Some(0),
        }).unwrap();
        mark_fork_waiting_human(&conn, &fk4, "stuck", None, Some(0)).unwrap();

        let res = settle_orphans(&conn, now).unwrap();
        assert_eq!(res.settled, 2, "settled: {:?}", res.fork_ids);
        assert!(res.fork_ids.contains(&fk1));
        assert!(res.fork_ids.contains(&fk4));
        assert!(!res.fork_ids.contains(&fk2));
        assert!(!res.fork_ids.contains(&fk3));
        let f1 = get_fork(&conn, &fk1).unwrap().unwrap();
        assert_eq!(f1.status, "abandoned");
        assert_eq!(f1.closed_by.as_deref(), Some("ttl"));
        let f3 = get_fork(&conn, &fk3).unwrap().unwrap();
        assert_eq!(f3.status, "open", "1일 경과 waiting_human은 면제 유지");
        // ttl_orphan 이벤트 + w=1.0 실패 표본(shell_exec 1건, file_read 1건)
        let orphan_events: i64 = conn
            .query_row("SELECT COUNT(*) FROM decision_events WHERE source = 'ttl_orphan'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(orphan_events, 2);
        let (_, f_fail, _, _) = stats(&conn, PID, RouteId::ToolSelect, "shell_exec");
        assert!((f_fail - 1.0).abs() < 1e-12);
        // 같은 시각 재정산 — 닫힌 fork·면제 중인 fork 모두 0건 (waiting_human은 정확히 7일까지 면제)
        let res2 = settle_orphans(&conn, now).unwrap();
        assert_eq!(res2.settled, 0);
        // 7일 면제 경과 직후에는 정산된다
        let res3 = settle_orphans(&conn, now + 1000).unwrap();
        assert_eq!(res3.settled, 1);
        assert!(res3.fork_ids.contains(&fk3));
    }

    // ---------- wrap 커밋 ----------

    #[test]
    fn commit_wrap_outcome_atomic_commit() {
        let conn = mem();
        let fk = issue(&conn, RouteId::ToolSelect, "shell_exec", 1000);
        let res = commit_wrap_outcome(
            &conn,
            &CommitWrapInput {
                fork_id: &fk,
                ok: true,
                outcome: "ok",
                cause: FailureCause::OkPass,
                exit_code: Some(0),
                tool_name: Some("dg_wrap"),
                now_ms: Some(2000),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(res.committed && res.closed && res.recorded);
        let fork = get_fork(&conn, &fk).unwrap().unwrap();
        assert_eq!(fork.status, "ok");
        assert_eq!(fork.closed_by.as_deref(), Some("worker"));
        let m = parse_fork_metadata(fork.metadata_json.as_deref());
        assert_eq!(m.get("cause").unwrap(), "ok_pass");
        // wrap 이벤트 + dedupe 키
        let (dk, src, ok): (String, String, i64) = conn
            .query_row("SELECT dedupe_key, source, ok FROM decision_events", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        assert_eq!(dk, format!("wrap:{}:a1", fk));
        assert_eq!(src, "wrap");
        assert_eq!(ok, 1);
        // EWMA 반영
        let (s, _, raw, _) = stats(&conn, PID, RouteId::ToolSelect, "shell_exec");
        assert!((s - 1.0).abs() < 1e-12 && raw == 1);
        // 구간 종결
        assert!(open_intervals(&conn, &fk).is_empty());
        // 지연 재커밋 → stale_or_closed
        let res2 = commit_wrap_outcome(
            &conn,
            &CommitWrapInput {
                fork_id: &fk,
                ok: true,
                outcome: "ok",
                cause: FailureCause::OkPass,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!res2.committed);
        assert_eq!(res2.reason, Some("stale_or_closed"));
    }

    #[test]
    fn commit_wrap_fencing_and_audit_isolation() {
        let conn = mem();
        let fk = issue(&conn, RouteId::ToolSelect, "shell_exec", 1000);
        // gen 펜스 거부
        let res = commit_wrap_outcome(
            &conn,
            &CommitWrapInput {
                fork_id: &fk,
                ok: false,
                outcome: "fail",
                cause: FailureCause::ToolError,
                expected_gen: Some(7),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!res.committed);
        assert_eq!(res.reason, Some("stale_generation"));
        assert_eq!(get_fork(&conn, &fk).unwrap().unwrap().status, "open");
        // attempt 펜스(fail만)
        let res2 = commit_wrap_outcome(
            &conn,
            &CommitWrapInput {
                fork_id: &fk,
                ok: false,
                outcome: "fail",
                cause: FailureCause::ToolError,
                expected_gen: Some(1),
                expected_attempt: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!res2.committed);
        assert_eq!(res2.reason, Some("stale_attempt"));
        // config_error — 커밋은 되지만 EWMA 격리
        let res3 = commit_wrap_outcome(
            &conn,
            &CommitWrapInput {
                fork_id: &fk,
                ok: false,
                outcome: "fail",
                cause: FailureCause::ConfigError,
                error_snippet: Some("command not found"),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(res3.committed);
        assert!(!res3.recorded, "config_error는 표본 미기록");
        assert!(stats_opt(&conn, PID).is_none(), "EWMA 격리");
        assert_eq!(event_count(&conn), 1);
        // rerouted도 표본 미기록
        let fk2 = issue(&conn, RouteId::FileSelect, "file_read", 2000);
        let res4 = commit_wrap_outcome(
            &conn,
            &CommitWrapInput {
                fork_id: &fk2,
                ok: false,
                outcome: "rerouted",
                cause: FailureCause::GateFail,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(res4.committed);
        assert!(!res4.recorded);
        // 존재하지 않는 fork
        let res5 = commit_wrap_outcome(
            &conn,
            &CommitWrapInput {
                fork_id: "fk_ghost00000000",
                ok: true,
                outcome: "ok",
                cause: FailureCause::OkPass,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!res5.committed);
        assert_eq!(res5.reason, Some("not_found"));
        assert_eq!(res5.p_after, 0.0);
    }

    #[test]
    fn commit_wrap_attempt_bump_changes_dedupe_key() {
        let conn = mem();
        let fk = issue(&conn, RouteId::ToolSelect, "shell_exec", 1000);
        bump_fork_attempt(&conn, &fk, false, Some(1500)).unwrap();
        let res = commit_wrap_outcome(
            &conn,
            &CommitWrapInput {
                fork_id: &fk,
                ok: false,
                outcome: "fail",
                cause: FailureCause::ToolError,
                expected_gen: Some(1),
                expected_attempt: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(res.committed);
        let dk: String = conn
            .query_row("SELECT dedupe_key FROM decision_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(dk, format!("wrap:{}:a2", fk), "dedupe 키가 attempt를 추적");
    }

    // ---------- backprop·verdict 적용 ----------

    #[test]
    fn backprop_fork_weighted_penalty_and_double_block() {
        let conn = mem();
        let fk = issue(&conn, RouteId::FileSelect, "file_read", 1000);
        let res = backprop_fork(&conn, &fk, 2.0, Some("neon says wrong"), BackpropOpts::default()).unwrap();
        assert!(res.committed);
        assert_eq!(res.weight_applied, 2.0);
        assert_eq!(res.neon_verdict, Some("fail"));
        let fork = get_fork(&conn, &fk).unwrap().unwrap();
        assert_eq!(fork.status, "backprop_fail");
        assert_eq!(fork.neon_verdict.as_deref(), Some("fail"));
        assert_eq!(fork.closed_by.as_deref(), Some("verifier"));
        let m = parse_fork_metadata(fork.metadata_json.as_deref());
        assert_eq!(m.get("penalty").unwrap().as_f64(), Some(2.0));
        assert_eq!(m.get("backprop_reason").unwrap(), "neon says wrong");
        // 가중 실패 반영(w=2)
        let (_, f, _, _) = stats(&conn, PID, RouteId::FileSelect, "file_read");
        assert!((f - 2.0).abs() < 1e-12);
        let (src, w): (String, f64) = conn
            .query_row("SELECT source, weight FROM decision_events", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(src, "neon_backprop");
        assert!((w - 2.0).abs() < 1e-12);
        // 이중 역귀속 차단 — TS와 동일하게 사전 상태 검사에서 오류(backprop_fail는 재개방 불가)
        let err = backprop_fork(&conn, &fk, 2.0, None, BackpropOpts::default()).unwrap_err();
        assert!(err.contains("cannot backprop"), "{}", err);
        assert_eq!(event_count(&conn), 1, "두 번째는 이벤트도 없음");
    }

    fn pass_doc(fk: &str) -> Value {
        json!({ "schema_version": 1, "fork_id": fk, "decision": "pass", "reason": "ok", "findings": [] })
    }

    fn fail_doc(fk: &str) -> Value {
        json!({ "schema_version": 1, "fork_id": fk, "decision": "fail", "reason": "bad",
               "findings": [{ "severity": "P0", "title": "치명", "evidence": "src/a.ts" }] })
    }

    fn rework_doc(fk: &str) -> Value {
        json!({ "schema_version": 1, "fork_id": fk, "decision": "rework", "reason": "부분 실패",
               "findings": [{ "severity": "P1", "title": "누락", "evidence": "e2e 미수행" }] })
    }

    #[test]
    fn apply_verdict_pass_closes_by_verifier() {
        let conn = mem();
        let fk = issue(&conn, RouteId::FileSelect, "file_read", 1000);
        let v = verdict_for(&pass_doc(&fk), &fk);
        assert!(v.valid);
        let res = apply_verdict_to_fork(&conn, &fk, &v, None, Some(2000)).unwrap();
        assert_eq!(res.action, "pass");
        assert_eq!(res.exit_code, 0);
        assert!(res.committed);
        let fork = get_fork(&conn, &fk).unwrap().unwrap();
        assert_eq!(fork.status, "ok");
        assert_eq!(fork.closed_by.as_deref(), Some("verifier"));
        // 멱등 재적용
        let v2 = verdict_for(&pass_doc(&fk), &fk);
        let res2 = apply_verdict_to_fork(&conn, &fk, &v2, None, Some(3000)).unwrap();
        assert_eq!(res2.action, "pass");
        assert_eq!(res2.reason.as_deref(), Some("idempotent"));
    }

    #[test]
    fn apply_verdict_fail_backprops_with_penalty_two() {
        let conn = mem();
        let fk = issue(&conn, RouteId::FileSelect, "file_read", 1000);
        let v = verdict_for(&fail_doc(&fk), &fk);
        let res = apply_verdict_to_fork(&conn, &fk, &v, None, Some(2000)).unwrap();
        assert_eq!(res.action, "backprop_fail");
        assert_eq!(res.exit_code, 0);
        let fork = get_fork(&conn, &fk).unwrap().unwrap();
        assert_eq!(fork.status, "backprop_fail");
        let m = parse_fork_metadata(fork.metadata_json.as_deref());
        assert_eq!(m.get("cause").unwrap(), "gate_fail");
        let (_, f, _, _) = stats(&conn, PID, RouteId::FileSelect, "file_read");
        assert!((f - 2.0).abs() < 1e-12, "verdict fail은 w=2.0 페널티");
        // 이미 backprop_fail이면 멱등
        let v2 = verdict_for(&fail_doc(&fk), &fk);
        let res2 = apply_verdict_to_fork(&conn, &fk, &v2, None, Some(3000)).unwrap();
        assert_eq!(res2.action, "backprop_fail");
        assert_eq!(res2.reason.as_deref(), Some("idempotent"));
    }

    #[test]
    fn apply_verdict_rework_increments_and_escalates_on_budget() {
        let conn = mem();
        let fk = issue(&conn, RouteId::FileSelect, "file_read", 1000);
        let v = verdict_for(&rework_doc(&fk), &fk);
        let res = apply_verdict_to_fork(&conn, &fk, &v, Some(2), Some(2000)).unwrap();
        assert_eq!(res.action, "rework");
        assert_eq!(res.exit_code, 0);
        let fork = get_fork(&conn, &fk).unwrap().unwrap();
        assert_eq!(fork.status, "open", "재시도를 위해 fork 유지");
        assert_eq!(fork.attempt, Some(2), "attempt 범프");
        let m = parse_fork_metadata(fork.metadata_json.as_deref());
        assert_eq!(meta_i64(&m, "rework_count"), Some(1));
        assert_eq!(m.get("cause").unwrap(), "gate_fail");
        assert!(m.get("rework_reason").is_some());
        // w=1.0 페널티 + neon 이벤트
        let (_, f, _, _) = stats(&conn, PID, RouteId::FileSelect, "file_read");
        assert!((f - 1.0).abs() < 1e-12);
        // 새 wait 구간(attempt 2)
        assert_eq!(open_intervals(&conn, &fk), vec![(2, "wait".to_string())]);
        // 두 번째 rework → 여전히 예산 내
        let v2 = verdict_for(&rework_doc(&fk), &fk);
        let res2 = apply_verdict_to_fork(&conn, &fk, &v2, Some(2), Some(3000)).unwrap();
        assert_eq!(res2.action, "rework");
        // 세 번째 → 예산 소진 → waiting_human + escalate
        let v3 = verdict_for(&rework_doc(&fk), &fk);
        let res3 = apply_verdict_to_fork(&conn, &fk, &v3, Some(2), Some(4000)).unwrap();
        assert_eq!(res3.action, "escalate_human");
        assert_eq!(res3.exit_code, 3);
        assert_eq!(res3.reason.as_deref(), Some("rework budget exhausted"));
        assert_eq!(res3.waiting_human, Some(true));
        let fork3 = get_fork(&conn, &fk).unwrap().unwrap();
        let m3 = parse_fork_metadata(fork3.metadata_json.as_deref());
        assert_eq!(m3.get("waiting_human").unwrap().as_bool(), Some(true));
        assert!(m3.get("waiting_reason").unwrap().as_str().unwrap().contains("rework budget exhausted"));
    }

    #[test]
    fn apply_verdict_invalid_fails_closed_to_human() {
        let conn = mem();
        let fk = issue(&conn, RouteId::FileSelect, "file_read", 1000);
        // 스키마 위반 문서 → fail-closed(human)
        let bad = json!({ "schema_version": 9, "fork_id": fk, "decision": "pass", "reason": "x", "findings": [] });
        let v = verdict_for(&bad, &fk);
        assert!(!v.valid);
        assert_eq!(v.effective_decision, crate::verdict::VerdictDecision::Human);
        let res = apply_verdict_to_fork(&conn, &fk, &v, None, Some(2000)).unwrap();
        assert_eq!(res.action, "escalate_human");
        assert_eq!(res.exit_code, 3);
        assert_eq!(res.waiting_human, Some(true));
        // fork는 절대 pass로 해석되지 않는다
        let fork = get_fork(&conn, &fk).unwrap().unwrap();
        assert_eq!(fork.status, "open");
        let m = parse_fork_metadata(fork.metadata_json.as_deref());
        assert_eq!(m.get("waiting_human").unwrap().as_bool(), Some(true));
        // 존재하지 않는 fork
        let v2 = verdict_for(&pass_doc("fk_ghost0000000"), "fk_ghost0000000");
        let res2 = apply_verdict_to_fork(&conn, "fk_ghost0000000", &v2, None, None).unwrap();
        assert_eq!(res2.action, "escalate_human");
        assert_eq!(res2.reason.as_deref(), Some("fork_not_found"));
    }

    // ---------- 컴팩션·통계·리스·회고 ----------

    #[test]
    fn compact_db_trims_old_events_and_closed_forks() {
        let conn = mem();
        let now = 1_000_000_000_000i64;
        let day = 86_400_000i64;
        // 31일 경과 이벤트 1건 + 최근 이벤트(cli 3, hook 1)
        for (age_ms, source) in [(40 * day, "cli"), (1 * 3600_000, "cli"), (2 * 3600_000, "cli"), (3 * 3600_000, "cli"), (1 * 3600_000, "hook")] {
            let mut i = ri(RouteId::ToolSelect, "shell_exec", "bash", true);
            i.source = Some(EventSource::parse(source).unwrap());
            i.now_ms = Some(now - age_ms);
            record_tool_outcome(&conn, &i).unwrap();
        }
        // 종결 31일 경과 fork + open fork
        let old_fk = issue(&conn, RouteId::FileSelect, "file_read", now - 40 * day);
        close_fork(&conn, &old_fk, "ok", "worker", None, &CloseForkOptions { now_ms: Some(now - 40 * day), ..Default::default() }).unwrap();
        let live_fk = issue(&conn, RouteId::FileSelect, "file_read", now - day);

        let res = compact_db(&conn, 3, 30, now).unwrap();
        assert_eq!(res.checkpoint, "TRUNCATE");
        assert!(res.deleted_events >= 1, "31일 경과 1건 이상 삭제");
        assert_eq!(res.deleted_forks, 1, "종결 31일 fork 삭제");
        assert_eq!(res.remaining_events, 4);
        // open fork 생존
        assert!(get_fork(&conn, &live_fk).unwrap().is_some());
        assert!(get_fork(&conn, &old_fk).unwrap().is_none());
        // hook 이벤트는 maxEvents 초과분에서도 보호(최근이면)
        let kept: Vec<String> = conn
            .prepare("SELECT source FROM decision_events ORDER BY event_id")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .flatten()
            .collect();
        assert!(kept.contains(&"hook".to_string()));
    }

    #[test]
    fn sample_sources_distribution() {
        let conn = mem();
        for _ in 0..2 {
            let mut i = ri(RouteId::ToolSelect, "shell_exec", "bash", true);
            i.source = Some(EventSource::Hook);
            record_tool_outcome(&conn, &i).unwrap();
        }
        let mut i = ri(RouteId::ToolSelect, "shell_exec", "bash", true);
        i.source = Some(EventSource::Cli);
        record_tool_outcome(&conn, &i).unwrap();
        let dist = sample_sources(&conn, PID, None, None, 500).unwrap();
        assert_eq!(dist[0], ("hook".to_string(), 2));
        assert_eq!(dist[1], ("cli".to_string(), 1));
        // 필터
        let dist2 = sample_sources(&conn, PID, Some(RouteId::ToolSelect), Some("shell_exec"), 500).unwrap();
        assert_eq!(dist2.len(), 2);
        let dist3 = sample_sources(&conn, PID, Some(RouteId::FileSelect), None, 500).unwrap();
        assert!(dist3.is_empty());
    }

    #[test]
    fn acquire_scope_lease_matrix() {
        let now = 10_000i64;
        let conn = mem();
        // 0) 빈 DB
        assert!(acquire_scope_lease(&conn, "repo-A", "me", None, now).unwrap().acquired);

        let mk = |conn: &Connection, scope: &str, owner: Option<&str>, until: Option<i64>, n: i64| -> String {
            let mut meta = Map::new();
            meta.insert("repo_scope".into(), json!(scope));
            if let Some(o) = owner {
                meta.insert("lease_owner".into(), json!(o));
            }
            if let Some(u) = until {
                meta.insert("lease_until_ms".into(), json!(u));
            }
            issue_fork(conn, &IssueForkInput {
                project_id: PID, project_label: "T", route: RouteId::FileSelect, tool_class: "file_read",
                decision: "split", p_at_gate: 0.5, basis_at_gate: DecisionBasis::Prior, forced_split: false,
                conversation_id: None, ttl_ms: None, title: None, metadata: Some(&meta), now_ms: Some(n),
            }).unwrap()
        };
        // 1) 유효 점유 + 생존 증명 → 거부
        mk(&conn, "repo-A", Some("other"), Some(now + 5000), 1);
        let live = vec!["other".to_string()];
        let res = acquire_scope_lease(&conn, "repo-A", "me", Some(&live), now).unwrap();
        assert!(!res.acquired);
        assert_eq!(res.holder.as_deref(), Some("other"));
        assert!(res.reason.as_deref().unwrap().contains("scope lease held by other until"));
        // 2) 만료 리스 → 탈취
        mk(&conn, "repo-B", Some("gone"), Some(now - 1), 2);
        assert!(acquire_scope_lease(&conn, "repo-B", "me", Some(&live), now).unwrap().acquired);
        // 3) 소유 세션 사망 → 탈취
        mk(&conn, "repo-C", Some("dead"), Some(now + 999_999), 3);
        assert!(acquire_scope_lease(&conn, "repo-C", "me", Some(&live), now).unwrap().acquired);
        // 4) 생존 검증 수단 없음 → fail-open 점유 무시
        mk(&conn, "repo-D", Some("anyone"), Some(now + 999_999), 4);
        assert!(acquire_scope_lease(&conn, "repo-D", "me", None, now).unwrap().acquired);
        // 5) 자기 점유는 충돌 아님
        mk(&conn, "repo-E", Some("me"), Some(now + 999_999), 5);
        assert!(acquire_scope_lease(&conn, "repo-E", "me", Some(&live), now).unwrap().acquired);
        // 6) 다른 스코프
        assert!(acquire_scope_lease(&conn, "repo-Z", "me", Some(&live), now).unwrap().acquired);
    }

    #[test]
    fn epoch_ms_to_iso_utc_format() {
        assert_eq!(epoch_ms_to_iso_utc(0), "1970-01-01T00:00:00.000Z");
        // 2026-01-01T00:00:00Z = 1767225600000
        assert_eq!(epoch_ms_to_iso_utc(1_767_225_600_000), "2026-01-01T00:00:00.000Z");
        assert_eq!(epoch_ms_to_iso_utc(86_399_999), "1970-01-01T23:59:59.999Z");
        // 윤일 경계 검증
        assert_eq!(epoch_ms_to_iso_utc(951_825_600_000), "2000-02-29T12:00:00.000Z");
        assert_eq!(epoch_ms_to_iso_utc(951_825_600_001), "2000-02-29T12:00:00.001Z");
    }

    #[test]
    fn collect_fork_retro_data_readonly() {
        let conn = mem();
        let fk = issue(&conn, RouteId::ToolSelect, "shell_exec", 1000);
        assert!(collect_fork_retro_data(&conn, "fk_none000000000", 5).unwrap().is_none());
        // 실패 도구 표본 생성 — fork에 귀속
        for _ in 0..3 {
            let mut i = ri(RouteId::ToolSelect, "shell_exec", "bash", false);
            i.fork_id = Some(&fk);
            record_tool_outcome(&conn, &i).unwrap();
        }
        let mut i2 = ri(RouteId::FileSelect, "file_read", "read", false);
        i2.fork_id = Some(&fk);
        record_tool_outcome(&conn, &i2).unwrap();
        let data = collect_fork_retro_data(&conn, &fk, 5).unwrap().unwrap();
        assert_eq!(data.fork.fork_id, fk);
        assert_eq!(data.event_count, 4);
        assert_eq!(data.top_failed_tools.len(), 2);
        assert_eq!(data.top_failed_tools[0].tool_name, "bash"); // 3회 우선
        assert_eq!(data.top_failed_tools[0].count, 3);
        assert_eq!(data.top_failed_tools[1].tool_name, "read");
        // 읽기 전용 — 표본 추가 없음
        let (_, _, raw, _) = stats(&conn, PID, RouteId::ToolSelect, "shell_exec");
        assert_eq!(raw, 3);
    }

    #[test]
    fn get_fork_gen_attempt_and_live_status() {
        let conn = mem();
        let fk = issue(&conn, RouteId::FileSelect, "file_read", 1);
        assert_eq!(get_fork_gen_attempt(&conn, &fk).unwrap(), Some((1, 1)));
        bump_fork_attempt(&conn, &fk, true, Some(2)).unwrap();
        assert_eq!(get_fork_gen_attempt(&conn, &fk).unwrap(), Some((2, 2)));
        assert!(get_fork_gen_attempt(&conn, "fk_x00").unwrap().is_none());
        assert!(is_fork_live_status("open"));
        assert!(is_fork_live_status("waiting_human"));
        for s in ["ok", "fail", "abandoned", "rerouted", "backprop_fail", "interrupted"] {
            assert!(!is_fork_live_status(s));
        }
    }

    #[test]
    fn enum_parsing_roundtrip() {
        for r in ROUTE_IDS {
            assert_eq!(RouteId::parse(r.as_str()), Some(r));
        }
        assert!(RouteId::parse("file").is_none());
        for s in EVENT_SOURCES {
            assert_eq!(EventSource::parse(s.as_str()), Some(s));
        }
        for c in FAILURE_CAUSES {
            assert_eq!(FailureCause::parse(c.as_str()), Some(c));
        }
        for t in TASK_KINDS {
            assert_eq!(TaskKind::parse(t.as_str()), Some(t));
        }
        assert!(is_route_id("delegate_swarm"));
        assert!(!is_route_id("x"));
        assert!(is_event_source("ttl_orphan"));
    }
}
