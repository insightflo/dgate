//! `dg retro` — fork 회고 마크다운 생성(dg.ts cmdRetro 대응).
use serde_json::{json, Value};

use crate::args::{optional_flag, require_flag, ParsedArgs};
use crate::util::prelude::*;
use dgate_core::engine;
use dgate_core::redact::redact_secrets;
use dgate_core::verdict::{build_retro_markdown, RetroFailedTool, RetroInput};

/// 원인·상태 기반 다음 시도 권고(최대 3줄) — retro 마크다운의 권고 블록 소재.
fn retro_recommendations(cause: Option<&str>, waiting_human: bool, rework_count: i64) -> Vec<String> {
    let mut out = Vec::new();
    if cause == Some("stall") || cause == Some("timeout") {
        out.push("무출력 스톨/타임아웃 — 작업을 더 작은 단위로 분할하고 주기적 진행 로그를 남겨라".to_string());
    }
    if cause == Some("artifact_missing") || cause == Some("no_result") {
        out.push("산출물 계약 미충족 — 요구 산출물 경로·작성 의무·종료 전 출력을 브리프에 명시하라".to_string());
    }
    if cause == Some("gate_fail") {
        out.push("Verifier fail — findings의 evidence를 해소한 뒤 재검증하라".to_string());
    }
    if cause == Some("config_error") {
        out.push("환경/설정 오류 — CLI 경로·플래그·인증 키를 먼저 점검하라(EWMA 표본 미반영)".to_string());
    }
    if waiting_human {
        out.push("사람 승인 대기 — waiting_reason을 확인하고 사람이 개입 결정하라".to_string());
    }
    if rework_count > 0 {
        out.push(format!(
            "재작업 {rework_count}회 누적 — 같은 접근 반복 대신 근본 원인을 재진단하라"
        ));
    }
    out.truncate(3);
    out
}

fn meta_str(meta: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    meta.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn meta_i64(meta: &serde_json::Map<String, Value>, key: &str) -> Option<i64> {
    meta.get(key).and_then(|v| v.as_i64())
}

pub fn cmd_retro(args: &ParsedArgs, json: bool) -> CmdResult {
    let fork_id = require_flag(args, "fork")?;
    let out_path = optional_flag(args, "out");
    let db = open_writer(db_flag(args).as_deref())?;
    let data = engine::collect_fork_retro_data(&db, &fork_id, 5)
        .map_err(DgError::Fatal)?
        .ok_or_else(|| DgError::Usage(crate::args::UsageError(format!("fork not found: {fork_id}"))))?;

    let cause = meta_str(&data.metadata, "cause");
    let rework_count = meta_i64(&data.metadata, "rework_count").unwrap_or(0);
    let reason = meta_str(&data.metadata, "close_reason")
        .or_else(|| meta_str(&data.metadata, "backprop_reason"))
        .or_else(|| meta_str(&data.metadata, "waiting_reason"));
    let attempts = meta_i64(&data.metadata, "attempt").filter(|n| *n > 0).or(Some(data.event_count));

    let md = build_retro_markdown(&RetroInput {
        fork_id: fork_id.clone(),
        project_label: Some(&data.fork.project_label),
        route: &data.fork.route,
        tool_class: &data.fork.tool_class,
        status: &data.fork.status,
        attempts,
        cause: cause.as_deref(),
        rework_count: Some(rework_count),
        reason: reason.as_deref(),
        top_failed_tools: data
            .top_failed_tools
            .iter()
            .map(|t| RetroFailedTool {
                tool_name: t.tool_name.clone(),
                count: t.count,
                last_error: t.last_error.clone(),
            })
            .collect(),
        recommendations: retro_recommendations(
            cause.as_deref(),
            data.metadata.get("waiting_human").and_then(|v| v.as_bool()).unwrap_or(false),
            rework_count,
        ),
        sanitize: Some(redact_secrets),
    });

    match &out_path {
        Some(p) => {
            if let Some(parent) = std::path::Path::new(p).parent() {
                std::fs::create_dir_all(parent).map_err(|e| DgError::Fatal(format!("mkdir {parent:?}: {e}")))?;
            }
            std::fs::write(p, &md)
                .map_err(|e| DgError::Fatal(format!("write {p}: {e}")))?;
        }
        None if !json => {
            // --out 미지정 + 사람 출력 — 마크다운 본문만 stdout
            let mut out = std::io::stdout().lock();
            use std::io::Write;
            let _ = out.write_all(md.as_bytes());
            let _ = out.write_all(b"\n");
            return Ok(0);
        }
        None => {}
    }
    let human = format!("[retro] {fork_id} → {}", out_path.clone().unwrap_or_else(|| "stdout".into()));
    emit(
        json,
        human,
        &json!({
            "fork_id": fork_id,
            "written": out_path.is_some(),
            "out": out_path,
            "markdown": md,
        }),
    );
    Ok(0)
}
