//! `dg stats` — 읽기전용 비용·타이밍 통계(dg.ts cmdStats 대응).
//!
//! PB-2.5: `readOnly: true` 연결만 사용하고 `settle_orphans`를 절대 호출하지
//! 않는다(v1 DB 파일 불변).
use serde_json::{json, Value};

use crate::args::{optional_flag, parse_int_flag, ParsedArgs};
use crate::util::prelude::*;
use dgate_core::stats::{compute_cost_stats, compute_timing_stats, CostStatsOptions, TimingStatsOptions};

fn cost_json(c: &dgate_core::stats::CostStatsResult) -> Value {
    fn bucket(b: &dgate_core::stats::CostStatusBucket) -> Value {
        json!({
            "events": b.events,
            "input_tokens": b.input_tokens,
            "output_tokens": b.output_tokens,
            "cache_read_tokens": b.cache_read_tokens,
            "cache_write_tokens": b.cache_write_tokens,
            "usd": b.usd,
        })
    }
    json!({
        "schema_v1": c.schema_v1,
        "total_events": c.total_events,
        "by_status": {
            "reported": c.by_status.reported,
            "estimated": c.by_status.estimated,
            "unknown": c.by_status.unknown,
        },
        "per_status": {
            "reported": bucket(&c.per_status.reported),
            "estimated": bucket(&c.per_status.estimated),
            "unknown": bucket(&c.per_status.unknown),
        },
        "reported_usd": c.reported_usd,
        "estimated_usd": c.estimated_usd,
        "known_usd": c.known_usd,
        "unknown_events": c.unknown_events,
        "total_usd_display": c.total_usd_display,
        "tokens": {
            "input": c.tokens.input,
            "output": c.tokens.output,
            "cache_read": c.tokens.cache_read,
            "cache_write": c.tokens.cache_write,
        },
    })
}

fn timing_json(t: &dgate_core::stats::TimingStatsResult) -> Value {
    json!({
        "intervals_unavailable": t.intervals_unavailable,
        "coverage_ratio": t.coverage_ratio,
        "execution_ms": t.execution_ms,
        "sum_execution_ms": t.sum_execution_ms,
        "parallel_gain_ms": t.parallel_gain_ms,
        "wait_ms": t.wait_ms,
        "human_wait_ms": t.human_wait_ms,
        "wall_ms": t.wall_ms,
        "dropped": t.dropped,
        "forks_total": t.forks_total,
        "forks_with_intervals": t.forks_with_intervals,
        "intervals": t.intervals,
    })
}

pub fn cmd_stats(args: &ParsedArgs, json: bool) -> CmdResult {
    let want_cost = crate::args::bool_flag(args, "cost");
    let want_timing = crate::args::bool_flag(args, "timing");
    let include_cost = want_cost || !want_timing;
    let include_timing = want_timing || !want_cost;
    let days = parse_int_flag(args, "days", 14)?;
    let fork_id = optional_flag(args, "fork");
    let project_dir = optional_flag(args, "project");
    let project_id = project_dir
        .as_deref()
        .map(|d| resolve_identity(Some(d)).map(|i| i.project_id))
        .transpose()?;
    let db = open_reader(db_flag(args).as_deref())?;

    let cost = if include_cost {
        Some(compute_cost_stats(
            &db,
            &CostStatsOptions { project_id: project_id.clone(), days: Some(days), now_ms: None },
        ))
    } else {
        None
    };
    let timing = if include_timing {
        Some(compute_timing_stats(
            &db,
            &TimingStatsOptions {
                project_id: project_id.clone(),
                fork_id: fork_id.clone(),
                days: Some(days),
                now_ms: None,
            },
        ))
    } else {
        None
    };

    let human = {
        let mut parts: Vec<String> = Vec::new();
        if let Some(c) = &cost {
            let usd = match c.total_usd_display {
                Some(v) => format!("${v:.4}"),
                None => "unavailable".to_string(),
            };
            parts.push(format!(
                "cost events={} known_usd={usd} unknown={}{}",
                c.total_events,
                c.unknown_events,
                if c.schema_v1 { " (schema_v1)" } else { "" },
            ));
        }
        if let Some(t) = &timing {
            parts.push(if t.intervals_unavailable {
                "timing intervals_unavailable (v1 schema)".to_string()
            } else {
                format!(
                    "timing exec={:.0}ms wait={:.0}ms wall={:.0}ms coverage={:.3}",
                    t.execution_ms, t.wait_ms, t.wall_ms, t.coverage_ratio
                )
            });
        }
        format!("[stats] {}", parts.join(" · "))
    };
    emit(
        json,
        human,
        &json!({
            "cmd": "stats",
            "project_id": project_id,
            "days": days,
            "fork_id": fork_id,
            "cost": cost.as_ref().map(cost_json),
            "timing": timing.as_ref().map(timing_json),
        }),
    );
    Ok(0)
}
