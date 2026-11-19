//! `dg compact` — 로그 컴팩션(dg.ts cmdCompact 대응).
use serde_json::json;

use crate::args::{parse_int_flag, ParsedArgs};
use crate::util::prelude::*;
use dgate_core::engine;

pub fn cmd_compact(args: &ParsedArgs, json: bool) -> CmdResult {
    let max_events = parse_int_flag(args, "max-events", 50_000)?;
    let days = parse_int_flag(args, "days", 30)?;
    let db = open_writer(db_flag(args).as_deref())?;
    let res = engine::compact_db(&db, max_events, days, now_ms()).map_err(DgError::Fatal)?;
    let human = format!(
        "[compact] deleted_events={} deleted_forks={} remaining={}",
        res.deleted_events, res.deleted_forks, res.remaining_events
    );
    emit(
        json,
        human,
        &json!({
            "deleted_events": res.deleted_events,
            "deleted_forks": res.deleted_forks,
            "checkpoint": res.checkpoint,
            "remaining_events": res.remaining_events,
        }),
    );
    Ok(0)
}
