//! PR-A3 Fail-Closed 구조화 Verdict 검증 + retro 마크다운 포매터 —
//! `src/decision/verdict.ts` 포팅 (loopforge-arch-hydrogen §4.2·§5.1).
//!
//! 목적: Verifier가 남기는 `.herdr/verify/<fork_id>-verdict.json`(SSOT)을
//! ① 파일·크기·JSON 파싱 ② fork_id/target_head 결속 ③ 모순 판정 3중 불변식으로
//! 검증해, 검증 불가 상태가 절대 통과(pass)로 해석되지 않게 한다 — 모든 거부는
//! `effective_decision = Human` 폴백(fail-closed)이다.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Verifier가 남길 수 있는 판정 4종 — CLI·판정 문서가 공유하는 SSOT 값 목록
pub const VERDICT_DECISIONS: [&str; 4] = ["pass", "rework", "fail", "human"];

/// finding 심각도 등급 — `P0`/`P1`은 pass 판정과 공존할 수 없다(불변식 3)
pub const FINDING_SEVERITIES: [&str; 4] = ["P0", "P1", "P2", "info"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerdictDecision {
    Pass,
    Rework,
    Fail,
    Human,
}

impl VerdictDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            VerdictDecision::Pass => "pass",
            VerdictDecision::Rework => "rework",
            VerdictDecision::Fail => "fail",
            VerdictDecision::Human => "human",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pass" => Some(VerdictDecision::Pass),
            "rework" => Some(VerdictDecision::Rework),
            "fail" => Some(VerdictDecision::Fail),
            "human" => Some(VerdictDecision::Human),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerdictFinding {
    pub severity: String,
    pub title: String,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerdictDocument {
    pub schema_version: i64,
    pub fork_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_head: Option<String>,
    pub decision: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_required: Option<String>,
    pub findings: Vec<VerdictFinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_commands: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifacts: Option<Vec<String>>,
}

/// 검증 결과 — `valid: false`면 반드시 `effective_decision: Human`(fail-closed)
#[derive(Debug, Clone, PartialEq)]
pub struct VerdictValidationResult {
    pub valid: bool,
    pub effective_decision: VerdictDecision,
    pub errors: Vec<String>,
    pub document: Option<VerdictDocument>,
}

#[derive(Debug, Clone)]
pub struct ValidateVerdictOptions {
    pub expected_fork_id: String,
    /// full sha 권장. 지정 시 문서 `target_head`와 일치·7자 이상 prefix 비교로 결속한다
    pub expected_head: Option<String>,
}

/// verdict 파일 크기 상한(64KB) — 압축폭탄·오염 방지 차원 거부
pub const DEFAULT_VERDICT_MAX_BYTES: u64 = 65536;

const MAX_REASON_LENGTH: usize = 4000;
const MAX_ACTION_REQUIRED_LENGTH: usize = 4000;
const MAX_FINDING_TITLE_LENGTH: usize = 500;
/// HEAD prefix 비교 최소 길이 — 그 미만 약칭은 모호해서 Stale Verdict로 거부
const MIN_HEAD_PREFIX_LENGTH: usize = 7;
/// retro 표 셀에 들어갈 오류 스니펫 상한(문자 수)
const RETRO_SNIPPET_MAX_CHARS: usize = 200;

fn fail_closed(errors: Vec<String>) -> VerdictValidationResult {
    VerdictValidationResult {
        valid: false,
        effective_decision: VerdictDecision::Human,
        errors,
        document: None,
    }
}

/// TS `JSON.stringify(v)` 근사 — undefined/객체 표기를 포함한 진단 메시지용.
fn stringify_jsonish(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(other) => serde_json::to_string(other).unwrap_or_else(|_| "?".to_string()),
    }
}

fn is_string_array(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Array(items)) => items.iter().all(|i| i.is_string()),
        _ => false,
    }
}

/// HEAD 결속 규칙: 문서 `target_head`가 `expected_head`와 동일하거나, 한쪽이 다른 쪽의
/// 7자 이상 prefix면 일치로 본다(git 약칭 표기 허용). 그 외는 전부 불일치(Stale).
fn head_matches(doc_head: &str, expected_head: &str) -> bool {
    if doc_head == expected_head {
        return true;
    }
    let (shorter, longer) = if doc_head.len() <= expected_head.len() {
        (doc_head, expected_head)
    } else {
        (expected_head, doc_head)
    };
    shorter.len() >= MIN_HEAD_PREFIX_LENGTH && longer.starts_with(shorter)
}

/// 불변식 1·2·3을 파싱된 JSON 값에 적용한다. 모든 위반은 누적돼 한 번에 보고되고,
/// 위반이 1건이라도 있으면 `{ valid: false, effective_decision: Human }`으로
/// 폴백한다 — 절대 pass로 넘어가지 않는다.
pub fn validate_verdict_data(raw: &Value, opts: &ValidateVerdictOptions) -> VerdictValidationResult {
    let mut errors: Vec<String> = Vec::new();

    // --- 불변식 1: 최소 구조·스키마 (Fail-Closed) ---
    let doc = match raw.as_object() {
        Some(o) => o,
        None => return fail_closed(vec!["verdict 문서가 JSON 객체가 아닙니다".to_string()]),
    };

    let schema_ok = doc.get("schema_version").and_then(|v| v.as_f64()) == Some(1.0);
    if !schema_ok {
        errors.push(format!(
            "schema_version가 1이 아닙니다: {}",
            stringify_jsonish(doc.get("schema_version"))
        ));
    }

    let decision_raw = doc.get("decision").and_then(|v| v.as_str());
    let known_decision = decision_raw.and_then(VerdictDecision::parse);
    if known_decision.is_none() {
        errors.push(format!(
            "알 수 없는 decision: {}",
            stringify_jsonish(doc.get("decision"))
        ));
    }

    let reason_raw = doc.get("reason").and_then(|v| v.as_str());
    match reason_raw {
        None => errors.push("reason가 빈 문자열이거나 누락됐습니다".to_string()),
        Some(r) if r.trim().is_empty() => errors.push("reason가 빈 문자열이거나 누락됐습니다".to_string()),
        Some(r) => {
            if r.chars().count() > MAX_REASON_LENGTH {
                errors.push(format!("reason가 {}자를 초과했습니다: {}자", MAX_REASON_LENGTH, r.chars().count()));
            }
        }
    }

    if let Some(ar) = doc.get("action_required").and_then(|v| v.as_str()) {
        if ar.chars().count() > MAX_ACTION_REQUIRED_LENGTH {
            errors.push(format!(
                "action_required가 {}자를 초과했습니다: {}자",
                MAX_ACTION_REQUIRED_LENGTH,
                ar.chars().count()
            ));
        }
    }

    // --- findings 스키마: 배열 + 항목별 severity/title/evidence 검증 ---
    let mut findings: Vec<VerdictFinding> = Vec::new();
    match doc.get("findings") {
        Some(Value::Array(raw_findings)) => {
            for (i, item) in raw_findings.iter().enumerate() {
                let finding = match item.as_object() {
                    Some(f) => f,
                    None => {
                        errors.push(format!("findings[{}]가 객체가 아닙니다", i));
                        continue;
                    }
                };
                let severity = finding.get("severity").and_then(|v| v.as_str());
                match severity {
                    Some(s) if FINDING_SEVERITIES.contains(&s) => {}
                    _ => errors.push(format!(
                        "findings[{}].severity가 허용 값(P0|P1|P2|info)이 아닙니다: {}",
                        i,
                        stringify_jsonish(finding.get("severity"))
                    )),
                }
                let title = finding.get("title").and_then(|v| v.as_str());
                match title {
                    Some(t) if !t.trim().is_empty() => {
                        if t.chars().count() > MAX_FINDING_TITLE_LENGTH {
                            errors.push(format!(
                                "findings[{}].title이 {}자를 초과했습니다: {}자",
                                i,
                                MAX_FINDING_TITLE_LENGTH,
                                t.chars().count()
                            ));
                        }
                    }
                    _ => errors.push(format!("findings[{}].title이 빈 문자열이거나 누락됐습니다", i)),
                }
                if finding.get("evidence").and_then(|v| v.as_str()).is_none() {
                    errors.push(format!("findings[{}].evidence가 문자열이 아닙니다", i));
                }
                findings.push(VerdictFinding {
                    severity: severity.unwrap_or("").to_string(),
                    title: title.unwrap_or("").to_string(),
                    evidence: finding
                        .get("evidence")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                });
            }
        }
        _ => errors.push("findings가 배열이 아닙니다".to_string()),
    }

    if let Some(cc) = doc.get("checked_commands") {
        if !cc.is_null() && !is_string_array(Some(cc)) {
            errors.push("checked_commands가 문자열 배열이 아닙니다".to_string());
        }
    }
    if let Some(arts) = doc.get("artifacts") {
        if !arts.is_null() && !is_string_array(Some(arts)) {
            errors.push("artifacts가 문자열 배열이 아닙니다".to_string());
        }
    }

    // --- 불변식 2: Fork & HEAD 결속 (Replay·Stale 차단) ---
    let fork_id = doc.get("fork_id").and_then(|v| v.as_str());
    if fork_id != Some(opts.expected_fork_id.as_str()) {
        errors.push(format!(
            "fork_id 불일치(Replay 의심): 문서={}, 예상={}",
            stringify_jsonish(doc.get("fork_id")),
            serde_json::to_string(&opts.expected_fork_id).unwrap()
        ));
    }
    if let Some(expected) = opts
        .expected_head
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let doc_head = doc.get("target_head").and_then(|v| v.as_str()).map(str::trim);
        match doc_head {
            Some(h) if !h.is_empty() => {
                if !head_matches(h, expected) {
                    errors.push(format!(
                        "target_head 불일치 (Stale Verdict): 문서={}, 예상={}",
                        doc.get("target_head").and_then(|v| v.as_str()).unwrap_or(""),
                        expected
                    ));
                }
            }
            _ => errors.push("expectedHead가 지정됐는데 target_head가 없습니다 (Stale Verdict 거부)".to_string()),
        }
    }

    // --- 불변식 3: 모순 판정 차단 ---
    if let Some(d) = known_decision {
        if d == VerdictDecision::Pass {
            let blocking = findings.iter().filter(|f| f.severity == "P0" || f.severity == "P1").count();
            if blocking > 0 {
                errors.push(format!(
                    "pass 판정에 P0/P1 finding이 {}건 존재합니다 (P2/info만 허용)",
                    blocking
                ));
            }
        }
        if d == VerdictDecision::Fail || d == VerdictDecision::Rework {
            if findings.is_empty() {
                errors.push(format!(
                    "{} 판정에 findings가 없습니다 (근거 없는 FAIL/REWORK 차단)",
                    d.as_str()
                ));
            } else if !findings.iter().any(|f| !f.evidence.trim().is_empty()) {
                errors.push(format!(
                    "{} 판정의 findings에 비공백 evidence가 1건도 없습니다 (근거 없는 FAIL/REWORK 차단)",
                    d.as_str()
                ));
            }
        }
        if d == VerdictDecision::Human {
            let ar = doc.get("action_required").and_then(|v| v.as_str()).map(str::trim);
            if ar.filter(|s| !s.is_empty()).is_none() {
                errors.push("human 판정에 action_required가 비어 있거나 누락됐습니다".to_string());
            }
        }
    }

    if !errors.is_empty() {
        return fail_closed(errors);
    }

    // --- 정상 문서 정규화 반환 ---
    let document = VerdictDocument {
        schema_version: 1,
        fork_id: fork_id.unwrap_or_default().to_string(),
        decision: decision_raw.unwrap_or_default().to_string(),
        reason: reason_raw.unwrap_or_default().to_string(),
        target_head: doc.get("target_head").and_then(|v| v.as_str()).map(str::to_string),
        action_required: doc.get("action_required").and_then(|v| v.as_str()).map(str::to_string),
        findings,
        checked_commands: doc
            .get("checked_commands")
            .filter(|v| is_string_array(Some(v)))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|i| i.as_str().map(str::to_string)).collect()),
        artifacts: doc
            .get("artifacts")
            .filter(|v| is_string_array(Some(v)))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|i| i.as_str().map(str::to_string)).collect()),
    };
    VerdictValidationResult {
        valid: true,
        effective_decision: VerdictDecision::parse(&document.decision).unwrap_or(VerdictDecision::Human),
        errors: Vec::new(),
        document: Some(document),
    }
}

/// `.herdr/verify/<fork_id>-verdict.json` 파일을 읽어 검증한다.
/// 파일 미존재·크기 초과·읽기 실패·JSON 파싱 실패까지 전부 fail-closed(`human`)다.
pub fn validate_verdict_file(
    file_path: &Path,
    opts: &ValidateVerdictOptions,
) -> VerdictValidationResult {
    validate_verdict_file_with_max(file_path, opts, DEFAULT_VERDICT_MAX_BYTES)
}

pub fn validate_verdict_file_with_max(
    file_path: &Path,
    opts: &ValidateVerdictOptions,
    max_bytes: u64,
) -> VerdictValidationResult {
    let meta = match std::fs::metadata(file_path) {
        Ok(m) => m,
        Err(_) => return fail_closed(vec![format!("verdict 파일을 찾을 수 없습니다: {}", file_path.display())]),
    };
    if !meta.is_file() {
        return fail_closed(vec![format!("verdict 경로가 파일이 아닙니다: {}", file_path.display())]);
    }
    if meta.len() > max_bytes {
        return fail_closed(vec![format!(
            "verdict 파일 크기 초과: {}바이트 > 최대 {}바이트",
            meta.len(),
            max_bytes
        )]);
    }
    let raw = match std::fs::read_to_string(file_path) {
        Ok(s) => s,
        Err(e) => return fail_closed(vec![format!("verdict 파일 읽기 실패: {}", e)]),
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(parsed) => validate_verdict_data(&parsed, opts),
        Err(e) => fail_closed(vec![format!("verdict JSON 파싱 실패: {}", e)]),
    }
}

/// Verdict SSOT 경로 — 입·출력 계약이 `.herdr/verify/`에 쌍으로 결속된다
pub fn resolve_verdict_path(repo_dir: &Path, fork_id: &str) -> PathBuf {
    repo_dir.join(".herdr").join("verify").join(format!("{}-verdict.json", fork_id))
}

/// retro 실패 도구 표본 — DB 집계에서 상위 N건을 뽑아 전달한다
#[derive(Debug, Clone, PartialEq)]
pub struct RetroFailedTool {
    pub tool_name: String,
    pub count: i64,
    pub last_error: Option<String>,
}

/// `dg retro` 입력 — DB(`forks`·`decision_events`)에서 뽑은 회고 재료.
/// `sanitize`는 선택적 문자열 스크러빙 콜백(예: `redact_secrets`).
pub struct RetroInput<'a> {
    pub fork_id: String,
    pub project_label: Option<&'a str>,
    pub route: &'a str,
    pub tool_class: &'a str,
    pub status: &'a str,
    pub attempts: Option<i64>,
    pub cause: Option<&'a str>,
    pub rework_count: Option<i64>,
    pub reason: Option<&'a str>,
    pub top_failed_tools: Vec<RetroFailedTool>,
    pub recommendations: Vec<String>,
    pub sanitize: Option<fn(&str) -> String>,
}

/// 표 셀·인용에 넣기 전 whitespace 정규화 — 줄바꿈이 마크다운 구조를 깨뜨리지 않게 한다
fn to_single_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_ws = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out.trim().to_string()
}

/// 마크다운 표 셀 이스케이프 — `|`가 열 구분자로 오인되지 않게 한다
fn escape_table_cell(text: &str) -> String {
    text.replace('|', "\\|")
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 다음 시도 워커 브리프에 주입할 리트로 마크다운을 생성한다(순수 함수, 파일 쓰기 없음).
/// 구성: 제목 → 프로젝트/라우트 요약 줄 → 상태·원인·시도·재작업 표 → 실패 도구 상위
/// 5건(절단) → 다음 시도 권고(최대 3줄). 오류·사유 문자열은 sanitize(지정 시)를
/// **전체 문자열에 먼저** 적용한 뒤 잘라낸다(절단이 패턴을 가르는 누출을 막기 위해).
pub fn build_retro_markdown(input: &RetroInput) -> String {
    let clean = |s: &str| -> String {
        match input.sanitize {
            Some(f) => f(s),
            None => s.to_string(),
        }
    };
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("# Loop Retro — {}", input.fork_id));
    lines.push(String::new());
    if let Some(label) = input.project_label.filter(|l| !l.trim().is_empty()) {
        lines.push(format!("- 프로젝트: {}", clean(&to_single_line(label))));
    }
    lines.push(format!("- 라우트 / 도구 클래스: {} / {}", input.route, input.tool_class));
    lines.push(String::new());
    lines.push("## 요약".to_string());
    lines.push(String::new());
    lines.push("| 항목 | 값 |".to_string());
    lines.push("| --- | --- |".to_string());
    lines.push(format!("| 상태 | {} |", escape_table_cell(&clean(&to_single_line(input.status)))));
    let cause_cell = match input.cause.filter(|c| !c.trim().is_empty()) {
        Some(c) => escape_table_cell(&clean(&to_single_line(c))),
        None => "-".to_string(),
    };
    lines.push(format!("| 원인 | {} |", cause_cell));
    lines.push(format!(
        "| 시도 횟수 | {} |",
        input.attempts.map(|a| a.to_string()).unwrap_or_else(|| "-".to_string())
    ));
    lines.push(format!(
        "| 재작업 횟수 | {} |",
        input.rework_count.map(|a| a.to_string()).unwrap_or_else(|| "-".to_string())
    ));
    if let Some(reason) = input.reason.filter(|r| !r.trim().is_empty()) {
        lines.push(String::new());
        lines.push(format!("> reason: {}", clean(&to_single_line(reason))));
    }
    lines.push(String::new());
    lines.push("## 실패 도구 상위 5건".to_string());
    lines.push(String::new());
    let top: Vec<&RetroFailedTool> = input.top_failed_tools.iter().take(5).collect();
    if top.is_empty() {
        lines.push("(실패 도구 표본 없음)".to_string());
    } else {
        lines.push("| # | 도구 | 실패 횟수 | 최근 오류 |".to_string());
        lines.push("| --- | --- | --- | --- |".to_string());
        for (index, tool) in top.iter().enumerate() {
            let last_error = match tool.last_error.as_deref().filter(|e| !e.trim().is_empty()) {
                Some(e) => escape_table_cell(&truncate_chars(&clean(&to_single_line(e)), RETRO_SNIPPET_MAX_CHARS)),
                None => "-".to_string(),
            };
            lines.push(format!(
                "| {} | {} | {} | {} |",
                index + 1,
                escape_table_cell(&clean(&to_single_line(&tool.tool_name))),
                tool.count,
                last_error
            ));
        }
    }
    lines.push(String::new());
    lines.push("## 다음 시도 권고".to_string());
    lines.push(String::new());
    let recommendations: Vec<String> = input
        .recommendations
        .iter()
        .take(3)
        .map(|r| clean(&to_single_line(r)))
        .collect();
    if recommendations.is_empty() {
        lines.push("1. (권고 없음 — 원인 데이터를 보강한 뒤 다시 생성한다)".to_string());
    } else {
        for (i, r) in recommendations.iter().enumerate() {
            lines.push(format!("{}. {}", i + 1, r));
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

/// `git rev-parse HEAD` 동기 조회 — git 부재·비 repo·실패 어느 경우든 오류 대신
/// `None`을 반환한다(무예외).
pub fn resolve_git_head(repo_dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", &repo_dir.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if head.is_empty() {
        None
    } else {
        Some(head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;
    use serde_json::json;

    fn opts() -> ValidateVerdictOptions {
        ValidateVerdictOptions {
            expected_fork_id: "fk_abc123def456".to_string(),
            expected_head: None,
        }
    }

    fn valid_pass_doc() -> Value {
        json!({
            "schema_version": 1,
            "fork_id": "fk_abc123def456",
            "decision": "pass",
            "reason": "모든 게이트 통과",
            "findings": [{ "severity": "P2", "title": "사소한 스타일", "evidence": "src/a.ts:10" }]
        })
    }

    #[test]
    fn valid_pass_document_passes() {
        let r = validate_verdict_data(&valid_pass_doc(), &opts());
        assert!(r.valid);
        assert_eq!(r.effective_decision, VerdictDecision::Pass);
        assert!(r.errors.is_empty());
        let doc = r.document.unwrap();
        assert_eq!(doc.schema_version, 1);
        assert_eq!(doc.findings.len(), 1);
    }

    #[test]
    fn non_object_fails_closed() {
        for raw in [json!(null), json!([]), json!("x"), json!(42)] {
            let r = validate_verdict_data(&raw, &opts());
            assert!(!r.valid);
            assert_eq!(r.effective_decision, VerdictDecision::Human);
            assert_eq!(r.errors, vec!["verdict 문서가 JSON 객체가 아닙니다"]);
        }
    }

    #[test]
    fn bad_schema_version_and_decision_fail_closed() {
        let mut doc = valid_pass_doc();
        doc["schema_version"] = json!(2);
        doc["decision"] = json!("yolo");
        let r = validate_verdict_data(&doc, &opts());
        assert!(!r.valid);
        assert_eq!(r.effective_decision, VerdictDecision::Human);
        assert!(r.errors.iter().any(|e| e.contains("schema_version가 1이 아닙니다")));
        assert!(r.errors.iter().any(|e| e.contains("알 수 없는 decision")));
    }

    #[test]
    fn reason_required_and_length_cap() {
        let mut doc = valid_pass_doc();
        doc["reason"] = json!("   ");
        let r = validate_verdict_data(&doc, &opts());
        assert!(!r.valid);
        assert!(r.errors.iter().any(|e| e.contains("reason가 빈 문자열")));

        let mut doc2 = valid_pass_doc();
        doc2["reason"] = json!("r".repeat(4001));
        let r2 = validate_verdict_data(&doc2, &opts());
        assert!(!r2.valid);
        assert!(r2.errors.iter().any(|e| e.contains("reason가 4000자를 초과")));

        let mut doc3 = valid_pass_doc();
        doc3["action_required"] = json!("a".repeat(4001));
        let r3 = validate_verdict_data(&doc3, &opts());
        assert!(!r3.valid);
        assert!(r3.errors.iter().any(|e| e.contains("action_required가 4000자를 초과")));
    }

    #[test]
    fn findings_schema_violations_accumulate() {
        let mut doc = valid_pass_doc();
        doc["findings"] = json!([
            { "severity": "P9", "title": "  ", "evidence": 1 },
            "not-an-object"
        ]);
        let r = validate_verdict_data(&doc, &opts());
        assert!(!r.valid);
        assert!(r.errors.iter().any(|e| e.contains("findings[0].severity가 허용 값")));
        assert!(r.errors.iter().any(|e| e.contains("findings[0].title이 빈 문자열")));
        assert!(r.errors.iter().any(|e| e.contains("findings[0].evidence가 문자열")));
        assert!(r.errors.iter().any(|e| e.contains("findings[1]가 객체가 아닙니다")));

        let mut doc2 = valid_pass_doc();
        doc2["findings"] = json!("nope");
        let r2 = validate_verdict_data(&doc2, &opts());
        assert!(r2.errors.iter().any(|e| e.contains("findings가 배열이 아닙니다")));

        let mut doc3 = valid_pass_doc();
        doc3["checked_commands"] = json!([1, 2]);
        doc3["artifacts"] = json!(null);
        let r3 = validate_verdict_data(&doc3, &opts());
        assert!(r3.errors.iter().any(|e| e.contains("checked_commands가 문자열 배열")));
        assert!(!r3.errors.iter().any(|e| e.contains("artifacts"))); // null은 undefined 취급
    }

    #[test]
    fn fork_id_mismatch_is_replay_suspect() {
        let mut doc = valid_pass_doc();
        doc["fork_id"] = json!("fk_other99999999");
        let r = validate_verdict_data(&doc, &opts());
        assert!(!r.valid);
        assert!(r.errors.iter().any(|e| e.contains("fork_id 불일치(Replay 의심)")));
    }

    #[test]
    fn head_binding_rules() {
        let full = "abcdef1234567890abcdef1234567890abcdef12";
        // 1) 일치·7자 이상 prefix 허용
        for head in [full.to_string(), full[..10].to_string()] {
            let mut doc = valid_pass_doc();
            doc["target_head"] = json!(head);
            let o = ValidateVerdictOptions {
                expected_fork_id: "fk_abc123def456".into(),
                expected_head: Some(full.to_string()),
            };
            let r = validate_verdict_data(&doc, &o);
            assert!(r.valid, "head={} should match: {:?}", head, r.errors);
        }
        // 2) 불일치
        let mut doc = valid_pass_doc();
        doc["target_head"] = json!("0000000000abcdef");
        let o = ValidateVerdictOptions {
            expected_fork_id: "fk_abc123def456".into(),
            expected_head: Some(full.to_string()),
        };
        let r = validate_verdict_data(&doc, &o);
        assert!(!r.valid);
        assert!(r.errors.iter().any(|e| e.contains("target_head 불일치 (Stale Verdict)")));
        // 3) 6자 prefix는 모호 → 거부
        let mut doc2 = valid_pass_doc();
        doc2["target_head"] = json!(&full[..6]);
        let r2 = validate_verdict_data(&doc2, &o);
        assert!(!r2.valid);
        // 4) expectedHead 있는데 target_head 없음 → 거부
        let mut doc3 = valid_pass_doc();
        doc3.as_object_mut().unwrap().remove("target_head");
        let r3 = validate_verdict_data(&doc3, &o);
        assert!(!r3.valid);
        assert!(r3.errors.iter().any(|e| e.contains("target_head가 없습니다")));
    }

    #[test]
    fn pass_with_p0_or_p1_finding_is_contradiction() {
        for sev in ["P0", "P1"] {
            let mut doc = valid_pass_doc();
            doc["findings"] = json!([{ "severity": sev, "title": "치명", "evidence": "x" }]);
            let r = validate_verdict_data(&doc, &opts());
            assert!(!r.valid, "pass+{} must fail", sev);
            assert_eq!(r.effective_decision, VerdictDecision::Human);
            assert!(r.errors.iter().any(|e| e.contains("pass 판정에 P0/P1 finding이 1건")));
        }
        // P2/info는 통과
        for sev in ["P2", "info"] {
            let mut doc = valid_pass_doc();
            doc["findings"] = json!([{ "severity": sev, "title": "경미", "evidence": "x" }]);
            let r = validate_verdict_data(&doc, &opts());
            assert!(r.valid, "pass+{} must pass", sev);
        }
    }

    #[test]
    fn fail_or_rework_requires_evidence() {
        // findings 없음 → 거부
        for decision in ["fail", "rework"] {
            let mut doc = valid_pass_doc();
            doc["decision"] = json!(decision);
            doc["findings"] = json!([]);
            let r = validate_verdict_data(&doc, &opts());
            assert!(!r.valid);
            assert!(r.errors.iter().any(|e| e.contains("findings가 없습니다")));

            // evidence 전부 공백 → 거부
            let mut doc2 = valid_pass_doc();
            doc2["decision"] = json!(decision);
            doc2["findings"] = json!([{ "severity": "P0", "title": "t", "evidence": "  " }]);
            let r2 = validate_verdict_data(&doc2, &opts());
            assert!(!r2.valid);
            assert!(r2.errors.iter().any(|e| e.contains("비공백 evidence가 1건도 없습니다")));

            // 비공백 evidence 1건 → 유효
            let mut doc3 = valid_pass_doc();
            doc3["decision"] = json!(decision);
            doc3["findings"] = json!([{ "severity": "P0", "title": "t", "evidence": " src/a.ts " }]);
            let r3 = validate_verdict_data(&doc3, &opts());
            assert!(r3.valid);
        }
    }

    #[test]
    fn human_requires_action_required() {
        let mut doc = valid_pass_doc();
        doc["decision"] = json!("human");
        let r = validate_verdict_data(&doc, &opts());
        assert!(!r.valid);
        assert!(r.errors.iter().any(|e| e.contains("human 판정에 action_required")));

        doc["action_required"] = json!("사람이 결정해야 합니다");
        let r2 = validate_verdict_data(&doc, &opts());
        assert!(r2.valid);
        assert_eq!(r2.effective_decision, VerdictDecision::Human);
    }

    #[test]
    fn schema_version_float_one_is_accepted_like_js() {
        let mut doc = valid_pass_doc();
        doc["schema_version"] = json!(1.0); // serde_json은 1.0을 f64로 유지
        let r = validate_verdict_data(&doc, &opts());
        assert!(r.valid, "JS 1.0 === 1 과 동일 취급: {:?}", r.errors);
    }

    #[test]
    fn file_validation_missing_oversized_malformed() {
        let tmp = temp_dir("verdict-file");
        std::fs::create_dir_all(&tmp).unwrap();

        // 미존재
        let missing = tmp.join("nope.json");
        let r = validate_verdict_file(&missing, &opts());
        assert!(!r.valid);
        assert!(r.errors[0].contains("찾을 수 없습니다"));

        // 정상 파일
        let ok_path = tmp.join("fk_abc123def456-verdict.json");
        std::fs::write(&ok_path, serde_json::to_string(&valid_pass_doc()).unwrap()).unwrap();
        let r2 = validate_verdict_file(&ok_path, &opts());
        assert!(r2.valid);

        // 크기 초과 (max를 10바이트로)
        let r3 = validate_verdict_file_with_max(&ok_path, &opts(), 10);
        assert!(!r3.valid);
        assert!(r3.errors[0].contains("크기 초과"));

        // 깨진 JSON
        let bad_path = tmp.join("bad.json");
        std::fs::write(&bad_path, "{not json").unwrap();
        let r4 = validate_verdict_file(&bad_path, &opts());
        assert!(!r4.valid);
        assert_eq!(r4.effective_decision, VerdictDecision::Human);
        assert!(r4.errors[0].contains("JSON 파싱 실패"));

        // 디렉터리 경로
        let r5 = validate_verdict_file(&tmp, &opts());
        assert!(!r5.valid);
        assert!(r5.errors[0].contains("파일이 아닙니다"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_verdict_path_layout() {
        let p = resolve_verdict_path(Path::new("/repo"), "fk_abc");
        assert_eq!(p, PathBuf::from("/repo/.herdr/verify/fk_abc-verdict.json"));
    }

    #[test]
    fn retro_markdown_full_layout() {
        let tools = (1..=7)
            .map(|i| RetroFailedTool {
                tool_name: format!("tool_{}", i),
                count: i,
                last_error: Some(format!("err {} | pipe", i)),
            })
            .collect();
        let input = RetroInput {
            fork_id: "fk_abc123def456".to_string(),
            project_label: Some("Gemini_trick"),
            route: "tool_select",
            tool_class: "shell_exec",
            status: "fail",
            attempts: Some(2),
            cause: Some("tool_error"),
            rework_count: Some(1),
            reason: Some("bash  exit 1  multiple\nlines"),
            top_failed_tools: tools,
            recommendations: vec!["r1".into(), "r2".into(), "r3".into(), "r4".into()],
            sanitize: Some(|s| s.to_string()),
        };
        let md = build_retro_markdown(&input);
        assert!(md.starts_with("# Loop Retro — fk_abc123def456\n"));
        assert!(md.contains("- 프로젝트: Gemini-trick") || md.contains("- 프로젝트: Gemini_trick"));
        assert!(md.contains("- 라우트 / 도구 클래스: tool_select / shell_exec"));
        assert!(md.contains("| 상태 | fail |"));
        assert!(md.contains("| 원인 | tool_error |"));
        assert!(md.contains("| 시도 횟수 | 2 |"));
        assert!(md.contains("| 재작업 횟수 | 1 |"));
        // whitespace 정규화
        assert!(md.contains("> reason: bash exit 1 multiple lines"));
        // 상위 5건만 + 파이프 이스케이프
        assert!(md.contains("| 1 | tool_1 | 1 | err 1 \\| pipe |"));
        assert!(!md.contains("tool_6"));
        assert!(!md.contains("tool_7"));
        // 권고는 최대 3줄
        assert!(md.contains("1. r1\n2. r2\n3. r3\n"));
        assert!(!md.contains("4. r4"));
        assert!(md.ends_with('\n'));
    }

    #[test]
    fn retro_markdown_empty_fallbacks() {
        let input = RetroInput {
            fork_id: "fk_x".to_string(),
            project_label: Some("  "),
            route: "file_select",
            tool_class: "file_read",
            status: "open",
            attempts: None,
            cause: Some(""),
            rework_count: None,
            reason: None,
            top_failed_tools: vec![],
            recommendations: vec![],
            sanitize: None,
        };
        let md = build_retro_markdown(&input);
        assert!(!md.contains("프로젝트:")); // 빈 라벨은 줄 생략
        assert!(md.contains("| 시도 횟수 | - |"));
        assert!(md.contains("| 재작업 횟수 | - |"));
        assert!(md.contains("| 원인 | - |"));
        assert!(md.contains("(실패 도구 표본 없음)"));
        assert!(md.contains("1. (권고 없음 — 원인 데이터를 보강한 뒤 다시 생성한다)"));
    }

    #[test]
    fn retro_snippet_truncated_to_200_chars() {
        let long_error = "E".repeat(500);
        let input = RetroInput {
            fork_id: "fk_t".to_string(),
            project_label: None,
            route: "r",
            tool_class: "c",
            status: "fail",
            attempts: None,
            cause: None,
            rework_count: None,
            reason: None,
            top_failed_tools: vec![RetroFailedTool {
                tool_name: "t".into(),
                count: 1,
                last_error: Some(long_error),
            }],
            recommendations: vec![],
            sanitize: None,
        };
        let md = build_retro_markdown(&input);
        assert!(md.contains(&"E".repeat(200)));
        assert!(!md.contains(&"E".repeat(201)));
    }

    #[test]
    fn sanitize_applied_before_truncation() {
        // sanitize가 먼저 적용되고 그 결과가 잘린다
        let input = RetroInput {
            fork_id: "fk_s".to_string(),
            project_label: None,
            route: "r",
            tool_class: "c",
            status: "fail",
            attempts: None,
            cause: None,
            rework_count: None,
            reason: None,
            top_failed_tools: vec![RetroFailedTool {
                tool_name: "t".into(),
                count: 1,
                last_error: Some("x".repeat(210)),
            }],
            recommendations: vec![],
            sanitize: Some(|s| format!("[R]{}", s)),
        };
        let md = build_retro_markdown(&input);
        // "[R]" + 200자 x → 셀 길이 203 (절단은 sanitize 이후)
        let cell = format!("[R]{}", "x".repeat(197));
        assert!(md.contains(&cell));
    }

    #[test]
    fn resolve_git_head_returns_repo_head() {
        let cwd = std::env::current_dir().unwrap();
        let head = resolve_git_head(&cwd);
        assert!(head.is_some());
        assert!(head.unwrap().len() >= 40);
        assert!(resolve_git_head(&temp_dir("no-repo-here")).is_none());
    }
}
