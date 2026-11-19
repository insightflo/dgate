//! 완전 읽기전용 규칙 후보 리포트 생성기 — `src/decision/report.ts` (PB-2B) 포팅.
//!
//! 목적: `decision_events`/`forks` 통계에서 "사람이 검토할 규칙 후보" 4종(도구 반복·
//! 핑퐁·FailureCause·고아/스톨)을 집계해 구조화 JSON + 마크다운으로만 출력한다.
//! 프롬프트·역할 파일(`.herdr/roles/*`)·기억·DB 내용 자동 수정 기능은 존재하지
//! 않으며 향후에도 만들지 않는다(자동 변조 영구 금지 — 설계 §5.1).
//!
//! 주의:
//! - v1 DB 안전 호환: `has_table('fork_intervals')`·`has_column('decision_events','cause')`·
//!   `has_column('forks','attempt')`로 지표 가용성을 판정하고, v1 전용 컬럼이 필요한
//!   지표는 크래시 없이 `unavailable`로 표기한다. (`cause`는 v1에서 `error_snippet`의
//!   `cause=<enum>;` 접두 파싱으로 대체 집계)
//! - `--out` 경로는 allowlist로 통제한다(검증서 A-1): `<repo_dir>/.herdr/report/` 내부
//!   또는 저장소 외부 절대경로만 허용하고, 그 외 저장소 내부 경로는 전부 거부한다.
//! - 모든 `error_snippet` 출력은 [`crate::redact::redact_secrets`]를 재적용한다(2차 방어).
//! - 이 모듈은 읽기전용 연결만 연다 — `settle_orphans` 등 어떤 쓰기도 호출하지 않는다.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::Serialize;

use crate::db::{has_column, has_table, open_db, OpenDbOptions};
use crate::engine::FAILURE_CAUSES;
use crate::redact::redact_secrets;

/// 기본 집계 윈도우(일) — 설계 §5.2
pub const DEFAULT_REPORT_DAYS: i64 = 14;
/// 규칙 후보 상위 노출 한
pub const TOP_N: i64 = 12;

/// `dg report`용 읽기전용 연결 — 쓰기 옵션 자체가 존재하지 않는다
pub fn open_report_db(db_path: Option<&str>) -> Result<Connection, String> {
    open_db(
        db_path,
        OpenDbOptions { read_only: true, busy_timeout_ms: Some(1000) },
    )
}

// ---------- 집계 타입 ----------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReportWindow {
    pub days: i64,
    pub from_ms: i64,
    pub to_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolRepetitionRow {
    pub route: String,
    pub tool_name: String,
    pub tool_class: String,
    pub events: i64,
    pub fail: i64,
    pub fail_ratio: f64,
    pub retries: i64,
    pub candidate: bool,
}

/// 도구 반복 섹션 — TS `{available: true, rows} | {available: false, reason}` 유니온 대응
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolRepetitionSection {
    pub available: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<ToolRepetitionRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PingpongStats {
    pub available: bool,
    pub retry_events: i64,
    pub total_events: i64,
    pub retry_ratio: f64,
    pub candidate: bool,
    pub forks_total: i64,
    /// v1 DB는 `forks.attempt` 컬럼 부재 — attempt>=2 집계 불가
    pub rework_forks: Option<i64>,
    pub rework_ratio: Option<f64>,
    pub attempt_available: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CauseSource {
    Column,
    Snippet,
}

impl CauseSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            CauseSource::Column => "column",
            CauseSource::Snippet => "snippet",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CauseRow {
    pub cause: String,
    pub count: i64,
    pub ratio: f64,
    pub example: Option<String>,
    pub candidate: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FailureCausesSection {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<CauseSource>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<CauseRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OrphanLevel {
    None,
    Warn,
    Critical,
}

impl OrphanLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            OrphanLevel::None => "none",
            OrphanLevel::Warn => "warn",
            OrphanLevel::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OrphanStallStats {
    pub available: bool,
    pub ttl_orphan_events: i64,
    pub ttl_orphan_ratio: f64,
    pub stall_events: Option<i64>,
    pub stall_ratio: Option<f64>,
    pub stall_available: bool,
    pub level: OrphanLevel,
}

/// Decision API shadow/on 평가 집계 — `forks.metadata_json.decision_api` 파싱 결과.
/// API 평가가 없던 윈도우는 모든 필드가 0이다(섹션 자체는 항상 결정적 형태로 존재).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DecisionApiShadowSummary {
    pub total: i64,
    pub ok: i64,
    pub errors: i64,
    pub shadow_count: i64,
    pub on_applied_count: i64,
    pub disagreed: i64,
    pub disagreement_rate: Option<f64>,
    pub total_input_tokens: i64,
    pub total_cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RuleCandidateReport {
    pub command: &'static str,
    pub schema_version: i64,
    pub generated_at_ms: i64,
    pub window: ReportWindow,
    pub project_id: Option<String>,
    pub totals: ReportTotals,
    pub sections: ReportSections,
    pub thresholds: ReportThresholds,
    /// 채택 임계를 만족한 후보 식별자(예: `tool_repetition:run_command`)
    pub candidates: Vec<String>,
    /// Decision API 감사 집계 — 읽기전용, total=0이면 마크다운 섹션 미출력
    #[serde(rename = "decisionApiShadow")]
    pub decision_api_shadow: DecisionApiShadowSummary,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReportTotals {
    pub events: i64,
    pub forks: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReportSections {
    pub tool_repetition: ToolRepetitionSection,
    pub pingpong: PingpongStats,
    pub failure_causes: FailureCausesSection,
    pub orphan_stall: OrphanStallStats,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ReportThresholds {
    pub tool_min_events: i64,
    pub pingpong_ratio: f64,
    pub cause_min_count: i64,
    pub orphan_warn: f64,
    pub orphan_critical: f64,
}

#[derive(Debug, Clone, Default)]
pub struct ReportOptions {
    pub project_id: Option<String>,
    pub days: Option<i64>,
    pub now_ms: Option<i64>,
}

// ---------- 내부 집계 헬퍼 ----------

fn is_valid_cause(v: &str) -> bool {
    FAILURE_CAUSES.iter().any(|c| c.as_str() == v)
}

/// `cause=<enum>; …` 접두 규약 파싱 — 미상·불일치는 None (추정 금지)
fn parse_cause_from_snippet(snippet: Option<&str>) -> Option<String> {
    let s = snippet?;
    let head = s.strip_prefix("cause=")?;
    let value = head.split(';').next().unwrap_or(head);
    if is_valid_cause(value) {
        Some(value.to_string())
    } else {
        None
    }
}

fn ratio_of(part: f64, total: f64) -> f64 {
    if total > 0.0 {
        part / total
    } else {
        0.0
    }
}

fn scalar(db: &Connection, sql: &str, params: &[Box<dyn rusqlite::types::ToSql>]) -> i64 {
    db.query_row(
        sql,
        rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

fn now_ms_of(opt: Option<i64>) -> i64 {
    opt.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    })
}

// ---------- 4대 규칙 후보 집계 ----------

pub fn generate_rule_candidate_report(db: &Connection, opts: &ReportOptions) -> RuleCandidateReport {
    let now_ms = now_ms_of(opts.now_ms);
    let days = match opts.days {
        Some(d) if d > 0 => d,
        _ => DEFAULT_REPORT_DAYS,
    };
    let from_ms = now_ms - days * 86_400_000;
    let project_id = opts.project_id.clone();

    let events_table = has_table(db, "decision_events");
    let forks_table = has_table(db, "forks");
    let has_cause_col = events_table && has_column(db, "decision_events", "cause");
    let has_attempt_col = forks_table && has_column(db, "forks", "attempt");

    let mut ev_conds: Vec<&str> = vec!["created_at_ms >= ?", "created_at_ms <= ?"];
    let mut ev_args: Vec<Box<dyn rusqlite::types::ToSql>> =
        vec![Box::new(from_ms), Box::new(now_ms)];
    if let Some(pid) = &project_id {
        ev_conds.push("project_id = ?");
        ev_args.push(Box::new(pid.clone()));
    }
    let ev_where = ev_conds.join(" AND ");

    let total_events = if events_table {
        scalar(
            db,
            &format!("SELECT COUNT(*) AS c FROM decision_events WHERE {}", ev_where),
            &ev_args,
        )
    } else {
        0
    };
    let mut fork_conds: Vec<&str> = vec!["created_at_ms >= ?", "created_at_ms <= ?"];
    let mut fork_args: Vec<Box<dyn rusqlite::types::ToSql>> =
        vec![Box::new(from_ms), Box::new(now_ms)];
    if let Some(pid) = &project_id {
        fork_conds.push("project_id = ?");
        fork_args.push(Box::new(pid.clone()));
    }
    let fork_where = fork_conds.join(" AND ");
    let total_forks = if forks_table {
        scalar(
            db,
            &format!("SELECT COUNT(*) AS c FROM forks WHERE {}", fork_where),
            &fork_args,
        )
    } else {
        0
    };

    // ① 도구 반복 상위 12 — (route, tool_name, tool_class) 실패/반복
    let mut tool_repetition = ToolRepetitionSection {
        available: false,
        rows: Vec::new(),
        reason: Some("decision_events table missing".to_string()),
    };
    if events_table {
        let sql = format!(
            "SELECT route, tool_name, tool_class, COUNT(*) AS events,
                    SUM(CASE WHEN ok = 0 THEN 1 ELSE 0 END) AS fail,
                    SUM(is_retry) AS retries
             FROM decision_events WHERE {}
             GROUP BY route, tool_name, tool_class
             ORDER BY fail DESC, events DESC, tool_name ASC, tool_class ASC
             LIMIT {}",
            ev_where, TOP_N
        );
        let rows: Vec<(String, String, String, i64, Option<i64>, Option<i64>)> = db
            .prepare(sql.as_str())
            .map(|mut stmt| {
                stmt.query_map(
                    rusqlite::params_from_iter(ev_args.iter().map(|p| p.as_ref())),
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
                )
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
            })
            .unwrap_or_default();
        tool_repetition = ToolRepetitionSection {
            available: true,
            reason: None,
            rows: rows
                .into_iter()
                .map(|(route, tool_name, tool_class, events, fail, retries)| {
                    let fail = fail.unwrap_or(0);
                    ToolRepetitionRow {
                        route,
                        tool_name,
                        tool_class,
                        events,
                        fail,
                        fail_ratio: ratio_of(fail as f64, events as f64),
                        retries: retries.unwrap_or(0),
                        candidate: events >= 5 && fail > 0,
                    }
                })
                .collect(),
        };
    }

    // ② 핑퐁 — retry_or_abort 이벤트 비율 + (attempt >= 2 또는 metadata rework_count >= 1) fork
    let retry_events = if events_table {
        scalar(
            db,
            &format!("SELECT COUNT(*) AS c FROM decision_events WHERE {} AND route = 'retry_or_abort'", ev_where),
            &ev_args,
        )
    } else {
        0
    };
    // json_valid 가드 선행 — 손상 JSON에서 json_extract는 오류를 낸다(검증서 주의 1)
    let meta_rework =
        "(json_valid(metadata_json) AND CAST(json_extract(metadata_json, '$.rework_count') AS INTEGER) >= 1)";
    let rework_forks: Option<i64> = if forks_table {
        let cond = if has_attempt_col {
            format!("(attempt >= 2 OR {})", meta_rework)
        } else {
            meta_rework.to_string()
        };
        Some(scalar(
            db,
            &format!("SELECT COUNT(*) AS c FROM forks WHERE {} AND {}", fork_where, cond),
            &fork_args,
        ))
    } else {
        None
    };
    let retry_ratio = ratio_of(retry_events as f64, total_events as f64);
    let pingpong = PingpongStats {
        available: events_table,
        retry_events,
        total_events,
        retry_ratio,
        candidate: total_events > 0 && retry_ratio >= 0.15,
        forks_total: total_forks,
        rework_ratio: match (&rework_forks, total_forks) {
            (Some(r), t) if t > 0 => Some(*r as f64 / t as f64),
            (Some(0), _) => Some(0.0),
            _ => None,
        },
        rework_forks,
        attempt_available: has_attempt_col,
    };

    // ③ FailureCause 상위 12 — v2는 cause 컬럼, v1은 error_snippet 파싱
    let mut failure_causes = FailureCausesSection {
        available: false,
        source: None,
        rows: Vec::new(),
        reason: Some("decision_events table missing".to_string()),
    };
    let mut full_tally: Option<std::collections::BTreeMap<String, (i64, Option<String>)>> = None;
    if events_table {
        if has_cause_col {
            let sql = format!(
                "SELECT COALESCE(cause, 'legacy_unknown') AS cause, COUNT(*) AS cnt, MAX(error_snippet) AS example
                 FROM decision_events WHERE {}
                 GROUP BY cause
                 ORDER BY cnt DESC, cause ASC
                 LIMIT {}",
                ev_where, TOP_N
            );
            let rows: Vec<(String, i64, Option<String>)> = db
                .prepare(sql.as_str())
                .map(|mut stmt| {
                    stmt.query_map(
                        rusqlite::params_from_iter(ev_args.iter().map(|p| p.as_ref())),
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .map(|rows| rows.flatten().collect())
                    .unwrap_or_default()
                })
                .unwrap_or_default();
            failure_causes = FailureCausesSection {
                available: true,
                source: Some(CauseSource::Column),
                reason: None,
                rows: rows
                    .into_iter()
                    .map(|(cause, count, example)| CauseRow {
                        cause,
                        count,
                        ratio: ratio_of(count as f64, total_events as f64),
                        example,
                        candidate: count >= 3,
                    })
                    .collect(),
            };
        } else {
            // v1 fallback: 스니펫 접두 파싱(전체 집계 — ④의 stall 비율 재사용)
            let mut tally: std::collections::BTreeMap<String, (i64, Option<String>)> =
                std::collections::BTreeMap::new();
            let sql = format!("SELECT error_snippet FROM decision_events WHERE {}", ev_where);
            let snippets: Vec<Option<String>> = db
                .prepare(sql.as_str())
                .map(|mut stmt| {
                    stmt.query_map(
                        rusqlite::params_from_iter(ev_args.iter().map(|p| p.as_ref())),
                        |r| r.get(0),
                    )
                    .map(|rows| rows.flatten().collect())
                    .unwrap_or_default()
                })
                .unwrap_or_default();
            for snippet in &snippets {
                let cause = parse_cause_from_snippet(snippet.as_deref()).unwrap_or_else(|| "legacy_unknown".to_string());
                let (prev_count, prev_example) = tally.get(&cause).cloned().unwrap_or((0, None));
                // 첫 non-null 스니펫이 예시로 유지된다(TS cur?.example ?? r.error_snippet)
                let example = prev_example.or_else(|| snippet.clone());
                tally.insert(cause, (prev_count + 1, example));
            }
            let mut rows: Vec<CauseRow> = tally
                .iter()
                .map(|(cause, (count, example))| CauseRow {
                    cause: cause.clone(),
                    count: *count,
                    ratio: ratio_of(*count as f64, total_events as f64),
                    example: example.clone(),
                    candidate: *count >= 3,
                })
                .collect();
            rows.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.cause.cmp(&b.cause)));
            rows.truncate(TOP_N as usize);
            full_tally = Some(tally);
            failure_causes = FailureCausesSection {
                available: true,
                source: Some(CauseSource::Snippet),
                reason: None,
                rows,
            };
        }
    }

    // ④ ttl_orphan / stall 비율
    let ttl_orphan_events = if events_table {
        scalar(
            db,
            &format!("SELECT COUNT(*) AS c FROM decision_events WHERE {} AND source = 'ttl_orphan'", ev_where),
            &ev_args,
        )
    } else {
        0
    };
    let mut stall_events: Option<i64> = None;
    let mut stall_available = false;
    if events_table {
        if has_cause_col {
            stall_events = Some(scalar(
                db,
                &format!("SELECT COUNT(*) AS c FROM decision_events WHERE {} AND cause = 'stall'", ev_where),
                &ev_args,
            ));
            stall_available = true;
        } else if let Some(tally) = &full_tally {
            stall_events = Some(tally.get("stall").map(|(c, _)| *c).unwrap_or(0));
            stall_available = true;
        }
    }
    let ttl_ratio = ratio_of(ttl_orphan_events as f64, total_events as f64);
    let stall_ratio = stall_events.map(|s| ratio_of(s as f64, total_events as f64));
    let worst = f64::max(ttl_ratio, stall_ratio.unwrap_or(0.0));
    let orphan_stall = OrphanStallStats {
        available: events_table,
        ttl_orphan_events,
        ttl_orphan_ratio: ttl_ratio,
        stall_events,
        stall_ratio,
        stall_available,
        level: if worst >= 0.25 {
            OrphanLevel::Critical
        } else if worst >= 0.1 {
            OrphanLevel::Warn
        } else {
            OrphanLevel::None
        },
    };

    // Decision API 감사 집계 — forks.metadata_json.decision_api (손상 JSON은 json_valid로 배제)
    let mut decision_api_shadow = DecisionApiShadowSummary {
        total: 0,
        ok: 0,
        errors: 0,
        shadow_count: 0,
        on_applied_count: 0,
        disagreed: 0,
        disagreement_rate: None,
        total_input_tokens: 0,
        total_cost_usd: 0.0,
    };
    if forks_table {
        let sql = format!(
            "SELECT COUNT(*) AS total,
                    SUM(CASE WHEN json_extract(metadata_json, '$.decision_api.ok') = 1 THEN 1 ELSE 0 END) AS ok_cnt,
                    SUM(CASE WHEN json_extract(metadata_json, '$.decision_api.mode') = 'shadow' THEN 1 ELSE 0 END) AS shadow_cnt,
                    SUM(CASE WHEN json_extract(metadata_json, '$.decision_api.mode') = 'on'
                              AND json_extract(metadata_json, '$.decision_api.applied') = 1 THEN 1 ELSE 0 END) AS on_applied,
                    SUM(CASE WHEN json_extract(metadata_json, '$.decision_api.disagreed') = 1 THEN 1 ELSE 0 END) AS disagreed,
                    COALESCE(SUM(json_extract(metadata_json, '$.decision_api.input_tokens')), 0) AS input_tokens,
                    COALESCE(SUM(json_extract(metadata_json, '$.decision_api.cost_usd')), 0) AS cost_usd
             FROM forks
             WHERE {}
               AND json_valid(metadata_json)
               AND json_type(metadata_json, '$.decision_api') = 'object'",
            fork_where
        );
        let row: (i64, Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<i64>, Option<f64>) = db
            .query_row(
                sql.as_str(),
                rusqlite::params_from_iter(fork_args.iter().map(|p| p.as_ref())),
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .unwrap_or((0, None, None, None, None, None, None));
        let total = row.0;
        let ok_cnt = row.1.unwrap_or(0);
        let disagreed = row.4.unwrap_or(0);
        decision_api_shadow = DecisionApiShadowSummary {
            total,
            ok: ok_cnt,
            errors: total - ok_cnt,
            shadow_count: row.2.unwrap_or(0),
            on_applied_count: row.3.unwrap_or(0),
            disagreed,
            disagreement_rate: if total > 0 { Some(disagreed as f64 / total as f64) } else { None },
            total_input_tokens: row.5.unwrap_or(0),
            total_cost_usd: row.6.unwrap_or(0.0),
        };
    }

    let mut candidates: Vec<String> = Vec::new();
    if tool_repetition.available {
        for r in &tool_repetition.rows {
            if r.candidate {
                candidates.push(format!("tool_repetition:{}", r.tool_name));
            }
        }
    }
    if pingpong.candidate {
        candidates.push("pingpong".to_string());
    }
    if failure_causes.available {
        for r in &failure_causes.rows {
            if r.candidate {
                candidates.push(format!("failure_causes:{}", r.cause));
            }
        }
    }
    if orphan_stall.level != OrphanLevel::None {
        candidates.push(format!("orphan_stall:{}", orphan_stall.level.as_str()));
    }

    RuleCandidateReport {
        command: "report",
        schema_version: 1,
        generated_at_ms: now_ms,
        window: ReportWindow { days, from_ms, to_ms: now_ms },
        project_id,
        totals: ReportTotals { events: total_events, forks: total_forks },
        sections: ReportSections { tool_repetition, pingpong, failure_causes, orphan_stall },
        thresholds: ReportThresholds {
            tool_min_events: 5,
            pingpong_ratio: 0.15,
            cause_min_count: 3,
            orphan_warn: 0.1,
            orphan_critical: 0.25,
        },
        candidates,
        decision_api_shadow,
    }
}

// ---------- 사람 검토용 마크다운 렌더 ----------

fn fmt_ratio(v: Option<f64>) -> String {
    match v {
        None => "unavailable".to_string(),
        Some(x) => format!("{:.3}", x),
    }
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

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// 규칙 후보 마크다운 — 사람이 검토 후 수동 반영하는 용도뿐이다(자동 적용 금지)
pub fn format_report_markdown(report: &RuleCandidateReport) -> String {
    let mut lines: Vec<String> = Vec::new();
    let t = &report.thresholds;
    lines.push("---".to_string());
    lines.push(format!("candidate: {}", if report.candidates.is_empty() { "false" } else { "true" }));
    lines.push("applied: false".to_string());
    lines.push(format!("generated_at: {}", epoch_ms_to_iso_utc(report.generated_at_ms)));
    lines.push(format!("window_days: {}", report.window.days));
    lines.push(format!("project_id: {}", report.project_id.clone().unwrap_or_else(|| "(all)".to_string())));
    lines.push(format!("schema_version: {}", report.schema_version));
    lines.push("---".to_string());
    lines.push(String::new());
    lines.push("# 규칙 후보 (자동 적용 금지 — 사람이 검토 후 수동 반영)".to_string());
    lines.push(String::new());

    lines.push("## ① 반복 실패 도구 상위 12".to_string());
    if report.sections.tool_repetition.available {
        lines.push("| # | route | tool_name | tool_class | events | fail | fail_ratio | retries | 후보 |".to_string());
        lines.push("|--:|-------|-----------|------------|-------:|-----:|-----------:|--------:|------|".to_string());
        for (i, r) in report.sections.tool_repetition.rows.iter().enumerate() {
            lines.push(format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                i + 1,
                r.route,
                r.tool_name,
                r.tool_class,
                r.events,
                r.fail,
                fmt_ratio(Some(r.fail_ratio)),
                r.retries,
                if r.candidate { "**예**" } else { "아니오" }
            ));
        }
        lines.push(String::new());
        lines.push(format!("- 채택 임계: events >= {} && fail > 0 — 권고는 사람이 작성한다", t.tool_min_events));
    } else {
        lines.push(format!("- unavailable: {}", report.sections.tool_repetition.reason.clone().unwrap_or_default()));
    }
    lines.push(String::new());

    let pp = &report.sections.pingpong;
    lines.push("## ② 핑퐁 (재시도/재작업)".to_string());
    lines.push(format!(
        "- retry_or_abort 이벤트: {} / 전체 {} = **{}**",
        pp.retry_events, pp.total_events, fmt_ratio(Some(pp.retry_ratio))
    ));
    match pp.rework_forks {
        Some(rework) => lines.push(format!(
            "- rework fork (attempt >= 2 또는 rework_count >= 1): {} / {} = {}",
            rework, pp.forks_total, fmt_ratio(pp.rework_ratio)
        )),
        None => lines.push("- rework fork: unavailable (forks 테이블 부재)".to_string()),
    }
    if !pp.attempt_available {
        lines.push("- `forks.attempt` 컬럼 부재(v1 DB) — attempt >= 2 집계는 unavailable".to_string());
    }
    lines.push(format!("- 후보: {} (임계 {})", if pp.candidate { "**예**" } else { "아니오" }, t.pingpong_ratio));
    lines.push(String::new());

    lines.push("## ③ FailureCause 상위 12".to_string());
    if report.sections.failure_causes.available {
        lines.push(format!(
            "- 집계 소스: {}",
            if report.sections.failure_causes.source == Some(CauseSource::Column) {
                "cause 컬럼(v2)"
            } else {
                "error_snippet 파싱(v1 폴백)"
            }
        ));
        lines.push("| cause | count | ratio | 최근 예시(리댁션) |".to_string());
        lines.push("|-------|------:|------:|------------------|".to_string());
        for r in &report.sections.failure_causes.rows {
            // 2차 방어 리댁션 — 절단 후 표 이스케이프
            let example = match &r.example {
                Some(e) => redact_secrets(&truncate_chars(e, 120)).replace('|', "\\|"),
                None => "(없음)".to_string(),
            };
            lines.push(format!("| {} | {} | {} | `{}` |", r.cause, r.count, fmt_ratio(Some(r.ratio)), example));
        }
    } else {
        lines.push(format!("- unavailable: {}", report.sections.failure_causes.reason.clone().unwrap_or_default()));
    }
    lines.push(String::new());

    let os = &report.sections.orphan_stall;
    lines.push("## ④ 고아/스톨".to_string());
    lines.push(format!(
        "- ttl_orphan: {} / 전체 {} = **{}** ({})",
        os.ttl_orphan_events,
        report.totals.events,
        fmt_ratio(Some(os.ttl_orphan_ratio)),
        os.level.as_str()
    ));
    if os.stall_available {
        lines.push(format!("- stall: {} / 전체 {} = {}", os.stall_events.unwrap_or(0), report.totals.events, fmt_ratio(os.stall_ratio)));
    } else {
        lines.push("- stall: unavailable".to_string());
    }
    lines.push(format!("- 경고 임계 {} / 심각 임계 {}", t.orphan_warn, t.orphan_critical));
    lines.push(String::new());

    let da = &report.decision_api_shadow;
    if da.total > 0 {
        lines.push("## Decision API Shadow 요약".to_string());
        lines.push(format!(
            "- API 평가: {}건 (shadow {} · on+적용 {} · 오류 {})",
            da.total, da.shadow_count, da.on_applied_count, da.errors
        ));
        lines.push(format!("- 로컬 판정과 불일치: {}건 (불일치율 {})", da.disagreed, fmt_ratio(da.disagreement_rate)));
        lines.push(format!("- 입력 토큰 합계: {} / 추정 비용: ${:.6}", da.total_input_tokens, da.total_cost_usd));
        lines.push(String::new());
    }

    lines.push("## ⑤ 적용 방법 (수동)".to_string());
    lines.push("1. 위 표에서 채택할 항목만 고른다.".to_string());
    lines.push("2. `LESSONS.md`의 기존 규칙과 중복인지 먼저 확인한다(중복 규칙 신설 금지).".to_string());
    lines.push("3. 한 줄 규칙(트리거 + 필수/금지 행동)으로 축약해 **사람이 직접** 편집한다.".to_string());
    lines.push("4. `dg report`는 이 과정에 관여하지 않는다.".to_string());
    lines.push(String::new());
    lines.join("\n")
}

// ---------- `--out` 경로 Allowlist 가드 (검증서 A-1) ----------

#[derive(Debug, Clone, Default)]
pub struct OutPathValidation {
    pub allowed: bool,
    pub resolved_path: Option<PathBuf>,
    pub reason: Option<String>,
}

fn absolute_path(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(p)
    }
}

/// 존재하면 realpath(canonicalize), 아니면 resolve — macOS /var↔/private/var 등 심볼릭 접두 정규화
fn normalize_dir(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| absolute_path(p))
}

/// 대상 파일 경로 정규화 — 파일이 존재하면 realpath, 아니면 부모 realpath + basename
fn normalize_file_path(p: &Path) -> PathBuf {
    let resolved = absolute_path(p);
    if let Ok(c) = fs::canonicalize(&resolved) {
        return c;
    }
    // 파일 미존재 — 부모 경로로 정규화
    if let Some(parent) = resolved.parent() {
        if let Ok(parent_real) = fs::canonicalize(parent) {
            if let Some(name) = resolved.file_name() {
                return parent_real.join(name);
            }
        }
    }
    resolved
}

fn is_inside(base: &Path, target: &Path) -> bool {
    target.starts_with(base)
}

/// `dg report --out` 경로 allowlist 판정.
/// - 허용: `<repo_dir>/.herdr/report/` 내부, 또는 `<repo_dir>` 외부 절대경로(예: /tmp).
/// - 거부: 그 외 저장소 내부 전 경로(`src/`, `scripts/`, `.agents/`, `.herdr/roles/`,
///   `tests/`, `package.json`, `.pi/`, `..` 순회 결과 등).
/// 부모 디렉터리 realpath로 심볼릭 링크를 통한 우회(`src-link/x.md`)도 막는다.
pub fn validate_report_out_path(out_path: &str, repo_dir: &Path) -> OutPathValidation {
    if out_path.trim().is_empty() {
        return OutPathValidation { allowed: false, resolved_path: None, reason: Some("empty_out_path".to_string()) };
    }
    let resolved = normalize_file_path(Path::new(out_path));
    let repo_plain = absolute_path(repo_dir);
    let repo_real = normalize_dir(repo_dir);
    let mut repo_candidates: Vec<PathBuf> = vec![repo_plain];
    if repo_real != repo_candidates[0] {
        repo_candidates.push(repo_real);
    }
    let report_roots: Vec<PathBuf> = repo_candidates
        .iter()
        .map(|r| normalize_file_path(&r.join(".herdr").join("report")))
        .collect();
    if report_roots.iter().any(|rr| is_inside(rr, &resolved)) {
        return OutPathValidation { allowed: true, resolved_path: Some(resolved), reason: None };
    }
    if repo_candidates.iter().any(|r| is_inside(r, &resolved)) {
        return OutPathValidation {
            allowed: false,
            resolved_path: Some(resolved),
            reason: Some("forbidden_out_path".to_string()),
        };
    }
    OutPathValidation { allowed: true, resolved_path: Some(resolved), reason: None }
}

#[derive(Debug, Clone, Default)]
pub struct WriteReportFileResult {
    pub written: bool,
    pub resolved_path: Option<PathBuf>,
    pub reason: Option<String>,
}

/// allowlist 통과 경로에만 리포트 파일을 쓴다. 거부 시 파일은 물론 부모 디렉터리도
/// 생성하지 않는다(부작용 0).
pub fn write_report_file_safe(out_path: &str, repo_dir: &Path, content: &str) -> WriteReportFileResult {
    let v = validate_report_out_path(out_path, repo_dir);
    if !v.allowed || v.resolved_path.is_none() {
        return WriteReportFileResult {
            written: false,
            resolved_path: v.resolved_path,
            reason: v.reason.or_else(|| Some("forbidden_out_path".to_string())),
        };
    }
    let resolved = v.resolved_path.unwrap();
    let write = (|| -> std::io::Result<()> {
        if let Some(parent) = resolved.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&resolved, content)
    })();
    match write {
        Ok(()) => WriteReportFileResult { written: true, resolved_path: Some(resolved), reason: None },
        Err(e) => WriteReportFileResult {
            written: false,
            resolved_path: Some(resolved),
            reason: Some(format!("write_failed: {}", e)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrate_v1_tables;
    use crate::testutil::temp_dir;
    use sha2::{Digest, Sha256};

    fn sha256_file(p: &Path) -> String {
        let mut h = Sha256::new();
        h.update(fs::read(p).unwrap());
        h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
    }

    #[test]
    fn v1_db_report_aggregation_snippet_fallback() {
        let tmp = temp_dir("rep-v1");
        fs::create_dir_all(&tmp).unwrap();
        let v1_path = tmp.join("v1.db");
        let now = 1_700_000_000_000i64;
        {
            let raw = Connection::open(&v1_path).unwrap();
            migrate_v1_tables(&raw).unwrap();
            let ins_fork = "INSERT INTO forks (fork_id, project_id, project_label, route, tool_class, decision, forced_split,
                p_at_gate, basis_at_gate, status, ttl_ms, created_at_ms, last_touch_ms, metadata_json)
               VALUES (?, 'proj_t1', 't1', ?, ?, ?, 0, 0.5, 'prior', 'open', 600000, ?, ?, ?)";
            raw.execute(ins_fork, rusqlite::params!["fk_1", "tool_select", "shell_exec", "split", now - 1000, now - 500, Option::<String>::None]).unwrap();
            raw.execute(ins_fork, rusqlite::params!["fk_2", "tool_select", "shell_exec", "split", now - 1000, now - 500, r#"{"rework_count": 1}"#]).unwrap();
            raw.execute(ins_fork, rusqlite::params!["fk_3", "file_select", "file_read", "sharp", now - 1000, now - 500, Option::<String>::None]).unwrap();
            let ins_ev = "INSERT INTO decision_events (dedupe_key, project_id, source, route, tool_name, tool_class, ok, is_retry, error_snippet, created_at_ms)
               VALUES (?, 'proj_t1', ?, ?, ?, ?, ?, ?, ?, ?)";
            for i in 0..6 {
                raw.execute(ins_ev, rusqlite::params![format!("e-t-{}", i), "wrap", "tool_select", "run_command", "shell_exec", 0, 0, "cause=tool_error; exit 1 boom", now - i]).unwrap();
            }
            for i in 0..2 {
                raw.execute(ins_ev, rusqlite::params![format!("e-o-{}", i), "wrap", "tool_select", "run_command", "shell_exec", 1, 0, Option::<String>::None, now - i]).unwrap();
            }
            for i in 0..3 {
                raw.execute(ins_ev, rusqlite::params![format!("e-r-{}", i), "wrap", "retry_or_abort", "run_command", "shell_exec", 0, 1, "cause=tool_error; retry again", now - i]).unwrap();
            }
            for i in 0..2 {
                raw.execute(ins_ev, rusqlite::params![format!("e-ttl-{}", i), "ttl_orphan", "file_select", "read_file", "file_read", 0, 0, "cause=stall; silent", now - i]).unwrap();
            }
            raw.execute(ins_ev, rusqlite::params!["e-h-0", "hook", "file_select", "read_file", "file_read", 1, 0, Option::<String>::None, now]).unwrap();
        }

        let hash_before = sha256_file(&v1_path);
        let db = open_report_db(Some(v1_path.to_str().unwrap())).unwrap();
        let rep = generate_rule_candidate_report(
            &db,
            &ReportOptions { now_ms: Some(now), ..Default::default() },
        );
        let md = format_report_markdown(&rep);
        drop(db);

        assert_eq!(rep.totals.events, 14);
        assert_eq!(rep.totals.forks, 3);
        assert!(rep.sections.tool_repetition.available);
        let run_cmd = rep
            .sections
            .tool_repetition
            .rows
            .iter()
            .find(|r| r.tool_name == "run_command")
            .unwrap();
        assert_eq!(run_cmd.fail, 6);
        assert_eq!(run_cmd.events, 8);
        assert!(run_cmd.candidate);
        assert!(rep.sections.pingpong.available);
        assert!(!rep.sections.pingpong.attempt_available, "v1 DB는 attempt 컬럼 부재 → unavailable");
        assert!((rep.sections.pingpong.retry_ratio - 3.0 / 14.0).abs() < 1e-12);
        assert!(rep.sections.pingpong.candidate);
        assert_eq!(rep.sections.pingpong.rework_forks, Some(1), "metadata rework_count 파싱");
        assert!(rep.sections.failure_causes.available);
        assert_eq!(rep.sections.failure_causes.source, Some(CauseSource::Snippet));
        assert_eq!(rep.sections.failure_causes.rows[0].cause, "tool_error");
        assert_eq!(rep.sections.failure_causes.rows[0].count, 9);
        assert!((rep.sections.orphan_stall.ttl_orphan_ratio - 2.0 / 14.0).abs() < 1e-12);
        assert_eq!(rep.sections.orphan_stall.level, OrphanLevel::Warn);
        assert_eq!(rep.sections.orphan_stall.stall_events, Some(2), "v1 stall은 snippet 파싱으로 집계");
        assert!(rep.candidates.iter().any(|c| c == "tool_repetition:run_command"));
        assert!(rep.candidates.iter().any(|c| c == "failure_causes:tool_error"));
        assert!(rep.candidates.iter().any(|c| c == "orphan_stall:warn"));
        assert!(md.contains("자동 적용 금지") && md.contains("run_command"), "마크다운 렌더에 핵심 섹션 존재");
        assert_eq!(sha256_file(&v1_path), hash_before, "v1 DB 파일이 리포트 실행 전후 불변");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn v2_db_report_aggregation_cause_column() {
        let tmp = temp_dir("rep-v2");
        let v2_path = tmp.join("v2.db");
        let now = 1_700_000_000_000i64;
        {
            let w = crate::db::open_db(Some(v2_path.to_str().unwrap()), OpenDbOptions::default()).unwrap();
            let ins_fork = "INSERT INTO forks (fork_id, project_id, project_label, route, tool_class, decision, forced_split,
                p_at_gate, basis_at_gate, generation, attempt, status, ttl_ms, created_at_ms, last_touch_ms)
               VALUES (?, 'proj_t2', 't2', 'tool_select', 'shell_exec', 'split', 0, 0.5, 'prior', 1, ?, 'open', 600000, ?, ?)";
            w.execute(ins_fork, rusqlite::params!["fk_a", 2, now - 1000, now - 500]).unwrap();
            w.execute(ins_fork, rusqlite::params!["fk_b", 1, now - 1000, now - 500]).unwrap();
            let ins_ev = "INSERT INTO decision_events (dedupe_key, project_id, source, route, tool_name, tool_class, ok, is_retry, error_snippet, cause, created_at_ms)
               VALUES (?, 'proj_t2', ?, 'tool_select', 'run_command', 'shell_exec', 0, 0, ?, ?, ?)";
            for i in 0..5 {
                w.execute(ins_ev, rusqlite::params![format!("v2-t-{}", i), "wrap", "cause=tool_error; boom", "tool_error", now - i]).unwrap();
            }
            w.execute(ins_ev, rusqlite::params!["v2-l-0", "wrap", Option::<String>::None, Option::<String>::None, now]).unwrap();
            w.execute(ins_ev, rusqlite::params!["v2-l-1", "wrap", Option::<String>::None, Option::<String>::None, now]).unwrap();
            w.execute(ins_ev, rusqlite::params!["v2-s-0", "ttl_orphan", "cause=stall; silent", "stall", now]).unwrap();
            // decision_api 감사 메타데이터 2건 (shadow 불일치 1 + on 적용 1)
            w.execute(
                "UPDATE forks SET metadata_json = ?1 WHERE fork_id = 'fk_a'",
                rusqlite::params![r#"{"decision_api":{"ok":1,"mode":"shadow","applied":0,"disagreed":1,"input_tokens":200,"cost_usd":0.00002}}"#],
            )
            .unwrap();
            w.execute(
                "UPDATE forks SET metadata_json = ?1 WHERE fork_id = 'fk_b'",
                rusqlite::params![r#"{"decision_api":{"ok":1,"mode":"on","applied":1,"disagreed":0,"input_tokens":100,"cost_usd":0.00001}}"#],
            )
            .unwrap();
        }

        let db = open_report_db(Some(v2_path.to_str().unwrap())).unwrap();
        let rep = generate_rule_candidate_report(
            &db,
            &ReportOptions { now_ms: Some(now), ..Default::default() },
        );
        drop(db);

        assert_eq!(rep.totals.events, 8);
        assert!(rep.sections.failure_causes.available);
        assert_eq!(rep.sections.failure_causes.source, Some(CauseSource::Column));
        assert!(rep.sections.failure_causes.rows.iter().any(|r| r.cause == "tool_error" && r.count == 5));
        assert!(rep.sections.failure_causes.rows.iter().any(|r| r.cause == "legacy_unknown" && r.count == 2), "NULL cause는 legacy_unknown 버킷");
        assert!(rep.sections.pingpong.attempt_available);
        assert_eq!(rep.sections.pingpong.rework_forks, Some(1), "attempt >= 2 fork 집계");
        assert_eq!(rep.sections.orphan_stall.stall_events, Some(1), "v2 stall은 cause 컬럼으로 집계");
        // decision_api shadow 집계
        let da = &rep.decision_api_shadow;
        assert_eq!(da.total, 2);
        assert_eq!(da.ok, 2);
        assert_eq!(da.shadow_count, 1);
        assert_eq!(da.on_applied_count, 1);
        assert_eq!(da.disagreed, 1);
        assert!((da.disagreement_rate.unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(da.total_input_tokens, 300);
        assert!((da.total_cost_usd - 0.00003).abs() < 1e-9);
        let md = format_report_markdown(&rep);
        assert!(md.contains("## Decision API Shadow 요약"), "total>0이면 마크다운 섹션 출력");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn readonly_report_connection_never_writes() {
        let tmp = temp_dir("rep-ro");
        let path = tmp.join("ro.db");
        {
            let _w = crate::db::open_db(Some(path.to_str().unwrap()), OpenDbOptions::default()).unwrap();
        }
        let ro = open_report_db(Some(path.to_str().unwrap())).unwrap();
        assert!(ro.execute("DELETE FROM stats_summary", []).is_err(), "query_only 연결은 쓰기 거부");
        let missing = open_report_db(Some(tmp.join("missing.db").to_str().unwrap()));
        assert!(missing.is_err(), "읽기전용 열기는 파일을 생성하지 않는다");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn out_path_allowlist_guards_sensitive_repo_paths() {
        let repo = temp_dir("rep-allow");
        for d in ["src", "scripts", ".agents", ".herdr/report", ".herdr/roles"] {
            fs::create_dir_all(repo.join(d)).unwrap();
        }
        // 심볼릭 링크 우회: repo/src-link → repo/src
        std::os::unix::fs::symlink(repo.join("src"), repo.join("src-link")).unwrap();
        fs::write(repo.join("package.json"), "{}").unwrap();

        let denied = [
            repo.join("src").join("x.md"),
            repo.join("scripts").join("x.md"),
            repo.join(".agents").join("x.md"),
            repo.join(".herdr").join("roles").join("lead.md"),
            repo.join(".herdr").join("report").join("..").join("..").join("src").join("x.md"),
            repo.join("src-link").join("x.md"),
            repo.join("package.json"),
        ];
        for p in &denied {
            let v = validate_report_out_path(p.to_str().unwrap(), &repo);
            assert!(!v.allowed, "거부 예상: {:?}", p);
            assert_eq!(v.reason.as_deref(), Some("forbidden_out_path"));
        }

        let outside = tmp_outside_path("rep-outside.md");
        let allowed = [repo.join(".herdr").join("report").join("r1.md"), outside.clone()];
        for p in &allowed {
            let v = validate_report_out_path(p.to_str().unwrap(), &repo);
            assert!(v.allowed, "허용 예상: {:?}", p);
            assert!(v.resolved_path.is_some());
        }

        // 거부 경로는 파일은 물론 부모 디렉터리도 생성하지 않는다
        let w_deny = write_report_file_safe(
            repo.join("src").join("deep").join("x.md").to_str().unwrap(),
            &repo,
            "nope",
        );
        assert!(!w_deny.written);
        assert!(!repo.join("src").join("deep").exists(), "거부 시 부모 디렉터리 미생성");

        let w_ok = write_report_file_safe(
            repo.join(".herdr").join("report").join("r1.md").to_str().unwrap(),
            &repo,
            "# report\n",
        );
        assert!(w_ok.written, "허용 경로 쓰기 실패: {:?}", w_ok.reason);
        assert_eq!(fs::read_to_string(repo.join(".herdr").join("report").join("r1.md")).unwrap(), "# report\n");

        let w_outside = write_report_file_safe(outside.to_str().unwrap(), &repo, "# tmp\n");
        assert!(w_outside.written);
        assert!(outside.exists());
        let _ = fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&repo);
    }

    fn tmp_outside_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        std::env::temp_dir().join(format!("dgate-report-{}-{}-{}", name, std::process::id(), nanos))
    }

    #[test]
    fn empty_out_path_rejected() {
        let repo = temp_dir("rep-empty");
        let v = validate_report_out_path("   ", &repo);
        assert!(!v.allowed);
        assert_eq!(v.reason.as_deref(), Some("empty_out_path"));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn iso_timestamp_and_cause_parsing() {
        assert_eq!(epoch_ms_to_iso_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(epoch_ms_to_iso_utc(1_767_225_600_000), "2026-01-01T00:00:00.000Z");
        assert_eq!(
            parse_cause_from_snippet(Some("cause=stall; silent")).as_deref(),
            Some("stall")
        );
        assert_eq!(parse_cause_from_snippet(Some("no prefix")), None);
        assert_eq!(parse_cause_from_snippet(Some("cause=bogus; x")), None, "미상 enum은 추정 금지");
        assert_eq!(parse_cause_from_snippet(None), None);
    }
}
