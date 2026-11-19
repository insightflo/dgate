//! `dg record` — 표본 1건 기록(dg.ts cmdRecord 대응).
use serde_json::{json, Map, Value};

use crate::args::{optional_flag, parse_float_flag, parse_int_flag, require_flag, ParsedArgs};
use crate::util::prelude::*;
use dgate_core::engine::{self, EventSource, RecordInput};

pub fn parse_source(v: &str) -> Result<EventSource, DgError> {
    EventSource::parse(v).ok_or_else(|| {
        DgError::Usage(crate::args::UsageError(format!(
            "invalid --source \"{v}\" (allowed: hook|agy_stream|transcript|rest_api|cli|wrap|ttl_orphan|neon_backprop)"
        )))
    })
}

pub fn cmd_record(args: &ParsedArgs, json: bool) -> CmdResult {
    let route = parse_route(&require_flag(args, "route")?, "route")?;
    let tool_class = require_flag(args, "tool-class")?;
    let tool_name = require_flag(args, "tool")?;
    let ok_raw = require_flag(args, "ok")?;
    if ok_raw != "0" && ok_raw != "1" {
        return Err(DgError::Usage(crate::args::UsageError(format!(
            "invalid --ok \"{ok_raw}\" (allowed: 0|1)"
        ))));
    }
    let ok = ok_raw == "1";
    let weight = parse_float_flag(args, "weight", 1.0)?;
    let is_retry = crate::args::bool_flag(args, "is-retry");
    let conversation = optional_flag(args, "conversation");
    let source = parse_source(&optional_flag(args, "source").unwrap_or_else(|| "cli".into()))?;
    let target_hash = optional_flag(args, "target-hash");
    let error_snippet = optional_flag(args, "error");
    let step = if optional_flag(args, "step").is_some() {
        Some(parse_int_flag(args, "step", 0)?)
    } else {
        None
    };
    let command_line = optional_flag(args, "command");
    let dedupe_key = optional_flag(args, "dedupe");
    let fork_id = optional_flag(args, "fork");
    let identity = resolve_identity(optional_flag(args, "project").as_deref())?;
    let db = open_writer(db_flag(args).as_deref())?;
    engine::settle_orphans(&db, now_ms()).map_err(DgError::Fatal)?;
    let res = engine::record_tool_outcome(
        &db,
        &RecordInput {
            project_id: &identity.project_id,
            project_label: &identity.project_label,
            route,
            tool_class: &tool_class,
            tool_name: &tool_name,
            ok,
            weight: Some(weight),
            is_retry: Some(is_retry),
            conversation_id: conversation.as_deref(),
            step,
            target_hash: target_hash.as_deref(),
            error_snippet: error_snippet.as_deref(),
            source: Some(source),
            dedupe_key: dedupe_key.as_deref(),
            fork_id: fork_id.as_deref(),
            command_line: command_line.as_deref(),
            ..Default::default()
        },
    )
    .map_err(DgError::Fatal)?;

    let mut data = Map::new();
    data.insert("recorded".into(), json!(res.recorded));
    data.insert("reason".into(), json!(res.reason));
    data.insert("is_retry".into(), json!(res.is_retry));
    data.insert("route".into(), json!(res.route.as_str()));
    data.insert("p_after".into(), json!(res.p_after));
    data.insert("consecutive_errors".into(), json!(res.consecutive_errors));
    data.insert("session_consec_err".into(), json!(res.session_consec_err));
    if let Some(k) = &dedupe_key {
        data.insert("dedupe_key".into(), json!(k));
    }
    let human = format!(
        "[record] {}/{} ok={} → {}{} p_after={:.3}",
        route.as_str(),
        tool_class,
        if ok { 1 } else { 0 },
        if res.recorded {
            "recorded".to_string()
        } else {
            format!("skipped ({})", res.reason.unwrap_or(""))
        },
        if res.is_retry { " [retry]" } else { "" },
        res.p_after,
    );
    emit(json, human, &Value::Object(data));
    Ok(0)
}
