//! `dg hook --guard | --collect` — 네이티브 훅 가드·수집기(dg-hook.ts 대응).
//!
//! stdin JSON을 통째로 읽어 `dgate_core::hook::run_hook_mode`에 전달한다.
//! guard는 `HERDR_READONLY=1`에서 fail-closed, collect는 항상 fail-open `{}`.
use std::io::Read;

use crate::args::{optional_flag, ParsedArgs};
use crate::util::prelude::*;
use dgate_core::hook::{run_hook_mode, HookEnv};

pub fn cmd_hook(args: &ParsedArgs, _json: bool) -> CmdResult {
    let mode = match optional_flag(args, "guard").is_some() {
        true => "--guard",
        false => {
            if optional_flag(args, "collect").is_some() {
                "--collect"
            } else {
                // 미지정 모드 — 가드 경로가 아니므로 fail-open `{}`
                ""
            }
        }
    };
    let mut stdin_json = String::new();
    let _ = std::io::stdin().lock().read_to_string(&mut stdin_json);
    let out = run_hook_mode(mode, &stdin_json, &HookEnv::from_process());
    let mut so = std::io::stdout().lock();
    use std::io::Write;
    let _ = writeln!(so, "{out}");
    Ok(0)
}
