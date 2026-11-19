//! TUI·매퍼용 읽기전용 텔레메트리 어댑터 — `src/decision/telemetry.ts` 포팅.
//!
//! 목적: 관제 구독이 SSOT를 `read_only + query_only`로만 읽게 하고,
//! DB 미존재·오류 시 휴리스틱 사전확률(prior)로 즉시 폴백해 관제 화면이 죽지 않게 한다.
//!
//! 주의: 이 모듈은 표시용 읽기 경로만 제공한다. 통계 갱신 판정은
//! `engine::record_tool_outcome`을 경유해야 하며(단일 구현 원칙),
//! reader 연결로 쓰지 않는다.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension};

use crate::db::{has_table, open_db_safe, resolve_db_path, resolve_project_identity, OpenDbOptions};
use crate::engine::{get_posterior, prior_p0, DecisionBasis, PosteriorResult, RouteId, ROUTE_IDS};

/// 라우트별 TUI 표기명 — v1 store 라벨 체계와 동일
pub fn route_label(id: RouteId) -> &'static str {
    match id {
        RouteId::FileSelect => "어느 파일",
        RouteId::ToolSelect => "어느 도구",
        RouteId::RetryOrAbort => "재시도 또는 중단",
        RouteId::DelegateSwarm => "스웜 위임",
    }
}

/// 라우트별 대표 tool_class (표본 없을 때 휴리스틱 조회 기준 키)
fn representative_class(id: RouteId) -> &'static str {
    match id {
        RouteId::FileSelect => "file_read",
        RouteId::ToolSelect => "shell_exec",
        RouteId::RetryOrAbort => "other",
        RouteId::DelegateSwarm => "swarm_dispatch",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TelemetryRoute {
    pub id: RouteId,
    pub name: &'static str,
    pub p: f64,
    pub basis: crate::engine::DecisionBasis,
    pub samples: i64,
    pub consecutive_errors: i64,
    pub forced_split: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecentFork {
    pub fork_id: String,
    pub route: String,
    pub tool_class: String,
    pub status: String,
    pub created_at_ms: i64,
    pub last_touch_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TelemetrySnapshot {
    pub routes: Vec<TelemetryRoute>,
    pub db_path: PathBuf,
    pub live: bool,
    /// 현재 `open`/`waiting_human` fork 수
    pub open_forks: i64,
    /// 최근 생성 순 fork 요약(최대 10건)
    pub recent_forks: Vec<RecentFork>,
    pub total_events: i64,
    pub total_forks: i64,
}

/// DB 미존재·오류 시 휴리스틱 prior safe fallback(§3.5).
/// p0 테이블값을 `basis: Prior, samples: 0`으로 반환한다.
pub fn heuristic_prior_routes() -> Vec<TelemetryRoute> {
    let priors = |id: RouteId| -> f64 {
        match id {
            RouteId::FileSelect => 0.94,
            RouteId::ToolSelect => 0.88,
            RouteId::RetryOrAbort => 0.85,
            RouteId::DelegateSwarm => 0.8,
        }
    };
    ROUTE_IDS
        .iter()
        .map(|&id| TelemetryRoute {
            id,
            name: route_label(id),
            p: priors(id),
            basis: crate::engine::DecisionBasis::Prior,
            samples: 0,
            consecutive_errors: 0,
            forced_split: false,
        })
        .collect()
}

/// 읽기전용 구독 진입점: `read_only: true` + `PRAGMA query_only=ON`.
/// 실패 시 휴리스틱 폴백 스냅숏(live=false)을 반환한다.
/// v1(v0)·v2 공통으로 안전하게 읽는다(부분 생성·빈 DB 포함).
pub fn read_telemetry_snapshot(
    db_path_input: Option<&str>,
    project_cwd: Option<&Path>,
) -> TelemetrySnapshot {
    let db_path = resolve_db_path(db_path_input);
    let Some(db) = open_db_safe(
        Some(db_path.to_string_lossy().into_owned().as_str()),
        OpenDbOptions { read_only: true, busy_timeout_ms: Some(1000) },
    ) else {
        return TelemetrySnapshot {
            routes: heuristic_prior_routes(),
            db_path,
            live: false,
            open_forks: 0,
            recent_forks: Vec::new(),
            total_events: 0,
            total_forks: 0,
        };
    };
    let snapshot = build_snapshot(&db, &db_path, project_cwd);
    let _ = db.close();
    snapshot
}

fn build_snapshot(db: &Connection, db_path: &Path, project_cwd: Option<&Path>) -> TelemetrySnapshot {
    // v1(v0)·v2 공통: stats_summary만 읽는다. 부분 생성·빈 DB에서는 읽기 가능
    // 상태(live)를 유지하되 표본 0의 휴리스틱 prior를 반환한다 (PB-1 §3.4)
    if !has_table(db, "stats_summary") {
        return TelemetrySnapshot {
            routes: heuristic_prior_routes(),
            db_path: db_path.to_path_buf(),
            live: true,
            open_forks: 0,
            recent_forks: Vec::new(),
            total_events: 0,
            total_forks: 0,
        };
    }
    let cwd = match project_cwd {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
    };
    let identity = resolve_project_identity(&cwd);
    let routes = ROUTE_IDS
        .iter()
        .map(|&id| {
            // 라우트 내 최다 표본 tool_class를 대표로 선택(동률면 tool_class 이름순)
            let tool_class: Option<String> = db
                .query_row(
                    "SELECT tool_class FROM stats_summary
                     WHERE project_id = ?1 AND route = ?2 ORDER BY raw_samples DESC, tool_class ASC LIMIT 1",
                    rusqlite::params![identity.project_id, id.as_str()],
                    |r| r.get(0),
                )
                .optional()
                .ok()
                .flatten();
            let class = tool_class.unwrap_or_else(|| representative_class(id).to_string());
            let post = get_posterior(db, &identity.project_id, id, &class).unwrap_or_else(|_| PosteriorResult {
                p: prior_p0(id, &class),
                basis: DecisionBasis::Prior,
                samples: 0,
                consecutive_errors: 0,
                lambda: 0.0,
                p_hier: prior_p0(id, &class),
                ewma_success: 0.0,
                ewma_fail: 0.0,
            });
            TelemetryRoute {
                id,
                name: route_label(id),
                p: post.p,
                basis: post.basis,
                samples: post.samples,
                consecutive_errors: post.consecutive_errors,
                forced_split: post.consecutive_errors >= 2,
            }
        })
        .collect();

    // forks/decision_events 요약 — 테이블 부재(v1 부분 스키마)에는 0으로 안전 처리
    let total_forks = if has_table(db, "forks") {
        db.query_row("SELECT COUNT(*) FROM forks", [], |r| r.get(0)).unwrap_or(0)
    } else {
        0
    };
    let open_forks = if has_table(db, "forks") {
        db.query_row(
            "SELECT COUNT(*) FROM forks WHERE status IN ('open','waiting_human')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0)
    } else {
        0
    };
    let total_events = if has_table(db, "decision_events") {
        db.query_row("SELECT COUNT(*) FROM decision_events", [], |r| r.get(0)).unwrap_or(0)
    } else {
        0
    };
    let recent_forks: Vec<RecentFork> = if has_table(db, "forks") {
        match db.prepare(
            "SELECT fork_id, route, tool_class, status, created_at_ms, last_touch_ms
             FROM forks ORDER BY created_at_ms DESC LIMIT 10",
        ) {
            Ok(mut stmt) => stmt
                .query_map([], |r| {
                    Ok(RecentFork {
                        fork_id: r.get(0)?,
                        route: r.get(1)?,
                        tool_class: r.get(2)?,
                        status: r.get(3)?,
                        created_at_ms: r.get(4)?,
                        last_touch_ms: r.get(5)?,
                    })
                })
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    TelemetrySnapshot {
        routes,
        db_path: db_path.to_path_buf(),
        live: true,
        open_forks,
        recent_forks,
        total_events,
        total_forks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{open_db, OpenDbOptions};
    use crate::engine::{
        issue_fork, record_tool_outcome, EventSource, ForkIntervalPhase, IssueForkInput, RecordInput,
        RouteId,
    };
    use crate::testutil::temp_dir;

    #[test]
    fn missing_db_falls_back_to_heuristic_priors_not_live() {
        let tmp = temp_dir("tele-missing");
        let path = tmp.join("nope.db");
        let snap = read_telemetry_snapshot(Some(path.to_str().unwrap()), Some(&tmp));
        assert!(!snap.live);
        assert_eq!(snap.routes.len(), 4);
        let by_name = |n: &str| snap.routes.iter().find(|r| r.name == n).unwrap();
        assert_eq!(by_name("어느 파일").p, 0.94);
        assert_eq!(by_name("어느 도구").p, 0.88);
        assert_eq!(by_name("재시도 또는 중단").p, 0.85);
        assert_eq!(by_name("스웜 위임").p, 0.8);
        for r in &snap.routes {
            assert_eq!(r.samples, 0);
            assert!(!r.forced_split);
        }
        assert_eq!(snap.db_path, path);
    }

    #[test]
    fn empty_v2_db_is_live_with_prior_basis() {
        let tmp = temp_dir("tele-empty");
        let path = tmp.join("decision.db");
        {
            let _conn = open_db(Some(path.to_str().unwrap()), OpenDbOptions::default()).unwrap();
        }
        let snap = read_telemetry_snapshot(Some(path.to_str().unwrap()), Some(&tmp));
        assert!(snap.live);
        assert_eq!(snap.total_events, 0);
        assert_eq!(snap.total_forks, 0);
        assert_eq!(snap.open_forks, 0);
        assert!(snap.recent_forks.is_empty());
        // 표본 없음 → prior 기반이지만 live
        assert_eq!(snap.routes[0].basis, crate::engine::DecisionBasis::Prior);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn snapshot_reflects_samples_and_top_tool_class() {
        let tmp = temp_dir("tele-samples");
        let path = tmp.join("decision.db");
        {
            let conn = open_db(Some(path.to_str().unwrap()), OpenDbOptions::default()).unwrap();
            let pid = resolve_project_identity(&tmp).project_id;
            for _ in 0..3 {
                record_tool_outcome(
                    &conn,
                    &RecordInput {
                        project_id: &pid,
                        project_label: "T",
                        route: RouteId::FileSelect,
                        tool_class: "file_read",
                        tool_name: "read",
                        ok: true,
                        source: Some(EventSource::Cli),
                        now_ms: Some(1000),
                        ..Default::default()
                    },
                )
                .unwrap();
            }
            issue_fork(
                &conn,
                &IssueForkInput {
                    project_id: &pid,
                    project_label: "T",
                    route: RouteId::ToolSelect,
                    tool_class: "shell_exec",
                    decision: "split",
                    p_at_gate: 0.5,
                    basis_at_gate: crate::engine::DecisionBasis::Prior,
                    forced_split: false,
                    conversation_id: None,
                    ttl_ms: None,
                    title: None,
                    metadata: None,
                    now_ms: Some(2000),
                },
            )
            .unwrap();
            // 참고: ForkIntervalPhase import는 v2 스키마 검증용 — 스키마 존재 확인
            assert!(crate::db::has_table(&conn, "fork_intervals"));
            let _ = ForkIntervalPhase::Wait;
        }
        let snap = read_telemetry_snapshot(Some(path.to_str().unwrap()), Some(&tmp));
        assert!(snap.live);
        assert_eq!(snap.total_events, 3);
        assert_eq!(snap.total_forks, 1);
        assert_eq!(snap.open_forks, 1);
        assert_eq!(snap.recent_forks.len(), 1);
        assert_eq!(snap.recent_forks[0].status, "open");
        assert_eq!(snap.recent_forks[0].route, "tool_select");
        // file_select 라우트: 표본 3건 < 5 → prior basis지만 EWMA에 실측 반영
        let fs = snap.routes.iter().find(|r| r.id == RouteId::FileSelect).unwrap();
        assert_eq!(fs.samples, 3);
        assert!(fs.p > 0.94, "ok 3건이면 prior 0.94보다 위로: {}", fs.p);
        assert!(!fs.forced_split);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn forced_flag_after_two_consecutive_errors() {
        let tmp = temp_dir("tele-forced");
        let path = tmp.join("decision.db");
        {
            let conn = open_db(Some(path.to_str().unwrap()), OpenDbOptions::default()).unwrap();
            let pid = resolve_project_identity(&tmp).project_id;
            for _ in 0..2 {
                record_tool_outcome(
                    &conn,
                    &RecordInput {
                        project_id: &pid,
                        project_label: "T",
                        route: RouteId::FileSelect,
                        tool_class: "file_read",
                        tool_name: "read",
                        ok: false,
                        source: Some(EventSource::Cli),
                        now_ms: Some(1000),
                        ..Default::default()
                    },
                )
                .unwrap();
            }
        }
        let snap = read_telemetry_snapshot(Some(path.to_str().unwrap()), Some(&tmp));
        assert!(snap.live);
        let fs = snap.routes.iter().find(|r| r.id == RouteId::FileSelect).unwrap();
        assert_eq!(fs.consecutive_errors, 2);
        assert!(fs.forced_split);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn readonly_connection_never_writes() {
        let tmp = temp_dir("tele-ro");
        let path = tmp.join("decision.db");
        {
            let _conn = open_db(Some(path.to_str().unwrap()), OpenDbOptions::default()).unwrap();
        }
        let ro = open_db(
            Some(path.to_str().unwrap()),
            OpenDbOptions { read_only: true, busy_timeout_ms: Some(1000) },
        )
        .unwrap();
        assert!(ro.execute("DELETE FROM stats_summary", []).is_err());
        let snap = read_telemetry_snapshot(Some(path.to_str().unwrap()), Some(&tmp));
        assert!(snap.live);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
