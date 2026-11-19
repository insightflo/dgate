//! 네이티브 훅 가드·수집기 — `.agents/hooks/dg-hook.ts` (plan-r2.1 §6.2, B4, N-NEW-1
//! 교정판)의 Rust 포팅.
//!
//! 목적: `--guard`(PreToolUse)가 위임 채널(`HERDR_DELEGATE_MODE=hc`)·읽기전용
//! (`HERDR_READONLY=1`) 위반을 차단하고, `--collect`(PostToolUse)가 도구 결과를
//! SQLite WAL SSOT에 기록한다.
//!
//! 외부 연결:
//! - stdin: `{"toolCall":{"id","name","args"},"stepIdx","conversationId","cwd",
//!   "transcriptPath"[,"error"]}`.
//! - stdout: guard → `{"decision":"allow"|"deny","reason":...}` / collect → `{}`.
//! - collect은 Wave 1A `db`·`engine` 모듈의 `open_db_safe`·`record_tool_outcome`·
//!   `resolve_project_identity`·`classify_tool`·`is_self_invocation`과 `redact`의
//!   `redact_secrets`를 재사용한다(단일 구현 원칙).
//!
//! 주의:
//! - guard는 `HERDR_READONLY=1`일 때 **fail-closed**: stdin JSON 파싱 실패·`toolCall`
//!   누락·내부 예외 시 `{"decision":"deny","reason":"readonly guard: fail-closed"}`.
//! - readonly 가드(B1·N-b): 명령 문자열의 개행·CR·널바이트 다중명령, 허용 명령의
//!   쓰기·외부실행 플래그(`--output`·`--pre`·`--import` 등), 셸 실행 도구 별칭
//!   (`bash`·`shell`·`exec`·`powershell`)와 소문자 `write`·`edit`까지 전부 차단한다.
//! - collect은 **fail-open**: 어떤 파싱 실패·DB 오류·잠금(`busy_timeout=250ms`)이어도
//!   `{}` 반환 후 정상 종료 — 에이전트 도구 루프를 절대 정지시키지 않는다(§3.5).
//! - DB 기록은 `HERDR_DB_WRITE == "1"` AND `DECISION_READONLY != "1"`일 때만 수행.
//! - env 접근은 [`HookEnv`] 주입형으로 처리한다 — 테스트가 실제 프로세스 env를
//!   건드리지 않고 판정을 재현한다.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::db::{open_db_safe, resolve_project_identity, OpenDbOptions};
use crate::engine::{classify_tool, is_self_invocation, record_tool_outcome, EventSource, RecordInput};
use crate::redact::redact_secrets;

const DELEGATE_DENY_REASON: &str = "Delegation must use hc CLI in herdr-quad mode";
const FAIL_CLOSED_REASON: &str = "readonly guard: fail-closed";
const DENY_MULTILINE_REASON: &str = "readonly guard: multiline/control char denied";
const DENY_NOT_WHITELISTED_REASON: &str = "readonly guard: command not whitelisted";

/// hc 단일 위임 채널 강제 — `HERDR_DELEGATE_MODE=hc` 세션에서 무조건 deny
const DELEGATE_DENY_TOOLS: [&str; 3] = ["invoke_subagent", "manage_subagents", "swarm"];

/// `HERDR_READONLY=1` 무조건 deny — 실측 존재 도구 (§6.2/B4)
const READONLY_DENY_TOOLS: [&str; 6] = [
    "write_file",
    "edit_file",
    "manage_subagents",
    "schedule",
    "manage_task",
    "invoke_subagent",
];

/// 타 CLI 호환 별칭 (실측 미존재 — 주석 분리 후 동일 deny, §6.2 / N-b: 소문자 write·edit 포함)
const READONLY_DENY_TOOL_ALIASES: [&str; 5] = [
    "replace_file_content",
    "multi_replace_file_content",
    "swarm",
    "write",
    "edit",
];

/// 셸 실행 계열 도구명 — `run_command`와 동일한 읽기전용 명령 검증을 거친다 (N-b)
const SHELL_EXEC_TOOL_NAMES: [&str; 5] = ["run_command", "bash", "shell", "exec", "powershell"];

/// 읽기전용 `run_command` 화이트리스트 (§6.2 — 안전 인자 포함 접두사 매칭)
const READONLY_CMD_PREFIXES: [&str; 10] = [
    "git status",
    "git diff",
    "git log",
    "node --test",
    "rg",
    "ls",
    "cat",
    "wc",
    "head",
    "tail",
];

/// 변이 토큰 (§6.2 명시 목록 + 자주 쓰이는 쓰기 계열)
const MUTATING_TOKENS: &[&str] = &[
    "rm", "mv", "cp", "chmod", "chown", "chgrp", "tee", "dd", "sed", "npm", "npx", "ln",
    "truncate", "mkfifo", "mknod", "install", "kill", "pkill", "sudo", "su", "mount",
    "umount", "diskutil", "hdiutil", "launchctl", "defaults", "crontab", "brew", "apt",
    "apt-get", "yum", "dnf", "pip", "pip3", "gem", "curl", "wget", "ssh", "scp", "rsync",
    "open", "osascript", "patch", "sqlite3",
];

/// 허용 명령별 금지 플래그 (B1) — 토큰이 플래그와 동일하거나 `플래그=` 접두면 deny.
/// `-c`는 git에서만 위험(rg -c 카운트·wc -c 바이트 수는 무해)하므로 명령별 스코프.
fn dangerous_flags(base: &str) -> &'static [&'static str] {
    match base {
        "git" => &[
            "--output", "-o", "-c", "--ext-diff", "--exec", "--upload-pack", "--pager",
            "--git-dir",
        ],
        "rg" => &["--pre", "--pre-glob"],
        "node" => &[
            "--import", "--require", "-r", "-e", "--eval", "-p", "--print", "--loader",
            "--experimental-loader",
        ],
        "ls" | "cat" | "wc" | "head" | "tail" => &["-o", "--output"],
        _ => &[],
    }
}

fn has_dangerous_flag(base: &str, tokens: &[&str]) -> bool {
    let flags = dangerous_flags(base);
    if flags.is_empty() {
        return false;
    }
    tokens.iter().any(|t| flags.iter().any(|f| *t == *f || t.starts_with(&format!("{}=", f))))
}

// ---------- 정규식 (지연 컴파일·무예외) ----------

fn re_control_chars() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"[\n\r\x00]").unwrap())
}

fn re_forbidden_shell_chars() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"[|><&;`\n\r\x00]").unwrap())
}

fn re_forbidden_cmd_subst() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\$\(|\$\{").unwrap())
}

fn re_git_readonly() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^git\s+(status|diff|log)(\s|$)").unwrap())
}

fn re_node_test() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^node\s+--test(\s|$)").unwrap())
}

// ---------- env 주입 ----------

/// 훅 판정에 필요한 env 스냅숏 — 지도 값을 우선하고(테스트 주입) `inherit`일 때만
/// 실제 프로세스 env로 폴백한다.
#[derive(Debug, Clone, Default)]
pub struct HookEnv {
    vars: BTreeMap<String, String>,
    inherit_process_env: bool,
}

impl HookEnv {
    /// 실제 프로세스 env를 읽는 기본 진입점(운용 경로).
    pub fn from_process() -> Self {
        HookEnv {
            vars: BTreeMap::new(),
            inherit_process_env: true,
        }
    }

    /// 프로세스 env를 절대 참조하지 않는 격리 env(테스트 전용 계약).
    pub fn isolated() -> Self {
        HookEnv {
            vars: BTreeMap::new(),
            inherit_process_env: false,
        }
    }

    pub fn set(&mut self, key: &str, value: &str) -> &mut Self {
        self.vars.insert(key.to_string(), value.to_string());
        self
    }

    pub fn get(&self, key: &str) -> Option<String> {
        if let Some(v) = self.vars.get(key) {
            return Some(v.clone());
        }
        if self.inherit_process_env {
            return std::env::var(key).ok();
        }
        None
    }

    /// `key == value` 비교 — env 플래그 판정(`HERDR_READONLY=1` 등).
    pub fn equals(&self, key: &str, value: &str) -> bool {
        self.get(key).as_deref() == Some(value)
    }
}

// ---------- 판정 결과 ----------

/// guard stdout JSON과 동일한 형태 — `{"decision":"allow"}` 또는
/// `{"decision":"deny","reason":...}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GuardDecision {
    pub decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl GuardDecision {
    pub fn allow() -> Self {
        GuardDecision {
            decision: "allow",
            reason: None,
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        GuardDecision {
            decision: "deny",
            reason: Some(reason.into()),
        }
    }

    pub fn is_allow(&self) -> bool {
        self.decision == "allow"
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| r#"{"decision":"allow"}"#.to_string())
    }
}

/// `run_command`·셸 실행 도구용 읽기전용 판정 (§6.2, B1): ① 개행·CR·널바이트는
/// 무조건 즉시 deny(별도 reason), ② 화이트리스트 접두사 매칭 AND 파이프/리다이렉션/
/// 백그라운드/명령 치환/변이 토큰 부재, ③ 허용 명령이라도 쓰기·외부실행 플래그
/// (`--output`·`--pre`·`--import` 등) 포함 시 deny일 때만 allow한다.
fn readonly_command_verdict(cmd: &str) -> (bool, Option<&'static str>) {
    if re_control_chars().is_match(cmd) {
        return (false, Some(DENY_MULTILINE_REASON));
    }
    let trimmed = cmd.trim();
    if trimmed.is_empty() {
        return (false, Some(DENY_NOT_WHITELISTED_REASON));
    }
    if re_forbidden_shell_chars().is_match(trimmed) || re_forbidden_cmd_subst().is_match(trimmed) {
        return (false, Some(DENY_NOT_WHITELISTED_REASON));
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let first = tokens[0];
    let base = first.rsplit('/').next().unwrap_or(first);
    if MUTATING_TOKENS.contains(&base) {
        return (false, Some(DENY_NOT_WHITELISTED_REASON));
    }
    if base == "git" && !re_git_readonly().is_match(trimmed) {
        return (false, Some(DENY_NOT_WHITELISTED_REASON));
    }
    if base == "node" && !re_node_test().is_match(trimmed) {
        return (false, Some(DENY_NOT_WHITELISTED_REASON));
    }
    let prefix_ok = READONLY_CMD_PREFIXES
        .iter()
        .any(|p| trimmed == *p || trimmed.starts_with(&format!("{} ", p)));
    if !prefix_ok {
        return (false, Some(DENY_NOT_WHITELISTED_REASON));
    }
    if has_dangerous_flag(base, &tokens[1..]) {
        return (false, Some(DENY_NOT_WHITELISTED_REASON));
    }
    (true, None)
}

/// JS `String()` 코어션 근사 — 문자열·숫자·불린은 그대로 문자열화, 나머지 객체 계열은
/// `"[object Object]"`. collect의 `String(args.CommandLine ?? ...)` 패리티용.
fn js_string_coerce(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(_) => "[object Object]".to_string(),
    }
}

/// nullish 병합(`??`) 규칙으로 첫 유효(비-null) 인자 값을 고른다.
fn first_non_null<'a>(args: &'a serde_json::Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    for key in keys {
        match args.get(*key) {
            Some(Value::Null) | None => continue,
            some => return some,
        }
    }
    None
}

/// JS `String(undefined)`/`String(null)` 패리티 — dedupe key 조립용.
fn ts_string_id(v: Option<&Value>) -> String {
    match v {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(_) => "[object Object]".to_string(),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

// ---------- --guard (PreToolUse) ----------

/// PreToolUse 가드 판정. stdin JSON 원문과 env 스냅숏을 받아 allow/deny를 반환한다.
/// - `HERDR_DELEGATE_MODE=hc` → 위임 도구 3종 무조건 deny.
/// - `HERDR_READONLY=1` → **fail-closed**: 파싱 실패·`toolCall`/`name` 누락·내부 오류
///   모두 deny. 쓰기·편집·위임 도구와 비허용 셸 명령을 차단한다.
/// - 어느 플래그도 없으면 항상 allow(비-readonly 세션은 fail-open).
pub fn run_guard(stdin_json: &str, env: &HookEnv) -> GuardDecision {
    let delegate_hc = env.equals("HERDR_DELEGATE_MODE", "hc");
    let readonly_mode = env.equals("HERDR_READONLY", "1");

    let parsed: Option<Value> = serde_json::from_str(stdin_json).ok();
    let input = match parsed {
        Some(v) if v.is_object() => v,
        _ => {
            // B4: readonly 가드는 fail-closed. 비-readonly 세션은 fail-open(허용).
            return if readonly_mode {
                GuardDecision::deny(FAIL_CLOSED_REASON)
            } else {
                GuardDecision::allow()
            };
        }
    };

    let result = guard_logic(&input, delegate_hc, readonly_mode);
    match result {
        Some(decision) => decision,
        None => {
            if readonly_mode {
                GuardDecision::deny(FAIL_CLOSED_REASON)
            } else {
                GuardDecision::allow()
            }
        }
    }
}

/// 내부 판정 본체 — `None`은 "내부 예외 상당"(fail-closed/허용 분기로 위임).
fn guard_logic(input: &Value, delegate_hc: bool, readonly_mode: bool) -> Option<GuardDecision> {
    let tool_call = input.get("toolCall").filter(|v| v.is_object());
    let name = tool_call.and_then(|tc| tc.get("name")).and_then(Value::as_str);

    if delegate_hc {
        if let Some(n) = name {
            if DELEGATE_DENY_TOOLS.contains(&n) {
                return Some(GuardDecision::deny(DELEGATE_DENY_REASON));
            }
        }
    }

    if readonly_mode {
        let Some(tc) = tool_call else {
            return Some(GuardDecision::deny(FAIL_CLOSED_REASON));
        };
        let Some(n) = name.filter(|s| !s.is_empty()) else {
            return Some(GuardDecision::deny(FAIL_CLOSED_REASON));
        };
        if READONLY_DENY_TOOLS.contains(&n) || READONLY_DENY_TOOL_ALIASES.contains(&n) {
            return Some(GuardDecision::deny(format!(
                "readonly guard: tool \"{}\" is denied",
                n
            )));
        }
        if SHELL_EXEC_TOOL_NAMES.contains(&n) {
            // run_command·bash·shell·exec·powershell 동일 검증 (N-b) — 인자 키도
            // CommandLine/command/cmd 셋 전부 동일하게 취급해 키별 판정 편차를 없앤다(B1).
            let args = tc.get("args").and_then(Value::as_object);
            let raw = args.and_then(|a| first_non_null(a, &["CommandLine", "command", "cmd"]));
            let Some(raw_str) = raw.and_then(Value::as_str) else {
                return Some(GuardDecision::deny(DENY_NOT_WHITELISTED_REASON));
            };
            let (allowed, reason) = readonly_command_verdict(raw_str);
            return Some(if allowed {
                GuardDecision::allow()
            } else {
                GuardDecision::deny(reason.unwrap_or(DENY_NOT_WHITELISTED_REASON))
            });
        }
        return Some(GuardDecision::allow());
    }

    Some(GuardDecision::allow())
}

// ---------- --collect (PostToolUse, fail-open) ----------

/// PostToolUse 수집 — 항상 `{}`(빈 JSON 객체)를 반환한다(fail-open 계약).
/// DB 기록은 `HERDR_DB_WRITE == "1"` AND `DECISION_READONLY != "1"`일 때만 수행하고,
/// 자기 호출(dg.ts/dg-hook.ts 경로 포함)은 기록하지 않는다(B6).
/// 어떤 파싱 실패·DB 오류가 나더라도 절대 패닉하지 않는다.
pub fn run_collect(stdin_json: &str, env: &HookEnv) -> Value {
    let _ = collect_inner(stdin_json, env);
    serde_json::json!({})
}

fn collect_inner(stdin_json: &str, env: &HookEnv) -> Result<(), String> {
    if !env.equals("HERDR_DB_WRITE", "1") || env.equals("DECISION_READONLY", "1") {
        return Ok(());
    }
    let input: Value = serde_json::from_str(stdin_json).map_err(|e| e.to_string())?;
    if !input.is_object() {
        return Ok(());
    }
    let tool_call = input.get("toolCall").and_then(Value::as_object);
    let Some(tool_name) = tool_call
        .and_then(|tc| tc.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Ok(());
    };
    let args = tool_call
        .and_then(|tc| tc.get("args"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let command_line = redact_secrets(&js_string_coerce(first_non_null(
        &args,
        &["CommandLine", "command"],
    )));

    // B6 자기 호출 제외: commandLine뿐 아니라 path 계열 인자에 dg.ts/dg-hook.ts가
    // 들어도 기록하지 않는다(파일 읽기 자체가 자기 호출인 케이스).
    let mut self_parts: Vec<String> = vec![tool_name.to_string(), command_line.clone()];
    for key in ["path", "file_path", "filePath", "target_file", "targetFile"] {
        if let Some(Value::String(s)) = args.get(key) {
            self_parts.push(s.clone());
        }
    }
    if is_self_invocation(&self_parts.join(" ")) {
        return Ok(());
    }

    let conv_id = input.get("conversationId").and_then(Value::as_str);
    let dedupe_key = format!(
        "hook:{}:{}",
        conv_id.unwrap_or("null"),
        ts_string_id(tool_call.and_then(|tc| tc.get("id")))
    );
    let cwd = input
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| ".".to_string())
        });
    let identity = resolve_project_identity(Path::new(&cwd));
    let (route, tool_class) = classify_tool(tool_name, false);

    // ok: error 필드 부재·null·false → 성공. 문자열·객체는 리댁션 후 400자 스니펫.
    let error_v = input.get("error");
    let ok = matches!(error_v, None | Some(Value::Null) | Some(Value::Bool(false)));
    let error_snippet: Option<String> = match error_v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => None,
        Some(Value::String(s)) => Some(truncate_chars(&redact_secrets(s), 400)),
        Some(other) => serde_json::to_string(other)
            .ok()
            .map(|t| truncate_chars(&redact_secrets(&t), 400)),
    };
    let step = input
        .get("stepIdx")
        .and_then(Value::as_f64)
        .map(|f| f as i64);

    // 훅 busy_timeout=250ms — 3s 훅 타임아웃 내 즉시 폴백(§3.2/N1)
    let db_path = ["DGATE_DB_PATH", "DECISION_ORCHESTRATOR_DB_PATH", "GEMINI_TRICK_DB_PATH"]
        .iter()
        .find_map(|k| env.get(k).map(|v| v).filter(|v| !v.trim().is_empty()));
    let db = open_db_safe(
        db_path.as_deref(),
        OpenDbOptions {
            read_only: false,
            busy_timeout_ms: Some(250),
        },
    );
    let Some(db) = db else {
        return Ok(());
    };

    let record_result = record_tool_outcome(
        &db,
        &RecordInput {
            project_id: &identity.project_id,
            project_label: &identity.project_label,
            route,
            tool_class: &tool_class,
            tool_name,
            ok,
            weight: Some(1.0),
            is_retry: None,
            conversation_id: conv_id,
            step,
            target_hash: None,
            error_snippet: error_snippet.as_deref(),
            source: Some(EventSource::Hook),
            dedupe_key: Some(&dedupe_key),
            fork_id: None,
            command_line: Some(&command_line),
            cause: None,
            input_tokens: None,
            output_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            cost_usd: None,
            cost_status: None,
            now_ms: Some(now_ms()),
        },
    );
    // record_tool_outcome 내부에서 오류가 나도 위 커버리지 표지는 시도한다(fail-open).

    if let Some(cid) = conv_id {
        // markHookCoverage(§5.2): transcriptMapper 중복 쓰기 금지 표지.
        // ON CONFLICT 시 session_consec_err를 덮어쓰지 않는다(TS 원문 계약).
        let now = now_ms();
        let _ = db.execute(
            "INSERT INTO session_cursor
               (conversation_id, project_id, has_hook_coverage, session_consec_err, updated_at_ms)
             VALUES (?1, ?2, 1, 0, ?3)
             ON CONFLICT(conversation_id) DO UPDATE SET has_hook_coverage = 1, updated_at_ms = ?4",
            rusqlite::params![cid, identity.project_id, now, now],
        );
    }
    let _ = record_result;
    Ok(())
}

/// CLI 진입 편의 — `--guard`·`--collect`·미지정 모드를 dispatch하고 stdout JSON
/// 문자열을 반환한다(미지정 모드는 가드 경로가 아니므로 fail-open `{}`).
pub fn run_hook_mode(mode: &str, stdin_json: &str, env: &HookEnv) -> String {
    match mode {
        "--guard" => run_guard(stdin_json, env).to_json(),
        "--collect" => run_collect(stdin_json, env).to_string(),
        _ => "{}".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{env_lock, temp_dir};
    use std::sync::MutexGuard;

    fn ro_env() -> HookEnv {
        let mut e = HookEnv::isolated();
        e.set("HERDR_READONLY", "1");
        e
    }

    fn plain_env() -> HookEnv {
        HookEnv::isolated()
    }

    fn tool_json(name: &str, args: serde_json::Value) -> String {
        serde_json::json!({ "toolCall": { "id": "t1", "name": name, "args": args } }).to_string()
    }

    fn run_cmd_json(cmd: &str) -> String {
        tool_json("run_command", serde_json::json!({ "CommandLine": cmd }))
    }

    // ---------- guard: 기본·위임 채널 ----------

    #[test]
    fn guard_allows_everything_when_no_env_flags() {
        let env = plain_env();
        assert!(run_guard(&tool_json("write_file", serde_json::json!({})), &env).is_allow());
        assert!(run_guard(&tool_json("invoke_subagent", serde_json::json!({})), &env).is_allow());
        // 파싱 실패·빈 stdin도 비-readonly 세션은 fail-open
        assert!(run_guard("not json", &env).is_allow());
        assert!(run_guard("", &env).is_allow());
        assert!(run_guard("123", &env).is_allow()); // 객체 아님
        assert!(run_guard("[1,2]", &env).is_allow());
    }

    #[test]
    fn guard_delegate_mode_denies_delegation_tools_only() {
        let mut env = HookEnv::isolated();
        env.set("HERDR_DELEGATE_MODE", "hc");
        for tool in ["invoke_subagent", "manage_subagents", "swarm"] {
            let d = run_guard(&tool_json(tool, serde_json::json!({})), &env);
            assert!(!d.is_allow());
            assert_eq!(d.reason.as_deref(), Some(DELEGATE_DENY_REASON));
        }
        // 위임 외 도구는 통과
        assert!(run_guard(&tool_json("write_file", serde_json::json!({})), &env).is_allow());
        assert!(run_guard(&run_cmd_json("rg foo"), &env).is_allow());
    }

    #[test]
    fn guard_delegate_mode_other_value_is_inert() {
        let mut env = HookEnv::isolated();
        env.set("HERDR_DELEGATE_MODE", "subagent");
        assert!(run_guard(&tool_json("invoke_subagent", serde_json::json!({})), &env).is_allow());
    }

    // ---------- guard: readonly fail-closed ----------

    #[test]
    fn guard_readonly_fail_closed_on_invalid_json() {
        let env = ro_env();
        for bad in ["not json", "", "null", "[1]", "\"str\"", "42"] {
            let d = run_guard(bad, &env);
            assert!(!d.is_allow(), "input={:?}", bad);
            assert_eq!(d.reason.as_deref(), Some(FAIL_CLOSED_REASON));
        }
    }

    #[test]
    fn guard_readonly_fail_closed_on_missing_tool_call_or_name() {
        let env = ro_env();
        for input in [
            serde_json::json!({}).to_string(),
            serde_json::json!({ "toolCall": null }).to_string(),
            serde_json::json!({ "toolCall": {} }).to_string(),
            serde_json::json!({ "toolCall": { "id": 1 } }).to_string(),
            serde_json::json!({ "toolCall": { "name": "" } }).to_string(),
            serde_json::json!({ "toolCall": { "name": 123 } }).to_string(),
        ] {
            let d = run_guard(&input, &env);
            assert!(!d.is_allow(), "input={}", input);
            assert_eq!(d.reason.as_deref(), Some(FAIL_CLOSED_REASON));
        }
    }

    #[test]
    fn guard_readonly_denies_write_and_delegate_tools() {
        let env = ro_env();
        for tool in READONLY_DENY_TOOLS {
            let d = run_guard(&tool_json(tool, serde_json::json!({})), &env);
            assert!(!d.is_allow(), "{}", tool);
            assert_eq!(
                d.reason.as_deref(),
                Some(format!("readonly guard: tool \"{}\" is denied", tool).as_str())
            );
        }
        // 별칭(소문자 write·edit 포함)
        for tool in READONLY_DENY_TOOL_ALIASES {
            let d = run_guard(&tool_json(tool, serde_json::json!({})), &env);
            assert!(!d.is_allow(), "{}", tool);
        }
    }

    #[test]
    fn guard_readonly_allows_read_tools() {
        let env = ro_env();
        assert!(run_guard(&tool_json("read_file", serde_json::json!({ "path": "a" })), &env).is_allow());
        assert!(run_guard(&tool_json("grep_search", serde_json::json!({})), &env).is_allow());
        assert!(run_guard(&tool_json("ask_question", serde_json::json!({})), &env).is_allow());
    }

    // ---------- guard: readonly 셸 명령 검증 ----------

    #[test]
    fn guard_readonly_allows_whitelisted_commands() {
        let env = ro_env();
        for cmd in [
            "git status",
            "git diff --stat",
            "git log -1",
            "node --test tests/",
            "rg pattern src/",
            "ls",
            "ls -la",
            "cat file.txt",
            "wc -l x",
            "head -5 y",
            "tail -3 z",
        ] {
            let d = run_guard(&run_cmd_json(cmd), &env);
            assert!(d.is_allow(), "cmd={:?} reason={:?}", cmd, d.reason);
        }
    }

    #[test]
    fn guard_readonly_denies_non_whitelisted_commands() {
        let env = ro_env();
        for cmd in [
            "npm install",
            "rm -rf /",
            "echo hi",
            "python script.py",
            "git push",
            "git commit -m x",
            "node script.js",
            "find . -name x",
        ] {
            let d = run_guard(&run_cmd_json(cmd), &env);
            assert!(!d.is_allow(), "cmd={:?}", cmd);
            assert_eq!(d.reason.as_deref(), Some(DENY_NOT_WHITELISTED_REASON));
        }
    }

    #[test]
    fn guard_readonly_multiline_and_control_chars_get_dedicated_reason() {
        let env = ro_env();
        for cmd in ["git status\nrm -rf /", "ls\rcat", "ls\0x"] {
            let d = run_guard(&run_cmd_json(cmd), &env);
            assert!(!d.is_allow());
            assert_eq!(d.reason.as_deref(), Some(DENY_MULTILINE_REASON), "cmd={:?}", cmd);
        }
    }

    #[test]
    fn guard_readonly_denies_shell_metacharacters_and_substitution() {
        let env = ro_env();
        for cmd in [
            "cat a | wc -l",
            "ls > out",
            "cat a < in",
            "ls & bg",
            "ls; pwd",
            "echo `whoami`",
            "echo $(whoami)",
            "echo ${HOME}",
            "git status &",
        ] {
            let d = run_guard(&run_cmd_json(cmd), &env);
            assert!(!d.is_allow(), "cmd={:?}", cmd);
            assert_eq!(d.reason.as_deref(), Some(DENY_NOT_WHITELISTED_REASON));
        }
    }

    #[test]
    fn guard_readonly_denies_dangerous_flags_on_allowed_commands() {
        let env = ro_env();
        for cmd in [
            "git log --output=/tmp/x",
            "git log -o file",
            "git -c x=y log",
            "git log --exec=evil",
            "git log --upload-pack=/tmp/evil",
            "git log --pager=evil",
            "git log --git-dir=/evil",
            "rg --pre=evil pattern",
            "rg --pre-glob=* pattern",
            "node --test --import=evil.js",
            "node --test -r evil.js",
            "node --test -e code",
            "node --test --eval code",
            "node --test -p x",
            "node --test --loader x",
            "ls -o out",
            "ls --output=out",
            "cat -o x",
            "wc --output=x",
            "head -o x",
            "tail --output=x",
        ] {
            let d = run_guard(&run_cmd_json(cmd), &env);
            assert!(!d.is_allow(), "cmd={:?}", cmd);
            assert_eq!(d.reason.as_deref(), Some(DENY_NOT_WHITELISTED_REASON));
        }
        // 플래그= 접두 형태도 차단
        assert!(!run_guard(&run_cmd_json("git log --output=/x"), &env).is_allow());
        // 무해한 유사 플래그는 통과(rg -c 카운트·wc -c 바이트)
        assert!(run_guard(&run_cmd_json("rg -c pattern file"), &env).is_allow());
        assert!(run_guard(&run_cmd_json("wc -c file"), &env).is_allow());
    }

    #[test]
    fn guard_readonly_base_token_mutation_via_path_is_caught() {
        let env = ro_env();
        // /usr/bin/rm 처럼 경로로 감싼 변이 토큰도 basename으로 잡는다
        let d = run_guard(&run_cmd_json("/usr/bin/rm -rf /tmp/x"), &env);
        assert!(!d.is_allow());
        // 절대경로 git도 ^git\s+ 앵커 규칙 밖 — not whitelisted(TS 원문 계약 동일)
        let d2 = run_guard(&run_cmd_json("/usr/bin/git status"), &env);
        assert!(!d2.is_allow());
        assert_eq!(d2.reason.as_deref(), Some(DENY_NOT_WHITELISTED_REASON));
    }

    #[test]
    fn guard_readonly_shell_alias_tools_share_command_validation() {
        let env = ro_env();
        for tool in ["bash", "shell", "exec", "powershell"] {
            assert!(
                run_guard(&tool_json(tool, serde_json::json!({ "command": "git status" })), &env).is_allow(),
                "{}",
                tool
            );
            assert!(!run_guard(&tool_json(tool, serde_json::json!({ "command": "npm i" })), &env).is_allow());
            assert!(
                !run_guard(&tool_json(tool, serde_json::json!({ "CommandLine": "npm i" })), &env).is_allow()
            );
            assert!(!run_guard(&tool_json(tool, serde_json::json!({ "cmd": "npm i" })), &env).is_allow());
        }
    }

    #[test]
    fn guard_readonly_shell_tool_with_non_string_command_denies() {
        let env = ro_env();
        for args in [
            serde_json::json!({}),
            serde_json::json!({ "CommandLine": 123 }),
            serde_json::json!({ "command": null, "cmd": null }),
            serde_json::json!({ "CommandLine": ["ls"] }),
        ] {
            let label = args.to_string();
            let d = run_guard(&tool_json("run_command", args), &env);
            assert!(!d.is_allow(), "args={}", label);
            assert_eq!(d.reason.as_deref(), Some(DENY_NOT_WHITELISTED_REASON));
        }
        // null 병합: CommandLine이 null이면 command로 폴백
        let d = run_guard(
            &tool_json("run_command", serde_json::json!({ "CommandLine": null, "command": "ls" })),
            &env,
        );
        assert!(d.is_allow());
    }

    #[test]
    fn guard_isolated_env_never_leaks_process_env() {
        // 프로세스 env에 HERDR_READONLY가 있어도 격리 env는 영향받지 않는다
        let _g: MutexGuard<'_, ()> = env_lock();
        std::env::set_var("HERDR_READONLY", "1");
        let isolated = HookEnv::isolated();
        assert!(run_guard("bad json", &isolated).is_allow());
        std::env::remove_var("HERDR_READONLY");
    }

    // ---------- collect ----------

    fn collect_env(db_path: &std::path::Path) -> HookEnv {
        let mut e = HookEnv::isolated();
        e.set("HERDR_DB_WRITE", "1")
            .set("DGATE_DB_PATH", db_path.to_str().unwrap());
        e
    }

    fn collect_input(name: &str, args: serde_json::Value) -> String {
        serde_json::json!({
            "toolCall": { "id": "id-1", "name": name, "args": args },
            "stepIdx": 3,
            "conversationId": "conv-1",
            "cwd": "/tmp",
        })
        .to_string()
    }

    #[test]
    fn collect_always_returns_empty_object() {
        let env = collect_env(Path::new("/nonexistent-xyz/db.db"));
        assert_eq!(run_collect("not json", &env), serde_json::json!({}));
        assert_eq!(run_collect("{}", &env), serde_json::json!({}));
        assert_eq!(run_collect("", &env), serde_json::json!({}));
        assert_eq!(
            run_collect(&collect_input("read_file", serde_json::json!({ "path": "a" })), &env),
            serde_json::json!({})
        );
    }

    #[test]
    fn collect_skips_when_db_write_disabled_or_readonly() {
        let tmp = temp_dir("hook-collect-skip");
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("d.db");

        let mut e1 = HookEnv::isolated();
        e1.set("DGATE_DB_PATH", db_path.to_str().unwrap()); // HERDR_DB_WRITE 없음
        run_collect(&collect_input("read_file", serde_json::json!({})), &e1);
        assert!(!db_path.exists(), "DB must not be created without HERDR_DB_WRITE=1");

        let mut e2 = HookEnv::isolated();
        e2.set("HERDR_DB_WRITE", "1")
            .set("DECISION_READONLY", "1")
            .set("DGATE_DB_PATH", db_path.to_str().unwrap());
        run_collect(&collect_input("read_file", serde_json::json!({})), &e2);
        assert!(!db_path.exists(), "DB must not be created in DECISION_READONLY=1");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_records_event_and_hook_coverage() {
        let tmp = temp_dir("hook-collect-write");
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("d.db");
        let env = collect_env(&db_path);

        let out = run_collect(&collect_input("read_file", serde_json::json!({ "path": "a.txt" })), &env);
        assert_eq!(out, serde_json::json!({}));
        assert!(db_path.exists());

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let (source, tool, ok, dedupe): (String, String, i64, String) = conn
            .query_row(
                "SELECT source, tool_name, ok, dedupe_key FROM decision_events",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(source, "hook");
        assert_eq!(tool, "read_file");
        assert_eq!(ok, 1);
        assert_eq!(dedupe, "hook:conv-1:id-1");
        let coverage: i64 = conn
            .query_row(
                "SELECT has_hook_coverage FROM session_cursor WHERE conversation_id = 'conv-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(coverage, 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_dedupes_repeated_tool_call_ids() {
        let tmp = temp_dir("hook-collect-dedupe");
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("d.db");
        let env = collect_env(&db_path);

        let input = collect_input("run_command", serde_json::json!({ "CommandLine": "ls" }));
        run_collect(&input, &env);
        run_collect(&input, &env);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM decision_events", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "dedupe_key must prevent double-write");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_records_failure_with_redacted_snippet() {
        let tmp = temp_dir("hook-collect-fail");
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("d.db");
        let env = collect_env(&db_path);

        let input = serde_json::json!({
            "toolCall": { "id": 7, "name": "run_command", "args": { "command": "curl https://x" } },
            "stepIdx": 1,
            "conversationId": "conv-e",
            "cwd": "/tmp",
            "error": "boom MY_API_KEY=abcdef123456 tail",
        })
        .to_string();
        run_collect(&input, &env);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let (ok, snippet, dedupe): (i64, Option<String>, String) = conn
            .query_row(
                "SELECT ok, error_snippet, dedupe_key FROM decision_events",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(ok, 0);
        assert_eq!(dedupe, "hook:conv-e:7");
        assert!(snippet.as_deref().unwrap_or("").contains("MY_API_KEY=<redacted>"), "{:?}", snippet);
        assert!(!snippet.unwrap_or_default().contains("abcdef123456"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_skips_self_invocation() {
        let tmp = temp_dir("hook-collect-self");
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("d.db");
        let env = collect_env(&db_path);

        // commandLine 자기 호출
        run_collect(
            &collect_input("run_command", serde_json::json!({ "command": "node src/decision/dg.ts stats" })),
            &env,
        );
        // path 인자 자기 호출 (파일 읽기가 dg-hook.ts)
        run_collect(
            &collect_input("read_file", serde_json::json!({ "path": "hooks/dg-hook.ts" })),
            &env,
        );
        run_collect(
            &collect_input("read_file", serde_json::json!({ "file_path": ".agents/hooks/dg-hook.ts" })),
            &env,
        );
        assert!(!db_path.exists(), "self-invocations must not even open the DB path writes");

        // 비자기 호출은 정상 기록
        run_collect(&collect_input("read_file", serde_json::json!({ "path": "src/a.ts" })), &env);
        assert!(db_path.exists());
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM decision_events", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_missing_tool_name_is_noop() {
        let tmp = temp_dir("hook-collect-noname");
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("d.db");
        let env = collect_env(&db_path);
        run_collect(&serde_json::json!({ "toolCall": { "id": 1 } }).to_string(), &env);
        run_collect(&serde_json::json!({}).to_string(), &env);
        assert!(!db_path.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_error_false_and_object_forms() {
        let tmp = temp_dir("hook-collect-errform");
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("d.db");
        let env = collect_env(&db_path);

        // error: false → ok=1
        let i1 = serde_json::json!({
            "toolCall": { "id": "a", "name": "ls", "args": {} },
            "conversationId": "c1", "cwd": "/tmp", "error": false
        })
        .to_string();
        run_collect(&i1, &env);
        // error: 객체 → ok=0, JSON 문자열 스니펫
        let i2 = serde_json::json!({
            "toolCall": { "id": "b", "name": "ls", "args": {} },
            "conversationId": "c1", "cwd": "/tmp",
            "error": { "code": "EFAIL", "key": "MY_API_KEY=abcdefghijklmnop" }
        })
        .to_string();
        run_collect(&i2, &env);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mut stmt = conn
            .prepare("SELECT dedupe_key, ok, error_snippet FROM decision_events ORDER BY event_id")
            .unwrap();
        let rows: Vec<(String, i64, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "hook:c1:a");
        assert_eq!(rows[0].1, 1);
        assert_eq!(rows[0].2, None);
        assert_eq!(rows[1].0, "hook:c1:b");
        assert_eq!(rows[1].1, 0);
        let snip = rows[1].2.clone().unwrap();
        assert!(snip.contains("<redacted>"), "{}", snip);
        assert!(!snip.contains("abcdefghijklmnop"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_conversation_id_null_uses_ts_null_string() {
        let tmp = temp_dir("hook-collect-nullconv");
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("d.db");
        let env = collect_env(&db_path);
        let input = serde_json::json!({
            "toolCall": { "name": "ls", "args": {} },
            "cwd": "/tmp",
        })
        .to_string();
        run_collect(&input, &env);
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let dedupe: String = conn
            .query_row("SELECT dedupe_key FROM decision_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(dedupe, "hook:null:undefined");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---------- 모드 dispatch ----------

    #[test]
    fn run_hook_mode_dispatches_and_unknown_mode_is_fail_open() {
        let env = ro_env();
        assert_eq!(run_hook_mode("--guard", "bad", &env), serde_json::json!({ "decision": "deny", "reason": FAIL_CLOSED_REASON }).to_string());
        assert_eq!(run_hook_mode("--collect", "bad", &env), "{}");
        assert_eq!(run_hook_mode("--whatever", "bad", &env), "{}");
        assert_eq!(
            run_hook_mode("--guard", &run_cmd_json("ls"), &env),
            r#"{"decision":"allow"}"#
        );
    }

    #[test]
    fn readonly_verdict_unit_matrix() {
        assert_eq!(readonly_command_verdict("ls"), (true, None));
        assert_eq!(
            readonly_command_verdict("git push"),
            (false, Some(DENY_NOT_WHITELISTED_REASON))
        );
        assert_eq!(
            readonly_command_verdict(""),
            (false, Some(DENY_NOT_WHITELISTED_REASON))
        );
        assert_eq!(
            readonly_command_verdict("   "),
            (false, Some(DENY_NOT_WHITELISTED_REASON))
        );
        assert_eq!(
            readonly_command_verdict("ls\n"),
            (false, Some(DENY_MULTILINE_REASON))
        );
    }
}
