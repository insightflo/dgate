//! `dg touch` — fork TTL 연장(dg.ts cmdTouch 대응).
use serde_json::json;

use crate::args::{optional_flag, parse_int_flag, require_flag, ParsedArgs};
use crate::util::prelude::*;
use dgate_core::engine;

pub fn cmd_touch(args: &ParsedArgs, json: bool) -> CmdResult {
    let fork_id = require_flag(args, "fork")?;
    let ttl_ms = if optional_flag(args, "ttl-ms").is_some() {
        Some(parse_int_flag(args, "ttl-ms", 600_000)?)
    } else {
        None
    };
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
    let res = engine::touch_fork(&db, &fork_id, ttl_ms, now_ms()).map_err(DgError::Fatal)?;
    let human = format!("[touch] {} last_touch={} ttl={}", res.0, res.1, res.2);
    emit(
        json,
        human,
        &json!({"fork_id": res.0, "last_touch_ms": res.1, "ttl_ms": res.2}),
    );
    Ok(0)
}
