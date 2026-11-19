//! dgate-cli — 결정 게이트 네이티브 CLI(v2 Rust).
//!
//! `src/decision/dg.ts`(TS v1)의 15명령 + `hook`·`setup`·`dispatch`를 완전 패리티로
//! 제공한다. 같은 플래그·JSON 스키마 키·종료 코드·안전 폴백을 유지하며,
//! `crates/dgate-core`의 모든 판정 로직을 그대로 호출한다(판정 중복 구현 없음).
//!
//! 외부 연결:
//! - `src/bin/dg.rs`·`src/bin/dgate.rs`가 이 lib의 `run()`을 호출한다(듀얼 bin 경고 해소).
//! - Node v1(`src/decision/dg.ts`)과 동일 SQLite SSOT를 공유한다(양방향 호환).
//!
//! 주의:
//! - 안전 폴백(§3.5): DB 오류 시 stderr 경고 + 안전 JSON + exit 0(게이트 SPOF 차단).
//!   단, 잘못된 enum·플래그 등 입력 검증 오류는 exit 2, verdict fail-closed는 3,
//!   worktree remove 게이트 거부는 4다.
//! - 모든 쓰기/조회 진입점은 트랜잭션 내 `settle_orphans`를 먼저 실행한다(§4.6).
//!   `stats`·`report`는 읽기전용 연결만 쓰며 절대 호출하지 않는다.

pub mod args;
pub mod commands;
pub mod util;

use args::ParsedArgs;
use util::{eprint_line, CmdResult, DgError};

/// 인식 가능한 명령 15종 + v2 신규 3종.
pub const COMMANDS: [&str; 18] = [
    "route", "gate", "close", "touch", "record", "backprop", "attempt", "retro",
    "settle-orphans", "wrap", "compact", "stats", "checkpoint", "worktree", "report",
    "hook", "setup", "dispatch",
];

const HELP: &str = "dg — decision gate CLI (plan-r2.1 §4.7, v2 native Rust)

사용법:
  dg route --task-kind <explore_read|code_search|static_check|single_edit|multi_edit|test_or_deploy|swarm_fanout> [--project <dir>] [--title <text>] [--context <text>] [--decision-api <off|shadow|on>] [--api-eval] [--json]
  dg gate --route <r> --tool-class <c> [--fork <id>] [--risk <low|medium|high>] [--conversation <id>] [--project <dir>] [--context <text>] [--decision-api <off|shadow|on>] [--api-eval] [--json]
  dg close --fork <id> --outcome <ok|fail|rerouted> [--ok 0|1] [--by dispatcher|lead|worker|ttl|verifier] [--reason <text>] [--json]
  dg touch --fork <id> [--ttl-ms <n>] [--json]
  dg record --route <r> --tool-class <c> --tool <name> --ok <0|1> [--weight 1.0] [--is-retry] [--conversation <id>] [--source <hook|agy_stream|transcript|rest_api|cli|wrap|ttl_orphan|neon_backprop>] [--target-hash <h>] [--error <snippet>] [--step <n>] [--command <text>] [--dedupe <key>] [--fork <id>] [--project <dir>] [--json]
  dg backprop --fork <id> [--penalty 2.0] [--reason <text>] [--json]
  dg backprop --fork <id> --verdict-file <p> [--expected-head <sha>] [--max-reworks 2] [--json]
  dg attempt --fork <id> [--bump-gen] [--json]
  dg retro --fork <id> [--out <path>] [--json]
  dg settle-orphans [--json]
  dg wrap --route <r> --tool-class <c> [--fork <id>] [--source wrap] [--require-artifact <path>] [--require-nonempty] [--require-stdout-nonempty] [--stall-ms 120000] [--max-ms 1200000] [--heartbeat-ms 60000] [--kill-grace-ms 1000] [--expected-gen <n>] [--expected-attempt <n>] [--sanitize-env] [--allow-env K1,K2] [--json] -- <cmd…>
  dg compact [--max-events 50000] [--days 30] [--json]
  dg stats [--cost] [--timing] [--project <dir>] [--fork <id>] [--days 14] [--db <path>] [--json]
  dg checkpoint create --worktree <path> --run-id <id> [--repo <dir>] [--name <wtName>] [--checkpoint-root <dir>] [--json]
  dg checkpoint verify --dir <checkpointDir> [--json]
  dg checkpoint restore --dir <checkpointDir> --target <worktreeDir> [--json]
  dg worktree remove --worktree <path> [--repo <dir>] [--run-id <id>] [--name <wtName>] [--checkpoint-root <dir>] [--force] [--json]
  dg report [--days 14] [--project <dir>] [--out <path>] [--repo <dir>] [--db <path>] [--json]
  dg hook --guard | --collect        (stdin JSON → guard deny/allow | collect {})
  dg setup [--check] [--json] [--repo <dir>]
  dg dispatch --task-kind <k> [--risk low|medium|high] [--repo <dir>] [--roster <path>] [--db <path>] [--run-id <id>] [--task-file <path>] [--idx <n>] [--active-explore <n>] [--active-coding <n>] [--dry-run] [--context <text>] [--decision-api <off|shadow|on>] [--api-eval] [--json]

라우트 r: file_select|tool_select|retry_or_abort|delegate_swarm
공용: --db <path>는 DGATE_DB_PATH·DECISION_ORCHESTRATOR_DB_PATH·GEMINI_TRICK_DB_PATH보다 우선. --roster <path>/--profile <name> 명시 시
route·gate에 roster 기반 동적 θ_sharp 적용 (미지정 시 0.70).
Decision API: DECISION_API=off|shadow|on + GEMINI_DECISION_API_KEY (키 부재 시 off 강등).
--decision-api는 env를 덮어쓰고, --context/--api-eval은 measured 기반에서도 평가를 강제한다.
DB 경로: DGATE_DB_PATH 환경변수(레거시 별칭 DECISION_ORCHESTRATOR_DB_PATH·GEMINI_TRICK_DB_PATH) 또는 기본 ~/.local/state/dgate/decision.db";

/// CLI 진입점 — 인자 파싱·명령 라우팅·폴백 처리 후 exit code를 반환한다.
pub fn run<I, S>(argv: I) -> i32
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
    if argv.is_empty() || argv[0] == "--help" || argv[0] == "-h" {
        println!("{HELP}");
        return if argv.is_empty() { 2 } else { 0 };
    }
    let cmd = argv[0].clone();
    if !COMMANDS.contains(&cmd.as_str()) {
        eprint_line(&format!("unknown command: {cmd}\n\n{HELP}"));
        return 2;
    }
    // checkpoint/worktree는 2단 서브커맨드 — 동사를 먼저 떼고 나머지를 플래그로 파싱
    let (sub, flag_args): (Option<String>, Vec<String>) = if cmd == "checkpoint" || cmd == "worktree"
    {
        match argv.get(1) {
            Some(s) => (Some(s.clone()), argv[2..].to_vec()),
            None => (None, argv[1..].to_vec()),
        }
    } else {
        (None, argv[1..].to_vec())
    };

    let parsed: ParsedArgs = match args::parse_args(&flag_args) {
        Ok(p) => p,
        Err(e) => {
            eprint_line(&format!("[dg] {e}"));
            return 2;
        }
    };
    let json = args::bool_flag(&parsed, "json");

    let result: CmdResult = match cmd.as_str() {
        "route" => commands::route::cmd_route(&parsed, json),
        "gate" => commands::gate::cmd_gate(&parsed, json),
        "close" => commands::close::cmd_close(&parsed, json),
        "touch" => commands::touch::cmd_touch(&parsed, json),
        "record" => commands::record::cmd_record(&parsed, json),
        "backprop" => commands::backprop::cmd_backprop(&parsed, json),
        "attempt" => commands::attempt::cmd_attempt(&parsed, json),
        "retro" => commands::retro::cmd_retro(&parsed, json),
        "settle-orphans" => commands::settle_orphans::cmd_settle_orphans(&parsed, json),
        "wrap" => commands::wrap::cmd_wrap(&parsed, json),
        "compact" => commands::compact::cmd_compact(&parsed, json),
        "stats" => commands::stats::cmd_stats(&parsed, json),
        "checkpoint" => commands::checkpoint::cmd_checkpoint(sub.as_deref(), &parsed, json),
        "worktree" => commands::worktree::cmd_worktree(sub.as_deref(), &parsed, json),
        "report" => commands::report::cmd_report(&parsed, json),
        "hook" => commands::hook::cmd_hook(&parsed, json),
        "setup" => commands::setup::cmd_setup(&parsed, json),
        "dispatch" => commands::dispatch::cmd_dispatch(&parsed, json),
        _ => unreachable!("command list checked above"),
    };

    match result {
        Ok(code) => code,
        Err(DgError::Usage(e)) => {
            eprint_line(&format!("[dg] {e}"));
            2
        }
        Err(DgError::Fatal(msg)) => {
            if cmd == "wrap" {
                // wrap은 자체 DB 폴백을 이미 수행한다 — 남은 Fatal은 진짜 내부 오류
                eprint_line(&format!("[dg wrap] fatal: {msg}"));
                return 1;
            }
            // 안전 폴백(§3.5): stderr 경고 + 안전 JSON + exit 0
            eprint_line(&format!("[dg] db fallback: {msg}"));
            let fb = util::fallback_json(&cmd, &msg, &parsed);
            println!("{fb}");
            0
        }
    }
}
