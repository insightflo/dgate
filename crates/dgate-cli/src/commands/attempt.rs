//! `dg attempt` — 재시도 경계 펜싱 토큰 갱신(dg.ts cmdAttempt 대응).
use serde_json::json;

use crate::args::{require_flag, ParsedArgs};
use crate::util::prelude::*;
use dgate_core::engine;

pub fn cmd_attempt(args: &ParsedArgs, json: bool) -> CmdResult {
    let fork_id = require_flag(args, "fork")?;
    let bump_gen = crate::args::bool_flag(args, "bump-gen");
    let db = open_writer(db_flag(args).as_deref())?;
    engine::settle_orphans(&db, now_ms()).map_err(DgError::Fatal)?;
    let fork = engine::get_fork(&db, &fork_id)
        .map_err(DgError::Fatal)?
        .ok_or_else(|| DgError::Usage(crate::args::UsageError(format!("fork not found: {fork_id}"))))?;
    if !engine::is_fork_live_status(&fork.status) {
        return Err(DgError::Usage(crate::args::UsageError(format!(
            "fork {fork_id} not open (status={})",
            fork.status
        ))));
    }
    let res = engine::bump_fork_attempt(&db, &fork_id, bump_gen, None).map_err(DgError::Fatal)?;
    let human = format!(
        "[attempt] {} → gen={} attempt={}{}",
        res.0,
        res.1,
        res.2,
        if bump_gen { " (gen bumped)" } else { "" }
    );
    emit(
        json,
        human,
        &json!({"fork_id": res.0, "generation": res.1, "attempt": res.2}),
    );
    Ok(0)
}
