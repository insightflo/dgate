//! `dg wrap` — 자식 프로세스 래핑 + 진행 게이트 하트비트 + 스톨/타임아웃 워치독
//! (dg.ts cmdWrap 대응).
//!
//! - 자식을 `setsid()`로 자체 프로세스 그룹 리더로 띄워 stall/timeout 킬이
//!   자식의 자식까지 프로세스 그룹 전체(`kill(-pgid)`)를 한 번에 끝낸다.
//! - SIGTERM → grace → SIGKILL 2단 킬, 진행 게이트 하트비트(stdout/stderr 진행
//!   시에만 `touch_fork`), redact된 마지막 20줄 테일 캡처, 첫 비공백 stdout 줄
//!   `"rerouted"` 검출, AND 게이트 산출물 검사, `commit_wrap_outcome` 단일 Tx 커밋.
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use crate::args::{optional_flag, parse_int_flag, require_flag, ParsedArgs};
use crate::commands::record::parse_source;
use crate::util::prelude::*;
use dgate_core::engine::{
    self, ClassifyFailureInput, CommitWrapInput, FailureCause, ForkIntervalArgs,
    ForkIntervalPhase, IssueForkInput, RecordInput,
};
use dgate_core::redact::{build_sanitized_child_env, redact_secrets};

/// TS `SIGNAL_EXIT_OFFSET` — 시그널 종료 시 exit code 매핑.
fn signal_exit_offset(signal: i32) -> i32 {
    match signal {
        libc::SIGHUP => 1,
        libc::SIGINT => 2,
        libc::SIGQUIT => 3,
        libc::SIGABRT => 6,
        libc::SIGKILL => 9,
        libc::SIGTERM => 15,
        _ => 0,
    }
}

fn signal_name(signal: i32) -> Option<&'static str> {
    match signal {
        libc::SIGHUP => Some("SIGHUP"),
        libc::SIGINT => Some("SIGINT"),
        libc::SIGQUIT => Some("SIGQUIT"),
        libc::SIGABRT => Some("SIGABRT"),
        libc::SIGKILL => Some("SIGKILL"),
        libc::SIGTERM => Some("SIGTERM"),
        _ => None,
    }
}

/// 스트림 공유 상태 — 두 스트림이 하나의 테일 버퍼에 라인을 누적한다(TS `ingest`).
#[derive(Default)]
struct CaptureState {
    /// 테일 라인(마지막 20줄, redact 적용)
    tail: Mutex<Vec<String>>,
    /// 부분 라인 버퍼(stdout+stderr 공유 — TS 단일 pendingLine 패리티)
    pending: Mutex<String>,
    /// stdout 첫 비공백 줄 스캔 누적 버퍼
    stdout_scan: Mutex<String>,
    first_non_blank: Mutex<Option<String>>,
    stdout_non_empty: AtomicBool,
    last_output_ms: AtomicI64,
    has_new_progress: AtomicBool,
    stalled: AtomicBool,
    timed_out: AtomicBool,
    kill_sent: AtomicBool,
    settled: AtomicBool,
    spawn_error: Mutex<Option<String>>,
}

impl CaptureState {
    fn push_line(&self, line: String) {
        let mut tail = self.tail.lock().unwrap();
        tail.push(redact_secrets(&line));
        if tail.len() > 20 {
            tail.remove(0);
        }
    }

    /// TS `ingest` — 개행 단위 분할, 마지막 조각은 pending으로.
    fn ingest(&self, text: &str) {
        let mut pending = self.pending.lock().unwrap();
        let combined = format!("{}{}", pending, text);
        let mut parts = combined.split('\n').collect::<Vec<&str>>();
        pending.clear();
        let last = parts.pop().unwrap_or("");
        *pending = last.to_string();
        drop(pending);
        for l in parts {
            self.push_line(l.to_string());
        }
    }
}

/// 워치독 + 판스루 리더를 구동하고 자식 종료를 기다린다.
struct WrapRunResult {
    exit_code: Option<i64>,
    signal: Option<i32>,
    spawn_error: Option<String>,
    duration_ms: i64,
    stalled: bool,
    timed_out: bool,
    stdout_non_empty: bool,
    first_non_blank_line: Option<String>,
    tail_lines: Vec<String>,
}

fn run_wrapped_child(
    command: &[String],
    env: Option<BTreeMap<String, String>>,
    stall_ms: i64,
    max_ms: i64,
    heartbeat_ms: i64,
    kill_grace_ms: i64,
    db: Option<Arc<Mutex<Option<rusqlite::Connection>>>>,
    fork_id: Option<String>,
    started_epoch_ms: i64,
    exec_attempt: Option<i64>,
) -> std::io::Result<WrapRunResult> {
    use std::os::unix::process::CommandExt;

    let started = std::time::Instant::now();
    let state = Arc::new(CaptureState {
        last_output_ms: AtomicI64::new(started_epoch_ms),
        ..Default::default()
    });

    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match &env {
        Some(e) => {
            cmd.env_clear();
            cmd.envs(e.iter());
        }
        None => {}
    }
    // 자체 프로세스 그룹 리더 — kill(-pgid)가 트리 전체(자식의 자식까지)를 끝낸다.
    // brief 계약상 setsid 대신 setpgid(0,0)을 쓴다: macOS fork 자식에서 setsid는
    // EINVAL을 내고 후속 setpgid 폴백까지 오염시켜 스폰 전체가 실패한다(실측).
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd.spawn()?;
    let child_pid = child.id() as i32;
    state
        .last_output_ms
        .store(started_epoch_ms, std::sync::atomic::Ordering::SeqCst);

    // PB-1 §3.2: 스폰 직후 `exec` 구간 개시(fallback fork·DB 무연결 시 생략)
    if let (Some(dbm), Some(fk), Some(att)) = (&db, &fork_id, exec_attempt) {
        if let Ok(mut guard) = dbm.lock() {
            if let Some(conn) = guard.as_mut() {
                let _ = engine::record_fork_interval_start(
                    conn,
                    &ForkIntervalArgs {
                        fork_id: fk,
                        attempt: att,
                        phase: ForkIntervalPhase::Exec,
                        now_ms: crate::util::now_ms(),
                    },
                );
            }
        }
    }

    // 리더 스레드 — stdout/stderr 패스스루 + 캡처
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    fn spawn_reader<R: Read + Send + 'static>(stream: Option<R>, is_stdout: bool, st: Arc<CaptureState>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let Some(mut stream) = stream else { return };
            let mut buf = [0u8; 8192];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                        st.last_output_ms
                            .store(crate::util::now_ms(), std::sync::atomic::Ordering::SeqCst);
                        st.has_new_progress
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        if is_stdout {
                            let mut so = std::io::stdout().lock();
                            let _ = so.write_all(text.as_bytes());
                            let _ = so.flush();
                            if !st.stdout_non_empty.load(std::sync::atomic::Ordering::SeqCst)
                                && text.chars().any(|c| !c.is_whitespace())
                            {
                                st.stdout_non_empty
                                    .store(true, std::sync::atomic::Ordering::SeqCst);
                            }
                            // 첫 비공백 줄 검출(TS stdoutScanBuf 로직)
                            {
                                let mut scan = st.stdout_scan.lock().unwrap();
                                let already = st.first_non_blank.lock().unwrap().is_some();
                                if !already {
                                    *scan += &text;
                                    for l in scan.split('\n') {
                                        if !l.trim().is_empty() {
                                            *st.first_non_blank.lock().unwrap() = Some(l.trim().to_string());
                                            break;
                                        }
                                    }
                                }
                            }
                        } else {
                            let mut se = std::io::stderr().lock();
                            let _ = se.write_all(text.as_bytes());
                            let _ = se.flush();
                        }
                        st.ingest(&text);
                    }
                }
            }
        })
    }
    let t_out = spawn_reader(stdout, true, Arc::clone(&state));
    let t_err = spawn_reader(stderr, false, Arc::clone(&state));

    // 워치독 스레드 — stall/timeout 킬 + 진행 게이트 하트비트
    let tick_ms = if stall_ms > 0 {
        (stall_ms / 4).clamp(10, 1000)
    } else {
        1000
    } as u64;
    let wd_state = Arc::clone(&state);
    let wd_db = db.clone();
    let wd_fork = fork_id.clone();
    let watchdog = std::thread::spawn(move || {
        let mut last_touch_ms = 0i64;
        loop {
            if wd_state.settled.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            if wd_state.kill_sent.load(std::sync::atomic::Ordering::SeqCst) {
                // 킬 개시 — grace 후 SIGKILL 확정 킬
                std::thread::sleep(std::time::Duration::from_millis(kill_grace_ms.max(0) as u64));
                if !wd_state.settled.load(std::sync::atomic::Ordering::SeqCst) {
                    unsafe {
                        libc::kill(-child_pid, libc::SIGKILL);
                        libc::kill(child_pid, libc::SIGKILL);
                    }
                }
                return;
            }
            let now = crate::util::now_ms();
            let last_output = wd_state.last_output_ms.load(std::sync::atomic::Ordering::SeqCst);
            if stall_ms > 0 && now - last_output >= stall_ms {
                wd_state.stalled.store(true, std::sync::atomic::Ordering::SeqCst);
                wd_state.kill_sent.store(true, std::sync::atomic::Ordering::SeqCst);
                unsafe { libc::kill(-child_pid, libc::SIGTERM) };
                continue;
            }
            if max_ms > 0 && now - started_epoch_ms >= max_ms {
                wd_state.timed_out.store(true, std::sync::atomic::Ordering::SeqCst);
                wd_state.kill_sent.store(true, std::sync::atomic::Ordering::SeqCst);
                unsafe { libc::kill(-child_pid, libc::SIGTERM) };
                continue;
            }
            let has_progress = wd_state.has_new_progress.load(std::sync::atomic::Ordering::SeqCst);
            if has_progress && heartbeat_ms > 0 && now - last_touch_ms >= heartbeat_ms {
                if let (Some(dbm), Some(fk)) = (&wd_db, &wd_fork) {
                    if let Ok(mut guard) = dbm.lock() {
                        if let Some(conn) = guard.as_mut() {
                            let _ = engine::touch_fork(conn, fk, None, crate::util::now_ms());
                        }
                    }
                }
                wd_state
                    .has_new_progress
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                last_touch_ms = now;
            }
            std::thread::sleep(std::time::Duration::from_millis(tick_ms));
        }
    });

    let status = child.wait()?;
    state
        .settled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = t_out.join();
    let _ = t_err.join();
    let _ = watchdog.join();

    // 잔여 pending 라인 flush
    {
        let mut pending = state.pending.lock().unwrap();
        if !pending.is_empty() {
            let rest = pending.clone();
            pending.clear();
            drop(pending);
            state.push_line(rest);
        }
    }

    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    };
    let result = WrapRunResult {
        exit_code: status.code().map(|c| c as i64),
        signal,
        spawn_error: state.spawn_error.lock().unwrap().take(),
        duration_ms: started.elapsed().as_millis() as i64,
        stalled: state.stalled.load(std::sync::atomic::Ordering::SeqCst),
        timed_out: state.timed_out.load(std::sync::atomic::Ordering::SeqCst),
        stdout_non_empty: state
            .stdout_non_empty
            .load(std::sync::atomic::Ordering::SeqCst),
        first_non_blank_line: state
            .first_non_blank
            .lock()
            .unwrap()
            .take(),
        tail_lines: state.tail.lock().unwrap().clone(),
    };
    Ok(result)
}

fn stat_size_or_null(p: &str) -> Option<u64> {
    std::fs::metadata(p).map(|m| m.len()).ok()
}

pub fn cmd_wrap(args: &ParsedArgs, json: bool) -> CmdResult {
    let route = parse_route(&require_flag(args, "route")?, "route")?;
    let tool_class = require_flag(args, "tool-class")?;
    let fork_opt = optional_flag(args, "fork");
    let source = parse_source(&optional_flag(args, "source").unwrap_or_else(|| "wrap".into()))?;
    let identity = resolve_identity(optional_flag(args, "project").as_deref())?;
    let command = args.rest.clone();
    if command.is_empty() {
        return Err(DgError::Usage(crate::args::UsageError(
            "missing child command after \"--\"".into(),
        )));
    }

    let require_artifact = optional_flag(args, "require-artifact");
    let require_nonempty = crate::args::bool_flag(args, "require-nonempty");
    let require_stdout = crate::args::bool_flag(args, "require-stdout-nonempty");
    let stall_ms = parse_int_flag(args, "stall-ms", 120_000)?;
    let max_ms = parse_int_flag(args, "max-ms", 1_200_000)?;
    let heartbeat_ms = parse_int_flag(args, "heartbeat-ms", 60_000)?;
    let kill_grace_ms = parse_int_flag(args, "kill-grace-ms", 1_000)?;
    let expected_gen = if optional_flag(args, "expected-gen").is_some() {
        Some(parse_int_flag(args, "expected-gen", 0)?)
    } else {
        None
    };
    let expected_attempt = if optional_flag(args, "expected-attempt").is_some() {
        Some(parse_int_flag(args, "expected-attempt", 0)?)
    } else {
        None
    };
    let sanitize_env = crate::args::bool_flag(args, "sanitize-env");
    let allow_env: Vec<String> = optional_flag(args, "allow-env")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    // ---------- fork 확보(DB 오류 → 폴백 fork) ----------
    let mut fork_id: Option<String> = None;
    let mut fork_exec_attempt: Option<i64> = None;
    let db_holder: Arc<Mutex<Option<rusqlite::Connection>>> = Arc::new(Mutex::new(None));
    {
        let db_result = (|| -> Result<Option<rusqlite::Connection>, DgError> {
            let db = open_writer(db_flag(args).as_deref())?;
            engine::settle_orphans(&db, now_ms()).map_err(DgError::Fatal)?;
            Ok(Some(db))
        })();
        match db_result {
            Ok(Some(db)) => {
                let assign = (|| -> Result<String, DgError> {
                    if let Some(fk) = &fork_opt {
                        let fork = engine::get_fork(&db, fk).map_err(DgError::Fatal)?.ok_or_else(|| {
                            DgError::Usage(crate::args::UsageError(format!("fork not found: {fk}")))
                        })?;
                        if !engine::is_fork_live_status(&fork.status) {
                            return Err(DgError::Usage(crate::args::UsageError(format!(
                                "fork {fk} not open (status={})",
                                fork.status
                            ))));
                        }
                        Ok(fk.clone())
                    } else {
                        let post = engine::get_posterior(&db, &identity.project_id, route, &tool_class)
                            .map_err(DgError::Fatal)?;
                        let forced = engine::evaluate_forced(&db, &identity.project_id, route, &tool_class, None)
                            .map_err(DgError::Fatal)?;
                        let decision: &str = if forced.forced_split {
                            "split"
                        } else if post.p >= 0.7 {
                            "sharp"
                        } else {
                            "split"
                        };
                        let title: String = command.join(" ").chars().take(80).collect();
                        engine::issue_fork(
                            &db,
                            &IssueForkInput {
                                project_id: &identity.project_id,
                                project_label: &identity.project_label,
                                route,
                                tool_class: &tool_class,
                                decision,
                                p_at_gate: post.p,
                                basis_at_gate: post.basis,
                                forced_split: forced.forced_split,
                                conversation_id: None,
                                ttl_ms: Some(engine::TTL_EXPLORE_MS),
                                title: Some(&title),
                                metadata: None,
                                now_ms: None,
                            },
                        )
                        .map_err(DgError::Fatal)
                    }
                })();
                match assign {
                    Ok(fk) => {
                        let exec_attempt = engine::get_fork_gen_attempt(&db, &fk)
                            .ok()
                            .flatten()
                            .map(|(_, att)| att);
                        *db_holder.lock().unwrap() = Some(db);
                        fork_id = Some(fk);
                        fork_exec_attempt = exec_attempt;
                    }
                    Err(DgError::Usage(u)) => return Err(DgError::Usage(u)),
                    Err(DgError::Fatal(e)) => {
                        crate::util::eprint_line(&format!("[dg wrap] db fallback: {e}"));
                        fork_id = Some(fallback_fork_id_hex());
                    }
                }
            }
            Err(DgError::Usage(u)) => return Err(DgError::Usage(u)),
            Err(DgError::Fatal(e)) => {
                crate::util::eprint_line(&format!("[dg wrap] db fallback: {e}"));
                fork_id = Some(fallback_fork_id_hex());
            }
            _ => {}
        }
    }

    let effective_fork = fork_id.clone().unwrap_or_else(fallback_fork_id_hex);
    let is_fallback_fork = effective_fork.starts_with("fk_fallback_");

    let child_env = if sanitize_env {
        Some(build_sanitized_child_env(&allow_env))
    } else {
        None
    };

    let started_epoch_ms = now_ms();
    let run = run_wrapped_child(
        &command,
        child_env,
        stall_ms,
        max_ms,
        heartbeat_ms,
        kill_grace_ms,
        Some(Arc::clone(&db_holder)),
        Some(effective_fork.clone()),
        started_epoch_ms,
        fork_exec_attempt,
    )
    .unwrap_or_else(|err| {
        // 스폰 실패 — run 결과와 동일 형태로 정리(원인은 stderr에 노출)
        crate::util::eprint_line(&format!("[dg wrap] spawn failed: {err}"));
        WrapRunResult {
            exit_code: None,
            signal: None,
            spawn_error: Some(err.to_string()),
            duration_ms: 0,
            stalled: false,
            timed_out: false,
            stdout_non_empty: false,
            first_non_blank_line: None,
            tail_lines: Vec::new(),
        }
    });

    // ---------- AND 게이트 출력 계약(§2.5): clean exit에서만 계약 평가 ----------
    let clean_exit =
        run.exit_code == Some(0) && !run.stalled && !run.timed_out && run.spawn_error.is_none();
    let mut contract_ok = true;
    let mut no_result = false;
    let mut artifact_missing = false;
    let mut rerouted = false;
    if require_stdout && clean_exit && run.first_non_blank_line.as_deref() == Some("rerouted") {
        rerouted = true; // 워커의 리드 승격 신호 — 표본·실패 모두 아님
    }
    if require_stdout && clean_exit && !rerouted && !run.stdout_non_empty {
        contract_ok = false;
        no_result = true;
    }
    if let Some(art) = &require_artifact {
        if clean_exit && !rerouted {
            match stat_size_or_null(art) {
                None => {
                    contract_ok = false;
                    artifact_missing = true;
                }
                Some(size) if require_nonempty && size == 0 => {
                    contract_ok = false;
                    artifact_missing = true;
                }
                _ => {}
            }
        }
    }
    let ok = clean_exit && contract_ok && !rerouted;

    let tail_join = run.tail_lines.join("\n");
    let mut cause = engine::classify_failure(&ClassifyFailureInput {
        ok: Some(ok),
        exit_code: run.exit_code,
        signal: run.signal.and_then(signal_name),
        stalled: run.stalled,
        timed_out: run.timed_out,
        artifact_missing,
        no_result,
        gate_fail: false,
        error_text: run.spawn_error.as_deref().or(Some(tail_join.as_str())),
    });
    // 외인성 정상 중단(SIGINT/SIGTERM, 스톨 킬 제외)은 rerouted로 분리
    let mut outcome: &str = if rerouted { "rerouted" } else if ok { "ok" } else { "fail" };
    if !rerouted && cause == FailureCause::Interrupted {
        outcome = "rerouted";
    }
    if rerouted && cause != FailureCause::Interrupted {
        cause = FailureCause::Interrupted;
    }

    // ---------- 단일 Tx 커밋(commit_wrap_outcome) ----------
    let mut committed = false;
    let mut recorded = false;
    let mut closed = false;
    let mut reject_reason: Option<String> = None;
    {
        let mut guard = db_holder.lock().unwrap();
        if let Some(db) = guard.as_mut() {
            let commit_res = (|| -> Result<dgate_core::engine::CommitWrapResult, String> {
                if !is_fallback_fork {
                    engine::commit_wrap_outcome(
                        db,
                        &CommitWrapInput {
                            fork_id: &effective_fork,
                            tool_name: Some("dg_wrap"),
                            ok,
                            outcome,
                            cause,
                            exit_code: run.exit_code,
                            signal: run.signal.and_then(signal_name),
                            error_snippet: run
                                .spawn_error
                                .as_deref()
                                .or(Some(tail_join.as_str())),
                            command_line: Some(&command.join(" ")),
                            source: Some(source),
                            expected_gen,
                            expected_attempt,
                            ..Default::default()
                        },
                    )
                } else {
                    // fork 미발행 폴백 — 표본만 기록(구 동작 호환)
                    let res = engine::record_tool_outcome(
                        db,
                        &RecordInput {
                            project_id: &identity.project_id,
                            project_label: &identity.project_label,
                            route,
                            tool_class: &tool_class,
                            tool_name: "dg_wrap",
                            ok,
                            weight: Some(1.0),
                            conversation_id: None,
                            source: Some(source),
                            fork_id: Some(&effective_fork),
                            command_line: Some(&command.join(" ")),
                            cause: Some(cause),
                            ..Default::default()
                        },
                    )
                    .map(|r| dgate_core::engine::CommitWrapResult {
                        committed: false,
                        closed: false,
                        recorded: r.recorded,
                        outcome: outcome.to_string(),
                        reason: None,
                        p_after: r.p_after,
                    })?;
                    Ok(res)
                }
            })();
            match commit_res {
                Ok(res) => {
                    committed = res.committed;
                    recorded = res.recorded;
                    closed = res.closed;
                    if let Some(r) = res.reason {
                        reject_reason = Some(r.to_string());
                    }
                }
                Err(e) => {
                    crate::util::eprint_line(&format!("[dg wrap] record fallback: {e}"));
                }
            }
        }
    }

    let exit_code: i64 = match run.exit_code {
        Some(c) => c,
        None => match run.signal {
            Some(sig) => 128 + signal_exit_offset(sig) as i64,
            None => 127,
        },
    };
    let mut summary = Map::new();
    summary.insert("fork_id".into(), json!(effective_fork));
    summary.insert("exit_code".into(), json!(exit_code));
    summary.insert("ok".into(), json!(if ok { 1 } else { 0 }));
    summary.insert("cause".into(), json!(cause.as_str()));
    summary.insert("outcome".into(), json!(outcome));
    summary.insert("committed".into(), json!(committed));
    summary.insert("recorded".into(), json!(recorded));
    summary.insert("closed".into(), json!(closed));
    summary.insert("stalled".into(), json!(run.stalled));
    summary.insert("timed_out".into(), json!(run.timed_out));
    summary.insert("duration_ms".into(), json!(run.duration_ms));
    summary.insert("tail_lines".into(), json!(run.tail_lines));
    if let Some(r) = reject_reason {
        summary.insert("reason".into(), json!(r));
    }
    let summary_value = Value::Object(summary);
    {
        let mut se = std::io::stderr().lock();
        let _ = writeln!(se, "{summary_value}");
    }
    if json {
        let mut so = std::io::stdout().lock();
        let _ = writeln!(so, "{summary_value}");
    }
    Ok(exit_code as i32)
}
