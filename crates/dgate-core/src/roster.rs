//! 역할↔모델 외부 설정(`roster.json`) 로더·워커 커맨드 어셈블러 —
//! `src/decision/roster.ts` (W3.5) 포팅.
//!
//! 목적: 5개 역할 슬롯(lead·explore·coder·verifier·advisor)의 CLI·모델·권한·쿼터
//! 버킷을 코드 하드코딩에서 분리해, 구독/결제 상황 변화를 설정 파일 교체만으로
//! 반영하게 한다.
//!
//! 주의:
//! - 설정 탐색 우선순위: ① 명시 `roster_path`(`--roster`) ② `HERDR_ROSTER_PATH`
//!   ③ `<repo>/.herdr/roster.json`(프로젝트 오버라이드) ④ `<XDG_CONFIG_HOME|~/.config>
//!   /herdr-quad/roster.json`(로컬 전역 기본) ⑤ 내장 기본값(quota-saver·agy-quad).
//! - 파일 프로필은 내장 기본 프로필과 **필드 단위 안전 병합**된다(부분 오버라이드 허용,
//!   미지정 슬롯은 내장 기본 상속). 깨진 JSON·스키마 위반은 경고 후 내장 폴백.
//! - 워커 실행 파일 경로는 하드코딩하지 않는다 — `resolve_worker_bin`이 설정(`bin`)·
//!   환경변수·PATH·포터블 폴백 순으로 기계 독립적으로 해석한다.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoleSlot {
    Lead,
    Explore,
    Coder,
    Verifier,
    Advisor,
}

pub const ROLE_SLOTS: [RoleSlot; 5] = [RoleSlot::Lead, RoleSlot::Explore, RoleSlot::Coder, RoleSlot::Verifier, RoleSlot::Advisor];

impl RoleSlot {
    pub fn as_str(&self) -> &'static str {
        match self {
            RoleSlot::Lead => "lead",
            RoleSlot::Explore => "explore",
            RoleSlot::Coder => "coder",
            RoleSlot::Verifier => "verifier",
            RoleSlot::Advisor => "advisor",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "lead" => Some(RoleSlot::Lead),
            "explore" => Some(RoleSlot::Explore),
            "coder" => Some(RoleSlot::Coder),
            "verifier" => Some(RoleSlot::Verifier),
            "advisor" => Some(RoleSlot::Advisor),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WorkerCliKind {
    Pi,
    Devin,
    Agy,
    Claude,
    #[default]
    Custom,
}

impl WorkerCliKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkerCliKind::Pi => "pi",
            WorkerCliKind::Devin => "devin",
            WorkerCliKind::Agy => "agy",
            WorkerCliKind::Claude => "claude",
            WorkerCliKind::Custom => "custom",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pi" => Some(WorkerCliKind::Pi),
            "devin" => Some(WorkerCliKind::Devin),
            "agy" => Some(WorkerCliKind::Agy),
            "claude" => Some(WorkerCliKind::Claude),
            "custom" => Some(WorkerCliKind::Custom),
            _ => None,
        }
    }
}

/// 비용 신뢰도 3값 (PB-2A) — 'unknown'은 절대 $0으로 합산하지 않는다
pub const COST_STATUSES: [&str; 3] = ["reported", "estimated", "unknown"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CostStatus {
    Reported,
    Estimated,
    Unknown,
}

impl CostStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            CostStatus::Reported => "reported",
            CostStatus::Estimated => "estimated",
            CostStatus::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "reported" => Some(CostStatus::Reported),
            "estimated" => Some(CostStatus::Estimated),
            "unknown" => Some(CostStatus::Unknown),
            _ => None,
        }
    }
}

/// 역할 슬롯의 선택적 비용 스펙 (PB-2A) — θ_sharp 동적 라우팅의 C_eff 입력
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RolePricingSpec {
    pub currency: Option<String>, // 'USD' | 'CNY'
    /// 백만 토큰당 입력 단가
    pub input_per_1m: Option<f64>,
    /// 백만 토큰당 출력 단가
    pub output_per_1m: Option<f64>,
    /// 백만 토큰당 캐시 읽기 단가
    pub cache_read_per_1m: Option<f64>,
    /// 백만 토큰당 캐시 쓰기 단가
    pub cache_write_per_1m: Option<f64>,
    /// 단가 미상·구독 정액 시 상대 비용 가중치
    pub relative_cost: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RoleWorkerSpec {
    #[serde(default = "default_cli")]
    pub cli: WorkerCliKind,
    /// 실행 파일 절대경로. 미지정 시 `resolve_worker_bin`이 env·PATH·폴백 순으로 해석.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<String>,
    /// 예: 'opencodex' (pi 전용)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub model: Option<String>,
    /// 예: 'max' | 'high' | 'medium' (pi 전용)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// 예: 'auto' | 'plan' | 'dangerous' — devin(claude도 사용), agy는 --mode로 매핑
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// true면 읽기전용 도구 제한 자동 적용 (pi: --tools read,grep,find,ls / agy: --mode plan)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readonly: Option<bool>,
    /// true면 상시 대기 세션 없이 필요 시 1회성 기동 (Carbon 관문 등)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_demand: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_args: Option<Vec<String>>,
    /// 쿼터 버킷 (예: 'agy', 'devin', 'opencodex'). 미지정 시 cli 종류로 추론.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_bucket: Option<String>,
    /// 선택적 비용 스펙 — 미지정 시 슬롯 기본 상대비용으로 폴백 (PB-2A)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<RolePricingSpec>,
}

fn default_cli() -> WorkerCliKind {
    WorkerCliKind::Custom
}

impl RoleWorkerSpec {
    /// 테스트·내장 프로필 구성용 축약 생성자.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cli: WorkerCliKind,
        provider: Option<&str>,
        model: Option<&str>,
        thinking: Option<&str>,
        permission_mode: Option<&str>,
        readonly: bool,
        on_demand: bool,
        quota_bucket: Option<&str>,
        relative_cost: f64,
    ) -> Self {
        RoleWorkerSpec {
            cli,
            bin: None,
            provider: provider.map(str::to_string),
            model: model.map(str::to_string),
            thinking: thinking.map(str::to_string),
            permission_mode: permission_mode.map(str::to_string),
            readonly: Some(readonly),
            on_demand: Some(on_demand),
            extra_args: None,
            quota_bucket: quota_bucket.map(str::to_string),
            pricing: Some(RolePricingSpec {
                relative_cost: Some(relative_cost),
                ..Default::default()
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RosterProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 일일 턴 예산 초과 시 강등할 프로필 (예: agy-quad → quota-saver)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_profile: Option<String>,
    /// 버킷별 일일 턴 예산 (예: { agy: 20 })
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_turn_budgets: Option<BTreeMap<String, i64>>,
    #[serde(default)]
    pub roles: HashMap<RoleSlot, RoleWorkerSpec>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RosterConfig {
    pub default_profile: String,
    pub profiles: BTreeMap<String, RosterProfile>,
}

// ---------- 워커 실행 파일 동적 해석 (기계 고유 경로 하드코딩 금지) ----------

/// CLI별 실행 파일 오버라이드 환경변수 (우선순위 순).
fn worker_bin_env_keys(cli: WorkerCliKind) -> &'static [&'static str] {
    match cli {
        WorkerCliKind::Pi => &["PI_BIN", "HERDR_PI_BIN"],
        WorkerCliKind::Devin => &["DEVIN_BIN"],
        WorkerCliKind::Agy => &["AGY_BIN"],
        WorkerCliKind::Claude => &["CLAUDE_BIN"],
        WorkerCliKind::Custom => &[],
    }
}

fn which_bin(name: &str) -> Option<String> {
    let out = std::process::Command::new("which").arg(name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    s.lines().next().map(str::to_string).filter(|l| !l.is_empty())
}

/// 포터블 기본 폴백 — 관례적 설치 위치에 실재하면 절대경로, 아니면 PATH에 맡긴다.
fn portable_default_bin(cli: WorkerCliKind) -> String {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    match cli {
        WorkerCliKind::Pi => {
            let b = home.join(".bun").join("bin").join("pi");
            if b.exists() {
                b.to_string_lossy().into_owned()
            } else {
                "pi".to_string()
            }
        }
        WorkerCliKind::Agy => {
            let b = home.join(".local").join("bin").join("agy");
            if b.exists() {
                b.to_string_lossy().into_owned()
            } else {
                "agy".to_string()
            }
        }
        other => other.as_str().to_string(), // devin·claude
    }
}

/// 워커 CLI 실행 파일 해석 — 우선순위:
/// ① `spec_bin`(roster.json `bin` 필드) ② 환경변수(`PI_BIN`/`HERDR_PI_BIN`·`DEVIN_BIN`·
/// `AGY_BIN`·`CLAUDE_BIN`) ③ PATH 탐색(`which`) ④ 포터블 기본 폴백.
/// `custom`은 해석 대상이 아니므로 호출자가 `spec.bin` 유지다.
pub fn resolve_worker_bin(cli: WorkerCliKind, spec_bin: Option<&str>) -> String {
    if let Some(b) = spec_bin.map(str::trim).filter(|s| !s.is_empty()) {
        return b.to_string();
    }
    for key in worker_bin_env_keys(cli) {
        if let Some(v) = std::env::var(key).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            return v;
        }
    }
    if let Some(on_path) = which_bin(cli.as_str()) {
        return on_path;
    }
    portable_default_bin(cli)
}

// ---------- 내장 기본 프로필 (기존 하드코딩 동작의 SSOT) ----------

fn builtin_quota_saver() -> RosterProfile {
    let mut roles = HashMap::new();
    roles.insert(
        RoleSlot::Lead,
        RoleWorkerSpec::new(WorkerCliKind::Pi, Some("opencodex"), Some("glm-zcode/glm-5.3"), Some("max"), None, false, false, None, 5.0),
    );
    roles.insert(
        RoleSlot::Explore,
        RoleWorkerSpec::new(WorkerCliKind::Pi, Some("opencodex"), Some("command-code/deepseek-deepseek-v4.1-flash"), Some("medium"), None, true, false, None, 1.0),
    );
    roles.insert(
        RoleSlot::Coder,
        RoleWorkerSpec::new(WorkerCliKind::Devin, None, None, None, Some("auto"), false, false, None, 3.0),
    );
    roles.insert(
        RoleSlot::Verifier,
        RoleWorkerSpec::new(WorkerCliKind::Pi, Some("opencodex"), Some("command-code/deepseek-deepseek-v4.1-flash"), Some("max"), None, true, false, None, 1.5),
    );
    roles.insert(
        RoleSlot::Advisor,
        RoleWorkerSpec::new(WorkerCliKind::Pi, Some("opencodex"), Some("glm-zcode/glm-5.3"), Some("high"), None, true, true, Some("pi"), 8.0),
    );
    RosterProfile {
        description: Some(
            "기본 프로필 — agy 0 (상시·온디맨드 모두), Lead/Verifier/Advisor 모두 pi, Advisor는 온디맨드 1회성".into(),
        ),
        fallback_profile: None,
        daily_turn_budgets: Some(BTreeMap::from([("agy".to_string(), 0)])),
        roles,
    }
}

fn builtin_agy_quad() -> RosterProfile {
    let mut roles = HashMap::new();
    roles.insert(
        RoleSlot::Lead,
        RoleWorkerSpec::new(WorkerCliKind::Agy, None, Some("gemini-4-argon-high"), None, Some("auto"), false, false, None, 5.0),
    );
    roles.insert(
        RoleSlot::Explore,
        RoleWorkerSpec::new(WorkerCliKind::Pi, Some("opencodex"), Some("command-code/deepseek-deepseek-v4.1-flash"), Some("medium"), None, true, false, None, 1.0),
    );
    roles.insert(
        RoleSlot::Coder,
        RoleWorkerSpec::new(WorkerCliKind::Devin, None, None, None, Some("auto"), false, false, None, 3.0),
    );
    roles.insert(
        RoleSlot::Verifier,
        RoleWorkerSpec::new(WorkerCliKind::Agy, None, Some("gemini-4-neon-high"), None, None, true, false, None, 1.5),
    );
    roles.insert(
        RoleSlot::Advisor,
        RoleWorkerSpec::new(WorkerCliKind::Agy, None, Some("gemini-4-carbon-high"), None, None, true, false, None, 8.0),
    );
    RosterProfile {
        description: Some("선택 프로필 — Argon(lead)/Carbon(advisor)/Neon(verifier) agy 3세션 상시".into()),
        fallback_profile: Some("quota-saver".into()),
        daily_turn_budgets: Some(BTreeMap::from([("agy".to_string(), 20)])),
        roles,
    }
}

/// 내장 기본 설정(딥카피 없이 값 생성 — 호출자 mutation 으로 내부 상태 오염 없음).
pub fn builtin_roster_config() -> RosterConfig {
    let mut profiles = BTreeMap::new();
    profiles.insert("quota-saver".to_string(), builtin_quota_saver());
    profiles.insert("agy-quad".to_string(), builtin_agy_quad());
    RosterConfig {
        default_profile: "quota-saver".to_string(),
        profiles,
    }
}

// ---------- 로딩·병합 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterSourceKind {
    Flag,
    Env,
    Project,
    Global,
    Builtin,
}

impl RosterSourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RosterSourceKind::Flag => "flag",
            RosterSourceKind::Env => "env",
            RosterSourceKind::Project => "project",
            RosterSourceKind::Global => "global",
            RosterSourceKind::Builtin => "builtin",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedRoster {
    pub config: RosterConfig,
    pub source: RosterSourceKind,
    pub source_path: Option<String>,
    /// 비치명 문제(깨진 JSON·스키마 위반·프로필 부재 등)
    pub warnings: Vec<String>,
}

/// 전역 기본 roster 경로: `$XDG_CONFIG_HOME/herdr-quad/roster.json` (없으면 `~/.config/...`).
pub fn global_roster_path() -> PathBuf {
    let xdg = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let base = match xdg {
        Some(x) => PathBuf::from(x),
        None => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".config"),
    };
    base.join("herdr-quad").join("roster.json")
}

/// JS `path.resolve` 근사 — 상대경로면 현재 작업 디렉터리 기준 절대경로화.
fn resolve_path(p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(path)
    }
}

fn merge_spec_value(base: &Value, over: &Value) -> Value {
    let mut out = base.clone();
    if let (Some(b), Some(o)) = (out.as_object_mut(), over.as_object()) {
        for (k, v) in o {
            if !v.is_null() {
                b.insert(k.clone(), v.clone());
            }
        }
    }
    out
}

/// 파일 프로필을 내장 기본 위에 병합: 동일 이름 프로필은 필드 단위 오버라이드,
/// 새 이름 프로필은 내장 `default_profile`(quota-saver)을 베이스로 상속한다.
fn merge_roster_file(
    builtin: &RosterConfig,
    file: &Value,
    source_path: &str,
    warnings: &mut Vec<String>,
) -> Result<RosterConfig, String> {
    // 병합은 값(JSON) 수준에서 수행한다 — TS의 "키 존재 여부 기반 필드 병합"과 동일 계약.
    let builtin_value = serde_json::to_value(builtin).map_err(|e| e.to_string())?;
    let mut merged = builtin_value
        .as_object()
        .ok_or_else(|| "builtin config must be an object".to_string())?
        .clone();
    let mut merged_profiles = merged
        .get("profiles")
        .and_then(|v| v.as_object())
        .cloned()
        .ok_or_else(|| "builtin profiles must be an object".to_string())?;
    let inherit_base = merged_profiles
        .get(&builtin.default_profile)
        .cloned()
        .unwrap_or_else(|| {
            serde_json::json!({ "roles": {
                "lead": { "cli": "custom" }, "explore": { "cli": "custom" }, "coder": { "cli": "custom" },
                "verifier": { "cli": "custom" }, "advisor": { "cli": "custom" } } })
        });

    let file_profiles = match file.get("profiles").cloned().unwrap_or_else(|| serde_json::json!({})) {
        Value::Object(m) => m,
        Value::Null => serde_json::Map::new(),
        other => {
            let kind = if other.is_array() { "array" } else { "other" };
            return Err(format!("profiles must be an object (got {})", kind));
        }
    };
    for (name, raw) in &file_profiles {
        if !raw.is_object() {
            warnings.push(format!("roster profile \"{}\" is not an object — skipped ({})", name, source_path));
            continue;
        }
        // 슬롯 필터: ROLE_SLOTS에 있는 객체 형태 슬롯만 채택
        let roles_input = raw.get("roles").cloned().unwrap_or_else(|| serde_json::json!({}));
        let mut roles = serde_json::Map::new();
        if let Some(roles_map) = roles_input.as_object() {
            for slot in ROLE_SLOTS {
                if let Some(spec) = roles_map.get(slot.as_str()) {
                    if spec.is_object() {
                        roles.insert(slot.as_str().to_string(), spec.clone());
                    }
                }
            }
        }
        if roles.is_empty() {
            warnings.push(format!(
                "roster profile \"{}\" has no valid role slots — quota-saver defaults inherited ({})",
                name, source_path
            ));
        }
        let mut over = raw.clone();
        over.as_object_mut().unwrap().insert("roles".into(), Value::Object(roles));
        // 프로필 필드 단위 병합
        let base_profile = merged_profiles.get(name).cloned().unwrap_or_else(|| inherit_base.clone());
        let mut prof = base_profile;
        if let (Some(p), Some(o)) = (prof.as_object_mut(), over.as_object()) {
            for (k, v) in o {
                match k.as_str() {
                    "roles" => {
                        if let Some(base_roles) = p.get("roles").and_then(|r| r.as_object()).cloned() {
                            let mut new_roles = base_roles;
                            if let Some(over_roles) = v.as_object() {
                                for (slot, over_spec) in over_roles {
                                    let base_spec = new_roles
                                        .get(slot)
                                        .cloned()
                                        .unwrap_or_else(|| serde_json::json!({ "cli": "custom" }));
                                    new_roles.insert(slot.clone(), merge_spec_value(&base_spec, over_spec));
                                }
                            }
                            p.insert("roles".into(), Value::Object(new_roles));
                        } else {
                            p.insert(k.clone(), v.clone());
                        }
                    }
                    _ => {
                        p.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        merged_profiles.insert(name.clone(), prof);
    }

    let mut default_profile = builtin.default_profile.clone();
    if let Some(file_default) = file.get("defaultProfile").and_then(|v| v.as_str()) {
        if !file_default.trim().is_empty() {
            if merged_profiles.contains_key(file_default) {
                default_profile = file_default.to_string();
            } else {
                warnings.push(format!(
                    "roster defaultProfile \"{}\" not defined — builtin \"{}\" kept ({})",
                    file_default, builtin.default_profile, source_path
                ));
            }
        }
    }
    merged.insert("profiles".into(), Value::Object(merged_profiles));
    merged.insert("defaultProfile".into(), Value::String(default_profile));
    serde_json::from_value(Value::Object(merged)).map_err(|e| format!("roster schema violation: {}", e))
}

/// roster 설정 로딩(우선순위 탐색 + 내장 병합 + 안전 폴백).
/// - 존재하는 후보가 깨졌으면(파싱·스키마 실패) 경고 후 **내장 기본값으로 즉시 폴백**.
/// - 경로가 지정됐지만 파일이 없으면 경고만 남기고 다음 후보로 진행한다.
pub fn load_roster_config(roster_path: Option<&str>, repo_dir: Option<&Path>) -> LoadedRoster {
    load_roster_config_inner(roster_path, repo_dir, false)
}

/// 경고를 stderr에도 출력할지 선택하는 내부 진입점(라이브러리는 조용히, CLI가 출력).
pub fn load_roster_config_quiet(roster_path: Option<&str>, repo_dir: Option<&Path>) -> LoadedRoster {
    load_roster_config_inner(roster_path, repo_dir, true)
}

fn load_roster_config_inner(roster_path: Option<&str>, repo_dir: Option<&Path>, quiet: bool) -> LoadedRoster {
    let mut warnings: Vec<String> = Vec::new();
    let builtin = builtin_roster_config();
    let candidates: Vec<(RosterSourceKind, Option<PathBuf>)> = vec![
        (
            RosterSourceKind::Flag,
            roster_path
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(resolve_path),
        ),
        (
            RosterSourceKind::Env,
            std::env::var("HERDR_ROSTER_PATH")
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .map(|v| resolve_path(&v)),
        ),
        (
            RosterSourceKind::Project,
            repo_dir.map(|d| d.join(".herdr").join("roster.json")),
        ),
        (RosterSourceKind::Global, Some(global_roster_path())),
    ];

    for (kind, path_opt) in candidates {
        let Some(path) = path_opt else { continue };
        if !path.exists() {
            if matches!(kind, RosterSourceKind::Flag | RosterSourceKind::Env) {
                warnings.push(format!("roster path not found ({}): {}", kind.as_str(), path.display()));
            }
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!(
                    "roster load failed ({}: {}): {} — falling back to builtin defaults",
                    kind.as_str(),
                    path.display(),
                    e
                );
                warnings.push(msg.clone());
                if !quiet {
                    eprintln!("[roster] {}", msg);
                }
                return LoadedRoster {
                    config: builtin,
                    source: RosterSourceKind::Builtin,
                    source_path: Some(path.to_string_lossy().into_owned()),
                    warnings,
                };
            }
        };
        let raw: Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                let msg = format!(
                    "roster load failed ({}: {}): {} — falling back to builtin defaults",
                    kind.as_str(),
                    path.display(),
                    e
                );
                warnings.push(msg.clone());
                if !quiet {
                    eprintln!("[roster] {}", msg);
                }
                return LoadedRoster {
                    config: builtin,
                    source: RosterSourceKind::Builtin,
                    source_path: Some(path.to_string_lossy().into_owned()),
                    warnings,
                };
            }
        };
        if !raw.is_object() {
            let msg = format!(
                "roster load failed ({}: {}): root must be an object (got {}) — falling back to builtin defaults",
                kind.as_str(),
                path.display(),
                if raw.is_array() { "array" } else { "other" }
            );
            warnings.push(msg.clone());
            if !quiet {
                eprintln!("[roster] {}", msg);
            }
            return LoadedRoster {
                config: builtin,
                source: RosterSourceKind::Builtin,
                source_path: Some(path.to_string_lossy().into_owned()),
                warnings,
            };
        }
        let source_path_str = path.to_string_lossy().into_owned();
        return match merge_roster_file(&builtin, &raw, &source_path_str, &mut warnings) {
            Ok(config) => LoadedRoster {
                config,
                source: kind,
                source_path: Some(source_path_str),
                warnings,
            },
            Err(e) => {
                let msg = format!(
                    "roster load failed ({}: {}): {} — falling back to builtin defaults",
                    kind.as_str(),
                    path.display(),
                    e
                );
                warnings.push(msg.clone());
                if !quiet {
                    eprintln!("[roster] {}", msg);
                }
                LoadedRoster {
                    config: builtin,
                    source: RosterSourceKind::Builtin,
                    source_path: Some(source_path_str),
                    warnings,
                }
            }
        };
    }
    LoadedRoster {
        config: builtin,
        source: RosterSourceKind::Builtin,
        source_path: None,
        warnings,
    }
}

/// 프로필 이름 해석 — 부재 시 defaultProfile → 내장 quota-saver 순으로 폴백.
pub fn resolve_roster_profile(config: &RosterConfig, name: &str) -> (String, RosterProfile, bool) {
    if let Some(p) = config.profiles.get(name) {
        return (name.to_string(), p.clone(), false);
    }
    if let Some(def) = config.profiles.get(&config.default_profile) {
        return (config.default_profile.clone(), def.clone(), true);
    }
    let builtin = builtin_roster_config();
    (
        "quota-saver".to_string(),
        builtin.profiles["quota-saver"].clone(),
        true,
    )
}

// ---------- 쿼터 버킷 집계 (하드코딩 제거 — 실제 roles에서 동적 계산) ----------

/// 스펙의 실효 쿼터 버킷: 명시 우선, 없으면 cli 종류로 추론(pi→opencodex, custom→미정).
pub fn effective_quota_bucket(spec: &RoleWorkerSpec) -> Option<String> {
    if let Some(q) = &spec.quota_bucket {
        return Some(q.clone());
    }
    match spec.cli {
        WorkerCliKind::Pi => Some("opencodex".to_string()),
        WorkerCliKind::Custom => None,
        other => Some(other.as_str().to_string()),
    }
}

/// 상시 대기 세션 수 = 버킷 소속 역할 중 `on_demand != true` 개수.
pub fn count_standing_sessions_by_bucket(profile: &RosterProfile, bucket: &str) -> usize {
    ROLE_SLOTS
        .iter()
        .filter(|slot| {
            profile
                .roles
                .get(slot)
                .map(|spec| effective_quota_bucket(spec).as_deref() == Some(bucket) && spec.on_demand != Some(true))
                .unwrap_or(false)
        })
        .count()
}

/// 버킷 최대 동시(온디맨드 포함) 한도 = 버킷 소속 역할 총 개수.
pub fn count_max_ondemand_by_bucket(profile: &RosterProfile, bucket: &str) -> usize {
    ROLE_SLOTS
        .iter()
        .filter(|slot| {
            profile
                .roles
                .get(slot)
                .map(|spec| effective_quota_bucket(spec).as_deref() == Some(bucket))
                .unwrap_or(false)
        })
        .count()
}

// ---------- 워커 커맨드 어셈블리 ----------

#[derive(Debug, Clone, Default)]
pub struct BuildWorkerCommandOptions<'a> {
    pub prompt: &'a str,
    /// 프롬프트의 `{brief}` 플레이스홀더를 이 경로로 치환한다.
    pub role_brief_path: Option<&'a str>,
    /// true면 초기 프롬프트 없이 대기형 세션으로 기동(기본 false).
    pub interactive: bool,
}

fn effective_prompt(opts: &BuildWorkerCommandOptions) -> String {
    if opts.interactive {
        return String::new();
    }
    if let (Some(brief), true) = (opts.role_brief_path, opts.prompt.contains("{brief}")) {
        return opts.prompt.replace("{brief}", brief);
    }
    opts.prompt.to_string()
}

/// 역할 스펙 → 워커 CLI argv.
/// - `pi`: `--no-extensions --no-mcp --no-approve` 항상 포함. `readonly`면
///   `--tools read,grep,find,ls` 자동 적용(§6.1 Layer 3). 사용자 roster가 `extraArgs`로
///   임의 `--tools`를 지정하면 그 값을 존중하되 `--exclude-tools edit,write`를 자동 붙인다.
/// - `devin`: 기본 `--permission-mode auto`, `--` 뒤에 프롬프트.
/// - `agy`: `--model <m> --mode <plan|auto>` (`readonly`면 plan), 프롬프트는 `-i`로만.
/// - `claude`/`custom`: bin + model + extraArgs + 프롬프트 순서로 조립.
pub fn build_worker_command(spec: &RoleWorkerSpec, opts: &BuildWorkerCommandOptions) -> Vec<String> {
    let prompt = effective_prompt(opts);
    let extras: Vec<String> = spec.extra_args.clone().unwrap_or_default();

    match spec.cli {
        WorkerCliKind::Pi => {
            let mut argv = vec![resolve_worker_bin(WorkerCliKind::Pi, spec.bin.as_deref())];
            if let Some(p) = &spec.provider {
                argv.push("--provider".into());
                argv.push(p.clone());
            }
            if let Some(m) = &spec.model {
                argv.push("--model".into());
                argv.push(m.clone());
            }
            if let Some(t) = &spec.thinking {
                argv.push("--thinking".into());
                argv.push(t.clone());
            }
            let has_tools = extras.iter().any(|a| a == "--tools");
            if spec.readonly == Some(true) && !has_tools {
                argv.push("--tools".into());
                argv.push("read,grep,find,ls".into());
            }
            argv.extend(extras.iter().cloned());
            if spec.readonly == Some(true) && has_tools && !extras.iter().any(|a| a == "--exclude-tools") {
                argv.push("--exclude-tools".into());
                argv.push("edit,write".into());
            }
            argv.push("--no-extensions".into());
            argv.push("--no-mcp".into());
            argv.push("--no-approve".into());
            if !prompt.is_empty() {
                argv.push(prompt);
            }
            argv
        }
        WorkerCliKind::Devin => {
            let mut argv = vec![resolve_worker_bin(WorkerCliKind::Devin, spec.bin.as_deref())];
            argv.push("--permission-mode".into());
            argv.push(spec.permission_mode.clone().unwrap_or_else(|| "auto".into()));
            argv.extend(extras.iter().cloned());
            argv.push("--".into());
            if !prompt.is_empty() {
                argv.push(prompt);
            }
            argv
        }
        WorkerCliKind::Agy => {
            let mut argv = vec![resolve_worker_bin(WorkerCliKind::Agy, spec.bin.as_deref())];
            if let Some(m) = &spec.model {
                argv.push("--model".into());
                argv.push(m.clone());
            }
            let mode = if spec.readonly == Some(true) {
                "plan"
            } else if spec.permission_mode.as_deref() == Some("plan") {
                "plan"
            } else {
                "auto"
            };
            argv.push("--mode".into());
            argv.push(mode.into());
            argv.push("--dangerously-skip-permissions".into());
            argv.extend(extras.iter().cloned());
            argv.push("-i".into());
            if !prompt.is_empty() {
                argv.push(prompt);
            }
            argv
        }
        WorkerCliKind::Claude => {
            let mut argv = vec![resolve_worker_bin(WorkerCliKind::Claude, spec.bin.as_deref())];
            if let Some(m) = &spec.model {
                argv.push("--model".into());
                argv.push(m.clone());
            }
            if let Some(pm) = &spec.permission_mode {
                argv.push("--permission-mode".into());
                argv.push(pm.clone());
            }
            argv.extend(extras.iter().cloned());
            if !prompt.is_empty() {
                argv.push(prompt);
            }
            argv
        }
        WorkerCliKind::Custom => {
            let mut argv = vec![spec.bin.clone().unwrap_or_else(|| "true".into())];
            if let Some(m) = &spec.model {
                argv.push("--model".into());
                argv.push(m.clone());
            }
            if let Some(pm) = &spec.permission_mode {
                argv.push("--permission-mode".into());
                argv.push(pm.clone());
            }
            argv.extend(extras.iter().cloned());
            if !prompt.is_empty() {
                argv.push(prompt);
            }
            argv
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{env_lock, temp_dir};

    fn pi_spec(readonly: bool) -> RoleWorkerSpec {
        RoleWorkerSpec {
            cli: WorkerCliKind::Pi,
            bin: Some("/opt/pi".into()),
            provider: Some("opencodex".into()),
            model: Some("m/x".into()),
            thinking: Some("max".into()),
            permission_mode: None,
            readonly: Some(readonly),
            on_demand: None,
            extra_args: None,
            quota_bucket: None,
            pricing: None,
        }
    }

    #[test]
    fn builtin_quota_saver_defaults_match_v1() {
        let cfg = builtin_roster_config();
        assert_eq!(cfg.default_profile, "quota-saver");
        assert!(cfg.profiles.contains_key("agy-quad"));
        let qs = &cfg.profiles["quota-saver"];
        assert_eq!(qs.daily_turn_budgets.as_ref().unwrap()["agy"], 0);
        let lead = &qs.roles[&RoleSlot::Lead];
        assert_eq!(lead.cli, WorkerCliKind::Pi);
        assert_eq!(lead.provider.as_deref(), Some("opencodex"));
        assert_eq!(lead.model.as_deref(), Some("glm-zcode/glm-5.3"));
        assert_eq!(lead.thinking.as_deref(), Some("max"));
        assert_eq!(lead.pricing.as_ref().unwrap().relative_cost, Some(5.0));
        let explore = &qs.roles[&RoleSlot::Explore];
        assert_eq!(explore.cli, WorkerCliKind::Pi);
        assert_eq!(explore.model.as_deref(), Some("command-code/deepseek-deepseek-v4.1-flash"));
        assert_eq!(explore.thinking.as_deref(), Some("medium"));
        assert_eq!(explore.readonly, Some(true));
        let coder = &qs.roles[&RoleSlot::Coder];
        assert_eq!(coder.cli, WorkerCliKind::Devin);
        assert_eq!(coder.permission_mode.as_deref(), Some("auto"));
        let verifier = &qs.roles[&RoleSlot::Verifier];
        assert_eq!(verifier.cli, WorkerCliKind::Pi);
        assert_eq!(verifier.thinking.as_deref(), Some("max"));
        assert_eq!(verifier.readonly, Some(true));
        let advisor = &qs.roles[&RoleSlot::Advisor];
        assert_eq!(advisor.cli, WorkerCliKind::Pi);
        assert_eq!(advisor.thinking.as_deref(), Some("high"));
        assert_eq!(advisor.on_demand, Some(true));
        assert_eq!(advisor.quota_bucket.as_deref(), Some("pi"));
    }

    #[test]
    fn builtin_agy_quad_matches_v1() {
        let cfg = builtin_roster_config();
        let aq = &cfg.profiles["agy-quad"];
        assert_eq!(aq.fallback_profile.as_deref(), Some("quota-saver"));
        assert_eq!(aq.daily_turn_budgets.as_ref().unwrap()["agy"], 20);
        assert_eq!(aq.roles[&RoleSlot::Lead].cli, WorkerCliKind::Agy);
        assert_eq!(aq.roles[&RoleSlot::Lead].model.as_deref(), Some("gemini-4-argon-high"));
        assert_eq!(aq.roles[&RoleSlot::Verifier].model.as_deref(), Some("gemini-4-neon-high"));
        assert_eq!(aq.roles[&RoleSlot::Advisor].model.as_deref(), Some("gemini-4-carbon-high"));
        assert_eq!(aq.roles[&RoleSlot::Coder].cli, WorkerCliKind::Devin);
    }

    #[test]
    fn load_priority_flag_over_env_over_project() {
        let _g = env_lock();
        let tmp = temp_dir("roster-prio");
        std::fs::create_dir_all(&tmp).unwrap();
        let flag = tmp.join("flag.json");
        let env = tmp.join("env.json");
        let project_dir = tmp.join("repo");
        std::fs::create_dir_all(project_dir.join(".herdr")).unwrap();
        let project = project_dir.join(".herdr/roster.json");
        for (p, desc) in [(&flag, "FROM-FLAG"), (&env, "FROM-ENV"), (&project, "FROM-PROJECT")] {
            std::fs::write(
                p,
                format!("{{\"profiles\": {{\"custom-p\": {{\"description\": \"{}\"}}}}}}", desc),
            )
            .unwrap();
        }
        std::env::remove_var("HERDR_ROSTER_PATH");
        std::env::remove_var("XDG_CONFIG_HOME");

        // flag 최우선
        let loaded = load_roster_config(Some(flag.to_str().unwrap()), Some(&project_dir));
        assert_eq!(loaded.source, RosterSourceKind::Flag);
        assert!(loaded.config.profiles["custom-p"].description.as_deref() == Some("FROM-FLAG"));

        // env가 다음
        std::env::set_var("HERDR_ROSTER_PATH", env.to_str().unwrap());
        let loaded2 = load_roster_config(None, Some(&project_dir));
        assert_eq!(loaded2.source, RosterSourceKind::Env);
        assert!(loaded2.config.profiles["custom-p"].description.as_deref() == Some("FROM-ENV"));

        // flag가 없는 경로면 경고 후 다음 후보(env)로 진행
        let loaded3 = load_roster_config(Some("/nonexistent/roster.json"), Some(&project_dir));
        assert_eq!(loaded3.source, RosterSourceKind::Env);
        assert!(loaded3.warnings.iter().any(|w| w.contains("roster path not found (flag)")));

        // env 제거 → project
        std::env::remove_var("HERDR_ROSTER_PATH");
        let loaded4 = load_roster_config(None, Some(&project_dir));
        assert_eq!(loaded4.source, RosterSourceKind::Project);
        assert!(loaded4.config.profiles["custom-p"].description.as_deref() == Some("FROM-PROJECT"));

        // project도 없으면 global(XDG로 격리) → builtin
        std::env::set_var("XDG_CONFIG_HOME", tmp.join("xdg").to_str().unwrap());
        let loaded5 = load_roster_config(None, Some(&tmp.join("empty-repo")));
        assert_eq!(loaded5.source, RosterSourceKind::Builtin);
        assert_eq!(loaded5.source_path, None);
        assert_eq!(loaded5.config.default_profile, "quota-saver");
        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn broken_json_falls_back_to_builtin_with_warning() {
        let _g = env_lock();
        std::env::remove_var("HERDR_ROSTER_PATH");
        let tmp = temp_dir("roster-broken");
        std::fs::create_dir_all(&tmp).unwrap();
        let bad = tmp.join("bad.json");
        std::fs::write(&bad, "{broken json!!").unwrap();
        let loaded = load_roster_config(Some(bad.to_str().unwrap()), None);
        assert_eq!(loaded.source, RosterSourceKind::Builtin);
        assert_eq!(loaded.config.default_profile, "quota-saver");
        assert!(loaded.warnings.iter().any(|w| w.contains("falling back to builtin defaults")));
        // 루트가 배열이어도 폴백
        let arr = tmp.join("arr.json");
        std::fs::write(&arr, "[1,2]").unwrap();
        let loaded2 = load_roster_config(Some(arr.to_str().unwrap()), None);
        assert_eq!(loaded2.source, RosterSourceKind::Builtin);
        // profiles가 배열이어도 폴백
        let bad_profiles = tmp.join("bp.json");
        std::fs::write(&bad_profiles, "{\"profiles\": [1]}").unwrap();
        let loaded3 = load_roster_config(Some(bad_profiles.to_str().unwrap()), None);
        assert_eq!(loaded3.source, RosterSourceKind::Builtin);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn partial_profile_merges_over_builtin_defaults() {
        let _g = env_lock();
        std::env::remove_var("HERDR_ROSTER_PATH");
        std::env::remove_var("XDG_CONFIG_HOME");
        let tmp = temp_dir("roster-merge");
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("roster.json");
        // quota-saver의 coder만 devin→agy로 오버라이드. 새 프로필은 quota-saver 상속.
        std::fs::write(
            &file,
            serde_json::json!({
                "profiles": {
                    "quota-saver": { "roles": { "coder": { "cli": "agy", "model": "gemini-4-argon-high" } } },
                    "my-profile": { "roles": { "lead": { "cli": "claude", "model": "c-1" } } },
                    "invalid-slots": { "description": "no roles" },
                    "not-object": "x"
                }
            })
            .to_string(),
        )
        .unwrap();
        let loaded = load_roster_config(Some(file.to_str().unwrap()), None);
        assert_eq!(loaded.source, RosterSourceKind::Flag);
        let qs = &loaded.config.profiles["quota-saver"];
        assert_eq!(qs.roles[&RoleSlot::Coder].cli, WorkerCliKind::Agy);
        assert_eq!(qs.roles[&RoleSlot::Coder].model.as_deref(), Some("gemini-4-argon-high"));
        // 미지정 슬롯은 내장 상속
        assert_eq!(qs.roles[&RoleSlot::Lead].cli, WorkerCliKind::Pi);
        assert_eq!(qs.roles[&RoleSlot::Lead].model.as_deref(), Some("glm-zcode/glm-5.3"));
        // 새 프로필은 quota-saver 베이스 + lead만 claude
        let my = &loaded.config.profiles["my-profile"];
        assert_eq!(my.roles[&RoleSlot::Lead].cli, WorkerCliKind::Claude);
        assert_eq!(my.roles[&RoleSlot::Lead].model.as_deref(), Some("c-1"));
        assert_eq!(my.roles[&RoleSlot::Explore].cli, WorkerCliKind::Pi); // 상속
        // 슬롯 없는 프로필 경고 + 객체 아닌 프로필 스킵 경고
        assert!(loaded.warnings.iter().any(|w| w.contains("has no valid role slots")));
        assert!(loaded.warnings.iter().any(|w| w.contains("is not an object — skipped")));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn default_profile_override_and_warning() {
        let _g = env_lock();
        std::env::remove_var("HERDR_ROSTER_PATH");
        std::env::remove_var("XDG_CONFIG_HOME");
        let tmp = temp_dir("roster-default");
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("r.json");
        std::fs::write(&file, serde_json::json!({ "defaultProfile": "agy-quad" }).to_string()).unwrap();
        let loaded = load_roster_config(Some(file.to_str().unwrap()), None);
        assert_eq!(loaded.config.default_profile, "agy-quad");

        let file2 = tmp.join("r2.json");
        std::fs::write(&file2, serde_json::json!({ "defaultProfile": "ghost" }).to_string()).unwrap();
        let loaded2 = load_roster_config(Some(file2.to_str().unwrap()), None);
        assert_eq!(loaded2.config.default_profile, "quota-saver");
        assert!(loaded2.warnings.iter().any(|w| w.contains("roster defaultProfile \"ghost\" not defined")));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_roster_profile_fallbacks() {
        let cfg = builtin_roster_config();
        let (n1, _, fb1) = resolve_roster_profile(&cfg, "agy-quad");
        assert_eq!(n1, "agy-quad");
        assert!(!fb1);
        let (n2, _, fb2) = resolve_roster_profile(&cfg, "no-such");
        assert_eq!(n2, "quota-saver");
        assert!(fb2);
    }

    #[test]
    fn effective_quota_bucket_inference() {
        let mut spec = RoleWorkerSpec { cli: WorkerCliKind::Pi, ..pi_spec(false) };
        assert_eq!(effective_quota_bucket(&spec).as_deref(), Some("opencodex"));
        spec.cli = WorkerCliKind::Devin;
        assert_eq!(effective_quota_bucket(&spec).as_deref(), Some("devin"));
        spec.cli = WorkerCliKind::Custom;
        assert_eq!(effective_quota_bucket(&spec), None);
        spec.quota_bucket = Some("agy".into());
        assert_eq!(effective_quota_bucket(&spec).as_deref(), Some("agy"));
    }

    #[test]
    fn standing_sessions_by_bucket() {
        let cfg = builtin_roster_config();
        let qs = &cfg.profiles["quota-saver"];
        // pi CLI의 추론 버킷은 opencodex: lead+explore+verifier 상시 = 3
        assert_eq!(count_standing_sessions_by_bucket(qs, "opencodex"), 3);
        assert_eq!(count_max_ondemand_by_bucket(qs, "opencodex"), 3);
        // advisor는 명시 quotaBucket=pi + onDemand — pi 버킷 상시 0
        assert_eq!(count_standing_sessions_by_bucket(qs, "pi"), 0);
        assert_eq!(count_max_ondemand_by_bucket(qs, "pi"), 1);
        // devin 버킷: coder 1
        assert_eq!(count_standing_sessions_by_bucket(qs, "devin"), 1);
        let aq = &cfg.profiles["agy-quad"];
        // agy 버킷: lead+verifier+advisor = 3 (전원 상시)
        assert_eq!(count_standing_sessions_by_bucket(aq, "agy"), 3);
        assert_eq!(count_max_ondemand_by_bucket(aq, "agy"), 3);
    }

    #[test]
    fn readonly_pi_gets_tool_restrictions() {
        let spec = pi_spec(true);
        let argv = build_worker_command(&spec, &BuildWorkerCommandOptions { prompt: "do it", ..Default::default() });
        assert_eq!(argv[0], "/opt/pi");
        let tools_idx = argv.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(argv[tools_idx + 1], "read,grep,find,ls");
        assert!(argv.contains(&"--no-extensions".to_string()));
        assert!(argv.contains(&"--no-mcp".to_string()));
        assert!(argv.contains(&"--no-approve".to_string()));
        assert_eq!(*argv.last().unwrap(), "do it");
        assert!(!argv.iter().any(|a| a == "--exclude-tools"));
    }

    #[test]
    fn readonly_pi_with_custom_tools_gets_exclude_guard() {
        let mut spec = pi_spec(true);
        spec.extra_args = Some(vec!["--tools".into(), "read,bash".into()]);
        let argv = build_worker_command(&spec, &BuildWorkerCommandOptions { prompt: "p", ..Default::default() });
        assert!(argv.windows(2).any(|w| w[0] == "--tools" && w[1] == "read,bash"));
        let ex_idx = argv.iter().position(|a| a == "--exclude-tools").unwrap();
        assert_eq!(argv[ex_idx + 1], "edit,write");
        // 사용자가 이미 --exclude-tools를 줬으면 중복 부착 없음
        let mut spec2 = pi_spec(true);
        spec2.extra_args = Some(vec!["--tools".into(), "read".into(), "--exclude-tools".into(), "bash".into()]);
        let argv2 = build_worker_command(&spec2, &BuildWorkerCommandOptions { prompt: "p", ..Default::default() });
        assert_eq!(argv2.iter().filter(|a| *a == "--exclude-tools").count(), 1);
        assert_eq!(argv2[argv2.iter().position(|a| *a == "--exclude-tools").unwrap() + 1], "bash");
    }

    #[test]
    fn writable_pi_has_no_tool_flags() {
        let spec = pi_spec(false);
        let argv = build_worker_command(&spec, &BuildWorkerCommandOptions { prompt: "p", ..Default::default() });
        assert!(!argv.iter().any(|a| a == "--tools"));
        assert!(!argv.iter().any(|a| a == "--exclude-tools"));
    }

    #[test]
    fn devin_command_shape() {
        let mut spec = RoleWorkerSpec { cli: WorkerCliKind::Devin, bin: Some("/opt/devin".into()), ..Default::default() };
        spec.permission_mode = None; // 기본 auto
        let argv = build_worker_command(&spec, &BuildWorkerCommandOptions { prompt: "fix it", ..Default::default() });
        assert_eq!(argv, vec!["/opt/devin", "--permission-mode", "auto", "--", "fix it"]);
        spec.permission_mode = Some("dangerous".into());
        let argv2 = build_worker_command(&spec, &BuildWorkerCommandOptions { prompt: "fix", ..Default::default() });
        assert_eq!(argv2[2], "dangerous");
        assert_eq!(argv2[3], "--");
    }

    #[test]
    fn agy_command_mode_and_prompt_flag() {
        let mut spec = RoleWorkerSpec { cli: WorkerCliKind::Agy, bin: Some("/opt/agy".into()), ..Default::default() };
        spec.model = Some("gemini-4-neon-high".into());
        spec.readonly = Some(true);
        let argv = build_worker_command(&spec, &BuildWorkerCommandOptions { prompt: "verify", ..Default::default() });
        let mode_idx = argv.iter().position(|a| a == "--mode").unwrap();
        assert_eq!(argv[mode_idx + 1], "plan");
        assert!(argv.contains(&"--dangerously-skip-permissions".to_string()));
        assert_eq!(*argv.last().unwrap(), "verify");
        assert_eq!(argv[argv.len() - 2], "-i"); // 프롬프트는 반드시 -i로

        spec.readonly = Some(false);
        spec.permission_mode = Some("auto".into());
        let argv2 = build_worker_command(&spec, &BuildWorkerCommandOptions { prompt: "go", ..Default::default() });
        let mode_idx2 = argv2.iter().position(|a| a == "--mode").unwrap();
        assert_eq!(argv2[mode_idx2 + 1], "auto");
    }

    #[test]
    fn claude_and_custom_command_shapes() {
        let spec = RoleWorkerSpec {
            cli: WorkerCliKind::Claude,
            bin: Some("/opt/claude".into()),
            model: Some("m1".into()),
            permission_mode: Some("plan".into()),
            ..Default::default()
        };
        let argv = build_worker_command(&spec, &BuildWorkerCommandOptions { prompt: "hi", ..Default::default() });
        assert_eq!(argv, vec!["/opt/claude", "--model", "m1", "--permission-mode", "plan", "hi"]);

        let custom = RoleWorkerSpec { cli: WorkerCliKind::Custom, ..Default::default() };
        let argv2 = build_worker_command(&custom, &BuildWorkerCommandOptions { prompt: "hi", ..Default::default() });
        assert_eq!(argv2, vec!["true", "hi"]); // bin 없으면 true
    }

    #[test]
    fn interactive_and_brief_substitution() {
        let spec = pi_spec(false);
        let argv = build_worker_command(
            &spec,
            &BuildWorkerCommandOptions { prompt: "ignored", interactive: true, ..Default::default() },
        );
        assert_eq!(*argv.last().unwrap(), "--no-approve"); // 프롬프트 없음

        let argv2 = build_worker_command(
            &spec,
            &BuildWorkerCommandOptions {
                prompt: "follow {brief} now",
                role_brief_path: Some(".herdr/roles/lead.md"),
                ..Default::default()
            },
        );
        assert_eq!(*argv2.last().unwrap(), "follow .herdr/roles/lead.md now");
        // {brief} 없으면 치환 없음
        let argv3 = build_worker_command(
            &spec,
            &BuildWorkerCommandOptions { prompt: "plain", role_brief_path: Some("x"), ..Default::default() },
        );
        assert_eq!(*argv3.last().unwrap(), "plain");
    }

    #[test]
    fn resolve_worker_bin_priority() {
        let _g = env_lock();
        // ① spec_bin 최우선
        assert_eq!(resolve_worker_bin(WorkerCliKind::Pi, Some(" /my/pi ")), "/my/pi");
        std::env::remove_var("PI_BIN");
        std::env::remove_var("HERDR_PI_BIN");
        // ② env 오버라이드
        std::env::set_var("PI_BIN", "/env/pi");
        assert_eq!(resolve_worker_bin(WorkerCliKind::Pi, None), "/env/pi");
        std::env::remove_var("PI_BIN");
        // HERDR_PI_BIN은 2순위
        std::env::set_var("HERDR_PI_BIN", "/env2/pi");
        assert_eq!(resolve_worker_bin(WorkerCliKind::Pi, None), "/env2/pi");
        std::env::remove_var("HERDR_PI_BIN");
        // ③/④ — which·포터블 폴백 중 하나(빈 문자열만 아니면 된다)
        let resolved = resolve_worker_bin(WorkerCliKind::Pi, None);
        assert!(!resolved.is_empty());
    }

    #[test]
    fn global_roster_path_respects_xdg() {
        let _g = env_lock();
        std::env::set_var("XDG_CONFIG_HOME", "/cfg");
        assert_eq!(global_roster_path(), PathBuf::from("/cfg/herdr-quad/roster.json"));
        std::env::remove_var("XDG_CONFIG_HOME");
        let p = global_roster_path();
        assert!(p.ends_with("herdr-quad/roster.json"));
        assert!(p.is_absolute());
    }
}
