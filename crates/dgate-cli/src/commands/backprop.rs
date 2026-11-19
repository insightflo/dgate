//! `dg backprop` — 수동 역귀속 + fail-closed verdict 파이프라인(dg.ts cmdBackprop 대응).
//!
//! - `--verdict-file` 부재: 기존 수동 역귀속(exit 0, UsageError=2).
//! - `--verdict-file <p>` 존재: validate → apply → 결과 exitCode(0 또는 3) 반환.
//!   verdict 경로의 내부 오류는 §3.5 DB 폴백으로 흡수하지 않고 exit 3을 유지한다.
use serde_json::{json, Value};

use crate::args::{optional_flag, parse_float_flag, parse_int_flag, require_flag, ParsedArgs};
use crate::util::prelude::*;
use dgate_core::engine::{self, BackpropOpts, FailureCause};
use dgate_core::verdict::{self, ValidateVerdictOptions};

pub fn cmd_backprop(args: &ParsedArgs, json: bool) -> CmdResult {
    let fork_id = require_flag(args, "fork")?;
    let verdict_file = optional_flag(args, "verdict-file");
    let penalty = parse_float_flag(args, "penalty", 2.0)?;
    let reason = optional_flag(args, "reason");
    let db = open_writer(db_flag(args).as_deref())?;
    engine::settle_orphans(&db, now_ms()).map_err(DgError::Fatal)?;
    let fork = engine::get_fork(&db, &fork_id)
        .map_err(DgError::Fatal)?
        .ok_or_else(|| DgError::Usage(crate::args::UsageError(format!("fork not found: {fork_id}"))))?;

    if let Some(vf) = &verdict_file {
        let expected_head = optional_flag(args, "expected-head");
        let max_reworks = parse_int_flag(args, "max-reworks", 2)?;
        // validate → apply — 유효성·판정·오류는 검증 결과(v)에서, 상태 반영은 apply에서.
        let v = verdict::validate_verdict_file(
            std::path::Path::new(vf),
            &ValidateVerdictOptions {
                expected_fork_id: fork_id.clone(),
                expected_head,
            },
        );
        match engine::apply_verdict_to_fork(&db, &fork_id, &v, Some(max_reworks), None) {
            Ok(res) => {
                let human = format!(
                    "[backprop] {fork_id} verdict={} → {} (exit {})",
                    v.effective_decision.as_str(),
                    res.action,
                    res.exit_code
                );
                emit(
                    json,
                    human,
                    &json!({
                        "fork_id": fork_id,
                        "valid": v.valid,
                        "effective_decision": v.effective_decision.as_str(),
                        "errors": v.errors,
                        "action": res.action,
                        "exit_code": res.exit_code,
                        "committed": res.committed,
                        "reason": res.reason,
                        "waiting_human": res.waiting_human.unwrap_or(false),
                        "p_after": res.p_after,
                    }),
                );
                Ok(res.exit_code)
            }
            Err(message) => {
                // fail-closed: verdict 파이프라인 내부 오류 — DB 폴백으로 흡수 금지
                crate::util::eprint_line(&format!(
                    "[dg backprop] verdict pipeline error: {message}"
                ));
                let waiting_human = engine::mark_fork_waiting_human(
                    &db,
                    &fork_id,
                    &format!("verdict pipeline error: {message}"),
                    None,
                    None,
                )
                .map(|r| r.0)
                .unwrap_or(false);
                let human = format!(
                    "[backprop] {fork_id} verdict 처리 오류 → escalate_human (exit 3): {message}"
                );
                emit(
                    json,
                    human,
                    &json!({
                        "fork_id": fork_id,
                        "valid": false,
                        "effective_decision": "human",
                        "errors": [message],
                        "action": "escalate_human",
                        "exit_code": 3,
                        "committed": waiting_human,
                        "reason": message,
                        "waiting_human": waiting_human,
                        "p_after": Value::Null,
                    }),
                );
                Ok(3)
            }
        }
    } else {
        if !engine::is_fork_live_status(&fork.status) && fork.status != "ok" {
            return Err(DgError::Usage(crate::args::UsageError(format!(
                "fork {fork_id} cannot backprop (status={})",
                fork.status
            ))));
        }
        let res = engine::backprop_fork(
            &db,
            &fork_id,
            penalty,
            reason.as_deref(),
            BackpropOpts { cause: Some(FailureCause::GateFail), now_ms: None },
        )
        .map_err(DgError::Fatal)?;
        if !res.committed {
            return Err(DgError::Usage(crate::args::UsageError(format!(
                "fork {fork_id} cannot backprop ({})",
                res.reason.unwrap_or("stale")
            ))));
        }
        let human = format!(
            "[backprop] {} → backprop_fail penalty={:.3} p_after={:.3}",
            res.fork_id, res.weight_applied, res.p_after
        );
        emit(
            json,
            human,
            &json!({
                "fork_id": res.fork_id,
                "status": res.status,
                "neon_verdict": res.neon_verdict,
                "weight_applied": res.weight_applied,
                "p_after": res.p_after,
            }),
        );
        Ok(0)
    }
}
