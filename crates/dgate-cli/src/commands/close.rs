//! `dg close` — fork 닫힘 + 닫힘 책임 매트릭스 강제(dg.ts cmdClose 대응).
use serde_json::json;

use crate::args::{optional_flag, require_flag, ParsedArgs};
use crate::util::prelude::*;
use dgate_core::engine;

/// §3.3 닫힘 책임 매트릭스: rerouted는 worker·dispatcher만, ok/fail은 ttl 제외.
const CLOSE_BY_OK_FAIL: [&str; 4] = ["dispatcher", "lead", "worker", "verifier"];
const CLOSE_BY_REROUTED: [&str; 2] = ["worker", "dispatcher"];

pub fn cmd_close(args: &ParsedArgs, json: bool) -> CmdResult {
    let fork_id = require_flag(args, "fork")?;
    let outcome_raw = optional_flag(args, "outcome");
    let ok_alias = optional_flag(args, "ok");
    if outcome_raw.is_some() && ok_alias.is_some() {
        return Err(DgError::Usage(crate::args::UsageError(
            "use either --outcome or --ok, not both".into(),
        )));
    }
    let outcome: &str = match (outcome_raw.as_deref(), ok_alias.as_deref()) {
        (Some("ok"), _) | (None, Some("1")) => "ok",
        (Some("fail"), _) | (None, Some("0")) => "fail",
        (Some("rerouted"), _) => "rerouted",
        (Some(other), _) => {
            return Err(DgError::Usage(crate::args::UsageError(format!(
                "invalid --outcome \"{other}\" (allowed: ok|fail|rerouted)"
            ))))
        }
        (None, Some(other)) => {
            return Err(DgError::Usage(crate::args::UsageError(format!(
                "invalid --ok \"{other}\" (allowed: 0|1)"
            ))))
        }
        (None, None) => {
            return Err(DgError::Usage(crate::args::UsageError(
                "missing required flag --outcome (or --ok 0|1)".into(),
            )))
        }
    };
    let by = optional_flag(args, "by").unwrap_or_else(|| "dispatcher".into());
    if !["dispatcher", "lead", "worker", "ttl", "verifier"].contains(&by.as_str()) {
        return Err(DgError::Usage(crate::args::UsageError(format!(
            "invalid --by \"{by}\" (allowed: dispatcher|lead|worker|ttl|verifier)"
        ))));
    }
    let allowed_by: &[&str] = if outcome == "rerouted" {
        &CLOSE_BY_REROUTED
    } else {
        &CLOSE_BY_OK_FAIL
    };
    if !allowed_by.contains(&by.as_str()) {
        return Err(DgError::Usage(crate::args::UsageError(format!(
            "--by {by} not allowed for outcome={outcome}"
        ))));
    }
    let reason = optional_flag(args, "reason");
    let db = open_writer(db_flag(args).as_deref())?;
    engine::settle_orphans(&db, now_ms()).map_err(DgError::Fatal)?;
    let fork = engine::get_fork(&db, &fork_id)
        .map_err(DgError::Fatal)?
        .ok_or_else(|| DgError::Usage(crate::args::UsageError(format!("fork not found: {fork_id}"))))?;
    if !engine::is_fork_live_status(&fork.status) {
        return Err(DgError::Usage(crate::args::UsageError(format!(
            "fork {fork_id} already closed (status={})",
            fork.status
        ))));
    }
    let res = engine::close_fork(
        &db,
        &fork_id,
        outcome,
        &by,
        reason.as_deref(),
        &engine::CloseForkOptions::default(),
    )
    .map_err(DgError::Fatal)?;
    let human = format!(
        "[close] {} → {} by={} recorded={}",
        res.fork_id, res.status, res.closed_by, res.recorded
    );
    emit(
        json,
        human,
        &json!({
            "fork_id": res.fork_id,
            "status": res.status,
            "closed_by": res.closed_by,
            "recorded": res.recorded,
            "p_after": res.p_after,
        }),
    );
    Ok(0)
}
