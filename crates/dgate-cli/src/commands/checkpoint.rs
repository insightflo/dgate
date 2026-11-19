//! `dg checkpoint create|verify|restore` — 워크트리 체크포인트(dg.ts cmdCheckpoint 대응).
//! 실패는 exit 1(삭제 게이트 거부 exit 4는 worktree remove 전용).
use std::path::PathBuf;

use serde_json::json;

use crate::args::{optional_flag, require_flag, ParsedArgs};
use crate::util::prelude::*;
use dgate_core::checkpoint::{
    create_worktree_checkpoint, restore_worktree_checkpoint, verify_worktree_checkpoint,
    CreateCheckpointInput,
};

pub fn cmd_checkpoint(sub: Option<&str>, args: &ParsedArgs, json: bool) -> CmdResult {
    match sub {
        Some("create") => {
            let repo = optional_flag(args, "repo").unwrap_or_else(|| ".".to_string());
            let worktree = require_flag(args, "worktree")?;
            let run_id = require_flag(args, "run-id")?;
            let name = optional_flag(args, "name");
            let checkpoint_root = optional_flag(args, "checkpoint-root");
            let res = create_worktree_checkpoint(&CreateCheckpointInput {
                repo_dir: &PathBuf::from(repo),
                worktree_dir: &PathBuf::from(worktree),
                run_id: &run_id,
                worktree_name: name.as_deref(),
                checkpoint_root: checkpoint_root.as_deref().map(PathBuf::from).as_deref(),
            });
            if !res.saved {
                for e in &res.errors {
                    crate::util::eprint_line(&format!("[dg checkpoint] {e}"));
                }
            }
            let human = format!(
                "[checkpoint create] {}",
                if res.saved {
                    format!("saved → {}", res.checkpoint_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default())
                } else {
                    "failed".to_string()
                }
            );
            emit(
                json,
                human,
                &json!({
                    "action": "create",
                    "saved": res.saved,
                    "checkpoint_dir": res.checkpoint_dir,
                    "errors": res.errors,
                }),
            );
            Ok(if res.saved { 0 } else { 1 })
        }
        Some("verify") => {
            let dir = require_flag(args, "dir")?;
            let res = verify_worktree_checkpoint(&PathBuf::from(dir));
            if !res.valid {
                for e in &res.errors {
                    crate::util::eprint_line(&format!("[dg checkpoint] {e}"));
                }
            }
            emit(
                json,
                format!("[checkpoint verify] {}", if res.valid { "valid" } else { "invalid" }),
                &json!({"action": "verify", "valid": res.valid, "errors": res.errors}),
            );
            Ok(if res.valid { 0 } else { 1 })
        }
        Some("restore") => {
            let dir = require_flag(args, "dir")?;
            let target = require_flag(args, "target")?;
            let res = restore_worktree_checkpoint(&PathBuf::from(dir), &PathBuf::from(target));
            if !res.restored {
                for e in &res.errors {
                    crate::util::eprint_line(&format!("[dg checkpoint] {e}"));
                }
            }
            emit(
                json,
                format!("[checkpoint restore] {}", if res.restored { "restored" } else { "failed" }),
                &json!({"action": "restore", "restored": res.restored, "errors": res.errors}),
            );
            Ok(if res.restored { 0 } else { 1 })
        }
        other => Err(DgError::Usage(crate::args::UsageError(format!(
            "unknown checkpoint subcommand: {} (allowed: create|verify|restore)",
            other.unwrap_or("(missing)")
        )))),
    }
}
