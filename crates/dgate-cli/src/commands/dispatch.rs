//! `dg dispatch` — Layer 0 Pre-Lead 디스패처(scripts/dispatch.ts 대응).
//!
//! 태스크(task_kind 7종)를 엔진으로 평가해 sharp fast-path(읽기전용 탐색 워커
//! 직행) 또는 split(리드 승격) 실행 계획을 산출한다. LLM 턴 0.
//! `--dry-run`은 `:memory:` DB 리허설 — fork는 `fk_dry_*` 가명, 사용자 DB 미오염.
//! `--brief-file`(Change ①)은 검증자 브리프 형식을 입구에서 검증해 워커 프롬프트에 주입하고,
//! `--after <fork_id>`(Change ③)는 종속 fork가 닫히기 전까지 발행을 미룬다(ready-frontier).
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::args::ParsedArgs;
use crate::util::prelude::*;
use crate::commands::route::parse_sharp_split;
use dgate_core::adapter::{
    self, DecisionAdapterInput, DecisionApiMode,
};
use dgate_core::db::{self, OpenDbOptions, ProjectIdentity};
use dgate_core::engine::{self, RouteEvalInput, RiskLevel};
use dgate_core::roster::{
    self, BuildWorkerCommandOptions, RoleSlot, ROLE_SLOTS,
};
use dgate_core::stats::{compute_dynamic_sharp_threshold, DynamicSharpThresholdInput};

pub const MAX_CONCURRENT_EXPLORE: i64 = 4;
pub const MAX_CONCURRENT_CODING: i64 = 2;
/// sublead 동시 상한 기본값·허용 범위(docs/sublead-design.md 항목 6: 기본 2, 설정 2~4).
pub const DEFAULT_MAX_CONCURRENT_SUBLEADS: i64 = 2;
pub const MIN_CONCURRENT_SUBLEADS: i64 = 2;
pub const MAX_CONCURRENT_SUBLEADS_LIMIT: i64 = 4;

pub fn clamp_max_subleads(raw: Option<i64>) -> i64 {
    match raw {
        Some(v) => v.clamp(MIN_CONCURRENT_SUBLEADS, MAX_CONCURRENT_SUBLEADS_LIMIT),
        None => DEFAULT_MAX_CONCURRENT_SUBLEADS,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcurrencyBudgetResult {
    pub deferred: bool,
    pub reason: Option<String>,
    pub effective_coding: i64,
    pub max_subleads: i64,
}

/// 동시성 예산 판정(docs/sublead-design.md 항목 6 — TS `evaluateConcurrencyBudget` 미러).
pub fn evaluate_concurrency_budget(
    is_sublead_request: bool,
    requires_write: bool,
    decision: &str,
    active_explore: i64,
    active_coding: i64,
    tree_active_coding: i64,
    active_subleads: i64,
    max_subleads_opt: Option<i64>,
) -> ConcurrencyBudgetResult {
    let max_subleads = clamp_max_subleads(max_subleads_opt);
    let effective_coding = active_coding.max(tree_active_coding);
    if is_sublead_request {
        if active_subleads >= max_subleads {
            return ConcurrencyBudgetResult {
                deferred: true,
                reason: Some(format!("sublead concurrency cap ({max_subleads}) reached")),
                effective_coding,
                max_subleads,
            };
        }
        return ConcurrencyBudgetResult {
            deferred: false,
            reason: None,
            effective_coding,
            max_subleads,
        };
    }
    if decision == "sharp" && active_explore >= MAX_CONCURRENT_EXPLORE {
        return ConcurrencyBudgetResult {
            deferred: true,
            reason: Some(format!("explore concurrency cap ({MAX_CONCURRENT_EXPLORE}) reached")),
            effective_coding,
            max_subleads,
        };
    }
    if requires_write && effective_coding >= MAX_CONCURRENT_CODING {
        return ConcurrencyBudgetResult {
            deferred: true,
            reason: Some(format!("coding concurrency cap ({MAX_CONCURRENT_CODING}) reached")),
            effective_coding,
            max_subleads,
        };
    }
    ConcurrencyBudgetResult {
        deferred: false,
        reason: None,
        effective_coding,
        max_subleads,
    }
}

fn count_tree_active_coding_forks(conn: &rusqlite::Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM forks WHERE status IN ('open', 'rework') AND tool_class = 'file_edit'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

/// `hc --name` 계약 — ^[a-z0-9_-]{1,32}$
pub fn session_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// 6자리 hex run id — 모든 hc --name이 32자를 초과하지 않게 한다(N-NEW-4·N12).
pub fn generate_run_id() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 3];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------- 브리프 파일 검증 (Change ① — mattpocock/skills code-review 두 축 + AGENT-BRIEF) ----------

pub const BRIEF_FILE_MAX_BYTES: usize = 64 * 1024;
pub const BLAST_RADIUS_MAX_CHARS: usize = 500;

/// 검증자 브리프 본문 형식 검증 — 순수 함수(단위 테스트 대상). TS `validateBriefText`와 동일 규칙.
/// 필수 요소: `## Standards`·`## Spec`·`## Merge Danger` 헤딩(두 축 + 합류 위험),
/// `door: one-way|two-way` 행, 비어있지 않은 `blast_radius:` 행(값 ≤500자).
/// 위반 목록(빈 벡터 = 통과)을 반환한다.
pub fn validate_brief_text(text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    let mut has_standards = false;
    let mut has_spec = false;
    let mut has_merge_danger = false;
    let mut door_ok = false;
    let mut blast_radius_ok = false;
    for raw_line in text.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.starts_with("## Standards") {
            has_standards = true;
        }
        if line.starts_with("## Spec") {
            has_spec = true;
        }
        if line.starts_with("## Merge Danger") {
            has_merge_danger = true;
        }
        if let Some(rest) = line.trim_start().strip_prefix("door:") {
            let value = rest.trim();
            if value == "one-way" || value == "two-way" {
                door_ok = true;
            }
        }
        if let Some(rest) = line.trim_start().strip_prefix("blast_radius:") {
            let value = rest.trim();
            let len = value.chars().count();
            if len > BLAST_RADIUS_MAX_CHARS {
                violations.push(format!("blast_radius {len}자 — 최대 {BLAST_RADIUS_MAX_CHARS}자 초과"));
            } else if !value.is_empty() {
                blast_radius_ok = true;
            }
        }
    }
    if !has_standards {
        violations.push("## Standards 헤딩 없음 (두 축 ① 기술 표준 위반 검토)".into());
    }
    if !has_spec {
        violations.push("## Spec 헤딩 없음 (두 축 ② 요구사항 충실도 검토)".into());
    }
    if !has_merge_danger {
        violations.push("## Merge Danger 헤딩 없음".into());
    }
    if !door_ok {
        violations.push("door 필드 없음/무효 (door: one-way|two-way 형식 필요)".into());
    }
    if !blast_radius_ok && !violations.iter().any(|v| v.starts_with("blast_radius")) {
        violations.push("blast_radius 필드 없음/빈값".into());
    }
    violations
}

/// 브리프 파일 원시 바이트 검증 — 64KB 상한 + UTF-8 유효성 우선 검사 후 본문 규칙을 적용한다.
/// TS `validateBriefFileBytes`와 동일 규칙.
pub fn validate_brief_bytes(bytes: &[u8]) -> Vec<String> {
    if bytes.len() > BRIEF_FILE_MAX_BYTES {
        return vec![format!("파일 크기 초과: {} > {BRIEF_FILE_MAX_BYTES} bytes (≤64KB)", bytes.len())];
    }
    match std::str::from_utf8(bytes) {
        Err(_) => vec!["유효한 UTF-8 아님".into()],
        Ok(text) => validate_brief_text(text),
    }
}

/// `hc ls --json`에서 live 터미널 식별자 수집 — 실패 시 빈 목록(fail-open).
pub fn list_hc_live_names() -> Vec<String> {
    let out = std::process::Command::new("hc")
        .args(["ls", "--json"])
        .output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    let parsed: Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(terminals) = parsed.get("terminals").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for t in terminals {
        if let Some(n) = t.get("name").and_then(|v| v.as_str()) {
            if !n.is_empty() {
                names.push(n.to_string());
            }
        }
        if let Some(a) = t.get("address").and_then(|v| v.as_str()) {
            names.push(a.to_string());
        }
    }
    names
}

/// 역할 계약서 경로 — repo 우선, 없으면 오케스트레이터 폴백.
fn resolve_role_contract_path(repo_dir: &Path, orchestrator_root: &Path, slot: &str) -> Option<String> {
    let repo_candidate = repo_dir.join(".herdr").join("roles").join(format!("{slot}.md"));
    if repo_candidate.exists() {
        return Some(repo_candidate.to_string_lossy().into_owned());
    }
    let orch_candidate = orchestrator_root.join(".herdr").join("roles").join(format!("{slot}.md"));
    if orch_candidate.exists() {
        return Some(orch_candidate.to_string_lossy().into_owned());
    }
    None
}

fn orchestrator_root() -> PathBuf {
    std::env::var("DGATE_ORCHESTRATOR_ROOT")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(dgate_core::setup::default_repo_root)
}

struct DispatchPlan {
    ok: bool,
    deferred: bool,
    reason: Option<String>,
    fork_id: Option<String>,
    task_kind: Option<String>,
    route: Option<String>,
    tool_class: Option<String>,
    requires_write: Option<bool>,
    risk_level: Option<String>,
    p: Option<f64>,
    basis: Option<String>,
    samples: i64,
    consecutive_errors: i64,
    decision: Option<String>,
    forced_split: bool,
    escalate_carbon: bool,
    ttl_ms: i64,
    session_name: Option<String>,
    hc_commands: Vec<Vec<String>>,
    worktree_commands: Vec<Vec<String>>,
    carbon_brief_path: Option<String>,
    db_path: String,
    run_id: String,
    target_repo: String,
    roster_source: String,
    roster_source_path: Option<String>,
    roster_warnings: Vec<String>,
    role_summary: Map<String, Value>,
    sharp_threshold: f64,
    decision_api: Option<Value>,
    /// 쓰기 태스크 브리프에 파일·줄번호 코드 앵커가 없을 때 경량 explore 선행을 권고한다.
    suggest_explore_first: bool,
}

struct PlanInput {
    task_kind: String,
    risk: Option<RiskLevel>,
    repo_dir: PathBuf,
    db_path: Option<String>,
    run_id: Option<String>,
    dry_run: bool,
    task_file: Option<String>,
    idx: i64,
    active_explore: i64,
    active_coding: i64,
    role: Option<String>,
    active_subleads: i64,
    max_subleads: Option<i64>,
    tree_active_coding: Option<i64>,
    roster_path: Option<String>,
    /// 검증 통과한 검증자 브리프 경로(Change ①) — 워커 프롬프트 `{brief}`로 주입
    brief_file: Option<String>,
    /// `--after <fork_id>`(Change ③) — 종속 fork가 닫힌 뒤에만 발행
    after_fork: Option<String>,
}

/// Decision API 오버라이드 — on+applied 판정만 반영.
struct ApiOverride {
    decision: String,
    p: f64,
    risk: RiskLevel,
    audit: Option<adapter::DecisionApiAudit>,
}

fn empty_plan(run_id: &str, db_path: &str) -> DispatchPlan {
    DispatchPlan {
        ok: false,
        deferred: false,
        reason: None,
        fork_id: None,
        task_kind: None,
        route: None,
        tool_class: None,
        requires_write: None,
        risk_level: None,
        p: None,
        basis: None,
        samples: 0,
        consecutive_errors: 0,
        decision: None,
        forced_split: false,
        escalate_carbon: false,
        ttl_ms: engine::TTL_EXPLORE_MS,
        session_name: None,
        hc_commands: Vec::new(),
        worktree_commands: Vec::new(),
        carbon_brief_path: None,
        db_path: db_path.to_string(),
        run_id: run_id.to_string(),
        target_repo: String::new(),
        roster_source: "builtin".to_string(),
        roster_source_path: None,
        roster_warnings: Vec::new(),
        role_summary: Map::new(),
        sharp_threshold: engine::SHARP_P_THRESHOLD,
        decision_api: None,
        suggest_explore_first: false,
    }
}

fn plan_dispatch_inner(input: &PlanInput, api: Option<&ApiOverride>) -> DispatchPlan {
    let run_id = input.run_id.clone().unwrap_or_else(generate_run_id);
    let db_path = input.db_path.clone().unwrap_or_else(|| {
        if input.dry_run {
            ":memory:".to_string()
        } else {
            db::resolve_db_path(None).display().to_string()
        }
    });
    let mut plan = empty_plan(&run_id, &db_path);

    let Some(spec) = engine::task_kind_spec(&input.task_kind) else {
        plan.reason = Some(format!("invalid task_kind: {}", input.task_kind));
        plan.target_repo = input.repo_dir.display().to_string();
        return plan;
    };
    let mut risk = engine::escalate_risk(spec.default_risk, input.risk);
    let identity: ProjectIdentity = db::resolve_project_identity(&input.repo_dir);

    // Change ③ — `--after <fork_id>` ready-frontier: 종속 fork는 읽기 전용 조회로 먼저 검증.
    // 미발견·DB 부재는 fail-closed(ok:false → exit 2).
    let after_fork_id: Option<&str> = input
        .after_fork
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut after_active = false;
    let mut after_status: Option<String> = None;
    if let Some(af) = after_fork_id {
        let lookup = engine::read_fork_status(&db_path, af);
        if !lookup.exists {
            plan.reason = Some(format!("--after fork not found: {af}"));
            plan.target_repo = input.repo_dir.display().to_string();
            return plan;
        }
        after_status = lookup.status;
        after_active = after_status.as_deref().is_some_and(engine::is_fork_live_status);
    }

    // W3.5 — roster에서 기본 프로필 해석
    let loaded_roster = roster::load_roster_config(
        input.roster_path.as_deref(),
        Some(input.repo_dir.as_path()),
    );
    let (_name, roster_profile, _fb) =
        roster::resolve_roster_profile(&loaded_roster.config, &loaded_roster.config.default_profile);

    let mut post = dgate_core::engine::PosteriorResult {
        p: engine::prior_p0(spec.route, spec.tool_class),
        basis: dgate_core::engine::DecisionBasis::Prior,
        samples: 0,
        consecutive_errors: 0,
        lambda: 0.0,
        p_hier: engine::prior_p0(spec.route, spec.tool_class),
        ewma_success: 0.0,
        ewma_fail: 0.0,
    };
    let mut forced = dgate_core::engine::ForcedState {
        forced_split: false,
        forced_reason: None,
        escalate_carbon: false,
        key_consecutive_errors: 0,
        session_consec_err: 0,
    };
    let mut decision: String = "split".to_string();

    // DB 연결 — dry-run은 명시 dbPath를 읽기전용, 없으면 :memory:
    let conn = if input.dry_run {
        match &input.db_path {
            Some(p) => db::open_db_safe(
                Some(p),
                OpenDbOptions { read_only: true, busy_timeout_ms: Some(1000) },
            ),
            None => db::open_db_safe(
                Some(":memory:"),
                OpenDbOptions { read_only: false, busy_timeout_ms: Some(250) },
            ),
        }
    } else {
        db::open_db_safe(
            Some(&db_path),
            OpenDbOptions { read_only: false, busy_timeout_ms: Some(5000) },
        )
    };

    // PB-2A: 동적 θ_sharp — 프로필 비용 + agy_budget 오늘 사용량
    let mut bucket_usage: HashMap<String, i64> = HashMap::new();
    if let Some(db) = conn.as_ref() {
        if !input.dry_run {
            if let Ok(Some(budget)) = db::get_agy_budget(db, now_ms()) {
                bucket_usage.insert("agy".to_string(), budget.turns_used);
            }
        }
    }
    let dyn_threshold = compute_dynamic_sharp_threshold(&DynamicSharpThresholdInput {
        profile: &roster_profile,
        bucket_usage: Some(&bucket_usage),
        switch_cost: None,
        floor: None,
        ceil: None,
    });
    let sharp_threshold = dyn_threshold.threshold;

    let mut db_open = conn.is_some();
    if let Some(db) = conn.as_ref() {
        let eval = (|| -> Option<dgate_core::engine::RouteEvalResult> {
            if !input.dry_run {
                engine::settle_orphans(db, now_ms()).ok()?;
            }
            engine::evaluate_route(
                db,
                &RouteEvalInput {
                    project_id: &identity.project_id,
                    task_kind: spec.task_kind.as_str(),
                    risk: input.risk,
                    conversation_id: None,
                    sharp_threshold: Some(sharp_threshold),
                },
            )
            .ok()?
        })();
        match eval {
            Some(ev) => {
                post = ev.post;
                forced = ev.forced;
                decision = ev.decision.to_string();
            }
            None => {
                db_open = false; // §3.5 휴리스틱 폴백
            }
        }
    }
    if !db_open {
        // §2.0 sharp 패스트패스 4조건 + 강제 split (cmdRoute와 동일 판정)
        decision = if forced.forced_split {
            "split".to_string()
        } else if spec.sharp_eligible
            && !spec.requires_write
            && risk == RiskLevel::Low
            && post.p >= sharp_threshold
            && post.consecutive_errors < 2
        {
            "sharp".to_string()
        } else {
            "split".to_string()
        };
    }

    // Decision API 오버라이드 — on+applied만 decision/p/risk에 반영
    if let Some(api) = api {
        if let Some(a) = &api.audit {
            plan.decision_api = Some(crate::util::audit_to_value(a));
            if a.applied {
                decision = api.decision.clone();
                post.p = api.p;
                risk = api.risk;
            }
        }
    }

    let idx = input.idx;
    let expected_session = if decision == "sharp" {
        format!("exp-{idx}-{run_id}")
    } else {
        format!("lead-{run_id}")
    };
    let is_sublead_request = input
        .role
        .as_deref()
        .map(|r| r.trim().eq_ignore_ascii_case("sublead"))
        .unwrap_or(false);
    let caller_is_sublead = std::env::var("HERDR_ROLE")
        .ok()
        .map(|r| r.trim().eq_ignore_ascii_case("sublead"))
        .unwrap_or(false);
    let tree_active_coding = match input.tree_active_coding {
        Some(v) => v,
        None if !input.dry_run && caller_is_sublead => conn
            .as_ref()
            .map(count_tree_active_coding_forks)
            .unwrap_or(0),
        None => 0,
    };
    let max_subleads_env = std::env::var("DGATE_MAX_SUBLEADS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok());
    let budget = evaluate_concurrency_budget(
        is_sublead_request,
        spec.requires_write,
        &decision,
        input.active_explore,
        input.active_coding,
        tree_active_coding,
        input.active_subleads,
        input.max_subleads.or(max_subleads_env),
    );
    let mut deferred = false;
    let mut reason: Option<String> = None;
    if after_active {
        deferred = true;
        reason = Some(format!(
            "after_fork {} not closed (status={})",
            after_fork_id.unwrap_or_default(),
            after_status.as_deref().unwrap_or_default()
        ));
    } else if budget.deferred {
        deferred = true;
        reason = budget.reason;
    }
    // PR-A2 Cooperative Lease — 동일 repo_scope 점유 시 deferred(fail-open)
    if !deferred && !input.dry_run {
        if let Some(db) = conn.as_ref() {
            if let Ok(lease) = engine::acquire_scope_lease(
                db,
                &input.repo_dir.display().to_string(),
                &expected_session,
                Some(&list_hc_live_names()),
                now_ms(),
            ) {
                if !lease.acquired {
                    deferred = true;
                    reason = Some(
                        lease
                            .reason
                            .unwrap_or_else(|| format!("scope lease held by {}", lease.holder.unwrap_or_default())),
                    );
                }
            }
        }
    }

    let mut fork_id: Option<String> = None;
    // after_fork deferred는 fork 미발행(ready-frontier) — 의존 fork가 닫힌 뒤 재디스패치된다
    if !input.dry_run && !after_active {
        if let Some(db) = conn.as_ref() {
            // 리스는 실제 디스패치 때만 기록 — deferred fork는 스코프를 홀드하지 않는다
            let metadata: Option<Map<String, Value>> = if deferred && plan.decision_api.is_none() {
                None
            } else {
                let mut m = Map::new();
                if !deferred {
                    m.insert("repo_scope".into(), json!(input.repo_dir.display().to_string()));
                    m.insert("lease_owner".into(), json!(expected_session));
                    m.insert("lease_until_ms".into(), json!(now_ms() + engine::TTL_EXPLORE_MS));
                }
                if let Some(a) = &plan.decision_api {
                    m.insert("decision_api".into(), a.clone());
                }
                Some(m)
            };
            // 집행 장치 근거 — dispatch 발급 표식(strict 모드에서 wrap이 검증)
            let metadata = match metadata {
                Some(mut m) => {
                    m.insert("dispatch".into(), json!(true));
                    Some(m)
                }
                None => {
                    let mut m = Map::new();
                    m.insert("dispatch".into(), json!(true));
                    Some(m)
                }
            };
            // 종속 fork(종결 상태)를 metadata에 기록 — additive 키(parse_fork_metadata가 그대로 노출)
            let metadata = metadata.map(|mut m| {
                if let Some(af) = after_fork_id {
                    m.insert("after_fork".into(), json!(af));
                }
                m
            });
            let env_conversation_id = std::env::var("HERDR_CONVERSATION")
                .ok()
                .filter(|s| !s.is_empty());
            let issue = engine::issue_fork(
                db,
                &engine::IssueForkInput {
                    project_id: &identity.project_id,
                    project_label: &identity.project_label,
                    route: spec.route,
                    tool_class: spec.tool_class,
                    decision: if decision == "sharp" { "sharp" } else { "split" },
                    p_at_gate: post.p,
                    basis_at_gate: post.basis,
                    forced_split: forced.forced_split,
                    conversation_id: env_conversation_id.as_deref(),
                    ttl_ms: Some(engine::TTL_EXPLORE_MS),
                    title: input.task_file.as_deref(),
                    metadata: metadata.as_ref(),
                    now_ms: None,
                },
            );
            if let Ok(fk) = issue {
                fork_id = Some(fk);
            }
        }
    }
    let fork_id = fork_id.unwrap_or_else(|| format!("fk_dry_{run_id}"));

    let mut session_name: Option<String> = None;
    let mut hc_commands: Vec<Vec<String>> = Vec::new();
    let mut carbon_brief_path: Option<String> = None;
    let orch_root = orchestrator_root();
    // Change ① — 브리프가 주어지면 `{brief}`(= role_brief_path 치환) 문장을 프롬프트 끝에 붙인다.
    // TS scripts/dispatch.ts와 문장·치환 방식을 바이트 단위로 맞춘다.
    let brief_path: Option<&str> = input
        .brief_file
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let brief_note = if brief_path.is_some() {
        " 브리프 파일을 먼저 읽고 두 축(Standards/Spec)과 Merge Danger을 따르라: {brief}"
    } else {
        ""
    };

    if !deferred && decision == "sharp" {
        session_name = Some(expected_session.clone());
        let prompt = format!(
            "임무파일 {} 를 읽고 조사 결과를 최종 메시지로 출력하라{brief_note}",
            input.task_file.clone().unwrap_or_else(|| "TASK.md".into())
        );
        // 네이티브 디스패처 — wrap은 node dg.ts 대신 자기 자신(네이티브 dg)을 쓴다
        let dg_exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "dg".to_string());
        let explore_spec = roster_profile.roles.get(&RoleSlot::Explore);
        let mut argv = vec![
            "run".to_string(),
            "--bg".to_string(),
            "--name".to_string(),
            expected_session.clone(),
            "--cwd".to_string(),
            input.repo_dir.display().to_string(),
            "--".to_string(),
            dg_exe,
            "wrap".to_string(),
            "--route".to_string(),
            spec.route.as_str().to_string(),
            "--tool-class".to_string(),
            spec.tool_class.to_string(),
            "--fork".to_string(),
            fork_id.clone(),
            "--require-stdout-nonempty".to_string(),
            "--stall-ms".to_string(),
            "120000".to_string(),
            "--".to_string(),
        ];
        if let Some(es) = explore_spec {
            argv.extend(roster::build_worker_command(
                es,
                &BuildWorkerCommandOptions { prompt: &prompt, role_brief_path: brief_path, interactive: false },
            ));
        }
        hc_commands.push(argv);
    } else if !deferred {
        // split → 리드 승격(§6.1 Layer 1)
        session_name = Some(expected_session.clone());
        let lead_contract = resolve_role_contract_path(&input.repo_dir, &orch_root, "lead")
            .unwrap_or_else(|| ".herdr/roles/lead.md".to_string());
        let prompt = format!("역할계약서 {lead_contract} 를 읽고 대기/수행하라{brief_note}");
        if let Some(lead_spec) = roster_profile.roles.get(&RoleSlot::Lead) {
            let mut argv = vec![
                "env".to_string(),
                "HERDR_ROLE=lead".to_string(),
                "HERDR_DELEGATE_MODE=hc".to_string(),
                "HERDR_DB_WRITE=1".to_string(),
            ];
            argv.extend(roster::build_worker_command(
                lead_spec,
                &BuildWorkerCommandOptions { prompt: &prompt, role_brief_path: brief_path, interactive: false },
            ));
            hc_commands.push(argv);
        }
        if forced.escalate_carbon {
            carbon_brief_path = Some(format!(".herdr/advice/gate1-{run_id}.md"));
        }
    }

    plan.ok = true;
    plan.deferred = deferred;
    plan.reason = reason;
    plan.fork_id = Some(fork_id);
    plan.task_kind = Some(spec.task_kind.as_str().to_string());
    plan.route = Some(spec.route.as_str().to_string());
    plan.tool_class = Some(spec.tool_class.to_string());
    plan.requires_write = Some(spec.requires_write);
    plan.risk_level = Some(risk.as_str().to_string());
    plan.p = Some(post.p);
    plan.basis = Some(post.basis.as_str().to_string());
    plan.samples = post.samples;
    plan.consecutive_errors = post.consecutive_errors;
    plan.decision = Some(decision);
    plan.forced_split = forced.forced_split;
    plan.escalate_carbon = forced.escalate_carbon;
    plan.ttl_ms = engine::TTL_EXPLORE_MS;
    plan.session_name = session_name;
    plan.hc_commands = hc_commands;
    plan.worktree_commands = Vec::new();
    plan.carbon_brief_path = carbon_brief_path;
    plan.target_repo = input.repo_dir.display().to_string();
    plan.roster_source = loaded_roster.source.as_str().to_string();
    plan.roster_source_path = loaded_roster.source_path.clone();
    plan.roster_warnings = loaded_roster.warnings.clone();
    for slot in ROLE_SLOTS {
        let label = roster_profile
            .roles
            .get(&slot)
            .map(|s| format!("{}:{}", s.cli.as_str(), s.model.clone().unwrap_or_else(|| "-".into())))
            .unwrap_or_else(|| "custom:-".to_string());
        plan.role_summary.insert(slot.as_str().to_string(), json!(label));
    }
    plan.sharp_threshold = sharp_threshold;
    if spec.requires_write {
        if let Some(bp) = brief_path {
            if let Ok(text) = std::fs::read_to_string(bp) {
                plan.suggest_explore_first = !dgate_core::pipeline::brief_has_code_anchors(&text);
            }
        }
    }
    if let Some(sn) = &plan.session_name {
        if !session_name_ok(sn) {
            plan.ok = false;
            plan.reason = Some(format!("session name violates hc --name contract: {sn}"));
        }
    }
    plan
}

fn plan_to_json(plan: &DispatchPlan) -> Value {
    let mut v = json!({
        "ok": plan.ok,
        "deferred": plan.deferred,
        "reason": plan.reason,
        "forkId": plan.fork_id,
        "taskKind": plan.task_kind,
        "route": plan.route,
        "toolClass": plan.tool_class,
        "requiresWrite": plan.requires_write,
        "riskLevel": plan.risk_level,
        "p": plan.p,
        "basis": plan.basis,
        "samples": plan.samples,
        "consecutiveErrors": plan.consecutive_errors,
        "decision": plan.decision,
        "forcedSplit": plan.forced_split,
        "escalateCarbon": plan.escalate_carbon,
        "ttlMs": plan.ttl_ms,
        "sessionName": plan.session_name,
        "hcCommands": plan.hc_commands,
        "worktreeCommands": plan.worktree_commands,
        "carbonBriefPath": plan.carbon_brief_path,
        "dbPath": plan.db_path,
        "runId": plan.run_id,
        "targetRepo": plan.target_repo,
        "rosterSource": plan.roster_source,
        "rosterSourcePath": plan.roster_source_path,
        "rosterWarnings": plan.roster_warnings,
        "roleSummary": plan.role_summary,
        "sharp_threshold": plan.sharp_threshold,
        "decision_api": plan.decision_api,
    });
    // TS DispatchPlan JSON 형상 정렬 — 감사가 없으면 키 자체를 생략한다(undefined 미직렬화).
    if plan.decision_api.is_none() {
        if let Some(m) = v.as_object_mut() {
            m.remove("decision_api");
        }
    }
    if plan.suggest_explore_first {
        if let Some(m) = v.as_object_mut() {
            m.insert("suggestExploreFirst".to_string(), json!(true));
        }
    }
    v
}

fn render_plan(plan: &DispatchPlan) {
    let mut out = std::io::stdout().lock();
    use std::io::Write;
    let head = format!(
        "[dispatch] {} → {}{} p={} θ_sharp={:.4} fork={}",
        plan.task_kind.clone().unwrap_or_else(|| "?".into()),
        plan.decision.clone().unwrap_or_else(|| "ERR".into()).to_uppercase(),
        if plan.deferred { " (deferred)" } else { "" },
        plan.p.map(|p| format!("{p:.3}")).unwrap_or_else(|| "-".into()),
        plan.sharp_threshold,
        plan.fork_id.clone().unwrap_or_else(|| "-".into()),
    );
    let _ = writeln!(out, "{head}");
    let _ = writeln!(out, "  target_repo={}", plan.target_repo);
    let _ = writeln!(
        out,
        "  roster_source={}{}",
        plan.roster_source,
        plan.roster_source_path
            .as_deref()
            .map(|p| format!(" ({p})"))
            .unwrap_or_default()
    );
    let roles = plan
        .role_summary
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ");
    let _ = writeln!(out, "  roles: {roles}");
    for w in &plan.roster_warnings {
        let _ = writeln!(out, "  roster warning: {w}");
    }
    for argv in &plan.hc_commands {
        let _ = writeln!(out, "  hc {}", argv.join(" "));
    }
    for argv in &plan.worktree_commands {
        let _ = writeln!(out, "  {}", argv.join(" "));
    }
    if let Some(c) = &plan.carbon_brief_path {
        let _ = writeln!(out, "  carbon brief: {c}");
    }
    if plan.suggest_explore_first {
        let _ = writeln!(
            out,
            "  [hint] 브리프에 파일·줄번호 앵커가 없습니다 — 경량 explore 선행(dg pipeline init --with-explore 또는 dg dispatch --task-kind code_search) 권장"
        );
    }
    if let Some(a) = &plan.decision_api {
        let mode = a.get("mode").and_then(|v| v.as_str()).unwrap_or("?");
        let ok = a.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        if ok {
            let p_api = a.get("p_api").and_then(|v| v.as_f64());
            let applied = a.get("applied").and_then(|v| v.as_bool()).unwrap_or(false);
            let disagreed = a.get("disagreed").and_then(|v| v.as_bool());
            let state = if applied {
                "applied"
            } else if disagreed == Some(true) {
                "disagreed"
            } else {
                "agreed"
            };
            let p_str = p_api.map(|p| format!("p_api={p:.3} ")).unwrap_or_default();
            let _ = writeln!(out, "  decision_api: {mode} {p_str}{state}");
        } else {
            let _ = writeln!(out, "  decision_api: {mode} fallback");
        }
    }
    if let Some(r) = &plan.reason {
        let _ = writeln!(out, "  reason: {r}");
    }
}

/// TS parseCli — 불리언 3종 + `--key value`/`--key=value`(unknown 플래그도 수용).
/// 메인 파서(BOOL_FLAGS)가 이미 동일 결과를 만들므로 ParsedArgs를 그대로 변환한다.

pub fn cmd_dispatch(args: &ParsedArgs, json: bool) -> CmdResult {
    let flags = parse_cli_flags_from_parsed(args);
    // 도움말은 모든 게이트(strict 브리프 포함)보다 먼저 — 도움말조차 못 보면 strict를 켠 사용자가 갇힌다.
    if crate::args::bool_flag(args, "help") || crate::args::bool_flag(args, "h") {
        println!("{}", dispatch_flags_help());
        return Ok(0);
    }
    let raw_mode = flags.get("decision-api").cloned();
    if let Some(m) = &raw_mode {
        if !adapter::is_decision_api_mode(m) {
            crate::util::eprint_line(&format!(
                "[dispatch] invalid --decision-api \"{m}\" (allowed: off|shadow|on)"
            ));
            return Ok(2);
        }
    }
    let risk = flags.get("risk").cloned();
    let risk = match risk.as_deref() {
        Some("low") => Some(RiskLevel::Low),
        Some("medium") => Some(RiskLevel::Medium),
        Some("high") => Some(RiskLevel::High),
        None => None,
        Some(other) => {
            crate::util::eprint_line(&format!(
                "[dispatch] invalid --risk \"{other}\" (allowed: low|medium|high)"
            ));
            return Ok(2);
        }
    };
    // Change ① — 브리프 게이트(입구, plan_dispatch_inner 이전): 명시적 입력은 양 모드에서
    // 반드시 유효해야 하고, strict 모드에서는 필수다(fail-closed). TS scripts/dispatch.ts와 동일 순서.
    let brief_file = flags.get("brief-file").cloned();
    match brief_file.as_deref() {
        Some(p) => match std::fs::read(p) {
            Err(e) => {
                return Err(DgError::Usage(crate::args::UsageError(format!(
                    "--brief-file 읽기 실패 ({p}): {e}"
                ))));
            }
            Ok(bytes) => {
                let violations = validate_brief_bytes(&bytes);
                if !violations.is_empty() {
                    let mut msg = format!("--brief-file 검증 실패 ({p}):");
                    for v in &violations {
                        msg.push_str(&format!("\n  - {v}"));
                    }
                    return Err(DgError::Usage(crate::args::UsageError(msg)));
                }
            }
        },
        None => {
            if engine::read_dispatch_mode() == "strict" {
                return Err(DgError::Usage(crate::args::UsageError(
                    "strict 모드: --brief-file 필수".into(),
                )));
            }
        }
    }
    let input = PlanInput {
        task_kind: flags.get("task-kind").cloned().unwrap_or_default(),
        risk,
        repo_dir: PathBuf::from(
            flags
                .get("repo")
                .or_else(|| flags.get("project"))
                .cloned()
                .unwrap_or_else(|| ".".to_string()),
        ),
        db_path: flags.get("db").cloned(),
        run_id: flags.get("run-id").cloned(),
        dry_run: flags.get("dry-run").map(|v| v == "1").unwrap_or(false),
        task_file: flags.get("task-file").cloned(),
        idx: flags.get("idx").and_then(|v| v.parse().ok()).unwrap_or(0),
        active_explore: flags.get("active-explore").and_then(|v| v.parse().ok()).unwrap_or(0),
        active_coding: flags.get("active-coding").and_then(|v| v.parse().ok()).unwrap_or(0),
        role: flags.get("role").cloned(),
        active_subleads: flags.get("active-subleads").and_then(|v| v.parse().ok()).unwrap_or(0),
        max_subleads: flags.get("max-subleads").and_then(|v| v.parse().ok()),
        tree_active_coding: flags.get("tree-active-coding").and_then(|v| v.parse().ok()),
        roster_path: flags.get("roster").cloned(),
        brief_file: brief_file.clone(),
        after_fork: flags.get("after").cloned(),
    };

    // Decision API — off는 plain planDispatch와 완전 동일, dry-run은 프로브 생략
    let env_map = adapter::process_env_map();
    let mode_override = raw_mode.as_deref().and_then(adapter::DecisionApiMode::parse);
    let mode = adapter::parse_decision_api_mode(&env_map, mode_override);
    let plan = if mode == DecisionApiMode::Off || input.dry_run {
        plan_dispatch_inner(&input, None)
    } else {
        // 읽기 전용 프로브 — 쓰기·fork 발행·리스 없이 로컬 판정만 재계산
        let probe = probe_dispatch_local(&input, mode);
        match probe {
            None => plan_dispatch_inner(&input, None),
            Some((local_decision, p_local, basis, forced_split, threshold, spec)) => {
                let identity = db::resolve_project_identity(&input.repo_dir);
                let adapter = adapter::create_decision_adapter(mode, &env_map);
                let context = flags.get("context").cloned();
                let adapter_input = DecisionAdapterInput {
                    project_id: &identity.project_id,
                    route: spec.route,
                    tool_class: spec.tool_class,
                    basis,
                    p_local,
                    task_kind: Some(spec.task_kind.as_str()),
                    requires_write: Some(spec.requires_write),
                    risk_level: None,
                    samples: None,
                    consecutive_errors: None,
                    title: input.task_file.as_deref(),
                    context: context.as_deref(),
                };
                let force_eval = flags.get("api-eval").map(|v| v == "1").unwrap_or(false)
                    || context.as_deref().map(|c| !c.trim().is_empty()).unwrap_or(false);
                let api = adapter::apply_decision_api(&adapter::ApplyDecisionApiInput {
                    mode,
                    adapter: adapter.as_ref(),
                    local_decision: parse_sharp_split(&local_decision),
                    p_local,
                    sharp_threshold: threshold,
                    basis,
                    sharp_eligible: spec.sharp_eligible,
                    local_risk: input.risk.unwrap_or(spec.default_risk),
                    forced_split,
                    force_api_eval: if force_eval { Some(true) } else { None },
                    adapter_input: &adapter_input,
                });
                let override_ = ApiOverride {
                    decision: api.decision.as_str().to_string(),
                    p: api.p,
                    risk: api.risk,
                    audit: api.audit,
                };
                plan_dispatch_inner(&input, Some(&override_))
            }
        }
    };

    if json {
        let mut so = std::io::stdout().lock();
        use std::io::Write;
        let _ = writeln!(so, "{}", plan_to_json(&plan));
    } else {
        render_plan(&plan);
    }
    Ok(if plan.ok { 0 } else { 2 })
}

/// ParsedArgs(BOOL_FLAGS 경유)를 TS parseCli 결과로 변환한다.
fn parse_cli_flags_from_parsed(args: &ParsedArgs) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for (k, v) in &args.flags {
        m.insert(k.clone(), v.clone());
    }
    m
}

type ProbeResult = (
    String,                                   // local decision
    f64,                                      // p_local
    dgate_core::engine::DecisionBasis,        // basis
    bool,                                     // forced_split
    f64,                                      // sharp threshold
    &'static dgate_core::engine::TaskKindSpec, // spec
);

/// 쓰기 없는 읽기 전용 프로브(TS probeDispatchLocal) — 평가 실패 시 None.
fn probe_dispatch_local(input: &PlanInput, _mode: DecisionApiMode) -> Option<ProbeResult> {
    let spec = engine::task_kind_spec(&input.task_kind)?;
    let risk = engine::escalate_risk(spec.default_risk, input.risk);
    let identity = db::resolve_project_identity(&input.repo_dir);
    let loaded_roster =
        roster::load_roster_config(input.roster_path.as_deref(), Some(&input.repo_dir));
    let (_n, profile, _fb) =
        roster::resolve_roster_profile(&loaded_roster.config, &loaded_roster.config.default_profile);

    let conn = if let Some(p) = &input.db_path {
        db::open_db_safe(
            Some(p),
            OpenDbOptions { read_only: true, busy_timeout_ms: Some(1000) },
        )
    } else if input.dry_run {
        db::open_db_safe(
            Some(":memory:"),
            OpenDbOptions { read_only: false, busy_timeout_ms: Some(250) },
        )
    } else {
        db::open_db_safe(
            None,
            OpenDbOptions { read_only: true, busy_timeout_ms: Some(1000) },
        )
    };

    let mut bucket_usage: HashMap<String, i64> = HashMap::new();
    if let Some(db) = conn.as_ref() {
        if !input.dry_run {
            if let Ok(Some(b)) = db::get_agy_budget(db, now_ms()) {
                bucket_usage.insert("agy".to_string(), b.turns_used);
            }
        }
    }
    let threshold = compute_dynamic_sharp_threshold(&DynamicSharpThresholdInput {
        profile: &profile,
        bucket_usage: Some(&bucket_usage),
        switch_cost: None,
        floor: None,
        ceil: None,
    })
    .threshold;

    let mut post = dgate_core::engine::PosteriorResult {
        p: engine::prior_p0(spec.route, spec.tool_class),
        basis: dgate_core::engine::DecisionBasis::Prior,
        samples: 0,
        consecutive_errors: 0,
        lambda: 0.0,
        p_hier: engine::prior_p0(spec.route, spec.tool_class),
        ewma_success: 0.0,
        ewma_fail: 0.0,
    };
    let mut forced_split = false;
    if let Some(db) = conn.as_ref() {
        if let Ok(Some(ev)) = engine::evaluate_route(
            db,
            &RouteEvalInput {
                project_id: &identity.project_id,
                task_kind: spec.task_kind.as_str(),
                risk: input.risk,
                conversation_id: None,
                sharp_threshold: Some(threshold),
            },
        ) {
            post = ev.post;
            forced_split = ev.forced.forced_split;
        }
    }
    let local_decision = if forced_split {
        "split".to_string()
    } else if spec.sharp_eligible
        && !spec.requires_write
        && risk == RiskLevel::Low
        && post.p >= threshold
        && post.consecutive_errors < 2
    {
        "sharp".to_string()
    } else {
        "split".to_string()
    };
    Some((local_decision, post.p, post.basis, forced_split, threshold, spec))
}

pub fn dispatch_flags_help() -> &'static str {
    "dg dispatch --task-kind <k> [--risk low|medium|high] [--repo <dir>] [--roster <path>] [--db <path>] [--run-id <id>] [--task-file <path>] [--brief-file <path>] [--after <fork_id>] [--idx <n>] [--active-explore <n>] [--active-coding <n>] [--dry-run] [--context <text>] [--decision-api off|shadow|on] [--api-eval] [--json]"
}

// ---------------------------------------------------------------------------
// 단위 테스트 — 브리프 검증기(Change ①)·`--after`(Change ③). CLI 게이트(strict 배선)
// 와 e2e는 `crates/dgate-cli/tests/cli_e2e.rs`가 담당한다.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    const VALID_BRIEF: &str = "\
# Verifier Brief — fk_test

- fork_id: fk_test

## Standards

- 위반 없음

## Spec

- 요구사항 충족

## Merge Danger

door: two-way
blast_radius: 테스트 경로 전반
";

    fn temp_db_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dg-dispatch-ut-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("decision.db")
    }

    fn plan_input(db: &str, after: Option<&str>) -> PlanInput {
        PlanInput {
            task_kind: "single_edit".into(),
            risk: Some(RiskLevel::High), // split 확정 — 리드 승격 경로
            repo_dir: std::env::temp_dir(),
            db_path: Some(db.to_string()),
            run_id: Some("dead01".into()),
            dry_run: false,
            task_file: None,
            idx: 0,
            active_explore: 0,
            active_coding: 0,
            role: None,
            active_subleads: 0,
            max_subleads: None,
            tree_active_coding: None,
            roster_path: None,
            brief_file: None,
            after_fork: after.map(str::to_string),
        }
    }

    #[test]
    fn brief_validator_accepts_valid_template() {
        assert!(validate_brief_text(VALID_BRIEF).is_empty());
        assert!(validate_brief_bytes(VALID_BRIEF.as_bytes()).is_empty());
        // CRLF 허용 — 행 끝 \r 정규화
        let crlf = VALID_BRIEF.replace('\n', "\r\n");
        assert!(validate_brief_text(&crlf).is_empty());
    }

    #[test]
    fn brief_validator_flags_missing_spec_heading() {
        let bad = VALID_BRIEF.replace("## Spec\n", "");
        let v = validate_brief_text(&bad);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("## Spec"));
    }

    #[test]
    fn brief_validator_flags_bad_door() {
        let bad = VALID_BRIEF.replace("door: two-way", "door: maybe");
        let v = validate_brief_text(&bad);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("door"));
    }

    #[test]
    fn brief_validator_flags_empty_and_oversized_blast_radius() {
        let empty = VALID_BRIEF.replace("blast_radius: 테스트 경로 전반", "blast_radius:   ");
        let v = validate_brief_text(&empty);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("blast_radius"));
        let long = format!(
            "## Standards\n## Spec\n## Merge Danger\ndoor: one-way\nblast_radius: {}\n",
            "x".repeat(501)
        );
        let v = validate_brief_text(&long);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("500자"));
    }

    #[test]
    fn brief_validator_flags_oversize_and_invalid_utf8() {
        let big = vec![b'a'; BRIEF_FILE_MAX_BYTES + 1];
        let v = validate_brief_bytes(&big);
        assert_eq!(v.len(), 1);
        assert!(v[0].contains("64KB"));
        let bad_utf8 = vec![0xff, 0xfe, 0xfd];
        assert_eq!(validate_brief_bytes(&bad_utf8), vec!["유효한 UTF-8 아님".to_string()]);
    }

    #[test]
    fn after_not_found_is_usage_failure() {
        let db = temp_db_path("after-nf");
        // DB 파일 자체가 없어도 fail-closed — exists:false → ok:false(exit 2 상당)
        let plan = plan_dispatch_inner(&plan_input(&db.display().to_string(), Some("fk_nope")), None);
        assert!(!plan.ok);
        assert_eq!(plan.reason.as_deref(), Some("--after fork not found: fk_nope"));
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
    }

    #[test]
    fn after_active_fork_defers_without_issuing() {
        let db_path = temp_db_path("after-active");
        let dbs = db_path.display().to_string();
        let conn = dgate_core::db::open_db_safe(
            Some(&dbs),
            OpenDbOptions { read_only: false, busy_timeout_ms: Some(5000) },
        )
        .unwrap();
        let fk = engine::issue_fork(
            &conn,
            &engine::IssueForkInput {
                project_id: "__GLOBAL__",
                project_label: "ut",
                route: dgate_core::engine::RouteId::FileSelect,
                tool_class: "file_edit",
                decision: "split",
                p_at_gate: 0.5,
                basis_at_gate: dgate_core::engine::DecisionBasis::Prior,
                forced_split: false,
                conversation_id: None,
                ttl_ms: None,
                title: None,
                metadata: None,
                now_ms: None,
            },
        )
        .unwrap();
        drop(conn);

        let plan = plan_dispatch_inner(&plan_input(&dbs, Some(&fk)), None);
        assert!(plan.ok, "deferred도 ok:true(기존 지연 방출 형상)");
        assert!(plan.deferred);
        assert_eq!(
            plan.reason.as_deref(),
            Some(format!("after_fork {fk} not closed (status=open)").as_str())
        );
        assert!(plan.hc_commands.is_empty());
        assert!(plan.worktree_commands.is_empty());
        assert!(plan.session_name.is_none());
        assert!(plan.fork_id.as_deref().unwrap_or_default().starts_with("fk_dry_"), "실제 fork는 미발행(가명만)");

        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM forks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "DB에는 종속 fork 1건만 존재");
        std::fs::remove_dir_all(db_path.parent().unwrap()).ok();
    }

    #[test]
    fn after_terminal_fork_proceeds_and_stamps_metadata() {
        let db_path = temp_db_path("after-term");
        let dbs = db_path.display().to_string();
        let conn = dgate_core::db::open_db_safe(
            Some(&dbs),
            OpenDbOptions { read_only: false, busy_timeout_ms: Some(5000) },
        )
        .unwrap();
        let fk = engine::issue_fork(
            &conn,
            &engine::IssueForkInput {
                project_id: "__GLOBAL__",
                project_label: "ut",
                route: dgate_core::engine::RouteId::FileSelect,
                tool_class: "file_edit",
                decision: "split",
                p_at_gate: 0.5,
                basis_at_gate: dgate_core::engine::DecisionBasis::Prior,
                forced_split: false,
                conversation_id: None,
                ttl_ms: None,
                title: None,
                metadata: None,
                now_ms: None,
            },
        )
        .unwrap();
        engine::close_fork(&conn, &fk, "ok", "dispatcher", None, &engine::CloseForkOptions::default())
            .unwrap();
        drop(conn);

        let plan = plan_dispatch_inner(&plan_input(&dbs, Some(&fk)), None);
        assert!(plan.ok);
        assert!(!plan.deferred);
        assert_eq!(plan.hc_commands.len(), 1, "리드 승격 커맨드 1건");
        assert!(plan.fork_id.as_deref().unwrap_or_default().starts_with("fk_"));

        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let meta: Option<String> = conn
            .query_row(
                "SELECT metadata_json FROM forks WHERE fork_id = ?1",
                [&plan.fork_id],
                |r| r.get(0),
            )
            .unwrap();
        let meta: serde_json::Value = serde_json::from_str(meta.as_deref().unwrap()).unwrap();
        assert_eq!(meta["after_fork"].as_str(), Some(fk.as_str()));
        assert_eq!(meta["dispatch"].as_bool(), Some(true));
        std::fs::remove_dir_all(db_path.parent().unwrap()).ok();
    }

    #[test]
    fn brief_file_injects_prompt_sentence_with_path_substitution() {
        let db = temp_db_path("brief");
        let brief = db.parent().unwrap().join("brief.md");
        std::fs::write(&brief, VALID_BRIEF).unwrap();
        let mut input = plan_input(&db.display().to_string(), None);
        input.dry_run = true; // :memory: 리허설 — DB 파일 미생성
        input.db_path = None;
        input.brief_file = Some(brief.display().to_string());
        let plan = plan_dispatch_inner(&input, None);
        assert!(plan.ok);
        assert_eq!(plan.hc_commands.len(), 1);
        let prompt = plan.hc_commands[0].last().unwrap();
        assert!(
            prompt.ends_with(&format!(
                "브리프 파일을 먼저 읽고 두 축(Standards/Spec)과 Merge Danger을 따르라: {}",
                brief.display()
            )),
            "프롬프트 끝에 브리프=경로 문장 주입: {prompt}"
        );
        assert!(!plan.hc_commands[0].join(" ").contains("{brief}"), "플레이스홀더 잔존 금지");
        std::fs::remove_dir_all(db.parent().unwrap()).ok();
    }

    #[test]
    fn write_brief_without_code_anchors_sets_suggest_explore_first() {
        let db = temp_db_path("explore-hint");
        let vague_brief = db.parent().unwrap().join("vague.md");
        std::fs::write(
            &vague_brief,
            "## Standards\n깔끔하게 구현\n## Spec\n검색 UX를 개선한다.\n## Merge Danger\ndoor: two-way\nblast_radius: 홈 검색 화면\n",
        )
        .unwrap();
        let anchored_brief = db.parent().unwrap().join("anchored.md");
        std::fs::write(
            &anchored_brief,
            "## Standards\n깔끔하게 구현\n## Spec\n`HomeCatalogView.vue:120-180` 디바운스 수정\n## Merge Danger\ndoor: two-way\nblast_radius: 홈 검색 화면\n",
        )
        .unwrap();

        let mut input = plan_input(&db.display().to_string(), None);
        input.task_kind = "multi_edit".to_string();
        input.dry_run = true;
        input.db_path = None;
        input.brief_file = Some(vague_brief.display().to_string());
        let plan_vague = plan_dispatch_inner(&input, None);
        assert!(plan_vague.suggest_explore_first);
        assert_eq!(plan_to_json(&plan_vague)["suggestExploreFirst"], json!(true));

        input.brief_file = Some(anchored_brief.display().to_string());
        let plan_anchored = plan_dispatch_inner(&input, None);
        assert!(!plan_anchored.suggest_explore_first);
        assert!(plan_to_json(&plan_anchored).get("suggestExploreFirst").is_none());

        std::fs::remove_dir_all(db.parent().unwrap()).ok();
    }

    #[test]
    fn issue_fork_persists_conversation_id_roundtrip() {
        let db_path = temp_db_path("conv-owner");
        let dbs = db_path.display().to_string();
        let conn = dgate_core::db::open_db_safe(
            Some(&dbs),
            OpenDbOptions { read_only: false, busy_timeout_ms: Some(5000) },
        )
        .unwrap();
        let fk = engine::issue_fork(
            &conn,
            &engine::IssueForkInput {
                project_id: "__GLOBAL__",
                project_label: "ut",
                route: dgate_core::engine::RouteId::FileSelect,
                tool_class: "file_edit",
                decision: "split",
                p_at_gate: 0.5,
                basis_at_gate: dgate_core::engine::DecisionBasis::Prior,
                forced_split: false,
                conversation_id: Some("conv-ut"),
                ttl_ms: None,
                title: None,
                metadata: None,
                now_ms: None,
            },
        )
        .unwrap();
        let row = engine::get_fork(&conn, &fk)
            .unwrap()
            .expect("발급된 fork가 DB에 존재해야 함");
        assert_eq!(row.conversation_id.as_deref(), Some("conv-ut"));
        drop(conn);
        std::fs::remove_dir_all(db_path.parent().unwrap()).ok();
    }

    #[test]
    fn concurrency_budget_enforces_sublead_cap_and_whole_tree_coding_cap() {
        // sublead는 coder 슬롯이 꽉 차도 소모하지 않으며, sublead 상한(2~4 클램프)만 집행
        let ok_sub = evaluate_concurrency_budget(true, true, "split", 0, 2, 2, 1, None);
        assert!(!ok_sub.deferred);
        assert_eq!(ok_sub.max_subleads, 2);

        let cap_sub = evaluate_concurrency_budget(true, true, "split", 0, 0, 0, 2, None);
        assert!(cap_sub.deferred);
        assert!(cap_sub.reason.as_deref().unwrap_or("").contains("sublead concurrency cap (2)"));

        let custom_cap = evaluate_concurrency_budget(true, true, "split", 0, 0, 0, 3, Some(9));
        assert!(!custom_cap.deferred);
        assert_eq!(custom_cap.max_subleads, 4);

        // 일반 write 워커는 active_coding=0이어도 tree_active_coding>=2면 차단
        let tree_cap = evaluate_concurrency_budget(false, true, "split", 0, 0, 2, 0, None);
        assert!(tree_cap.deferred);
        assert!(tree_cap.reason.as_deref().unwrap_or("").contains("coding concurrency cap (2)"));
    }
}
