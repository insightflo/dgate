//! `gemini-decision-1` REST 어댑터 + 모드별 적용 정책 — `src/decision/adapter.ts` 포팅.
//!
//! 목적: 콜드스타트(`basis='prior'`) 앞단 분류에 외부 Decision 모델(`POST /v1beta/decisions`)
//! 결과를 `off | shadow | on` 3단계로 접목한다. `off`는 로컬 EWMA 완결(네트워크 0),
//! `shadow`는 병행 호출 후 가상 판정만 기록(적용 0), `on`은 하드 안전 가드를 통과한
//! API 판정을 실제 적용한다.
//!
//! 주의:
//! - 인증은 `x-goog-api-key` 헤더 전용 — URL `?key=` 쿼리 절대 금지, 키는 로그·반환·
//!   DB 어디에도 기록하지 않는다(오류 문자열은 `redact_secrets` + 명시 키 스크럽 2중 적용).
//! - `GEMINI_DECISION_API_KEY` 부재·공백이면 `shadow`/`on` 요청도 `off`로 강등(무비용·무I/O).
//! - `on` 모드도 `basis='measured'`이면 `force_api_eval` 없이는 API를 호출하지 않는다.
//! - 하드 안전 가드는 절대 우회 불가: `sharp_eligible`·`escalate_risk`(상향만)·`forced_split`.
//! - 모든 네트워크·타임아웃·HTTP·JSON·스키마 오류는 fail-safe: 패닉 없이 구조화 실패
//!   (`ok:false`)와 로컬 판정 폴백을 반환한다.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use crate::engine::{escalate_risk, DecisionBasis, RiskLevel, RouteId};
use crate::redact::redact_secrets;
use crate::roster::CostStatus;

/// 환경 변수 조회용 맵 — 테스트 주입 가능한 `Record<string, string|undefined>` 대응
pub type EnvMap = HashMap<String, String>;

/// 현재 프로세스 환경을 [`EnvMap`]으로 수집한다.
pub fn process_env_map() -> EnvMap {
    std::env::vars().collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionApiMode {
    Off,
    Shadow,
    On,
}

impl DecisionApiMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            DecisionApiMode::Off => "off",
            DecisionApiMode::Shadow => "shadow",
            DecisionApiMode::On => "on",
        }
    }
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "off" => Some(DecisionApiMode::Off),
            "shadow" => Some(DecisionApiMode::Shadow),
            "on" => Some(DecisionApiMode::On),
            _ => None,
        }
    }
}

pub fn is_decision_api_mode(v: &str) -> bool {
    DecisionApiMode::parse(v).is_some()
}

// ---------- 환경 설정 ----------

#[derive(Debug, Clone, PartialEq)]
pub struct DecisionApiConfig {
    pub base_url: String,
    pub model: String,
    pub timeout_ms: u64,
    pub api_key: String,
}

pub const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
pub const DEFAULT_MODEL: &str = "gemini-decision-1";
pub const DEFAULT_TIMEOUT_MS: u64 = 3000;
pub const TIMEOUT_MIN_MS: u64 = 50;
pub const TIMEOUT_MAX_MS: u64 = 30000;
/// 입력 토큰 단가 ($ / 1M tokens) — output_tokens=0 규약
pub const INPUT_TOKEN_PRICE_PER_M: f64 = 0.10;
/// 외부 API로 보내는 context 최대 길이 (리댁션 적용 후 절단)
pub const CONTEXT_MAX_CHARS: usize = 4000;
/// 오류 문자열 상한
pub const ERROR_MAX_CHARS: usize = 240;

fn env_get<'a>(env: &'a EnvMap, key: &str) -> Option<&'a str> {
    env.get(key).map(String::as_str)
}

/// JS `parseInt` 근사 — 선행 정수부만 취급(후행 쓰레기 무시), 실패 시 None
fn parse_int_prefix(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let mut end = 0;
    let bytes = s.as_bytes();
    if end < bytes.len() && (bytes[end] == b'+' || bytes[end] == b'-') {
        end += 1;
    }
    let digit_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == digit_start {
        return None;
    }
    s[..end].parse::<i64>().ok()
}

pub fn resolve_decision_api_config(env: &EnvMap) -> DecisionApiConfig {
    let base_url = env_get(env, "DECISION_API_BASE_URL")
        .unwrap_or(DEFAULT_BASE_URL)
        .trim()
        .trim_end_matches('/')
        .to_string();
    let model = match env_get(env, "DECISION_API_MODEL").map(str::trim) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => DEFAULT_MODEL.to_string(),
    };
    let timeout_ms = env_get(env, "DECISION_API_TIMEOUT_MS")
        .and_then(parse_int_prefix)
        .map(|t| (t.max(TIMEOUT_MIN_MS as i64)).min(TIMEOUT_MAX_MS as i64) as u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    let api_key = env_get(env, "GEMINI_DECISION_API_KEY").map(str::trim).unwrap_or("").to_string();
    DecisionApiConfig { base_url, model, timeout_ms, api_key }
}

/// `DECISION_API` 파싱 + 무키 강등 규칙.
/// - `override_mode`가 주어지면(`--decision-api` CLI 값) 그 값을 우선한다.
/// - 정상값(`shadow`|`on`)이 아니면 `off`.
/// - `shadow`/`on`이어도 `GEMINI_DECISION_API_KEY` 부재·공백 시 `off` (네트워크 I/O·비용 0).
pub fn parse_decision_api_mode(env: &EnvMap, override_mode: Option<DecisionApiMode>) -> DecisionApiMode {
    let raw = match override_mode {
        Some(m) => m.as_str().to_string(),
        None => env_get(env, "DECISION_API").unwrap_or("off").to_string(),
    };
    let raw = raw.trim().to_lowercase();
    let Some(mode) = DecisionApiMode::parse(&raw) else {
        return DecisionApiMode::Off;
    };
    if mode == DecisionApiMode::Off {
        return DecisionApiMode::Off;
    }
    match env_get(env, "GEMINI_DECISION_API_KEY").map(str::trim) {
        Some(key) if !key.is_empty() => mode,
        _ => DecisionApiMode::Off,
    }
}

// ---------- 요청 프롬프트 / 본문 ----------

#[derive(Debug, Clone, Copy)]
pub struct DecisionAdapterInput<'a> {
    pub project_id: &'a str,
    pub route: RouteId,
    pub tool_class: &'a str,
    pub basis: DecisionBasis,
    pub p_local: f64,
    pub task_kind: Option<&'a str>,
    pub requires_write: Option<bool>,
    pub risk_level: Option<RiskLevel>,
    pub samples: Option<i64>,
    pub consecutive_errors: Option<i64>,
    pub title: Option<&'a str>,
    pub context: Option<&'a str>,
}

impl<'a> Default for DecisionAdapterInput<'a> {
    fn default() -> Self {
        DecisionAdapterInput {
            project_id: "",
            route: RouteId::FileSelect,
            tool_class: "",
            basis: DecisionBasis::Prior,
            p_local: 0.0,
            task_kind: None,
            requires_write: None,
            risk_level: None,
            samples: None,
            consecutive_errors: None,
            title: None,
            context: None,
        }
    }
}

/// 외부 API `input` 프롬프트 — title·context는 반드시 `redact_secrets`를 거친 뒤
/// 전송한다(절단보다 리댁션 선행 — 경계 걸친 시크릿도 탐지되도록).
pub fn build_decision_prompt_input(input: &DecisionAdapterInput) -> String {
    let mut lines: Vec<String> = vec![
        format!("route: {}", input.route.as_str()),
        format!("tool_class: {}", input.tool_class),
    ];
    if let Some(kind) = input.task_kind.filter(|k| !k.is_empty()) {
        lines.push(format!("task_kind: {}", kind));
    }
    if let Some(rw) = input.requires_write {
        lines.push(format!("requires_write: {}", rw));
    }
    if let Some(risk) = input.risk_level {
        lines.push(format!("risk_level: {}", risk.as_str()));
    }
    lines.push(format!("local_p: {}", input.p_local));
    lines.push(format!("local_basis: {}", input.basis.as_str()));
    lines.push(format!("samples: {}", input.samples.unwrap_or(0)));
    lines.push(format!("consecutive_errors: {}", input.consecutive_errors.unwrap_or(0)));
    if let Some(title) = input.title.filter(|t| !t.is_empty()) {
        lines.push(format!("title: {}", redact_secrets(title)));
    }
    if let Some(ctx) = input.context.filter(|c| !c.is_empty()) {
        let redacted = redact_secrets(ctx);
        lines.push(format!("context: {}", redacted.chars().take(CONTEXT_MAX_CHARS).collect::<String>()));
    }
    lines.join("\n")
}

/// 공식 Decisions REST 질문 스키마 — noul criteria는 live 검증된
/// `true_description`/`false_description` 키만 쓴다('true'/'false' 금지).
pub fn build_decision_request_body(model: &str, prompt_input: &str) -> Value {
    json!({
        "model": model,
        "input": prompt_input,
        "questions": {
            "should_fast_path": {
                "type": "noul",
                "instructions": "Estimate the probability that this step succeeds cleanly on a single-turn fast-path without multi-step orchestration.",
                "criteria": {
                    "true_description": "Simple, bounded, read-only or low-risk operation with high likelihood of first-try success.",
                    "false_description": "Ambiguous, write-heavy, high-risk, multi-file, or error-prone operation requiring structured planning or verification."
                }
            },
            "risk_tier": {
                "type": "choice",
                "instructions": "Classify the operational risk tier of this operation.",
                "criteria": {
                    "low": "Read-only inspection, search, or isolated non-destructive check.",
                    "medium": "Bounded single-file edit, local config change, or moderate blast radius.",
                    "high": "Multi-file refactor, migration, security/auth change, deployment, or destructive action."
                }
            }
        }
    })
}

// ---------- 응답 파싱 ----------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DecisionApiDetailedResult {
    pub ok: bool,
    pub p: Option<f64>,
    pub confidence: Option<f64>,
    pub risk_tier: Option<RiskLevel>,
    pub input_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub cost_status: CostStatus,
    pub latency_ms: u64,
    pub model: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecisionAdapterResult {
    pub p: f64,
    pub source: DecisionSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionSource {
    Local,
    Api,
}

fn as_record(v: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    v.and_then(|v| v.as_object())
}

fn is_finite_num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|n| n.is_finite())
}

/// JSON 수치가 정수(소수부 0·범위 내)인지 — TS `Number.isInteger` 대응
fn as_integer(v: Option<&Value>) -> Option<i64> {
    let n = is_finite_num(v)?;
    if n.fract() != 0.0 || n < i64::MIN as f64 || n > i64::MAX as f64 {
        return None;
    }
    Some(n as i64)
}

/// `answers.should_fast_path.noul` 필수 유효성 + 선택 필드 추출.
/// noul이 유한수가 아니면 오류를 반환한다(호출자가 `ok:false`로 변환).
fn parse_decision_api_answers(data: &Value) -> Result<DecisionApiDetailedResult, String> {
    let root = data.as_object();
    let answers = as_record(root.and_then(|r| r.get("answers")));
    let sfp = as_record(answers.and_then(|a| a.get("should_fast_path")));
    let invalid = || "invalid_response: missing should_fast_path.noul".to_string();
    let Some(noul) = sfp.and_then(|s| s.get("noul")).and_then(Value::as_f64).filter(|n| n.is_finite()) else {
        return Err(invalid());
    };
    let confidence = match is_finite_num(sfp.and_then(|s| s.get("confidence"))) {
        Some(c) if (0.0..=1.0).contains(&c) => Some(c),
        _ => None,
    };
    let rt = as_record(answers.and_then(|a| a.get("risk_tier")));
    let risk_tier = rt
        .and_then(|r| r.get("choice"))
        .and_then(Value::as_str)
        .and_then(RiskLevel::parse);
    let usage = as_record(root.and_then(|r| r.get("usage")));
    let usage_meta = as_record(root.and_then(|r| r.get("usage_metadata")));
    // ?? 체인: 첫 non-nullish 값을 취하고 그 값 자체를 검증한다(타입 불량은 폴백 안 함)
    let it = usage
        .and_then(|u| u.get("total_input_tokens"))
        .or_else(|| usage.and_then(|u| u.get("input_tokens")))
        .or_else(|| usage_meta.and_then(|u| u.get("input_tokens")))
        .or_else(|| usage_meta.and_then(|u| u.get("total_input_tokens")))
        .filter(|v| !v.is_null());
    let input_tokens = as_integer(it).filter(|t| *t >= 0);
    let cost_usd = input_tokens.map(|t| round8(t as f64 * INPUT_TOKEN_PRICE_PER_M / 1_000_000.0));
    Ok(DecisionApiDetailedResult {
        ok: true,
        p: Some(noul.clamp(0.01, 0.99)),
        confidence,
        risk_tier,
        input_tokens,
        cost_usd,
        cost_status: if input_tokens.is_some() { CostStatus::Estimated } else { CostStatus::Unknown },
        latency_ms: 0,
        model: String::new(),
        error: None,
    })
}

/// TS `Number(x.toFixed(8))` — 소수 8자리로 반올림 후 재구성
fn round8(x: f64) -> f64 {
    (x * 1e8).round() / 1e8
}

/// `encodeURIComponent` 대응 — 예약 외 문자는 그대로, 나머지는 %XX로 인코딩
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// 명시 API 키 스크럽 — 커스텀 테스트 키가 표준 토큰 정규식에 걸리지 않는 경우를 대비
fn scrub_api_key(text: &str, api_key: &str) -> String {
    if api_key.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    if out.contains(api_key) {
        out = out.replace(api_key, "<redacted>");
    }
    let encoded = encode_uri_component(api_key);
    if encoded != api_key && out.contains(&encoded) {
        out = out.replace(&encoded, "<redacted>");
    }
    out
}

fn sanitize_api_error(msg: &str, api_key: &str) -> String {
    redact_secrets(&scrub_api_key(msg, api_key)).chars().take(ERROR_MAX_CHARS).collect()
}

// ---------- 어댑터 구현 ----------

pub trait DecisionAdapter {
    fn classify(&self, input: &DecisionAdapterInput) -> Option<DecisionAdapterResult>;
    fn classify_detailed(&self, input: &DecisionAdapterInput) -> DecisionApiDetailedResult;
    fn model_name(&self) -> String;
}

/// 공식 Decisions REST 호출 구현 — `POST ${base_url}/v1beta/decisions`.
/// 인증은 `x-goog-api-key` 헤더 전용이다(`?key=` 쿼리 금지). 모든 실패는
/// `ok:false` 구조화 결과로 변환해 호출 경로를 죽이지 않는다(fail-safe).
pub struct GeminiDecisionAdapter {
    config: DecisionApiConfig,
}

impl GeminiDecisionAdapter {
    pub fn new(config: DecisionApiConfig) -> Self {
        GeminiDecisionAdapter { config }
    }
}

impl DecisionAdapter for GeminiDecisionAdapter {
    fn classify(&self, input: &DecisionAdapterInput) -> Option<DecisionAdapterResult> {
        let res = self.classify_detailed(input);
        match (res.ok, res.p) {
            (true, Some(p)) => Some(DecisionAdapterResult { p, source: DecisionSource::Api }),
            _ => None,
        }
    }

    fn classify_detailed(&self, input: &DecisionAdapterInput) -> DecisionApiDetailedResult {
        let started = Instant::now();
        let DecisionApiConfig { base_url, model, timeout_ms, api_key } = &self.config;
        let fail = |msg: String| DecisionApiDetailedResult {
            ok: false,
            p: None,
            confidence: None,
            risk_tier: None,
            input_tokens: None,
            cost_usd: None,
            cost_status: CostStatus::Unknown,
            latency_ms: started.elapsed().as_millis() as u64,
            model: model.clone(),
            error: Some(sanitize_api_error(&msg, api_key)),
        };
        let body = match serde_json::to_string(&build_decision_request_body(model, &build_decision_prompt_input(input))) {
            Ok(b) => b,
            Err(e) => return fail(e.to_string()),
        };
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_millis(*timeout_ms)).build();
        let outcome = agent
            .post(&format!("{}/v1beta/decisions", base_url))
            .set("Content-Type", "application/json")
            .set("x-goog-api-key", api_key)
            .send_string(&body);
        let text = match outcome {
            Ok(resp) => match resp.into_string() {
                Ok(t) => t,
                Err(_) => return fail("invalid_response: malformed_json".to_string()),
            },
            Err(ureq::Error::Status(code, resp)) => {
                let body_text = resp.into_string().unwrap_or_default();
                let clipped: String = body_text.chars().take(400).collect();
                return fail(format!("http_{}: {}", code, clipped));
            }
            Err(e) => return fail(e.to_string()),
        };
        let data: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => return fail("invalid_response: malformed_json".to_string()),
        };
        match parse_decision_api_answers(&data) {
            Ok(mut parsed) => {
                parsed.latency_ms = started.elapsed().as_millis() as u64;
                parsed.model = model.clone();
                parsed
            }
            Err(msg) => fail(msg),
        }
    }

    fn model_name(&self) -> String {
        self.config.model.clone()
    }
}

/// 노옵 로컬 구현 — 네트워크 I/O 없음, 항상 로컬 판정 유지(fail 결과)
pub struct NoopDecisionAdapter;

impl DecisionAdapter for NoopDecisionAdapter {
    fn classify(&self, _input: &DecisionAdapterInput) -> Option<DecisionAdapterResult> {
        None
    }

    fn classify_detailed(&self, _input: &DecisionAdapterInput) -> DecisionApiDetailedResult {
        DecisionApiDetailedResult {
            ok: false,
            p: None,
            confidence: None,
            risk_tier: None,
            input_tokens: None,
            cost_usd: None,
            cost_status: CostStatus::Unknown,
            latency_ms: 0,
            model: String::new(),
            error: Some("decision api disabled".to_string()),
        }
    }

    fn model_name(&self) -> String {
        String::new()
    }
}

pub fn create_decision_adapter(mode: DecisionApiMode, env: &EnvMap) -> Box<dyn DecisionAdapter> {
    if mode == DecisionApiMode::Off {
        return Box::new(NoopDecisionAdapter);
    }
    Box::new(GeminiDecisionAdapter::new(resolve_decision_api_config(env)))
}

// ---------- 모드별 적용 정책 + 감사 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SharpSplit {
    Sharp,
    Split,
}

impl SharpSplit {
    pub fn as_str(&self) -> &'static str {
        match self {
            SharpSplit::Sharp => "sharp",
            SharpSplit::Split => "split",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DecisionApiAudit {
    pub mode: DecisionApiMode,
    pub model: String,
    pub applied: bool,
    pub ok: bool,
    pub p_local: f64,
    pub p_api: Option<f64>,
    pub confidence: Option<f64>,
    pub risk_tier: Option<RiskLevel>,
    pub local_decision: SharpSplit,
    pub api_decision: Option<SharpSplit>,
    pub disagreed: Option<bool>,
    pub input_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
    pub cost_status: CostStatus,
    pub latency_ms: u64,
    pub error: Option<String>,
}

pub struct ApplyDecisionApiInput<'a> {
    pub mode: DecisionApiMode,
    pub adapter: &'a dyn DecisionAdapter,
    pub local_decision: SharpSplit,
    pub p_local: f64,
    pub sharp_threshold: f64,
    pub basis: DecisionBasis,
    /// route: spec.sharp_eligible, gate: true
    pub sharp_eligible: bool,
    /// route: 에스컬레이션된 태스크 위험도, gate: risk_override.unwrap_or(Low)
    pub local_risk: RiskLevel,
    pub forced_split: bool,
    /// `--api-eval` 또는 비공백 `--context` — measured 기반에서도 강제 API 평가
    pub force_api_eval: Option<bool>,
    pub adapter_input: &'a DecisionAdapterInput<'a>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApplyDecisionApiResult {
    pub decision: SharpSplit,
    pub p: f64,
    pub risk: RiskLevel,
    pub audit: Option<DecisionApiAudit>,
}

/// 모드별 Decision API 적용 정책 — CLI·디스패처 공용 단일 진입.
///
/// - `off`: 즉시 반환 (HTTP 0, audit None).
/// - `shadow`: API 호출 + 동일 하드 가드로 가상 `api_decision` 계산·불일치 기록만
///   (`applied:false` — 로컬 decision/p/risk 절대 불변).
/// - `on`: `basis='prior'` 또는 `force_api_eval`일 때만 호출. 성공 시 하드 가드로
///   `api_decision`을 계산해 적용(`applied:true`), 실패 시 로컬 폴백(`applied:false`).
///
/// 하드 안전 가드(우회 불가): `api_decision = sharp_eligible && effective_risk=='low'
/// && !forced_split && p_api >= sharp_threshold ? 'sharp' : 'split'` —
/// `effective_risk`는 `escalate_risk(local_risk, api.risk_tier)`로 상향만 허용한다.
/// EWMA 격리: 이 경로는 `decision_events`에 어떤 행도 기록하지 않는다.
pub fn apply_decision_api(input: &ApplyDecisionApiInput) -> ApplyDecisionApiResult {
    let (local_decision, p_local, local_risk) = (input.local_decision, input.p_local, input.local_risk);
    if input.mode == DecisionApiMode::Off {
        return ApplyDecisionApiResult { decision: local_decision, p: p_local, risk: local_risk, audit: None };
    }
    if input.mode == DecisionApiMode::On
        && input.basis == DecisionBasis::Measured
        && input.force_api_eval != Some(true)
    {
        // 학습된 로컬 EWMA 신뢰 — API 호출 0회, 감사 없음
        return ApplyDecisionApiResult { decision: local_decision, p: p_local, risk: local_risk, audit: None };
    }

    let res = input.adapter.classify_detailed(input.adapter_input);

    let mut api_decision: Option<SharpSplit> = None;
    let mut effective_risk = local_risk;
    if res.ok && res.p.is_some() {
        effective_risk = escalate_risk(local_risk, res.risk_tier);
        api_decision = Some(
            if input.sharp_eligible
                && effective_risk == RiskLevel::Low
                && !input.forced_split
                && res.p.unwrap() >= input.sharp_threshold
            {
                SharpSplit::Sharp
            } else {
                SharpSplit::Split
            },
        );
    }
    let audit = DecisionApiAudit {
        mode: input.mode,
        model: res.model.clone(),
        applied: false,
        ok: res.ok,
        p_local,
        p_api: res.p,
        confidence: res.confidence,
        risk_tier: res.risk_tier,
        local_decision,
        api_decision,
        disagreed: api_decision.map(|d| d != local_decision),
        input_tokens: res.input_tokens,
        cost_usd: res.cost_usd,
        cost_status: res.cost_status.clone(),
        latency_ms: res.latency_ms,
        error: res.error.clone(),
    };
    if input.mode == DecisionApiMode::On && res.ok && api_decision.is_some() {
        let decision = api_decision.unwrap();
        return ApplyDecisionApiResult {
            decision,
            p: res.p.unwrap_or(p_local),
            risk: effective_risk,
            audit: Some(DecisionApiAudit { applied: true, ..audit }),
        };
    }
    ApplyDecisionApiResult { decision: local_decision, p: p_local, risk: local_risk, audit: Some(audit) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    const TEST_KEY: &str = "TEST-KEY-9f8e7d6c5b4a3210";
    const CONTEXT_SECRET: &str = "sk-test-secret-1234567890abcdef";

    // ---------- 로컬 목 서버 (std::net::TcpListener — 외부 네트워크 0) ----------

    #[derive(Debug, Clone)]
    struct CapturedRequest {
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl CapturedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    #[derive(Debug, Clone)]
    struct MockResp {
        status: u16,
        body: String,
        delay_ms: u64,
        /// 요청 헤더의 API 키를 응답 본문에 그대로 반영(키 스크럽 검증용)
        echo_key: bool,
    }

    impl Default for MockResp {
        fn default() -> Self {
            MockResp { status: 200, body: "{}".to_string(), delay_ms: 0, echo_key: false }
        }
    }

    struct MockServer {
        base_url: String,
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl MockServer {
        fn start(handler: Arc<dyn Fn(&CapturedRequest) -> MockResp + Send + Sync>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("목 서버 바인드 실패");
            let port = listener.local_addr().unwrap().port();
            let requests: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let requests_for_thread = requests.clone();
            let stop = Arc::new(AtomicBool::new(false));
            listener.set_nonblocking(true).unwrap();
            let stop_clone = stop.clone();
            let thread = std::thread::spawn(move || {
                while !stop_clone.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            // macOS(BSD 소켓): nonblocking 리스너에서 accept된 스트림은
                            // O_NONBLOCK을 상속한다. 명시적으로 블로킹 모드로 되돌린다.
                            let _ = stream.set_nonblocking(false);
                            handle_conn(stream, &handler, &requests_for_thread);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(3));
                        }
                        Err(_) => break,
                    }
                }
            });
            MockServer {
                base_url: format!("http://127.0.0.1:{}", port),
                requests,
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    fn handle_conn(
        mut stream: TcpStream,
        handler: &Arc<dyn Fn(&CapturedRequest) -> MockResp + Send + Sync>,
        requests: &Arc<Mutex<Vec<CapturedRequest>>>,
    ) {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        // 헤더 종결(CRLFCRLF)까지 읽기
        let header_end = loop {
            if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
                break pos;
            }
            match stream.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::Interrupted
                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    continue; // read timeout(5s)이 걸린 블로킹 소켓이므로 재시도
                }
                Err(_) => return,
            }
            if buf.len() > 1024 * 1024 {
                return;
            }
        };
        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default().to_string();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let url = parts.next().unwrap_or_default().to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| {
                let (k, v) = l.split_once(':')?;
                Some((k.trim().to_string(), v.trim().to_string()))
            })
            .collect();
        let content_length: usize = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        // 본문 나머지 읽기
        while buf.len() < header_end + 4 + content_length {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::Interrupted
                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    continue;
                }
                Err(_) => break,
            }
        }
        let body = String::from_utf8_lossy(&buf[header_end + 4..(header_end + 4 + content_length).min(buf.len())]).into_owned();
        let captured = CapturedRequest { method, url, headers, body };
        requests.lock().unwrap_or_else(|e| e.into_inner()).push(captured.clone());
        let out = handler(&captured);
        if out.delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(out.delay_ms));
        }
        let payload = if out.echo_key {
            format!("{{\"error\": \"echo {}\"}}", captured.header("x-goog-api-key").unwrap_or_default())
        } else {
            out.body.clone()
        };
        let reason = match out.status {
            200 => "OK",
            400 => "Bad Request",
            500 => "Internal Server Error",
            _ => "Status",
        };
        let response = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            out.status,
            reason,
            payload.len(),
            payload
        );
        if stream.write_all(response.as_bytes()).is_ok() {
            let _ = stream.flush();
        }
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn decisions_body(noul: f64, risk_tier: &str, input_tokens: i64, usage_shape: &str) -> String {
        let usage = match usage_shape {
            "usage" => json!({ "usage": { "total_input_tokens": input_tokens, "output_tokens": 0 } }),
            _ => json!({ "usage_metadata": { "input_tokens": input_tokens, "output_tokens": 0 } }),
        };
        let mut body = json!({
            "model": "gemini-decision-1",
            "answers": {
                "should_fast_path": { "noul": noul, "confidence": 0.94, "reasoning": "mock" },
                "risk_tier": { "choice": risk_tier, "reasoning": "mock" },
            },
        });
        if let (Some(obj), Some(u)) = (body.as_object_mut(), usage.as_object()) {
            for (k, v) in u {
                obj.insert(k.clone(), v.clone());
            }
        }
        body.to_string()
    }

    fn api_env(base_url: &str) -> EnvMap {
        let mut env = EnvMap::new();
        env.insert("GEMINI_DECISION_API_KEY".to_string(), TEST_KEY.to_string());
        env.insert("DECISION_API_BASE_URL".to_string(), base_url.to_string());
        env
    }

    fn adapter_input<'a>(context: Option<&'a str>) -> DecisionAdapterInput<'a> {
        DecisionAdapterInput {
            project_id: "p1",
            route: RouteId::FileSelect,
            tool_class: "file_read",
            basis: DecisionBasis::Prior,
            p_local: 0.94,
            task_kind: Some("explore_read"),
            requires_write: None,
            risk_level: Some(RiskLevel::Low),
            samples: Some(0),
            consecutive_errors: Some(0),
            title: Some("explore"),
            context,
        }
    }

    #[test]
    fn mode_parsing_no_key_demotes_to_off() {
        let mut env = EnvMap::new();
        assert_eq!(parse_decision_api_mode(&env, None), DecisionApiMode::Off, "기본 off");
        env.insert("DECISION_API".to_string(), "on".to_string());
        assert_eq!(parse_decision_api_mode(&env, None), DecisionApiMode::Off, "무키 on → off 강등");
        env.insert("DECISION_API".to_string(), "shadow".to_string());
        assert_eq!(parse_decision_api_mode(&env, None), DecisionApiMode::Off, "무키 shadow → off 강등");
        env.insert("GEMINI_DECISION_API_KEY".to_string(), "   ".to_string());
        assert_eq!(parse_decision_api_mode(&env, None), DecisionApiMode::Off, "공백 키 → off 강등");
        env.insert("GEMINI_DECISION_API_KEY".to_string(), TEST_KEY.to_string());
        assert_eq!(parse_decision_api_mode(&env, None), DecisionApiMode::Shadow);
        assert_eq!(parse_decision_api_mode(&env, Some(DecisionApiMode::On)), DecisionApiMode::On, "CLI 오버라이드 우선");
        // CLI off 오버라이드가 env on을 덮어쓴다
        assert_eq!(parse_decision_api_mode(&env, Some(DecisionApiMode::Off)), DecisionApiMode::Off);
        // 무키 + CLI on → off 강등 (오버라이드도 키 검증을 통과해야 한다)
        env.remove("GEMINI_DECISION_API_KEY");
        assert_eq!(parse_decision_api_mode(&env, Some(DecisionApiMode::On)), DecisionApiMode::Off);
        // 잉여 공백·대소문자 정규화
        let mut env2 = EnvMap::new();
        env2.insert("DECISION_API".to_string(), " SHADOW ".to_string());
        env2.insert("GEMINI_DECISION_API_KEY".to_string(), TEST_KEY.to_string());
        assert_eq!(parse_decision_api_mode(&env2, None), DecisionApiMode::Shadow);
    }

    #[test]
    fn config_resolution_defaults_and_clamps() {
        let cfg = resolve_decision_api_config(&EnvMap::new());
        assert_eq!(cfg.base_url, DEFAULT_BASE_URL);
        assert_eq!(cfg.model, DEFAULT_MODEL);
        assert_eq!(cfg.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(cfg.api_key, "");

        let mut env = EnvMap::new();
        env.insert("DECISION_API_BASE_URL".to_string(), "http://x.test//".to_string());
        env.insert("DECISION_API_MODEL".to_string(), "  ".to_string());
        env.insert("DECISION_API_TIMEOUT_MS".to_string(), "99999".to_string());
        env.insert("GEMINI_DECISION_API_KEY".to_string(), TEST_KEY.to_string());
        let cfg = resolve_decision_api_config(&env);
        assert_eq!(cfg.base_url, "http://x.test");
        assert_eq!(cfg.model, DEFAULT_MODEL, "공백 모델은 기본값 폴백");
        assert_eq!(cfg.timeout_ms, TIMEOUT_MAX_MS, "상한 clamp");

        let mut env = EnvMap::new();
        env.insert("DECISION_API_TIMEOUT_MS".to_string(), "1".to_string());
        assert_eq!(resolve_decision_api_config(&env).timeout_ms, TIMEOUT_MIN_MS, "하한 clamp");

        let mut env = EnvMap::new();
        env.insert("DECISION_API_TIMEOUT_MS".to_string(), "80junk".to_string());
        assert_eq!(resolve_decision_api_config(&env).timeout_ms, 80, "parseInt 근사 — 후행 쓰레기 무시");
    }

    #[test]
    fn request_contract_header_auth_no_query_key_redaction() {
        let server = MockServer::start(Arc::new(|_| MockResp {
            body: decisions_body(0.15, "high", 200, "usage"),
            ..Default::default()
        }));
        let adapter = GeminiDecisionAdapter::new(resolve_decision_api_config(&api_env(&server.base_url)));
        let ctx = format!("read this {} now", CONTEXT_SECRET);
        let res = adapter.classify_detailed(&adapter_input(Some(&ctx)));

        assert!(res.ok, "error={:?}", res.error);
        let reqs = server.requests.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(reqs.len(), 1);
        let req = &reqs[0];
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "/v1beta/decisions");
        assert!(!req.url.contains("?key="), "URL에 ?key= 금지");
        assert_eq!(req.header("x-goog-api-key"), Some(TEST_KEY), "헤더 인증 전용");
        let body: Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["model"], "gemini-decision-1");
        let q = &body["questions"];
        assert_eq!(q["should_fast_path"]["type"], "noul");
        assert_eq!(q["risk_tier"]["type"], "choice");
        let sfp = q["should_fast_path"]["criteria"].as_object().unwrap();
        assert!(sfp.get("true_description").is_some(), "criteria.true_description 필수");
        assert!(sfp.get("false_description").is_some(), "criteria.false_description 필수");
        assert!(!sfp.contains_key("true"), "criteria.true 키 금지");
        assert!(!sfp.contains_key("false"), "criteria.false 키 금지");
        let rt = q["risk_tier"]["criteria"].as_object().unwrap();
        for k in ["low", "medium", "high"] {
            assert!(rt.contains_key(k), "risk_tier.criteria.{} 필수", k);
        }
        let input = body["input"].as_str().unwrap();
        assert!(!input.contains(CONTEXT_SECRET), "context 시크릿은 전송 전 리댁션");
        assert!(input.contains("route: file_select"));
        assert!(input.contains("task_kind: explore_read"));
        assert!(input.contains("requires_write") == false, "requires_write=None은 미출력");
        assert_eq!(res.p, Some(0.15));
        assert_eq!(res.risk_tier, Some(RiskLevel::High));
        assert_eq!(res.input_tokens, Some(200));
        assert!((res.cost_usd.unwrap() - 0.00002).abs() < 1e-12, "cost={:?}", res.cost_usd);
        assert_eq!(res.cost_status, CostStatus::Estimated);
    }

    #[test]
    fn usage_shape_official_and_legacy_both_parse() {
        let shape_ptr: Arc<Mutex<String>> = Arc::new(Mutex::new("usage".to_string()));
        let shape_for_handler = shape_ptr.clone();
        let server = MockServer::start(Arc::new(move |_| {
            let s = shape_for_handler.lock().unwrap_or_else(|e| e.into_inner()).clone();
            MockResp { body: decisions_body(0.8, "low", 150, &s), ..Default::default() }
        }));
        let adapter = GeminiDecisionAdapter::new(resolve_decision_api_config(&api_env(&server.base_url)));
        let official = adapter.classify_detailed(&adapter_input(None));
        assert!(official.ok);
        assert_eq!(official.input_tokens, Some(150), "usage.total_input_tokens 파싱");
        assert_eq!(official.cost_status, CostStatus::Estimated);

        *shape_ptr.lock().unwrap_or_else(|e| e.into_inner()) = "usage_metadata".to_string();
        let legacy = adapter.classify_detailed(&adapter_input(None));
        assert!(legacy.ok);
        assert_eq!(legacy.input_tokens, Some(150), "usage_metadata.input_tokens 파싱");
        assert_eq!(legacy.cost_status, CostStatus::Estimated);
    }

    #[test]
    fn noul_clamp_and_invalid_responses_fail_safe() {
        // 극단 noul 클램프 [0.01, 0.99]
        let server = MockServer::start(Arc::new(|_| MockResp {
            body: decisions_body(0.000001, "low", 10, "usage"),
            ..Default::default()
        }));
        let adapter = GeminiDecisionAdapter::new(resolve_decision_api_config(&api_env(&server.base_url)));
        assert_eq!(adapter.classify_detailed(&adapter_input(None)).p, Some(0.01));
        let server = MockServer::start(Arc::new(|_| MockResp {
            body: decisions_body(1.7, "low", 10, "usage"),
            ..Default::default()
        }));
        let adapter = GeminiDecisionAdapter::new(resolve_decision_api_config(&api_env(&server.base_url)));
        assert_eq!(adapter.classify_detailed(&adapter_input(None)).p, Some(0.99));

        // noul 누락 → 구조화 실패
        let server = MockServer::start(Arc::new(|_| MockResp {
            body: json!({ "answers": { "risk_tier": { "choice": "low" } } }).to_string(),
            ..Default::default()
        }));
        let adapter = GeminiDecisionAdapter::new(resolve_decision_api_config(&api_env(&server.base_url)));
        let res = adapter.classify_detailed(&adapter_input(None));
        assert!(!res.ok);
        assert!(res.error.as_deref().unwrap().contains("should_fast_path.noul"));

        // 깨진 JSON → 구조화 실패
        let server = MockServer::start(Arc::new(|_| MockResp {
            body: "not-json{{{".to_string(),
            ..Default::default()
        }));
        let adapter = GeminiDecisionAdapter::new(resolve_decision_api_config(&api_env(&server.base_url)));
        let res = adapter.classify_detailed(&adapter_input(None));
        assert!(!res.ok);
        assert!(res.error.as_deref().unwrap().contains("malformed_json"));
    }

    #[test]
    fn http_500_key_echo_scrubbed_from_error() {
        let server = MockServer::start(Arc::new(|_| MockResp { status: 500, echo_key: true, ..Default::default() }));
        let adapter = GeminiDecisionAdapter::new(resolve_decision_api_config(&api_env(&server.base_url)));
        let res = adapter.classify_detailed(&adapter_input(None));
        assert!(!res.ok);
        let err = res.error.unwrap();
        assert!(err.contains("http_500"), "err={}", err);
        assert!(!err.contains(TEST_KEY), "감사 error에 키 없음: {}", err);
    }

    #[test]
    fn timeout_falls_back_without_panic() {
        let server = MockServer::start(Arc::new(|_| MockResp {
            body: decisions_body(0.99, "low", 100, "usage"),
            delay_ms: 350,
            ..Default::default()
        }));
        let mut env = api_env(&server.base_url);
        env.insert("DECISION_API_TIMEOUT_MS".to_string(), "80".to_string());
        let adapter = GeminiDecisionAdapter::new(resolve_decision_api_config(&env));
        let started = Instant::now();
        let res = adapter.classify_detailed(&adapter_input(None));
        assert!(!res.ok, "타임아웃은 ok:false 폴백");
        assert!(res.error.is_some());
        assert!(res.p.is_none());
        assert!(started.elapsed() < Duration::from_millis(2000), "80ms 타임아웃이 실제로 걸린다");
    }

    // ---------- apply_decision_api (하드 가드 — 네트워크 없는 카운팅 어댑터) ----------

    struct CountingAdapter {
        calls: std::sync::atomic::AtomicUsize,
        result: Mutex<DecisionApiDetailedResult>,
    }

    impl CountingAdapter {
        fn new(ok: bool, p: Option<f64>, risk_tier: Option<RiskLevel>) -> Self {
            CountingAdapter {
                calls: std::sync::atomic::AtomicUsize::new(0),
                result: Mutex::new(DecisionApiDetailedResult {
                    ok,
                    p,
                    confidence: Some(0.9),
                    risk_tier,
                    input_tokens: Some(100),
                    cost_usd: Some(0.00001),
                    cost_status: CostStatus::Estimated,
                    latency_ms: 5,
                    model: "gemini-decision-1".to_string(),
                    error: if ok { None } else { Some("boom".to_string()) },
                }),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl DecisionAdapter for CountingAdapter {
        fn classify(&self, input: &DecisionAdapterInput) -> Option<DecisionAdapterResult> {
            self.classify_detailed(input).p.filter(|_| self.result.lock().unwrap().ok).map(|p| DecisionAdapterResult { p, source: DecisionSource::Api })
        }
        fn classify_detailed(&self, _input: &DecisionAdapterInput) -> DecisionApiDetailedResult {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.result.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
        fn model_name(&self) -> String {
            "gemini-decision-1".to_string()
        }
    }

    fn apply_input<'a>(adapter: &'a CountingAdapter, adapter_input: &'a DecisionAdapterInput<'a>) -> ApplyDecisionApiInput<'a> {
        ApplyDecisionApiInput {
            mode: DecisionApiMode::On,
            adapter,
            local_decision: SharpSplit::Sharp,
            p_local: 0.8,
            sharp_threshold: 0.70,
            basis: DecisionBasis::Prior,
            sharp_eligible: true,
            local_risk: RiskLevel::Low,
            forced_split: false,
            force_api_eval: None,
            adapter_input,
        }
    }

    #[test]
    fn off_mode_makes_zero_calls_and_measured_on_skips_api() {
        let adapter = CountingAdapter::new(true, Some(0.99), Some(RiskLevel::Low));
        let input = adapter_input(None);
        let off = apply_decision_api(&ApplyDecisionApiInput { mode: DecisionApiMode::Off, ..apply_input(&adapter, &input) });
        assert_eq!(off.decision, SharpSplit::Sharp);
        assert_eq!(off.p, 0.8);
        assert!(off.audit.is_none());
        assert_eq!(adapter.calls(), 0, "off → HTTP 0");

        let measured = apply_decision_api(&ApplyDecisionApiInput {
            basis: DecisionBasis::Measured,
            ..apply_input(&adapter, &input)
        });
        assert_eq!(measured.audit, None);
        assert_eq!(adapter.calls(), 0, "measured + on → API 호출 0");

        let forced = apply_decision_api(&ApplyDecisionApiInput {
            basis: DecisionBasis::Measured,
            force_api_eval: Some(true),
            ..apply_input(&adapter, &input)
        });
        assert!(forced.audit.is_some(), "--api-eval은 measured에서도 호출");
        assert_eq!(adapter.calls(), 1);
    }

    #[test]
    fn on_mode_applies_api_decision_when_guards_pass() {
        // 로컬 split(prior p 낮음) → API sharp 반영
        let adapter = CountingAdapter::new(true, Some(0.91), Some(RiskLevel::Low));
        let input = adapter_input(None);
        let out = apply_decision_api(&ApplyDecisionApiInput { local_decision: SharpSplit::Split, p_local: 0.62, ..apply_input(&adapter, &input) });
        assert_eq!(adapter.calls(), 1);
        assert_eq!(out.decision, SharpSplit::Sharp, "API noul 0.91 >= 0.70 → sharp 전환");
        assert_eq!(out.p, 0.91);
        let audit = out.audit.unwrap();
        assert!(audit.applied);
        assert_eq!(audit.mode, DecisionApiMode::On);
        assert_eq!(audit.local_decision, SharpSplit::Split);
        assert_eq!(audit.api_decision, Some(SharpSplit::Sharp));
        assert_eq!(audit.disagreed, Some(true));
    }

    #[test]
    fn hard_safety_guards_never_bypassable() {
        let input = adapter_input(None);
        // ① sharp_eligible=false (requires_write 류) — 낙관 API에도 split
        let adapter = CountingAdapter::new(true, Some(0.99), Some(RiskLevel::Low));
        let out = apply_decision_api(&ApplyDecisionApiInput { sharp_eligible: false, ..apply_input(&adapter, &input) });
        assert_eq!(out.decision, SharpSplit::Split, "sharp 비적격은 API로도 sharp 불가");
        assert_eq!(out.audit.unwrap().api_decision, Some(SharpSplit::Split));

        // ② forced_split
        let adapter = CountingAdapter::new(true, Some(0.99), Some(RiskLevel::Low));
        let out = apply_decision_api(&ApplyDecisionApiInput { forced_split: true, ..apply_input(&adapter, &input) });
        assert_eq!(out.decision, SharpSplit::Split, "forced split 우선");

        // ③ local risk high → escalate_risk는 상향만 → split
        let adapter = CountingAdapter::new(true, Some(0.99), Some(RiskLevel::Low));
        let out = apply_decision_api(&ApplyDecisionApiInput { local_risk: RiskLevel::High, ..apply_input(&adapter, &input) });
        assert_eq!(out.decision, SharpSplit::Split);
        assert_eq!(out.risk, RiskLevel::High, "상향만 허용 — API low로 하향되지 않음");

        // ④ API risk_tier high → 상향 에스컬레이션으로 split
        let adapter = CountingAdapter::new(true, Some(0.99), Some(RiskLevel::High));
        let out = apply_decision_api(&ApplyDecisionApiInput { ..apply_input(&adapter, &input) });
        assert_eq!(out.decision, SharpSplit::Split, "API risk high → split");
        assert_eq!(out.risk, RiskLevel::High);
        let audit = out.audit.unwrap();
        assert!(audit.applied, "적용은 되지만 api_decision=split");
        assert_eq!(audit.api_decision, Some(SharpSplit::Split));

        // ⑤ p_api < threshold → split
        let adapter = CountingAdapter::new(true, Some(0.5), Some(RiskLevel::Low));
        let out = apply_decision_api(&ApplyDecisionApiInput { ..apply_input(&adapter, &input) });
        assert_eq!(out.decision, SharpSplit::Split, "p_api 0.5 < 0.70 → split");

        // ⑥ API 실패(ok:false) → 로컬 폴백 + applied:false
        let adapter = CountingAdapter::new(false, None, None);
        let out = apply_decision_api(&ApplyDecisionApiInput { local_decision: SharpSplit::Split, p_local: 0.6, ..apply_input(&adapter, &input) });
        assert_eq!(out.decision, SharpSplit::Split);
        assert_eq!(out.p, 0.6);
        let audit = out.audit.unwrap();
        assert!(!audit.ok);
        assert!(!audit.applied);
        assert_eq!(audit.api_decision, None);
        assert_eq!(audit.disagreed, None);
    }

    #[test]
    fn shadow_mode_never_alters_local_decision() {
        let adapter = CountingAdapter::new(true, Some(0.15), Some(RiskLevel::High));
        let input = adapter_input(None);
        let out = apply_decision_api(&ApplyDecisionApiInput {
            mode: DecisionApiMode::Shadow,
            local_decision: SharpSplit::Sharp,
            p_local: 0.94,
            ..apply_input(&adapter, &input)
        });
        assert_eq!(adapter.calls(), 1, "shadow는 호출한다");
        assert_eq!(out.decision, SharpSplit::Sharp, "로컬 판정 절대 불변");
        assert_eq!(out.p, 0.94);
        assert_eq!(out.risk, RiskLevel::Low, "로컬 risk도 불변");
        let audit = out.audit.unwrap();
        assert_eq!(audit.mode, DecisionApiMode::Shadow);
        assert!(!audit.applied);
        assert_eq!(audit.p_api, Some(0.15));
        assert_eq!(audit.api_decision, Some(SharpSplit::Split));
        assert_eq!(audit.disagreed, Some(true), "sharp vs split 불일치 기록");
    }

    #[test]
    fn prompt_redaction_truncation_and_field_omission() {
        let long_secret = "sk-abcdefghijklmnop0123456789";
        let long_ctx = format!("{} {}", long_secret, "x".repeat(5000));
        let input = DecisionAdapterInput {
            context: Some(&long_ctx),
            title: Some(""),
            requires_write: Some(true),
            risk_level: None,
            ..adapter_input(None)
        };
        let prompt = build_decision_prompt_input(&input);
        assert!(prompt.contains("requires_write: true"));
        assert!(!prompt.contains("requires_write: false"));
        assert!(!prompt.contains("title:"), "빈 title 미출력");
        assert!(!prompt.contains(long_secret), "context 시크릿 리댁션");
        let ctx_line = prompt.lines().find(|l| l.starts_with("context: ")).unwrap();
        assert!(ctx_line.chars().count() <= "context: ".len() + CONTEXT_MAX_CHARS, "4000자 절단");
        assert!(prompt.contains("local_basis: prior"));
        assert!(prompt.contains("local_p: 0.94"));
    }

    #[test]
    fn noop_adapter_returns_disabled_result() {
        let noop = NoopDecisionAdapter;
        assert!(noop.classify(&adapter_input(None)).is_none());
        let res = noop.classify_detailed(&adapter_input(None));
        assert!(!res.ok);
        assert_eq!(res.error.as_deref(), Some("decision api disabled"));
        assert_eq!(res.model, "");
        let boxed = create_decision_adapter(DecisionApiMode::Off, &EnvMap::new());
        assert!(boxed.classify_detailed(&adapter_input(None)).error.is_some());
    }

    #[test]
    fn round8_and_uri_component_helpers() {
        assert!((round8(0.0000215) - 0.0000215).abs() < 1e-12);
        assert!((round8(200.0 * 0.10 / 1_000_000.0) - 0.00002).abs() < 1e-12);
        assert_eq!(encode_uri_component("a b/c?d=e&f"), "a%20b%2Fc%3Fd%3De%26f");
        assert_eq!(encode_uri_component("AZaz09-_.!~*'()"), "AZaz09-_.!~*'()");
        let scrubbed = scrub_api_key(&format!("failed with {} and {}", TEST_KEY, encode_uri_component(TEST_KEY)), TEST_KEY);
        assert!(!scrubbed.contains(TEST_KEY));
        assert_eq!(scrubbed.matches("<redacted>").count(), 2, "raw + URL 인코딩 형태 모두 스크럽");
    }

    #[test]
    fn temp_dir_helper_compiles() {
        // adapter 테스트 유틸 회귀 가드 — temp_dir 패닉 없음 확인
        let t = temp_dir("adapter-t");
        assert!(t.starts_with(std::env::temp_dir()));
    }
}
