//! `dg gate` — route/tool_class 게이트 판정 + 기존 fork 재사용(dg.ts cmdGate 대응).
use serde_json::{json, Map, Value};

use crate::args::{optional_flag, require_flag, ParsedArgs};
use crate::util::prelude::*;
use crate::util::{adapter_input, api_suffix, audit_to_value, env_map};
use crate::commands::route::parse_sharp_split;
use dgate_core::adapter::{apply_decision_api, create_decision_adapter};
use dgate_core::engine::{self, GateEvalInput, IssueForkInput, RiskLevel};

/// TS `mergeForkDecisionApi` — 기존 fork의 metadata_json에 decision_api audit 병합.
fn merge_fork_decision_api(
    db: &rusqlite::Connection,
    fork_id: &str,
    audit: &dgate_core::adapter::DecisionApiAudit,
) -> Result<(), String> {
    let row: Option<(Option<String>,)> = db
        .query_row(
            "SELECT metadata_json FROM forks WHERE fork_id = ?1",
            [fork_id],
            |r| Ok((r.get(0)?,)),
        )
        .map(Some)
        .or_else(|e| {
            if let rusqlite::Error::QueryReturnedNoRows = e {
                Ok(None)
            } else {
                Err(e.to_string())
            }
        })?;
    let Some((metadata_json,)) = row else { return Ok(()) };
    let mut meta: Map<String, Value> = match metadata_json
        .as_deref()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
    {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    meta.insert("decision_api".into(), audit_to_value(audit));
    db.execute_batch("BEGIN IMMEDIATE").map_err(|e| e.to_string())?;
    let work = db
        .prepare("UPDATE forks SET metadata_json = ?1 WHERE fork_id = ?2")
        .and_then(|mut stmt| {
            stmt.execute(rusqlite::params![Value::Object(meta).to_string(), fork_id])
        });
    match work {
        Ok(_) => db.execute_batch("COMMIT").map_err(|e| e.to_string()),
        Err(e) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(e.to_string())
        }
    }
}

pub fn cmd_gate(args: &ParsedArgs, json: bool) -> CmdResult {
    let route = parse_route(&require_flag(args, "route")?, "route")?;
    let tool_class = require_flag(args, "tool-class")?;
    let risk_override = parse_risk(optional_flag(args, "risk"))?;
    let conversation = optional_flag(args, "conversation");
    let fork_opt = optional_flag(args, "fork");
    let api_flags = parse_decision_api_flags(args)?;
    let theta = resolve_sharp_theta(args)?;
    let identity = resolve_identity(optional_flag(args, "project").as_deref())?;
    let db = open_writer(db_flag(args).as_deref())?;
    (|| -> CmdResult {
        engine::settle_orphans(&db, now_ms()).map_err(DgError::Fatal)?;
        let ev = engine::evaluate_gate(
            &db,
            &GateEvalInput {
                project_id: &identity.project_id,
                route,
                tool_class: &tool_class,
                conversation_id: conversation.as_deref(),
                sharp_threshold: theta,
            },
        )
        .map_err(DgError::Fatal)?;

        let mut decision = ev.decision;
        let mut final_p = ev.post.p;
        // gate 경로 로컬 위험도는 --risk 오버라이드뿐 — API는 상향만 가능
        let local_risk: RiskLevel = risk_override.unwrap_or(RiskLevel::Low);
        let mut audit: Option<dgate_core::adapter::DecisionApiAudit> = None;
        if api_flags.mode != dgate_core::adapter::DecisionApiMode::Off {
            let adapter = create_decision_adapter(api_flags.mode, &env_map());
            let input = adapter_input(
                &identity.project_id,
                route,
                &tool_class,
                ev.post.basis,
                ev.post.p,
                None,
                None,
                Some(local_risk),
                Some(ev.post.samples),
                Some(ev.post.consecutive_errors),
                None,
                api_flags.context.as_deref(),
            );
            let force_eval = api_flags.api_eval || api_flags.context.is_some();
            let api = apply_decision_api(&dgate_core::adapter::ApplyDecisionApiInput {
                mode: api_flags.mode,
                adapter: adapter.as_ref(),
                local_decision: parse_sharp_split(ev.decision),
                p_local: ev.post.p,
                sharp_threshold: ev.sharp_threshold,
                basis: ev.post.basis,
                sharp_eligible: true,
                local_risk,
                forced_split: ev.forced.forced_split,
                force_api_eval: if force_eval { Some(true) } else { None },
                adapter_input: &input,
            });
            decision = api.decision.as_str();
            final_p = api.p;
            audit = api.audit;
        }

        let fork_id = if let Some(fk) = &fork_opt {
            let fork = engine::get_fork(&db, fk)
                .map_err(DgError::Fatal)?
                .ok_or_else(|| DgError::Usage(crate::args::UsageError(format!("fork not found: {fk}"))))?;
            if !engine::is_fork_live_status(&fork.status) {
                return Err(DgError::Usage(crate::args::UsageError(format!(
                    "fork {fk} not open (status={})",
                    fork.status
                ))));
            }
            engine::ensure_fork_wait_interval(&db, fk, None).map_err(DgError::Fatal)?;
            if let Some(a) = &audit {
                merge_fork_decision_api(&db, fk, a).map_err(DgError::Fatal)?;
            }
            fk.clone()
        } else {
            let metadata_value = audit.as_ref().map(|a| {
                let mut m = Map::new();
                m.insert("decision_api".into(), audit_to_value(a));
                m
            });
            engine::issue_fork(
                &db,
                &IssueForkInput {
                    project_id: &identity.project_id,
                    project_label: &identity.project_label,
                    route,
                    tool_class: &tool_class,
                    decision,
                    p_at_gate: final_p,
                    basis_at_gate: ev.post.basis,
                    forced_split: ev.forced.forced_split,
                    conversation_id: conversation.as_deref(),
                    ttl_ms: Some(engine::TTL_CODING_MS),
                    title: None,
                    metadata: metadata_value.as_ref(),
                    now_ms: None,
                },
            )
            .map_err(DgError::Fatal)?
        };

        let mut data = json!({
            "fork_id": fork_id,
            "route": route.as_str(),
            "tool_class": tool_class,
            "p": final_p,
            "basis": ev.post.basis.as_str(),
            "samples": ev.post.samples,
            "consecutive_errors": ev.forced.key_consecutive_errors,
            "session_consec_err": ev.forced.session_consec_err,
            "decision": decision,
            "forced_split": ev.forced.forced_split,
            "forced_reason": ev.forced.forced_reason,
            "escalate_carbon": ev.forced.escalate_carbon,
            "sharp_threshold": ev.sharp_threshold,
            "ttl_ms": engine::TTL_CODING_MS,
            "project_id": identity.project_id,
            "sample_sources": sample_sources_json(&db, &identity.project_id, Some(route), Some(&tool_class)),
        });
        if let Some(a) = &audit {
            data["decision_api"] = audit_to_value(a);
        }
        let human = format!(
            "[gate] {}/{} → {}{} p={:.3} ({}, n={}) fork={}{}",
            route.as_str(),
            tool_class,
            decision.to_uppercase(),
            if ev.forced.forced_split { " (forced)" } else { "" },
            final_p,
            ev.post.basis.as_str(),
            ev.post.samples,
            fork_id,
            api_suffix(audit.as_ref()),
        );
        emit(json, human, &data);
        Ok(0)
    })()
}