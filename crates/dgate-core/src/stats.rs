//! 비용 인심 동적 라우팅(θ_sharp)·비용 통계·구간 합집합 타이밍 통계 —
//! `src/decision/stats.ts` (PB-2A)의 Rust 포팅.
//!
//! 목적:
//! - B2: `θ_sharp = clamp((C_eff(explore)+C_switch)/(C_eff(lead)+C_switch), 0.70, 0.90)`
//!   — 상수 0.70 대신 탐색/리드 상대비용과 쿼터 희소도 승수(κ_quota)로 임계치를 산출한다.
//! - B2: `decision_events.cost_status` 집계 — 'unknown' 이벤트를 $0으로 위장 합산하지 않는다.
//! - B3: `fork_intervals`의 exec/wait/human 구간을 합집합으로 측정해 병렬 중복을 제거한다.
//!
//! 주의:
//! - v1 DB 호환: `cost_status` 컬럼/`fork_intervals` 테이블 부재 시 오류 없이 안전 반환한다.
//! - 읽기 전용 — `settle_orphans` 등 쓰기 경로를 절대 호출하지 않는다.
//! - `θ` 하한은 `SHARP_P_THRESHOLD`(0.70) — 어떤 비용 설정에서도 하향하지 않는다.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::db::{has_column, has_table};
use crate::engine::SHARP_P_THRESHOLD;
use crate::roster::{effective_quota_bucket, RoleSlot, RoleWorkerSpec, RosterProfile};

// ---------- B2: 비용 인식 동적 sharp 임계치 (arch §3.2) ----------

/// θ_sharp 하한 — 현행 상수와 동일(0.70). 표본 정직성 보호를 위해 하향 금지
pub const SHARP_FLOOR: f64 = SHARP_P_THRESHOLD;
/// θ_sharp 상한 — 탐색이 리드보다 비싼 프로필의 실패 비용 폭등 방지
pub const SHARP_CEIL: f64 = 0.9;
/// 전환 오버헤드 상대비용 C_switch (arch §3.2)
pub const C_SWITCH_REL: f64 = 1.5;

/// `pricing.relativeCost` 미지정 시 슬롯 기본 상대비용 (1.0 = deepseek-flash 기준)
pub fn slot_default_relative_cost(slot: RoleSlot) -> f64 {
    match slot {
        RoleSlot::Lead => 5.0,
        RoleSlot::Explore => 1.0,
        RoleSlot::Coder => 3.0,
        RoleSlot::Verifier => 1.5,
        RoleSlot::Advisor => 8.0,
    }
}

/// 슬롯의 유효 상대비용 — 명시 `pricing.relativeCost`(유효 숫자·비음수) 우선, 아니면 슬롯 기본값
fn relative_cost_of(slot: RoleSlot, spec: Option<&RoleWorkerSpec>) -> f64 {
    if let Some(rel) = spec.and_then(|s| s.pricing.as_ref()).and_then(|p| p.relative_cost) {
        if rel.is_finite() && rel >= 0.0 {
            return rel;
        }
    }
    slot_default_relative_cost(slot)
}

/// 쿼터 희소도 승수 κ_quota(b) = 1 + 4·max(0, turnsUsed/budget − 0.5)².
/// 예산 미정의·0 이하이면 1.0(중립). 50% 이하 → 1.0, 100% → 2.0.
pub fn compute_quota_multiplier(turns_used: f64, daily_budget: f64) -> f64 {
    if !(daily_budget > 0.0) {
        return 1.0;
    }
    let over = turns_used / daily_budget - 0.5;
    1.0 + 4.0 * f64::max(0.0, over).powi(2)
}

/// [`crate::engine::compute_dynamic_sharp_threshold`] 입력 — 버킷 사용량은 미지정 버킷이 사용량 0(κ=1.0)이다.
pub struct DynamicSharpThresholdInput<'a> {
    pub profile: &'a RosterProfile,
    /// 버킷별 오늘 사용 턴 수 (예: `{ agy: 15 }`). 미지정 버킷은 사용량 0 → κ=1.0
    pub bucket_usage: Option<&'a HashMap<String, i64>>,
    /// 전환 오버헤드 — 기본 C_SWITCH_REL = 1.5
    pub switch_cost: Option<f64>,
    /// 하한 — 기본 SHARP_FLOOR = 0.70
    pub floor: Option<f64>,
    /// 상한 — 기본 SHARP_CEIL = 0.90
    pub ceil: Option<f64>,
}

/// 동적 sharp 임계치 산출 결과 — 비용 구성 요소까지 그대로 노출해 감사 가능성을 유지한다.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DynamicSharpThresholdResult {
    /// clamp((exploreCost + switch) / (leadCost + switch), floor, ceil)
    pub threshold: f64,
    /// κ 반영 후 탐색 슬롯 유효 비용 C_eff(explore)
    pub explore_cost: f64,
    /// κ 반영 후 리드 슬롯 유효 비용 C_eff(lead)
    pub lead_cost: f64,
    pub explore_multiplier: f64,
    pub lead_multiplier: f64,
}

/// 슬롯의 (스펙, κ_quota) 쌍 — 버킷 사용량/예산이 모두 알려져야 승수가 움직인다.
fn slot_multiplier<'a>(
    profile: &'a RosterProfile,
    usage: &HashMap<String, i64>,
    slot: RoleSlot,
) -> (Option<&'a RoleWorkerSpec>, f64) {
    let spec = profile.roles.get(&slot);
    let bucket = spec.and_then(effective_quota_bucket);
    let used = bucket.as_ref().map(|b| usage.get(b).copied().unwrap_or(0)).unwrap_or(0);
    let budget = profile
        .daily_turn_budgets
        .as_ref()
        .and_then(|b| bucket.as_ref().and_then(|bk| b.get(bk).copied()))
        .unwrap_or(0);
    (spec, compute_quota_multiplier(used as f64, budget as f64))
}

/// 프로필 기반 동적 sharp 임계치 (arch §3.2).
/// 기본 `quota-saver`(explore=1.0, lead=5.0, switch=1.5) → 2.5/6.5 = 0.3846 → 0.70
/// (기존 상수와 정확히 일치). `C_eff(lead)+switch <= 0`이면 보수 최대 ceil 반환.
pub fn compute_dynamic_sharp_threshold(input: &DynamicSharpThresholdInput) -> DynamicSharpThresholdResult {
    let switch_cost = input.switch_cost.unwrap_or(C_SWITCH_REL);
    let floor = input.floor.unwrap_or(SHARP_FLOOR);
    let ceil = input.ceil.unwrap_or(SHARP_CEIL);
    let empty: HashMap<String, i64> = HashMap::new();
    let usage = input.bucket_usage.unwrap_or(&empty);

    let (ex_spec, ex_m) = slot_multiplier(input.profile, usage, RoleSlot::Explore);
    let (ld_spec, ld_m) = slot_multiplier(input.profile, usage, RoleSlot::Lead);
    let explore_cost = relative_cost_of(RoleSlot::Explore, ex_spec) * ex_m;
    let lead_cost = relative_cost_of(RoleSlot::Lead, ld_spec) * ld_m;
    let denom = lead_cost + switch_cost;
    // 분모 0 방어: 비용 정보가 붕괴한 경우 보수 최대치로 고정
    let ratio = if denom > 0.0 { (explore_cost + switch_cost) / denom } else { f64::INFINITY };
    DynamicSharpThresholdResult {
        threshold: ratio.clamp(floor, ceil),
        explore_cost,
        lead_cost,
        explore_multiplier: ex_m,
        lead_multiplier: ld_m,
    }
}

// ---------- B2: 비용 통계 (decision_events cost_status 집계) ----------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostStatsOptions {
    pub project_id: Option<String>,
    pub days: Option<i64>,
    pub now_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostStatusBucket {
    pub events: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub usd: f64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostByStatus {
    pub reported: i64,
    pub estimated: i64,
    pub unknown: i64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostPerStatus {
    pub reported: CostStatusBucket,
    pub estimated: CostStatusBucket,
    pub unknown: CostStatusBucket,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostTokensSummary {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CostStatsResult {
    /// v1 DB(cost_status 컬럼 부재) — 집계 불가를 명시한다
    pub schema_v1: bool,
    pub total_events: i64,
    /// cost_status별 이벤트 건수
    pub by_status: CostByStatus,
    /// cost_status별 건수·토큰·금액 상세
    pub per_status: CostPerStatus,
    pub reported_usd: f64,
    pub estimated_usd: f64,
    /// reported + estimated — 실제 알려진 금액만의 합
    pub known_usd: f64,
    pub unknown_events: i64,
    /// 불변식 C-1: unknown_events > 0이면 반드시 None ($0 위장 합산 금지)
    pub total_usd_display: Option<f64>,
    pub tokens: CostTokensSummary,
}

impl Default for CostStatsResult {
    fn default() -> Self {
        CostStatsResult {
            schema_v1: true,
            total_events: 0,
            by_status: CostByStatus::default(),
            per_status: CostPerStatus::default(),
            reported_usd: 0.0,
            estimated_usd: 0.0,
            known_usd: 0.0,
            unknown_events: 0,
            total_usd_display: None,
            tokens: CostTokensSummary::default(),
        }
    }
}

fn now_ms_of(opt: Option<i64>) -> i64 {
    opt.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    })
}

/// `decision_events` 비용 집계 (읽기 전용).
/// v1 DB(cost_status 부재)는 전 건을 unknown으로 안전 반환한다.
pub fn compute_cost_stats(db: &Connection, opts: &CostStatsOptions) -> CostStatsResult {
    let now_ms = now_ms_of(opts.now_ms);
    let since_ms: Option<i64> = opts.days.map(|d| now_ms - d * 86_400_000);

    let mut where_clauses: Vec<&str> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(pid) = &opts.project_id {
        where_clauses.push("project_id = ?");
        params.push(Box::new(pid.clone()));
    }
    if let Some(since) = since_ms {
        where_clauses.push("created_at_ms >= ?");
        params.push(Box::new(since));
    }
    let where_sql = if where_clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_clauses.join(" AND "))
    };

    let mut base = CostStatsResult::default();
    if !has_table(db, "decision_events") {
        return base;
    }

    if !has_column(db, "decision_events", "cost_status") {
        // v1 안전 반환 — 비용 메타가 없으므로 전 건을 unknown으로 보고한다
        let sql = format!("SELECT COUNT(*) AS n FROM decision_events{}", where_sql);
        let n: i64 = db
            .query_row(
                sql.as_str(),
                rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
                |r| r.get(0),
            )
            .unwrap_or(0);
        base.total_events = n;
        base.unknown_events = n;
        base.by_status.unknown = n;
        base.per_status.unknown.events = n;
        return base;
    }

    let sql = format!(
        "SELECT cost_status AS st, COUNT(*) AS n,
                COALESCE(SUM(input_tokens), 0)       AS ti,
                COALESCE(SUM(output_tokens), 0)      AS to_,
                COALESCE(SUM(cache_read_tokens), 0)  AS tcr,
                COALESCE(SUM(cache_write_tokens), 0) AS tcw,
                COALESCE(SUM(cost_usd), 0)           AS usd
           FROM decision_events{}
          GROUP BY cost_status",
        where_sql
    );
    let rows: Vec<(String, i64, i64, i64, i64, i64, f64)> = match db.prepare(sql.as_str()) {
        Ok(mut stmt) => stmt
            .query_map(rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, f64>(6)?,
                ))
            })
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    let mut total = 0;
    for (st, n, ti, to_, tcr, tcw, usd) in rows {
        let bucket = match st.as_str() {
            "reported" => &mut base.per_status.reported,
            "estimated" => &mut base.per_status.estimated,
            _ => &mut base.per_status.unknown,
        };
        bucket.events += n;
        bucket.input_tokens += ti;
        bucket.output_tokens += to_;
        bucket.cache_read_tokens += tcr;
        bucket.cache_write_tokens += tcw;
        bucket.usd += usd;
        total += n;
        match st.as_str() {
            "reported" => base.reported_usd += usd,
            "estimated" => base.estimated_usd += usd,
            _ => base.unknown_events += n,
        }
    }
    base.schema_v1 = false;
    base.total_events = total;
    base.by_status = CostByStatus {
        reported: base.per_status.reported.events,
        estimated: base.per_status.estimated.events,
        unknown: base.per_status.unknown.events,
    };
    base.known_usd = base.reported_usd + base.estimated_usd;
    base.tokens = CostTokensSummary {
        input: base.per_status.reported.input_tokens
            + base.per_status.estimated.input_tokens
            + base.per_status.unknown.input_tokens,
        output: base.per_status.reported.output_tokens
            + base.per_status.estimated.output_tokens
            + base.per_status.unknown.output_tokens,
        cache_read: base.per_status.reported.cache_read_tokens
            + base.per_status.estimated.cache_read_tokens
            + base.per_status.unknown.cache_read_tokens,
        cache_write: base.per_status.reported.cache_write_tokens
            + base.per_status.estimated.cache_write_tokens
            + base.per_status.unknown.cache_write_tokens,
    };
    // 불변식 C-1: unknown이 1건이라도 있으면 총액 표시 자체를 거부한다
    base.total_usd_display = if base.unknown_events > 0 { None } else { Some(base.known_usd) };
    base
}

// ---------- B3: 구간 합집합 타이밍 통계 (fork_intervals) ----------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnionInterval {
    pub start: f64,
    pub end: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UnionResult {
    /// 겹침·인접 병합 후 실측 길이 합 (ms)
    pub union_ms: f64,
    /// 병합 없는 단순 합 (ms) — 병렬 처리량 지표
    pub sum_ms: f64,
    /// 유효하지 않아 버린 구간 수 (end < start, 비유한 값)
    pub dropped: usize,
}

/// 구간 합집합 길이 측정 — 시작 시각 정렬 후 겹치거나 인접한(s <= curE) 구간을 병합한다.
/// 인접 병합([0,100]+[100,200] → 200)이 빈틈을 만들지 않는다.
pub fn union_intervals_ms(intervals: &[UnionInterval]) -> UnionResult {
    let mut segs: Vec<(f64, f64)> = Vec::new();
    let mut dropped: usize = 0;
    let mut sum_ms = 0.0;
    for it in intervals {
        let (s, e) = (it.start, it.end);
        if !s.is_finite() || !e.is_finite() || e < s {
            dropped += 1;
            continue;
        }
        segs.push((s, e));
        sum_ms += e - s;
    }
    segs.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    let mut union_ms = 0.0;
    let mut cur: Option<(f64, f64)> = None;
    for &(s, e) in &segs {
        match cur {
            None => cur = Some((s, e)),
            Some((cs, mut ce)) => {
                if s <= ce {
                    if e > ce {
                        ce = e;
                    }
                    cur = Some((cs, ce));
                } else {
                    union_ms += ce - cs;
                    cur = Some((s, e));
                }
            }
        }
    }
    if let Some((cs, ce)) = cur {
        union_ms += ce - cs;
    }
    UnionResult { union_ms, sum_ms, dropped }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimingStatsOptions {
    pub project_id: Option<String>,
    pub fork_id: Option<String>,
    pub days: Option<i64>,
    pub now_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimingStatsResult {
    /// v1 DB(fork_intervals 테이블 부재) — 구간 데이터 없음을 명시한다
    pub intervals_unavailable: bool,
    /// 구간 보유 fork / 전체 fork
    pub coverage_ratio: f64,
    /// exec 구간 합집합 — 병렬 중복 제거된 실제 실행 시간
    pub execution_ms: f64,
    /// exec 단순 합 — 병렬 포함 처리량
    pub sum_execution_ms: f64,
    /// max(0, sum_execution_ms - execution_ms) — 병렬화로 절약한 시간
    pub parallel_gain_ms: f64,
    /// wait 구간 합집합 — 발행~실행 개시·종료~종결 대기
    pub wait_ms: f64,
    /// human 구간 합집합 — 사람 승인/파킹
    pub human_wait_ms: f64,
    /// 전체 구간 최소 start ~ 최대 end
    pub wall_ms: f64,
    /// 버린 비정상 구간 수
    pub dropped: usize,
    pub forks_total: i64,
    pub forks_with_intervals: usize,
    pub intervals: usize,
}

impl Default for TimingStatsResult {
    fn default() -> Self {
        TimingStatsResult {
            intervals_unavailable: true,
            coverage_ratio: 0.0,
            execution_ms: 0.0,
            sum_execution_ms: 0.0,
            parallel_gain_ms: 0.0,
            wait_ms: 0.0,
            human_wait_ms: 0.0,
            wall_ms: 0.0,
            dropped: 0,
            forks_total: 0,
            forks_with_intervals: 0,
            intervals: 0,
        }
    }
}

/// `fork_intervals` 기반 실행/대기 통계 (읽기 전용).
/// v1 DB(테이블 부재)는 `intervals_unavailable: true` + 전부 0으로 안전 반환한다.
/// 열린 구간(end NULL)은 now_ms로 절단해 미래로 새어 나가지 않게 한다.
pub fn compute_timing_stats(db: &Connection, opts: &TimingStatsOptions) -> TimingStatsResult {
    if !has_table(db, "fork_intervals") || !has_table(db, "forks") {
        return TimingStatsResult::default();
    }

    let now_ms = now_ms_of(opts.now_ms);
    let since_ms: Option<i64> = opts.days.map(|d| now_ms - d * 86_400_000);

    // fork 스코프 총수 (coverage 분모)
    let mut fork_where: Vec<&str> = Vec::new();
    let mut fork_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(fid) = &opts.fork_id {
        fork_where.push("fork_id = ?");
        fork_params.push(Box::new(fid.clone()));
    }
    if let Some(pid) = &opts.project_id {
        fork_where.push("project_id = ?");
        fork_params.push(Box::new(pid.clone()));
    }
    if let Some(since) = since_ms {
        fork_where.push("created_at_ms >= ?");
        fork_params.push(Box::new(since));
    }
    let fork_sql = format!(
        "SELECT COUNT(*) AS n FROM forks{}",
        if fork_where.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", fork_where.join(" AND "))
        }
    );
    let forks_total: i64 = db
        .query_row(
            fork_sql.as_str(),
            rusqlite::params_from_iter(fork_params.iter().map(|p| p.as_ref())),
            |r| r.get(0),
        )
        .unwrap_or(0);

    // 구간 수집: fork 스코프에 JOIN, 윈도 겹침(end >= since 또는 미종결)만
    let mut iv_where: Vec<&str> = Vec::new();
    let mut iv_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(fid) = &opts.fork_id {
        iv_where.push("fi.fork_id = ?");
        iv_params.push(Box::new(fid.clone()));
    }
    if let Some(pid) = &opts.project_id {
        iv_where.push("f.project_id = ?");
        iv_params.push(Box::new(pid.clone()));
    }
    if let Some(since) = since_ms {
        iv_where.push("(fi.ended_at_ms IS NULL OR fi.ended_at_ms >= ?)");
        iv_params.push(Box::new(since));
    }
    let iv_sql = format!(
        "SELECT fi.fork_id, fi.phase, fi.started_at_ms, fi.ended_at_ms
           FROM fork_intervals fi JOIN forks f ON f.fork_id = fi.fork_id
          {}",
        if iv_where.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", iv_where.join(" AND "))
        }
    );
    let rows: Vec<(String, String, i64, Option<i64>)> = match db.prepare(iv_sql.as_str()) {
        Ok(mut stmt) => stmt
            .query_map(rusqlite::params_from_iter(iv_params.iter().map(|p| p.as_ref())), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    // 윈도 클램프 + phase별 구간 분류
    let clip_lo = since_ms.map(|v| v as f64).unwrap_or(f64::NEG_INFINITY);
    let clip_hi = now_ms as f64;
    let mut by_phase: (Vec<UnionInterval>, Vec<UnionInterval>, Vec<UnionInterval>) =
        (Vec::new(), Vec::new(), Vec::new());
    let mut covered_forks: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut dropped: usize = 0;
    for (fork_id, phase, started, ended) in &rows {
        let s = f64::max(*started as f64, clip_lo);
        let e = f64::min(ended.unwrap_or(now_ms) as f64, clip_hi);
        if e < s {
            dropped += 1;
            continue;
        }
        let iv = UnionInterval { start: s, end: e };
        match phase.as_str() {
            "exec" => by_phase.0.push(iv),
            "human" => by_phase.2.push(iv),
            // 'wait' 및 미상 phase는 wait 버킷 — TS와 동일한 폴백
            _ => by_phase.1.push(iv),
        }
        covered_forks.insert(fork_id.clone());
    }

    let exec_res = union_intervals_ms(&by_phase.0);
    let wait_res = union_intervals_ms(&by_phase.1);
    let human_res = union_intervals_ms(&by_phase.2);

    let mut wall_ms = 0.0;
    let all_segs = by_phase
        .0
        .iter()
        .chain(by_phase.1.iter())
        .chain(by_phase.2.iter());
    let mut min_s = f64::INFINITY;
    let mut max_e = f64::NEG_INFINITY;
    let mut any = false;
    for seg in all_segs {
        any = true;
        if seg.start < min_s {
            min_s = seg.start;
        }
        if seg.end > max_e {
            max_e = seg.end;
        }
    }
    if any {
        wall_ms = max_e - min_s;
    }

    TimingStatsResult {
        intervals_unavailable: false,
        coverage_ratio: if forks_total > 0 {
            covered_forks.len() as f64 / forks_total as f64
        } else {
            0.0
        },
        execution_ms: exec_res.union_ms,
        sum_execution_ms: exec_res.sum_ms,
        parallel_gain_ms: f64::max(0.0, exec_res.sum_ms - exec_res.union_ms),
        wait_ms: wait_res.union_ms,
        human_wait_ms: human_res.union_ms,
        wall_ms,
        dropped: dropped + exec_res.dropped + wait_res.dropped + human_res.dropped,
        forks_total,
        forks_with_intervals: covered_forks.len(),
        intervals: rows.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{migrate_v1_tables, open_db, OpenDbOptions};
    use crate::roster::{builtin_roster_config, RolePricingSpec};
    use crate::testutil::temp_dir;
    use std::collections::BTreeMap;

    /// 테스트용 입력 축약 — 프로필만 갈아끼운다(나머지 옵션은 기본값).
    fn thr<'a>(profile: &'a RosterProfile) -> DynamicSharpThresholdInput<'a> {
        DynamicSharpThresholdInput {
            profile,
            bucket_usage: None,
            switch_cost: None,
            floor: None,
            ceil: None,
        }
    }

    fn spec_with_cost(rel: f64) -> RoleWorkerSpec {
        RoleWorkerSpec {
            pricing: Some(RolePricingSpec { relative_cost: Some(rel), ..Default::default() }),
            ..Default::default()
        }
    }

    fn empty_profile() -> RosterProfile {
        RosterProfile {
            description: None,
            fallback_profile: None,
            daily_turn_budgets: None,
            roles: HashMap::new(),
        }
    }

    fn quota_saver_profile() -> RosterProfile {
        builtin_roster_config().profiles.get("quota-saver").cloned().unwrap()
    }

    #[test]
    fn quota_multiplier_curve_boundaries() {
        assert_eq!(compute_quota_multiplier(0.0, 20.0), 1.0, "budget 0% → 1.0");
        assert_eq!(compute_quota_multiplier(10.0, 20.0), 1.0, "budget 50% → 1.0");
        assert_eq!(compute_quota_multiplier(20.0, 20.0), 2.0, "budget 100% → 2.0");
        assert_eq!(compute_quota_multiplier(0.0, 0.0), 1.0, "budget 미정의 → 중립 1.0");
        assert_eq!(compute_quota_multiplier(5.0, -1.0), 1.0, "budget 음수 → 중립 1.0");
        assert!((compute_quota_multiplier(15.0, 20.0) - 1.25).abs() < 1e-12, "budget 75% → 1.25");
    }

    #[test]
    fn dynamic_threshold_quota_saver_matches_legacy_constant() {
        let profile = quota_saver_profile();
        let t = compute_dynamic_sharp_threshold(&thr(&profile));
        assert!((t.threshold - 0.7).abs() < 1e-12, "quota-saver θ={} — 기존 0.70 상수와 일치", t.threshold);
        assert!((t.explore_cost - 1.0).abs() < 1e-12);
        assert!((t.lead_cost - 5.0).abs() < 1e-12);
        assert_eq!(t.explore_multiplier, 1.0);
        assert_eq!(t.lead_multiplier, 1.0);
    }

    #[test]
    fn dynamic_threshold_explicit_costs_match_relative_weights() {
        // 명시 relativeCost(explore 3.5, lead 5.0) → (3.5+1.5)/(5.0+1.5) ≈ 0.7692
        let mut profile = empty_profile();
        profile.roles.insert(RoleSlot::Lead, spec_with_cost(5.0));
        profile.roles.insert(RoleSlot::Explore, spec_with_cost(3.5));
        let t = compute_dynamic_sharp_threshold(&thr(&profile));
        assert!((t.threshold - 0.7692).abs() < 1e-3, "hybrid θ={}", t.threshold);
        assert!((t.explore_cost - 3.5).abs() < 1e-12);
        assert!((t.lead_cost - 5.0).abs() < 1e-12);
    }

    #[test]
    fn dynamic_threshold_ceiling_clamp_and_quota_usage() {
        // 탐색이 리드보다 훨씬 비싼 프로필 → 0.90 상한 clamp
        let mut expensive = empty_profile();
        expensive.roles.insert(RoleSlot::Lead, spec_with_cost(1.0));
        expensive.roles.insert(RoleSlot::Explore, spec_with_cost(100.0));
        let t = compute_dynamic_sharp_threshold(&thr(&expensive));
        assert_eq!(t.threshold, 0.9, "explore >> lead → ceil 0.90 clamp");

        // claude-hybrid 유사(explore 3.5, lead 5.0, opencodex 예산 20) → ≈0.7692
        let mut budgets: BTreeMap<String, i64> = BTreeMap::new();
        budgets.insert("opencodex".to_string(), 20);
        let mut hybrid = empty_profile();
        hybrid.daily_turn_budgets = Some(budgets);
        hybrid.roles.insert(RoleSlot::Lead, spec_with_cost(5.0));
        hybrid.roles.insert(
            RoleSlot::Explore,
            RoleWorkerSpec {
                quota_bucket: Some("opencodex".to_string()),
                ..spec_with_cost(3.5)
            },
        );
        let cool = compute_dynamic_sharp_threshold(&thr(&hybrid));
        assert!((cool.threshold - 0.7692).abs() < 1e-3, "cool θ={}", cool.threshold);

        let mut usage: HashMap<String, i64> = HashMap::new();
        usage.insert("opencodex".to_string(), 20);
        let hot = compute_dynamic_sharp_threshold(&DynamicSharpThresholdInput {
            profile: &hybrid,
            bucket_usage: Some(&usage),
            ..thr(&hybrid)
        });
        assert_eq!(hot.explore_multiplier, 2.0, "opencodex 버킷 100% 소진 → κ=2.0");
        assert!((hot.explore_cost - 7.0).abs() < 1e-12);
        assert_eq!(hot.threshold, 0.9, "탐색 비용 급등 → 상한 clamp (8.5/6.5=1.31→0.90)");
    }

    #[test]
    fn cost_stats_unknown_disguise_guard_and_v1_safe() {
        let tmp = temp_dir("stats-cost");
        let path = tmp.join("decision.db");
        {
            let conn = open_db(Some(path.to_str().unwrap()), OpenDbOptions::default()).unwrap();
            for (i, (tokens, usd, status)) in [
                (Some(100_000i64), Some(1.0f64), "reported"),
                (Some(200_000), Some(0.5), "reported"),
                (Some(50_000), Some(0.25), "estimated"),
                (None, None, "unknown"),
                (None, None, "unknown"),
            ]
            .into_iter()
            .enumerate()
            {
                conn.execute(
                    "INSERT INTO decision_events
                       (project_id, source, route, tool_name, tool_class, ok, weight, is_retry, created_at_ms,
                        input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd, cost_status)
                     VALUES ('p_cost', 'wrap', 'tool_select', 'x', 'shell_exec', 1, 1.0, 0, ?, ?, 1, 1, 1, ?, ?)",
                    rusqlite::params![1000 + i as i64, tokens, usd, status],
                )
                .unwrap();
            }
            let s = compute_cost_stats(
                &conn,
                &CostStatsOptions { project_id: Some("p_cost".to_string()), ..Default::default() },
            );
            assert!(!s.schema_v1);
            assert_eq!(s.total_events, 5);
            assert_eq!(s.by_status.reported, 2);
            assert_eq!(s.by_status.estimated, 1);
            assert_eq!(s.by_status.unknown, 2);
            assert!((s.reported_usd - 1.5).abs() < 1e-9, "reported 분리 합산");
            assert!((s.estimated_usd - 0.25).abs() < 1e-9);
            assert!((s.known_usd - 1.75).abs() < 1e-9);
            assert_eq!(s.unknown_events, 2);
            assert_eq!(s.total_usd_display, None, "불변식 C-1: unknown>0 → 총액 None ($0 위장 금지)");
            assert_eq!(s.tokens.input, 350_000);
            assert_eq!(s.per_status.reported.events, 2);
            assert_eq!(s.per_status.unknown.events, 2);
        }

        // v1 DB: cost_status 컬럼 부재 → 전 건 unknown 안전 반환
        let v1_path = tmp.join("v1.db");
        {
            let raw = Connection::open(&v1_path).unwrap();
            migrate_v1_tables(&raw).unwrap();
            raw.execute(
                "INSERT INTO decision_events (project_id, source, route, tool_name, tool_class, ok, weight, is_retry, created_at_ms)
                 VALUES ('p1', 'wrap', 'tool_select', 'x', 'shell_exec', 1, 1.0, 0, 1000)",
                [],
            )
            .unwrap();
            raw.execute(
                "INSERT INTO decision_events (project_id, source, route, tool_name, tool_class, ok, weight, is_retry, created_at_ms)
                 VALUES ('p1', 'cli', 'tool_select', 'y', 'shell_exec', 1, 1.0, 0, 1100)",
                [],
            )
            .unwrap();
            let v1 = compute_cost_stats(&raw, &CostStatsOptions::default());
            assert!(v1.schema_v1, "v1 DB → schema_v1 마커");
            assert_eq!(v1.total_events, 2);
            assert_eq!(v1.unknown_events, 2);
            assert_eq!(v1.reported_usd, 0.0);
            assert_eq!(v1.estimated_usd, 0.0);
            assert_eq!(v1.total_usd_display, None, "v1 합계는 절대 표시 안 함");
            assert_eq!(v1.by_status.unknown, 2);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn cost_stats_known_only_sums_display_and_filters() {
        let tmp = temp_dir("stats-cost-known");
        let path = tmp.join("decision.db");
        {
            let conn = open_db(Some(path.to_str().unwrap()), OpenDbOptions::default()).unwrap();
            let now = 1_700_000_000_000i64;
            conn.execute(
                "INSERT INTO decision_events
                   (project_id, source, route, tool_name, tool_class, ok, weight, is_retry, created_at_ms,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd, cost_status)
                 VALUES ('p2', 'wrap', 'tool_select', 'x', 'shell_exec', 1, 1.0, 0, ?, 100, 50, 0, 0, 2.0, 'reported'),
                        ('p2', 'wrap', 'tool_select', 'x', 'shell_exec', 1, 1.0, 0, ?, 100, 50, 0, 0, 0.5, 'estimated'),
                        ('other', 'wrap', 'tool_select', 'x', 'shell_exec', 1, 1.0, 0, ?, 100, 50, 0, 0, 9.0, 'reported')",
                rusqlite::params![now - 1000, now - 900, now - 800],
            )
            .unwrap();
            let s = compute_cost_stats(
                &conn,
                &CostStatsOptions { project_id: Some("p2".to_string()), ..Default::default() },
            );
            assert_eq!(s.unknown_events, 0);
            assert_eq!(s.total_usd_display, Some(2.5), "전 건 known → 합계 2.5 표시");
            assert!((s.known_usd - 2.5).abs() < 1e-9);
            let s30 = compute_cost_stats(
                &conn,
                &CostStatsOptions {
                    project_id: Some("p2".to_string()),
                    days: Some(30),
                    now_ms: Some(now),
                    ..Default::default()
                },
            );
            assert_eq!(s30.total_events, 2, "days=30 윈도 안 2건");
            let none = compute_cost_stats(
                &conn,
                &CostStatsOptions { project_id: Some("p_none".to_string()), ..Default::default() },
            );
            assert_eq!(none.total_events, 0);
            assert_eq!(none.total_usd_display, Some(0.0), "unknown 0 + known 0 → 합계 0 허용");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn union_intervals_merge_adjacent_and_drop_invalid() {
        let u = union_intervals_ms(&[
            UnionInterval { start: 0.0, end: 100.0 },
            UnionInterval { start: 50.0, end: 150.0 },
        ]);
        assert_eq!(u.union_ms, 150.0);
        assert_eq!(u.sum_ms, 200.0);
        assert_eq!(u.dropped, 0);

        let u2 = union_intervals_ms(&[
            UnionInterval { start: 0.0, end: 100.0 },
            UnionInterval { start: 150.0, end: 50.0 },  // 역전 → dropped
            UnionInterval { start: 100.0, end: 200.0 }, // 인접(s == curE) → 병합
        ]);
        assert_eq!(u2.dropped, 1);
        assert_eq!(u2.union_ms, 200.0, "[0,100]+[100,200] 인접 병합 → 빈틈 없는 200");
        assert_eq!(u2.sum_ms, 200.0);

        let u3 = union_intervals_ms(&[UnionInterval { start: f64::NAN, end: 1.0 }]);
        assert_eq!(u3.dropped, 1);
        assert_eq!(u3.union_ms, 0.0);
    }

    #[test]
    fn timing_stats_union_gain_coverage_and_v1_safe() {
        let tmp = temp_dir("stats-timing");
        let path = tmp.join("decision.db");
        {
            let conn = open_db(Some(path.to_str().unwrap()), OpenDbOptions::default()).unwrap();
            for fork in ["f1", "f2", "f3"] {
                conn.execute(
                    "INSERT INTO forks
                       (fork_id, project_id, project_label, route, tool_class, decision, forced_split, p_at_gate,
                        basis_at_gate, generation, attempt, status, closed_by, ttl_ms, created_at_ms, last_touch_ms, closed_at_ms)
                     VALUES (?, 'p_t', 'p_t', 'tool_select', 'shell_exec', 'sharp', 0, 0.8, 'prior', 1, 1, 'ok', 'worker', 600000, 0, 0, 200)",
                    [fork],
                )
                .unwrap();
            }
            let ins = "INSERT INTO fork_intervals (fork_id, attempt, phase, started_at_ms, ended_at_ms) VALUES (?, 1, ?, ?, ?)";
            conn.execute(ins, rusqlite::params!["f1", "exec", 0, 100]).unwrap();
            conn.execute(ins, rusqlite::params!["f1", "wait", 100, 200]).unwrap();
            conn.execute(ins, rusqlite::params!["f2", "exec", 50, 150]).unwrap();
            conn.execute(ins, rusqlite::params!["f2", "wait", 150, 200]).unwrap();
            // 윈도(nowMs=200) 이후 미래 구간 — clip 결과 end < start → dropped
            conn.execute(ins, rusqlite::params!["f1", "wait", 500, 600]).unwrap();

            let t = compute_timing_stats(
                &conn,
                &TimingStatsOptions {
                    project_id: Some("p_t".to_string()),
                    now_ms: Some(200),
                    ..Default::default()
                },
            );
            assert!(!t.intervals_unavailable);
            assert_eq!(t.execution_ms, 150.0, "exec 합집합 [0,100]∪[50,150] = 150");
            assert_eq!(t.sum_execution_ms, 200.0, "exec 단순 합 = 200");
            assert_eq!(t.parallel_gain_ms, 50.0, "병렬 절약 = 200−150 = 50");
            assert_eq!(t.wait_ms, 100.0, "wait 합집합 [100,200]∪[150,200] = 100");
            assert_eq!(t.wall_ms, 200.0, "전체 구간 0~200");
            assert_eq!(t.dropped, 1, "미래 구간 clip 후 end<start → dropped");
            assert_eq!(t.forks_total, 3);
            assert_eq!(t.forks_with_intervals, 2);
            assert!((t.coverage_ratio - 2.0 / 3.0).abs() < 1e-9, "coverage = 구간 보유 2 / 전체 3");
            assert_eq!(t.human_wait_ms, 0.0);
            assert_eq!(t.intervals, 5);
        }

        // v1 DB: fork_intervals 부재 → 안전 반환
        let v1_path = tmp.join("v1t.db");
        {
            let raw = Connection::open(&v1_path).unwrap();
            migrate_v1_tables(&raw).unwrap();
            let tv = compute_timing_stats(&raw, &TimingStatsOptions::default());
            assert!(tv.intervals_unavailable, "v1 → intervals_unavailable 마커");
            assert_eq!(tv.coverage_ratio, 0.0);
            assert_eq!(tv.execution_ms, 0.0);
            assert_eq!(tv.sum_execution_ms, 0.0);
            assert_eq!(tv.parallel_gain_ms, 0.0);
            assert_eq!(tv.wait_ms, 0.0);
            assert_eq!(tv.human_wait_ms, 0.0);
            assert_eq!(tv.wall_ms, 0.0);
            assert_eq!(tv.dropped, 0);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
