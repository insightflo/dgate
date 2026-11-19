//! 명령 구현 공용 헬퍼 — 출력·DB 연결·프로젝트 식별·θ_sharp·Decision API 플래그.
//!
//! TS `dg.ts`의 `emit`/`fallbackJson`/`openWriter`/`openReader`/`resolveIdentity`/
//! `resolveSharpTheta`/`parseDecisionApiFlags` 대응물이다.
use std::io::Write;
use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::{json, Map, Value};

use crate::args::{optional_flag, UsageError, ParsedArgs};
use dgate_core::adapter::{
    self, DecisionApiMode, DecisionAdapterInput,
};
use dgate_core::db::{self, OpenDbOptions, ProjectIdentity};
use dgate_core::engine::{self, RouteId, RiskLevel};
use dgate_core::roster;
use dgate_core::stats::{self, DynamicSharpThresholdInput};

/// 에러 모델 — Usage(exit 2) / Fatal(DB·엔진 오류 → §3.5 안전 폴백 exit 0).
#[derive(Debug)]
pub enum DgError {
    Usage(UsageError),
    Fatal(String),
}

impl From<UsageError> for DgError {
    fn from(e: UsageError) -> Self {
        DgError::Usage(e)
    }
}

pub type CmdResult = Result<i32, DgError>;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// TS `emit` — json이면 JSON, 아니면 human 한 줄.
pub fn emit(json: bool, human: String, data: &Value) {
    let mut out = std::io::stdout().lock();
    if json {
        let _ = writeln!(out, "{data}");
    } else {
        let _ = writeln!(out, "{human}");
    }
}

pub fn eprint_line(msg: &str) {
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{msg}");
}

/** `fk_fallback_` + hex 8자 — DB 무연결 폴백 fork ID(dg.ts `fallbackForkId`). */
pub fn fallback_fork_id_hex() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 4];
    rand::thread_rng().fill_bytes(&mut buf);
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("fk_fallback_{hex}")
}

/** `--db` 공용 옵션 — env(DGATE_DB_PATH 등)보다 우선(resolveDbPath 우선순위 그대로). */
pub fn db_flag(args: &ParsedArgs) -> Option<String> {
    optional_flag(args, "db")
}

/// 쓰기 연결 — busy 5000ms. 실패는 Fatal(§3.5 폴백)로 흘린다.
pub fn open_writer(db_path: Option<&str>) -> Result<Connection, DgError> {
    db::open_db_safe(
        db_path,
        OpenDbOptions { read_only: false, busy_timeout_ms: Some(5000) },
    )
    .ok_or_else(|| DgError::Fatal("decision db unavailable".to_string()))
}

/// 읽기전용 연결(stats 전용) — settle_orphans 등 쓰기 경로를 절대 타지 않는다.
pub fn open_reader(db_path: Option<&str>) -> Result<Connection, DgError> {
    db::open_db_safe(
        db_path,
        OpenDbOptions { read_only: true, busy_timeout_ms: Some(1000) },
    )
    .ok_or_else(|| DgError::Fatal("decision db unavailable".to_string()))
}

/// TS `resolveIdentity` — 디렉터리 부재는 Usage, 그 외는 core git-root 해석.
pub fn resolve_identity(project_dir: Option<&str>) -> Result<ProjectIdentity, DgError> {
    let dir = project_dir.unwrap_or(".");
    let p = PathBuf::from(dir);
    if !p.exists() {
        return Err(DgError::Usage(UsageError(format!(
            "--project directory not found: {dir}"
        ))));
    }
    Ok(db::resolve_project_identity(&p))
}

/// TS `resolveSharpTheta` — `--roster`/`--profile` 명시 시 roster 기반 동적 θ.
/// 둘 다 미지정이면 None(→ 기본 0.70 경로와 동일).
pub fn resolve_sharp_theta(args: &ParsedArgs) -> Result<Option<f64>, DgError> {
    let roster_path = optional_flag(args, "roster");
    let profile_name = optional_flag(args, "profile");
    if roster_path.is_none() && profile_name.is_none() {
        return Ok(None);
    }
    let repo_dir = optional_flag(args, "project");
    let loaded = roster::load_roster_config(
        roster_path.as_deref(),
        repo_dir.as_deref().map(PathBuf::from).as_deref(),
    );
    let name = profile_name.unwrap_or_else(|| loaded.config.default_profile.clone());
    let (_resolved_name, profile, _fb) = roster::resolve_roster_profile(&loaded.config, &name);
    Ok(Some(
        stats::compute_dynamic_sharp_threshold(&DynamicSharpThresholdInput {
            profile: &profile,
            bucket_usage: None,
            switch_cost: None,
            floor: None,
            ceil: None,
        })
        .threshold,
    ))
}

// ---------- Decision API 공용 플래그 ----------

pub struct ApiFlags {
    pub mode: DecisionApiMode,
    pub context: Option<String>,
    pub api_eval: bool,
}

/// TS `parseDecisionApiFlags` — 무효 모드는 Usage(exit 2), env는 core가 해석.
pub fn parse_decision_api_flags(args: &ParsedArgs) -> Result<ApiFlags, DgError> {
    let raw = optional_flag(args, "decision-api");
    if let Some(r) = &raw {
        if !adapter::is_decision_api_mode(r) {
            return Err(DgError::Usage(UsageError(format!(
                "invalid --decision-api \"{r}\" (allowed: off|shadow|on)"
            ))));
        }
    }
    let env = adapter::process_env_map();
    let mode_override = raw.as_deref().and_then(adapter::DecisionApiMode::parse);
    Ok(ApiFlags {
        mode: adapter::parse_decision_api_mode(&env, mode_override),
        context: optional_flag(args, "context"),
        api_eval: crate::args::bool_flag(args, "api-eval"),
    })
}

/// 사람 출력 접미 — audit가 있을 때만(off 모드 출력 100% 불변 유지).
pub fn api_suffix(audit: Option<&adapter::DecisionApiAudit>) -> String {
    let Some(a) = audit else { return String::new() };
    let inner = if a.ok {
        let p = a.p_api.map(|p| format!("p={p:.3}")).unwrap_or_else(|| "p=?".into());
        let state = if a.applied {
            "applied"
        } else if a.disagreed == Some(true) {
            "disagreed"
        } else {
            "agreed"
        };
        format!("{p},{state}")
    } else {
        "fallback".to_string()
    };
    format!(" api={}({})", a.mode.as_str(), inner)
}

pub fn audit_to_value(audit: &adapter::DecisionApiAudit) -> Value {
    serde_json::to_value(audit).unwrap_or(Value::Null)
}

/// TS `apiSuffix` 조합용 audit JSON 표기(사람 출력의 `p=…` 대응은 위 함수).
pub fn env_map() -> adapter::EnvMap {
    adapter::process_env_map()
}

/// TS `parseRisk` — low|medium|high.
pub fn parse_risk(v: Option<String>) -> Result<Option<RiskLevel>, DgError> {
    match v.as_deref() {
        None => Ok(None),
        Some("low") => Ok(Some(RiskLevel::Low)),
        Some("medium") => Ok(Some(RiskLevel::Medium)),
        Some("high") => Ok(Some(RiskLevel::High)),
        Some(other) => Err(DgError::Usage(UsageError(format!(
            "invalid --risk \"{other}\" (allowed: low|medium|high)"
        )))),
    }
}

/// TS `parseRoute` — 4개 Route enum.
pub fn parse_route(v: &str, flag_name: &str) -> Result<RouteId, DgError> {
    RouteId::parse(v).ok_or_else(|| {
        DgError::Usage(UsageError(format!(
            "invalid --{flag_name} \"{v}\" (allowed: file_select|tool_select|retry_or_abort|delegate_swarm)"
        )))
    })
}

/// `dg.ts` `fallbackJson` — 폴백 JSON 본문(cmd별 스키마 유지).
pub fn fallback_json(cmd: &str, err_msg: &str, args: &ParsedArgs) -> Value {
    let fork = optional_flag(args, "fork");
    let base = |extra: Vec<(&str, Value)>| {
        let mut m = Map::new();
        m.insert("cmd".into(), json!(cmd));
        m.insert("fallback".into(), json!(true));
        m.insert("error".into(), json!(err_msg));
        for (k, v) in extra {
            m.insert(k.into(), v);
        }
        Value::Object(m)
    };
    match cmd {
        "route" => base(vec![
            ("fork_id", json!(fallback_fork_id_hex())),
            ("task_kind", json!(optional_flag(args, "task-kind"))),
            ("decision", json!("split")),
            ("forced_split", json!(false)),
        ]),
        "gate" => base(vec![
            ("fork_id", json!(fork.clone().unwrap_or_else(fallback_fork_id_hex))),
            ("decision", json!("split")),
            ("forced_split", json!(false)),
        ]),
        "settle-orphans" => base(vec![
            ("settled", json!(0)),
            ("fork_ids", json!([])),
            (
                "ttl_policy_ms",
                json!({"explore": engine::TTL_EXPLORE_MS, "coding": engine::TTL_CODING_MS}),
            ),
        ]),
        "compact" => base(vec![
            ("deleted_events", json!(0)),
            ("deleted_forks", json!(0)),
            ("checkpoint", json!("TRUNCATE")),
            ("remaining_events", json!(0)),
        ]),
        "record" => base(vec![("recorded", json!(false))]),
        "close" => base(vec![("fork_id", json!(fork)), ("recorded", json!(false))]),
        "backprop" => base(vec![("fork_id", json!(fork)), ("status", Value::Null)]),
        "retro" => base(vec![("fork_id", json!(fork)), ("written", json!(false))]),
        "attempt" => base(vec![
            ("fork_id", json!(fork)),
            ("generation", Value::Null),
            ("attempt", Value::Null),
        ]),
        "stats" => base(vec![("cost", Value::Null), ("timing", Value::Null)]),
        "report" => base(vec![("written", json!(false))]),
        _ => match fork {
            Some(f) => base(vec![("fork_id", json!(f))]),
            None => base(vec![]),
        },
    }
}

/// `sample_sources` → JSON 배열 [[source, n], …].
pub fn sample_sources_json(
    db: &Connection,
    project_id: &str,
    route: Option<RouteId>,
    tool_class: Option<&str>,
) -> Value {
    match engine::sample_sources(db, project_id, route, tool_class, 200) {
        Ok(rows) => json!(rows
            .into_iter()
            .map(|(s, n)| json!([s, n]))
            .collect::<Vec<_>>()),
        Err(_) => json!([]),
    }
}

/// DecisionAdapterInput 조립 헬퍼(route/gate 공용).
pub fn adapter_input<'a>(
    project_id: &'a str,
    route: RouteId,
    tool_class: &'a str,
    basis: dgate_core::engine::DecisionBasis,
    p_local: f64,
    task_kind: Option<&'a str>,
    requires_write: Option<bool>,
    risk_level: Option<RiskLevel>,
    samples: Option<i64>,
    consecutive_errors: Option<i64>,
    title: Option<&'a str>,
    context: Option<&'a str>,
) -> DecisionAdapterInput<'a> {
    DecisionAdapterInput {
        project_id,
        route,
        tool_class,
        basis,
        p_local,
        task_kind,
        requires_write,
        risk_level,
        samples,
        consecutive_errors,
        title,
        context,
    }
}

pub mod prelude {
    pub use super::{
        adapter_input, api_suffix, audit_to_value, db_flag, emit, fallback_json, open_reader,
        open_writer, parse_decision_api_flags, parse_risk, parse_route, resolve_identity,
        resolve_sharp_theta, sample_sources_json, CmdResult, DgError,
    };
    pub use crate::args::{
        bool_flag, optional_flag, parse_float_flag, parse_int_flag, require_flag, ParsedArgs,
        UsageError,
    };
    pub use super::{now_ms, fallback_fork_id_hex};
}
