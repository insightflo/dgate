//! CLI 종단간 검증(brief-rust-cli §2.1) — 컴파일된 `dg` 바이너리를 직접 호출해
//! 전체 생명주기·읽기전용 불변성·게이트 종료코드·훅·setup을 검증한다.
//!
//! 실행: `cargo test -p dgate-cli --test cli_e2e`
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use sha2::{Digest, Sha256};

const DG: &str = env!("CARGO_BIN_EXE_dg");

fn temp_base(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dg-e2e-{}-{}-{}",
        prefix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sha256_file(p: &Path) -> String {
    let data = std::fs::read(p).unwrap();
    let mut h = Sha256::new();
    h.update(&data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

struct DgResult {
    status: i32,
    stdout: String,
    stderr: String,
}

fn dg_with_env(args: &[&str], envs: &[(&str, &str)], stdin: Option<&str>, cwd: Option<&Path>) -> DgResult {
    let mut cmd = Command::new(DG);
    cmd.args(args);
    // DB 격리 — 어떤 테스트도 사용자 기본 DB를 건드리지 않는다
    cmd.env_remove("DGATE_DB_PATH");
    cmd.env_remove("DECISION_ORCHESTRATOR_DB_PATH");
    cmd.env_remove("GEMINI_TRICK_DB_PATH");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if stdin.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }
    // spawn()은 기본적으로 stdout/stderr을 상속한다 — wait_with_output로 읽으려면
    // 명시적으로 파이프해야 한다.
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("dg spawn");
    if let Some(input) = stdin {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().expect("dg wait");
    DgResult {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn dg(args: &[&str], envs: &[(&str, &str)]) -> DgResult {
    dg_with_env(args, envs, None, None)
}

// ---------------------------------------------------------------------------
// 크로스플랫폼 wrap 자식 헬퍼 — Unix: /bin/sh -c, Windows: cmd /C
// ---------------------------------------------------------------------------

fn strs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

fn shell_command(script: &str) -> Vec<String> {
    #[cfg(unix)]
    {
        vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()]
    }
    #[cfg(windows)]
    {
        vec!["cmd".to_string(), "/C".to_string(), script.to_string()]
    }
}

/// 산출물 생성 커맨드 argv — Windows는 `>`·`&`를 별도 argv 토큰으로 넘겨야
/// `cmd /C`가 구문 그대로 해석한다. 단일 스크립트 문자열 안에 손수 넣은 `"`는
/// Rust Command::arg가 MSVCRT `\"`로 재인용해 cmd가 "파일 이름 구문 오류"를
/// 내는 실측 버그가 있다(pipeline.rs wrap_inner_command와 동일 설계: 토큰 단위
/// 전달 — 리다이렉트 대상이 공백을 포함해도 Rust가 그 토큰만 따로 인용한다).
fn write_artifact_command(path: &str) -> Vec<String> {
    #[cfg(unix)]
    {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("echo working; echo result > '{path}'; echo done"),
        ]
    }
    #[cfg(windows)]
    {
        vec![
            "cmd".to_string(),
            "/C".to_string(),
            "echo".to_string(),
            "working".to_string(),
            "&".to_string(),
            "echo".to_string(),
            "result".to_string(),
            ">".to_string(),
            path.to_string(),
            "&".to_string(),
            "echo".to_string(),
            "done".to_string(),
        ]
    }
}

/// 스톨 유도 스크립트 — 60초 무출력 손자 프로세스(워치독 그룹/트리 킬 검증).
fn stall_script() -> String {
    #[cfg(unix)]
    {
        "echo start; /bin/sh -c 'sleep 60' ; echo never".to_string()
    }
    #[cfg(windows)]
    {
        "echo start & ping -n 60 127.0.0.1 > nul & echo never".to_string()
    }
}

/// exit 0 + 마지막 stdout 줄을 JSON로 파싱
fn dg_json(args: &[&str], envs: &[(&str, &str)]) -> Value {
    let mut all = args.to_vec();
    all.push("--json");
    let r = dg(&all, envs);
    assert_eq!(r.status, 0, "dg {} exit={} stderr={}", all.join(" "), r.status, r.stderr);
    let last = r.stdout.trim().lines().last().unwrap_or("");
    serde_json::from_str(last).unwrap_or_else(|e| panic!("invalid JSON from dg {}: {e} ({last})", all.join(" ")))
}

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("git spawn");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_repo(base: &Path) -> PathBuf {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_ok(&repo, &["init", "-q"]);
    git_ok(&repo, &["config", "user.email", "e2e@example.com"]);
    git_ok(&repo, &["config", "user.name", "e2e-test"]);
    git_ok(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("base.txt"), "v0\n").unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "init"]);
    repo
}

/// 스테이지드+언스테이지드+언트랙 dirty 워크트리
fn add_worktree(repo: &Path, name: &str) -> PathBuf {
    let wt = repo.parent().unwrap().join(name);
    git_ok(repo, &["worktree", "add", "-q", "-b", name, &wt.display().to_string()]);
    std::fs::write(wt.join("base.txt"), "v2-idx\n").unwrap();
    git_ok(&wt, &["add", "base.txt"]);
    std::fs::write(wt.join("base.txt"), "v2-wt\n").unwrap();
    std::fs::create_dir_all(wt.join("notes")).unwrap();
    std::fs::write(wt.join("notes/draft.txt"), "draft\n").unwrap();
    wt
}

// ---------------------------------------------------------------------------

#[test]
fn test_01_route_wrap_lifecycle_stats_report_readonly() {
    let base = temp_base("lifecycle");
    let repo = init_repo(&base);
    let db = base.join("lifecycle.db");
    let dbs = db.display().to_string();
    let envs = [("DGATE_DB_PATH", dbs.as_str())];
    let repo_s = repo.display().to_string();

    // ---------- route → fork 발행 ----------
    let route = dg_json(
        &["route", "--task-kind", "single_edit", "--project", &repo_s],
        &envs,
    );
    let fork_id = route["fork_id"].as_str().unwrap().to_string();
    assert!(fork_id.starts_with("fk_"), "fork id 형식: {fork_id}");
    assert_eq!(route["task_kind"], "single_edit");
    assert_eq!(route["decision"], "split", "초기 p<θ → split");

    // ---------- wrap 성공(산출물 AND 게이트 통과) ----------
    let artifact = base.join("out.txt");
    let mut wrap_args: Vec<String> = [
        "wrap", "--route", "file_select", "--tool-class", "file_edit",
        "--fork", &fork_id, "--project", &repo_s,
        "--require-artifact", &artifact.display().to_string(),
        "--require-nonempty", "--require-stdout-nonempty",
        "--stall-ms", "8000",
        "--",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    wrap_args.extend(write_artifact_command(&artifact.display().to_string()));
    let r = dg(&strs(&wrap_args), &envs);
    assert_eq!(r.status, 0, "wrap success exit=0 (stderr={})", r.stderr);
    let summary: Value = serde_json::from_str(r.stderr.trim().lines().last().unwrap()).unwrap();
    assert_eq!(summary["ok"], 1, "AND 게이트 통과: {summary}");
    assert_eq!(summary["outcome"], "ok");
    assert_eq!(summary["committed"], true);
    assert_eq!(summary["closed"], true);

    // ---------- wrap 스톨 타임아웃 — 프로세스 그룹/트리 전체 킬 ----------
    let gate2 = dg_json(&["gate", "--route", "tool_select", "--tool-class", "shell_exec", "--project", &repo_s], &envs);
    let fork2 = gate2["fork_id"].as_str().unwrap().to_string();
    let started = std::time::Instant::now();
    let mut wrap_args: Vec<String> = [
        "wrap", "--route", "tool_select", "--tool-class", "shell_exec",
        "--fork", &fork2, "--project", &repo_s,
        "--stall-ms", "800", "--kill-grace-ms", "300", "--max-ms", "60000",
        "--",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    wrap_args.extend(shell_command(&stall_script()));
    let r = dg(&strs(&wrap_args), &envs);
    let elapsed = started.elapsed();
    if cfg!(unix) {
        assert_eq!(r.status, 143, "SIGTERM 그룹 킬 → 128+15 (got {})", r.status);
    } else {
        // Windows: taskkill /T /F 강제 종료 — 자식은 비정상 종료 코드로 끝난다
        assert_ne!(r.status, 0, "taskkill 트리 킬 → 비정상 종료 (got {})", r.status);
    }
    assert!(elapsed.as_secs() < 10, "워치독이 60s 슬립을 조기 킬해야 함: {elapsed:?}");
    let summary: Value = serde_json::from_str(r.stderr.trim().lines().last().unwrap()).unwrap();
    assert_eq!(summary["stalled"], true);
    assert_eq!(summary["cause"], "stall");
    assert_eq!(summary["outcome"], "fail");
    assert_eq!(summary["committed"], true, "스톨 실패도 단일 Tx 커밋");
    // 손자 sleep 60도 그룹 킬로 소멸 확인(Unix 전용 pgrep)
    #[cfg(unix)]
    {
        let leftover = Command::new("pgrep").args(["-f", "sleep 60"]).output().unwrap();
        assert!(
            String::from_utf8_lossy(&leftover.stdout).trim().is_empty(),
            "프로세스 그룹 킬 후 sleep 60 잔존 없음"
        );
    }

    // ---------- wrap --stall-action warn: 무출력 임계 초과 시 경고만 출력하고 킬 금지 ----------
    let gate_warn = dg_json(&["gate", "--route", "tool_select", "--tool-class", "shell_exec", "--project", &repo_s], &envs);
    let fork_warn = gate_warn["fork_id"].as_str().unwrap().to_string();
    let warn_script = if cfg!(windows) {
        "ping -n 2 127.0.0.1 > NUL & echo ok-after-silence"
    } else {
        "sleep 0.6; echo ok-after-silence"
    };
    let mut warn_args: Vec<String> = [
        "wrap", "--route", "tool_select", "--tool-class", "shell_exec",
        "--fork", &fork_warn, "--project", &repo_s,
        "--stall-ms", "200", "--stall-action", "warn", "--max-ms", "15000",
        "--",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    warn_args.extend(shell_command(warn_script));
    let rw = dg(&strs(&warn_args), &envs);
    assert_eq!(rw.status, 0, "warn 모드는 자식을 끊지 않고 완주: {}", rw.stderr);
    assert!(rw.stdout.contains("ok-after-silence"));
    assert!(rw.stderr.contains("stall warning"), "경고 로그 출력: {}", rw.stderr);
    let summary_w: Value = serde_json::from_str(rw.stderr.trim().lines().last().unwrap()).unwrap();
    assert_eq!(summary_w["outcome"], "ok");
    assert_eq!(summary_w["stalled"], false);

    // ---------- wrap 산출물 누락 → fail 기록(exit은 자식 종료코드) ----------
    let gate3 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &envs);
    let fork3 = gate3["fork_id"].as_str().unwrap().to_string();
    let mut wrap_args: Vec<String> = [
        "wrap", "--route", "file_select", "--tool-class", "file_edit",
        "--fork", &fork3, "--project", &repo_s,
        "--require-artifact", &base.join("missing.txt").display().to_string(),
        "--",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    wrap_args.extend(shell_command("echo clean-but-no-artifact"));
    let r = dg(&strs(&wrap_args), &envs);
    assert_eq!(r.status, 0, "자식 exit 0 → wrap exit 0(계약 위반은 DB에 fail)");
    let summary: Value = serde_json::from_str(r.stderr.trim().lines().last().unwrap()).unwrap();
    assert_eq!(summary["ok"], 0);
    assert_eq!(summary["cause"], "artifact_missing");
    assert_eq!(summary["outcome"], "fail");

    // ---------- wrap rerouted — 첫 줄 "rerouted" → 표본·실패 아님 ----------
    let gate4 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &envs);
    let fork4 = gate4["fork_id"].as_str().unwrap().to_string();
    let mut wrap_args: Vec<String> = [
        "wrap", "--route", "file_select", "--tool-class", "file_edit",
        "--fork", &fork4, "--project", &repo_s,
        "--require-stdout-nonempty",
        "--",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    wrap_args.extend(shell_command("echo rerouted"));
    let r = dg(&strs(&wrap_args), &envs);
    assert_eq!(r.status, 0);
    let summary: Value = serde_json::from_str(r.stderr.trim().lines().last().unwrap()).unwrap();
    assert_eq!(summary["outcome"], "rerouted");
    assert_eq!(summary["cause"], "interrupted");

    // ---------- 생명주기 나머지: touch/attempt/close/retro/settle/compact/record ----------
    let gate5 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_read", "--project", &repo_s], &envs);
    let fork5 = gate5["fork_id"].as_str().unwrap().to_string();
    let touch = dg_json(&["touch", "--fork", &fork5, "--ttl-ms", "900000"], &envs);
    assert_eq!(touch["ttl_ms"], 900000);
    let att = dg_json(&["attempt", "--fork", &fork5, "--bump-gen"], &envs);
    assert_eq!(att["generation"], 2);
    assert_eq!(att["attempt"], 2);
    let rec = dg_json(
        &["record", "--route", "tool_select", "--tool-class", "shell_exec", "--tool", "run_command",
          "--ok", "1", "--project", &repo_s],
        &envs,
    );
    assert_eq!(rec["recorded"], true);
    let close = dg_json(&["close", "--fork", &fork5, "--outcome", "ok", "--by", "dispatcher"], &envs);
    assert_eq!(close["status"], "ok");
    let retro = dg_json(&["retro", "--fork", &fork5], &envs);
    assert!(retro["markdown"].as_str().unwrap().contains("Loop Retro"));
    let settle = dg_json(&["settle-orphans"], &envs);
    assert_eq!(settle["settled"], 0);
    let compact = dg_json(&["compact"], &envs);
    assert!(compact["remaining_events"].as_i64().unwrap() > 0);

    // ---------- close 책임 매트릭스: ttl은 ok/fail 불가 ----------
    let gate6 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_read", "--project", &repo_s], &envs);
    let fork6 = gate6["fork_id"].as_str().unwrap().to_string();
    let r = dg(&["close", "--fork", &fork6, "--outcome", "fail", "--by", "ttl", "--json"], &envs);
    assert_eq!(r.status, 2, "close by=ttl outcome=fail → UsageError exit 2");

    // ---------- stats: 읽기전용 — DB sha256 불변 ----------
    let hash_before = sha256_file(&db);
    let stats = dg_json(&["stats", "--cost", "--timing", "--project", &repo_s], &envs);
    assert_eq!(stats["cmd"], "stats");
    assert_eq!(stats["cost"]["schema_v1"], false);
    let total = stats["cost"]["total_events"].as_i64().unwrap();
    assert!(total >= 5, "wrap/record/close 이벤트 집계: {total}");
    assert_eq!(stats["timing"]["intervals_unavailable"], false);
    assert_eq!(sha256_file(&db), hash_before, "stats 실행 후 DB 파일 sha256 불변");

    // ---------- report: allowlist 거부 exit 2 + 파일 0개 생성 + DB 불변 ----------
    for denied in [repo.join("src/evil.md"), repo.join(".agents/evil.md")] {
        let r = dg(
            &["report", "--db", &dbs, "--repo", &repo_s, "--out", &denied.display().to_string()],
            &envs,
        );
        assert_eq!(r.status, 2, "거부 경로 {} → exit 2", denied.display());
        assert!(!denied.exists(), "거부 경로 파일 미생성");
        assert!(
            !denied.parent().map(|p| p.join(".gitkeep").exists()).unwrap_or(false)
        );
    }
    let allowed = repo.join(".herdr/report/r.md");
    let rep = dg_json(&["report", "--repo", &repo_s, "--out", &allowed.display().to_string()], &envs);
    assert_eq!(rep["written"], true);
    assert!(allowed.exists(), ".herdr/report 허용 경로 생성");
    assert_eq!(sha256_file(&db), hash_before, "report 실행 후에도 DB 불변");

    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn test_02_backprop_verdict_pipeline_fail_closed() {
    let base = temp_base("verdict");
    let repo = init_repo(&base);
    let db = base.join("v.db");
    let dbs = db.display().to_string();
    // XDG_STATE_HOME 격리 — 머신 전역 dispatch-mode(strict 포함)와 무관하게 off로 동작.
    let xstate_off = base.join("xstate-off").display().to_string();
    let envs = [("DGATE_DB_PATH", dbs.as_str()), ("XDG_STATE_HOME", xstate_off.as_str())];
    let repo_s = repo.display().to_string();

    let mk_verdict = |fork: &str, decision: &str, findings: Value, path: &str| {
        let v = serde_json::json!({
            "schema_version": 1,
            "fork_id": fork,
            "decision": decision,
            "reason": "e2e verdict",
            "findings": findings,
        });
        std::fs::write(path, v.to_string()).unwrap();
    };

    // (a) pass verdict → exit 0
    let g1 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &envs);
    let f1 = g1["fork_id"].as_str().unwrap().to_string();
    mk_verdict(&f1, "pass", serde_json::json!([]), &base.join("v-ok.json").display().to_string());
    let r = dg(
        &["backprop", "--fork", &f1, "--verdict-file", &base.join("v-ok.json").display().to_string()],
        &envs,
    );
    assert_eq!(r.status, 0, "pass verdict → exit 0 (stderr={})", r.stderr);

    // (b) 모순 verdict(pass + P0 finding) → fail-closed exit 3
    let g2 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &envs);
    let f2 = g2["fork_id"].as_str().unwrap().to_string();
    mk_verdict(
        &f2,
        "pass",
        serde_json::json!([{"severity": "P0", "title": "broken", "evidence": "hard fail"}]),
        &base.join("v-bad.json").display().to_string(),
    );
    let r = dg(&["backprop", "--fork", &f2, "--verdict-file", &base.join("v-bad.json").display().to_string(), "--json"], &envs);
    assert_eq!(r.status, 3, "모순 verdict → fail-closed exit 3");
    let parsed: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(parsed["valid"], false);
    assert_eq!(parsed["effective_decision"], "human");
    assert_eq!(parsed["exit_code"], 3);

    // (c) stale HEAD — expected-head 불일치 → exit 3
    let g3 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &envs);
    let f3 = g3["fork_id"].as_str().unwrap().to_string();
    mk_verdict(&f3, "pass", serde_json::json!([]), &base.join("v-stale.json").display().to_string());
    let r = dg(
        &[
            "backprop", "--fork", &f3, "--verdict-file", &base.join("v-stale.json").display().to_string(),
            "--expected-head", "0123456789abcdef0123456789abcdef01234567",
        ],
        &envs,
    );
    assert_eq!(r.status, 3, "stale verdict → exit 3");

    // (d) verdict 파일 부재 → exit 3 (exit 2가 아님 — fail-closed)
    let r = dg(
        &["backprop", "--fork", &f3, "--verdict-file", &base.join("nope.json").display().to_string()],
        &envs,
    );
    assert_eq!(r.status, 3, "missing verdict file → exit 3");

    // (e) fail verdict + evidence → backprop_fail exit 0
    let g4 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &envs);
    let f4 = g4["fork_id"].as_str().unwrap().to_string();
    mk_verdict(
        &f4,
        "fail",
        serde_json::json!([{"severity": "P1", "title": "wrong logic", "evidence": "line 42"}]),
        &base.join("v-fail.json").display().to_string(),
    );
    let r = dg(&["backprop", "--fork", &f4, "--verdict-file", &base.join("v-fail.json").display().to_string(), "--json"], &envs);
    assert_eq!(r.status, 0, "fail verdict → backprop_fail exit 0");
    let parsed: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(parsed["action"], "backprop_fail");

    // (f) 수동 backprop — backprop_fail 상태 재역귀속 거부(f1은 ok라 재개방 허용됨)
    let r = dg(&["backprop", "--fork", &f4, "--penalty", "2.0", "--json"], &envs);
    assert_eq!(r.status, 2, "backprop_fail 상태 재역귀속 → UsageError 2");

    // (g) merge_danger 포함 pass verdict → exit 0 + 출력 JSON 에코(Change②)
    let g5 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &envs);
    let f5 = g5["fork_id"].as_str().unwrap().to_string();
    let v5 = serde_json::json!({
        "schema_version": 1,
        "fork_id": f5,
        "decision": "pass",
        "reason": "e2e verdict",
        "findings": [],
        "merge_danger": { "door": "two-way", "blast_radius": "verdict.rs 검증 경로" },
    });
    let p5 = base.join("v-md.json");
    std::fs::write(&p5, v5.to_string()).unwrap();
    let r = dg(&["backprop", "--fork", &f5, "--verdict-file", &p5.display().to_string(), "--json"], &envs);
    assert_eq!(r.status, 0, "merge_danger verdict → exit 0 (stderr={})", r.stderr);
    let parsed: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(parsed["merge_danger"]["door"], "two-way");
    assert_eq!(parsed["merge_danger"]["blast_radius"], "verdict.rs 검증 경로");

    // (h) strict 모드 — merge_danger 없으면 UsageError exit 2, 있으면 exit 0
    let xstrict = base.join("xstate-strict");
    std::fs::create_dir_all(xstrict.join("dgate")).unwrap();
    std::fs::write(xstrict.join("dgate").join("dispatch-mode"), "strict\n").unwrap();
    let xstrict_s = xstrict.display().to_string();
    let strict_envs = [("DGATE_DB_PATH", dbs.as_str()), ("XDG_STATE_HOME", xstrict_s.as_str())];
    let g6 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &strict_envs);
    let f6 = g6["fork_id"].as_str().unwrap().to_string();
    mk_verdict(&f6, "pass", serde_json::json!([]), &base.join("v-nomd.json").display().to_string());
    let r = dg(
        &["backprop", "--fork", &f6, "--verdict-file", &base.join("v-nomd.json").display().to_string(), "--json"],
        &strict_envs,
    );
    assert_eq!(r.status, 2, "strict + merge_danger 부재 → UsageError 2");
    assert!(r.stderr.contains("merge_danger"), "거부 사유에 merge_danger 언급: {}", r.stderr);
    let p7 = base.join("v-md-strict.json");
    std::fs::write(&p7, serde_json::json!({
        "schema_version": 1,
        "fork_id": f6,
        "decision": "pass",
        "reason": "e2e verdict",
        "findings": [],
        "merge_danger": { "door": "one-way", "blast_radius": "CLI backprop 게이트" },
    }).to_string()).unwrap();
    let r = dg(&["backprop", "--fork", &f6, "--verdict-file", &p7.display().to_string(), "--json"], &strict_envs);
    assert_eq!(r.status, 0, "strict + merge_danger 있음 → exit 0 (stderr={})", r.stderr);
    let parsed: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(parsed["merge_danger"]["door"], "one-way");

    // (i) strict 모드 + 무효 verdict(모순) → 게이트가 아닌 기존 fail-closed exit 3
    let p8 = base.join("v-strict-bad.json");
    std::fs::write(&p8, serde_json::json!({
        "schema_version": 1,
        "fork_id": f6,
        "decision": "pass",
        "reason": "e2e verdict",
        "findings": [{ "severity": "P0", "title": "broken", "evidence": "hard fail" }],
    }).to_string()).unwrap();
    let r = dg(&["backprop", "--fork", &f6, "--verdict-file", &p8.display().to_string(), "--json"], &strict_envs);
    assert_eq!(r.status, 3, "strict라도 무효 verdict는 escalate_human exit 3");

    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn test_03_checkpoint_worktree_gates() {
    let base = temp_base("ckpt");
    let repo = init_repo(&base);
    let wt = add_worktree(&repo, "coder-e2e");
    let ckpt_root = base.join("ckpt");
    let repo_s = repo.display().to_string();
    let envs: [(&str, &str); 0] = [];

    // create → verify → restore 왕복
    let created = dg_json(
        &[
            "checkpoint", "create", "--worktree", &wt.display().to_string(),
            "--run-id", "run-e2e", "--repo", &repo_s,
            "--checkpoint-root", &ckpt_root.display().to_string(),
        ],
        &envs,
    );
    assert_eq!(created["saved"], true, "create: {:?}", created["errors"]);
    let ckpt_dir = created["checkpoint_dir"].as_str().unwrap().to_string();
    assert_eq!(Path::new(&ckpt_dir), ckpt_root.join("run-e2e/coder-e2e"));

    let verified = dg_json(&["checkpoint", "verify", "--dir", &ckpt_dir], &envs);
    assert_eq!(verified["valid"], true);

    let target = base.join("restore-wt");
    git_ok(&repo, &["worktree", "add", "-q", "-b", "restore-wt", &target.display().to_string()]);
    let restored = dg_json(
        &["checkpoint", "restore", "--dir", &ckpt_dir, "--target", &target.display().to_string()],
        &envs,
    );
    assert_eq!(restored["restored"], true, "restore: {:?}", restored["errors"]);
    assert_eq!(std::fs::read_to_string(target.join("base.txt")).unwrap().replace("\r\n", "\n"), "v2-wt\n", "unstaged 복원");
    assert!(target.join("notes/draft.txt").exists(), "untracked 복원");

    // 1바이트 변조 → verify exit 1
    let mut patch = std::fs::read(Path::new(&ckpt_dir).join("index.patch")).unwrap();
    patch.push(b'X');
    std::fs::write(Path::new(&ckpt_dir).join("index.patch"), patch).unwrap();
    let r = dg(&["checkpoint", "verify", "--dir", &ckpt_dir, "--json"], &envs);
    assert_eq!(r.status, 1, "변조 체크포인트 verify → exit 1");

    // worktree remove — 읽기전용 ckpt root → 게이트 거부 exit 4 + 워크트리 보존
    // (Windows는 디렉터리 읽기전용 비트가 쓰기를 차단하지 않는다 — Unix 전용 시나리오)
    let ckpt_ro = base.join("ckpt-ro");
    std::fs::create_dir_all(&ckpt_ro).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&ckpt_ro).unwrap().permissions();
        perm.set_mode(0o500);
        std::fs::set_permissions(&ckpt_ro, perm).unwrap();
        let denied = dg(
            &[
                "worktree", "remove", "--worktree", &wt.display().to_string(),
                "--repo", &repo_s, "--run-id", "rm-fail",
                "--checkpoint-root", &ckpt_ro.display().to_string(),
            ],
            &envs,
        );
        assert_eq!(denied.status, 4, "체크포인트 게이트 거부 → exit 4");
        assert!(wt.join("base.txt").exists(), "워크트리 보존(거부 시)");
        let mut perm2 = std::fs::metadata(&ckpt_ro).unwrap().permissions();
        perm2.set_mode(0o755);
        std::fs::set_permissions(&ckpt_ro, perm2).unwrap();
    }

    // 정상 경로 → exit 0 + 삭제 + 체크포인트 보존
    let removed = dg(
        &[
            "worktree", "remove", "--worktree", &wt.display().to_string(),
            "--repo", &repo_s, "--run-id", "rm-ok",
            "--checkpoint-root", &ckpt_ro.display().to_string(), "--force", "--json",
        ],
        &envs,
    );
    assert_eq!(removed.status, 0);
    let parsed: Value = serde_json::from_str(removed.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(parsed["removed"], true);
    assert!(!wt.exists(), "워크트리 삭제됨");
    let ck = parsed["checkpoint_dir"].as_str().unwrap();
    assert!(Path::new(ck).exists(), "삭제 후 체크포인트 보존");
    let list = git_ok(&repo, &["worktree", "list", "--porcelain"]);
    assert!(!list.contains(&wt.display().to_string()), "git worktree list에서 제거");

    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn test_04_hook_guard_collect() {
    let envs_ro = [("HERDR_READONLY", "1")];

    // fail-closed: 잘못된 JSON → deny
    let r = dg_with_env(&["hook", "--guard"], &envs_ro, Some("not-json"), None);
    assert_eq!(r.status, 0);
    let v: Value = serde_json::from_str(r.stdout.trim()).unwrap();
    assert_eq!(v["decision"], "deny");
    assert!(v["reason"].as_str().unwrap().contains("fail-closed"));

    // write_file → deny
    let r = dg_with_env(
        &["hook", "--guard"],
        &envs_ro,
        Some(r#"{"toolCall":{"id":"1","name":"write_file","args":{"path":"/tmp/x"}},"conversationId":"c1","cwd":"/tmp"}"#),
        None,
    );
    let v: Value = serde_json::from_str(r.stdout.trim()).unwrap();
    assert_eq!(v["decision"], "deny");

    // read_file → allow
    let r = dg_with_env(
        &["hook", "--guard"],
        &envs_ro,
        Some(r#"{"toolCall":{"id":"1","name":"read_file","args":{"path":"/tmp/x"}},"conversationId":"c1","cwd":"/tmp"}"#),
        None,
    );
    let v: Value = serde_json::from_str(r.stdout.trim()).unwrap();
    assert_eq!(v["decision"], "allow");

    // collect — fail-open {}
    let r = dg_with_env(&["hook", "--collect"], &[], Some("garbage"), None);
    assert_eq!(r.status, 0);
    assert_eq!(r.stdout.trim(), "{}");
}

#[test]
fn test_05_setup_check_json() {
    let base = temp_base("setup");
    let r = dg(&["setup", "--check", "--json"], &[]);
    assert_eq!(r.status, 0, "setup --check --json exit 0 (stderr={})", r.stderr);
    let v: Value = serde_json::from_str(&r.stdout).expect("유효한 JSON 출력");
    assert!(v["ok"].is_boolean(), "ok 불린 필드");
    assert!(v["node"].is_object());
    assert!(v["clis"].is_object());
    assert!(v["secrets"].is_object());
    // 비밀 값 누출 없음 — 존재 여부 불린만 존재
    let secrets = v["secrets"].to_string();
    assert!(!secrets.contains("AIza"), "API 키 값 미포함");
    assert!(v["secrets"]["envSet"].is_boolean());
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn test_06_usage_errors_and_safe_fallback() {
    // 인증 오류 — exit 2
    let cases: Vec<(&str, Vec<&str>, i32)> = vec![
        ("invalid route enum", vec!["gate", "--route", "bogus", "--tool-class", "x"], 2),
        ("invalid risk", vec!["route", "--task-kind", "explore_read", "--risk", "extreme"], 2),
        ("unknown command", vec!["nope"], 2),
        ("missing value", vec!["gate", "--route"], 2),
        ("duplicate flag", vec!["gate", "--route", "file_select", "--route", "tool_select", "--tool-class", "x"], 2),
        ("positional arg", vec!["stats", "oops"], 2),
        ("invalid source", vec!["record", "--route", "tool_select", "--tool-class", "c", "--tool", "t", "--ok", "1", "--source", "bogus"], 2),
    ];
    for (label, args, expected) in cases {
        let r = dg(&args, &[]);
        assert_eq!(r.status, expected, "{label} → exit {expected} (got {})", r.status);
        if label != "unknown command" {
            // UsageError 계열만 [dg] 접두 — unknown command는 help 본문 출력(TS 패리티)
            assert!(r.stderr.contains("[dg]"), "{label} stderr에 [dg] 오류 라인: {}", r.stderr);
        }
    }

    // DB 폴백: 상위 구성요소가 일반 파일인 경로 → create_dir_all이 어떤 OS에서도
    // 실패해 진짜 open 실패를 유발한다(POSIX `/nonexistent/…`는 Windows에서
    // `C:\nonexistent\…`로 풀려 드라이브 루트에 실제 생성될 수 있어 폴백이 안 뜬다).
    let bad_root = temp_base("baddb");
    let blocker = bad_root.join("blocker");
    std::fs::write(&blocker, "x").unwrap();
    let bad_db = blocker.join("dg-e2e").join("none.db").display().to_string();
    let bad_db = bad_db.as_str();
    let r = dg(&["route", "--task-kind", "explore_read", "--db", bad_db, "--json"], &[]);
    assert_eq!(r.status, 0, "DB 폴백 경로는 exit 0");
    assert!(r.stderr.contains("db fallback"), "stderr에 폴백 경고: {}", r.stderr);
    let v: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(v["fallback"], true);
    assert_eq!(v["decision"], "split");
    assert!(v["fork_id"].as_str().unwrap().starts_with("fk_fallback_"));

    let r = dg(&["gate", "--route", "file_select", "--tool-class", "file_read", "--db", bad_db], &[]);
    assert_eq!(r.status, 0);

    let r = dg(&["record", "--route", "file_select", "--tool-class", "file_read", "--tool", "t", "--ok", "1", "--db", bad_db, "--json"], &[]);
    assert_eq!(r.status, 0);
    let v: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(v["recorded"], false);
    std::fs::remove_dir_all(&bad_root).unwrap();
}

#[test]
fn test_07_theta_and_exit_code_matrix() {
    let base = temp_base("theta");
    let repo = init_repo(&base);
    let repo_s = repo.display().to_string();
    let db = base.join("theta.db");
    let dbs = db.display().to_string();
    let envs = [("DGATE_DB_PATH", dbs.as_str())];

    // 기본 θ=0.70
    let r = dg_json(&["route", "--task-kind", "explore_read", "--project", &repo_s], &envs);
    assert_eq!(r["sharp_threshold"], 0.7);

    // checkpoint 서브커맨드 오류 — exit 2
    let r = dg(&["checkpoint", "bogus"], &[]);
    assert_eq!(r.status, 2);
    let r = dg(&["worktree"], &[]);
    assert_eq!(r.status, 2);

    // help exit 0
    let r = dg(&["--help"], &[]);
    assert_eq!(r.status, 0);
    assert!(r.stdout.contains("dg — decision gate CLI"));
    // 무인자 exit 2 + help
    let out = Command::new(DG).output().unwrap();
    assert_eq!(out.status.code(), Some(2));

    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn test_08_dispatch_brief_after_gates() {
    let base = temp_base("dispatch");
    let repo = init_repo(&base);
    let db = base.join("d.db");
    let dbs = db.display().to_string();
    let repo_s = repo.display().to_string();
    // XDG_STATE_HOME 격리 — 머신 전역 dispatch-mode(strict 포함)와 무관하게 off로 동작
    let xstate_off = base.join("xstate-off").display().to_string();
    let xstrict = base.join("xstate-strict");
    std::fs::create_dir_all(xstrict.join("dgate")).unwrap();
    std::fs::write(xstrict.join("dgate").join("dispatch-mode"), "strict\n").unwrap();
    let xstrict_s = xstrict.display().to_string();
    let off_envs = [("DGATE_DB_PATH", dbs.as_str()), ("XDG_STATE_HOME", xstate_off.as_str())];
    let strict_envs = [("DGATE_DB_PATH", dbs.as_str()), ("XDG_STATE_HOME", xstrict_s.as_str())];

    let valid_brief = base.join("brief-ok.md");
    std::fs::write(
        &valid_brief,
        "# Verifier Brief\n\n## Standards\n\n- 없음\n\n## Spec\n\n- 충족\n\n## Merge Danger\n\ndoor: two-way\nblast_radius: e2e 경로\n",
    )
    .unwrap();
    let valid_s = valid_brief.display().to_string();
    let bad_brief = base.join("brief-bad.md");
    std::fs::write(&bad_brief, "no headings here\n").unwrap();
    let bad_s = bad_brief.display().to_string();

    // (a) off + 브리프 부재 → 정상 진행(dry-run 리허설)
    let r = dg(&["dispatch", "--task-kind", "explore_read", "--repo", &repo_s, "--dry-run", "--json"], &off_envs);
    assert_eq!(r.status, 0, "off 부재 → exit 0 (stderr={})", r.stderr);
    let plan: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(plan["ok"], true);

    // (b) strict + 브리프 부재 → UsageError exit 2(fail-closed)
    let r = dg(&["dispatch", "--task-kind", "explore_read", "--repo", &repo_s, "--dry-run"], &strict_envs);
    assert_eq!(r.status, 2, "strict 부재 → exit 2");
    assert!(r.stderr.contains("strict 모드: --brief-file 필수"), "사유: {}", r.stderr);

    // (b2) strict + --help → 게이트 이전에 도움말 exit 0 (TS와 동일 순서)
    let r = dg(&["dispatch", "--help"], &strict_envs);
    assert_eq!(r.status, 0, "strict + --help → exit 0 (stderr={})", r.stderr);
    assert!(r.stdout.contains("--brief-file"), "도움말에 신규 플래그 포함: {}", r.stdout);

    // (c) 무효 브리프 → off·strict 양 모드 exit 2 + 위반 나열
    for envs in [&off_envs, &strict_envs] {
        let r = dg(&["dispatch", "--task-kind", "explore_read", "--repo", &repo_s, "--dry-run", "--brief-file", &bad_s], envs);
        assert_eq!(r.status, 2, "무효 브리프 → exit 2 (stderr={})", r.stderr);
        assert!(r.stderr.contains("## Standards 헤딩 없음"), "위반 나열: {}", r.stderr);
        assert!(r.stderr.contains("blast_radius"), "위반 나열: {}", r.stderr);
    }
    // 읽기 불가(부재 파일)도 exit 2
    let r = dg(
        &["dispatch", "--task-kind", "explore_read", "--repo", &repo_s, "--dry-run", "--brief-file", &base.join("nope.md").display().to_string()],
        &off_envs,
    );
    assert_eq!(r.status, 2);
    assert!(r.stderr.contains("읽기 실패"), "사유: {}", r.stderr);

    // (d) 유효 브리프 e2e — exit 0 + 워커 프롬프트에 브리프 경로 문장 주입
    let r = dg(
        &["dispatch", "--task-kind", "explore_read", "--repo", &repo_s, "--dry-run", "--json", "--brief-file", &valid_s],
        &strict_envs,
    );
    assert_eq!(r.status, 0, "strict 유효 브리프 → exit 0 (stderr={})", r.stderr);
    let plan: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(plan["decision"], "sharp");
    let hc = plan["hcCommands"][0].as_array().unwrap();
    let prompt = hc.last().unwrap().as_str().unwrap();
    assert!(
        prompt.ends_with(&format!(
            "브리프 파일을 먼저 읽고 두 축(Standards/Spec)과 Merge Danger을 따르라: {valid_s}"
        )),
        "프롬프트 주입: {prompt}"
    );
    let hc_text: Vec<String> = hc.iter().map(|v| v.as_str().unwrap_or_default().to_string()).collect();
    assert!(!hc_text.join(" ").contains("{brief}"), "플레이스홀더 잔존 금지");

    // (e) --after 미발견 → exit 2
    let r = dg(&["dispatch", "--task-kind", "single_edit", "--repo", &repo_s, "--db", &dbs, "--after", "fk_nope", "--json"], &off_envs);
    assert_eq!(r.status, 2, "--after 미발견 → exit 2");
    let plan: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(plan["ok"], false);
    assert_eq!(plan["reason"], "--after fork not found: fk_nope");

    // (f) 살아 있는 fork → deferred, fork 미발행·커맨드 0
    let g = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &off_envs);
    let fk = g["fork_id"].as_str().unwrap().to_string();
    let r = dg(&["dispatch", "--task-kind", "single_edit", "--repo", &repo_s, "--db", &dbs, "--after", &fk, "--json"], &off_envs);
    assert_eq!(r.status, 0, "active deferred → exit 0 (stderr={})", r.stderr);
    let plan: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(plan["ok"], true);
    assert_eq!(plan["deferred"], true);
    assert_eq!(plan["reason"], format!("after_fork {fk} not closed (status=open)"));
    assert_eq!(plan["hcCommands"].as_array().map(Vec::len), Some(0));

    // (g) 닫힌 fork → 정상 발행 + metadata.after_fork 스탬프
    dg(&["close", "--fork", &fk, "--outcome", "ok", "--by", "dispatcher"], &off_envs);
    let r = dg(&["dispatch", "--task-kind", "single_edit", "--repo", &repo_s, "--db", &dbs, "--after", &fk, "--json"], &off_envs);
    assert_eq!(r.status, 0, "terminal → exit 0 (stderr={})", r.stderr);
    let plan: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(plan["ok"], true);
    assert_eq!(plan["deferred"], false);
    assert_eq!(plan["hcCommands"].as_array().map(Vec::len), Some(1));
    let issued = plan["forkId"].as_str().unwrap();
    let conn = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let meta: Option<String> = conn
        .query_row("SELECT metadata_json FROM forks WHERE fork_id = ?1", [issued], |row| row.get(0))
        .unwrap();
    let meta: Value = serde_json::from_str(meta.as_deref().unwrap()).unwrap();
    assert_eq!(meta["after_fork"].as_str(), Some(fk.as_str()));
    assert_eq!(meta["dispatch"].as_bool(), Some(true));
    // Windows는 열린 DB 핸들이 남아 있으면 remove_dir_all이 ERROR_SHARING_VIOLATION
    // (code 32)으로 실패한다 — 연결을 명시적으로 닫고 정리한다.
    drop(conn);

    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn test_09_pipeline_init_step_status_lifecycle() {
    let base = temp_base("pipeline");
    let state_dir = base.join("pipes");
    let sd = state_dir.display().to_string();
    let envs: [(&str, &str); 0] = [];

    let brief = base.join("brief.md");
    std::fs::write(&brief, "# brief\n\n## Spec\n- `src/app.rs:10-40` 수정\n").unwrap();
    let brief_s = brief.display().to_string();

    // ---------- 앵커 없는 브리프 → 자동 ExploreRunning → explore-ok → CoderRunning ----------
    let vague_brief = base.join("vague-brief.md");
    std::fs::write(&vague_brief, "# brief\n\n## Spec\n- 검색 UX 개선\n").unwrap();
    let init_exp = dg_json(
        &[
            "pipeline", "init", "--fork", "fk_pipe_exp", "--brief", &vague_brief.display().to_string(),
            "--branch", "feat/pipe-exp", "--coder-model", "codex-gpt5", "--verifier-model", "glm-5.3",
            "--state-dir", &sd,
        ],
        &envs,
    );
    assert_eq!(init_exp["phase"], "explore_running");
    assert_eq!(init_exp["next_action"]["kind"], "spawn_explore");
    let step_exp = dg_json(
        &[
            "pipeline", "step", "--fork", "fk_pipe_exp", "--event", "explore-ok",
            "--context-map", ".herdr/context/fk_pipe_exp-map.md", "--state-dir", &sd,
        ],
        &envs,
    );
    assert_eq!(step_exp["phase"], "coder_running");
    assert_eq!(step_exp["next_action"]["kind"], "spawn_coder");

    // ---------- init(앵커 있음) → SpawnCoder + 상태 파일 생성 ----------
    let init = dg_json(
        &[
            "pipeline", "init", "--fork", "fk_pipe1", "--brief", &brief_s,
            "--branch", "feat/pipe", "--worktree", &base.join("wt").display().to_string(),
            "--scope", "crates/*", "--coder-model", "codex-gpt5", "--verifier-model", "glm-5.3",
            "--state-dir", &sd,
        ],
        &envs,
    );
    assert_eq!(init["action"], "init");
    assert_eq!(init["phase"], "coder_running");
    assert_eq!(init["next_action"]["kind"], "spawn_coder");
    assert_eq!(init["next_action"]["attempt"], 0);
    // 브리프/워크트리가 crates/* 도메인 밖 → scope_warnings에 경고
    let warns = init["scope_warnings"].as_array().unwrap();
    assert!(warns.iter().any(|w| w.as_str().unwrap().contains("brief.md")), "scope 경고: {warns:?}");
    let state_file = state_dir.join("fk_pipe1.json");
    assert!(state_file.exists(), "상태 파일 생성");

    // ---------- coder-ok → 이종 모델 → SpawnVerifier ----------
    let step = dg_json(
        &[
            "pipeline", "step", "--fork", "fk_pipe1", "--event", "coder-ok",
            "--head", "abc1234def", "--state-dir", &sd,
        ],
        &envs,
    );
    assert_eq!(step["phase"], "verifier_running");
    assert_eq!(step["next_action"]["kind"], "spawn_verifier");
    assert_eq!(step["next_action"]["verifier_model"], "glm-5.3");
    assert_eq!(step["next_action"]["head_sha"], "abc1234def");

    // ---------- verdict pass → SpawnAdvisorG3 ----------
    let verdict_path = base.join("v-pass.json");
    std::fs::write(
        &verdict_path,
        serde_json::json!({
            "schema_version": 1,
            "fork_id": "fk_pipe1",
            "target_head": "abc1234def",
            "decision": "pass",
            "reason": "e2e ok",
            "findings": [],
        })
        .to_string(),
    )
    .unwrap();
    let step = dg_json(
        &[
            "pipeline", "step", "--fork", "fk_pipe1", "--event", "verdict",
            "--verdict-file", &verdict_path.display().to_string(), "--state-dir", &sd,
        ],
        &envs,
    );
    assert_eq!(step["phase"], "advisor_g3_running");
    assert_eq!(step["next_action"]["kind"], "spawn_advisor_g3");
    assert_eq!(step["verdict_errors"].as_array().unwrap().len(), 0);

    // ---------- g3-pass → ReadyPr + READY_PR 신호 ----------
    let step = dg_json(
        &[
            "pipeline", "step", "--fork", "fk_pipe1", "--event", "g3-pass",
            "--pr", "42", "--state-dir", &sd,
        ],
        &envs,
    );
    assert_eq!(step["phase"], "ready_pr");
    assert_eq!(step["next_action"]["kind"], "emit_ready_pr");
    assert_eq!(step["ready_pr_signal"], "READY_PR=42 HEAD=abc1234def G3=PASS");

    // ---------- status — 읽기 전용 조회 ----------
    let status = dg_json(
        &["pipeline", "status", "--fork", "fk_pipe1", "--state-dir", &sd],
        &envs,
    );
    assert_eq!(status["state"]["phase"], "ready_pr");
    assert_eq!(status["state"]["pr_number"], 42);
    assert_eq!(
        status["state"]["history"].as_array().unwrap().len(),
        4, // init + coder_exited + verdict_received + advisor_g3_result
    );

    // ---------- 재작업 루프 → 에스컬레이션 exit 3 ----------
    dg_json(
        &[
            "pipeline", "init", "--fork", "fk_pipe2", "--brief", &brief_s,
            "--branch", "feat/p2", "--coder-model", "codex", "--verifier-model", "glm",
            "--state-dir", &sd,
        ],
        &envs,
    );
    for (i, phase) in ["rework_pending", "rework_pending"].iter().enumerate() {
        let step = dg_json(
            &[
                "pipeline", "step", "--fork", "fk_pipe2", "--event", "coder-fail",
                "--state-dir", &sd,
            ],
            &envs,
        );
        assert_eq!(step["phase"], *phase, "{}번째 실패", i + 1);
        assert_eq!(step["next_action"]["kind"], "spawn_coder");
        assert_eq!(step["attempt"], (i + 1) as i64);
    }
    // max_reworks 소진 → Escalated + exit 3
    let r = dg(
        &[
            "pipeline", "step", "--fork", "fk_pipe2", "--event", "coder-fail",
            "--stalled", "--state-dir", &sd, "--json",
        ],
        &envs,
    );
    assert_eq!(r.status, 3, "재시도 소진 → 에스컬레이션 exit 3 (stderr={})", r.stderr);
    let parsed: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(parsed["phase"], "escalated");
    assert_eq!(parsed["next_action"]["kind"], "wake_sublead_escalation");

    // ---------- 동종 모델 → fail-closed 에스컬레이션 ----------
    // gpt-4o/gpt-5 모두 normalize_model_family → "gpt" (동종 → 이종 불변식 위반)
    dg_json(
        &[
            "pipeline", "init", "--fork", "fk_pipe3", "--brief", &brief_s,
            "--branch", "feat/p3", "--coder-model", "gpt-4o", "--verifier-model", "gpt-5",
            "--state-dir", &sd,
        ],
        &envs,
    );
    let r = dg(
        &[
            "pipeline", "step", "--fork", "fk_pipe3", "--event", "coder-ok",
            "--state-dir", &sd, "--json",
        ],
        &envs,
    );
    assert_eq!(r.status, 3, "동종 모델 → fail-closed exit 3");
    let parsed: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(parsed["phase"], "escalated");
    assert!(parsed["escalation_reason"].as_str().unwrap().contains("이종 불변식"));

    // ---------- 사용법 오류 ----------
    let r = dg(&["pipeline", "bogus"], &envs);
    assert_eq!(r.status, 2, "알 수 없는 서브커맨드 → exit 2");
    let r = dg(&["pipeline", "status", "--fork", "fk_none", "--state-dir", &sd], &envs);
    assert_eq!(r.status, 2, "미존재 상태 파일 → exit 2");
    let r = dg(&["pipeline", "step", "--fork", "fk_pipe1", "--event", "bogus", "--state-dir", &sd], &envs);
    assert_eq!(r.status, 2, "알 수 없는 이벤트 → exit 2");

    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn test_10_merge_train_guard_and_dry_run() {
    let base = temp_base("merge-train");
    // HERDR_ROLE을 lead로 고정 — 호출 세션의 역할 env가 새어 들어와도 건조하게 유지.
    let envs = [("HERDR_ROLE", "lead")];

    // (a) dry-run — 러너 호출 0회(gh/git 미호출), 예정 단계 목록 + exit 0
    let r = dg(
        &["merge-train", "--pr", "42", "--dry-run", "--json"],
        &envs,
    );
    assert_eq!(r.status, 0, "dry-run → exit 0 (stderr={})", r.stderr);
    let v: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(v["cmd"], "merge-train");
    assert_eq!(v["status"], "dry_run");
    assert_eq!(v["pr_number"], 42);
    assert_eq!(v["exit_code"], 0);
    let steps: Vec<&str> = v["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert_eq!(
        steps,
        vec![
            "update_branch",
            "add_label",
            "wait_for_ci_run",
            "rerun_failed",
            "check_merge_state",
            "squash_merge",
        ],
        "worktree 미지정 → cleanup_worktree 미포함"
    );

    // (b) HERDR_ROLE=sublead → 물리 차단 UsageError exit 2 (dry-run 진입 전 차단)
    let r = dg(
        &["merge-train", "--pr", "42", "--dry-run", "--json"],
        &[("HERDR_ROLE", "sublead")],
    );
    assert_eq!(r.status, 2, "HERDR_ROLE=sublead → exit 2");
    assert!(r.stderr.contains("sublead"), "거부 사유: {}", r.stderr);

    // (c) --pr 누락/정수 아님 → exit 2
    let r = dg(&["merge-train", "--dry-run"], &envs);
    assert_eq!(r.status, 2, "--pr 누락 → exit 2");
    let r = dg(&["merge-train", "--pr", "abc", "--dry-run"], &envs);
    assert_eq!(r.status, 2, "--pr 정수 아님 → exit 2");
    let r = dg(&["merge-train", "--pr", "42", "--max-reruns", "-1", "--dry-run"], &envs);
    assert_eq!(r.status, 2, "--max-reruns 음수 → exit 2");

    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn test_11_suggest_sublead_proactive_scopes() {
    // --scopes 사전 분해: DB 없이 순수 판정 — 비겹침 2영역 → 병렬 2개 추천
    let r = dg(
        &["suggest-sublead", "--scopes", "crates/a,crates/b", "--json"],
        &[],
    );
    assert_eq!(r.status, 0, "--scopes → exit 0 (stderr={})", r.stderr);
    let v: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(v["recommended"], true);
    assert_eq!(v["suggested_subleads"], 2);
    assert_eq!(v["disjoint_groups"].as_array().unwrap().len(), 2);

    // 겹치는 경로만 → 단일 트랙 유지 권장
    let r = dg(
        &["suggest-sublead", "--scopes", "crates/a,crates/a/sub", "--json"],
        &[],
    );
    assert_eq!(r.status, 0, "겹침 → exit 0 (stderr={})", r.stderr);
    let v: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(v["recommended"], false);
    assert_eq!(v["suggested_subleads"], 0);

    // --scopes 미지정 시 기존 DB 사후 집계 경로 유지(회귀 방지)
    let base = temp_base("suggest-db");
    let db = base.join("s.db");
    let dbs = db.display().to_string();
    let envs = [("DGATE_DB_PATH", dbs.as_str())];
    let r = dg(&["suggest-sublead", "--json"], &envs);
    assert_eq!(r.status, 0, "DB 경로 → exit 0 (stderr={})", r.stderr);
    let v: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(v["recommended"], false, "빈 DB → 추천 없음");
    std::fs::remove_dir_all(&base).unwrap();
}
