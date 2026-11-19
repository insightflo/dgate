//! `dg report` — 읽기전용 규칙 후보 리포트(dg.ts cmdReport 대응).
//!
//! PB-2.5: `open_report_db`(readOnly + query_only)만 쓰고 `settle_orphans`
//! 금지. `--out`은 allowlist 통과 경로에만 기록 — 거부 시 파일·부모 디렉터리
//! 모두 미생성 + stderr + exit 2.
use serde_json::json;

use crate::args::{optional_flag, parse_int_flag, ParsedArgs};
use crate::util::prelude::*;
use dgate_core::report::{
    format_report_markdown, generate_rule_candidate_report, open_report_db,
    write_report_file_safe, ReportOptions,
};

pub fn cmd_report(args: &ParsedArgs, json: bool) -> CmdResult {
    let days = parse_int_flag(args, "days", 14)?;
    let project_dir = optional_flag(args, "project");
    let project_id = project_dir
        .as_deref()
        .map(|d| resolve_identity(Some(d)).map(|i| i.project_id))
        .transpose()?;
    let repo_dir = optional_flag(args, "repo").unwrap_or_else(|| ".".to_string());
    let out_path = optional_flag(args, "out");
    let db = open_report_db(db_flag(args).as_deref()).map_err(DgError::Fatal)?;

    let rep = generate_rule_candidate_report(
        &db,
        &ReportOptions { project_id: project_id.clone(), days: Some(days), now_ms: None },
    );
    let rep_value = serde_json::to_value(&rep).unwrap_or(serde_json::Value::Null);
    let md = format_report_markdown(&rep);

    if let Some(op) = &out_path {
        let w = write_report_file_safe(op, std::path::Path::new(&repo_dir), &md);
        if !w.written {
            crate::util::eprint_line(&format!(
                "[dg report] --out refused: {op} ({})",
                w.reason.as_deref().unwrap_or("forbidden_out_path")
            ));
            return Ok(2);
        }
        let human = format!("[report] written → {}", w.resolved_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default());
        emit(
            json,
            human,
            &json!({"cmd": "report", "written": true, "out": w.resolved_path, "report": rep_value}),
        );
        return Ok(0);
    }
    emit(
        json,
        md.clone(),
        &json!({"cmd": "report", "written": false, "out": serde_json::Value::Null, "report": rep_value, "markdown": md}),
    );
    Ok(0)
}
