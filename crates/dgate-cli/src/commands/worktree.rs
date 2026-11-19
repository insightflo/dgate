//! `dg worktree remove` — 체크포인트 게이트 경유 안전 삭제(dg.ts cmdWorktree 대응).
//!
//! `safe_remove_worktree` 단일 진입만 경유(우회 경로 없음). 종료코드는 결과의
//! exit_code 그대로 — 체크포인트 미통과 시 4(하드 게이트, `--force`로도 우회 불가).
use std::path::PathBuf;

use serde_json::json;

use crate::args::{optional_flag, require_flag, ParsedArgs};
use crate::util::prelude::*;
use dgate_core::checkpoint::{safe_remove_worktree, SafeRemoveInput};

pub fn cmd_worktree(sub: Option<&str>, args: &ParsedArgs, json: bool) -> CmdResult {
    if sub != Some("remove") {
        return Err(DgError::Usage(crate::args::UsageError(format!(
            "unknown worktree subcommand: {} (allowed: remove)",
            sub.unwrap_or("(missing)")
        ))));
    }
    let repo = optional_flag(args, "repo").unwrap_or_else(|| ".".to_string());
    let worktree = require_flag(args, "worktree")?;
    let run_id = optional_flag(args, "run-id")
        .unwrap_or_else(|| format!("wt-rm-{}", now_ms()));
    let name = optional_flag(args, "name");
    let checkpoint_root = optional_flag(args, "checkpoint-root");
    let force = crate::args::bool_flag(args, "force");
    let res = safe_remove_worktree(&SafeRemoveInput {
        repo_dir: &PathBuf::from(repo),
        worktree_dir: &PathBuf::from(worktree),
        run_id: &run_id,
        worktree_name: name.as_deref(),
        checkpoint_root: checkpoint_root.as_deref().map(PathBuf::from).as_deref(),
        force_git_remove: force,
    });
    if !res.removed {
        for e in &res.errors {
            crate::util::eprint_line(&format!("[dg worktree] {e}"));
        }
    }
    let human = format!(
        "[worktree remove] {}",
        if res.removed {
            format!("removed (checkpoint → {})", res.checkpoint_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default())
        } else {
            format!("refused/failed (exit {})", res.exit_code)
        }
    );
    emit(
        json,
        human,
        &json!({
            "action": "remove",
            "removed": res.removed,
            "exit_code": res.exit_code,
            "checkpoint_dir": res.checkpoint_dir,
            "errors": res.errors,
        }),
    );
    Ok(res.exit_code)
}
