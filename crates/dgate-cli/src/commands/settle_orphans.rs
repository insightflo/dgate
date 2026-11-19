//! `dg settle-orphans` — TTL 초과 고아 fork 정산(dg.ts cmdSettleOrphans 대응).
use serde_json::json;

use crate::args::ParsedArgs;
use crate::util::prelude::*;
use dgate_core::engine;

pub fn cmd_settle_orphans(args: &ParsedArgs, json: bool) -> CmdResult {
    let db = open_writer(db_flag(args).as_deref())?;
    let res = engine::settle_orphans(&db, now_ms()).map_err(DgError::Fatal)?;
    let human = format!(
        "[settle-orphans] settled={}{}",
        res.settled,
        if res.fork_ids.is_empty() {
            String::new()
        } else {
            format!(" ({})", res.fork_ids.join(", "))
        }
    );
    emit(
        json,
        human,
        &json!({
            "settled": res.settled,
            "fork_ids": res.fork_ids,
            "ttl_policy_ms": {"explore": engine::TTL_EXPLORE_MS, "coding": engine::TTL_CODING_MS},
        }),
    );
    Ok(0)
}
