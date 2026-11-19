//! `dg route` — task_kind 기반 태스크 단위 라우팅(dg.ts cmdRoute 대응).
use serde_json::{json, Map, Value};

use crate::args::{optional_flag, require_flag, ParsedArgs};
use crate::util::prelude::*;
use crate::util::{adapter_input, api_suffix, audit_to_value, env_map};
use dgate_core::adapter::{apply_decision_api, create_decision_adapter};
use dgate_core::engine::{self, DecisionBasis, IssueForkInput, RouteEvalInput};

pub fn cmd_route(args: &ParsedArgs, json: bool) -> CmdResult {
    let task_kind_raw = require_flag(args, "task-kind")?;
    let spec = engine::task_kind_spec(&task_kind_raw).ok_or_else(|| {
        DgError::Usage(crate::args::UsageError(format!(
            "invalid --task-kind \"{task_kind_raw}\" (allowed: explore_read|code_search|static_check|single_edit|multi_edit|test_or_deploy|swarm_fanout)"
        )))
    })?;
    let risk_override = parse_risk(optional_flag(args, "risk"))?;
    let title = optional_flag(args, "title");
    let api_flags = parse_decision_api_flags(args)?;
    let theta = resolve_sharp_theta(args)?;
    let identity = resolve_identity(optional_flag(args, "project").as_deref())?;
    let db = open_writer(db_flag(args).as_deref())?;
    let result = (|| -> CmdResult {
        engine::settle_orphans(&db, now_ms()).map_err(DgError::Fatal)?;
        let ev = engine::evaluate_route(
            &db,
            &RouteEvalInput {
                project_id: &identity.project_id,
                task_kind: spec.task_kind.as_str(),
                risk: risk_override,
                conversation_id: None,
                sharp_threshold: theta,
            },
        )
        .map_err(DgError::Fatal)?
        .ok_or_else(|| {
            DgError::Usage(crate::args::UsageError(format!(
                "invalid --task-kind \"{task_kind_raw}\""
            )))
        })?;

        let mut decision = ev.decision;
        let mut final_p = ev.post.p;
        let mut final_risk = ev.risk;
        let mut audit: Option<dgate_core::adapter::DecisionApiAudit> = None;
        if api_flags.mode != dgate_core::adapter::DecisionApiMode::Off {
            let adapter = create_decision_adapter(api_flags.mode, &env_map());
            let input = adapter_input(
                &identity.project_id,
                spec.route,
                spec.tool_class,
                ev.post.basis,
                ev.post.p,
                Some(spec.task_kind.as_str()),
                Some(spec.requires_write),
                Some(ev.risk),
                Some(ev.post.samples),
                Some(ev.post.consecutive_errors),
                title.as_deref(),
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
                sharp_eligible: spec.sharp_eligible,
                local_risk: ev.risk,
                forced_split: ev.forced.forced_split,
                force_api_eval: if force_eval { Some(true) } else { None },
                adapter_input: &input,
            });
            decision = api.decision.as_str();
            final_p = api.p;
            final_risk = api.risk;
            audit = api.audit;
        }

        // metadata: decision_api audit가 있을 때만 병합
        let metadata_value = audit.as_ref().map(|a| {
            let mut m = Map::new();
            m.insert("decision_api".into(), audit_to_value(a));
            m
        });
        let fork_id = engine::issue_fork(
            &db,
            &IssueForkInput {
                project_id: &identity.project_id,
                project_label: &identity.project_label,
                route: spec.route,
                tool_class: spec.tool_class,
                decision,
                p_at_gate: final_p,
                basis_at_gate: ev.post.basis,
                forced_split: ev.forced.forced_split,
                conversation_id: None,
                ttl_ms: Some(engine::TTL_EXPLORE_MS),
                title: title.as_deref(),
                metadata: metadata_value.as_ref(),
                now_ms: None,
            },
        )
        .map_err(DgError::Fatal)?;

        let mut data = json!({
            "fork_id": fork_id,
            "task_kind": spec.task_kind.as_str(),
            "route": spec.route.as_str(),
            "tool_class": spec.tool_class,
            "requires_write": spec.requires_write,
            "risk_level": final_risk.as_str(),
            "p": final_p,
            "basis": basis_str(ev.post.basis),
            "samples": ev.post.samples,
            "consecutive_errors": ev.post.consecutive_errors,
            "session_consec_err": ev.forced.session_consec_err,
            "decision": decision,
            "forced_split": ev.forced.forced_split,
            "forced_reason": ev.forced.forced_reason,
            "escalate_carbon": ev.forced.escalate_carbon,
            "sharp_threshold": ev.sharp_threshold,
            "ttl_ms": engine::TTL_EXPLORE_MS,
            "project_id": identity.project_id,
            "sample_sources": sample_sources_json(&db, &identity.project_id, Some(spec.route), Some(spec.tool_class)),
        });
        if let Some(a) = &audit {
            data["decision_api"] = audit_to_value(a);
        }
        let human = format!(
            "[route] {} → {} ({}/{}) p={:.3} ({}, n={}) fork={}{}",
            spec.task_kind.as_str(),
            decision.to_uppercase(),
            spec.route.as_str(),
            spec.tool_class,
            final_p,
            basis_str(ev.post.basis),
            ev.post.samples,
            fork_id,
            api_suffix(audit.as_ref()),
        );
        emit(json, human, &data);
        Ok(0)
    })();
    result
}

pub fn parse_sharp_split(s: &str) -> dgate_core::adapter::SharpSplit {
    if s == "sharp" {
        dgate_core::adapter::SharpSplit::Sharp
    } else {
        dgate_core::adapter::SharpSplit::Split
    }
}

pub fn basis_str(b: DecisionBasis) -> &'static str {
    b.as_str()
}

pub fn opt_str(v: Option<String>) -> Value {
    json!(v)
}
